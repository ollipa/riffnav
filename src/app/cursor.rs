//! The line cursor and scrolling the diff.

use super::{App, CURSOR_PADDING, Focus};
use crate::render::Key;

/// What a render looks like, as far as the line cursor's index into it goes. When
/// this changes, the index is stale.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct CursorToken {
    key: Key,
    comment_rev: u64,
    copies_rev: u64,
}

impl App {
    pub(super) fn scroll_diff(&mut self, delta: i32) {
        self.diff_scroll = self.diff_scroll.saturating_add_signed(delta);
        self.park_cursor_in_view();
    }

    /// The render backing the current view, if it's already cached. Rendering is
    /// off this thread, so `None` here can last for many frames.
    pub(super) fn current_render(&self) -> Option<&crate::render::Rendered> {
        self.render_of(self.selected_file()?)
    }

    /// File `idx`'s render as it would be shown now, if it's already cached.
    pub(super) fn render_of(&self, idx: usize) -> Option<&crate::render::Rendered> {
        self.cache.get(self.render_key(idx))
    }

    /// What file `idx`'s render looks like now, to hold a cursor's index against.
    /// Comments count once they're in the render on show.
    pub(super) fn render_token(&self, idx: usize) -> CursorToken {
        let key = self.render_key(idx);
        CursorToken {
            key,
            comment_rev: self.cache.rev(key).unwrap_or(self.comment_rev),
            copies_rev: self.cache.copies_rev(key).unwrap_or(0),
        }
    }

    /// The key file `idx`'s render is cached under as it would be shown now.
    pub(super) fn render_key(&self, idx: usize) -> Key {
        Key {
            file: idx,
            width: self.last_width,
            side_by_side: self.side_by_side_for(idx),
            theme: self.diff_theme,
        }
    }

    /// Re-resolve the cursor's line index after the render it pointed into was
    /// rebuilt (a comment spliced in above it, a theme or width change, a layout
    /// switch), going by the diff line it's anchored to.
    pub(super) fn resync_cursor(&mut self) {
        let Some(idx) = self.selected_file() else {
            return;
        };
        let token = self.render_token(idx);
        if self.cursor_token == Some(token) {
            return;
        }
        // Not noted until there's a render to go by, as that's what the cursor has
        // to be put right against.
        if self.current_render().is_none() {
            return;
        }
        self.cursor_token = Some(token);
        let Some(render) = self.current_render() else {
            return;
        };
        let lines = render.lines();
        self.diff_cursor = match self.cursor_anchor.and_then(|a| render.line_map.row_for(a)) {
            Some(row) => row,
            // Opening a file for the first time: start on the first real diff
            // line rather than the file header, so `c` works straight away.
            None if self.cursor_anchor.is_none() && self.diff_cursor == 0 => {
                render.line_map.first_code_row().unwrap_or(0)
            }
            // The anchored line left the diff: keep the index, just make sure it
            // still points inside the render.
            None => self.diff_cursor.min(lines.saturating_sub(1)),
        };
        self.remember_cursor_anchor();
    }

    /// Record the diff line the cursor now sits on, so a later re-render can put
    /// it back. A row with no line number (a hunk header, a comment row) leaves
    /// the previous anchor alone rather than clearing it.
    pub(super) fn remember_cursor_anchor(&mut self) {
        if let Some(anchor) = self
            .current_render()
            .and_then(|r| r.line_map.get(self.diff_cursor).anchor())
        {
            self.cursor_anchor = Some(anchor);
        }
    }

    pub(super) fn move_cursor(&mut self, delta: i32) {
        let Some(max) = self.current_render().map(|r| r.lines().saturating_sub(1)) else {
            return;
        };
        self.diff_cursor = self
            .diff_cursor
            .saturating_add_signed(delta as isize)
            .min(max);
        self.remember_cursor_anchor();
        self.scroll_cursor_into_view();
    }

    pub(super) fn set_cursor(&mut self, line: usize) {
        let Some(max) = self.current_render().map(|r| r.lines().saturating_sub(1)) else {
            return;
        };
        self.diff_cursor = line.min(max);
        self.remember_cursor_anchor();
        self.scroll_cursor_into_view();
    }

