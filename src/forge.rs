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
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

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
/// Marks are handed to a background thread so the UI never blocks on `gh`: each
/// `v` keypress enqueues a job and returns immediately, and the event loop drains
/// outcomes — surfacing only failures — via [`ReviewSync::drain`]. The PR's
/// GraphQL node id is resolved once on the worker and cached; a branch with no PR
/// is cached too, reported once and then quietly skipped for the session.
pub struct ReviewSync {
    /// Hands jobs to the worker. Dropping it (on app shutdown) ends the thread.
    jobs: Sender<Job>,
    /// One outcome per enqueued job: `Some(msg)` is an error to surface, `None`
    /// is a success or a deliberate skip (no PR for this branch).
    results: Receiver<Option<String>>,
    /// Jobs enqueued but not yet drained — i.e. syncs still in flight.
    pending: usize,
}

/// A queued mark/unmark request for the worker thread.
struct Job {
    path: String,
    viewed: bool,
}

/// Cached PR resolution for the current branch, owned by the worker thread.
enum PrState {
    /// Not looked up yet — the next job resolves it.
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
    /// Arm sync and spawn the worker thread. The thread idles on its channel
    /// until the first [`ReviewSync::enqueue`], so arming is cheap.
    pub fn new() -> Self {
        let (jobs_tx, jobs_rx) = channel::<Job>();
        let (results_tx, results_rx) = channel::<Option<String>>();
        thread::spawn(move || worker(jobs_rx, results_tx));
        Self {
            jobs: jobs_tx,
            results: results_rx,
            pending: 0,
        }
    }

    /// Queue a mark (`viewed`) / unmark of `path` on the branch's PR and return
    /// at once; the `gh` round trip runs on the worker. Drain the outcome later
    /// with [`ReviewSync::drain`]. The local viewed mark stands regardless.
    pub fn enqueue(&mut self, path: &str, viewed: bool) {
        let job = Job {
            path: path.to_string(),
            viewed,
        };
        // Send only fails once the worker has gone; then there's nothing in
        // flight, so don't count a job whose result will never arrive.
        if self.jobs.send(job).is_ok() {
            self.pending += 1;
        }
    }

    /// Whether any queued sync hasn't reported back yet — the event loop uses
    /// this to keep polling so results surface without needing a keypress.
    pub fn has_pending(&self) -> bool {
        self.pending > 0
    }

    /// Collect finished syncs without blocking, returning one message per failed
    /// job (successes and deliberate skips report nothing).
    pub fn drain(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        while let Ok(outcome) = self.results.try_recv() {
            self.pending = self.pending.saturating_sub(1);
            if let Some(msg) = outcome {
                errors.push(msg);
            }
        }
        errors
    }

    /// Best-effort wait for in-flight syncs to finish on shutdown, so a file
    /// marked moments before quitting still reaches the PR. Bounded by `grace`,
    /// so a slow or stuck `gh` can never hang quit.
    pub fn flush(&mut self, grace: Duration) {
        let deadline = Instant::now() + grace;
        while self.pending > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.results.recv_timeout(remaining) {
                Ok(_) => self.pending = self.pending.saturating_sub(1),
                // Timed out, or the worker is gone: stop waiting either way.
                Err(_) => break,
            }
        }
    }
}

/// Worker loop: own the PR resolution and run each queued mark/unmark to
/// completion, reporting a message back for any that fails (and `None` otherwise,
/// so the handle can track the in-flight count). Ends when the job sender drops.
fn worker(jobs: Receiver<Job>, results: Sender<Option<String>>) {
    let mut pr = PrState::Unresolved;
    while let Ok(job) = jobs.recv() {
        let outcome = run_job(&mut pr, &job);
        // A send error means the app (and its receiver) is gone; stop.
        if results.send(outcome).is_err() {
            break;
        }
    }
}

/// Run one job against the cached PR state: resolve the PR lazily, cache a
/// missing PR so we don't re-probe, and report a failure only the first time
/// (then stay quiet). Returns an error message to surface, or `None`.
fn run_job(pr: &mut PrState, job: &Job) -> Option<String> {
    let id = match pr {
        PrState::Ready(id) => id.clone(),
        // Already determined there's nothing to sync to; stay quiet.
        PrState::Unavailable => return None,
        PrState::Unresolved => match resolve_pr() {
            Ok(id) => {
                *pr = PrState::Ready(id.clone());
                id
            }
            Err(e) => {
                // Cache the failure so we don't re-probe on every job, and
                // report it just this once.
                *pr = PrState::Unavailable;
                return Some(format!("{e:#}"));
            }
        },
    };
    match set_file_viewed(&id, &job.path, job.viewed) {
        Ok(()) => None,
        Err(e) => Some(format!("{e:#}")),
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
    use super::{Job, PrState, parse_host, run_job, viewed_mutation};

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
    fn unavailable_pr_skips_without_touching_the_network() {
        // Once the branch is known to have no PR, a job neither shells out to
        // `gh` nor reports anything — a silent no-op (mirrors "report once").
        let mut pr = PrState::Unavailable;
        let job = Job {
            path: "src/main.rs".to_string(),
            viewed: true,
        };
        assert!(run_job(&mut pr, &job).is_none());
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
