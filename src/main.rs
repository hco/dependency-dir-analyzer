mod cli;
mod report;
mod scanner;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Format};
use crate::report::Report;
use crate::scanner::{scan, ScanOptions};

fn main() -> Result<()> {
    let args = Cli::parse();

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
    let mut directories = scan(&args.path, &opts);

    if min_size > 0 {
        directories.retain(|d| d.size_bytes >= min_size);
    }
    if args.top > 0 && directories.len() > args.top {
        directories.truncate(args.top);
    }

    let root = args.path.display().to_string();
    let report = Report::new(&root, &directories);

    match args.format {
        Format::Table => report::print_table(&report, !args.no_summary)
            .context("failed to write table output")?,
        Format::Json => report::print_json(&report).context("failed to write JSON output")?,
    }

    Ok(())
}
