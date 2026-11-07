//! Which diff is shown, and reloading it.

use crate::autodiff::{DiffSource, GitDiff};
use crate::diff::FileDiff;
use crate::tree::{self, RowKind};

use super::{App, CONTEXT_STEP};

impl App {
    /// Record the git command behind the diff (`riffnav diff|show`), so the
    /// header can label the view and the refresh key can run it again. The diff
    /// text itself was already loaded and parsed into `files`.
    pub fn enable_git_diff(&mut self, git: GitDiff) {
        self.git = Some(git);
        self.cache.read_starts();
    }

    /// The view's label (e.g. "all uncommitted", or `git diff HEAD~3`), or `None`
    /// when the diff came from stdin.
    pub fn diff_label(&self) -> Option<String> {
        self.git.as_ref().map(GitDiff::label)
    }

    /// Whether the `d` view-cycle applies: only to a plain built-in view, since
    /// there is nothing to cycle a user's own `git diff` arguments through.
    pub fn can_cycle_source(&self) -> bool {
        self.git.as_ref().and_then(GitDiff::plain_source).is_some()
    }

    /// Cycle to the next diff view (the `d` key). Views needing a base branch are
    /// skipped when none was detected.
    pub(super) fn cycle_diff_source(&mut self) {
        let Some(git) = &self.git else { return };
        let Some(source) = git.plain_source() else {
            return;
        };
        self.set_diff_source(source.next(git.base.is_some()));
    }

    /// Jump straight to the view at 1-based position `n` in the cycle (the number
    /// keys). Out-of-range digits are inert; a view that needs a base branch when
    /// none was detected says so rather than silently leaving the screen as it
    /// was, since nothing else would explain the keypress doing nothing.
    pub(super) fn select_diff_source(&mut self, n: usize) {
        let Some(git) = &self.git else { return };
        let Some(source) = DiffSource::nth(n) else {
            return;
        };
        if !source.available(git.base.is_some()) {
            self.set_status(format!("◆ {} · no base branch detected", source.label()));
            return;
        }
        if git.plain_source() == Some(source) {
            self.set_status(format!("◆ {} · already showing", source.label()));
            return;
        }
        self.set_diff_source(source);
    }

    /// Switch to `next`: re-run the matching git command and reload the file set.
    /// A source that yields nothing reloads to an empty set with an explanatory
    /// status, rather than leaving the previous view's files on screen under the
    /// new view's name.
    fn set_diff_source(&mut self, next: DiffSource) {
        let Some(git) = &self.git else { return };
        let mut candidate = GitDiff::diff(next, git.base.clone(), vec![]);
        candidate.context = git.context;
        // The immutable borrow of `self.git` ends here (candidate is owned),
        // freeing `self` for the mutable reload below.
        match candidate.load() {
            Ok(text) => {
                let files = crate::diff::parse(&text);
                // Before the reload, not after: `reload_files` publishes the
                // session, and it reads the view's name off `self.git`. Swapped,
                // the CLI would be told the new file set is the old view's, and
                // nothing would publish again to correct it.
                self.git = Some(candidate);
                self.reload_files(files);
                let summary = if self.files.is_empty() {
                    format!("◆ {} · no changes", next.label())
                } else {
                    format!("◆ {} · {} files", next.label(), self.files.len())
                };
                self.set_status(summary);
            }
            // `{e:#}` includes git's own message (e.g. a bad base ref).
            Err(e) => self.set_status(format!("diff source: {e:#}")),
        }
    }

    /// Whether there's a git command that can be run again, which is what makes
    /// the `r` refresh key worth binding. A diff piped in on stdin can only be
    /// read once.
    pub fn can_refresh(&self) -> bool {
        self.git.is_some()
    }

    /// The `r` key: re-run the diff and reload it, so work done since launch
    /// shows up without restarting. Notes written elsewhere are re-read with it:
    /// they're part of "what has changed since", and the filesystem watcher that
    /// normally brings them in isn't always there — no watch can be established
    /// on some filesystems, and none exists at all outside a repo scope. `r` is
    /// the key you press when the screen looks stale, so it re-reads everything
    /// rather than leaving one half to a watcher that may be missing.
    pub(super) fn refresh_diff(&mut self) {
        let Some(git) = &self.git else { return };
        // The immutable borrow of `self.git` ends here (the text is owned),
        // freeing `self` for the mutable reload below.
        let loaded = git.load();
        match loaded {
            Ok(text) => self.reload_in_place(crate::diff::parse(&text)),
            // `{e:#}` includes the command's own message.
            Err(e) => {
                self.set_status(format!("refresh: {e:#}"));
                return;
            }
        }
        if let Some(new) = self.reload_comments() {
            self.set_status(format!("💬 {new} new comment(s)"));
        }
    }

