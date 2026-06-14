mod app;
mod cli;
mod delta;
mod diff;
mod tree;
mod ui;

use std::io::Read;

use anyhow::{Context, Result};
use clap::Parser;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("failed to read diff from stdin")?;

    let files = diff::parse(&input);

    if cli.list {
        cli::print_list(&files);
        return Ok(());
    }

    if files.is_empty() {
        eprintln!("riffnav: no changes to display");
        return Ok(());
    }

    delta::ensure_available()?;

    let side_by_side = cli.side_by_side.then_some(true);
    app::App::new(files, side_by_side).run()
}
