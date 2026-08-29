//! Producing a diff straight from git, for `riffnav diff` and `riffnav show`.
//!
//! `riffnav diff` renders one of several "views" of the branch / working tree,
//! modeled by [`DiffSource`] and selected by its flags — plus any extra
//! arguments handed straight to git, so `riffnav diff HEAD~3 -- src/` works like
//! the `git diff` it shadows. [`GitDiff`] bundles the view, the base branch and
//! those extra arguments into one re-runnable command.
//!
//! The base branch the branch-vs-base view compares against is detected by
//! [`detect_base`]: whichever of `origin/HEAD` and a local `main`/`master` forks
//! off the current branch later.
//!
//! `git diff` never reports untracked files, so the working-tree views fold them
//! in explicitly (see [`untracked_diff`]) — otherwise a brand-new file would be
//! invisible until staged. A diff piped in on stdin never reaches this module.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// Pin the `a/`…`b/` diff path prefixes the parser strips, overriding whatever
/// `diff.mnemonicPrefix` / `diff.noprefix` the user's git config sets. Without
/// this, a machine with `diff.mnemonicPrefix = true` emits `i/`/`w/`/`c/`
/// prefixes, leaving a stray prefix on each path so the `o` key opens nothing.
const PREFIX_ARGS: [&str; 2] = ["--src-prefix=a/", "--dst-prefix=b/"];

/// Which slice of the branch / working tree to render as a diff. The runtime
/// toggle (`d`) cycles through these in [`DiffSource::CYCLE`] order. The names in
/// the attributes are the spellings accepted by the `diff_source` config key;
/// on the command line each is a flag of its own (`riffnav diff --staged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum DiffSource {
    /// Staged + unstaged working-tree changes vs `HEAD`, plus untracked files
    /// (`git diff HEAD`, with untracked files synthesized in).
    #[serde(rename = "all", alias = "uncommitted")]
    #[value(name = "all", alias = "uncommitted")]
    AllUncommitted,
    /// What the current branch adds over its base, three-dot merge-base
    /// (`git diff <base>...HEAD`) — mirrors a pull-request diff.
    #[serde(alias = "base")]
    #[value(alias = "base")]
    Committed,
    /// Staged changes only (`git diff --staged`).
    Staged,
    /// Unstaged working-tree changes only (`git diff`).
    Unstaged,
}

impl DiffSource {
    /// Short human label for the header/status line.
    pub fn label(self) -> &'static str {
        match self {
            Self::AllUncommitted => "all uncommitted",
            Self::Committed => "branch vs base",
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
        }
    }

    /// The revision selector this view contributes to its `git diff` — the part
    /// that says *which* diff, before any pass-through arguments. `base` is only
    /// used by [`DiffSource::Committed`]; the others ignore it. Unstaged needs
    /// nothing at all, which is what makes a bare `riffnav diff` run the same
    /// command as `git diff` — the only difference being the untracked files
    /// folded in afterwards (see [`DiffSource::includes_untracked`]).
    fn rev_args(self, base: &str) -> Vec<String> {
        match self {
            Self::AllUncommitted => vec!["HEAD".to_string()],
            Self::Committed => vec![format!("{base}...HEAD")],
            Self::Staged => vec!["--staged".to_string()],
            Self::Unstaged => vec![],
        }
    }

    /// Whether this view should fold in untracked files. The working-tree views
    /// do; the staged and branch-vs-base views legitimately exclude them (an
    /// untracked file is neither staged nor part of the branch's history).
    fn includes_untracked(self) -> bool {
        matches!(self, Self::AllUncommitted | Self::Unstaged)
    }

    /// The order the runtime view-toggle (`d`) steps through.
    const CYCLE: [DiffSource; 4] = [
        Self::AllUncommitted,
        Self::Staged,
        Self::Unstaged,
        Self::Committed,
    ];

    /// The next source when cycling. `has_base` drops the branch-vs-base view
    /// when no base was detected (it can't be produced); the working-tree views
    /// are always available, so cycling always lands somewhere valid.
    pub fn next(self, has_base: bool) -> DiffSource {
        let here = Self::CYCLE.iter().position(|&s| s == self).unwrap_or(0);
        for step in 1..=Self::CYCLE.len() {
            let cand = Self::CYCLE[(here + step) % Self::CYCLE.len()];
            if has_base || cand != Self::Committed {
                return cand;
            }
        }
        self
    }
}

