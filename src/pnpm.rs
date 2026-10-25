//! pnpm's content-addressable store: finding the stores projects use, so the
//! space they hold shows up as rows of their own.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Ecosystem label (and row name) for pnpm store rows.
pub const STORE_ECOSYSTEM: &str = "pnpm store";

/// Extract `storeDir` from the contents of a `node_modules/.modules.yaml`.
///
/// pnpm has written this file both as YAML (`storeDir: /p`) and as JSON
/// (`"storeDir": "/p",`), so rather than pull in a parser this finds the one
/// line that names the key. A relative path is resolved against `base`, the
/// `node_modules` directory holding the file.
pub fn parse_store_dir(contents: &str, base: &Path) -> Option<PathBuf> {
    contents.lines().find_map(|line| {
        let rest = line.trim().trim_start_matches('"');
        let value = rest.strip_prefix("storeDir")?;
        let value = value
            .trim_start_matches('"')
            .trim_start()
            .strip_prefix(':')?;
        let value = value
            .trim()
            .trim_end_matches(',')
            .trim_matches(|c| c == '"' || c == '\'');
        if value.is_empty() {
            return None;
        }
        Some(base.join(value))
    })
}

/// The store the pnpm project owning `node_modules` installed from, if it was
/// installed by pnpm.
pub fn store_dir_for(node_modules: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(node_modules.join(".modules.yaml")).ok()?;
    parse_store_dir(&contents, node_modules)
}

/// The versioned stores (`v3`, `v10`, …) under pnpm's default store location
/// for this platform, whether or not any project in the scan uses them.
pub fn default_store_dirs() -> Vec<PathBuf> {
    let Some(base) = default_store_root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| is_version_dir(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// The pnpm major version whose store layout `store` is, read from its
/// `v<N>` directory name. pnpm 7 to 9 shared the `v3` layout, so that maps to 9.
pub fn store_major(store: &Path) -> Option<u32> {
    let name = store.file_name()?.to_str()?;
    if !is_version_dir(name) {
        return None;
    }
    match name[1..].parse::<u32>().ok()? {
        3 => Some(9),
        n if n >= 10 => Some(n),
        _ => None,
    }
}

/// Whether a pnpm of major version `installed` reads the store layout that
/// pnpm `store_major` writes.
fn reads_layout(installed: u32, store_major: u32) -> bool {
    installed == store_major || (store_major == 9 && (7..=9).contains(&installed))
}

/// The command that prunes `store`: the installed pnpm if it reads that
/// store's layout, otherwise the matching pnpm via `npx`, since a pnpm of
/// another version would look in a different `v<N>` directory.
pub fn prune_command(store: &Path, installed_major: Option<u32>) -> (String, Vec<String>) {
    let major = store_major(store);
    // `--store-dir` takes the store root; pnpm appends its own `v<N>`.
    let store_dir = match major {
        Some(_) => store.parent().unwrap_or(store),
        None => store,
    };
    let mut args: Vec<String> = Vec::new();
    let program = match (major, installed_major) {
        (Some(m), Some(i)) if reads_layout(i, m) => "pnpm",
        (None, _) => "pnpm",
        (Some(m), _) => {
            args.push("--yes".to_string());
            args.push(format!("pnpm@{m}"));
            "npx"
        }
    };
    args.extend(["store", "prune", "--store-dir"].map(String::from));
    args.push(store_dir.display().to_string());
    (program.to_string(), args)
}

/// Run `pnpm store prune` on `store`, removing packages no project references
/// anymore. On failure the error names the command so it can be run by hand.
pub fn prune(store: &Path) -> Result<(), String> {
    let (program, args) = prune_command(store, installed_major());
    let manual = shell_line(&program, &args);
    if !store.is_dir() {
        return Err(format!("store not found; run manually: {manual}"));
    }
    let output = Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not start {program}: {e}; run manually: {manual}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(str::trim)
        .unwrap_or("pnpm store prune failed");
    Err(format!("{reason}; run manually: {manual}"))
}

/// Major version of the pnpm on `PATH`, if there is one.
fn installed_major() -> Option<u32> {
    let output = Command::new("pnpm")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// A command line to copy into a shell, quoting arguments that need it.
fn shell_line(program: &str, args: &[String]) -> String {
    let mut line = program.to_string();
    for arg in args {
        line.push(' ');
        if arg
            .chars()
            .any(|c| c.is_whitespace() || "'\"$`\\".contains(c))
        {
            line.push_str(&format!("'{}'", arg.replace('\'', r"'\''")));
        } else {
            line.push_str(arg);
        }
    }
    line
}

/// Whether `store` is laid out like a pnpm store: a `v<N>` directory holding
/// `files/` plus either pnpm 10+'s index or project registry, or (pnpm 7-9's
/// `v3`) two-hex-digit content buckets under `files/`. Paths come from
/// `.modules.yaml` files in the scanned tree, so anything else (a broken or
/// hostile `storeDir: /`) must never become a deletable row.
pub fn looks_like_store(store: &Path) -> bool {
    let named_like_store = store
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_version_dir);
    let files = store.join("files");
    if !named_like_store || !files.is_dir() {
        return false;
    }
    ["index", "index.db", "projects", "links"]
        .iter()
        .any(|marker| store.join(marker).exists())
        || has_hex_bucket(&files)
}

/// Whether `files` holds a content bucket such as `0a/`.
fn has_hex_bucket(files: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(files) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name.len() == 2
            && name.bytes().all(|b| b.is_ascii_hexdigit())
            && e.file_type().is_ok_and(|t| t.is_dir())
    })
}