    /// The `+`/`-` keys: show more or fewer lines of context around each change by
    /// re-running the diff with `-U<n>`. Never narrower than git's own default,
    /// which is what the diff opened with.
    pub(super) fn change_context(&mut self, wider: bool) {
        let Some(git) = &self.git else { return };
        let floor = crate::autodiff::default_context();
        let current = git.context.unwrap_or(floor);
        let next = if wider {
            current.saturating_add(CONTEXT_STEP)
        } else {
            current.saturating_sub(CONTEXT_STEP).max(floor)
        };
        if next == current {
            self.set_status(format!("context: {current} lines (minimum)"));
            return;
        }
        let mut candidate = git.clone();
        candidate.context = (next > floor).then_some(next);
        match candidate.load() {
            Ok(text) => {
                // Before the reload, for the same reason as in `set_diff_source`:
                // the session it publishes reads the command off `self.git`.
                self.git = Some(candidate);
                self.hold_cursor_on_screen();
                self.reload_in_place(crate::diff::parse(&text));
                self.set_status(format!("context: {next} lines"));
            }
            Err(e) => self.set_status(format!("context: {e:#}")),
        }
    }

    /// Lines of context the diff was widened to, or `None` at git's default —
    /// for the header, which only mentions it when it's been changed.
    pub fn diff_context(&self) -> Option<u32> {
        self.git.as_ref().and_then(|g| g.context)
    }

    /// Remember the cursor's screen row, for [`App::restore_cursor_screen_row`]
    /// to return it to once the reloaded file has rendered. Only with comments
    /// on: without them there's no cursor, and the scroll is kept as it was.
    pub(super) fn hold_cursor_on_screen(&mut self) {
        if !self.comments_on {
            return;
        }
        if let Some(render) = self.current_render() {
            let row = render.row_of(self.diff_cursor);
            self.cursor_screen_row = Some(row.saturating_sub(self.diff_scroll));
        }
    }

    /// Scroll so the cursor lands back on the screen row it held before the
    /// reload. Waits, like the cursor itself, until the new render exists.
    pub(super) fn restore_cursor_screen_row(&mut self) {
        let Some(offset) = self.cursor_screen_row else {
            return;
        };
        let Some(render) = self.current_render() else {
            return;
        };
        self.diff_scroll = render.row_of(self.diff_cursor).saturating_sub(offset);
        self.cursor_screen_row = None;
    }

    /// Swap in a freshly loaded file set the way a refresh wants it: as
    /// [`App::reload_files`], but holding the scroll and the line cursor when the
    /// same file is still selected afterwards. Reloading rebuilds every render,
    /// yet the file on screen is usually the one you were just reading — only
    /// longer or shorter — and being thrown back to its top on every refresh is
    /// what would make `r` unusable mid-review.
    pub(super) fn reload_in_place(&mut self, files: Vec<FileDiff>) {
        let (scroll, anchor) = (self.diff_scroll, self.cursor_anchor);
        let before = self
            .selected_file()
            .map(|i| self.files[i].path().to_string());
        self.reload_files(files);
        let after = self
            .selected_file()
            .map(|i| self.files[i].path().to_string());
        if before.is_some() && before == after {
            self.diff_scroll = scroll; // the draw clamps it if the file shrank
            self.cursor_anchor = anchor;
            self.cursor_token = None; // re-resolve the cursor from that anchor
        }
    }

    /// Swap in a freshly parsed file set, rebuilding the tree while preserving
    /// the selected file by path where it still exists.
    pub(super) fn reload_files(&mut self, files: Vec<FileDiff>) {
        let prev_path = self
            .selected_file()
            .map(|i| self.files[i].path().to_string());
        let resume = self.search_resume();

        self.files = files;
        self.file_hashes = self
            .files
            .iter()
            .map(|f| crate::review::file_hash(&f.raw))
            .collect();
        self.nodes = tree::build(&self.files);
        self.rows = tree::flatten(&self.nodes, &self.collapsed);
        self.cache.clear();
        self.cache.index_moves(&self.files);
        self.last_width = 0; // force a re-render at the next draw
        self.finder = None; // indices changed; a stale finder would mislead
        self.restart_search(resume);
        // So did the file each of these is heading for.
        self.abandon_change_landing();
        self.comment_landing = None;
        self.move_landing = None;
        self.copy_return = None;
        self.move_turns.clear();
        self.prefetch_asked = None;

        let target = prev_path
            .as_deref()
            .and_then(|p| self.files.iter().position(|f| f.path() == p))
            .and_then(|di| {
                self.rows.iter().position(
                    |r| matches!(r.kind, RowKind::File { diff_index } if diff_index == di),
                )
            })
            .or_else(|| {
                self.rows
                    .iter()
                    .position(|r| matches!(r.kind, RowKind::File { .. }))
            });
        self.tree_state.select(Some(target.unwrap_or(0)));
        self.diff_scroll = 0;
        self.reset_cursor();
        self.publish_session(); // the file set changed; keep the CLI's view current
        self.status = Some(format!("↻ refreshed · {} files", self.files.len()));
    }

