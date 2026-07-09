use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::SystemTime;

use rayon::prelude::*;
use serde::Serialize;
use walkdir::WalkDir;

/// A dependency directory that was found during a scan, together with its
/// total on-disk size and the most recent modification time beneath it.
#[derive(Debug, Clone, Serialize)]
pub struct DependencyDir {
    /// Absolute or relative path to the directory, depending on the scan root.
    pub path: PathBuf,
    /// Human label for the tool/ecosystem that produced the directory.
    pub ecosystem: &'static str,
    /// The directory's base name that matched a known pattern (e.g. `node_modules`).
    pub name: &'static str,
    /// Total size of all regular files contained in the directory, in bytes.
    pub size_bytes: u64,
    /// Number of regular files contained in the directory.
    pub file_count: u64,
    /// Most recent modification time found within the directory (the directory
    /// itself or any file beneath it). `None` if no timestamp was readable.
    #[serde(serialize_with = "serialize_modified")]
    pub modified: Option<SystemTime>,
}

/// Serialize an optional `SystemTime` as an object with a Unix timestamp and an
/// RFC 3339 (UTC) string, or `null` when unknown. This keeps the JSON output
/// both machine- and human-friendly without leaking platform-specific types.
fn serialize_modified<S>(value: &Option<SystemTime>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeStruct;

    match value.and_then(unix_seconds) {
        Some(secs) => {
            let mut s = serializer.serialize_struct("Modified", 2)?;
            s.serialize_field("unix", &secs)?;
            s.serialize_field("rfc3339", &rfc3339_utc(secs))?;
            s.end()
        }
        None => serializer.serialize_none(),
    }
}

/// Convert a `SystemTime` to whole seconds since the Unix epoch (may be
/// negative for pre-1970 timestamps). Returns `None` only if the value is wildly
/// out of range.
pub fn unix_seconds(t: SystemTime) -> Option<i64> {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).ok(),
        Err(e) => i64::try_from(e.duration().as_secs()).ok().map(|s| -s),
    }
}

/// Format Unix seconds as an RFC 3339 timestamp in UTC, e.g.
/// `2026-06-13T09:28:57Z`.
pub fn rfc3339_utc(secs: i64) -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_else(|| secs.to_string())
}

/// A known dependency-directory name paired with the ecosystem it belongs to.
struct Pattern {
    name: &'static str,
    ecosystem: &'static str,
}

/// Directory names that package managers and build tools generate and that can
/// generally be regenerated from source. Ordering does not matter; lookups are
/// by exact base-name match.
const PATTERNS: &[Pattern] = &[
    Pattern {
        name: "node_modules",
        ecosystem: "Node.js",
    },
    Pattern {
        name: "bower_components",
        ecosystem: "Bower",
    },
    Pattern {
        name: ".next",
        ecosystem: "Next.js",
    },
    Pattern {
        name: ".nuxt",
        ecosystem: "Nuxt",
    },
    Pattern {
        name: ".svelte-kit",
        ecosystem: "SvelteKit",
    },
    Pattern {
        name: ".angular",
        ecosystem: "Angular",
    },
    Pattern {
        name: "target",
        ecosystem: "Rust / Maven",
    },
    Pattern {
        name: ".venv",
        ecosystem: "Python (venv)",
    },
    Pattern {
        name: "venv",
        ecosystem: "Python (venv)",
    },
    Pattern {
        name: "__pycache__",
        ecosystem: "Python (cache)",
    },
    Pattern {
        name: ".tox",
        ecosystem: "Python (tox)",
    },
    Pattern {
        name: ".mypy_cache",
        ecosystem: "Python (mypy)",
    },
    Pattern {
        name: ".pytest_cache",
        ecosystem: "Python (pytest)",
    },
    Pattern {
        name: "vendor",
        ecosystem: "Go / PHP / Ruby",
    },
    Pattern {
        name: ".bundle",
        ecosystem: "Ruby (Bundler)",
    },
    Pattern {
        name: ".gradle",
        ecosystem: "Gradle",
    },
    Pattern {
        name: ".dart_tool",
        ecosystem: "Dart / Flutter",
    },
    Pattern {
        name: "elm-stuff",
        ecosystem: "Elm",
    },
    Pattern {
        name: "Pods",
        ecosystem: "CocoaPods",
    },
    Pattern {
        name: ".terraform",
        ecosystem: "Terraform",
    },
    Pattern {
        name: "DerivedData",
        ecosystem: "Xcode",
    },
    Pattern {
        name: ".cargo",
        ecosystem: "Cargo (cache)",
    },
    Pattern {
        name: ".cache",
        ecosystem: "Generic cache",
    },
];

