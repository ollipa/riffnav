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

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};

use autodiff::{DiffSource, GitDiff};

/// Where the diff came from: its text, plus the git command that produced it
/// (when there is one) so the app can run it again.
struct Input {
    text: String,
    git: Option<GitDiff>,
}

/// Build the git command `riffnav diff` asks for. The view resolves as
/// unstaged < config < flag, and the base as detect < config < `--base`, so a
/// bare `riffnav diff` matches `git diff` unless the user says otherwise.
///
/// Pass-through arguments name their own revisions, so a `diff_source` set in
/// the config file is skipped once there are any: stacking a configured
/// selector onto the user's would quietly turn `riffnav diff HEAD~1` into
/// `git diff --staged HEAD~1`. A view flag still wins — that one was typed
/// alongside the arguments, so it was meant.
///
/// The flag returned alongside says whether the view was riffnav's own default
/// rather than the user's choice, which is what licenses the empty-diff fallback
/// in [`diff_input`].
///
/// The base is resolved for every view, not just the base-relative ones, so `d`
/// and the number keys can reach those without re-detecting it mid-session.
fn diff_command(args: cli::DiffArgs, config: &config::Config) -> (GitDiff, bool) {
    let configured = args.args.is_empty().then_some(config.diff_source).flatten();
    let chosen = args.view().or(configured);
    let defaulted = chosen.is_none() && args.args.is_empty();
    let source = chosen.unwrap_or(DiffSource::Unstaged);
    let base = args
        .base
        .or_else(|| config.base_branch.clone())
        .or_else(autodiff::detect_base);
    (
        GitDiff::diff(source, base, cli::git_args(args.args)),
        defaulted,
    )
}

/// Load the diff for `riffnav diff`, stepping to another view when the defaulted
/// one is empty.
///
/// A bare `riffnav diff` means `git diff`: unstaged work. On a clean tree that
/// is nothing at all, and printing "no changes to display" in a repo whose
/// branch is full of commits is unhelpful — what the user wants to read there is
/// what the branch adds over its base, the same diff `riffnav diff --vs-base`
/// shows. So an *empty* default steps on to the narrowest view that shows
/// everything there is (see [`autodiff::fallback_view`]). A view the user named
/// is left alone, empty or not: `riffnav diff --unstaged` on a clean tree
/// correctly shows nothing.
fn diff_input(args: cli::DiffArgs, config: &config::Config) -> Result<Input> {
    if let Some((old, new)) = args.renamed_flag() {
        anyhow::bail!("{old} was renamed to {new} in riffnav 1.1");
    }
    let (git, defaulted) = diff_command(args, config);
    let text = git.load()?;
    if defaulted
        && text.trim().is_empty()
        && let Some((source, found)) = autodiff::fallback_view(git.base.as_deref())
    {
        // Re-made rather than patched, so the header names the view on screen
        // and `r` re-runs the command that produced it.
        let git = GitDiff::diff(source, git.base, vec![]);
        return Ok(Input {
            text: found,
            git: Some(git),
        });
    }
    Ok(Input {
        text,
        git: Some(git),
    })
}

/// Read the unified diff piped or redirected in on stdin (the pager path).
fn read_stdin() -> Result<Input> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("failed to read diff from stdin")?;
    Ok(Input { text, git: None })
}

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let config = config::Config::load(cli.config.as_deref())?;

    let input = match cli.command {
        // `diff`/`show` are just another way to *source* the diff — they still
        // open the TUI, so they fall through to the app below.
        Some(cli::Command::Diff(args)) => diff_input(args, &config)?,
        Some(cli::Command::Show { args }) => {
            let git = GitDiff::show(cli::git_args(args));
            Input {
                text: git.load()?,
                git: Some(git),
            }
        }
        // The `comment`/`skill` subcommands are plain CLI tools: no TUI, no diff
        // on stdin, no delta. They talk to the same on-disk state a running
        // window does.
        Some(command) => return cmd::run(command),
        // No subcommand: either a diff is being piped in, or the user typed
        // `riffnav` with nothing to show it — in which case show them what they
        // can ask for.
        None if std::io::stdin().is_terminal() => {
            cli::Cli::command().print_help()?;
            println!();
            std::process::exit(2);
        }
        None => read_stdin()?,
    };

    // `--list` is a debug helper: print the parsed files for whatever source was
    // selected (piped diff, or the git command that was run) and exit.
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
    if let Some(git) = input.git {
        app.enable_git_diff(git);
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

#[cfg(test)]
mod tests {
    use super::*;
    use autodiff::View;

    /// The view `riffnav diff` resolves to for a command line, under a config
    /// that sets `diff_source`. `base_branch` is pinned so nothing shells out to
    /// detect one.
    fn view(line: &str, configured: DiffSource) -> View {
        let config = config::Config {
            diff_source: Some(configured),
            base_branch: Some("origin/main".to_string()),
            ..Default::default()
        };
        let args = match cli::Cli::parse_from(line.split_whitespace()).command {
            Some(cli::Command::Diff(args)) => args,
            _ => panic!("expected a diff command"),
        };
        diff_command(args, &config).0.view
    }

    /// Whether that command line left the view to riffnav — the condition for
    /// the empty-diff fallback.
    fn defaulted(line: &str, config: &config::Config) -> bool {
        let args = match cli::Cli::parse_from(line.split_whitespace()).command {
            Some(cli::Command::Diff(args)) => args,
            _ => panic!("expected a diff command"),
        };
        diff_command(args, config).1
    }

    #[test]
    fn a_configured_view_applies_to_a_bare_diff() {
        assert_eq!(
            view("riffnav diff", DiffSource::Staged),
            View::Diff(DiffSource::Staged)
        );
    }

    #[test]
    fn a_configured_view_never_stacks_onto_pass_through_args() {
        // `riffnav diff HEAD~1` has to mean `git diff HEAD~1`; a configured
        // `diff_source` would otherwise add `--staged` (or `HEAD`) in front of
        // the user's revision and render a diff they never asked for.
        for line in ["riffnav diff HEAD~1", "riffnav diff -w -- src/"] {
            assert_eq!(
                view(line, DiffSource::Staged),
                View::Diff(DiffSource::Unstaged),
                "{line} should fall back to git's own default"
            );
            assert_eq!(
                view(line, DiffSource::AllUncommitted),
                View::Diff(DiffSource::Unstaged),
                "{line} should fall back to git's own default"
            );
        }
    }

    /// The fallback to branch-vs-base is for the view riffnav picked on its own.
    /// Anything the user named — a flag, a configured `diff_source`, or their own
    /// arguments — is shown as asked, empty or not.
    #[test]
    fn only_riffnavs_own_default_view_may_fall_back_when_empty() {
        let bare = config::Config {
            base_branch: Some("origin/main".to_string()),
            ..Default::default()
        };
        assert!(defaulted("riffnav diff", &bare));
        assert!(!defaulted("riffnav diff --unstaged", &bare));
        assert!(!defaulted("riffnav diff --vs-base", &bare));
        assert!(!defaulted("riffnav diff HEAD~1", &bare));

        let configured = config::Config {
            diff_source: Some(DiffSource::Unstaged),
            ..bare.clone()
        };
        assert!(
            !defaulted("riffnav diff", &configured),
            "a configured view was still a choice"
        );
    }

    #[test]
    fn an_explicit_flag_still_wins_alongside_pass_through_args() {
        assert_eq!(
            view("riffnav diff --staged HEAD~1", DiffSource::AllUncommitted),
            View::Diff(DiffSource::Staged)
        );
    }
}