/// Which git command produced the diff on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// `riffnav diff`: one of the built-in working-tree / branch views.
    Diff(DiffSource),
    /// `riffnav show`: whatever `git show` renders (`HEAD` by default).
    Show,
}

/// The git command behind the diff on screen, kept whole so it can be run again
/// — for the `r` refresh key, and to re-read a single file after `$EDITOR`.
///
/// `extra` holds the arguments the user spelled out after `riffnav diff` /
/// `riffnav show`, passed to git verbatim. They make the command arbitrary, so
/// the features that only make sense for a plain built-in view — cycling with
/// `d`, folding in untracked files, re-reading one file — key off
/// [`GitDiff::plain_source`] rather than the view alone.
#[derive(Debug, Clone)]
pub struct GitDiff {
    pub view: View,
    /// Detected (or configured) base branch, used by the branch-vs-base view and
    /// to re-run it when cycling. `None` when no base could be found, in which
    /// case that view is skipped.
    pub base: Option<String>,
    /// Extra arguments handed straight to git, after riffnav's own.
    pub extra: Vec<String>,
}

impl GitDiff {
    /// `riffnav diff`, in one of the built-in views.
    pub fn diff(source: DiffSource, base: Option<String>, extra: Vec<String>) -> Self {
        Self {
            view: View::Diff(source),
            base,
            extra,
        }
    }

    /// `riffnav show`.
    pub fn show(extra: Vec<String>) -> Self {
        Self {
            view: View::Show,
            base: None,
            extra,
        }
    }

    /// The built-in view this is showing, but only when the user added no
    /// arguments of their own. Pass-through arguments may name a revision or a
    /// pathspec, and stacking riffnav's own selectors or another pathspec on top
    /// of those would produce something the user never asked for — so anything
    /// that would do so asks here first and backs off when the answer is `None`.
    pub fn plain_source(&self) -> Option<DiffSource> {
        match self.view {
            View::Diff(source) if self.extra.is_empty() => Some(source),
            _ => None,
        }
    }

    /// The git subcommand this view runs.
    fn sub(&self) -> &'static str {
        match self.view {
            View::Diff(_) => "diff",
            View::Show => "show",
        }
    }

    /// Header label: the built-in view's name, or the actual git command line
    /// when the user passed arguments of their own. Truncated, since the header
    /// is one line and a long pathspec list would crowd everything else off it.
    pub fn label(&self) -> String {
        if let Some(source) = self.plain_source() {
            return source.label().to_string();
        }
        let rev = match self.view {
            View::Diff(source) => source.rev_args(self.base.as_deref().unwrap_or("")),
            View::Show => vec![],
        };
        let mut label = format!("git {}", self.sub());
        for arg in rev.iter().chain(&self.extra) {
            label.push(' ');
            label.push_str(arg);
        }
        if label.chars().count() > 48 {
            label = label.chars().take(47).collect::<String>() + "…";
        }
        label
    }

    /// The full `git` argument list. `--no-pager` keeps a `pager.diff = riffnav`
    /// config from ever re-entering riffnav, and `color.ui=never` stops a
    /// `color.ui = always` config from handing the parser ANSI-laden text.
    fn argv(&self) -> Result<Vec<String>> {
        let rev = match self.view {
            View::Diff(DiffSource::Committed) => {
                let base = self
                    .base
                    .as_deref()
                    .context("no base branch detected to compare the branch against")?;
                DiffSource::Committed.rev_args(base)
            }
            // The other views never read `base`; pass an empty placeholder.
            View::Diff(source) => source.rev_args(""),
            View::Show => vec![],
        };
        let mut argv: Vec<String> = ["--no-pager", "-c", "color.ui=never", self.sub()]
            .into_iter()
            .chain(PREFIX_ARGS)
            .map(str::to_string)
            .collect();
        argv.extend(rev);
        argv.extend(self.extra.iter().cloned());
        Ok(argv)
    }

    /// Whether this command's output should have untracked files folded in.
    fn includes_untracked(&self) -> bool {
        self.plain_source()
            .is_some_and(DiffSource::includes_untracked)
    }

    /// Run the command, returning the raw unified-diff text. Errors carry git's
    /// own stderr.
    pub fn load(&self) -> Result<String> {
        let tracked = run_git(&self.argv()?);
        if self.includes_untracked() {
            // `git diff [HEAD]` fails on an unborn branch (no commits yet); treat
            // that as "no tracked changes" so untracked files still surface.
            Ok(tracked.unwrap_or_default() + &untracked_diff())
        } else {
            tracked
        }
    }

    /// Re-run the diff for a single `path`, returning its raw unified-diff text —
    /// or an empty string when the file no longer differs. Only valid for a plain
    /// view ([`GitDiff::plain_source`]); callers check first. Mirrors [`Self::load`]'s
    /// untracked handling: a path `git diff` omits because it is untracked is
    /// rendered against `/dev/null` instead, but only when the view folds
    /// untracked files in (so a tracked-but-now-unchanged file correctly reports
    /// no diff rather than showing up as fully added).
    pub fn load_file(&self, path: &str) -> Result<String> {
        if self.includes_untracked() && is_untracked(path) {
            return Ok(diff_against_devnull(path).unwrap_or_default());
        }
        let mut argv = self.argv()?;
        argv.push("--".to_string());
        argv.push(path.to_string());
        match run_git(&argv) {
            Ok(text) => Ok(text),
            // An unborn branch makes `git diff HEAD -- path` fail; for the
            // working-tree views treat that as "nothing tracked" (mirrors `load`).
            Err(_) if self.includes_untracked() => Ok(String::new()),
            Err(e) => Err(e),
        }
    }
}

