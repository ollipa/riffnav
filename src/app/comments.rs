//! The comment store, and the `]` and `[` keys.

use super::prefetch::By;
use super::{App, Direction};
use crate::comment::content::{BlobReader, FileBlobs};
use crate::comment::reanchor::Current;
use crate::comment::{CommentStore, CommentWatch};
use crate::diff::FileDiff;
use crate::render::CommentLayer;
use crate::session::Session;

impl App {
    /// Re-read the comment file, discarding what was in memory (every local write
    /// has already been saved). Reports how many notes appeared, or `None` when
    /// none did — a reload that brings nothing new isn't worth a status line.
    pub(super) fn reload_comments(&mut self) -> Option<usize> {
        let before = self.comments.all().len();
        self.comments.reload();
        self.bump_comments();
        let new = self.comments.all().len().checked_sub(before)?;
        (new > 0).then_some(new)
    }

    /// Load inline comments for the current repo+branch and start watching the
    /// file for notes written elsewhere. Outside a git repo the store stays
    /// session-only and there's nothing to watch, but the keys still work.
    pub fn enable_comments(&mut self, show: bool, retention_days: u64, author: Option<&str>) {
        self.comments_on = show;
        if !show {
            return;
        }
        self.comment_author = author
            .map(str::to_string)
            .or_else(|| std::env::var("USER").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "me".to_string());
        self.comments = CommentStore::load(retention_days);
        self.comment_watch = self.comments.path().and_then(CommentWatch::new);
        self.publish_session();
        self.follow_code(true);
        self.bump_comments();
    }

