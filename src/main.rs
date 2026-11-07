/// `println!` that returns the write's result, so a reader that stops reading,
/// as `riffnav --list | head` does, ends the program quietly instead of
/// panicking. See [`quiet_on_closed_pipe`].
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        writeln!(std::io::stdout(), $($arg)*)
    }};
}

mod app;
mod autodiff;
mod cli;
mod cmd;
mod comment;
mod config;
mod diff;
mod forge;
mod git;
mod herdr;
mod icons;
mod render;
mod review;
mod search;
mod session;
mod state;
#[cfg(test)]
mod testing;
mod theme;
mod tree;
mod ui;

use std::io::{ErrorKind, IsTerminal, Read};

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
/// unstaged < config < flag, and the base as detect < config < `--base`.
///
/// A configured `diff_source` is skipped once there are pass-through arguments,
/// or `riffnav diff HEAD~1` would become `git diff --staged HEAD~1`. A view flag
/// still wins, as it was typed alongside them.
///
/// The returned flag says the view was riffnav's own default, which allows the
/// empty-diff fallback in [`diff_input`]. The base is resolved for every view so
/// `d` and the number keys can reach the base-relative ones.
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

/// Load the diff for `riffnav diff`. A bare `riffnav diff` shows unstaged work,
/// which is empty on a clean tree even when the branch has commits, so an empty
/// default steps on to [`autodiff::fallback_view`]. A view the user named is
/// left alone, empty or not.
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

/// What to say about a diff with no files in it. git's view of a merge is what
/// the merge did beyond its parents, which for a clean one is nothing, and that's
/// no reason to think it changed nothing.
fn nothing_to_display(text: &str) -> String {
    if !text.lines().any(|line| line.starts_with("Merge: ")) {
        return "no changes to display".to_string();
    }
    let rev = text
        .lines()
        .find_map(|line| line.strip_prefix("commit "))
        .and_then(|line| line.split_whitespace().next())
        .map_or("<merge>", |id| &id[..id.len().min(10)]);
    format!(
        "no changes to display: a clean merge has no lines that differ from all its \
         parents.\nTo see what it brought in: riffnav show --first-parent {rev}"
    )
}

/// Read the unified diff piped or redirected in on stdin (the pager path).
fn read_stdin() -> Result<Input> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .context("failed to read diff from stdin")?;
    // Like a diff from git, as a name or a line needn't be UTF-8.
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(Input { text, git: None })
}

fn main() -> Result<()> {
    quiet_on_closed_pipe(run())
}

/// `result`, but a write to a pipe its reader has closed counts as done, as it
/// does for any command-line tool whose output was cut short on purpose.
fn quiet_on_closed_pipe(result: Result<()>) -> Result<()> {
    match result {
        Err(e)
            if e.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == ErrorKind::BrokenPipe)
            }) =>
        {
            Ok(())
        }
        result => result,
    }
}

fn run() -> Result<()> {
    let cli = cli::Cli::parse();
    let config = config::Config::load(cli.config.as_deref())?;

    if matches!(
        cli.command,
        None | Some(cli::Command::Diff(_) | cli::Command::Show { .. })
    ) {
        render::warm_up();
    }

    let input = match cli.command {
        // `diff`/`show` only source the diff; they still open the TUI below.
        Some(cli::Command::Diff(args)) => diff_input(args, &config)?,
        Some(cli::Command::Show { args }) => {
            let git = GitDiff::show(cli::git_args(args));
            Input {
                text: git.load()?,
                git: Some(git),
            }
        }
        // The other subcommands are plain CLI tools: no TUI, no diff on stdin.
        Some(command) => return cmd::run(command),
        // No subcommand and nothing piped in: show what can be asked for.
        None if std::io::stdin().is_terminal() => {
            cli::Cli::command().print_help()?;
            say!()?;
            std::process::exit(2);
        }
        None => read_stdin()?,
    };

    // `--list` is a debug helper: print the parsed files for whatever source was
    // selected (piped diff, or the git command that was run) and exit.
    if cli.list {
        return cli::print_list(&diff::parse(&input.text));
    }

    let files = diff::parse(&input.text);

    if files.is_empty() {
        eprintln!("riffnav: {}", nothing_to_display(&input.text));
        return Ok(());
    }

    // Layout precedence: CLI -s/-u win, then the config file.
    let side_by_side = if cli.side_by_side {
        true
    } else if cli.unified {
        false
    } else {
        config.side_by_side
    };

    let mut app = app::App::new(files, side_by_side, &config);
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
    let result = app.run();
    // Everything that matters is saved, and the process is about to give the memory
    // back in one go. Freeing a big render piece by piece would only hold up the exit.
    std::mem::forget(app);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use autodiff::View;

    /// A reader that stopped reading ends the program quietly; any other error,
    /// even one about writing, is still reported.
    #[test]
    fn only_a_closed_pipe_is_quiet() {
        let io = |kind| std::io::Error::from(kind);
        let closed = anyhow::Error::from(io(ErrorKind::BrokenPipe)).context("listing");
        assert!(quiet_on_closed_pipe(Err(closed)).is_ok());
        assert!(quiet_on_closed_pipe(Err(io(ErrorKind::PermissionDenied).into())).is_err());
        assert!(quiet_on_closed_pipe(Err(anyhow::anyhow!("no repo"))).is_err());
    }

    #[test]
    fn an_empty_merge_says_why_and_how_to_see_it() {
        let text = "commit 0123456789abcdef\nMerge: 1234 5678\nAuthor: a\n\n    Merge\n";
        let said = nothing_to_display(text);
        assert!(said.contains("clean merge"), "{said}");
        assert!(
            said.ends_with("riffnav show --first-parent 0123456789"),
            "{said}"
        );
    }

    #[test]
    fn anything_else_empty_has_nothing_to_explain() {
        for text in ["", "commit 0123\nAuthor: a\n\n    Merge: not really\n"] {
            assert_eq!(nothing_to_display(text), "no changes to display");
        }
    }

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
        // `riffnav diff HEAD~1` has to mean `git diff HEAD~1`.
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
