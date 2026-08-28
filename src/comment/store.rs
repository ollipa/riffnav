//! Persistent inline review comments, anchored to a line of a file's diff.
//!
//! An anchor is `(file, side, line)` in *diff-line* space — the line number git
//! prints in the hunk, never a rendered row. That's what lets a note survive a
//! theme switch, a resize, or a unified/side-by-side toggle: the mapping to a
//! screen row is derived at render time (see [`super::anchor`]) and thrown away.
//!
//! Scope, location and garbage collection are shared with the "viewed" marks —
//! see [`crate::state`] — so comments live per repository and per branch under
//! `$XDG_STATE_HOME/riffnav/comments/<repo>/<branch>.json`. Outside a git repo
//! there's no stable scope, so the store degrades to session-only: comments
//! still work, nothing persists.
//!
//! Unlike hunk (which keeps notes in the running window's memory and loses them
//! on exit), persisting to disk means `riffnav comment add` works with no window
//! open and notes survive a restart.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use twox_hash::XxHash3_128;

use crate::state::{self, DAY_SECS};

/// Which side of the diff a comment hangs on. A comment on a removed line
/// anchors to `Old`; on an added or context line, to `New`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Old => "old",
            Side::New => "new",
        }
    }
}

/// The one diff line a comment hangs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Anchor {
    pub side: Side,
    pub line: u32,
}

/// One review note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    /// Short stable handle, used by `riffnav comment rm` and `--reply-to`.
    pub id: String,
    /// The file's tree path (`FileDiff::path()`): the new path, or the old path
    /// for a deletion.
    pub file: String,
    pub side: Side,
    pub line: u32,
    pub body: String,
    pub author: String,
    /// Unix seconds when the comment was written.
    pub created: u64,
    /// The comment this one replies to, if any. Replies share their parent's
    /// anchor and render beneath it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// `review::file_hash` of the file's diff when the comment was written, so a
    /// note whose code has since changed can be flagged instead of silently
    /// sliding onto an unrelated line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_hash: Option<String>,
}

impl Comment {
    pub fn anchor(&self) -> Anchor {
        Anchor {
            side: self.side,
            line: self.line,
        }
    }

    /// Whether the file's diff has changed since this comment was written. A
    /// comment with no recorded hash (hand-edited JSON) is never stale.
    pub fn is_stale(&self, current: u128) -> bool {
        self.diff_hash
            .as_ref()
            .is_some_and(|h| *h != format!("{current:032x}"))
    }
}

/// On-disk shape of a single `<branch>.json`. `repo`/`branch` are stored only so
/// the hash-named file is self-describing when debugging.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    repo: String,
    branch: String,
    comments: Vec<Comment>,
}

/// Inline comments for the current (repo, branch) scope. A `path` of `None`
/// means session-only.
pub struct CommentStore {
    path: Option<PathBuf>,
    repo: String,
    branch: String,
    comments: Vec<Comment>,
    dirty: bool,
}

impl CommentStore {
    /// A store that never persists. Used before comments are enabled (and in
    /// tests), and as the fallback when no repo scope can be determined.
    pub fn disabled() -> Self {
        Self {
            path: None,
            repo: String::new(),
            branch: String::new(),
            comments: Vec::new(),
            dirty: false,
        }
    }

    /// Load the comments for the current repo+branch, sweeping stale branch files
    /// first. Falls back to a session-only store outside a repo.
    pub fn load(retention_days: u64) -> Self {
        let retention = retention_days.saturating_mul(DAY_SECS);
        let (Some((repo, branch)), Some(dir)) = (state::detect_scope(), state::dir("comments"))
        else {
            return Self::disabled();
        };
        let path = state::scope_path(&dir, &repo, &branch);

        // Reap branch files untouched for `retention`, but never the one we're
        // about to use — opening a branch counts as keeping it alive.
        state::sweep(&dir, retention, &path);

        let mut store = Self {
            path: Some(path),
            repo,
            branch,
            comments: Vec::new(),
            dirty: false,
        };
        store.reload();
        store
    }

