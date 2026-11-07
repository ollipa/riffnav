//! Persistent inline review comments, anchored to a line of a file's diff.
//!
//! An anchor is `(file, side, line)` in *diff-line* space — the line number git
//! prints in the hunk, never a rendered row. That's what lets a note survive a
//! theme switch, a resize, or a unified/side-by-side toggle: the mapping to a
//! screen row is derived at render time (see [`super::anchor`]) and thrown away.
//!
//! Scope, location and garbage collection are shared with the "viewed" marks (see
//! [`crate::state`]). Outside a git repo the store is session-only: comments still
//! work, nothing persists.
//!
//! Persisting to disk means `riffnav comment add` works with no window open and
//! notes survive a restart. Every write goes through [`CommentStore::update`],
//! which re-reads the file under a lock first, so a window and any number of
//! `riffnav comment` runs can write at once without losing each other's notes.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use twox_hash::XxHash3_128;

use super::snapshot::Snapshots;
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
    /// Key of the snapshot of this side of the file as it was when the comment
    /// was written, or last moved (see [`super::reanchor`]). Diffing it against
    /// the file as it is now tells where the commented line went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// The commented line's text, to find it again should the snapshot be gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The commented line was rewritten or deleted since, so the note now sits on
    /// the code that replaced it, or the nearest line that survived.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outdated: bool,
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
    /// The comments tree's root, whose `.lock` guards every write (see
    /// [`state::lock`]).
    root: PathBuf,
    repo: String,
    branch: String,
    comments: Vec<Comment>,
    /// How many comments render in each file, by their thread root's file the
    /// way [`Self::threads`] places them. Rebuilt on every change, since the
    /// tree asks for it per row per frame.
    counts: HashMap<String, usize>,
    /// The file snapshots comments are anchored against.
    snapshots: Snapshots,
    dirty: bool,
}

impl CommentStore {
    /// A store that never persists. Used before comments are enabled (and in
    /// tests), and as the fallback when no repo scope can be determined.
    pub fn disabled() -> Self {
        Self {
            path: None,
            root: PathBuf::new(),
            repo: String::new(),
            branch: String::new(),
            comments: Vec::new(),
            counts: HashMap::new(),
            snapshots: Snapshots::memory(),
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
        let snapshots = state::dir("snapshots").map_or_else(Snapshots::memory, Snapshots::at);

        // Reap branch files untouched for `retention`, but never the one we're
        // about to use — opening a branch counts as keeping it alive. Under the
        // lock, so a file another process is rewriting isn't deleted under it.
        // Then the snapshots no comment left anywhere refers to.
        if let Some(_lock) = state::lock(&dir) {
            state::sweep(&dir, retention, &path);
            snapshots.sweep(&dir);
        }

        let mut store = Self {
            path: Some(path),
            root: dir,
            repo,
            branch,
            comments: Vec::new(),
            counts: HashMap::new(),
            snapshots,
            dirty: false,
        };
        store.reload();
        store
    }

    /// Re-read the backing file, discarding in-memory state. Called on startup
    /// and whenever the filesystem watcher reports the file changed — including
    /// after our own save, where it's a harmless no-op. A file that won't parse
    /// reads as empty here; [`Self::update`] refuses to write over it.
    pub fn reload(&mut self) {
        let _ = self.read();
    }

    /// [`Self::reload`], saying whether the file was readable. A missing file is
    /// an empty store, not an error.
    fn read(&mut self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let read = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<StoreFile>(&text)
                .map(|stored| stored.comments)
                .map_err(|e| {
                    let why = format!("the store doesn't parse ({e}): {}", path.display());
                    io::Error::new(io::ErrorKind::InvalidData, why)
                }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        };
        let (comments, result) = match read {
            Ok(comments) => (comments, Ok(())),
            Err(e) => (Vec::new(), Err(e)),
        };
        self.comments = comments;
        self.dirty = false;
        self.recount();
        result
    }

    /// Change the comments as they are on disk now, not as this store last read
    /// them, and write the result: lock, re-read, apply `f`, write, unlock. That
    /// way a note another process saved since the last read survives, whoever
    /// writes next. Returns what `f` returned.
    ///
    /// Fails without writing when the lock can't be taken, the file is there but
    /// unreadable (overwriting it would lose every note in it), or the write
    /// fails. The store is then left as the file is. A session-only store just
    /// applies `f`.
    pub fn update<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> io::Result<R> {
        if self.path.is_none() {
            return Ok(f(self));
        }
        let Some(_lock) = state::lock(&self.root) else {
            return Err(io::Error::other(format!(
                "couldn't lock {}",
                self.root.join(".lock").display()
            )));
        };
        self.read()?;
        let out = f(self);
        if let Err(e) = self.save() {
            self.reload();
            return Err(e);
        }
        Ok(out)
    }

    /// Where this scope persists, for the filesystem watcher. `None` when
    /// session-only.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The repository's top folder, empty when session-only.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// The branch, `(detached)` off one, empty when session-only.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    pub fn all(&self) -> &[Comment] {
        &self.comments
    }

