//! Diffs for tests to start from.

use super::{FileDiff, FileStatus, parse};

/// The `diff --git` header of a file at `path`.
pub fn header(path: &str) -> String {
    format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n")
}

/// A diff of `path` with one hunk from line 1, its line counts read off `body`.
pub fn hunk(path: &str, body: &str) -> String {
    let old = body.lines().filter(|l| !l.starts_with('+')).count();
    let new = body.lines().filter(|l| !l.starts_with('-')).count();
    format!("{}@@ -1,{old} +1,{new} @@\n{body}", header(path))
}

/// The first file of `raw`.
pub fn one(raw: &str) -> FileDiff {
    parse(raw).remove(0)
}

/// A file at `path` with no diff text.
pub fn bare(path: &str, status: FileStatus) -> FileDiff {
    FileDiff {
        old_path: None,
        new_path: Some(path.to_string()),
        status,
        additions: 0,
        deletions: 0,
        raw: String::new(),
    }
}
