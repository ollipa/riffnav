//! Fixtures and hooks for the app's tests.

use crate::diff::testing::bare;
use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use crate::comment::CommentStore;
use crate::config::Config;
use crate::diff::{FileDiff, FileStatus};

impl App {
    /// Turn comments on with an in-memory store, bypassing disk and git scope
    /// detection so rendering and the cursor can be tested in isolation.
    pub(crate) fn install_comments_for_test(&mut self, comments: CommentStore) {
        self.comments = comments;
        self.comments_on = true;
        self.comment_author = "tester".to_string();
        self.bump_comments();
    }

    /// Drive one keypress through the routing the event loop uses, so tests in
    /// other modules can exercise the keys without an event queue.
    pub(crate) fn press_for_test(&mut self, code: KeyCode, ctrl: bool) {
        let mods = if ctrl {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        self.press_mods_for_test(code, mods);
    }

    /// The same, for the keys whose meaning turns on a modifier other than Ctrl.
    pub(crate) fn press_mods_for_test(&mut self, code: KeyCode, mods: KeyModifiers) {
        let _ = self.dispatch_key(crossterm::event::KeyEvent::new(code, mods));
    }

    /// Seed the render cache for the selected file, splicing in whatever the
    /// installed store holds. Stands in for the `RenderCache::ensure` the event
    /// loop would do, for tests that want exact text rather than a diff.
    pub(crate) fn seed_render_for_test(&mut self, width: u16, text: ratatui::text::Text<'static>) {
        let idx = self.selected_file().expect("a file is selected");
        let layer = Self::comment_layer(
            &self.files,
            &self.comments,
            &self.comment_author,
            0,
            self.comment_rev,
            self.comments_on,
            idx,
        );
        self.cache
            .insert_for_test_with_comments(idx, width, false, self.diff_theme, text, &layer);
        self.last_width = width;
        self.cursor_token = None; // the render is new; let the cursor re-resolve
        self.resync_cursor();
    }
}

pub(super) fn file(path: &str) -> FileDiff {
    bare(path, FileStatus::Modified)
}

pub(super) fn app_with(files: Vec<FileDiff>) -> App {
    App::new(files, false, &Config::default())
}

/// Like `file`, but with a distinct `raw` so each file hashes differently —
/// the viewed state is keyed on diff content, and the bare `file` helper
/// leaves `raw` empty (all-identical hashes).
pub(super) fn file_with_raw(path: &str) -> FileDiff {
    FileDiff {
        raw: format!("diff --git a/{path} b/{path}\n@@ -1 +1 @@\n-old\n+new\n"),
        ..file(path)
    }
}

/// An app with auto-advance off, so `toggle_viewed` exercises only the
/// mark/unmark logic without moving the selection out from under the test.
pub(super) fn app_no_advance(files: Vec<FileDiff>) -> App {
    let cfg = Config {
        review_auto_advance: false,
        ..Config::default()
    };
    App::new(files, false, &cfg)
}

/// A three-line diff with a gutter, so the line map has real numbers to anchor
/// against.
pub(super) fn seed_text() -> ratatui::text::Text<'static> {
    ratatui::text::Text::from(
        (1..=3)
            .map(|n| ratatui::text::Line::from(format!("{n:>5}⋮{n:>5}│let x{n} = {n};")))
            .collect::<Vec<_>>(),
    )
}