    pub fn get(&self, id: &str) -> Option<&Comment> {
        self.comments.iter().find(|c| c.id == id)
    }

    pub fn snapshots(&self) -> &Snapshots {
        &self.snapshots
    }

    /// Change the comment with `id` in place, saying whether there was one.
    pub fn edit(&mut self, id: &str, f: impl FnOnce(&mut Comment)) -> bool {
        let Some(c) = self.comments.iter_mut().find(|c| c.id == id) else {
            return false;
        };
        f(c);
        self.dirty = true;
        self.recount();
        true
    }

    /// How many comments render in `file`: the notes of every thread rooted
    /// there, the same set [`Self::threads`] draws.
    pub fn count_for_file(&self, file: &str) -> usize {
        self.counts.get(file).copied().unwrap_or(0)
    }

    /// The file a comment renders in: its thread root's (see [`Self::root_of`]).
    pub fn file_of<'a>(&'a self, c: &'a Comment) -> &'a str {
        &self.root_of(c).file
    }

    /// Rebuild [`Self::counts`] after the comments changed.
    fn recount(&mut self) {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for c in &self.comments {
            *counts.entry(self.file_of(c).to_string()).or_default() += 1;
        }
        self.counts = counts;
    }

    /// The comment a thread hangs from: `c` itself when it starts one, otherwise
    /// the furthest ancestor its `reply_to` chain reaches.
    ///
    /// The root's anchor is the thread's. A reply whose parent isn't in the store
    /// is its own root, so an orphan is promoted rather than dropped; so is one
    /// caught in a `reply_to` cycle a hand-edited file left behind.
    pub fn root_of<'a>(&'a self, c: &'a Comment) -> &'a Comment {
        let mut root = c;
        let mut seen = vec![root.id.as_str()];
        while let Some(parent) = root.reply_to.as_deref().and_then(|p| self.get(p)) {
            if seen.contains(&parent.id.as_str()) {
                break; // only reachable from a hand-edited cycle
            }
            seen.push(parent.id.as_str());
            root = parent;
        }
        root
    }