/// Whether `name` looks like a store layout version directory (`v10`).
fn is_version_dir(name: &str) -> bool {
    name.strip_prefix('v')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(target_os = "macos")]
fn default_store_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/pnpm/store"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn default_store_root() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(data.join("pnpm/store"))
}

#[cfg(windows)]
fn default_store_root() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("pnpm").join("store"))
}

#[cfg(not(any(unix, windows)))]
fn default_store_root() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_store_dir_yaml_style() {
        let yaml =
            "layoutVersion: 5\nstoreDir: /Users/x/Library/pnpm/store/v10\nvirtualStoreDir: .pnpm\n";
        assert_eq!(
            parse_store_dir(yaml, Path::new("/p/node_modules")),
            Some(PathBuf::from("/Users/x/Library/pnpm/store/v10"))
        );
        let quoted = "storeDir: '/Users/x/store/v3'\n";
        assert_eq!(
            parse_store_dir(quoted, Path::new("/p/node_modules")),
            Some(PathBuf::from("/Users/x/store/v3"))
        );
    }

    #[test]
    fn parse_store_dir_json_style() {
        let json = "{\n  \"layoutVersion\": 5,\n  \"storeDir\": \"/Users/x/Library/pnpm/store/v11\",\n  \"virtualStoreDir\": \".pnpm\"\n}\n";
        assert_eq!(
            parse_store_dir(json, Path::new("/p/node_modules")),
            Some(PathBuf::from("/Users/x/Library/pnpm/store/v11"))
        );
    }

    #[test]
    fn parse_store_dir_relative_is_resolved_against_base() {
        assert_eq!(
            parse_store_dir(
                "storeDir: ../.pnpm-store/v10\n",
                Path::new("/p/node_modules")
            ),
            Some(PathBuf::from("/p/node_modules/../.pnpm-store/v10"))
        );
    }

    #[test]
    fn parse_store_dir_missing_returns_none() {
        assert_eq!(parse_store_dir("layoutVersion: 5\n", Path::new("/p")), None);
        assert_eq!(parse_store_dir("storeDir:\n", Path::new("/p")), None);
        assert_eq!(parse_store_dir("\u{0}\u{1}garbage", Path::new("/p")), None);
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn store_major_maps_layout_to_pnpm_version() {
        assert_eq!(store_major(Path::new("/s/store/v10")), Some(10));
        assert_eq!(store_major(Path::new("/s/store/v11")), Some(11));
        // pnpm 7 to 9 all wrote the v3 layout.
        assert_eq!(store_major(Path::new("/s/store/v3")), Some(9));
        assert_eq!(store_major(Path::new("/x/custom")), None);
    }

    #[test]
    fn prune_command_uses_matching_pnpm() {
        let store = Path::new("/s/store/v10");
        assert_eq!(
            prune_command(store, Some(10)),
            (
                "pnpm".to_string(),
                args(&["store", "prune", "--store-dir", "/s/store"])
            )
        );
        assert_eq!(
            prune_command(store, Some(11)),
            (
                "npx".to_string(),
                args(&[
                    "--yes",
                    "pnpm@10",
                    "store",
                    "prune",
                    "--store-dir",
                    "/s/store"
                ])
            )
        );
        assert_eq!(
            prune_command(store, None),
            (
                "npx".to_string(),
                args(&[
                    "--yes",
                    "pnpm@10",
                    "store",
                    "prune",
                    "--store-dir",
                    "/s/store"
                ])
            )
        );
        // pnpm 8 still reads the v3 layout, so no download is needed.
        assert_eq!(prune_command(Path::new("/s/store/v3"), Some(8)).0, "pnpm");
        assert_eq!(
            prune_command(Path::new("/x/custom"), Some(10)),
            (
                "pnpm".to_string(),
                args(&["store", "prune", "--store-dir", "/x/custom"])
            )
        );
    }

    #[test]
    fn prune_failure_names_the_command_to_run() {
        let err = prune(Path::new("/definitely/not/a/store/v10")).unwrap_err();
        assert!(
            err.contains("store prune --store-dir /definitely/not/a/store"),
            "{err}"
        );
    }

    #[test]
    fn store_layout_needs_version_name_and_files() {
        let tmp = std::env::temp_dir().join(format!("dda-pnpm-{}-layout", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mk = |p: &str| std::fs::create_dir_all(tmp.join(p)).unwrap();
        // pnpm 10/11: `files/` next to an index or project registry.
        mk("v10/files");
        mk("v10/index");
        mk("v11/files");
        std::fs::write(tmp.join("v11/index.db"), b"").unwrap();
        // pnpm 7-9 (`v3`): only `files/`, with two-hex-digit buckets.
        mk("v3/files/0a");
        mk("v3/tmp");
        // Look-alikes: a user folder with `files/` and a bare version dir.
        mk("v2/files/photos");
        mk("v12");
        mk("other/files/00");
        mk("other/index");
        assert!(looks_like_store(&tmp.join("v10")));
        assert!(looks_like_store(&tmp.join("v11")));
        assert!(looks_like_store(&tmp.join("v3")));
        assert!(!looks_like_store(&tmp.join("v2")));
        assert!(!looks_like_store(&tmp.join("v12")));
        assert!(!looks_like_store(&tmp.join("other")));
        assert!(!looks_like_store(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn version_dirs_are_recognised() {
        assert!(is_version_dir("v10"));
        assert!(is_version_dir("v3"));
        assert!(!is_version_dir("v"));
        assert!(!is_version_dir("tmp"));
        assert!(!is_version_dir("v10-old"));
    }

    #[test]
    fn store_dir_for_reads_modules_yaml() {
        let tmp = std::env::temp_dir().join(format!("dda-pnpm-{}-modyaml", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert_eq!(store_dir_for(&tmp), None);
        std::fs::write(tmp.join(".modules.yaml"), "storeDir: /s/v10\n").unwrap();
        assert_eq!(store_dir_for(&tmp), Some(PathBuf::from("/s/v10")));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