    /// What `files` hold now, to move comments with their code and stamp new
    /// ones (see [`crate::comment::reanchor`]). Not a method, so the store can
    /// be written while it's borrowed.
    pub(super) fn current_code<'a>(
        files: &[FileDiff],
        blobs: &'a mut Option<BlobReader>,
    ) -> Current<'a> {
        let files = files.iter().map(FileBlobs::of);
        Current::new(files, blobs.get_or_insert_with(BlobReader::here))
    }

    /// Whether comments follow their code in the diff on screen: only in one of
    /// riffnav's own views of the branch, never a commit, a pipe, or a git
    /// command of the user's own, which show versions they don't follow.
    fn follows_code(&self) -> bool {
        self.git
            .as_ref()
            .is_some_and(|g| g.plain_source().is_some())
    }

    /// Move the comments whose code moved since they were written or last moved.
    /// `save` writes the store when one did; otherwise they're only placed here,
    /// as for notes another process just wrote: saving those could undo what a
    /// window on another view of the branch saved, and that one would answer.
    pub(super) fn follow_code(&mut self, save: bool) {
        if !self.comments_on || !self.follows_code() {
            return;
        }
        let current = Self::current_code(&self.files, &mut self.blobs);
        current.prefetch(&self.comments);
        let moved = match current.needs(&self.comments) {
            true if save => self.comments.update(|store| current.apply(store)),
            true => Ok(current.apply(&mut self.comments)),
            false => Ok(0),
        };
        match moved {
            Ok(0) => {}
            Ok(_) => self.bump_comments(),
            Err(e) => self.set_status(format!("Couldn't move comments with their code: {e}")),
        }
    }

    /// Publish what this window is showing, so `riffnav comment add` in another
    /// terminal can validate `--file`/`--line` against the diff on screen.
    /// Best-effort; without a session the CLI re-derives the diff from git.
    pub(super) fn publish_session(&self) {
        if !self.comments_on {
            return;
        }
        let source = self.diff_label().unwrap_or_else(|| "stdin".to_string());
        let base = self.git.as_ref().and_then(|g| g.base.clone());
        let mut session = Session::new(&self.files, &source, base);
        session.follows = self.follows_code();
        session.save();
    }

    pub fn comments_enabled(&self) -> bool {
        self.comments_on
    }

    /// Comments anchored in the file at `diff_index`, for the tree's badge.
    pub fn comment_count(&self, diff_index: usize) -> usize {
        if !self.comments_on {
            return 0;
        }
        self.files
            .get(diff_index)
            .map_or(0, |f| self.comments.count_for_file(f.path()))
    }

    pub fn comment_total(&self) -> usize {
        if !self.comments_on {
            return 0;
        }
        (0..self.files.len()).map(|i| self.comment_count(i)).sum()
    }

    /// Note that the comment set changed, so cached renders re-splice.
    pub(super) fn bump_comments(&mut self) {
        self.comment_rev = self.comment_rev.wrapping_add(1);
    }

    /// The comment layer for file `idx`, borrowed for one `RenderCache::ensure`.
    /// Not a method: `&self` would keep `cache` borrowed, which `ensure` needs
    /// mutably.
    pub(super) fn comment_layer<'a>(
        files: &'a [FileDiff],
        comments: &'a CommentStore,
        me: &'a str,
        diff_hash: u128,
        rev: u64,
        enabled: bool,
        idx: usize,
    ) -> CommentLayer<'a> {
        if !enabled {
            return CommentLayer::none();
        }
        let file = files[idx].path();
        CommentLayer {
            threads: comments.threads(file),
            file,
            me,
            diff_hash,
            rev,
        }
    }

    /// Reload the store when the watcher reports the file changed, so a note an
    /// agent just wrote appears here. Also fires after our own save, where the
    /// reload is a harmless no-op beyond one re-splice.
    pub(super) fn poll_comments(&mut self) {
        if !self
            .comment_watch
            .as_ref()
            .is_some_and(CommentWatch::changed)
        {
            return;
        }
        let new = self.reload_comments();
        self.follow_code(false);
        if let Some(new) = new {
            self.set_status(format!("💬 {new} new comment(s)"));
        }
    }

    /// The spliced comment block the cursor is inside, if any.
    pub(super) fn block_at_cursor(&self) -> Option<&crate::comment::CommentBlock> {
        let cursor = self.diff_cursor;
        self.current_render()?
            .comment_rows
            .iter()
            .find(|b| cursor >= b.start && cursor < b.start + b.len)
    }

    /// Jump to the next or previous comment, crossing into the next commented file
    /// and wrapping around the whole diff, so `]` held down visits every note in
    /// the review exactly once per lap.
    pub(crate) fn jump_comment(&mut self, dir: Direction) {
        self.head(dir, By::Comment);
        let starts: Vec<usize> = self
            .current_render()
            .map(|r| r.comment_rows.iter().map(|b| b.start).collect())
            .unwrap_or_default();
        let cursor = self.diff_cursor;
        let here = if dir == Direction::Forward {
            starts.iter().find(|&&s| s > cursor).copied()
        } else {
            starts.iter().rev().find(|&&s| s < cursor).copied()
        };
        if let Some(target) = here {
            self.set_cursor_on_block(target);
            return;
        }
        // Off the end of this file: carry on into the next file that has notes.
        if self.jump_commented_file(dir) {
            return;
        }
        // Nowhere else to go — this file holds every comment there is, so wrap
        // inside it, or say there are none at all.
        let wrapped = if dir == Direction::Forward {
            starts.first()
        } else {
            starts.last()
        };
        match wrapped {
            Some(&target) => self.set_cursor_on_block(target),
            None => self.set_status("No comments in this diff"),
        }
    }

    /// Select the next/previous *other* file carrying comments, wrapping around,
    /// and arm the cursor to land on the comment nearest the edge we entered from.
    /// The landing is deferred because the file isn't rendered until the event
    /// loop's next pass.
    fn jump_commented_file(&mut self, dir: Direction) -> bool {
        let order = self.tree_order();
        let n = order.len();
        if n == 0 {
            return false;
        }
        let here = self.selected_file();
        let from = here
            .and_then(|idx| order.iter().position(|&d| d == idx))
            // A folder row is selected, so there's no file to start from: sweep
            // the whole list from its far end instead.
            .unwrap_or(if dir == Direction::Forward { n - 1 } else { 0 });
        // Every file but the one we're on, in the direction of travel.
        let target = (1..=n)
            .map(|off| {
                order[(if dir == Direction::Forward {
                    from + off
                } else {
                    from + n - off
                }) % n]
            })
            .find(|&idx| Some(idx) != here && self.comment_count(idx) > 0);
        let Some(idx) = target else {
            return false;
        };
        self.reveal_file(idx);
        self.comment_landing = Some((idx, dir));
        true
    }

    /// Finish a `]`/`[` jump that crossed into another file, once the event loop
    /// has rendered it: the cursor goes on that file's first comment when we came
    /// in going forward, its last when going back.
    pub(super) fn land_on_comment(&mut self) {
        let Some((file, dir)) = self.comment_landing else {
            return;
        };
        if self.selected_file() != Some(file) {
            self.comment_landing = None; // The view has moved on without it.
            return;
        }
        let Some(render) = self.current_render() else {
            return;
        };
        let block = if dir == Direction::Forward {
            render.comment_rows.first()
        } else {
            render.comment_rows.last()
        };
        let start = block.map(|b| b.start);
        self.comment_landing = None;
        if let Some(start) = start {
            self.set_cursor_on_block(start);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::Direction;
    use crate::app::test_support::{app_with, file, file_with_raw, seed_text};
    use crate::app::{App, Focus};
    use crate::comment::testing::note;
    use crate::comment::{Comment, CommentStore};
    use crate::testing::TempDir;
    use crate::tree;

    /// `r` re-reads the notes as well as the diff. The filesystem watcher usually
    /// gets there first, but it isn't always there to — this is what makes a
    /// stale-looking screen fixable by hand.
    #[test]
    fn refreshing_re_reads_notes_written_by_another_process() {
        let dir = TempDir::new("refresh");
        let path = dir.join("scope.json");

        let mut app = app_with(vec![file_with_raw("a.rs")]);
        app.install_comments_for_test(CommentStore::with_path(path.clone()));
        assert!(app.comments.all().is_empty());

        // An agent in another terminal writes to the same scope file.
        let mut theirs = CommentStore::with_path(path.clone());
        theirs
            .update(|s| {
                s.add(Comment {
                    author: "claude".to_string(),
                    created: crate::state::now_unix(),
                    ..note("a.rs", 2, "written elsewhere")
                })
            })
            .unwrap();

        let rev = app.comment_rev;
        assert_eq!(app.reload_comments(), Some(1), "the note is picked up");
        assert_eq!(app.comments.all()[0].body, "written elsewhere");
        assert_ne!(app.comment_rev, rev, "and the renders re-splice");
        assert_eq!(app.reload_comments(), None, "a second pass adds nothing");
    }

    /// Two files, one note each, both rendered — the state `]` crosses a file
    /// boundary from. The event loop renders only what's selected, so each file
    /// is seeded while it is.
    fn app_with_a_note_in_two_files() -> App {
        let mut app = app_with(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        let mut store = CommentStore::disabled();
        for (path, line) in [("a.rs", 1u32), ("b.rs", 2)] {
            store.add(Comment {
                created: crate::state::now_unix(),
                ..note(path, line, &format!("a note in {path}"))
            });
        }
        app.install_comments_for_test(store);
        app.focus = Focus::Diff;
        app.diff_height = 20;

        app.select(1); // b.rs, so its render exists when the jump arrives
        app.seed_render_for_test(60, seed_text());
        app.select(0);
        app.seed_render_for_test(60, seed_text());
        app.resync_cursor();
        app
    }

    /// One pass through the event loop, with the render already seeded.
    fn settle(app: &mut App) {
        app.resync_cursor();
        app.land_on_comment();
    }

    /// `]` past the last note in a file carries on into the next file that has
    /// one, so a review is walked end to end rather than circling one file.
    #[test]
    fn jumping_walks_off_the_end_of_a_file_into_the_next_commented_one() {
        let mut app = app_with_a_note_in_two_files();

        app.jump_comment(Direction::Forward);
        assert_eq!(app.selected_file(), Some(0), "the note in a.rs comes first");
        assert_eq!(app.diff_cursor, 1, "the block under diff line 1");

        app.jump_comment(Direction::Forward);
        settle(&mut app);
        assert_eq!(app.selected_file(), Some(1), "`]` crossed into b.rs");
        assert_eq!(app.diff_cursor, 2, "onto its note, not the top of the file");

        // Past the last note in the last file, the lap starts over.
        app.jump_comment(Direction::Forward);
        settle(&mut app);
        assert_eq!(app.selected_file(), Some(0));
        assert_eq!(app.diff_cursor, 1);

        // And `[` off the top of a file lands on the *last* note in the one before
        // it, which here is b.rs again.
        app.jump_comment(Direction::Back);
        settle(&mut app);
        assert_eq!(app.selected_file(), Some(1));
        assert_eq!(app.diff_cursor, 2);
    }

    /// A folded folder hides its files from the tree rows, but the notes inside
    /// it are still part of the review, so the jump opens the folder rather than
    /// stepping over them.
    #[test]
    fn jumping_reaches_a_note_inside_a_collapsed_folder() {
        let mut app = app_with(vec![file_with_raw("a.rs"), file_with_raw("src/b.rs")]);
        let mut store = CommentStore::disabled();
        store.add(Comment {
            created: crate::state::now_unix(),
            ..note("src/b.rs", 2, "behind a fold")
        });
        app.install_comments_for_test(store);
        app.collapsed.insert("src".to_string());
        app.rows = tree::flatten(&app.nodes, &app.collapsed);
        assert_eq!(app.selected_file(), Some(0), "a.rs is open, src is folded");

        app.jump_comment(Direction::Forward);
        assert_eq!(app.selected_file(), Some(1), "the fold was opened");
        assert!(app.comment_landing.is_some(), "and the landing is armed");
    }

    /// With every note in the file that's open there's nowhere to cross to, so
    /// the jump wraps inside it.
    #[test]
    fn jumping_wraps_inside_the_file_when_no_other_file_has_notes() {
        let mut app = app_with_a_note_in_two_files();
        app.comments.remove_where(|s, c| s.file_of(c) == "b.rs");
        app.bump_comments();
        app.seed_render_for_test(60, seed_text());
        app.resync_cursor();

        app.jump_comment(Direction::Forward);
        assert_eq!(app.diff_cursor, 1);
        app.jump_comment(Direction::Forward);
        assert_eq!(app.selected_file(), Some(0), "stayed in a.rs");
        assert_eq!(app.diff_cursor, 1, "wrapped back onto the only note");
    }

    /// Comments off means no line cursor, so j/k scroll the pane.
    #[test]
    fn without_comments_the_diff_keys_still_scroll() {
        let mut app = app_with(vec![file("a.rs")]);
        app.focus = Focus::Diff;
        assert!(!app.comments_enabled());
        // No render is cached, so a cursor move would be a no-op; a scroll isn't.
        app.scroll_diff(5);
        assert_eq!(app.diff_scroll, 5);
        app.scroll_diff(-2);
        assert_eq!(app.diff_scroll, 3);
    }
}
