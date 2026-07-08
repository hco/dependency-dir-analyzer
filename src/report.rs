use std::io::{self, Write};
use std::time::SystemTime;

use anyhow::Result;
use humansize::{format_size, DECIMAL};
use serde::Serialize;

use crate::scanner::{rfc3339_utc, unix_seconds, DependencyDir};

/// The full result of a scan, suitable for serialization.
#[derive(Debug, Serialize)]
pub struct Report<'a> {
    pub root: String,
    pub total_size_bytes: u64,
    pub total_file_count: u64,
    pub directory_count: usize,
    pub directories: &'a [DependencyDir],
}

impl<'a> Report<'a> {
    pub fn new(root: &str, directories: &'a [DependencyDir]) -> Self {
        let total_size_bytes = directories.iter().map(|d| d.size_bytes).sum();
        let total_file_count = directories.iter().map(|d| d.file_count).sum();
        Report {
            root: root.to_string(),
            total_size_bytes,
            total_file_count,
            directory_count: directories.len(),
            directories,
        }
    }
}

/// Render a size in human-readable, decimal (KB = 1000) units.
pub fn human_size(bytes: u64) -> String {
    format_size(bytes, DECIMAL)
}

/// Render a size as whole (decimal) megabytes with thousands separators, e.g.
/// `12,300 MB`. Rounds to the nearest megabyte, so the column stays directly
/// comparable without decimal noise.
pub fn megabytes_size(bytes: u64) -> String {
    let mb = (bytes as f64 / 1_000_000.0).round() as u64;
    format!("{} MB", group_thousands(mb))
}

/// Format an integer with `,` grouping every three digits (e.g. `1,234,567`).
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i != 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// Render a modification time as a short, human-friendly relative age such as
/// `3d ago`, `5mo ago` or `just now`, measured against [`SystemTime::now`].
/// Returns `"—"` when the timestamp is unknown.
pub fn relative_age(modified: Option<SystemTime>) -> String {
    relative_age_from(modified, SystemTime::now())
}

/// Like [`relative_age`] but against an explicit `now`, so it can be tested
/// deterministically.
pub fn relative_age_from(modified: Option<SystemTime>, now: SystemTime) -> String {
    let Some(modified) = modified else {
        return "—".to_string();
    };
    let secs = match now.duration_since(modified) {
        Ok(d) => d.as_secs(),
        // Modified in the (clock-skew) future: treat as just now.
        Err(_) => return "just now".to_string(),
    };

    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;
    const YEAR: u64 = 365 * DAY;

    let (value, unit) = match secs {
        s if s < MINUTE => return "just now".to_string(),
        s if s < HOUR => (s / MINUTE, "m"),
        s if s < DAY => (s / HOUR, "h"),
        s if s < WEEK => (s / DAY, "d"),
        s if s < MONTH => (s / WEEK, "w"),
        s if s < YEAR => (s / MONTH, "mo"),
        s => (s / YEAR, "y"),
    };
    format!("{value}{unit} ago")
}

/// Write the report as pretty-printed JSON.
pub fn write_json(w: &mut impl Write, report: &Report) -> Result<()> {
    serde_json::to_writer_pretty(&mut *w, report)?;
    writeln!(w)?;
    Ok(())
}

/// Write the report as CSV with a header row. Paths are quoted by the `csv`
/// crate as needed, so paths containing commas or quotes are handled safely.
pub fn write_csv(w: &mut impl Write, report: &Report) -> Result<()> {
    let mut wtr = csv::Writer::from_writer(w);
    wtr.write_record([
        "path",
        "ecosystem",
        "name",
        "size_bytes",
        "file_count",
        "modified_unix",
        "modified_rfc3339",
    ])?;

    for d in report.directories {
        let (unix, rfc) = match d.modified.and_then(unix_seconds) {
            Some(secs) => (secs.to_string(), rfc3339_utc(secs)),
            None => (String::new(), String::new()),
        };
        wtr.write_record([
            &d.path.display().to_string(),
            d.ecosystem,
            d.name,
            &d.size_bytes.to_string(),
            &d.file_count.to_string(),
            &unix,
            &rfc,
        ])?;
    }
    wtr.flush()?;
    Ok(())
}

/// Write the report as an aligned, human-readable table.
pub fn write_table(w: &mut impl Write, report: &Report, show_summary: bool) -> Result<()> {
    if report.directories.is_empty() {
        writeln!(w, "No dependency directories found under {}.", report.root)?;
        return Ok(());
    }

    // Pre-format the columns so we can align each one to a common width.
    let sizes: Vec<String> = report
        .directories
        .iter()
        .map(|d| human_size(d.size_bytes))
        .collect();
    let ages: Vec<String> = report
        .directories
        .iter()
        .map(|d| relative_age(d.modified))
        .collect();
    let size_width = sizes.iter().map(|s| s.len()).max().unwrap_or(0);
    let age_width = ages.iter().map(|s| s.len()).max().unwrap_or(0);
    let eco_width = report
        .directories
        .iter()
        .map(|d| d.ecosystem.len())
        .max()
        .unwrap_or(0);

    for ((dir, size), age) in report.directories.iter().zip(&sizes).zip(&ages) {
        writeln!(
            w,
            "{size:>size_width$}  {age:>age_width$}  {eco:<eco_width$}  {path}",
            size = size,
            age = age,
            eco = dir.ecosystem,
            path = dir.path.display(),
        )?;
    }

    if show_summary {
        let total = human_size(report.total_size_bytes);
        writeln!(w)?;
        writeln!(
            w,
            "{} across {} director{} ({} files)",
            total,
            report.directory_count,
            if report.directory_count == 1 {
                "y"
            } else {
                "ies"
            },
            report.total_file_count,
        )?;
    }

    Ok(())
}

