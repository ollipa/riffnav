//! Producing a diff straight from git, for `riffnav diff` and `riffnav show`.
//!
//! `riffnav diff` renders one of several "views" of the branch / working tree,
//! modeled by [`DiffSource`] and selected by its flags — plus any extra
//! arguments handed straight to git, so `riffnav diff HEAD~3 -- src/` works like
//! the `git diff` it shadows. [`GitDiff`] bundles the view, the base branch and
//! those extra arguments into one re-runnable command.
//!
//! The base branch the base-relative views compare against is detected by
//! [`detect_base`]: whichever of `origin/HEAD` and a local `main`/`master` forks
//! off the current branch later.
//!
//! `git diff` never reports untracked files, so the working-tree views fold them
//! in explicitly (see [`untracked_diff`]) — otherwise a brand-new file would be
//! invisible until staged.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::diff::path;
use crate::git;

/// Pin the `a/`…`b/` path prefixes the parser strips, whatever `diff.mnemonicPrefix` /
/// `diff.noprefix` say. Otherwise paths keep a stray `i/`/`w/` prefix and `o` opens nothing.
const PREFIX_ARGS: [&str; 2] = ["--src-prefix=a/", "--dst-prefix=b/"];

/// Name every file from the top of the repository, whatever `diff.relative` says,
/// as riffnav's pathspecs, blob reads and editor all do. Arguments of the user's
/// own come after it, so a `--relative` among them still wins.
const NO_RELATIVE: &str = "--no-relative";

/// Ask for git's own diff, never a `diff.external` tool's, which isn't a unified diff
/// the parser can read.
const NO_EXT_DIFF: &str = "--no-ext-diff";

/// Have git quote every name that isn't ASCII, whatever `core.quotePath` says, as a
/// name that isn't UTF-8 only reaches us intact in its quoted form.
const QUOTE_ARGS: [&str; 2] = ["-c", "core.quotePath=on"];

/// Which slice of the branch / working tree to render as a diff. `d` cycles through these in
/// [`DiffSource::CYCLE`] order and the number keys pick one by position. The serde names are
/// the `diff_source` config spellings; on the command line each is a flag of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum DiffSource {
    /// Everything the branch has done since it forked from its base, committed
    /// or not: its commits, plus staged, unstaged and untracked work
    /// (`git diff <fork point>`, with untracked files synthesized in).
    All,
    /// Staged + unstaged working-tree changes vs `HEAD`, plus untracked files
    /// (`git diff HEAD`, with untracked files synthesized in).
    AllUncommitted,
    /// Staged changes only (`git diff --staged`).
    Staged,
    /// Unstaged working-tree changes only (`git diff`).
    Unstaged,
    /// What the current branch adds over its base, three-dot merge-base
    /// (`git diff <base>...HEAD`) — mirrors a pull-request diff.
    VsBase,
}