    /// Land the cursor on the comment block starting at `line` and bring the
    /// whole of it into view, as a note is read as a unit. A block too tall for
    /// the pane is aligned to its top, which is where reading starts.
    pub(super) fn set_cursor_on_block(&mut self, line: usize) {
        self.set_cursor(line);
        let height = u32::from(self.diff_height);
        if height == 0 {
            return;
        }
        let Some((top, bottom)) = self.current_render().and_then(|r| {
            let block = r.comment_rows.iter().find(|b| b.start == line)?;
            Some((r.row_of(block.start), r.row_of(block.start + block.len)))
        }) else {
            return;
        };
        let pad = CURSOR_PADDING.min(height / 3);
        if bottom + pad > self.diff_scroll.saturating_add(height) {
            self.diff_scroll = (bottom + pad).saturating_sub(height);
        }
        // Last, so a block taller than the pane keeps its top rather than its
        // foot: the two pulls are in opposite directions and only one can win.
        if top < self.diff_scroll.saturating_add(pad) {
            self.diff_scroll = top.saturating_sub(pad);
        }
    }

    /// Scroll the minimum needed to keep the cursor line — all of it, when it
    /// wraps onto several rows — inside the viewport, with [`CURSOR_PADDING`].
    fn scroll_cursor_into_view(&mut self) {
        let (Some(render), height) = (self.current_render(), u32::from(self.diff_height)) else {
            return;
        };
        if height == 0 {
            return;
        }
        let top = render.row_of(self.diff_cursor);
        let bottom = render.row_of(self.diff_cursor + 1).max(top + 1);
        // On a short pane, padding that exceeds a third of the height would fight
        // itself, so cap it.
        let pad = CURSOR_PADDING.min(height / 3);
        let scroll = self.diff_scroll;
        if top < scroll.saturating_add(pad) {
            self.diff_scroll = top.saturating_sub(pad);
        } else if bottom + pad > scroll.saturating_add(height) {
            self.diff_scroll = (bottom + pad).saturating_sub(height);
        }
    }

    /// After a scroll or page jump, pull the cursor to the nearest visible line
    /// rather than letting it sit off-screen.
    fn park_cursor_in_view(&mut self) {
        let (Some(render), height) = (self.current_render(), u32::from(self.diff_height)) else {
            return;
        };
        if height == 0 {
            return;
        }
        let scroll = self.diff_scroll.min(render.height.saturating_sub(height));
        let first = render.line_at(scroll);
        let last = render.line_at(scroll.saturating_add(height.saturating_sub(1)));
        let parked = self.diff_cursor.clamp(first, last.max(first));
        if parked != self.diff_cursor {
            self.diff_cursor = parked;
            self.remember_cursor_anchor();
        }
    }

    /// One PageUp/PageDown step: the diff viewport height less a line of overlap,
    /// so a line of context carries across the jump. At least one line.
    fn page(&self) -> i32 {
        i32::from(self.diff_height.saturating_sub(1)).max(1)
    }

    pub(super) fn page_move(&mut self, down: bool) {
        let delta = if down { self.page() } else { -self.page() };
        if self.focus == Focus::Tree {
            self.move_selection(delta as isize);
        } else {
            self.scroll_diff(delta);
        }
    }

    /// Put the cursor on the line a jump was after. One that was off screen lands
    /// a third of the way down, so the block below it can be read and the note
    /// above it stays in view. So does a line wrapped too tall to fit below where
    /// it was: bringing its foot into view would scroll away where it starts.
    pub(super) fn land_cursor(&mut self, line: usize) {
        let Some(top) = self.current_render().map(|r| r.row_of(line)) else {
            return;
        };
        let height = u32::from(self.diff_height);
        let on_screen = (self.diff_scroll..self.diff_scroll.saturating_add(height)).contains(&top);
        self.set_cursor(line);
        if !on_screen || top < self.diff_scroll {
            self.diff_scroll = top.saturating_sub(height / 3);
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::app::Focus;
    use crate::app::test_support::{app_with, file};

    #[test]
    fn page_keys_scroll_diff_by_a_screenful() {
        let mut app = app_with(vec![file("a.rs")]);
        app.focus = Focus::Diff;
        app.diff_height = 20;

        app.page_move(true);
        assert_eq!(app.diff_scroll, 19);

        // PageUp comes back and never scrolls above the top.
        app.page_move(false);
        assert_eq!(app.diff_scroll, 0);
    }

    #[test]
    fn page_is_at_least_one_line() {
        // Before the first render diff_height is 0; a page must still advance.
        let app = app_with(vec![file("a.rs")]);
        assert_eq!(app.page(), 1);
    }
}