/// Returns the matching pattern for a directory base name, if any.
fn match_pattern(name: &str) -> Option<&'static Pattern> {
    PATTERNS.iter().find(|p| p.name == name)
}

/// Options controlling a scan.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanOptions {
    pub follow_symlinks: bool,
}

/// An event emitted while a streaming scan is in progress. Consumers (such as
/// the interactive TUI) use these to render results as they arrive instead of
/// waiting for the whole scan to finish.
#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// A dependency directory was discovered and queued for sizing.
    Discovered {
        path: PathBuf,
        ecosystem: &'static str,
    },
    /// A previously discovered directory has finished being sized.
    Sized(DependencyDir),
    /// A non-fatal problem (e.g. an unreadable entry).
    Warning(String),
}

/// Walk `root` and invoke `on_match` for every dependency directory beneath it,
/// as each one is found.
///
/// When a dependency directory is found the walk does *not* descend into it —
/// the whole subtree is accounted for as a single unit, and nested dependency
/// directories (e.g. a `node_modules` inside another `node_modules`) are not
/// reported separately. Unreadable entries invoke `on_warning` rather than
/// aborting the scan. Reporting matches through a callback lets streaming
/// consumers act on each one immediately instead of waiting for the full walk.
fn walk_matches(
    root: &Path,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    mut on_warning: impl FnMut(String),
    mut on_match: impl FnMut(PathBuf, &'static Pattern),
) {
    let mut walker = WalkDir::new(root)
        .follow_links(opts.follow_symlinks)
        .into_iter();

    while let Some(entry) = walker.next() {
        // Bail out of the walk promptly once the caller asks us to stop (e.g. the
        // TUI has quit), instead of traversing the rest of the tree for results
        // nobody is waiting for.
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                on_warning(format!("{err}"));
                continue;
            }
        };

        if entry.file_type().is_dir() {
            if let Some(pattern) = entry.file_name().to_str().and_then(match_pattern) {
                on_match(entry.path().to_path_buf(), pattern);
                // Don't descend into a matched directory: we size it whole and
                // avoid double-counting nested dependency directories.
                walker.skip_current_dir();
            }
        }
    }
}

/// Collect the paths of every dependency directory beneath `root` into a `Vec`.
/// Thin wrapper over [`walk_matches`] for the batch code paths that need the
/// full list up front.
fn collect_matches(
    root: &Path,
    opts: &ScanOptions,
    on_warning: impl FnMut(String),
) -> Vec<(PathBuf, &'static Pattern)> {
    let mut matches: Vec<(PathBuf, &'static Pattern)> = Vec::new();
    // The batch path always runs to completion, so cancellation never fires.
    let never = AtomicBool::new(false);
    walk_matches(root, opts, &never, on_warning, |path, pattern| {
        matches.push((path, pattern));
    });
    matches
}

/// Walk `root` and find every dependency directory beneath it, returning the
/// results sorted largest-first. This is the batch entry point used for the
/// machine-readable output formats and tests.
pub fn scan(root: &Path, opts: &ScanOptions) -> Vec<DependencyDir> {
    let matches = collect_matches(root, opts, |w| eprintln!("warning: {w}"));

    // Sizing touches the filesystem a lot, so do it in parallel across matches.
    let never = AtomicBool::new(false);
    let mut results: Vec<DependencyDir> = matches
        .into_par_iter()
        .map(|(path, pattern)| size_match(path, pattern, opts.follow_symlinks, &never))
        .collect();

    sort_by_size(&mut results);
    results
}

/// Streaming variant of [`scan`]. Emits a `Discovered` event and kicks off
/// sizing for each match *as the walk finds it*, so results begin flowing to the
/// UI without waiting for the whole tree to be traversed. Sizing runs in
/// parallel on the rayon pool; each finished directory produces a `Sized` event.
/// Returns once the walk is done and every match has been sized (or `tx` has
/// hung up). Designed to run on a background thread feeding a live UI.
pub fn scan_streaming(
    root: &Path,
    opts: &ScanOptions,
    tx: Sender<ScanEvent>,
    cancel: Arc<AtomicBool>,
) {
    let follow = opts.follow_symlinks;
    let cancel: &AtomicBool = &cancel;

    // The walk runs on this thread while sizing tasks are spawned onto the rayon
    // pool. The scope waits for all sizing tasks before returning — so when the
    // caller sets `cancel` (e.g. the TUI has quit), the walk stops spawning and
    // in-flight sizing tasks bail early, letting the scan thread be joined
    // promptly instead of finishing the whole tree first.
    rayon::scope(|scope| {
        walk_matches(
            root,
            opts,
            cancel,
            |w| {
                let _ = tx.send(ScanEvent::Warning(w));
            },
            |path, pattern| {
                let _ = tx.send(ScanEvent::Discovered {
                    path: path.clone(),
                    ecosystem: pattern.ecosystem,
                });
                let tx = tx.clone();
                scope.spawn(move |_| {
                    let dir = size_match(path, pattern, follow, cancel);
                    let _ = tx.send(ScanEvent::Sized(dir));
                });
            },
        );
    });
}

/// Size a single matched directory into a [`DependencyDir`].
fn size_match(
    path: PathBuf,
    pattern: &Pattern,
    follow_symlinks: bool,
    cancel: &AtomicBool,
) -> DependencyDir {
    let (size_bytes, file_count, modified) = directory_stats(&path, follow_symlinks, cancel);
    DependencyDir {
        path,
        ecosystem: pattern.ecosystem,
        name: pattern.name,
        size_bytes,
        file_count,
        modified,
    }
}

/// Sort dependency directories largest-first, breaking ties by path for stable
/// output.
pub fn sort_by_size(results: &mut [DependencyDir]) {
    results.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.path.cmp(&b.path))
    });
}