impl DiffSource {
    /// Short label for the header. The base-relative views name what they're
    /// measured against, as the file list can't tell you that.
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all vs base",
            Self::AllUncommitted => "all uncommitted",
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::VsBase => "branch vs base",
        }
    }

    /// The revision selector this view adds to `git diff`. Unstaged adds nothing, so a
    /// bare `riffnav diff` runs the same command as `git diff`. [`DiffSource::All`] wants
    /// the fork point: [`GitDiff::argv`] passes a resolved merge-base, [`GitDiff::label`]
    /// the readable branch name.
    fn rev_args(self, base: &str) -> Vec<String> {
        match self {
            Self::All => vec![base.to_string()],
            Self::AllUncommitted => vec!["HEAD".to_string()],
            Self::Staged => vec!["--staged".to_string()],
            Self::Unstaged => vec![],
            Self::VsBase => vec![format!("{base}...HEAD")],
        }
    }

    /// Whether this view is measured against the base branch, and so can't be
    /// produced when none was detected.
    fn needs_base(self) -> bool {
        matches!(self, Self::All | Self::VsBase)
    }

    /// Whether this view folds in untracked files. Staged and branch-vs-base leave
    /// them out: an untracked file is neither staged nor committed.
    fn includes_untracked(self) -> bool {
        matches!(self, Self::All | Self::AllUncommitted | Self::Unstaged)
    }

    /// The order `d` steps through, widest slice first. The number keys index this
    /// same order.
    pub const CYCLE: [DiffSource; 5] = [
        Self::All,
        Self::AllUncommitted,
        Self::Staged,
        Self::Unstaged,
        Self::VsBase,
    ];

    /// The view at 1-based position `n` in the cycle — what the number keys
    /// select. `None` past the end, so an unbound digit stays inert.
    pub fn nth(n: usize) -> Option<DiffSource> {
        n.checked_sub(1).and_then(|i| Self::CYCLE.get(i)).copied()
    }

    /// Whether this view can be produced right now: the base-relative views need a
    /// detected base.
    pub fn available(self, has_base: bool) -> bool {
        has_base || !self.needs_base()
    }

    /// The next available source when cycling. The working-tree views are always
    /// available, so this always lands somewhere valid.
    pub fn next(self, has_base: bool) -> DiffSource {
        let here = Self::CYCLE.iter().position(|&s| s == self).unwrap_or(0);
        for step in 1..=Self::CYCLE.len() {
            let cand = Self::CYCLE[(here + step) % Self::CYCLE.len()];
            if cand.available(has_base) {
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
    /// Detected (or configured) base branch for the base-relative views. `None`
    /// skips them.
    pub base: Option<String>,
    /// Extra arguments handed straight to git, after riffnav's own.
    extra: Vec<String>,
    /// Lines of context around each change (`-U<n>`), as widened with `+`/`-`.
    /// `None` leaves it to git — its `diff.context` setting, or 3 — which is
    /// also the floor `-` stops at (see [`default_context`]).
    pub context: Option<u32>,
}

/// Where to read a file the diff doesn't touch, which is the same on both sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tree {
    /// The index, for a view that reaches past `HEAD`.
    Index,
    Rev(String),
}

impl Tree {
    /// How `git cat-file` names `path` in this tree. The index's stage is spelled
    /// out, as a file named `1:x` would otherwise read as stage 1 of `x`.
    pub fn spec(&self, path: &str) -> String {
        match self {
            Self::Index => format!(":0:{path}"),
            Self::Rev(rev) => format!("{rev}:{path}"),
        }
    }
}

impl GitDiff {
    /// `riffnav diff`, in one of the built-in views.
    pub fn diff(source: DiffSource, base: Option<String>, extra: Vec<String>) -> Self {
        Self {
            view: View::Diff(source),
            base,
            extra,
            context: None,
        }
    }

    /// `riffnav show`.
    pub fn show(extra: Vec<String>) -> Self {
        Self {
            view: View::Show,
            base: None,
            extra,
            context: None,
        }
    }

    /// The built-in view, but only when the user added no arguments of their own.
    /// Those may name a revision or a pathspec, and stacking riffnav's own on top
    /// would produce a diff nobody asked for.
    pub fn plain_source(&self) -> Option<DiffSource> {
        match self.view {
            View::Diff(source) if self.extra.is_empty() => Some(source),
            _ => None,
        }
    }

    /// What holds the files this diff leaves alone, if that can be told. Arguments of
    /// the user's own can name any revisions at all, so a diff with them doesn't say,
    /// and `show` only does when it's of one thing.
    pub fn tree(&self) -> Option<Tree> {
        match self.view {
            View::Show => {
                let mut revs = self.extra.iter().take_while(|arg| *arg != "--");
                let revs: Vec<&String> = revs.by_ref().filter(|a| !a.starts_with('-')).collect();
                match revs[..] {
                    [] => Some(Tree::Rev("HEAD".to_string())),
                    [rev] => Some(Tree::Rev(rev.clone())),
                    _ => None,
                }
            }
            View::Diff(DiffSource::VsBase) if self.extra.is_empty() => {
                Some(Tree::Rev("HEAD".to_string()))
            }
            View::Diff(_) if self.extra.is_empty() => Some(Tree::Index),
            View::Diff(_) => None,
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
            View::Diff(source) if source.needs_base() => {
                let base = self
                    .base
                    .as_deref()
                    .context("no base branch detected to compare the branch against")?;
                source.rev_args(&resolve_base(source, base)?)
            }
            // The other views never read `base`; pass an empty placeholder.
            View::Diff(source) => source.rev_args(""),
            View::Show => vec![],
        };
        let mut argv: Vec<String> = ["--no-pager", "-c", "color.ui=never"]
            .into_iter()
            .chain(QUOTE_ARGS)
            .chain([self.sub(), NO_EXT_DIFF])
            .chain(PREFIX_ARGS)
            .chain([NO_RELATIVE])
            .map(str::to_string)
            .collect();
        argv.extend(self.context.map(|n| format!("-U{n}")));
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
        let here = Path::new(".");
        let tracked = run_git(&self.argv()?).map(|text| describe_unmerged(here, text));
        if self.includes_untracked() {
            // `git diff [HEAD]` fails on an unborn branch (no commits yet); treat
            // that as "no tracked changes" so untracked files still surface.
            Ok(tracked.unwrap_or_default() + &untracked_diff(here))
        } else {
            tracked
        }
    }

    /// Re-run the diff for one `path`, or an empty string when it no longer differs.
    /// Only for a plain view ([`GitDiff::plain_source`]). Like [`Self::load`], an
    /// untracked path is diffed against `/dev/null`, but only in a view that folds
    /// them in, so a tracked file that is now unchanged reports no diff.
    pub fn load_file(&self, path: &str) -> Result<String> {
        let here = Path::new(".");
        if self.includes_untracked() && is_untracked(here, path) {
            let diff = git::toplevel(here).and_then(|top| diff_against_devnull(&top, path));
            return Ok(diff.unwrap_or_default());
        }
        let mut argv = self.argv()?;
        argv.push("--".to_string());
        argv.push(literal_pathspec(path));
        match run_git(&argv).map(|text| describe_unmerged(here, text)) {
            Ok(text) => Ok(text),
            // An unborn branch, as in `load`.
            Err(_) if self.includes_untracked() => Ok(String::new()),
            Err(e) => Err(e),
        }
    }
}

/// The context git gives a diff when not told otherwise: the `diff.context`
/// setting, or git's built-in 3.
pub fn default_context() -> u32 {
    git::text(&["config", "--get", "diff.context"])
        .and_then(|n| n.parse().ok())
        .unwrap_or(3)
}

/// Whether the current directory is inside a git work tree.
pub fn in_repo() -> bool {
    git::text(&["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}

/// Detect the base branch the current branch should be compared against. Two
/// candidates are considered — `origin/HEAD` (the remote's default branch) and a
/// local `main`/`master` — and the one whose merge-base with `HEAD` is *newer*
/// wins, so commits the branch merely inherited from an already-updated local
/// `main` don't show up as its own work. Ties keep the remote candidate, which
/// is what a pull request would compare against. `None` when neither resolves.
pub fn detect_base() -> Option<String> {
    let remote = git::text(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]); // e.g. "origin/main"
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
    let head = git::text(&["rev-parse", "HEAD"]);
    ["main", "master"]
        .into_iter()
        .find(|name| {
            git::text(&[
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
    let Some(cand_mb) = git::text(&["merge-base", cand, "HEAD"]) else {
        return false;
    };
    let Some(other_mb) = git::text(&["merge-base", other, "HEAD"]) else {
        return false;
    };
    cand_mb != other_mb && git::ok(&["merge-base", "--is-ancestor", &other_mb, &cand_mb])
}

/// The revision a base-relative view diffs *from*. [`DiffSource::VsBase`] keeps
/// the branch name: its three-dot range makes git find the fork point itself.
/// [`DiffSource::All`] can't, because `git diff` has no three-dot form that
/// reaches the working tree — so the fork point is resolved here and passed as a
/// plain revision. Diffing against the base branch's tip instead would report
/// every commit the base has gained since the fork as a reverse change of the
/// branch's own, which is exactly the noise the fork point exists to cut.
fn resolve_base(source: DiffSource, base: &str) -> Result<String> {
    if source != DiffSource::All {
        return Ok(base.to_string());
    }
    git::text(&["merge-base", base, "HEAD"])
        .with_context(|| format!("no common ancestor between {base} and HEAD"))
}

/// Run the plain diff for `source`. The TUI goes through [`GitDiff`], which can also
/// carry the user's own arguments.
fn load(source: DiffSource, base: Option<&str>) -> Result<String> {
    GitDiff::diff(source, base.map(str::to_string), vec![]).load()
}

/// The pathspec for exactly the file `path`, so that a name like `*.txt` or `:x`
/// means itself. `path` is from the top of the repository, as git's diff names
/// are, wherever riffnav was started.
fn literal_pathspec(path: &str) -> String {
    format!(":(top,literal){path}")
}

/// Whether `path` is an untracked, non-ignored file (so `git diff` omits it), asking
/// git in `dir`.
fn is_untracked(dir: &Path, path: &str) -> bool {
    let pathspec = path::to_os(&literal_pathspec(path));
    let flags = ["ls-files", "--others", "--exclude-standard", "--"].map(OsStr::new);
    git::raw(dir, flags.into_iter().chain([pathspec.as_os_str()]))
        .is_some_and(|list| !list.trim_ascii().is_empty())
}

/// The view to show when the one asked for renders nothing: the narrowest one that
/// still shows everything, uncommitted and committed work if both exist, else
/// whichever does. `None` when neither has anything.
///
/// A half git can't produce counts as empty: `git diff HEAD` fails on an unborn
/// branch, and the three-dot diff fails on unrelated histories.
pub fn fallback_view(base: Option<&str>) -> Option<(DiffSource, String)> {
    let uncommitted = load(DiffSource::AllUncommitted, base).unwrap_or_default();
    let has_uncommitted = !uncommitted.trim().is_empty();
    let Some(base) = base else {
        return has_uncommitted.then_some((DiffSource::AllUncommitted, uncommitted));
    };
    let committed = load(DiffSource::VsBase, Some(base)).unwrap_or_default();
    match (has_uncommitted, !committed.trim().is_empty()) {
        // Only the union view shows both, and it doesn't flip as the reader
        // commits. If its merge-base fails, fall back to the uncommitted half.
        (true, true) => Some(match load(DiffSource::All, Some(base)) {
            Ok(all) if !all.trim().is_empty() => (DiffSource::All, all),
            _ => (DiffSource::AllUncommitted, uncommitted),
        }),
        // One side only: name the narrower view. `branch vs base` mirrors a PR,
        // which is what pushes viewed marks to GitHub.
        (true, false) => Some((DiffSource::AllUncommitted, uncommitted)),
        (false, true) => Some((DiffSource::VsBase, committed)),
        (false, false) => None,
    }
}

/// Pick a source adaptively and load it, for a caller that needs *some* view
/// even when every one of them is empty.
pub fn load_initial(base: Option<&str>) -> (DiffSource, String) {
    // Nothing anywhere: hand back the empty uncommitted view and let the
    // caller's "no changes to display" path take over.
    fallback_view(base).unwrap_or((DiffSource::AllUncommitted, String::new()))
}

/// Run `git` with `args`, returning its stdout verbatim (unlike [`git::text`],
/// as leading/trailing whitespace can be significant), or its stderr as the error.
fn run_git(args: &[String]) -> Result<String> {
    let shown = format!("git {}", args.join(" "));
    let out = Command::new("git")
        .args(args.iter().map(|arg| path::to_os(arg)))
        .output()
        .with_context(|| format!("running `{shown}`"))?;
    git::stdout_or_error(out, &shown, &format!("{shown}: "))
}

/// Synthetic "added file" diffs for every untracked, non-ignored file, so the
/// working-tree views show brand-new files that `git diff` omits by design.
///
/// Run from the top of the repository: `ls-files` started in a subdirectory lists
/// only that subdirectory, and names its files from there rather than from the
/// top, as `git diff` does. `dir` is anywhere in the work tree.
fn untracked_diff(dir: &Path) -> String {
    let Some(top) = git::toplevel(dir) else {
        return String::new();
    };
    let flags = ["ls-files", "--others", "--exclude-standard", "-z"];
    let Some(list) = git::raw(&top, flags) else {
        return String::new();
    };
    // `-z` is NUL-separated and unquoted, so paths with spaces/newlines stay intact.
    let mut out = String::new();
    for name in list.split(|byte| *byte == 0).filter(|p| !p.is_empty()) {
        if let Some(diff) = diff_against_devnull(&top, &path::from_bytes(name)) {
            out.push_str(&diff);
        }
    }
    out
}

/// `git diff --no-index /dev/null <path>` renders an untracked file as fully
/// added, `path` named from the top of the repository at `top`. `--no-index`
/// exits non-zero whenever the inputs differ (always the case here), so success
/// is judged by whether it produced output, not by the exit status.
///
/// git reads a file named `-` as its stdin, even after `--`, so that one is
/// passed as `./-` and the name put back in the headers. Stdin is null anyway, so
/// nothing could ever wait on it.
fn diff_against_devnull(top: &Path, path: &str) -> Option<String> {
    let stdin_name = path == "-";
    let arg = path::to_os(if stdin_name { "./-" } else { path });
    let out = Command::new("git")
        .current_dir(top)
        .stdin(Stdio::null())
        .args(devnull_args(&arg))
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.is_empty() {
        return None;
    }
    Some(if stdin_name {
        unwrap_dot_dash(&text)
    } else {
        text
    })
}

/// `./-`'s diff, with the name in its headers back to `-`. The other side is
/// `/dev/null`, so `/./-` can only be the file's name.
fn unwrap_dot_dash(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_hunk = false;
    for line in text.split_inclusive('\n') {
        in_hunk |= line.starts_with("@@");
        match in_hunk {
            true => out.push_str(line),
            false => out.push_str(&line.replace("/./-", "/-")),
        }
    }
    out
}

/// The diff text with a `* Conflict: <how>` line after each `* Unmerged path <name>`
/// git wrote, saying how the merge left it, in `git status`'s words. git writes
/// that line for a path it can't diff, such as one side deleted and the other
/// modified, and the line alone doesn't say which. `dir` is anywhere in the work
/// tree.
fn describe_unmerged(dir: &Path, text: String) -> String {
    const UNMERGED: &str = "* Unmerged path ";
    if !text.starts_with(UNMERGED) && !text.contains(&format!("\n{UNMERGED}")) {
        return text;
    }
    let stages = unmerged_stages(dir);
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(line);
        let Some(name) = line.strip_prefix(UNMERGED) else {
            continue;
        };
        let name = name.strip_suffix('\n').unwrap_or(name);
        if let Some(how) = stages.get(name).and_then(|&s| conflict(s)) {
            if !line.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("* Conflict: {how}\n"));
        }
    }
    out
}

/// Which index stages each unmerged path has, as bits 1 (base), 2 (ours) and 3
/// (theirs), by its name from the top of the repository, asking git in `dir`.
fn unmerged_stages(dir: &Path) -> HashMap<String, u8> {
    let flags = ["ls-files", "--unmerged", "--full-name", "-z", "--", ":/"];
    let mut stages = HashMap::new();
    for entry in git::raw(dir, flags)
        .unwrap_or_default()
        .split(|byte| *byte == 0)
    {
        // `<mode> <id> <stage>\t<name>`
        let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let Some(stage @ b'1'..=b'3') = entry[..tab].last().copied() else {
            continue;
        };
        *stages
            .entry(path::from_bytes(&entry[tab + 1..]))
            .or_insert(0) |= 1 << (stage - b'0');
    }
    stages
}

/// `git status`'s words for an unmerged path with these stages.
fn conflict(stages: u8) -> Option<&'static str> {
    const BASE: u8 = 1 << 1;
    const OURS: u8 = 1 << 2;
    const THEIRS: u8 = 1 << 3;
    Some(match stages {
        s if s == BASE | OURS | THEIRS => "both modified",
        s if s == OURS | THEIRS => "both added",
        s if s == BASE | OURS => "deleted by them",
        s if s == BASE | THEIRS => "deleted by us",
        BASE => "both deleted",
        OURS => "added by us",
        THEIRS => "added by them",
        _ => return None,
    })
}

/// The arguments for [`diff_against_devnull`]. `color.ui=never` for the same
/// reason as [`GitDiff`]'s.
fn devnull_args(path: &OsStr) -> Vec<&OsStr> {
    let mut args = ["-c", "color.ui=never"].to_vec();
    args.extend(QUOTE_ARGS);
    args.extend(["diff", "--no-index", NO_EXT_DIFF]);
    args.extend(PREFIX_ARGS);
    args.extend(["--", "/dev/null"]);
    let mut args: Vec<&OsStr> = args.into_iter().map(OsStr::new).collect();
    args.push(path);
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::FileStatus;

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
        // Every view pins the path prefixes and disables the pager.
        let head = [
            "--no-pager",
            "-c",
            "color.ui=never",
            "-c",
            "core.quotePath=on",
            "diff",
            "--no-ext-diff",
        ];
        let pre = ["--src-prefix=a/", "--dst-prefix=b/", "--no-relative"];
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
    fn widened_context_precedes_the_revision_and_the_users_args() {
        let mut git = GitDiff::diff(DiffSource::AllUncommitted, None, vec!["-w".to_string()]);
        git.context = Some(8);
        assert!(git.argv().unwrap().ends_with(&[
            "-U8".to_string(),
            "HEAD".to_string(),
            "-w".to_string()
        ]));
    }

    #[test]
    fn vs_base_args_interpolate_the_base_as_three_dot() {
        assert!(
            argv(DiffSource::VsBase, &[]).ends_with(&["origin/main...HEAD".to_string()]),
            "the branch-vs-base view diffs against the merge base"
        );
    }

    #[test]
    fn the_union_view_diffs_from_a_resolved_fork_point() {
        // Three dots would leave the working tree out, so the resolved fork point
        // is passed as the one revision, with no `...HEAD` bolted on.
        assert_eq!(DiffSource::All.rev_args("abc123"), ["abc123"]);
        assert_eq!(resolve_base(DiffSource::VsBase, "main").unwrap(), "main");
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
        // With no flags and no arguments, riffnav adds no revision selector.
        assert!(DiffSource::Unstaged.rev_args("origin/main").is_empty());
    }

    #[test]
    fn base_relative_views_without_a_base_are_an_error() {
        assert!(load(DiffSource::VsBase, None).is_err());
        assert!(load(DiffSource::All, None).is_err());
    }

    #[test]
    fn only_working_tree_views_fold_in_untracked_files() {
        assert!(DiffSource::All.includes_untracked());
        assert!(DiffSource::AllUncommitted.includes_untracked());
        assert!(DiffSource::Unstaged.includes_untracked());
        assert!(!DiffSource::Staged.includes_untracked());
        assert!(!DiffSource::VsBase.includes_untracked());
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
    fn the_tree_behind_a_diff_is_only_named_when_its_known() {
        let rev = |rev: &str| Some(Tree::Rev(rev.to_string()));
        let show = |args: &[&str]| GitDiff::show(args.iter().map(|a| a.to_string()).collect());
        let diff = |source, args: &[&str]| {
            GitDiff::diff(source, None, args.iter().map(|a| a.to_string()).collect())
        };
        assert_eq!(show(&[]).tree(), rev("HEAD"));
        assert_eq!(show(&["abc123"]).tree(), rev("abc123"));
        assert_eq!(show(&["-w", "abc123", "--", "src"]).tree(), rev("abc123"));
        assert_eq!(show(&["a", "b"]).tree(), None);
        assert_eq!(diff(DiffSource::Unstaged, &[]).tree(), Some(Tree::Index));
        assert_eq!(diff(DiffSource::Staged, &[]).tree(), Some(Tree::Index));
        assert_eq!(diff(DiffSource::VsBase, &[]).tree(), rev("HEAD"));
        assert_eq!(diff(DiffSource::Unstaged, &["HEAD~3"]).tree(), None);
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
    fn cycle_with_a_base_visits_all_five_then_wraps() {
        use DiffSource::*;
        let mut cur = All;
        for &expected in &[AllUncommitted, Staged, Unstaged, VsBase] {
            cur = cur.next(true);
            assert_eq!(cur, expected);
        }
        assert_eq!(cur.next(true), All); // wraps back to the start
    }

    #[test]
    fn cycle_without_a_base_never_lands_on_a_base_relative_view() {
        let mut cur = DiffSource::AllUncommitted;
        for _ in 0..8 {
            cur = cur.next(false);
            assert!(cur.available(false), "{cur:?} needs a base branch");
        }
    }

    #[test]
    fn number_keys_index_the_cycle_from_one() {
        // `1` is where `d` starts, and `0` selects nothing.
        assert_eq!(DiffSource::nth(1), Some(DiffSource::All));
        assert_eq!(DiffSource::nth(5), Some(DiffSource::VsBase));
        assert_eq!(DiffSource::nth(0), None);
        assert_eq!(DiffSource::nth(6), None);
    }

    #[test]
    fn every_source_has_a_distinct_label() {
        let labels: Vec<_> = DiffSource::CYCLE.iter().map(|s| s.label()).collect();
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }

    /// A `color.ui = always` config would hand the parser an untracked file's
    /// diff with ANSI codes in it.
    #[test]
    fn an_untracked_file_comes_back_without_color() {
        let dir = crate::testing::TempDir::new("devnull");
        let file = dir.join("new.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let out = Command::new("git")
            .args(["-c", "color.ui=always"])
            .args(devnull_args(file.as_os_str()))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("+one"), "{text}");
        assert!(!text.contains('\x1b'), "{text:?}");
    }

    /// A `diff.external` tool's output isn't a unified diff.
    #[test]
    fn an_untracked_file_is_diffed_by_git_itself() {
        let dir = crate::testing::TempDir::new("ext-diff");
        let file = dir.join("new.txt");
        std::fs::write(&file, "one\n").unwrap();
        let out = Command::new("git")
            .args(["-c", "diff.external=echo external"])
            .args(devnull_args(file.as_os_str()))
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("+one"), "{text}");
        assert!(!text.contains("external"), "{text}");
    }

    /// The untracked files of a repository, named from its top, from a nested
    /// subdirectory of it. A file named `-` is read, not taken for stdin.
    #[test]
    fn untracked_files_are_found_and_named_from_the_top_anywhere_in_the_tree() {
        let repo = crate::testing::Repo::new();
        repo.commit("tracked.txt", "x\n");
        repo.write("top.txt", "top\n");
        repo.write("a/b/low.txt", "low\n");
        repo.write("-", "dash\n");
        let deep = repo.0.join("a/b");
        let files = crate::diff::parse(&untracked_diff(&deep));
        let mut got: Vec<(&str, u32)> = files.iter().map(|f| (f.path(), f.additions)).collect();
        got.sort();
        assert_eq!(got, [("-", 1), ("a/b/low.txt", 1), ("top.txt", 1)]);
        let dash = files.iter().find(|f| f.path() == "-").unwrap();
        assert!(dash.raw.starts_with("diff --git a/- b/-\n"), "{}", dash.raw);
        assert!(
            dash.raw.contains("+++ b/-\n@@ -0,0 +1 @@\n+dash\n"),
            "{}",
            dash.raw
        );

        assert!(is_untracked(&deep, "top.txt"));
        assert!(is_untracked(&deep, "a/b/low.txt"));
        assert!(!is_untracked(&deep, "low.txt"));
        assert!(!is_untracked(&deep, "tracked.txt"));
    }

    /// One file's diff, as `load_file` asks for it, from a subdirectory.
    #[test]
    fn one_file_is_diffed_by_its_name_from_the_top() {
        let repo = crate::testing::Repo::new();
        repo.commit("top.txt", "x\n");
        repo.write("top.txt", "y\n");
        std::fs::create_dir_all(repo.0.join("sub")).unwrap();
        let out = Command::new("git")
            .current_dir(repo.0.join("sub"))
            .args(["diff", "--"])
            .arg(literal_pathspec("top.txt"))
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("+y"),
            "{out:?}"
        );
    }

    /// Each way a merge can leave a path that `git diff` can only call unmerged
    /// is named, as `git status` names it.
    #[test]
    fn unmerged_paths_say_how_the_merge_left_them() {
        let repo = crate::testing::Repo::new();
        let files = ["model", "dus", "ddel", "rn"];
        for name in files {
            repo.write(&format!("{name}.txt"), "1\n2\n3\n");
        }
        repo.commit("both.txt", "1\n");
        repo.git(&["checkout", "-q", "-b", "other"]);
        repo.git(&["rm", "-q", "model.txt", "ddel.txt"]);
        repo.git(&["mv", "rn.txt", "rn_theirs.txt"]);
        repo.write("dus.txt", "1\n2\n3\nthem\n");
        repo.commit("aa.txt", "theirs\n");
        repo.git(&["checkout", "-q", "-"]);
        repo.git(&["rm", "-q", "dus.txt", "ddel.txt"]);
        repo.git(&["mv", "rn.txt", "rn_ours.txt"]);
        repo.write("model.txt", "1\n2\n3\nours\n");
        repo.commit("aa.txt", "ours\n");
        assert!(!repo.run(&["merge", "other"]).status.success());

        std::fs::create_dir_all(repo.0.join("sub")).unwrap();
        let text = String::from_utf8(repo.run(&["diff", "--staged"]).stdout).unwrap();
        let described = describe_unmerged(&repo.0.join("sub"), text);
        let files = crate::diff::parse(&described);
        let got: Vec<_> = files
            .iter()
            .map(|f| (f.path(), f.status, crate::diff::notes(f)))
            .collect();
        let want = [
            ("aa.txt", "both added"),
            ("ddel.txt", "both deleted"),
            ("dus.txt", "deleted by us"),
            ("model.txt", "deleted by them"),
            ("rn_ours.txt", "added by us"),
            ("rn_theirs.txt", "added by them"),
        ];
        assert_eq!(got.len(), want.len(), "{described}");
        for ((path, status, notes), (name, how)) in got.iter().zip(want) {
            assert_eq!((*path, *status), (name, FileStatus::Unmerged));
            assert_eq!(notes, &[how.to_string()], "{name}");
        }
    }

    /// A diff with no unmerged paths comes back untouched, without asking git.
    #[test]
    fn a_diff_without_unmerged_paths_is_left_as_it_is() {
        let text = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-* Unmerged path f\n+x\n";
        let missing = Path::new("/nonexistent/riffnav");
        assert_eq!(describe_unmerged(missing, text.to_string()), text);
    }

    /// Under `diff.relative`, a plain view started in a subdirectory still names
    /// and shows every file from the top; a `--relative` of the user's own wins.
    #[test]
    fn a_relative_diff_config_does_not_change_the_names() {
        let repo = crate::testing::Repo::new();
        repo.write("top.txt", "x\n");
        repo.commit("sub/low.txt", "x\n");
        repo.git(&["config", "diff.relative", "true"]);
        repo.write("top.txt", "y\n");
        repo.write("sub/low.txt", "y\n");
        let names = |extra: &[&str]| {
            let git = GitDiff::diff(
                DiffSource::Unstaged,
                None,
                extra.iter().map(|a| a.to_string()).collect(),
            );
            let out = Command::new("git")
                .current_dir(repo.0.join("sub"))
                .args(git.argv().unwrap())
                .output()
                .unwrap();
            let text = String::from_utf8(out.stdout).unwrap();
            let files = crate::diff::parse(&text);
            files
                .iter()
                .map(|f| f.path().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&[]), ["sub/low.txt", "top.txt"]);
        assert_eq!(names(&["--relative"]), ["low.txt"]);
    }
}
