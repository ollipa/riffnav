mod changes;
mod comments;
mod composer;
mod cursor;
mod editor;
mod files;
mod keys;
mod mouse;
mod moves;
mod prefetch;
mod search;
mod source;
mod terminal;
#[cfg(test)]
mod test_support;
mod viewed;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, KeyEvent};
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use serde::Deserialize;

use crate::autodiff::GitDiff;
use crate::comment::{Anchor, CommentStore, CommentWatch, Composer, PendingComment, Side};
use crate::config::Config;
use crate::diff::FileDiff;
use crate::forge::{Forge, ReviewSync};
use crate::herdr::Herdr;
use crate::icons::IconStyle;
use crate::render::{Key, RenderCache, Rendered, Request, ViewTop, WAIT};
use crate::review::ReviewStore;
use crate::session::Session;
use crate::theme::DiffTheme;
use crate::tree::{self, Node, Row, RowKind};

use self::cursor::CursorToken;
use self::files::Finder;
use self::moves::{CopyReturn, MoveLanding};
use self::prefetch::{By, Heading, Prefetched};
use self::terminal::{
    disable_enhanced_keys, disable_mouse, enable_enhanced_keys, enable_mouse, init_terminal,
    report_background_panics,
};

const MIN_DIFF_WIDTH: u16 = 20;
const HALF_PAGE: i32 = 15;
/// How long a transient status message stays on screen before clearing itself.
const STATUS_TTL: Duration = Duration::from_secs(3);
/// While a GitHub "viewed" sync is in flight, cap the interactive input wait so
/// its result is drained and surfaced promptly, without waiting on a keypress.
const SYNC_POLL: Duration = Duration::from_millis(200);
/// How long quitting waits for queued GitHub syncs to finish before giving up,
/// so a just-marked file still reaches the PR without a stuck `gh` hanging exit.
const SYNC_FLUSH_GRACE: Duration = Duration::from_secs(1);
/// Minimum spacing between redraws (~60 fps). A fast wheel emits input far quicker
/// than a terminal can repaint a full-screen diff, and repainting per event makes
/// the view (and quitting) lag. Every event is still applied, so scroll speed is
/// unchanged; only the repaints are coalesced.
const FRAME_MIN: Duration = Duration::from_micros(16_667);

/// Rows of context kept between the diff cursor and the viewport edge, so moving
/// the cursor scrolls before it reaches the very top or bottom.
const CURSOR_PADDING: u32 = 4;

/// Lines of context `+`/`-` add or take away around each change, per press.
const CONTEXT_STEP: u32 = 1;
/// While watching for comments written elsewhere, cap the input wait so an
/// agent's note appears without needing a keypress.
const COMMENT_POLL: Duration = Duration::from_millis(250);
/// While a render is under way, how often to look for it between keypresses. A
/// key read can't be interrupted, so this is as late as a finished render can land.
const RENDER_POLL: Duration = Duration::from_millis(5);
/// How long the first frame waits for its render. There's nothing on screen yet to
/// be held up, and the syntaxes it needs may still be loading.
const FIRST_WAIT: Duration = Duration::from_millis(150);

/// Which pane the j/k keys act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Focus {
    Tree,
    Diff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Back,
}

impl Direction {
    /// The end of the diff a jump this way comes round to when it wraps.
    fn end(self) -> &'static str {
        match self {
            Self::Forward => "top",
            Self::Back => "bottom",
        }
    }
}

/// What the view was on: the render, or once that's gone, what it showed.
enum ViewFrom {
    Render(Arc<Rendered>),
    /// The code at the top, and whether the cursor was on its anchored line.
    Top(Option<ViewTop>, bool),
}

