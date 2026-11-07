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
    /// it back. A row with no line number (a hunk header, a comment row) is held
    /// by the nearest line, looking up first: the line a comment is on, or the
    /// last one before a header.
    pub(super) fn remember_cursor_anchor(&mut self) {
        self.remember_cursor_anchor_toward(false);
    }

    /// Whether the cursor is on the line it's anchored to, rather than a row with
    /// no line held by one near it. A cursor a jump left on its line is, so one
    /// that has moved off it onto a header or a wrapped row near it doesn't look
    /// like it's still there.
    pub(super) fn on_anchored_line(&self) -> bool {
        let anchor = |r: &crate::render::Rendered| r.line_map.get(self.diff_cursor).anchor();
        self.current_render()
            .is_none_or(|r| anchor(r) == self.cursor_anchor)
    }

    /// The same, looking down first when `down` is set.
    fn remember_cursor_anchor_toward(&mut self, down: bool) {
        let near = self.current_render();
        if let Some(anchor) = near.and_then(|r| r.anchor_near(self.diff_cursor, down)) {
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

    /// Scroll the line cursor back into view if a new width or layout has pushed
    /// it off the pane. One still in view, even partly, stays where it is.
    pub fn keep_cursor_in_view(&mut self) {
        if !std::mem::take(&mut self.keep_cursor) {
            return;
        }
        let Some(render) = self.current_render() else {
            return;
        };
        let height = u32::from(self.diff_height);
        let top = render.row_of(self.diff_cursor);
        let scroll = self.diff_scroll.min(render.height.saturating_sub(height));
        if !(scroll..scroll.saturating_add(height)).contains(&top) {
            self.scroll_cursor_into_view();
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
            // Parked at the top, a row with no line is held by the line below it,
            // which is in view, and at the foot by the one above.
            let down = parked > self.diff_cursor;
            self.diff_cursor = parked;
            self.remember_cursor_anchor_toward(down);
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

    use crossterm::event::KeyCode::Char;

    use crate::app::test_support::{HEIGHT, app_with, file, frame_at};
    use crate::app::{App, Focus};
    use crate::comment::CommentStore;
    use crate::config::Config;
    use crate::diff::parse;
    use crate::diff::testing::hunk;
    use crate::render::test_support::Gate;

    /// `lines` lines of context in `path`, long enough to wrap differently at every
    /// width, then a change.
    fn wrapping(path: &str, lines: usize) -> App {
        let mut body: String = (1..=lines)
            .map(|i| format!(" line {i} {}\n", "word ".repeat(i * 7 % 40)))
            .collect();
        body.push_str("-old\n+new\n");
        let mut app = App::new(parse(&hunk(path, &body)), false, &Config::default());
        app.install_comments_for_test(CommentStore::disabled());
        app.focus = Focus::Diff;
        app
    }

    /// The new number of the line at the top of the view, and of the cursor's.
    fn top_and_cursor(app: &App) -> (Option<u32>, Option<u32>) {
        let render = app.current_render().unwrap();
        let top = render.code_line(render.line_at(app.diff_scroll));
        let at = |line| render.line_map.get(line).new;
        (at(top), at(app.diff_cursor))
    }

    /// Scroll so the line numbered `line` starts at the top, with the cursor on it.
    fn scroll_to_line(app: &mut App, line: usize) {
        let render = app.current_render().unwrap();
        let at = (0..render.lines())
            .find(|&at| render.line_map.get(at).new == Some(line as u32))
            .unwrap();
        app.diff_scroll = render.row_of(at);
        app.diff_cursor = at;
        app.remember_cursor_anchor();
    }

    #[test]
    fn the_top_line_stays_at_the_top_through_new_widths() {
        let mut app = wrapping("a.txt", 3000);
        frame_at(&mut app, 120, 40);
        scroll_to_line(&mut app, 1994);
        frame_at(&mut app, 120, 40);
        let kept = top_and_cursor(&app);
        assert_eq!(kept, (Some(1994), Some(1994)));
        for (width, height) in [(100, 30), (70, 40), (120, 40), (45, 20)] {
            frame_at(&mut app, width, height);
            assert_eq!(top_and_cursor(&app), kept, "at {width}x{height}");
        }
        for key in ['e', 's', 'e', 's'] {
            app.press_for_test(Char(key), false);
            frame_at(&mut app, 120, 40);
            assert_eq!(top_and_cursor(&app), kept, "after {key}");
        }
    }

    /// A line partly scrolled past keeps as much of itself scrolled past, in
    /// proportion to how many rows it wraps onto now.
    #[test]
    fn a_line_partly_scrolled_past_stays_partly_scrolled_past() {
        let mut app = wrapping("a.txt", 50);
        frame_at(&mut app, 60, 20);
        scroll_to_line(&mut app, 39); // `line 39` and 33 words: several rows at 60.
        app.diff_scroll += 2;
        frame_at(&mut app, 60, 20);
        frame_at(&mut app, 40, 20);
        let render = app.current_render().unwrap();
        let top = render.line_at(app.diff_scroll);
        assert_eq!(render.line_map.get(top).new, Some(39));
        assert!(app.diff_scroll > render.row_of(top), "still partway in");
    }

    #[test]
    fn a_cursor_a_new_width_pushes_off_the_pane_is_brought_back() {
        let mut app = wrapping("a.txt", 400);
        frame_at(&mut app, 120, 30);
        scroll_to_line(&mut app, 200);
        app.set_cursor(app.diff_cursor + 20);
        frame_at(&mut app, 120, 30);
        let cursor = top_and_cursor(&app).1;
        frame_at(&mut app, 40, 30);
        let render = app.current_render().unwrap();
        let row = render.row_of(app.diff_cursor);
        assert_eq!(top_and_cursor(&app).1, cursor);
        assert!(row >= app.diff_scroll && row < app.diff_scroll + u32::from(app.diff_height));
    }

    /// The pane can get shorter as well as narrower, and a frame with no render
    /// yet can be drawn at the new size before the view is carried over.
    #[test]
    fn a_cursor_a_shorter_pane_would_hide_stays_in_view() {
        for late in [false, true] {
            let mut app = wrapping("a.txt", 3000);
            frame_at(&mut app, 120, 40);
            scroll_to_line(&mut app, 1994);
            // Low in the pane, where a shorter one won't reach.
            let render = app.current_render().unwrap();
            let low = app.diff_scroll + 30;
            let line = (app.diff_cursor..)
                .find(|&l| render.row_of(l) >= low)
                .unwrap();
            app.set_cursor(line);
            frame_at(&mut app, 120, 40);
            let cursor = top_and_cursor(&app).1;
            if late {
                let gate = Gate::default();
                app.cache.wait(std::time::Duration::ZERO);
                app.cache.hooks().start = Some(gate.clone());
                frame_at(&mut app, 100, 20);
                assert!(app.current_render().is_none());
                gate.open();
                app.cache.finish();
            }
            frame_at(&mut app, 100, 20);
            let render = app.current_render().unwrap();
            let row = render.row_of(app.diff_cursor);
            assert_eq!(top_and_cursor(&app).1, cursor);
            let scroll = app.diff_scroll;
            let bottom = scroll + u32::from(app.diff_height);
            assert!(
                row >= scroll && row < bottom,
                "{row} {scroll}, late: {late}"
            );
        }
    }

    #[test]
    fn rows_past_a_u16_carry_over_too() {
        let mut app = wrapping("a.txt", 30_000);
        frame_at(&mut app, 60, HEIGHT);
        scroll_to_line(&mut app, 29_000);
        frame_at(&mut app, 60, HEIGHT);
        assert!(app.diff_scroll > u32::from(u16::MAX), "{}", app.diff_scroll);
        let kept = top_and_cursor(&app);
        for width in [50, 90, 60] {
            frame_at(&mut app, width, HEIGHT);
            assert_eq!(top_and_cursor(&app), kept, "at {width}");
        }
    }

    /// A render that's late is waited for, and its preview puts the view where its
    /// real render keeps it.
    #[test]
    fn a_late_render_and_its_preview_land_on_the_same_line() {
        let mut app = wrapping("a.rs", 3000);
        frame_at(&mut app, 120, 40);
        scroll_to_line(&mut app, 1994);
        frame_at(&mut app, 120, 40);
        let kept = top_and_cursor(&app);
        let (start, real) = (Gate::default(), Gate::default());
        app.cache.wait(std::time::Duration::ZERO);
        app.cache.hooks().start = Some(start.clone());
        app.cache.hooks().real = Some(real.clone());
        frame_at(&mut app, 70, 40);
        assert!(app.current_render().is_none());
        frame_at(&mut app, 80, 40); // Changed again before anything landed.
        start.open();
        while app.current_render().is_none() {
            app.cache.collect();
            frame_at(&mut app, 80, 40);
        }
        assert!(app.cache.is_preview(0, app.last_width, false));
        let scroll = app.diff_scroll;
        assert_eq!(top_and_cursor(&app), kept, "the preview");
        real.open();
        app.cache.finish();
        frame_at(&mut app, 80, 40);
        assert!(!app.cache.is_preview(0, app.last_width, false));
        assert_eq!((top_and_cursor(&app), app.diff_scroll), (kept, scroll));
    }

    /// Many short hunks, so paging often leaves a hunk header at the top.
    fn hunks() -> App {
        let mut raw = crate::diff::testing::header("h.txt");
        for at in 0..150 {
            let start = at * 13 + 1;
            raw.push_str(&format!("@@ -{start},4 +{start},4 @@\n"));
            raw.push_str(&format!(
                " a {at}\n b {at}\n-old {at}\n+new {at}\n c {at}\n"
            ));
        }
        let mut app = App::new(parse(&raw), false, &Config::default());
        app.install_comments_for_test(CommentStore::disabled());
        app.focus = Focus::Diff;
        app
    }

    /// Page until the cursor is parked on a row with no line number: a hunk
    /// header, or the rest of a wrapped side-by-side row.
    fn park_on_a_header(app: &mut App) {
        for _ in 0..200 {
            app.press_for_test(Char(' '), false);
            frame_at(app, 100, 20);
            let render = app.current_render().unwrap();
            if render.line_map.get(app.diff_cursor).anchor().is_none() {
                return;
            }
        }
        panic!("never parked on a header");
    }

    fn cursor_in_view(app: &App) -> bool {
        let row = app.current_render().unwrap().row_of(app.diff_cursor);
        (app.diff_scroll..app.diff_scroll + u32::from(app.diff_height)).contains(&row)
    }

    /// A cursor paged onto a hunk header is held by the line below it, so a theme,
    /// a width or a comment re-rendering the file leaves it in view, where it was
    /// sent back to the line it was on before paging.
    #[test]
    fn a_cursor_parked_on_a_header_stays_in_view_through_a_re_render() {
        for change in ["theme", "width", "comment"] {
            let mut app = hunks();
            frame_at(&mut app, 100, 20);
            park_on_a_header(&mut app);
            let scroll = app.diff_scroll;
            match change {
                "theme" => app.press_for_test(Char('T'), false),
                "width" => {
                    frame_at(&mut app, 90, 20);
                }
                _ => {
                    let file = app.files[0].path().to_string();
                    let note = crate::comment::testing::note(&file, 1, "far away");
                    app.comments.update(|store| store.add(note)).unwrap();
                    app.bump_comments();
                }
            }
            frame_at(&mut app, if change == "width" { 90 } else { 100 }, 20);
            let render = app.current_render().unwrap();
            assert!(
                render.line_map.get(app.diff_cursor).anchor().is_some(),
                "{change}"
            );
            assert!(cursor_in_view(&app), "{change}");
            if change != "comment" {
                assert_eq!(app.diff_scroll, scroll, "{change}: the view stays put");
            }
        }
    }

    /// Side by side, paging can park the cursor on the rest of a wrapped row whose
    /// start is above the view. It's held by the line below, so a re-render
    /// neither moves the view nor loses the cursor.
    #[test]
    fn a_cursor_parked_on_a_wrapped_row_stays_put_side_by_side() {
        let mut app = wrapping("a.txt", 400);
        app.side_by_side = true;
        frame_at(&mut app, 100, 20);
        park_on_a_header(&mut app);
        let scroll = app.diff_scroll;
        app.press_for_test(Char('T'), false);
        frame_at(&mut app, 100, 20);
        assert_eq!(app.diff_scroll, scroll);
        assert!(cursor_in_view(&app));
        let render = app.current_render().unwrap();
        assert!(render.line_map.get(app.diff_cursor).anchor().is_some());
    }

    /// `g` puts the cursor on the file's header, and a re-render puts it on the
    /// first line rather than wherever it was before.
    #[test]
    fn g_then_a_re_render_leaves_the_cursor_at_the_top() {
        let mut app = hunks();
        frame_at(&mut app, 100, 20);
        for _ in 0..10 {
            app.press_for_test(Char(' '), false);
        }
        app.press_for_test(Char('j'), false);
        app.press_for_test(Char('g'), false);
        frame_at(&mut app, 100, 20);
        app.press_for_test(Char('T'), false);
        frame_at(&mut app, 100, 20);
        assert_eq!(app.diff_scroll, 0);
        assert!(cursor_in_view(&app));
    }

    /// A reload keeps the row it was on, as before, rather than following line
    /// numbers that may now be other code.
    #[test]
    fn a_reload_keeps_the_scroll_it_had() {
        let mut app = wrapping("a.txt", 400);
        frame_at(&mut app, 120, 30);
        scroll_to_line(&mut app, 200);
        frame_at(&mut app, 120, 30);
        let scroll = app.diff_scroll;
        let files = app.files.clone();
        app.reload_in_place(files);
        frame_at(&mut app, 120, 30);
        assert_eq!(app.diff_scroll, scroll);
    }

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