    /// Comments on `file`, grouped by anchor and ordered for display: anchors in
    /// line order, and within each anchor every root followed depth-first by its
    /// thread, oldest first. Each comment sits at its thread's anchor (see
    /// [`Self::root_of`]), so a reply stored on a shifted line stays in its thread.
    pub fn threads(&self, file: &str) -> Vec<(Anchor, Vec<&Comment>)> {
        // Resolve each comment's thread anchor once: `root_of` walks a chain, and
        // the grouping below would otherwise re-walk it for every anchor.
        let placed: Vec<(&Comment, Anchor)> = self
            .comments
            .iter()
            .map(|c| (c, self.root_of(c)))
            .filter(|(_, root)| root.file == file)
            .map(|(c, root)| (c, root.anchor()))
            .collect();

        let mut anchors: Vec<Anchor> = placed.iter().map(|(_, a)| *a).collect();
        anchors.sort_unstable();
        anchors.dedup();

        anchors
            .into_iter()
            .map(|anchor| {
                let here: Vec<&Comment> = placed
                    .iter()
                    .filter(|(_, a)| *a == anchor)
                    .map(|(c, _)| *c)
                    .collect();
                // Oldest first, with the id as a tiebreaker so the order is
                // stable across runs for comments written in the same second.
                let by_age =
                    |a: &&Comment, b: &&Comment| (a.created, &a.id).cmp(&(b.created, &b.id));

                // A parent absent from `here` is a parent absent from the store:
                // every descendant of a root resolves to that root's anchor, so
                // the whole thread groups together whatever lines it carries.
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
        self.recount();
        id
    }

    /// `id` and every comment replying beneath it, transitively — exactly what a
    /// delete of `id` would take with it, so it can be counted before it goes.
    /// Empty when nothing carries that id.
    pub fn thread_ids(&self, id: &str) -> Vec<String> {
        let mut doomed: Vec<String> = self
            .comments
            .iter()
            .filter(|c| c.id == id)
            .map(|c| c.id.clone())
            .collect();
        // Each pass adopts the children of everything condemned so far; the set
        // only grows, so a fixed point is reached in at most `comments` passes.
        while !doomed.is_empty() {
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
        doomed
    }

    /// Delete the comment with `id` and the whole thread beneath it — replies to
    /// its replies included, or they'd be left dangling as roots of their own.
    /// Returns how many were removed.
    pub fn remove(&mut self, id: &str) -> usize {
        let doomed = self.thread_ids(id);
        self.drop_ids(&doomed)
    }

    /// Delete every comment for which `doomed` holds, given the store and the
    /// comment. Returns how many went.
    pub fn remove_where(&mut self, doomed: impl Fn(&Self, &Comment) -> bool) -> usize {
        let ids: Vec<String> = self
            .comments
            .iter()
            .filter(|c| doomed(self, c))
            .map(|c| c.id.clone())
            .collect();
        self.drop_ids(&ids)
    }

    fn drop_ids(&mut self, ids: &[String]) -> usize {
        let before = self.comments.len();
        self.comments.retain(|c| !ids.contains(&c.id));
        let removed = before - self.comments.len();
        if removed > 0 {
            self.dirty = true;
            self.recount();
        }
        removed
    }

    /// Persist pending changes atomically. An emptied scope deletes its file
    /// rather than leaving a husk behind. Only [`Self::update`] calls this, with
    /// the lock held and the file freshly read.
    fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        if self.comments.is_empty() {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        } else {
            let stored = StoreFile {
                repo: self.repo.clone(),
                branch: self.branch.clone(),
                comments: self.comments.clone(),
            };
            let json = serde_json::to_vec_pretty(&stored).map_err(io::Error::other)?;
            if !state::write_atomic(&path, &json) {
                return Err(io::Error::other(format!(
                    "couldn't write {}",
                    path.display()
                )));
            }
        }
        self.dirty = false;
        Ok(())
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
            root: path.parent().expect("a file in a directory").to_path_buf(),
            snapshots: Snapshots::at(path.with_extension("snapshots")),
            path: Some(path),
            repo: "repo".to_string(),
            branch: "branch".to_string(),
            comments: Vec::new(),
            counts: HashMap::new(),
            dirty: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::testing::note;
    use crate::testing::TempDir;

    fn draft(file: &str, line: u32, body: &str) -> Comment {
        Comment {
            created: 100,
            ..note(file, line, body)
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

    /// The diff moves during a review, so a reply can carry a line its parent
    /// doesn't. It still renders at the anchor its thread hangs on.
    #[test]
    fn a_reply_stored_on_another_line_still_renders_inside_its_thread() {
        let mut store = CommentStore::disabled();
        let root = store.add(draft("f", 100, "no backoff here"));
        let mut reply = draft("f", 92, "fixed — the loop backs off now");
        reply.created = 200;
        reply.reply_to = Some(root.clone());
        let reply = store.add(reply);
        // A reply to that reply, drifted again.
        let mut nested = draft("f", 80, "thanks");
        nested.created = 300;
        nested.reply_to = Some(reply);
        store.add(nested);

        let threads = store.threads("f");
        assert_eq!(threads.len(), 1, "one conversation is one box: {threads:?}");
        assert_eq!(threads[0].0.line, 100, "the root's anchor governs");
        let bodies: Vec<&str> = threads[0].1.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(
            bodies,
            [
                "no backoff here",
                "fixed — the loop backs off now",
                "thanks"
            ]
        );
    }

    /// A reply follows its parent even across files, so a hand-edited store can't
    /// leave half a thread hanging off a file the rest of it isn't in.
    #[test]
    fn a_reply_stored_against_another_file_follows_its_parent() {
        let mut store = CommentStore::disabled();
        let root = store.add(draft("a", 10, "root"));
        let mut reply = draft("b", 3, "reply");
        reply.created = 200;
        reply.reply_to = Some(root);
        store.add(reply);

        assert!(store.threads("b").is_empty(), "not a thread of its own");
        let threads = store.threads("a");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].1.len(), 2);
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

    /// Only a hand-edited file can produce one, but resolving a thread's anchor
    /// walks `reply_to`, so a cycle must terminate and keep every note visible.
    #[test]
    fn a_reply_to_cycle_terminates_and_drops_nothing() {
        let mut store = CommentStore::disabled();
        let a = store.add(draft("f", 1, "a"));
        let mut second = draft("f", 1, "b");
        second.reply_to = Some(a.clone());
        let b = store.add(second);
        // Close the loop: a replies to b, which replies to a.
        store.comments[0].reply_to = Some(b);

        let shown: usize = store.threads("f").iter().map(|(_, t)| t.len()).sum();
        assert_eq!(shown, 2, "both notes still render");
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
    fn count_and_removal_are_scoped_to_one_file() {
        let mut store = CommentStore::disabled();
        store.add(draft("a", 1, "x"));
        store.add(draft("a", 2, "y"));
        store.add(draft("b", 1, "z"));
        assert_eq!(store.count_for_file("a"), 2);
        assert_eq!(store.remove_where(|s, c| s.file_of(c) == "a"), 2);
        assert_eq!(store.count_for_file("a"), 0);
        assert_eq!(store.count_for_file("b"), 1);
    }

    /// The tree's badge counts what the threads draw, even for a reply a
    /// hand-edited store files under another path than its root.
    #[test]
    fn the_count_agrees_with_the_threads_drawn() {
        let mut store = CommentStore::disabled();
        let root = store.add(draft("a", 10, "root"));
        let mut reply = draft("b", 3, "reply filed under b");
        reply.reply_to = Some(root.clone());
        store.add(reply);
        let mut orphan = draft("b", 4, "reply to nothing");
        orphan.reply_to = Some("gone00".to_string());
        store.add(orphan);

        for file in ["a", "b", "c"] {
            let drawn: usize = store.threads(file).iter().map(|(_, t)| t.len()).sum();
            assert_eq!(store.count_for_file(file), drawn, "{file}");
        }
        assert_eq!(store.count_for_file("a"), 2);

        // Removing the root takes its reply off a's count, not b's.
        store.remove(&root);
        assert_eq!(store.count_for_file("a"), 0);
        assert_eq!(store.count_for_file("b"), 1);
    }

    #[test]
    fn update_round_trips_and_emptying_removes_the_file() {
        let dir = TempDir::new("comments");
        let path = dir.join("nested").join("scope.json");
        let mut store = CommentStore::with_path(path.clone());
        let id = store
            .update(|s| s.add(draft("src/app.rs", 103, "why the retry here?")))
            .unwrap();

        let mut reloaded = CommentStore::with_path(path.clone());
        reloaded.reload();
        let got = reloaded.get(&id).expect("comment survived the round trip");
        assert_eq!(got.body, "why the retry here?");
        assert_eq!(got.line, 103);
        assert_eq!(got.side, Side::New);

        assert_eq!(reloaded.update(|s| s.remove(&id)).unwrap(), 1);
        assert!(!path.exists(), "an emptied scope deletes its file");
    }

    #[test]
    fn an_update_that_changes_nothing_writes_nothing() {
        let dir = TempDir::new("comments-clean");
        let path = dir.join("scope.json");
        let mut store = CommentStore::with_path(path.clone());
        assert_eq!(store.update(|s| s.remove("nope")).unwrap(), 0);
        assert!(!path.exists());
    }

    /// A window's store was read before an agent wrote; its next write must
    /// keep the agent's note rather than write back what it read.
    #[test]
    fn an_update_keeps_a_note_written_since_the_last_read() {
        let dir = TempDir::new("comments-merge");
        let path = dir.join("scope.json");
        let mut window = CommentStore::with_path(path.clone());
        window.reload();

        let mut agent = CommentStore::with_path(path.clone());
        agent
            .update(|s| s.add(draft("f", 2, "agent note")))
            .unwrap();

        window.update(|s| s.add(draft("f", 5, "my note"))).unwrap();
        let mut bodies: Vec<String> = window.all().iter().map(|c| c.body.clone()).collect();
        bodies.sort();
        assert_eq!(bodies, ["agent note", "my note"], "in memory");

        let mut fresh = CommentStore::with_path(path);
        fresh.reload();
        assert_eq!(fresh.all().len(), 2, "and on disk");
    }

    /// A delete is applied to what's on disk too, so it can't resurrect or drop
    /// someone else's note either.
    #[test]
    fn a_delete_keeps_a_note_written_since_the_last_read() {
        let dir = TempDir::new("comments-delete");
        let path = dir.join("scope.json");
        let mut window = CommentStore::with_path(path.clone());
        let mine = window.update(|s| s.add(draft("f", 5, "mine"))).unwrap();

        let mut agent = CommentStore::with_path(path.clone());
        agent
            .update(|s| s.add(draft("f", 2, "agent note")))
            .unwrap();

        assert_eq!(window.update(|s| s.remove(&mine)).unwrap(), 1);
        let bodies: Vec<&str> = window.all().iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, ["agent note"]);
    }

    /// A file that's there but won't parse holds notes we can't see. Writing
    /// over it would lose all of them, so the write is refused.
    #[test]
    fn an_unreadable_store_is_not_written_over() {
        let dir = TempDir::new("comments-corrupt");
        let path = dir.join("scope.json");
        std::fs::write(&path, "{\"comments\": [ half a file").unwrap();
        let mut store = CommentStore::with_path(path.clone());
        let err = store.update(|s| s.add(draft("f", 1, "x"))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"comments\": [ half a file",
            "the file is left as it was"
        );
    }

    /// Writers in parallel, each adding its notes one update at a time: nothing
    /// is lost and the file always parses.
    #[test]
    fn parallel_updates_lose_nothing() {
        let dir = TempDir::new("comments-parallel");
        let path = dir.join("scope.json");
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut store = CommentStore::with_path(path);
                    for i in 0..25 {
                        store
                            .update(|s| s.add(draft("f", i, &format!("{w}-{i}"))))
                            .unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        let mut store = CommentStore::with_path(path);
        store.read().expect("the file parses");
        assert_eq!(store.all().len(), 100);
    }
}