pub struct App {
    pub files: Vec<FileDiff>,
    pub rows: Vec<Row>,
    pub tree_state: ListState,
    pub diff_scroll: u32,
    /// Height of the diff viewport at the last render, used to size page jumps.
    pub diff_height: u16,
    pub side_by_side: bool,
    pub show_tree: bool,
    pub show_header: bool,
    pub show_footer: bool,
    pub tree_width: u16,
    /// The tree width the session started with, which dragging the divider
    /// never shrinks below.
    tree_width_min: u16,
    dragging_divider: bool,
    /// Screen rects of the panes from the last render, for mapping mouse events.
    /// `None` before the first draw; `tree_area` is also `None` while the tree is
    /// hidden.
    pub tree_area: Option<Rect>,
    pub diff_area: Option<Rect>,
    pub focus: Focus,
    pub show_help: bool,
    pub status: Option<String>,
    pub icon_style: IconStyle,
    pub diff_theme: DiffTheme,
    pub finder: Option<Finder>,
    /// Text search across the diff: `/`, `n` and `N`.
    find: search::Find,
    /// Moving between changes: `J` and `K`.
    change_jumps: changes::ChangeJumps,
    pub cache: RenderCache,
    /// Persistent "viewed" review state, keyed per repo+branch. Session-only
    /// (no persistence) until [`App::enable_review`] runs, or when not in a repo.
    review: ReviewStore,
    /// One content hash per file in `files`, parallel by index, used to look up
    /// viewed state. Recomputed whenever `files` changes.
    file_hashes: Vec<u128>,
    review_auto_advance: bool,
    /// Inline review comments for this repo+branch. Session-only until
    /// [`App::enable_comments`] runs, or when not in a repo.
    comments: CommentStore,
    /// Bumped on every change to `comments`. The render cache carries the
    /// revision each file was spliced at, so a change re-splices exactly the
    /// renders that are stale.
    comment_rev: u64,
    /// Watches the comment file so notes written by an agent in another terminal
    /// show up here. `None` when comments don't persist (no repo scope).
    comment_watch: Option<CommentWatch>,
    /// Name recorded as the author of comments written in this window.
    comment_author: String,
    /// Whether comments are shown and the `c`/`x`/`]`/`[` keys are live.
    comments_on: bool,
    /// Line cursor in the diff pane: an index into the current render's lines,
    /// not a screen row. Only meaningful while comments are on.
    pub diff_cursor: usize,
    /// The diff line the cursor is on, kept so its line index can be recovered
    /// after a re-render moves it (a comment spliced in above, a theme change, a
    /// resize). `None` when the cursor is on a row with no line number.
    cursor_anchor: Option<Anchor>,
    /// Identity of the render `diff_cursor` indexes into. When it changes, the
    /// index is stale and gets re-resolved from `cursor_anchor`.
    cursor_token: Option<CursorToken>,
    /// The screen row the cursor sat on before `+`/`-` reloaded the diff, to put
    /// it back there once the new render exists. Widening the context pushes the
    /// cursor's line down the render; without this it would slide off screen.
    cursor_screen_row: Option<u32>,
    /// What the scroll was last measured against, to carry the view over to the
    /// next render when the width or the layout changes.
    view_from: Option<(Key, ViewFrom)>,
    /// Set when the view was carried over to a new render with the line cursor in
    /// view, for the draw to keep it there.
    keep_cursor: bool,
    /// Whether the line cursor was on screen the last time a render was drawn.
    pub cursor_on_screen: bool,
    /// A `]`/`[` jump that crossed into another file, waiting for that file to be
    /// rendered before the cursor can be put on the comment it was aimed at:
    /// coming in forward lands on its first comment, back on its last.
    comment_landing: Option<(usize, Direction)>,
    /// A diff line an `m` jump is heading for, waiting on the event loop to render
    /// the file it's in, the same way `comment_landing` does.
    move_landing: Option<MoveLanding>,
    /// The last `m` onto a copy's other end, so a second press comes back. A source
    /// can have several copies, so it can't always say which one the reader was on.
    copy_return: Option<CopyReturn>,
    /// The side `m` last followed from a side-by-side row with an end on both sides,
    /// by file and the row's new line number. The cursor has no side of its own, so
    /// this is how the two take turns.
    move_turns: HashMap<(usize, u32), Side>,
    /// Keys that act on the render, pressed before it was there for them to act on,
    /// oldest first. They run in order once it lands, so a render only ever delays
    /// a key rather than swallowing it. See [`App::must_wait`].
    held_keys: VecDeque<KeyEvent>,
    heading: Heading,
    /// What the files to prefetch were worked out from, as they only change with
    /// these, and the files.
    prefetch_asked: Option<Prefetched>,
    /// The id `x` has asked about deleting, waiting on the y/N answer.
    confirm_delete: Option<String>,
    /// The comment being typed, if any. While it's open it owns every keypress.
    composer: Option<Composer>,
    /// A comment handed off from the composer to `$EDITOR` (`Ctrl-O`), run once
    /// the event loop regains the terminal.
    pending_comment: Option<PendingComment>,
    matcher: SkimMatcherV2,
    nodes: Vec<Node>,
    collapsed: HashSet<String>,
    last_width: u16,
    quit: bool,
    pending_editor: Option<String>,
    /// The git command behind the diff, when there is one (`riffnav diff|show`).
    /// It is what the refresh key re-runs; `None` for a diff piped in on stdin,
    /// which can only be read once.
    git: Option<GitDiff>,
    herdr: Option<Herdr>,
    /// Enables the `W` web-diff key; `None` when no supported forge backs this repo.
    forge: Option<Forge>,
    /// One-way "viewed" sync to the branch's GitHub PR, when armed via config
    /// (and a GitHub forge is present). `None` leaves marks purely local.
    review_sync: Option<ReviewSync>,
    /// Whether we've zoomed our own herdr pane, so we can restore it on exit
    /// rather than leaving herdr maximized behind us.
    zoomed: bool,
    status_deadline: Option<Instant>,
}

