mod app;
mod cli;
mod delta;
mod diff;
mod icons;
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

    // Initial layout: CLI flags win, else follow the user's delta config default.
    let config_sbs = delta::detect_side_by_side();
    let side_by_side = if cli.side_by_side {
        true
    } else if cli.unified {
        false
    } else {
        config_sbs
    };
    app::App::new(files, side_by_side, config_sbs).run()
}