    /// After a file is opened in `$EDITOR` (the `o` key), re-run git for just
    /// that file and splice the fresh diff back in, so edits made while it was
    /// open show on return. A piped diff has no git source to re-read, so it's a
    /// no-op there; a command carrying the user's own arguments can't be scoped
    /// to one path unambiguously, so that falls back to a full refresh. A file
    /// whose changes were fully reverted drops out of the tree.
    pub(super) fn refresh_file(&mut self, path: &str) {
        let git = match &self.git {
            Some(git) if git.plain_source().is_some() => git,
            Some(_) => return self.refresh_diff(),
            None => return,
        };
        // The immutable borrow of `self.git` ends here (the text is owned),
        // freeing `self` for the mutable splice below.
        let text = match git.load_file(path) {
            Ok(text) => text,
            // Keep the stale diff rather than blanking it on a transient error.
            Err(e) => return self.set_status(format!("refresh {path}: {e:#}")),
        };
        let fresh = crate::diff::parse(&text)
            .into_iter()
            .find(|f| f.path() == path);
        match (self.files.iter().position(|f| f.path() == path), fresh) {
            // Still differs: swap the diff in place. The tree is unchanged (same
            // path → same index), so only this file's render and hash refresh —
            // and the scroll position is left alone (the draw clamps it).
            (Some(i), Some(file)) => self.swap_file_diff(i, file),
            // No longer differs (changes reverted): drop it from the tree.
            // Removal shifts later indices, so reload the remaining set wholesale.
            (Some(i), None) => {
                let mut files = self.files.clone();
                files.remove(i);
                self.reload_files(files);
            }
            // The path is gone from the set; nothing sensible to splice.
            (None, _) => {}
        }
    }

    /// Replace file `i`'s diff with a freshly read one, dropping everything
    /// derived from the old text: its hash, its cached render, and the line
    /// cursor's index into that render.
    ///
    /// Clearing `cursor_token` is what makes the cursor recover. `last_width = 0`
    /// forces the re-render, but the next frame sets it straight back to the same
    /// width — so the token [`App::resync_cursor`] compares would be unchanged and
    /// the cursor would keep an index into a render that no longer exists, leaving
    /// it on a different line (or past the end) of the new diff.
    fn swap_file_diff(&mut self, i: usize, file: FileDiff) {
        let resume = self.search_resume();
        self.file_hashes[i] = crate::review::file_hash(&file.raw);
        self.files[i] = file;
        self.cache.invalidate(i);
        self.cache.index_moves(&self.files);
        self.prefetch_asked = None;
        self.restart_search(resume);
        self.last_width = 0; // force a re-render at the next draw
        self.cursor_token = None; // and let the cursor re-resolve from its anchor
        self.publish_session();
    }
}

#[cfg(test)]
mod tests {

    use crossterm::event::KeyCode;

    use crate::app::test_support::{app_with, file, file_with_raw, seed_text};
    use crate::app::{App, Focus};
    use crate::autodiff::{DiffSource, GitDiff};
    use crate::tree::RowKind;

    #[test]
    fn minus_stops_at_gits_default_context() {
        // At the default already, `-` says so instead of re-running anything.
        let mut app = app_with(vec![file("a.rs")]);
        app.enable_git_diff(GitDiff::diff(DiffSource::Unstaged, None, vec![]));

        app.press_for_test(KeyCode::Char('-'), false);
        let floor = crate::autodiff::default_context();
        assert_eq!(
            app.status.as_deref(),
            Some(format!("context: {floor} lines (minimum)").as_str())
        );
        assert_eq!(app.diff_context(), None);
    }

    #[test]
    fn context_keys_are_inert_for_a_piped_diff() {
        let mut app = app_with(vec![file("a.rs")]);
        app.press_for_test(KeyCode::Char('+'), false);
        app.press_for_test(KeyCode::Char('-'), false);
        assert_eq!(app.status, None);
    }

