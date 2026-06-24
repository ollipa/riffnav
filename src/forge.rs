//! Optional integration with the source-code hosting provider ("forge") backing
//! the current repository — currently GitHub via the `gh` CLI.
//!
//! When riffnav detects a supported forge, the `W` ("web") key opens the current
//! branch's pull-request diff in the browser, delegating entirely to the forge's
//! own CLI (`gh pr diff --web`) so we never build URLs or launch a browser
//! ourselves. Detection mirrors the herdr integration: [`Forge::detect`] returns
//! `None` when no supported forge is available, so the key is simply inert.
//!
//! Adding another forge (GitLab, Bitbucket, …) means adding a variant plus its
//! detection and open command; the `W` key and the UI wiring stay the same.

use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// A detected source-code hosting provider for the current repository.
pub enum Forge {
    /// GitHub, reachable through the `gh` CLI.
    GitHub,
}

impl Forge {
    /// Detect the forge backing the current repo, or `None` when none is
    /// supported or available. Cheap, local-only checks (no network) run once at
    /// startup, matching how herdr detection works.
    pub fn detect() -> Option<Self> {
        if gh_available() && remote_host().as_deref() == Some("github.com") {
            return Some(Self::GitHub);
        }
        None
    }

    /// Human-readable name, used in status messages.
    pub fn name(&self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
        }
    }

    /// Open the current branch's PR diff in the browser via the forge's CLI.
    /// Returns an error carrying the CLI's own message on failure (e.g. no PR
    /// exists for the branch), which the caller surfaces in the status line.
    pub fn open_web_diff(&self) -> Result<()> {
        match self {
            Self::GitHub => run_quietly("gh", &["pr", "diff", "--web"]),
        }
    }
}

/// One-way sync of riffnav's "viewed" marks to the GitHub pull request for the
/// current branch, ticking GitHub's per-file "Viewed" checkbox to match. Only
/// meaningful for the branch-vs-base ("Committed") view — the caller enforces
/// that — since the other views don't correspond to anything on the PR.
///
/// The PR's GraphQL node id is resolved once and cached so each mark is a single
/// `gh api graphql` round trip. A branch with no PR is cached too, reported once
/// and then quietly skipped for the rest of the session.
pub struct ReviewSync {
    pr: PrState,
}

/// Cached PR resolution for the current branch.
enum PrState {
    /// Not looked up yet — the next mark resolves it.
    Unresolved,
    /// The PR's GraphQL node id, ready to mark against.
    Ready(String),
    /// No PR for this branch (or `gh` failed) — already reported once, now mute.
    Unavailable,
}

/// Shape of the one field we read from `gh pr view --json id`.
#[derive(Deserialize)]
struct PrView {
    id: String,
}

impl ReviewSync {
    pub fn new() -> Self {
        Self {
            pr: PrState::Unresolved,
        }
    }

    /// Mark (`viewed`) or unmark `path` as viewed on the current branch's PR,
    /// resolving and caching the PR on first use. Returns the `gh` error so the
    /// caller can surface it; a missing PR is reported only on the first mark and
    /// is a silent no-op thereafter. The local viewed mark stands regardless.
    pub fn mark(&mut self, path: &str, viewed: bool) -> Result<()> {
        let id = match &self.pr {
            PrState::Ready(id) => id.clone(),
            // Already determined there's nothing to sync to; stay quiet.
            PrState::Unavailable => return Ok(()),
            PrState::Unresolved => match resolve_pr() {
                Ok(id) => {
                    self.pr = PrState::Ready(id.clone());
                    id
                }
                Err(e) => {
                    // Cache the failure so we don't re-probe on every keypress,
                    // and report it this once.
                    self.pr = PrState::Unavailable;
                    return Err(e);
                }
            },
        };
        set_file_viewed(&id, path, viewed)
    }
}

/// The PR node id for the current branch, via `gh pr view --json id`. Errors
/// carry `gh`'s own message (e.g. "no pull requests found for branch …").
fn resolve_pr() -> Result<String> {
    let json = run_capture("gh", &["pr", "view", "--json", "id"])?;
    let view: PrView =
        serde_json::from_str(&json).context("parsing `gh pr view --json id` output")?;
    Ok(view.id)
}

/// Run the mark/unmark GraphQL mutation for one file against `pr_id`.
fn set_file_viewed(pr_id: &str, path: &str, viewed: bool) -> Result<()> {
    run_quietly(
        "gh",
        &[
            "api",
            "graphql",
            "-f",
            &format!("query={}", viewed_mutation(viewed)),
            "-f",
            &format!("pr={pr_id}"),
            "-f",
            &format!("path={path}"),
        ],
    )
}