/// Whether the current directory is inside a git work tree.
pub fn in_repo() -> bool {
    git(&["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}

/// Detect the base branch the current branch should be compared against. Two
/// candidates are considered — `origin/HEAD` (the remote's default branch) and a
/// local `main`/`master` — and the one whose merge-base with `HEAD` is *newer*
/// wins, so commits the branch merely inherited from an already-updated local
/// `main` don't show up as its own work. Ties keep the remote candidate, which
/// is what a pull request would compare against. Returns `None` when neither
/// resolves, in which case the branch-vs-base view is unavailable.
pub fn detect_base() -> Option<String> {
    let remote = git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]); // e.g. "origin/main"
    let local = local_base();
    match (remote, local) {
        (Some(remote), Some(local)) => Some(if merge_base_is_newer(&local, &remote) {
            local
        } else {
            remote
        }),
        (remote, local) => remote.or(local),
    }
}

/// The local `main`/`master` candidate, if one exists. A branch sitting on the
/// same commit as `HEAD` is skipped: its merge-base with `HEAD` is `HEAD` itself,
/// so it would render an empty diff — the common case being a local commit on
/// `main` that hasn't been pushed, where `origin/main` is the useful base.
fn local_base() -> Option<String> {
    let head = git(&["rev-parse", "HEAD"]);
    ["main", "master"]
        .into_iter()
        .find(|name| {
            git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ])
            .is_some_and(|tip| Some(&tip) != head.as_ref())
        })
        .map(str::to_string)
}

/// Whether `cand`'s merge-base with `HEAD` is strictly newer than `other`'s, i.e.
/// `other`'s merge-base is a proper ancestor of it. False when either merge-base
/// can't be computed (unrelated histories), leaving the caller on its default.
fn merge_base_is_newer(cand: &str, other: &str) -> bool {
    let Some(cand_mb) = git(&["merge-base", cand, "HEAD"]) else {
        return false;
    };
    let Some(other_mb) = git(&["merge-base", other, "HEAD"]) else {
        return false;
    };
    cand_mb != other_mb && git_ok(&["merge-base", "--is-ancestor", &other_mb, &cand_mb])
}

/// Run the plain diff for `source`, returning the raw unified-diff text. Used by
/// [`load_initial`]; the TUI goes through [`GitDiff`], which can also carry the
/// user's own arguments.
fn load(source: DiffSource, base: Option<&str>) -> Result<String> {
    GitDiff::diff(source, base.map(str::to_string), vec![]).load()
}

/// Whether `path` is an untracked, non-ignored file (so `git diff` omits it).
fn is_untracked(path: &str) -> bool {
    git_raw(&["ls-files", "--others", "--exclude-standard", "--", path])
        .is_some_and(|s| !s.trim().is_empty())
}