    #[test]
    fn a_number_key_for_an_unreachable_view_says_why() {
        // Without a detected base the base-relative views can't be produced, so
        // their digits report that instead of appearing to do nothing. Both of
        // these stop before any git command runs — the test shells out to
        // nothing, which is also why it can assert on the status at all.
        let mut app = app_with(vec![file("a.rs")]);
        app.enable_git_diff(GitDiff::diff(DiffSource::Unstaged, None, vec![]));

        app.press_for_test(KeyCode::Char('1'), false);
        assert_eq!(
            app.status.as_deref(),
            Some("◆ all vs base · no base branch detected")
        );

        app.status = None;
        app.press_for_test(KeyCode::Char('4'), false);
        assert_eq!(
            app.status.as_deref(),
            Some("◆ unstaged · already showing"),
            "the view already on screen is reloaded by `r`, not by its own digit"
        );

        // Past the end of the cycle, and `0`: inert, leaving the status alone.
        app.status = None;
        app.press_for_test(KeyCode::Char('9'), false);
        app.press_for_test(KeyCode::Char('0'), false);
        assert_eq!(app.status, None);
    }

    #[test]
    fn reload_keeps_selection_by_path() {
        let mut app = app_with(vec![file("a.rs"), file("b.rs"), file("c.rs")]);
        // Select c.rs, then reload with the order shuffled and a file added.
        let c_row = app
            .rows
            .iter()
            .position(|r| matches!(r.kind, RowKind::File { diff_index } if app.files[diff_index].path() == "c.rs"))
            .unwrap();
        app.tree_state.select(Some(c_row));
        app.reload_files(vec![file("z.rs"), file("c.rs"), file("a.rs")]);
        assert_eq!(
            app.selected_file().map(|i| app.files[i].path()),
            Some("c.rs")
        );
    }

    /// `r` is for re-reading the diff mid-review, so it must not throw the reader
    /// back to the top of the file they were in the middle of.
    #[test]
    fn refreshing_holds_the_scroll_when_the_same_file_is_still_selected() {
        let mut app = app_with(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        app.diff_scroll = 12;

        // Same file still selected after the reload: the place is kept.
        app.reload_in_place(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        assert_eq!(
            app.selected_file().map(|i| app.files[i].path()),
            Some("a.rs")
        );
        assert_eq!(app.diff_scroll, 12);

        // The file dropped out of the diff, so the selection moved — a kept
        // scroll would now point into unrelated code.
        app.reload_in_place(vec![file_with_raw("z.rs")]);
        assert_eq!(app.diff_scroll, 0);
    }

    #[test]
    fn reload_falls_back_to_first_file_when_selection_gone() {
        let mut app = app_with(vec![file("a.rs"), file("b.rs")]);
        app.reload_files(vec![file("x.rs"), file("y.rs")]);
        assert_eq!(
            app.selected_file().map(|i| app.files[i].path()),
            Some("x.rs")
        );
    }

    /// Re-reading one file's diff (the `o` key, via `refresh_file`) rebuilds its
    /// render at the same width and theme, so nothing in `resync_cursor`'s token
    /// changes on its own — only dropping the token keeps the cursor off a row
    /// index that means nothing in the new render. Left stale, `c` would anchor
    /// the next note to whatever line happens to sit at that row.
    #[test]
    fn a_re_read_file_puts_the_cursor_back_on_its_diff_line() {
        // Comments stay off: this is the line cursor's own bookkeeping, and it
        // keeps `publish_session` from touching the real state directory.
        let mut app = app_with(vec![file_with_raw("a.rs")]);
        app.focus = Focus::Diff;
        app.diff_height = 20;
        app.seed_render_for_test(60, seed_text());
        app.resync_cursor();

        app.move_cursor(2);
        assert_eq!(app.cursor_anchor.map(|a| a.line), Some(3));

        app.swap_file_diff(0, file_with_raw("a.rs"));
        assert!(app.cursor_token.is_none(), "the token is dropped");

        // The re-read diff carries two header rows ahead of the
        // code, so diff line 3 now sits two rows further down. Seeded the way the
        // event loop would, leaving the cursor state alone.
        let text = ratatui::text::Text::from(
            ["a.rs".to_string(), "────".to_string()]
                .into_iter()
                .chain((1..=3).map(|n| format!("{n:>5}⋮{n:>5}│let x{n} = {n};")))
                .map(ratatui::text::Line::from)
                .collect::<Vec<_>>(),
        );
        let layer = App::comment_layer(
            &app.files,
            &app.comments,
            &app.comment_author,
            0,
            app.comment_rev,
            app.comments_on,
            0,
        );
        app.cache
            .insert_for_test_with_comments(0, 60, false, app.diff_theme, text, &layer);
        app.last_width = 60;

        app.resync_cursor();
        assert_eq!(app.diff_cursor, 4, "the cursor followed diff line 3 down");
        assert_eq!(app.cursor_anchor.map(|a| a.line), Some(3));
    }
}
