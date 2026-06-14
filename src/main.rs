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
        let results =
            tui::run(&args.path, opts, args.top, min_size).context("interactive UI failed")?;
        // Leave a static summary in the scrollback after the alt-screen UI exits.
        let report = Report::new(&root, &results);
        report::print_table(&report, !args.no_summary)?;
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
