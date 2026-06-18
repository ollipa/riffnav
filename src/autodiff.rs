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
//! Piped-stdin and `--watch` launches never reach this module; bare launch is
//! the only new entry path.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// Which slice of the branch / working tree to render as a diff.
///
/// The adaptive startup default ([`load_initial`]) only ever picks
/// `AllUncommitted` or `Committed`; `Staged`/`Unstaged` become reachable when
/// Phase 2 wires the runtime view-toggle.
#[allow(dead_code)] // Staged/Unstaged: see above — toggled in Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSource {
    /// Staged + unstaged working-tree changes vs `HEAD` (`git diff HEAD`).
    AllUncommitted,
    /// What the current branch adds over its base, three-dot merge-base
    /// (`git diff <base>...HEAD`) — mirrors a pull-request diff.
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
    /// by [`DiffSource::Committed`]; the others ignore it.
    fn args(self, base: &str) -> Vec<String> {
        let owned = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect();
        match self {
            Self::AllUncommitted => owned(&["diff", "HEAD"]),
            Self::Committed => vec!["diff".to_string(), format!("{base}...HEAD")],
            Self::Staged => owned(&["diff", "--staged"]),
            Self::Unstaged => owned(&["diff"]),
        }
    }
}

/// Live auto-diff state carried by the app on a bare launch: which view is
/// shown and the base branch (if any) the branch-vs-base view compares against.
pub struct AutoDiff {
    pub source: DiffSource,
    /// Detected base branch, read in Phase 2 to re-run the branch-vs-base view
    /// when toggling sources. Unused in Phase 1 (the header only labels it).
    #[allow(dead_code)]
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
        .find(|name| git(&["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")]).is_some())
        .map(str::to_string)
}

/// Run the diff for `source`, returning the raw unified-diff text. Errors carry
/// git's own stderr. The branch-vs-base source needs a `base`; without one it is
/// an error to ask for it.
pub fn load(source: DiffSource, base: Option<&str>) -> Result<String> {
    let base = match source {
        DiffSource::Committed => {
            base.context("no base branch detected to compare the branch against")?
        }
        // The other sources never read `base`; pass an empty placeholder.
        _ => "",
    };
    run_git(&source.args(base))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_match_the_intended_git_commands() {
        assert_eq!(DiffSource::AllUncommitted.args("base"), ["diff", "HEAD"]);
        assert_eq!(DiffSource::Staged.args("base"), ["diff", "--staged"]);
        assert_eq!(DiffSource::Unstaged.args("base"), ["diff"]);
    }

    #[test]
    fn committed_args_interpolate_the_base_as_three_dot() {
        assert_eq!(
            DiffSource::Committed.args("origin/main"),
            ["diff", "origin/main...HEAD"]
        );
    }

    #[test]
    fn committed_without_a_base_is_an_error() {
        assert!(load(DiffSource::Committed, None).is_err());
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
