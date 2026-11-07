//! Shared plumbing for riffnav's on-disk state: where it lives, how it's scoped
//! to a repo and branch, and how stale scopes are swept.
//!
//! Two features persist review state — the "viewed" file marks ([`crate::review`])
//! and inline comments ([`crate::comment`]) — and both want the same shape: a
//! per-(repo, branch) JSON file under `$XDG_STATE_HOME/riffnav/<kind>/`, written
//! atomically, garbage-collected by age. This module owns that shape so the two
//! stores can't drift apart.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use twox_hash::XxHash3_128;

use crate::git;

pub const DAY_SECS: u64 = 86_400;

/// Distinguishes temp files written by one process, whose pid is shared across
/// every write it makes. See [`write_atomic`].
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// `$XDG_STATE_HOME/riffnav/<kind>`, falling back to
/// `$HOME/.local/state/riffnav/<kind>`. `None` when neither variable is set.
pub fn dir(kind: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state"))
        })?;
    Some(base.join("riffnav").join(kind))
}

/// Filesystem-safe path for a scope: `<dir>/<hash(repo)>/<hash(branch)>.json`.
/// Branch names contain slashes and other hostile characters, so both components
/// are hashed rather than used verbatim.
pub fn scope_path(dir: &Path, repo: &str, branch: &str) -> PathBuf {
    dir.join(hash_hex(repo))
        .join(format!("{}.json", hash_hex(branch)))
}

fn hash_hex(s: &str) -> String {
    format!("{:032x}", XxHash3_128::oneshot(s.as_bytes()))
}

/// The current repo's toplevel and branch, or `None` outside a repo. A detached
/// HEAD has no branch, so it shares one repo-level bucket.
pub fn detect_scope() -> Option<(String, String)> {
    let repo = git::text(&["rev-parse", "--show-toplevel"])?;
    let branch = git::text(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let branch = if branch.is_empty() || branch == "HEAD" {
        "(detached)".to_string()
    } else {
        branch
    };
    Some((repo, branch))
}

/// Delete scope files not modified within `retention`, and remove repo
/// directories left empty, except for `keep` (the scope being opened now).
/// Entirely best-effort: any IO error just leaves that entry in place.
pub fn sweep(dir: &Path, retention: u64, keep: &Path) {
    let Ok(repos) = std::fs::read_dir(dir) else {
        return;
    };
    for repo in repos.flatten() {
        let repo_path = repo.path();
        if !repo_path.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&repo_path) else {
            continue;
        };
        let mut remaining = 0;
        for file in files.flatten() {
            let fpath = file.path();
            if fpath == keep {
                remaining += 1;
                continue;
            }
            if file_age(&fpath).is_some_and(|age| age > retention) {
                let _ = std::fs::remove_file(&fpath);
            } else {
                remaining += 1;
            }
        }
        // The scope being opened now is left alone even before its file exists:
        // a window may be watching this directory for the first comment to land
        // in it, and pruning it would break that watch for the session.
        let keeping = keep.parent() == Some(repo_path.as_path());
        if remaining == 0 && !keeping {
            // Only succeeds if truly empty, so this can't clobber a live repo.
            let _ = std::fs::remove_dir(&repo_path);
        }
    }
}

/// Write `bytes` to `path` via a temp file + rename, creating parent directories
/// as needed. Best-effort: returns whether the file landed. The rename keeps a
/// concurrent reader from ever seeing a half-written file.
///
/// The temp name is unique per write because two writers of the same scope is
/// the normal case: an agent's `riffnav comment add` writes the file the open
/// window saves to. A shared temp path lets them interleave into a half-written
/// file, and a comment store that fails to parse is silently read as empty.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let tmp = temp_path(path);
    if std::fs::write(&tmp, bytes).is_err() {
        return false;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// A scratch path beside `path`, unique to this write: same directory (so the
/// rename onto `path` is atomic), pid and sequence number in the name (so no two
/// writers can pick the same one).
fn temp_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Seconds since `path` was last modified, or `None` if that can't be read.
fn file_age(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(now_unix().saturating_sub(secs))
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn scope_path_hashes_both_components() {
        // Branch names like `feat/x` must not create nested directories.
        let p = scope_path(Path::new("/state"), "/repo", "feat/x");
        assert_eq!(p.components().count(), 4); // / state <repo> <branch>.json
        assert!(p.to_str().unwrap().ends_with(".json"));
        assert_ne!(
            scope_path(Path::new("/s"), "/r", "a"),
            scope_path(Path::new("/s"), "/r", "b")
        );
    }

    #[test]
    fn write_atomic_creates_parents_and_leaves_no_temp() {
        let dir = TempDir::new("state");
        let path = dir.join("nested").join("scope.json");
        assert!(write_atomic(&path, b"{}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        // Only the finished file is left behind, whatever the temp was called.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers, ["scope.json"]);
    }

    /// The window watches the scope's directory before its first comment lands.
    #[test]
    fn sweep_keeps_the_directory_of_the_scope_being_opened() {
        let dir = TempDir::new("sweep");
        let keep = scope_path(&dir, "/repo", "branch");
        let other = scope_path(&dir, "/elsewhere", "branch");
        std::fs::create_dir_all(keep.parent().unwrap()).unwrap();
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();

        sweep(&dir, DAY_SECS, &keep);
        assert!(
            keep.parent().unwrap().is_dir(),
            "the scope being opened keeps its directory"
        );
        assert!(
            !other.parent().unwrap().exists(),
            "an unrelated empty directory is still swept"
        );
    }

    /// Two writers of the same scope must not share a temp path.
    #[test]
    fn write_atomic_uses_a_distinct_temp_per_write() {
        let path = Path::new("/state/repo/scope.json");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..4 {
            let tmp = temp_path(path);
            assert!(seen.insert(tmp.clone()), "temp path repeated: {tmp:?}");
            assert!(
                tmp.to_str()
                    .unwrap()
                    .contains(&std::process::id().to_string())
            );
            // It stays beside its target, so the rename is atomic.
            assert_eq!(tmp.parent(), path.parent());
        }
    }
}