    /// Re-read the backing file, discarding in-memory state. Called on startup
    /// and whenever the filesystem watcher reports the file changed — including
    /// after our own save, where it's a harmless no-op.
    pub fn reload(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let comments = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<StoreFile>(&text).ok())
            .map(|stored| stored.comments)
            .unwrap_or_default();
        self.comments = comments;
        self.dirty = false;
    }

    /// Where this scope persists, for the filesystem watcher. `None` when
    /// session-only.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn all(&self) -> &[Comment] {
        &self.comments
    }

    pub fn get(&self, id: &str) -> Option<&Comment> {
        self.comments.iter().find(|c| c.id == id)
    }

    /// How many comments are anchored in `file`.
    pub fn count_for_file(&self, file: &str) -> usize {
        self.comments.iter().filter(|c| c.file == file).count()
    }

    /// Comments on `file`, grouped by anchor and ordered for display: anchors in
    /// line order, and within each anchor every root comment followed by the
    /// thread beneath it, oldest first.
    ///
    /// The walk under a root is depth-first, so a reply to a reply lands under
    /// the note it answers instead of vanishing — `c` on a reply row, and
    /// `comment add --reply-to <reply-id>`, both produce exactly that.
    ///
    /// A reply whose parent is missing (the parent was deleted) is promoted to a
    /// root so the text is never silently dropped, and so is anything a hand-
    /// edited file leaves unreachable.
    pub fn threads(&self, file: &str) -> Vec<(Anchor, Vec<&Comment>)> {
        let mut anchors: Vec<Anchor> = self
            .comments
            .iter()
            .filter(|c| c.file == file)
            .map(|c| c.anchor())
            .collect();
        anchors.sort_unstable();
        anchors.dedup();

        anchors
            .into_iter()
            .map(|anchor| {
                let here: Vec<&Comment> = self
                    .comments
                    .iter()
                    .filter(|c| c.file == file && c.anchor() == anchor)
                    .collect();
                // Oldest first, with the id as a tiebreaker so the order is
                // stable across runs for comments written in the same second.
                let by_age =
                    |a: &&Comment, b: &&Comment| (a.created, &a.id).cmp(&(b.created, &b.id));

                let mut roots: Vec<&Comment> = here
                    .iter()
                    .copied()
                    .filter(|c| {
                        c.reply_to
                            .as_ref()
                            .is_none_or(|p| !here.iter().any(|o| &o.id == p))
                    })
                    .collect();
                roots.sort_by(by_age);

                let mut ordered: Vec<&Comment> = Vec::with_capacity(here.len());
                // Oldest root first, so popping the stack visits them in order.
                let mut stack: Vec<&Comment> = roots.into_iter().rev().collect();
                while let Some(c) = stack.pop() {
                    if ordered.iter().any(|seen| seen.id == c.id) {
                        continue; // only reachable from a hand-edited cycle
                    }
                    ordered.push(c);
                    let mut replies: Vec<&Comment> = here
                        .iter()
                        .copied()
                        .filter(|r| r.reply_to.as_deref() == Some(c.id.as_str()))
                        .collect();
                    replies.sort_by(by_age);
                    stack.extend(replies.into_iter().rev());
                }
                // Nothing may be dropped: a cycle among replies has no root to
                // descend from, so sweep up whatever the walk couldn't reach.
                let mut stranded: Vec<&Comment> = here
                    .iter()
                    .copied()
                    .filter(|c| !ordered.iter().any(|seen| seen.id == c.id))
                    .collect();
                stranded.sort_by(by_age);
                ordered.extend(stranded);
                (anchor, ordered)
            })
            .collect()
    }

    /// Record a new comment, assigning it a short unique id which is returned.
    pub fn add(&mut self, mut comment: Comment) -> String {
        comment.id = self.mint_id(&comment);
        let id = comment.id.clone();
        self.comments.push(comment);
        self.dirty = true;
        id
    }

    /// Delete the comment with `id` and the whole thread beneath it — replies to
    /// its replies included, or they'd be left dangling as roots of their own.
    /// Returns how many were removed.
    pub fn remove(&mut self, id: &str) -> usize {
        let before = self.comments.len();
        let mut doomed = vec![id.to_string()];
        // Each pass adopts the children of everything condemned so far; the set
        // only grows, so a fixed point is reached in at most `comments` passes.
        loop {
            let next: Vec<String> = self
                .comments
                .iter()
                .filter(|c| {
                    !doomed.contains(&c.id)
                        && c.reply_to.as_ref().is_some_and(|p| doomed.contains(p))
                })
                .map(|c| c.id.clone())
                .collect();
            if next.is_empty() {
                break;
            }
            doomed.extend(next);
        }
        self.comments.retain(|c| !doomed.contains(&c.id));
        let removed = before - self.comments.len();
        self.dirty |= removed > 0;
        removed
    }

    /// Delete every comment, or only those on `file`. Returns how many went.
    pub fn clear(&mut self, file: Option<&str>) -> usize {
        let before = self.comments.len();
        match file {
            Some(f) => self.comments.retain(|c| c.file != f),
            None => self.comments.clear(),
        }
        let removed = before - self.comments.len();
        self.dirty |= removed > 0;
        removed
    }

    /// Persist pending changes atomically, best-effort. An emptied scope deletes
    /// its file rather than leaving a husk behind.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(path) = self.path.clone() else {
            return;
        };
        if self.comments.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let stored = StoreFile {
            repo: self.repo.clone(),
            branch: self.branch.clone(),
            comments: self.comments.clone(),
        };
        if let Ok(json) = serde_json::to_vec_pretty(&stored) {
            state::write_atomic(&path, &json);
        }
    }

    /// A short handle derived from the comment's content, salted until it doesn't
    /// collide with an existing id. Content-derived rather than random so the
    /// module stays free of a RNG dependency and ids are reproducible in tests.
    fn mint_id(&self, c: &Comment) -> String {
        for salt in 0u32.. {
            let seed = format!(
                "{salt}\u{0}{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
                c.file,
                c.side.as_str(),
                c.line,
                c.created,
                c.body
            );
            let id = format!(
                "{:06x}",
                XxHash3_128::oneshot(seed.as_bytes()) as u32 & 0xff_ffff
            );
            if !self.comments.iter().any(|o| o.id == id) {
                return id;
            }
        }
        unreachable!("u32 range exhausted minting a comment id")
    }
}