/// The GraphQL mutation marking (or unmarking) a file as viewed. Variables `$pr`
/// and `$path` are supplied as `gh api graphql -f pr=… -f path=…`.
fn viewed_mutation(viewed: bool) -> &'static str {
    if viewed {
        "mutation($pr:ID!,$path:String!){ markFileAsViewed(input:{pullRequestId:$pr,path:$path}){ clientMutationId } }"
    } else {
        "mutation($pr:ID!,$path:String!){ unmarkFileAsViewed(input:{pullRequestId:$pr,path:$path}){ clientMutationId } }"
    }
}

/// Whether the `gh` CLI is callable on PATH.
fn gh_available() -> bool {
    Command::new("gh")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The host of the current branch's remote, lowercased, or `None` when it can't
/// be resolved (not a repo, no remote, detached with no `origin`, …).
fn remote_host() -> Option<String> {
    let remote = current_branch_remote().unwrap_or_else(|| "origin".to_string());
    let url = git_output(&["remote", "get-url", &remote])
        .or_else(|| git_output(&["remote", "get-url", "origin"]))?;
    parse_host(&url)
}

/// The remote tracked by the current branch (e.g. `origin`), or `None` when the
/// branch has no configured remote or HEAD is detached.
fn current_branch_remote() -> Option<String> {
    let branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        return None; // detached HEAD — no branch-specific remote
    }
    git_output(&["config", &format!("branch.{branch}.remote")])
}

/// Run a git command and return its trimmed stdout, or `None` on any failure or
/// empty output.
fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Run `program` to completion and map a non-zero exit into an error carrying its
/// first stderr line (falling back to the exit status).
fn run_quietly(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run `{program}`"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    match stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(line) => bail!("{line}"),
        None => bail!("`{program}` exited with {}", out.status),
    }
}

/// Like [`run_quietly`], but returns the command's stdout on success. Used for
/// `gh` calls whose output we parse (e.g. `gh pr view --json …`).
fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run `{program}`"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    match stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(line) => bail!("{line}"),
        None => bail!("`{program}` exited with {}", out.status),
    }
}

/// Extract the host from a git remote URL, handling both scp-like SSH
/// (`git@github.com:owner/repo.git`) and URL forms
/// (`https://github.com/owner/repo.git`, `ssh://git@github.com:22/owner/repo`).
/// Returns the lowercased host, or `None` if none can be found.
fn parse_host(url: &str) -> Option<String> {
    // Drop the scheme for URL forms; scp-like remotes have none.
    let rest = match url.trim().split_once("://") {
        Some((_scheme, rest)) => rest,
        None => url.trim(),
    };
    // The authority is everything before the path. For scp-like `host:path` the
    // ':' separates host from path, so split on '/' first to keep that ':' with
    // the authority, then strip any `user@` and trailing `:port`/`:path`.
    let authority = rest.split('/').next().unwrap_or(rest);
    let host_port = authority.rsplit_once('@').map_or(authority, |(_user, h)| h);
    let host = host_port.split(':').next().unwrap_or(host_port);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{parse_host, viewed_mutation};

    #[test]
    fn viewed_mutation_picks_the_right_graphql_field() {
        assert!(viewed_mutation(true).contains("markFileAsViewed"));
        assert!(!viewed_mutation(true).contains("unmarkFileAsViewed"));
        assert!(viewed_mutation(false).contains("unmarkFileAsViewed"));
        // Both reference the `$pr`/`$path` variables `gh -f pr=…/-f path=…` fill.
        for q in [viewed_mutation(true), viewed_mutation(false)] {
            assert!(q.contains("$pr") && q.contains("$path"));
        }
    }

    #[test]
    fn scp_like_ssh() {
        assert_eq!(
            parse_host("git@github.com:ollipa/riffnav.git").unwrap(),
            "github.com"
        );
    }

    #[test]
    fn https_with_dot_git() {
        assert_eq!(
            parse_host("https://github.com/ollipa/riffnav.git").unwrap(),
            "github.com"
        );
    }

    #[test]
    fn https_with_credentials() {
        assert_eq!(
            parse_host("https://user@github.com/o/r").unwrap(),
            "github.com"
        );
    }

    #[test]
    fn ssh_url_with_port() {
        assert_eq!(
            parse_host("ssh://git@github.com:22/o/r.git").unwrap(),
            "github.com"
        );
    }

    #[test]
    fn scp_like_without_user() {
        assert_eq!(parse_host("github.com:o/r.git").unwrap(), "github.com");
    }

    #[test]
    fn host_is_lowercased() {
        assert_eq!(parse_host("git@GitHub.com:o/r.git").unwrap(), "github.com");
    }

    #[test]
    fn non_github_host() {
        assert_eq!(parse_host("git@gitlab.com:o/r.git").unwrap(), "gitlab.com");
    }
}
