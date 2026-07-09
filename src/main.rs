mod cli;
mod report;
mod scanner;
mod state;
mod tui;

use std::io::{self, IsTerminal};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use clap_complete::{generate, Shell};

use crate::cli::{Cli, Command, Format};
use crate::report::Report;
use crate::scanner::{scan, ScanOptions};

fn main() -> Result<()> {
    let args = Cli::parse();

    // Utility subcommands short-circuit the scan.
    if let Some(Command::Completions { shell }) = args.command {
        print_completions(shell);
        return Ok(());
    }

    if !args.path.exists() {
        anyhow::bail!("path does not exist: {}", args.path.display());
    }
    if !args.path.is_dir() {
        anyhow::bail!("path is not a directory: {}", args.path.display());
    }

    let min_size = match &args.min_size {
        Some(s) => cli::parse_size(s).map_err(|e| anyhow::anyhow!(e))?,
        None => 0,
    };

    let opts = ScanOptions {
        follow_symlinks: args.follow_symlinks,
    };
    let root = args.path.display().to_string();

    // On a terminal, the default `table` format launches the interactive,
    // live-updating UI. When piped, or with `--plain`, or for the machine
    // formats, we run a batch scan and print once.
    let interactive = args.format == Format::Table && !args.plain && io::stdout().is_terminal();

    if interactive {
        let mouse = resolve_mouse(args.no_mouse);
        tui::run(&args.path, opts, args.top, min_size, args.icons, mouse)
            .context("interactive UI failed")?;
        return Ok(());
    }

    let mut directories = scan(&args.path, &opts);
    if min_size > 0 {
        directories.retain(|d| d.size_bytes >= min_size);
    }
    if args.top > 0 && directories.len() > args.top {
        directories.truncate(args.top);
    }

    let report = Report::new(&root, &directories);
    match args.format {
        Format::Table => report::print_table(&report, !args.no_summary)
            .context("failed to write table output")?,
        Format::Json => report::print_json(&report).context("failed to write JSON output")?,
        Format::Csv => report::print_csv(&report).context("failed to write CSV output")?,
    }

    Ok(())
}

/// Decide whether the interactive UI should capture the mouse, honouring — in
/// priority order — the `--no-mouse` flag, the `DDA_NO_MOUSE` environment
/// variable, then the persisted `mouse` config setting (default: enabled).
fn resolve_mouse(no_mouse_flag: bool) -> bool {
    if no_mouse_flag {
        return false;
    }
    // The env var names the "no mouse" intent: truthy disables, falsy forces it
    // back on (overriding a config that turned it off); anything else defers.
    if let Some(no_mouse) = std::env::var("DDA_NO_MOUSE")
        .ok()
        .and_then(|raw| parse_env_bool(&raw))
    {
        return !no_mouse;
    }
    crate::state::settings().mouse
}

/// Parse a boolean-ish environment variable value, returning `None` for unset,
/// empty or unrecognised input so the caller can fall through to the next layer.
fn parse_env_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Print a shell completion script for `shell` to stdout.
///
/// The script is generated into a buffer first and then written out, so a
/// closed pipe (e.g. `dda completions bash | head`) is handled gracefully
/// instead of panicking inside the generator.
fn print_completions(shell: Shell) {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    let mut buf: Vec<u8> = Vec::new();
    generate(shell, &mut cmd, name, &mut buf);

    use std::io::Write;
    if let Err(e) = io::stdout().write_all(&buf) {
        if e.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("error writing completions: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_bool_recognises_common_spellings() {
        for on in ["1", "true", "TRUE", "yes", "on", " On "] {
            assert_eq!(parse_env_bool(on), Some(true), "{on:?}");
        }
        for off in ["0", "false", "no", "off"] {
            assert_eq!(parse_env_bool(off), Some(false), "{off:?}");
        }
        // Unset/empty/garbage defers to the next layer.
        for other in ["", "maybe", "2"] {
            assert_eq!(parse_env_bool(other), None, "{other:?}");
        }
    }

    #[test]
    fn no_mouse_flag_beats_everything() {
        // The flag short-circuits before the env var or config are consulted.
        assert!(!resolve_mouse(true));
    }
}