#[cfg(test)]
impl CommentStore {
    /// Build a persistent store pointed at an explicit file, bypassing git scope
    /// detection so the save/load IO path is testable in isolation.
    pub(crate) fn with_path(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            repo: "repo".to_string(),
            branch: "branch".to_string(),
            comments: Vec::new(),
            dirty: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(file: &str, line: u32, body: &str) -> Comment {
        Comment {
            id: String::new(),
            file: file.to_string(),
            side: Side::New,
            line,
            body: body.to_string(),
            author: "tester".to_string(),
            created: 100,
            reply_to: None,
            diff_hash: None,
        }
    }

    #[test]
    fn add_assigns_unique_ids_even_for_identical_comments() {
        let mut store = CommentStore::disabled();
        let a = store.add(draft("f", 1, "same"));
        let b = store.add(draft("f", 1, "same"));
        assert_ne!(a, b, "identical drafts must still get distinct ids");
        assert_eq!(a.len(), 6);
        assert_eq!(store.all().len(), 2);
    }

    #[test]
    fn threads_orders_by_anchor_then_puts_replies_under_their_root() {
        let mut store = CommentStore::disabled();
        let root = store.add(draft("f", 10, "root"));
        let mut later = draft("f", 10, "second root");
        later.created = 200;
        store.add(later);
        let mut reply = draft("f", 10, "reply");
        reply.created = 150;
        reply.reply_to = Some(root.clone());
        store.add(reply);
        store.add(draft("f", 3, "earlier line"));

        let threads = store.threads("f");
        assert_eq!(threads.len(), 2);
        // Anchors come out in line order.
        assert_eq!(threads[0].0.line, 3);
        assert_eq!(threads[1].0.line, 10);
        // The reply sits directly under its root, ahead of the newer root.
        let bodies: Vec<&str> = threads[1].1.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, ["root", "reply", "second root"]);
    }

