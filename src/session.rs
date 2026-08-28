//! What the running window is showing, published for the `riffnav comment`
//! subcommands.
//!
//! An agent leaving notes runs `riffnav comment add` as a *separate process*,
//! which has no diff of its own: the one on screen may have come from a pipe
//! (`git show abc | riffnav`) that can't be reproduced. So the TUI writes its
//! parsed file and hunk structure to a small JSON file, and the CLI reads it to
//! answer "does this file exist, and is line 103 actually in it?".
//!
//! This is deliberately *not* a daemon. hunk solves the same problem with a
//! loopback HTTP broker on a fixed port, which its own docs then have to explain
//! how to unblock inside an agent sandbox. A file needs no port, no process, and
//! survives the window closing.
//!
//! Scoped per repo and branch like the other state (see [`crate::state`]), so two
//! windows on the same branch share one entry — they're showing the same diff.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::comment::Side;
use crate::diff::{FileDiff, Hunk};
use crate::state;

/// One file in the diff on screen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFile {
    /// The tree path a comment's `--file` must match.
    pub path: String,
    /// The pre-image path, when it differs (a rename), also accepted by `--file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    /// `review::file_hash` of this file's diff, recorded on comments written
    /// against it so they can later be flagged as stale.
    pub diff_hash: String,
    pub hunks: Vec<Hunk>,
}

impl SessionFile {
    /// Whether `line` on the given side is inside any of this file's hunks.
    pub fn covers(&self, side: Side, line: u32) -> bool {
        self.hunks.iter().any(|h| match side {
            Side::Old => h.contains_old(line),
            Side::New => h.contains_new(line),
        })
    }

    /// Human-readable list of the line ranges a comment may target, for the error
    /// message when one doesn't.
    pub fn ranges(&self, side: Side) -> String {
        let spans: Vec<String> = self
            .hunks
            .iter()
            .filter_map(|h| {
                let (start, len) = match side {
                    Side::Old => (h.old_start, h.old_len),
                    Side::New => (h.new_start, h.new_len),
                };
                (len > 0).then(|| format!("{}-{}", start, start + len - 1))
            })
            .collect();
        if spans.is_empty() {
            "none".to_string()
        } else {
            spans.join(", ")
        }
    }
}

/// The published state of one riffnav window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Recorded for debugging a stale file; liveness isn't checked, since a
    /// validation failure against a stale session is reported clearly anyway.
    pub pid: u32,
    pub started: u64,
    /// Which diff is on screen ("all uncommitted", "stdin", …).
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub files: Vec<SessionFile>,
}

impl Session {
    /// Snapshot the file set on screen.
    pub fn new(files: &[FileDiff], source: &str, base: Option<String>) -> Self {
        Self {
            pid: std::process::id(),
            started: state::now_unix(),
            source: source.to_string(),
            base,
            files: files.iter().map(describe).collect(),
        }
    }

    /// The file matching `path` by either its post- or pre-image name, so a
    /// rename can be commented on under whichever name the agent knows.
    pub fn file(&self, path: &str) -> Option<&SessionFile> {
        self.files
            .iter()
            .find(|f| f.path == path || f.old_path.as_deref() == Some(path))
    }

    /// Where this scope's session file lives, or `None` outside a repo.
    pub fn path() -> Option<PathBuf> {
        let (repo, branch) = state::detect_scope()?;
        let dir = state::dir("sessions")?;
        Some(state::scope_path(&dir, &repo, &branch))
    }

    /// Publish this snapshot, best-effort. A window that can't write its session
    /// still works; the CLI just falls back to re-deriving the diff from git.
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            state::write_atomic(&path, &json);
        }
    }

    /// Read the session published for the current scope, if any.
    pub fn load() -> Option<Self> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Remove this scope's session file on exit, so the CLI doesn't keep
    /// validating against a window that's gone.
    pub fn clear() {
        if let Some(path) = Self::path() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn describe(f: &FileDiff) -> SessionFile {
    let path = f.path().to_string();
    SessionFile {
        old_path: f.old_path.clone().filter(|p| *p != path),
        path,
        status: f.status.sigil().to_string(),
        additions: f.additions,
        deletions: f.deletions,
        diff_hash: format!("{:032x}", crate::review::file_hash(&f.raw)),
        hunks: crate::diff::hunks(&f.raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        let files = crate::diff::parse(
            "diff --git a/src/app.rs b/src/app.rs\n--- a/src/app.rs\n+++ b/src/app.rs\n\
             @@ -10,3 +12,4 @@\n ctx\n-gone\n+one\n+two\n",
        );
        Session::new(&files, "stdin", None)
    }

    #[test]
    fn describes_files_with_their_hunk_ranges() {
        let s = session();
        let f = s.file("src/app.rs").expect("file is in the session");
        assert_eq!(f.status, "M");
        assert_eq!(f.hunks.len(), 1);
        assert_eq!((f.hunks[0].new_start, f.hunks[0].new_len), (12, 4));
        assert_eq!(f.diff_hash.len(), 32);
    }

    #[test]
    fn covers_only_lines_inside_a_hunk() {
        let s = session();
        let f = s.file("src/app.rs").unwrap();
        assert!(f.covers(Side::New, 12));
        assert!(f.covers(Side::New, 15));
        assert!(!f.covers(Side::New, 16));
        assert!(f.covers(Side::Old, 10));
        assert!(!f.covers(Side::Old, 13));
    }

    #[test]
    fn ranges_reads_as_the_targets_a_comment_may_use() {
        let s = session();
        let f = s.file("src/app.rs").unwrap();
        assert_eq!(f.ranges(Side::New), "12-15");
        assert_eq!(f.ranges(Side::Old), "10-12");
    }

    #[test]
    fn a_renamed_file_is_findable_under_either_name() {
        let files = crate::diff::parse(
            "diff --git a/old.rs b/new.rs\nsimilarity index 90%\n\
             rename from old.rs\nrename to new.rs\n",
        );
        let s = Session::new(&files, "stdin", None);
        assert!(s.file("new.rs").is_some());
        assert!(s.file("old.rs").is_some());
        assert!(s.file("other.rs").is_none());
    }
}