impl App {
    pub fn new(files: Vec<FileDiff>, side_by_side: bool, cfg: &Config) -> Self {
        let file_hashes = files
            .iter()
            .map(|f| crate::review::file_hash(&f.raw))
            .collect();
        let nodes = tree::build(&files);
        let collapsed = tree::initial_collapsed(&nodes, cfg.open_depth);
        let rows = tree::flatten(&nodes, &collapsed);
        let first_file = rows
            .iter()
            .position(|r| matches!(r.kind, RowKind::File { .. }))
            .unwrap_or(0);
        let mut tree_state = ListState::default();
        tree_state.select(Some(first_file));
        let mut cache = RenderCache::new(cfg.tab_width, cfg.detect_moves, cfg.detect_copies);
        cache.index_moves(&files);

        Self {
            files,
            rows,
            tree_state,
            diff_scroll: 0,
            diff_height: 0,
            side_by_side,
            show_tree: cfg.show_tree,
            show_header: cfg.show_header,
            show_footer: cfg.show_footer,
            tree_width: cfg.tree_width.max(MIN_DIFF_WIDTH),
            tree_width_min: cfg.tree_width.max(MIN_DIFF_WIDTH),
            dragging_divider: false,
            tree_area: None,
            diff_area: None,
            // Start in the diff by default, so the first file is ready to read
            // and scroll; the tree can't hold focus when it's hidden.
            focus: if cfg.show_tree {
                cfg.start_focus
            } else {
                Focus::Diff
            },
            show_help: false,
            status: None,
            icon_style: cfg.icon_style,
            diff_theme: cfg.diff_theme,
            finder: None,
            find: search::Find::default(),
            change_jumps: changes::ChangeJumps::default(),
            cache,
            review: ReviewStore::disabled(),
            file_hashes,
            review_auto_advance: cfg.review_auto_advance,
            comments: CommentStore::disabled(),
            comment_rev: 0,
            comment_watch: None,
            comment_author: String::new(),
            comments_on: false,
            diff_cursor: 0,
            cursor_anchor: None,
            cursor_token: None,
            cursor_screen_row: None,
            view_from: None,
            keep_cursor: false,
            cursor_on_screen: false,
            comment_landing: None,
            move_landing: None,
            copy_return: None,
            move_turns: HashMap::new(),
            held_keys: VecDeque::new(),
            heading: Heading {
                dir: Direction::Forward,
                by: By::File,
            },
            prefetch_asked: None,
            confirm_delete: None,
            composer: None,
            pending_comment: None,
            matcher: SkimMatcherV2::default(),
            nodes,
            collapsed,
            last_width: 0,
            quit: false,
            pending_editor: None,
            git: None,
            herdr: None,
            forge: None,
            review_sync: None,
            zoomed: false,
            status_deadline: None,
        }
    }

