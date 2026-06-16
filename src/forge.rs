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
    use super::parse_host;

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
