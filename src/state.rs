//! Persistence of the interactive view preferences (sort, grouping, filters and
//! folded groups) so they survive across restarts, keyed by the scan root.
//!
//! The store is a single JSON file mapping a canonicalised directory path to the
//! [`ViewState`] last used there. All filesystem access is best-effort: a
//! missing, unreadable or malformed store simply yields defaults, and write
//! failures are ignored — persisting view preferences is a convenience, never a
//! reason to fail a scan.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The column the table is sorted by. Cycled with `s` in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SortKey {
    #[default]
    Size,
    Modified,
    Name,
    Files,
}

impl SortKey {
    pub fn next(self) -> Self {
        match self {
            SortKey::Size => SortKey::Modified,
            SortKey::Modified => SortKey::Name,
            SortKey::Name => SortKey::Files,
            SortKey::Files => SortKey::Size,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Name => "name",
            SortKey::Files => "files",
        }
    }
}

/// Minimum-size threshold cycler applied on top of the CLI `--min-size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SizeFilter {
    #[default]
    All,
    Mb1,
    Mb10,
    Mb100,
    Gb1,
}

impl SizeFilter {
    pub fn next(self) -> Self {
        match self {
            SizeFilter::All => SizeFilter::Mb1,
            SizeFilter::Mb1 => SizeFilter::Mb10,
            SizeFilter::Mb10 => SizeFilter::Mb100,
            SizeFilter::Mb100 => SizeFilter::Gb1,
            SizeFilter::Gb1 => SizeFilter::All,
        }
    }

    pub fn min_bytes(self) -> u64 {
        match self {
            SizeFilter::All => 0,
            SizeFilter::Mb1 => 1_000_000,
            SizeFilter::Mb10 => 10_000_000,
            SizeFilter::Mb100 => 100_000_000,
            SizeFilter::Gb1 => 1_000_000_000,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SizeFilter::All => "all",
            SizeFilter::Mb1 => "≥1 MB",
            SizeFilter::Mb10 => "≥10 MB",
            SizeFilter::Mb100 => "≥100 MB",
            SizeFilter::Gb1 => "≥1 GB",
        }
    }
}

/// "Modified older than" threshold cycler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AgeFilter {
    #[default]
    Any,
    Day1,
    Week1,
    Month1,
    Month6,
    Year1,
}

impl AgeFilter {
    pub fn next(self) -> Self {
        match self {
            AgeFilter::Any => AgeFilter::Day1,
            AgeFilter::Day1 => AgeFilter::Week1,
            AgeFilter::Week1 => AgeFilter::Month1,
            AgeFilter::Month1 => AgeFilter::Month6,
            AgeFilter::Month6 => AgeFilter::Year1,
            AgeFilter::Year1 => AgeFilter::Any,
        }
    }

    /// Minimum age in seconds a directory must have to pass, or `None` for "any".
    pub fn min_age_secs(self) -> Option<u64> {
        const DAY: u64 = 86_400;
        match self {
            AgeFilter::Any => None,
            AgeFilter::Day1 => Some(DAY),
            AgeFilter::Week1 => Some(7 * DAY),
            AgeFilter::Month1 => Some(30 * DAY),
            AgeFilter::Month6 => Some(182 * DAY),
            AgeFilter::Year1 => Some(365 * DAY),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgeFilter::Any => "any",
            AgeFilter::Day1 => ">1d",
            AgeFilter::Week1 => ">1w",
            AgeFilter::Month1 => ">1mo",
            AgeFilter::Month6 => ">6mo",
            AgeFilter::Year1 => ">1y",
        }
    }
}

/// The persisted, restorable view preferences for one scan root.
///
/// Ecosystems are stored by their string label; on load these are matched
/// against whatever the next scan actually finds, so a label that no longer
/// appears is simply inert rather than an error.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewState {
    pub sort: SortKey,
    pub group: bool,
    pub size_filter: SizeFilter,
    pub age_filter: AgeFilter,
    pub disabled_ecos: HashSet<String>,
    pub collapsed_ecos: HashSet<String>,
}

/// Load the saved view state for `root`, or defaults if none is stored.
pub fn load(root: &Path) -> ViewState {
    let key = key_for(root);
    load_all().remove(&key).unwrap_or_default()
}

/// Persist `state` for `root`, leaving every other directory's entry untouched.
/// Errors are intentionally swallowed — this is a best-effort convenience.
pub fn save(root: &Path, state: &ViewState) {
    let Some(path) = store_path() else {
        return;
    };
    let mut all = load_all();
    all.insert(key_for(root), state.clone());

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(&all) {
        let _ = std::fs::write(&path, json);
    }
}

/// Read the whole directory→state map, or an empty map if it can't be read.
fn load_all() -> HashMap<String, ViewState> {
    store_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// A stable key for a scan root: its canonical absolute path when resolvable,
/// otherwise the path as given.
fn key_for(root: &Path) -> String {
    std::fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Location of the JSON store, following the platform's conventional per-user
/// state directory. Returns `None` only when no home/state directory can be
/// determined, in which case persistence is silently disabled.
fn store_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        Some(PathBuf::from(xdg))
    } else {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
    };
    base.map(|b| b.join("dda").join("view-state.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_key_cycles() {
        let mut k = SortKey::Size;
        k = k.next();
        assert_eq!(k, SortKey::Modified);
        k = k.next().next().next();
        assert_eq!(k, SortKey::Size);
    }

    #[test]
    fn size_filter_cycles_and_thresholds_increase() {
        let mut f = SizeFilter::All;
        assert_eq!(f.min_bytes(), 0);
        f = f.next();
        assert_eq!(f, SizeFilter::Mb1);
        // Cycling all the way around returns to All.
        let back = f.next().next().next().next();
        assert_eq!(back, SizeFilter::All);
    }

    #[test]
    fn view_state_round_trips_through_json() {
        let mut state = ViewState {
            sort: SortKey::Modified,
            group: true,
            size_filter: SizeFilter::Mb10,
            age_filter: AgeFilter::Month1,
            ..Default::default()
        };
        state.disabled_ecos.insert("Node.js".to_string());
        state.collapsed_ecos.insert("Rust / Maven".to_string());

        let json = serde_json::to_vec(&state).unwrap();
        let back: ViewState = serde_json::from_slice(&json).unwrap();

        assert_eq!(back.sort, SortKey::Modified);
        assert!(back.group);
        assert_eq!(back.size_filter, SizeFilter::Mb10);
        assert_eq!(back.age_filter, AgeFilter::Month1);
        assert!(back.disabled_ecos.contains("Node.js"));
        assert!(back.collapsed_ecos.contains("Rust / Maven"));
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // An older or partial store should deserialize, filling gaps with
        // defaults rather than failing.
        let back: ViewState = serde_json::from_str("{\"group\":true}").unwrap();
        assert!(back.group);
        assert_eq!(back.sort, SortKey::Size);
        assert_eq!(back.size_filter, SizeFilter::All);
        assert_eq!(back.age_filter, AgeFilter::Any);
        assert!(back.disabled_ecos.is_empty());
    }
}
