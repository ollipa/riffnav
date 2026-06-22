//! Auto-diff mode: when riffnav is launched bare (no piped diff, not watch mode),
//! produce a diff straight from the current git repository instead of reading
//! stdin.
//!
//! The diff is one of several "views" of the branch / working tree, modeled by
//! [`DiffSource`]. At startup the source is chosen adaptively by
//! [`load_initial`]: show uncommitted work if there is any, otherwise fall back
//! to what the branch adds over its base (the "PR view"). The base branch is
//! detected from `origin/HEAD`, falling back to a local `main`/`master`.
//!
//! `git diff` never reports untracked files, so the working-tree views fold them
//! in explicitly (see [`untracked_diff`]) — otherwise a brand-new file would be
//! invisible until staged. Piped-stdin and `--watch` launches never reach this
//! module; bare launch is the only new entry path.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// Pin the `a/`…`b/` diff path prefixes the parser strips, overriding whatever
/// `diff.mnemonicPrefix` / `diff.noprefix` the user's git config sets. Without
/// this, a machine with `diff.mnemonicPrefix = true` emits `i/`/`w/`/`c/`
/// prefixes, leaving a stray prefix on each path so the `o` key opens nothing.
const PREFIX_ARGS: [&str; 2] = ["--src-prefix=a/", "--dst-prefix=b/"];

/// Which slice of the branch / working tree to render as a diff. The runtime
/// toggle (`d`) cycles through these in [`DiffSource::CYCLE`] order. The names in
/// the attributes are the spellings accepted by `--diff` and the `diff_source`
/// config key.
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

    /// The `git` arguments that produce this source's diff. `base` is only used
    /// by [`DiffSource::Committed`]; the others ignore it. Every invocation forces
    /// the conventional `a/`…`b/` path prefixes via [`PREFIX_ARGS`].
    fn args(self, base: &str) -> Vec<String> {
        let mut args: Vec<String> = ["diff"]
            .into_iter()
            .chain(PREFIX_ARGS)
            .map(str::to_string)
            .collect();
        match self {
            Self::AllUncommitted => args.push("HEAD".to_string()),
            Self::Committed => args.push(format!("{base}...HEAD")),
            Self::Staged => args.push("--staged".to_string()),
            Self::Unstaged => {}
        }
        args
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

/// Live auto-diff state carried by the app on a bare launch: which view is
/// shown and the base branch (if any) the branch-vs-base view compares against.
pub struct AutoDiff {
    pub source: DiffSource,
    /// Detected base branch, used to re-run the branch-vs-base view when toggling
    /// sources. `None` when no base could be found (that view is then skipped).
    pub base: Option<String>,
}

/// Whether the current directory is inside a git work tree.
pub fn in_repo() -> bool {
    git(&["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}

/// Detect the base branch the current branch should be compared against:
/// `origin/HEAD` (the remote's default branch) first, then a local `main` or
/// `master`. Returns `None` when none of these resolve, in which case the
/// branch-vs-base view is unavailable.
pub fn detect_base() -> Option<String> {
    if let Some(head) = git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        return Some(head); // e.g. "origin/main"
    }
    ["main", "master"]
        .into_iter()
        .find(|name| {
            git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ])
            .is_some()
        })
        .map(str::to_string)
}

/// Run the diff for `source`, returning the raw unified-diff text. Errors carry
/// git's own stderr. The branch-vs-base source needs a `base`; without one it is
/// an error to ask for it.
pub fn load(source: DiffSource, base: Option<&str>) -> Result<String> {
    let tracked = match source {
        DiffSource::Committed => {
            let base = base.context("no base branch detected to compare the branch against")?;
            run_git(&source.args(base))
        }
        // The other sources never read `base`; pass an empty placeholder.
        _ => run_git(&source.args("")),
    };
    if source.includes_untracked() {
        // `git diff [HEAD]` fails on an unborn branch (no commits yet); treat that
        // as "no tracked changes" so untracked files still surface.
        Ok(tracked.unwrap_or_default() + &untracked_diff())
    } else {
        tracked
    }
}

/// Re-run the active source's diff for a single `path`, returning its raw
/// unified-diff text — or an empty string when the file no longer differs.
/// Mirrors [`load`]'s untracked handling: a path `git diff` omits because it is
/// untracked is rendered against `/dev/null` instead, but only when the source
/// folds untracked files in (so a tracked-but-now-unchanged file correctly
/// reports no diff rather than showing up as fully added).
pub fn load_file(source: DiffSource, base: Option<&str>, path: &str) -> Result<String> {
    if source.includes_untracked() && is_untracked(path) {
        return Ok(diff_against_devnull(path).unwrap_or_default());
    }
    let mut args = match source {
        DiffSource::Committed => {
            let base = base.context("no base branch detected to compare the branch against")?;
            source.args(base)
        }
        _ => source.args(""),
    };
    args.push("--".to_string());
    args.push(path.to_string());
    match run_git(&args) {
        Ok(text) => Ok(text),
        // An unborn branch makes `git diff HEAD -- path` fail; for the
        // working-tree views treat that as "nothing tracked" (mirrors `load`).
        Err(_) if source.includes_untracked() => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// Whether `path` is an untracked, non-ignored file (so `git diff` omits it).
fn is_untracked(path: &str) -> bool {
    git_raw(&["ls-files", "--others", "--exclude-standard", "--", path])
        .is_some_and(|s| !s.trim().is_empty())
}

/// Pick the startup source adaptively and load it: prefer uncommitted work, and
/// only fall back to the branch-vs-base view when the tree is clean. Returns the
/// chosen source alongside its diff text so the caller can show which view it is.
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

    #[test]
    fn args_match_the_intended_git_commands() {
        // Every view pins `a/`…`b/` prefixes so the parser resolves paths
        // regardless of the user's `diff.mnemonicPrefix` config.
        let pre = ["--src-prefix=a/", "--dst-prefix=b/"];
        assert_eq!(
            DiffSource::AllUncommitted.args("base"),
            ["diff", pre[0], pre[1], "HEAD"]
        );
        assert_eq!(
            DiffSource::Staged.args("base"),
            ["diff", pre[0], pre[1], "--staged"]
        );
        assert_eq!(DiffSource::Unstaged.args("base"), ["diff", pre[0], pre[1]]);
    }

    #[test]
    fn committed_args_interpolate_the_base_as_three_dot() {
        assert_eq!(
            DiffSource::Committed.args("origin/main"),
            [
                "diff",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "origin/main...HEAD"
            ]
        );
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
