mod app;
mod cli;
mod config;
mod delta;
mod diff;
mod herdr;
mod icons;
mod tree;
mod ui;
mod watch;

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

const DEFAULT_WATCH_CMD: &str = "git diff";
const DEFAULT_WATCH_INTERVAL: f64 = 2.0;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let config = config::Config::load(cli.config.as_deref())?;

    // `--list` is a debug helper that always reads the diff from stdin.
    if cli.list {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .context("failed to read diff from stdin")?;
        cli::print_list(&diff::parse(&input));
        return Ok(());
    }

    let watch_cmd = cli
        .watch_cmd
        .clone()
        .unwrap_or_else(|| DEFAULT_WATCH_CMD.to_string());

    // Source the initial diff: the watch command in watch mode (stdin can only be
    // read once, so it's unusable as a refreshable source), else stdin.
    let input = if cli.watch {
        watch::run_once(&watch_cmd).context("failed to run watch command")?
    } else {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("failed to read diff from stdin")?;
        s
    };

    let files = diff::parse(&input);

    // Without watch mode an empty diff is a no-op; with it we stay open and wait.
    if files.is_empty() && !cli.watch {
        eprintln!("riffnav: no changes to display");
        return Ok(());
    }

    delta::ensure_available()?;

    // Layout precedence: CLI -s/-u win, then the config file, then the user's
    // delta.side-by-side default.
    let config_sbs = delta::detect_side_by_side();
    let side_by_side = if cli.side_by_side {
        true
    } else if cli.unified {
        false
    } else {
        config.side_by_side.unwrap_or(config_sbs)
    };

    let mut app = app::App::new(files, side_by_side, config_sbs, &config);
    if cli.watch {
        let secs = cli
            .watch_interval
            .unwrap_or(DEFAULT_WATCH_INTERVAL)
            .max(0.05);
        app.enable_watch(watch_cmd, Duration::from_secs_f64(secs), input)?;
    }
    app.enable_herdr();
    app.run()
}
