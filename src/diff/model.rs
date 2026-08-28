use serde::{Deserialize, Serialize};

/// How a file changed, derived from the diff's extended headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

impl FileStatus {
    /// Single-letter status used in the tree and the `--list` debug output.
    pub fn sigil(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
        }
    }
}

/// One file's section of a unified diff: the metadata riffnav needs for the tree
/// plus the exact `raw` bytes to hand to delta for rendering.
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// Pre-image path (`None` for an added file).
    pub old_path: Option<String>,
    /// Post-image path (`None` for a deleted file).
    pub new_path: Option<String>,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
    /// Verbatim diff text for this file, starting at its `diff --git` line.
    pub raw: String,
}

impl FileDiff {
    /// The path used to place this file in the tree: the new path normally,
    /// falling back to the old path for deletions.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("(unknown)")
    }
}

/// One `@@ -old_start,old_len +new_start,new_len @@` region of a file's diff.
///
/// Parsed on demand from [`FileDiff::raw`] (see `diff::hunks`) rather than stored
/// on `FileDiff`: only the comment CLI needs it, and keeping it out of the struct
/// leaves every existing `FileDiff` construction site untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// The `@@ … @@` header line verbatim, including any trailing context.
    pub header: String,
}

impl Hunk {
    /// Whether `line` falls within this hunk's pre-image range. A zero-length
    /// side (an added file's `-0,0`) contains nothing.
    pub fn contains_old(&self, line: u32) -> bool {
        self.old_len > 0 && line >= self.old_start && line < self.old_start + self.old_len
    }

    /// Whether `line` falls within this hunk's post-image range.
    pub fn contains_new(&self, line: u32) -> bool {
        self.new_len > 0 && line >= self.new_start && line < self.new_start + self.new_len
    }
}