    /// `c` on a reply row threads the new note under *that* reply, so a nested
    /// reply has to render — and has to go when the thread above it is deleted.
    #[test]
    fn a_reply_to_a_reply_stays_in_the_thread() {
        let mut store = CommentStore::disabled();
        let root = store.add(draft("f", 10, "root"));
        let mut reply = draft("f", 10, "reply");
        reply.created = 150;
        reply.reply_to = Some(root.clone());
        let reply = store.add(reply);
        let mut nested = draft("f", 10, "reply to the reply");
        nested.created = 200;
        nested.reply_to = Some(reply.clone());
        store.add(nested);
        // A second root, newer than the whole thread above it.
        let mut later = draft("f", 10, "second root");
        later.created = 300;
        store.add(later);

        let threads = store.threads("f");
        let bodies: Vec<&str> = threads[0].1.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(
            bodies,
            ["root", "reply", "reply to the reply", "second root"],
            "the nested reply follows the note it answers"
        );

        // Deleting the root takes the whole subtree, not just its direct replies.
        assert_eq!(store.remove(&root), 3);
        assert_eq!(store.threads("f")[0].1.len(), 1);
    }

    #[test]
    fn orphaned_reply_is_promoted_rather_than_dropped() {
        let mut store = CommentStore::disabled();
        let root = store.add(draft("f", 1, "root"));
        let mut reply = draft("f", 1, "reply");
        reply.created = 200;
        reply.reply_to = Some(root.clone());
        store.add(reply);

        assert_eq!(store.remove(&root), 2, "removing a root takes its replies");
        assert!(store.threads("f").is_empty());

        // A reply pointing at an id that was never here still renders.
        let mut orphan = draft("f", 1, "orphan");
        orphan.reply_to = Some("deadbe".to_string());
        store.add(orphan);
        assert_eq!(store.threads("f")[0].1.len(), 1);
    }

    #[test]
    fn side_distinguishes_anchors_on_the_same_line() {
        let mut store = CommentStore::disabled();
        store.add(draft("f", 7, "on new"));
        let mut old = draft("f", 7, "on old");
        old.side = Side::Old;
        store.add(old);

        let threads = store.threads("f");
        assert_eq!(threads.len(), 2, "old/new line 7 are different anchors");
        assert_eq!(threads[0].0.side, Side::Old);
        assert_eq!(threads[1].0.side, Side::New);
    }

    #[test]
    fn stale_only_when_the_recorded_hash_differs() {
        let mut c = draft("f", 1, "x");
        assert!(!c.is_stale(42), "no recorded hash means never stale");
        c.diff_hash = Some(format!("{:032x}", 42u128));
        assert!(!c.is_stale(42));
        assert!(c.is_stale(43));
    }

    #[test]
    fn count_and_clear_are_scoped_to_one_file() {
        let mut store = CommentStore::disabled();
        store.add(draft("a", 1, "x"));
        store.add(draft("a", 2, "y"));
        store.add(draft("b", 1, "z"));
        assert_eq!(store.count_for_file("a"), 2);
        assert_eq!(store.clear(Some("a")), 2);
        assert_eq!(store.count_for_file("a"), 0);
        assert_eq!(store.count_for_file("b"), 1);
    }

    #[test]
    fn save_round_trips_and_emptying_removes_the_file() {
        let dir = std::env::temp_dir().join(format!("riffnav-comments-{}", std::process::id()));
        let path = dir.join("nested").join("scope.json");
        let mut store = CommentStore::with_path(path.clone());
        let id = store.add(draft("src/app.rs", 103, "why the retry here?"));
        store.save();

        let mut reloaded = CommentStore::with_path(path.clone());
        reloaded.reload();
        let got = reloaded.get(&id).expect("comment survived the round trip");
        assert_eq!(got.body, "why the retry here?");
        assert_eq!(got.line, 103);
        assert_eq!(got.side, Side::New);

        reloaded.remove(&id);
        reloaded.save();
        assert!(!path.exists(), "an emptied scope deletes its file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_a_noop_without_changes() {
        let dir =
            std::env::temp_dir().join(format!("riffnav-comments-clean-{}", std::process::id()));
        let path = dir.join("scope.json");
        let mut store = CommentStore::with_path(path.clone());
        store.save();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
