use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Scan a directory tree for dependency directories and report how much space
/// they use.
///
/// A "dependency directory" is a folder that a package manager or build tool
/// generates and that can usually be regenerated from source (for example
/// `node_modules`, `target`, `.venv` or `vendor`). This tool finds them,
/// totals their size and shows you where your disk space went.
#[derive(Debug, Parser)]
#[command(name = "dda", version, about, long_about = None)]
pub struct Cli {
    /// Root directory to scan.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Output format.
    #[arg(short, long, value_enum, default_value_t = Format::Table)]
    pub format: Format,

    /// Only report directories at least this large (e.g. `10MB`, `1.5GB`, `500K`).
    #[arg(short = 'm', long, value_name = "SIZE")]
    pub min_size: Option<String>,

    /// Show at most this many results (largest first). 0 means no limit.
    #[arg(short, long, default_value_t = 0)]
    pub top: usize,

    /// Follow symbolic links while scanning.
    #[arg(long)]
    pub follow_symlinks: bool,

    /// Do not print the summary footer (table format only).
    #[arg(long)]
    pub no_summary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Human-readable, aligned columns.
    Table,
    /// Machine-readable JSON.
    Json,
}

/// Parse a human-friendly size string such as `10MB`, `1.5GiB` or `500k` into
/// a number of bytes.
///
/// Both decimal (KB = 1000) and binary (KiB = 1024) suffixes are accepted, and
/// the suffix is case-insensitive. A bare number is interpreted as bytes.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("empty size".to_string());
    }

    let split = trimmed
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split);

    let value: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("invalid number in size '{input}'"))?;
    if value < 0.0 {
        return Err(format!("size cannot be negative: '{input}'"));
    }

    let multiplier: f64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1_000.0,
        "kib" => 1_024.0,
        "m" | "mb" => 1_000_000.0,
        "mib" => 1_048_576.0,
        "g" | "gb" => 1_000_000_000.0,
        "gib" => 1_073_741_824.0,
        "t" | "tb" => 1_000_000_000_000.0,
        "tib" => 1_099_511_627_776.0,
        other => return Err(format!("unknown size suffix '{other}' in '{input}'")),
    };

    Ok((value * multiplier).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_bytes() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
    }

    #[test]
    fn parses_decimal_suffixes() {
        assert_eq!(parse_size("10MB").unwrap(), 10_000_000);
        assert_eq!(parse_size("1.5GB").unwrap(), 1_500_000_000);
        assert_eq!(parse_size("500k").unwrap(), 500_000);
    }

    #[test]
    fn parses_binary_suffixes() {
        assert_eq!(parse_size("1KiB").unwrap(), 1024);
        assert_eq!(parse_size("1MiB").unwrap(), 1_048_576);
    }

    #[test]
    fn is_case_insensitive_and_trims() {
        assert_eq!(parse_size("  2gb ").unwrap(), 2_000_000_000);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10XB").is_err());
        assert!(parse_size("-5MB").is_err());
    }
}
