use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use crate::state::IconStyle;

/// Scan a directory tree for dependency directories and report how much space
/// they use.
///
/// A "dependency directory" is a folder that a package manager or build tool
/// generates and that can usually be regenerated from source (for example
/// `node_modules`, `target`, `.venv` or `vendor`). This tool finds them,
/// totals their size, shows when each was last touched, and helps you reclaim
/// disk space.
///
/// Run without arguments to scan the current directory in an interactive,
/// live-updating terminal UI. Pipe the output or pass `--format json|csv` for
/// machine-readable results.
#[derive(Debug, Parser)]
#[command(name = "dda", version, about, long_about = None)]
pub struct Cli {
    /// Root directory to scan.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Output format. `table` shows an interactive TUI on a terminal (or a
    /// plain table when piped); `json` and `csv` are always machine-readable.
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

    /// Ecosystem icon style in the interactive UI (emoji, nerd or off).
    /// Overrides the remembered setting for this run; toggle live with `i`.
    #[arg(long, value_enum, value_name = "STYLE")]
    pub icons: Option<IconStyle>,

    /// Print a plain static table instead of the interactive TUI, even on a
    /// terminal.
    #[arg(long)]
    pub plain: bool,

    /// Disable mouse capture in the interactive UI, so the terminal's own
    /// click-drag text selection and scrollback keep working. Also settable
    /// with the `DDA_NO_MOUSE` environment variable or the persisted `mouse`
    /// config setting; this flag overrides both.
    #[arg(long)]
    pub no_mouse: bool,

    /// Do not print the summary footer (plain table / non-interactive output).
    #[arg(long)]
    pub no_summary: bool,

    /// Generate shell completion scripts and other utilities.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print a shell completion script to stdout.
    ///
    /// Example (bash):  `dda completions bash > /etc/bash_completion.d/dda`
    /// Example (zsh):   `dda completions zsh  > "${fpath[1]}/_dda"`
    /// Example (fish):  `dda completions fish > ~/.config/fish/completions/dda.fish`
    Completions {
        /// Shell to generate completions for.
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Interactive TUI on a terminal, or an aligned plain table when piped.
    Table,
    /// Machine-readable JSON.
    Json,
    /// Machine-readable CSV.
    Csv,
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

    #[test]
    fn verifies_cli() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
