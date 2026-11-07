//! Running git, and reading back what it said.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Result, bail};

/// Trimmed stdout, or `None` if git failed or printed nothing.
pub fn text(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Whether git succeeded, for commands that answer by exit status alone, like
/// `merge-base --is-ancestor`.
pub fn ok(args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Untrimmed stdout on success, for output where every byte counts, such as
/// NUL-separated names.
pub fn raw<'a>(args: impl IntoIterator<Item = &'a OsStr>) -> Option<Vec<u8>> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// A git to run in `repo` over pipes, its stderr dropped.
pub fn piped(repo: &Path) -> Command {
    let mut git = Command::new("git");
    git.current_dir(repo)
        // A partial clone would otherwise go to the network for a missing blob.
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    git
}

/// A finished command's stdout, or an error: its first stderr line after
/// `prefix`, or the exit status if it said nothing.
pub fn stdout_or_error(out: Output, shown: &str, prefix: &str) -> Result<String> {
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    match stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(line) => bail!("{prefix}{line}"),
        None => bail!("`{shown}` exited with {}", out.status),
    }
}
