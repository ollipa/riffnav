//! Persistent "viewed" review state: which file-diffs the user has marked as
//! reviewed, in the spirit of GitHub's per-file "Viewed" checkbox.
//!
//! State is keyed on the *content* of each change (a hash of the file's diff),
//! not its path, so a file automatically reverts to unviewed when its diff
//! changes — exactly like GitHub un-ticking a file the author has pushed to.
//!
//! Scope is per repository **and** per branch, mirroring GitHub's per-PR model:
//! the same change reviewed on another branch is reviewed independently. State
//! lives under `$XDG_STATE_HOME/riffnav/viewed/<repo>/<branch>.json` and is
//! garbage-collected by age — abandoned branch files are swept on startup, and
//! within an active file stale entries are pruned on save.
//!
//! When riffnav isn't run inside a git repo (e.g. an arbitrary diff piped in),
//! there's no stable scope to anchor to, so the store degrades to session-only:
//! toggling still works, nothing persists.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use twox_hash::XxHash3_128;

const DAY_SECS: u64 = 86_400;

/// One reviewed file: enough to prune by age and to make the on-disk file
/// human-readable when debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    path: String,
    /// Unix seconds when this file was last marked viewed.
    seen: u64,
}

/// On-disk shape of a single `<branch>.json`. `files` maps a hex diff-hash to
/// its entry; `repo`/`branch` are stored only so the hash-named file is
/// self-describing.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    repo: String,
    branch: String,
    files: HashMap<String, Entry>,
}

/// Tracks which file-diffs are marked viewed for the current (repo, branch)
/// scope, persisting changes to disk. A `path` of `None` means session-only.
pub struct ReviewStore {
    /// Where this scope persists, or `None` for a session-only store.
    path: Option<PathBuf>,
    repo: String,
    branch: String,
    /// Diff hashes currently marked viewed, with their metadata.
    files: HashMap<u128, Entry>,
    /// Whether `files` has unsaved changes.
    dirty: bool,
}

impl ReviewStore {
    /// A store that never persists. Used before review is enabled (and in tests),
    /// and as the fallback whenever no repo scope can be determined.
    pub fn disabled() -> Self {
        Self {
            path: None,
            repo: String::new(),
            branch: String::new(),
            files: HashMap::new(),
            dirty: false,
        }
    }

    /// Load the viewed state for the current repo+branch, sweeping stale branch
    /// files first. Falls back to a session-only store when not in a repo or the
    /// state directory can't be located.
    pub fn load(retention_days: u64) -> Self {
        let retention = retention_days.saturating_mul(DAY_SECS);
        let (Some((repo, branch)), Some(dir)) = (detect_scope(), state_dir()) else {
            return Self::disabled();
        };
        let path = scope_path(&dir, &repo, &branch);

        // Reap branch files we haven't touched in `retention`, but never the one
        // we're about to use — opening a branch counts as keeping it alive.
        sweep(&dir, retention, &path);

        let mut files = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(stored) = serde_json::from_str::<StoreFile>(&text)
        {
            let now = now_unix();
            for (hex, entry) in stored.files {
                // Drop entries older than retention; skip anything unparseable.
                if let Ok(hash) = u128::from_str_radix(&hex, 16)
                    && now.saturating_sub(entry.seen) <= retention
                {
                    files.insert(hash, entry);
                }
            }
        }

        Self {
            path: Some(path),
            repo,
            branch,
            files,
            dirty: false,
        }
    }

    pub fn is_viewed(&self, hash: u128) -> bool {
        self.files.contains_key(&hash)
    }

    /// Count how many of `hashes` are currently marked viewed.
    pub fn count_viewed(&self, hashes: &[u128]) -> usize {
        hashes.iter().filter(|h| self.is_viewed(**h)).count()
    }

    /// Flip the viewed state of `hash`, returning the new state (`true` = now
    /// viewed). `path` is recorded for readability/GC when newly marked.
    pub fn toggle(&mut self, hash: u128, path: &str) -> bool {
        self.dirty = true;
        if self.files.remove(&hash).is_some() {
            false
        } else {
            self.files.insert(
                hash,
                Entry {
                    path: path.to_string(),
                    seen: now_unix(),
                },
            );
            true
        }
    }

    /// Persist pending changes via a temp-file + rename, best-effort. Writing
    /// refreshes the file's mtime so the active branch survives the next sweep;
    /// an emptied scope deletes its file rather than leaving a husk behind.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(path) = self.path.clone() else {
            return;
        };

        if self.files.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let Some(parent) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }

        let stored = StoreFile {
            repo: self.repo.clone(),
            branch: self.branch.clone(),
            files: self
                .files
                .iter()
                .map(|(h, e)| (format!("{h:032x}"), e.clone()))
                .collect(),
        };
        let Ok(json) = serde_json::to_vec_pretty(&stored) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// The stable identity of a file's change: an XXH3-128 hash of its diff with the
/// volatile `index <sha>..<sha>` header lines removed, so blob-SHA churn alone
/// never re-flags a file. 128 bits makes a collision (a wrongly shared viewed
/// mark) a non-issue.
pub fn file_hash(raw: &str) -> u128 {
    let mut buf = Vec::with_capacity(raw.len());
    for line in raw.lines() {
        if line.starts_with("index ") {
            continue;
        }
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
    }
    XxHash3_128::oneshot(&buf)
}

