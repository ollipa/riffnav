mod app;
mod autodiff;
mod cli;
mod config;
mod delta;
mod diff;
mod forge;
mod herdr;
mod icons;
mod review;
mod theme;
mod tree;
mod ui;
mod watch;

use std::io::{IsTerminal, Read};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;

use autodiff::DiffSource;

const DEFAULT_WATCH_CMD: &str = "git diff";
const DEFAULT_WATCH_INTERVAL: f64 = 2.0;

/// Where the initial diff came from, plus the auto-diff context to carry into the
/// app (the active source and detected base) when launched bare.
struct Input {
    text: String,
    autodiff: Option<(DiffSource, Option<String>)>,
}

/// Decide where the diff comes from and load it:
/// - `--watch`: the watch command (stdin can only be read once, so it can't be a
///   refreshable source);
/// - bare launch (stdin is a terminal): auto-diff from the current git repo;
/// - otherwise: a unified diff piped/redirected on stdin (the original path).
fn acquire(cli: &cli::Cli, config: &config::Config, watch_cmd: &str) -> Result<Input> {
    if cli.watch {
        let text = watch::run_once(watch_cmd).context("failed to run watch command")?;
        return Ok(Input {
            text,
            autodiff: None,
        });
    }

    if std::io::stdin().is_terminal() {
        if !autodiff::in_repo() {
            bail!(
                "no diff on stdin and not inside a git repository\n\
                 pipe a unified diff (e.g. `git diff | riffnav`) or run inside a repo"
            );
        }
        // Base and starting view resolve as detect/adaptive < config < CLI.
        let base = cli
            .base
            .clone()
            .or_else(|| config.base_branch.clone())
            .or_else(autodiff::detect_base);
        let (source, text) = match cli.diff.or(config.diff_source) {
            Some(source) => (source, autodiff::load(source, base.as_deref())?),
            None => autodiff::load_initial(base.as_deref())?,
        };
        return Ok(Input {
            text,
            autodiff: Some((source, base)),
        });
    }

    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("failed to read diff from stdin")?;
    Ok(Input {
        text,
        autodiff: None,
    })
}

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let config = config::Config::load(cli.config.as_deref())?;

    let watch_cmd = cli
        .watch_cmd
        .clone()
        .unwrap_or_else(|| DEFAULT_WATCH_CMD.to_string());

    let input = acquire(&cli, &config, &watch_cmd)?;

    // `--list` is a debug helper: print the parsed files for whatever source was
    // selected (piped diff, or the auto-diff on a bare launch) and exit.
    if cli.list {
        cli::print_list(&diff::parse(&input.text));
        return Ok(());
    }

    let files = diff::parse(&input.text);

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
        app.enable_watch(watch_cmd, Duration::from_secs_f64(secs), input.text)?;
    }
    if let Some((source, base)) = input.autodiff {
        app.enable_autodiff(source, base);
    }
    app.enable_herdr();
    app.enable_forge();
    app.enable_review(config.review_retention_days);
    app.run()
}