    /// Enables the `z` zoom key when running inside herdr.
    pub fn enable_herdr(&mut self) {
        self.herdr = Herdr::detect();
    }

    pub fn in_herdr(&self) -> bool {
        self.herdr.is_some()
    }

    /// Enables the `W` key when a supported forge (GitHub via `gh`) is available.
    pub fn enable_forge(&mut self) {
        self.forge = Forge::detect();
    }

    pub fn has_forge(&self) -> bool {
        self.forge.is_some()
    }

    /// The diff pane width the cached renders were built at, for looking one up
    /// outside the draw path.
    pub fn last_diff_width(&self) -> u16 {
        self.last_width
    }

    /// Show a transient status message that auto-clears after [`STATUS_TTL`].
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
        self.status_deadline = Some(Instant::now() + STATUS_TTL);
    }

    fn clear_status(&mut self) {
        self.status = None;
        self.status_deadline = None;
    }

    fn expire_status(&mut self) {
        if let Some(deadline) = self.status_deadline
            && Instant::now() >= deadline
        {
            self.clear_status();
        }
    }

    fn toggle_herdr_zoom(&mut self) {
        let Some(herdr) = &mut self.herdr else { return };
        let msg = match herdr.toggle_zoom() {
            Ok(Some(zoomed)) => {
                self.zoomed = zoomed;
                if zoomed { "⊕ Zoomed" } else { "⊖ Unzoomed" }.to_string()
            }
            Ok(None) => "⧉ Zoom toggled".to_string(),
            // `{e:#}` includes the cause chain, not just the top-level context.
            Err(e) => format!("herdr: {e:#}"),
        };
        self.set_status(msg);
    }

    /// Open the branch's PR diff in the browser. The forge's CLI launches the
    /// browser, so this returns promptly.
    fn open_web_diff(&mut self) {
        let Some(forge) = &self.forge else { return };
        let msg = match forge.open_web_diff() {
            Ok(()) => format!("Opened {} PR diff in browser", forge.name()),
            Err(e) => format!("{}: {e:#}", forge.name()),
        };
        self.set_status(msg);
    }

    /// Undo a zoom we toggled on, so herdr's layout is left as we found it.
    /// Best-effort: we're shutting down, so errors are ignored.
    fn restore_herdr_zoom(&mut self) {
        if !self.zoomed {
            return;
        }
        if let Some(herdr) = &self.herdr {
            let _ = herdr.restore_zoom();
        }
        self.zoomed = false;
    }

    pub fn run(&mut self) -> Result<()> {
        let mut terminal = init_terminal();
        enable_mouse();
        enable_enhanced_keys();
        let result = self.event_loop(&mut terminal);
        disable_enhanced_keys();
        disable_mouse();
        ratatui::restore();
        self.cache.stop();
        report_background_panics();
        self.restore_herdr_zoom();
        self.flush_review_sync(); // let in-flight GitHub marks finish (bounded)
        self.review.save(); // safety net; toggles already persist eagerly
        if self.comments_on {
            Session::clear(); // don't leave the CLI validating against a dead window
        }
        result
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.cache.wait(FIRST_WAIT);
        while !self.quit {
            let diff_width = self.diff_pane_width(terminal.size()?.width);
            self.prepare_frame(diff_width);
            self.cache.wait(WAIT);

            terminal.draw(|frame| crate::ui::draw(frame, self, diff_width))?;
            let last_draw = Instant::now();
            self.start_copies();

            self.wait_for_event()?;
            // Apply buffered input until the frame budget (`FRAME_MIN`) elapses or
            // input goes quiet, then redraw once. Quitting/editor break out at once.
            while !self.quit && self.pending_editor.is_none() {
                let remaining = FRAME_MIN.saturating_sub(last_draw.elapsed());
                if remaining.is_zero() || !event::poll(remaining)? {
                    break;
                }
                self.handle_event()?;
            }

            // Suspending the TUI to run an editor needs the owned terminal.
            if let Some(path) = self.pending_editor.take() {
                self.open_editor(terminal, &path)?;
                self.refresh_file(&path);
            }
            if let Some(pending) = self.pending_comment.take() {
                self.compose_comment(terminal, pending)?;
            }
        }
        Ok(())
    }

    /// Everything a frame needs before it's drawn: the selected file's render, and
    /// the cursor and any jump in progress settled onto it.
    fn prepare_frame(&mut self, diff_width: u16) {
        // On resize, drop renders made at the old width so the cache stays
        // bounded and the current file re-renders to the new width.
        if diff_width != self.last_width {
            self.let_go_of_view_render();
            self.cache.clear();
            self.last_width = diff_width;
        }

        // Before the render, as it can select the file a match is in.
        self.advance_search();
        // A jump that goes on into another file needs that file rendered too, and
        // no keypress may be coming to wake the event loop up for it. It stops at a
        // file that isn't ready, or at the frame's time, as a run of ordinary files
        // adds up, and the event loop comes back for the rest either way.
        let deadline = Instant::now() + self.cache.waits();
        loop {
            let file = self.selected_file();
            self.settle_view(diff_width);
            if self.selected_file() == file || Instant::now() >= deadline {
                break;
            }
        }
        self.prefetch_next(diff_width);
        if let Some(failure) = self.cache.take_failure() {
            self.set_status(format!("Couldn't render {failure}"));
        }
    }

    /// Start the search for copied code, when there's a new diff to do it for.
    fn start_copies(&mut self) {
        if self.cache.wants_copies() {
            let tree = self.git.as_ref().and_then(GitDiff::tree);
            self.cache.start_copies(&self.files, tree);
        }
    }

    /// Put a question to the render cache about file `idx`, shown `width` wide.
    fn ask_cache<T>(
        &mut self,
        idx: usize,
        width: u16,
        ask: impl FnOnce(&mut RenderCache, &Request<'_>) -> T,
    ) -> T {
        let side_by_side = self.side_by_side_for(idx);
        let diff_hash = self.file_hashes.get(idx).copied().unwrap_or(0);
        let comments = Self::comment_layer(
            &self.files,
            &self.comments,
            &self.comment_author,
            diff_hash,
            self.comment_rev,
            self.comments_on,
            idx,
        );
        let request = Request {
            file: idx,
            diff: &self.files[idx],
            width,
            side_by_side,
            theme: self.diff_theme,
            comments,
        };
        ask(&mut self.cache, &request)
    }

    /// Render the selected file, and put the cursor and any jump in progress on it.
    /// The render may not be there yet, and all of this then waits for it to land.
    fn settle_view(&mut self, diff_width: u16) {
        match self.selected_file() {
            Some(idx) => self.ask_cache(idx, diff_width, RenderCache::ensure),
            None => self.cache.release(),
        }
        self.keep_view_on_copies();
        self.carry_view();
        // The render may have changed shape (new comments, new theme, new
        // width), so put the cursor back on the line it was on.
        self.resync_cursor();
        self.restore_cursor_screen_row();
        self.follow_change();
        // Jumps into this file have been waiting for that render.
        self.land_on_comment();
        self.land_on_move();
        self.land_on_match();
        self.run_change_jumps();
        self.replay_held_keys();
    }

    /// Notes on copies land whenever the search is done, which is no reason for the
    /// code being read to shift down the screen.
    fn keep_view_on_copies(&mut self) {
        let Some(file) = self.selected_file() else {
            return;
        };
        let key = self.render_key(file);
        if let Some(scroll) = self.cache.kept_scroll(key, self.diff_scroll) {
            self.diff_scroll = scroll;
        }
    }

    /// A new width or layout wraps the file onto other rows, so the scroll is
    /// moved to keep the same code at the top. Waits until the new render is
    /// there, scrolling from the one the view was on. A line cursor that was in
    /// view on that one is kept in view on this, once the pane's height is known.
    fn carry_view(&mut self) {
        let Some(file) = self.selected_file() else {
            return;
        };
        let key = self.render_key(file);
        let Some(now) = self.cache.shared(key) else {
            return;
        };
        let Some((was, from)) = self.view_from.replace((key, ViewFrom::Render(now.clone()))) else {
            return;
        };
        if was.file == file && was != key {
            let (top, anchored) = match &from {
                ViewFrom::Render(old) => self.view_on(old),
                ViewFrom::Top(top, anchored) => (*top, *anchored),
            };
            self.keep_cursor = self.comments_enabled() && anchored && self.cursor_on_screen;
            if let Some(scroll) = top.and_then(|top| now.scroll_to(top)) {
                self.diff_scroll = scroll;
            }
        }
        if let ViewFrom::Render(old) = from {
            self.forget_render(old);
        }
    }

    /// What `render` shows at the top at the current scroll, if not the top of the
    /// file, and whether the line cursor is on the line it's anchored to. A cursor
    /// parked on a header or a note goes back to the last numbered line it was on,
    /// which needn't be in view, so it isn't kept in view.
    fn view_on(&self, render: &Rendered) -> (Option<ViewTop>, bool) {
        let top = (self.diff_scroll > 0)
            .then(|| render.top_at(self.diff_scroll))
            .flatten();
        let anchored = render.line_map.get(self.diff_cursor).anchor() == self.cursor_anchor;
        (top, anchored)
    }

    /// Keep only what the render the view is on shows, as a resize is about to
    /// drop it from the cache and it can be big.
    fn let_go_of_view_render(&mut self) {
        let Some((key, ViewFrom::Render(render))) = &self.view_from else {
            return;
        };
        let (key, render) = (*key, render.clone());
        let (top, anchored) = self.view_on(&render);
        self.view_from = Some((key, ViewFrom::Top(top, anchored)));
        self.forget_render(render);
    }

    /// A reloaded diff keeps the scroll it had, rather than have it carried by line
    /// numbers that may now be other code.
    fn forget_view_render(&mut self) {
        if let Some((_, ViewFrom::Render(render))) = self.view_from.take() {
            self.forget_render(render);
        }
    }

    /// Let go of a render the app held on to.
    fn forget_render(&mut self, render: Arc<Rendered>) {
        if Arc::strong_count(&render) == 1 {
            self.cache.discard(render);
        }
    }

    /// Whether a key that reads the render can act now: the one on show is there
    /// for it, the cursor is good for it, and no jump is still on its way.
    fn view_settled(&self) -> bool {
        self.comment_landing.is_none()
            && self.move_landing.is_none()
            && !self.heading_for_match()
            && !self.change_jumps_waiting()
            && self.render_ready()
    }

    /// Blocks for the next event, bounded by [`App::idle_timeout`]. While a render
    /// is running it also looks for that between keys, and returns when one lands.
    fn wait_for_event(&mut self) -> Result<()> {
        let deadline = self.idle_timeout().map(|timeout| Instant::now() + timeout);
        loop {
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            let busy = self.cache.expecting().then_some(RENDER_POLL);
            let slice = left.into_iter().chain(busy).min();
            let Some(slice) = slice else {
                self.handle_event()?;
                break;
            };
            if event::poll(slice)? {
                self.handle_event()?;
                break;
            }
            if self.cache.collect() || left.is_some_and(|left| left <= slice) {
                break;
            }
        }
        self.expire_status();
        self.drain_review_sync();
        self.poll_comments();
        Ok(())
    }

    /// How long the input wait may block, or `None` to block until a key: work that
    /// needs servicing without a keypress (status expiry, sync results, comment
    /// watching, search, an unsettled view) bounds it.
    fn idle_timeout(&self) -> Option<Duration> {
        let status = self
            .status_deadline
            .map(|d| d.saturating_duration_since(Instant::now()));
        let syncing = self
            .review_sync
            .as_ref()
            .is_some_and(ReviewSync::has_pending);
        // The filesystem watcher can't interrupt a blocking key read, so while
        // it's armed the wait is capped and the channel drained on each wake.
        let watching_comments = self.comment_watch.is_some();
        let bound = match (syncing, watching_comments) {
            (true, _) => Some(SYNC_POLL),
            (false, true) => Some(COMMENT_POLL),
            (false, false) => None,
        };
        // A frame that ran out of time before the view settled has more to do, and
        // no render on the way is going to say so.
        let unsettled = !self.cache.busy() && !(self.view_settled() && self.held_keys.is_empty());
        let settling = unsettled.then_some(FRAME_MIN);
        [status, bound, self.search_wait(), settling]
            .into_iter()
            .flatten()
            .min()
    }

    fn diff_pane_width(&self, total: u16) -> u16 {
        let used = if self.show_tree { self.tree_width } else { 0 };
        total.saturating_sub(used).max(MIN_DIFF_WIDTH)
    }

    /// The render cache is keyed by theme, so cycling back to a theme already seen
    /// is instant.
    fn cycle_theme(&mut self) {
        self.diff_theme = self.diff_theme.next();
        self.set_status(format!("Diff theme: {}", self.diff_theme.name()));
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{app_with, file};
    use super::*;

    #[test]
    fn status_clears_once_its_deadline_passes() {
        let mut app = app_with(vec![file("a.rs")]);
        app.set_status("hi");
        assert!(app.status.is_some());

        app.expire_status();
        assert!(app.status.is_some());

        app.status_deadline = Some(Instant::now());
        app.expire_status();
        assert!(app.status.is_none());
        assert!(app.status_deadline.is_none());
    }

    /// A passing message sets its own deadline: with none it would stay up for
    /// good, or go at one an earlier message left behind.
    #[test]
    fn each_passing_message_sets_its_own_deadline() {
        let mut app = app_with(vec![file("a.rs")]);
        app.press_for_test(crossterm::event::KeyCode::Char('i'), false);
        assert!(
            app.status.is_some() && app.status_deadline.is_some(),
            "icons"
        );
        app.clear_status();
        app.reload_files(vec![file("a.rs")]);
        assert!(
            app.status.is_some() && app.status_deadline.is_some(),
            "refresh"
        );
    }

    #[test]
    fn start_focus_follows_config_but_yields_when_tree_hidden() {
        let diff_first = App::new(vec![file("a.rs")], false, &Config::default());
        assert_eq!(diff_first.focus, Focus::Diff); // default: single-file view

        let tree_cfg = Config {
            start_focus: Focus::Tree,
            ..Config::default()
        };
        let tree_first = App::new(vec![file("a.rs")], false, &tree_cfg);
        assert_eq!(tree_first.focus, Focus::Tree);

        // With the tree hidden there's nothing to focus but the diff.
        let hidden_cfg = Config {
            start_focus: Focus::Tree,
            show_tree: false,
            ..Config::default()
        };
        let hidden = App::new(vec![file("a.rs")], false, &hidden_cfg);
        assert_eq!(hidden.focus, Focus::Diff);
    }
}