/// `$XDG_STATE_HOME/riffnav/viewed`, falling back to
/// `$HOME/.local/state/riffnav/viewed`.
fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state"))
        })?;
    Some(base.join("riffnav").join("viewed"))
}

/// Filesystem-safe path for a scope: `<dir>/<hash(repo)>/<hash(branch)>.json`.
/// Branch names contain slashes and other hostile characters, so both
/// components are hashed rather than used verbatim.
fn scope_path(dir: &Path, repo: &str, branch: &str) -> PathBuf {
    dir.join(hash_hex(repo))
        .join(format!("{}.json", hash_hex(branch)))
}

fn hash_hex(s: &str) -> String {
    format!("{:032x}", XxHash3_128::oneshot(s.as_bytes()))
}

/// The current repo's toplevel and branch, or `None` outside a repo. A detached
/// HEAD has no branch, so it shares one repo-level bucket.
fn detect_scope() -> Option<(String, String)> {
    let repo = git(&["rev-parse", "--show-toplevel"])?;
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let branch = if branch.is_empty() || branch == "HEAD" {
        "(detached)".to_string()
    } else {
        branch
    };
    Some((repo, branch))
}

/// Delete branch files not modified within `retention`, and remove repo
/// directories left empty, except for `keep` (the scope being opened now).
/// Entirely best-effort: any IO error just leaves that entry in place.
fn sweep(dir: &Path, retention: u64, keep: &Path) {
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
        if remaining == 0 {
            // Only succeeds if truly empty, so this can't clobber a live repo.
            let _ = std::fs::remove_dir(&repo_path);
        }
    }
}

/// Seconds since `path` was last modified, or `None` if that can't be read.
fn file_age(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(now_unix().saturating_sub(secs))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run a git command, returning trimmed stdout or `None` on any failure or empty
/// output. Mirrors the helper in `forge.rs`.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
impl ReviewStore {
    /// Build a persistent store pointed at an explicit file, bypassing git scope
    /// detection so the save/load IO path is testable in isolation.
    fn with_path(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            repo: "repo".to_string(),
            branch: "branch".to_string(),
            files: HashMap::new(),
            dirty: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "diff --git a/f b/f\nindex 1111111..2222222 100644\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n";

    #[test]
    fn hash_is_stable_for_identical_diffs() {
        assert_eq!(file_hash(RAW), file_hash(RAW));
    }

    #[test]
    fn hash_ignores_index_line_churn() {
        // Only the blob SHAs on the `index` line differ; the visible change is
        // identical, so the hash — and thus the viewed mark — must not change.
        let churned = RAW.replace("1111111..2222222", "abcdef0..fedcba9");
        assert_ne!(RAW, churned);
        assert_eq!(file_hash(RAW), file_hash(&churned));
    }

    #[test]
    fn hash_changes_when_content_changes() {
        let edited = RAW.replace("+new", "+newer");
        assert_ne!(file_hash(RAW), file_hash(&edited));
    }

    #[test]
    fn toggle_round_trips_and_reports_state() {
        let mut store = ReviewStore::disabled();
        let h = file_hash(RAW);
        assert!(!store.is_viewed(h));
        assert!(store.toggle(h, "f")); // now viewed
        assert!(store.is_viewed(h));
        assert!(!store.toggle(h, "f")); // back to unviewed
        assert!(!store.is_viewed(h));
    }

    #[test]
    fn count_viewed_counts_only_marked() {
        let mut store = ReviewStore::disabled();
        let a = file_hash("a");
        let b = file_hash("b");
        store.toggle(a, "a");
        assert_eq!(store.count_viewed(&[a, b]), 1);
    }

    #[test]
    fn disabled_store_never_records_a_path() {
        // Session-only stores hold marks in memory but never get a file path.
        let mut store = ReviewStore::disabled();
        store.toggle(file_hash(RAW), "f");
        assert!(store.path.is_none());
    }

    #[test]
    fn save_persists_and_empties_clean_up() {
        // A unique dir per test process; the save path creates missing parents.
        let dir = std::env::temp_dir().join(format!("riffnav-review-{}", std::process::id()));
        let path = dir.join("nested").join("scope.json");
        let mut store = ReviewStore::with_path(path.clone());
        let hash = file_hash(RAW);

        store.toggle(hash, "f");
        store.save();
        // The on-disk file parses back with our hash recorded.
        let text = std::fs::read_to_string(&path).expect("file written");
        let parsed: StoreFile = serde_json::from_str(&text).expect("valid json");
        assert!(parsed.files.contains_key(&format!("{hash:032x}")));

        // Unmarking the last entry deletes the file rather than leaving a husk.
        store.toggle(hash, "f");
        store.save();
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_a_noop_without_changes() {
        let dir = std::env::temp_dir().join(format!("riffnav-review-clean-{}", std::process::id()));
        let path = dir.join("scope.json");
        let mut store = ReviewStore::with_path(path.clone());
        store.save(); // nothing dirty
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
