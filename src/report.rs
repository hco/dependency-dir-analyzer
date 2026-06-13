use std::io::{self, Write};

use anyhow::Result;
use humansize::{format_size, DECIMAL};
use serde::Serialize;

use crate::scanner::DependencyDir;

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

/// Write the report as pretty-printed JSON.
pub fn write_json(w: &mut impl Write, report: &Report) -> Result<()> {
    serde_json::to_writer_pretty(&mut *w, report)?;
    writeln!(w)?;
    Ok(())
}

/// Write the report as an aligned, human-readable table.
pub fn write_table(w: &mut impl Write, report: &Report, show_summary: bool) -> Result<()> {
    if report.directories.is_empty() {
        writeln!(w, "No dependency directories found under {}.", report.root)?;
        return Ok(());
    }

    // Pre-format the size column so we can right-align it to a common width.
    let sizes: Vec<String> = report
        .directories
        .iter()
        .map(|d| format_size(d.size_bytes, DECIMAL))
        .collect();
    let size_width = sizes.iter().map(|s| s.len()).max().unwrap_or(0);
    let eco_width = report
        .directories
        .iter()
        .map(|d| d.ecosystem.len())
        .max()
        .unwrap_or(0);

    for (dir, size) in report.directories.iter().zip(&sizes) {
        writeln!(
            w,
            "{size:>size_width$}  {eco:<eco_width$}  {path}",
            size = size,
            eco = dir.ecosystem,
            path = dir.path.display(),
        )?;
    }

    if show_summary {
        let total = format_size(report.total_size_bytes, DECIMAL);
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

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.downcast_ref::<io::Error>()
        .map(|e| e.kind() == io::ErrorKind::BrokenPipe)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample() -> Vec<DependencyDir> {
        vec![
            DependencyDir {
                path: PathBuf::from("./a/node_modules"),
                ecosystem: "Node.js",
                name: "node_modules",
                size_bytes: 2_000_000,
                file_count: 10,
            },
            DependencyDir {
                path: PathBuf::from("./b/target"),
                ecosystem: "Rust / Maven",
                name: "target",
                size_bytes: 500_000,
                file_count: 4,
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
    }
}