/// Pick a source adaptively and load it: prefer uncommitted work, and only fall
/// back to the branch-vs-base view when the tree is clean. Returns the chosen
/// source alongside its diff text so the caller can show which view it is.
///
/// This is the guess the `riffnav comment` subcommands make when no window has
/// published a session — there is no user-chosen view to go on, so it picks the
/// diff the user is most likely working on.
///
/// On an unborn branch (no commits yet) `git diff HEAD` fails; we treat that
/// probe as "no uncommitted changes" rather than erroring, so such a repo simply
/// reports nothing to show.
pub fn load_initial(base: Option<&str>) -> Result<(DiffSource, String)> {
    let uncommitted = load(DiffSource::AllUncommitted, base).unwrap_or_default();
    if !uncommitted.trim().is_empty() {
        return Ok((DiffSource::AllUncommitted, uncommitted));
    }
    if base.is_some() {
        let committed = load(DiffSource::Committed, base)?;
        return Ok((DiffSource::Committed, committed));
    }
    // Nothing uncommitted and no base to diff against — leave it empty; the
    // caller's "no changes to display" path takes over.
    Ok((DiffSource::AllUncommitted, uncommitted))
}

/// Run `git` with `args`, returning trimmed stdout or `None` on any failure or
/// empty output. Mirrors the helpers in `forge.rs` and `review.rs`.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Run `git` with `args` for its exit status alone. Unlike [`git`], a successful
/// command that prints nothing still counts as success — needed for predicates
/// like `merge-base --is-ancestor`, which answer purely through their status.
fn git_ok(args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Run `git` with `args`, returning full stdout, and surfacing git's stderr in
/// the error when it exits non-zero (unlike [`git`], the diff text is kept
/// verbatim — leading/trailing whitespace can be significant).
fn run_git(args: &[String]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        match stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
            Some(line) => bail!("git {}: {line}", args.join(" ")),
            None => bail!("`git {}` exited with {}", args.join(" "), out.status),
        }
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Synthetic "added file" diffs for every untracked, non-ignored file, so the
/// working-tree views show brand-new files that `git diff` omits by design.
/// `.gitignore`d files are excluded (`--exclude-standard`).
fn untracked_diff() -> String {
    let Some(list) = git_raw(&["ls-files", "--others", "--exclude-standard", "-z"]) else {
        return String::new();
    };
    // `-z` is NUL-separated, so paths with spaces/newlines stay intact.
    let mut out = String::new();
    for path in list.split('\0').filter(|p| !p.is_empty()) {
        if let Some(diff) = diff_against_devnull(path) {
            out.push_str(&diff);
        }
    }
    out
}

/// `git diff --no-index /dev/null <path>` renders an untracked file as fully
/// added. `--no-index` exits non-zero whenever the inputs differ (always the
/// case here), so success is judged by whether it produced output, not by the
/// exit status.
fn diff_against_devnull(path: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["diff", "--no-index"])
        .args(PREFIX_ARGS)
        .args(["--", "/dev/null", path])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    (!text.is_empty()).then_some(text)
}

