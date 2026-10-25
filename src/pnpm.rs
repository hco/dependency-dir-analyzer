//! pnpm's content-addressable store: finding the stores projects use, so the
//! space they hold shows up as rows of their own.

use std::path::{Path, PathBuf};

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
