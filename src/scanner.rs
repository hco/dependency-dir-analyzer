use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::Serialize;
use walkdir::WalkDir;

/// A dependency directory that was found during a scan, together with its
/// total on-disk size.
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
pub struct ScanOptions {
    pub follow_symlinks: bool,
}

/// Walk `root` and find every dependency directory beneath it.
///
/// When a dependency directory is found it is recorded and the walk does *not*
/// descend into it — the whole subtree is accounted for as a single unit, and
/// nested dependency directories (e.g. a `node_modules` inside another
/// `node_modules`) are not reported separately. Unreadable entries are skipped
/// with a warning on stderr rather than aborting the scan.
pub fn scan(root: &Path, opts: &ScanOptions) -> Vec<DependencyDir> {
    let mut matches: Vec<(PathBuf, &'static Pattern)> = Vec::new();

    let mut walker = WalkDir::new(root)
        .follow_links(opts.follow_symlinks)
        .into_iter();

    while let Some(entry) = walker.next() {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                eprintln!("warning: {err}");
                continue;
            }
        };

        if entry.file_type().is_dir() {
            if let Some(pattern) = entry.file_name().to_str().and_then(match_pattern) {
                matches.push((entry.path().to_path_buf(), pattern));
                // Don't descend into a matched directory: we size it whole and
                // avoid double-counting nested dependency directories.
                walker.skip_current_dir();
            }
        }
    }

    // Sizing touches the filesystem a lot, so do it in parallel across matches.
    let mut results: Vec<DependencyDir> = matches
        .into_par_iter()
        .map(|(path, pattern)| {
            let (size_bytes, file_count) = directory_size(&path, opts.follow_symlinks);
            DependencyDir {
                path,
                ecosystem: pattern.ecosystem,
                name: pattern.name,
                size_bytes,
                file_count,
            }
        })
        .collect();

    results.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.path.cmp(&b.path))
    });
    results
}

/// Sum the size and count of all regular files within `dir`.
fn directory_size(dir: &Path, follow_symlinks: bool) -> (u64, u64) {
    let mut total = 0u64;
    let mut count = 0u64;

    for entry in WalkDir::new(dir).follow_links(follow_symlinks) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                total += meta.len();
                count += 1;
            }
        }
    }

    (total, count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn matches_known_names_only() {
        assert!(match_pattern("node_modules").is_some());
        assert_eq!(match_pattern("node_modules").unwrap().ecosystem, "Node.js");
        assert!(match_pattern("src").is_none());
        assert!(match_pattern("Node_Modules").is_none()); // case-sensitive
    }

    #[test]
    fn finds_dependency_dirs_and_does_not_recurse_into_them() {
        let tmp = std::env::temp_dir().join(format!("dda-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);

        let nm = tmp.join("project").join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("a.js"), b"hello").unwrap();
        // A nested node_modules should NOT be reported separately.
        let nested = nm.join("pkg").join("node_modules");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("b.js"), b"world!!").unwrap();

        let results = scan(
            &tmp,
            &ScanOptions {
                follow_symlinks: false,
            },
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "node_modules");
        // 5 bytes + 7 bytes from both files are summed into the single match.
        assert_eq!(results[0].size_bytes, 12);
        assert_eq!(results[0].file_count, 2);

        let _ = fs::remove_dir_all(&tmp);
    }
}