/// Run `git` and return raw, untrimmed stdout on success (or `None` on failure).
/// Used where output framing matters — e.g. NUL-separated lists — unlike [`git`],
/// which trims.
fn git_raw(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `git` arguments a view runs with, for assertions below.
    fn argv(source: DiffSource, extra: &[&str]) -> Vec<String> {
        GitDiff::diff(
            source,
            Some("origin/main".to_string()),
            extra.iter().map(|s| s.to_string()).collect(),
        )
        .argv()
        .unwrap()
    }

    #[test]
    fn args_match_the_intended_git_commands() {
        // Every view pins `a/`…`b/` prefixes so the parser resolves paths
        // regardless of the user's `diff.mnemonicPrefix` config, and disables the
        // pager so `pager.diff = riffnav` can't recurse.
        let head = ["--no-pager", "-c", "color.ui=never", "diff"];
        let pre = ["--src-prefix=a/", "--dst-prefix=b/"];
        let expect = |tail: &[&str]| -> Vec<String> {
            head.iter()
                .chain(&pre)
                .chain(tail)
                .map(|s| s.to_string())
                .collect()
        };
        assert_eq!(argv(DiffSource::AllUncommitted, &[]), expect(&["HEAD"]));
        assert_eq!(argv(DiffSource::Staged, &[]), expect(&["--staged"]));
        assert_eq!(argv(DiffSource::Unstaged, &[]), expect(&[]));
    }

    #[test]
    fn committed_args_interpolate_the_base_as_three_dot() {
        assert!(
            argv(DiffSource::Committed, &[]).ends_with(&["origin/main...HEAD".to_string()]),
            "the branch-vs-base view diffs against the merge base"
        );
    }

    #[test]
    fn pass_through_args_come_last_so_git_sees_them_as_written() {
        assert!(
            argv(DiffSource::Staged, &["-w", "--", "src/"]).ends_with(&[
                "--staged".into(),
                "-w".into(),
                "--".into(),
                "src/".into()
            ]),
            "riffnav's own selectors precede the user's arguments"
        );
    }

    #[test]
    fn a_bare_diff_runs_the_same_command_as_git_diff() {
        // The whole point of `riffnav diff` shadowing `git diff`: with no flags
        // and no arguments, riffnav contributes no revision selector of its own.
        // (The output still gains the untracked files git leaves out — see
        // `only_working_tree_views_fold_in_untracked_files`.)
        assert!(DiffSource::Unstaged.rev_args("origin/main").is_empty());
    }

    #[test]
    fn committed_without_a_base_is_an_error() {
        assert!(load(DiffSource::Committed, None).is_err());
    }

    #[test]
    fn only_working_tree_views_fold_in_untracked_files() {
        assert!(DiffSource::AllUncommitted.includes_untracked());
        assert!(DiffSource::Unstaged.includes_untracked());
        assert!(!DiffSource::Staged.includes_untracked());
        assert!(!DiffSource::Committed.includes_untracked());
    }

    #[test]
    fn pass_through_args_disable_the_plain_view_features() {
        // With arguments of the user's own, the view can't be cycled, untracked
        // files aren't folded in, and one file can't be re-read on its own —
        // riffnav has no idea whether `HEAD~3` is a revision or a pathspec.
        let plain = GitDiff::diff(DiffSource::Unstaged, None, vec![]);
        assert_eq!(plain.plain_source(), Some(DiffSource::Unstaged));
        assert!(plain.includes_untracked());

        let scoped = GitDiff::diff(DiffSource::Unstaged, None, vec!["HEAD~3".to_string()]);
        assert_eq!(scoped.plain_source(), None);
        assert!(!scoped.includes_untracked());

        // `git show` has no built-in views at all.
        assert_eq!(GitDiff::show(vec![]).plain_source(), None);
    }

    #[test]
    fn labels_name_the_view_or_the_command() {
        assert_eq!(
            GitDiff::diff(DiffSource::Staged, None, vec![]).label(),
            "staged"
        );
        assert_eq!(
            GitDiff::diff(DiffSource::Unstaged, None, vec!["HEAD~3".to_string()]).label(),
            "git diff HEAD~3"
        );
        assert_eq!(
            GitDiff::show(vec!["abc123".to_string()]).label(),
            "git show abc123"
        );
        assert_eq!(GitDiff::show(vec![]).label(), "git show");
        // A long pathspec list is cut down to keep the header on one line.
        let long = GitDiff::show(vec!["x".repeat(80)]).label();
        assert_eq!(long.chars().count(), 48);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn cycle_with_a_base_visits_all_four_then_wraps() {
        use DiffSource::*;
        let mut cur = AllUncommitted;
        for &expected in &[Staged, Unstaged, Committed] {
            cur = cur.next(true);
            assert_eq!(cur, expected);
        }
        assert_eq!(cur.next(true), AllUncommitted); // wraps back to the start
    }

    #[test]
    fn cycle_without_a_base_never_lands_on_branch_vs_base() {
        let mut cur = DiffSource::AllUncommitted;
        for _ in 0..6 {
            cur = cur.next(false);
            assert_ne!(cur, DiffSource::Committed);
        }
    }

    #[test]
    fn every_source_has_a_distinct_label() {
        let labels = [
            DiffSource::AllUncommitted.label(),
            DiffSource::Committed.label(),
            DiffSource::Staged.label(),
            DiffSource::Unstaged.label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }
}
