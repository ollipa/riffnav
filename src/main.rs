mod app;
mod autodiff;
mod cli;
mod cmd;
mod comment;
mod config;
mod delta;
mod diff;
mod forge;
mod herdr;
mod icons;
mod review;
mod session;
mod state;
mod theme;
mod tree;
mod ui;

use std::io::{IsTerminal, Read};

use anyhow::{Context, Result, bail};
use clap::Parser;

use autodiff::DiffSource;

/// Where the initial diff came from, plus the auto-diff context to carry into the
/// app (the active source and detected base) when launched bare.
struct Input {
    text: String,
    autodiff: Option<(DiffSource, Option<String>)>,
}

/// Decide where the diff comes from and load it:
/// - bare launch (stdin is a terminal): auto-diff from the current git repo;
/// - otherwise: a unified diff piped/redirected on stdin (the original path).
fn acquire(cli: &cli::Cli, config: &config::Config) -> Result<Input> {
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

    // The `comment`/`skill` subcommands are plain CLI tools: no TUI, no diff on
    // stdin, no delta. They talk to the same on-disk state a running window does.
    if let Some(command) = cli.command {
        return cmd::run(command);
    }

    let config = config::Config::load(cli.config.as_deref())?;

    let input = acquire(&cli, &config)?;

    // `--list` is a debug helper: print the parsed files for whatever source was
    // selected (piped diff, or the auto-diff on a bare launch) and exit.
    if cli.list {
        cli::print_list(&diff::parse(&input.text));
        return Ok(());
    }

    let files = diff::parse(&input.text);

    if files.is_empty() {
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
    if let Some((source, base)) = input.autodiff {
        app.enable_autodiff(source, base);
    }
    app.enable_herdr();
    app.enable_forge();
    app.enable_review_sync(config.review_sync_github);
    app.enable_review(config.review_retention_days);
    app.enable_comments(
        config.show_comments,
        config.comment_retention_days,
        config.comment_author.as_deref(),
    );
    app.run()
}