/// Sum the size and count of all regular files within `dir`, and find the most
/// recent modification time of the directory and everything beneath it.
fn directory_stats(
    dir: &Path,
    follow_symlinks: bool,
    cancel: &AtomicBool,
) -> (u64, u64, Option<SystemTime>) {
    let mut total = 0u64;
    let mut count = 0u64;
    let mut newest: Option<SystemTime> = None;

    for entry in WalkDir::new(dir).follow_links(follow_symlinks) {
        // A single dependency directory can be enormous; checking here lets a
        // cancelled scan abandon sizing mid-tree rather than walking it all.
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if let Ok(meta) = entry.metadata() {
            if entry.file_type().is_file() {
                total += meta.len();
                count += 1;
            }
            if let Ok(modified) = meta.modified() {
                newest = Some(match newest {
                    Some(current) if current >= modified => current,
                    _ => modified,
                });
            }
        }
    }

    (total, count, newest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;

    #[test]
    fn matches_known_names_only() {
        assert!(match_pattern("node_modules").is_some());
        assert_eq!(match_pattern("node_modules").unwrap().ecosystem, "Node.js");
        assert!(match_pattern("src").is_none());
        assert!(match_pattern("Node_Modules").is_none()); // case-sensitive
    }

    fn make_tree(tag: &str) -> PathBuf {
        let tmp = std::env::temp_dir().join(format!("dda-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);

        let nm = tmp.join("project").join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("a.js"), b"hello").unwrap();
        // A nested node_modules should NOT be reported separately.
        let nested = nm.join("pkg").join("node_modules");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("b.js"), b"world!!").unwrap();
        tmp
    }

    #[test]
    fn finds_dependency_dirs_and_does_not_recurse_into_them() {
        let tmp = make_tree("batch");

        let results = scan(&tmp, &ScanOptions::default());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "node_modules");
        // 5 bytes + 7 bytes from both files are summed into the single match.
        assert_eq!(results[0].size_bytes, 12);
        assert_eq!(results[0].file_count, 2);
        assert!(results[0].modified.is_some());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn streaming_emits_discovered_then_sized() {
        let tmp = make_tree("stream");

        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        scan_streaming(&tmp, &ScanOptions::default(), tx, cancel);

        let mut discovered = 0;
        let mut sized = Vec::new();
        for ev in rx {
            match ev {
                ScanEvent::Discovered { ecosystem, .. } => {
                    assert_eq!(ecosystem, "Node.js");
                    discovered += 1;
                }
                ScanEvent::Sized(d) => sized.push(d),
                ScanEvent::Warning(_) => {}
            }
        }
        assert_eq!(discovered, 1);
        assert_eq!(sized.len(), 1);
        assert_eq!(sized[0].size_bytes, 12);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rfc3339_formats_epoch() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
    }
}