/// Convenience helper that writes to stdout, ignoring broken-pipe errors (so
/// piping into `head` and friends doesn't print a scary error).
pub fn print_table(report: &Report, show_summary: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match write_table(&mut lock, report, show_summary) {
        Err(e) if is_broken_pipe(&e) => Ok(()),
        other => other,
    }
}

/// Convenience helper that writes JSON to stdout.
pub fn print_json(report: &Report) -> Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match write_json(&mut lock, report) {
        Err(e) if is_broken_pipe(&e) => Ok(()),
        other => other,
    }
}

/// Convenience helper that writes CSV to stdout.
pub fn print_csv(report: &Report) -> Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match write_csv(&mut lock, report) {
        Err(e) if is_broken_pipe(&e) => Ok(()),
        other => other,
    }
}

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    if let Some(e) = err.downcast_ref::<io::Error>() {
        return e.kind() == io::ErrorKind::BrokenPipe;
    }
    // serde_json wraps the underlying I/O error rather than exposing it as an
    // `io::Error`, so check it separately (e.g. `dda --format json | head`).
    if let Some(e) = err.downcast_ref::<serde_json::Error>() {
        return e.io_error_kind() == Some(io::ErrorKind::BrokenPipe);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn sample() -> Vec<DependencyDir> {
        vec![
            DependencyDir {
                path: PathBuf::from("./a/node_modules"),
                ecosystem: "Node.js",
                name: "node_modules",
                size_bytes: 2_000_000,
                file_count: 10,
                modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            },
            DependencyDir {
                path: PathBuf::from("./b/target"),
                ecosystem: "Rust / Maven",
                name: "target",
                size_bytes: 500_000,
                file_count: 4,
                modified: None,
            },
        ]
    }

    #[test]
    fn report_totals_are_summed() {
        let dirs = sample();
        let report = Report::new(".", &dirs);
        assert_eq!(report.total_size_bytes, 2_500_000);
        assert_eq!(report.total_file_count, 14);
        assert_eq!(report.directory_count, 2);
    }

    #[test]
    fn table_contains_paths_and_summary() {
        let dirs = sample();
        let report = Report::new(".", &dirs);
        let mut buf = Vec::new();
        write_table(&mut buf, &report, true).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("node_modules"));
        assert!(out.contains("target"));
        assert!(out.contains("across 2 directories"));
    }

    #[test]
    fn empty_report_is_friendly() {
        let dirs: Vec<DependencyDir> = Vec::new();
        let report = Report::new("/tmp", &dirs);
        let mut buf = Vec::new();
        write_table(&mut buf, &report, true).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("No dependency directories found"));
    }

    #[test]
    fn json_is_valid() {
        let dirs = sample();
        let report = Report::new(".", &dirs);
        let mut buf = Vec::new();
        write_json(&mut buf, &report).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(value["directory_count"], 2);
        assert_eq!(value["total_size_bytes"], 2_500_000);
        // The first directory carries a structured timestamp, the second null.
        assert_eq!(
            value["directories"][0]["modified"]["unix"],
            1_700_000_000_i64
        );
        assert!(value["directories"][1]["modified"].is_null());
    }

    #[test]
    fn csv_has_header_and_rows() {
        let dirs = sample();
        let report = Report::new(".", &dirs);
        let mut buf = Vec::new();
        write_csv(&mut buf, &report).unwrap();
        let out = String::from_utf8(buf).unwrap();
        let mut lines = out.lines();
        assert_eq!(
            lines.next().unwrap(),
            "path,ecosystem,name,size_bytes,file_count,modified_unix,modified_rfc3339"
        );
        assert!(out.contains("node_modules"));
        assert!(out.contains("2000000"));
        // The directory with no timestamp leaves the two trailing time columns
        // empty.
        assert!(out.contains("500000,4,,"));
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(42), "42");
        assert_eq!(group_thousands(1_234), "1,234");
        assert_eq!(group_thousands(12_345), "12,345");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn megabytes_round_and_group() {
        assert_eq!(megabytes_size(0), "0 MB");
        assert_eq!(megabytes_size(512), "0 MB");
        assert_eq!(megabytes_size(2_000_000), "2 MB");
        // 1.53 MB rounds up to 2 MB — no decimals in this mode.
        assert_eq!(megabytes_size(1_530_000), "2 MB");
        assert_eq!(megabytes_size(12_300_000_000), "12,300 MB");
    }

    #[test]
    fn relative_age_buckets() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let ago = |secs: u64| now - Duration::from_secs(secs);
        assert_eq!(relative_age_from(None, now), "—");
        assert_eq!(relative_age_from(Some(ago(10)), now), "just now");
        assert_eq!(relative_age_from(Some(ago(120)), now), "2m ago");
        assert_eq!(relative_age_from(Some(ago(3 * 3600)), now), "3h ago");
        assert_eq!(relative_age_from(Some(ago(2 * 86400)), now), "2d ago");
        assert_eq!(relative_age_from(Some(ago(3 * 7 * 86400)), now), "3w ago");
        assert_eq!(relative_age_from(Some(ago(2 * 30 * 86400)), now), "2mo ago");
        assert_eq!(relative_age_from(Some(ago(2 * 365 * 86400)), now), "2y ago");
    }
}
