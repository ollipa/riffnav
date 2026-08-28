use std::collections::HashSet;
use std::io::IsTerminal;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::DefaultTerminal;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;
use serde::Deserialize;

use crate::autodiff::{AutoDiff, DiffSource};
use crate::comment::{Anchor, Comment, CommentStore, CommentWatch, Composer, PendingComment};
use crate::config::Config;
use crate::delta::{CommentLayer, RenderCache};
use crate::diff::{FileDiff, FileStatus};
use crate::forge::{Forge, ReviewSync};
use crate::herdr::Herdr;
use crate::icons::IconStyle;
use crate::review::ReviewStore;
use crate::session::Session;
use crate::theme::DiffTheme;
use crate::tree::{self, Node, Row, RowKind};
use crate::watch::Watch;

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
/// Minimum spacing between redraws (~60 fps). A fast wheel can emit input far
/// quicker than a terminal can repaint a full-screen diff; redrawing once per
/// event piles full-screen repaints onto the terminal until it falls behind and
/// the view (and quitting) lag. We still apply every event to the scroll state —
/// so scroll speed is unchanged — but coalesce a burst into a single repaint.
const FRAME_MIN: Duration = Duration::from_micros(16_667);

/// The only mouse reporting riffnav uses: button presses — which include wheel
/// ticks — plus motion *while a button is held*, with SGR-encoded coordinates
/// (`?1002` + `?1006`). Button-drag motion is what makes the pane divider
/// draggable. Deliberately *not* crossterm's `EnableMouseCapture`, which also
/// enables any-motion tracking (`?1003`): riffnav ignores unpressed pointer
/// motion, so reporting it just wakes the loop to repaint the whole screen for
/// every mouse twitch.
const MOUSE_ON: &[u8] = b"\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1002l";

/// Rows of context kept between the diff cursor and the viewport edge, so moving
/// the cursor scrolls before it reaches the very top or bottom. Matches the file
/// tree's `SCROLL_PADDING`.
const CURSOR_PADDING: u16 = 4;
/// While watching for comments written elsewhere, cap the input wait so an
/// agent's note appears without needing a keypress.
const COMMENT_POLL: Duration = Duration::from_millis(250);

/// Best-effort terminal mouse reporting, toggled around screen ownership: on
/// while the TUI runs so clicks and the wheel reach us, off whenever we hand the
/// terminal back (teardown, or suspending for `$EDITOR`). Failures are ignored —
/// a terminal without mouse support just keeps working off the keyboard.
fn enable_mouse() {
    write_stdout(MOUSE_ON);
}

fn disable_mouse() {
    write_stdout(MOUSE_OFF);
}

fn write_stdout(bytes: &[u8]) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// Separator between a comment body and the explanatory footer, borrowed from
/// git's `--verbose` scissors. Everything from this line down is discarded, which
/// leaves `#` usable inside the body itself.
const SCISSORS: &str = "# ------------------------ >8 ------------------------";

/// The buffer `$EDITOR` opens on: whatever was already typed in the composer (or
/// a blank line to type into), then the scissors and the context being
/// commented on.
fn comment_template(pending: &PendingComment) -> String {
    let mut out = if pending.draft.is_empty() {
        String::from("\n")
    } else {
        format!("{}\n", pending.draft)
    };
    out.push_str(SCISSORS);
    out.push('\n');
    out.push_str(&format!(
        "# Comment on {}:{} ({} side). Everything below the line above is\n\
         # ignored; save an empty comment to abort.\n#\n",
        pending.file,
        pending.anchor.line,
        pending.anchor.side.as_str(),
    ));
    for line in &pending.context {
        out.push_str("#   ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Everything above the scissors line, which is the comment the author typed.
fn strip_scissors(text: &str) -> String {
    text.split(SCISSORS).next().unwrap_or("").trim().to_string()
}

/// Which pane the j/k keys act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Focus {
    Tree,
    Diff,
}

/// State of the fuzzy file-finder overlay.
pub struct Finder {
    pub query: String,
    /// File indices that match `query`, best first.
    pub matches: Vec<usize>,
    /// Index into `matches` of the highlighted row.
    pub selected: usize,
}

pub struct App {
    pub files: Vec<FileDiff>,
    pub rows: Vec<Row>,
    pub tree_state: ListState,
    pub diff_scroll: u16,
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
    /// Whether a mouse drag of the tree/diff divider is in progress.
    dragging_divider: bool,
    /// Screen rects of the tree and diff panes from the last render, so mouse
    /// clicks and wheel scrolls map back to a row or a pane. `None` before the
    /// first draw; `tree_area` is also `None` whenever the tree is hidden.
    pub tree_area: Option<Rect>,
    pub diff_area: Option<Rect>,
    pub focus: Focus,
    pub show_help: bool,
    pub status: Option<String>,
    pub icon_style: IconStyle,
    pub diff_theme: DiffTheme,
    pub finder: Option<Finder>,
    pub cache: RenderCache,
    /// Persistent "viewed" review state, keyed per repo+branch. Session-only
    /// (no persistence) until [`App::enable_review`] runs, or when not in a repo.
    review: ReviewStore,
    /// One content hash per file in `files`, parallel by index, used to look up
    /// viewed state. Recomputed whenever `files` changes.
    file_hashes: Vec<u128>,
    /// Whether marking a file viewed advances to the next unviewed file.
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
    cursor_token: Option<(usize, u16, bool, DiffTheme, u64)>,
    /// The comment being typed, if any. While it's open it owns every keypress.
    composer: Option<Composer>,
    /// A comment handed off from the composer to `$EDITOR` (`Ctrl-O`), run once
    /// the event loop regains the terminal — next to `pending_editor`.
    pending_comment: Option<PendingComment>,
    matcher: SkimMatcherV2,
    nodes: Vec<Node>,
    collapsed: HashSet<String>,
    last_width: u16,
    quit: bool,
    pending_editor: Option<String>,
    watch: Option<Watch>,
    /// Auto-diff state when launched bare (no piped diff): the active git-derived
    /// source and the base it can compare against. `None` for a piped/watch diff.
    autodiff: Option<AutoDiff>,
    herdr: Option<Herdr>,
    /// The detected source-code forge (e.g. GitHub), enabling the `W` web-diff
    /// key; `None` when no supported forge backs this repo.
    forge: Option<Forge>,
    /// One-way "viewed" sync to the branch's GitHub PR, when armed via config
    /// (and a GitHub forge is present). `None` leaves marks purely local.
    review_sync: Option<ReviewSync>,
    /// Whether we've zoomed our own herdr pane, so we can restore it on exit
    /// rather than leaving herdr maximized behind us.
    zoomed: bool,
    /// When set, the current `status` clears itself once this instant passes.
    status_deadline: Option<Instant>,
}

impl App {
    pub fn new(files: Vec<FileDiff>, side_by_side: bool, config_sbs: bool, cfg: &Config) -> Self {
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
            cache: RenderCache::new(config_sbs),
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
            composer: None,
            pending_comment: None,
            matcher: SkimMatcherV2::default(),
            nodes,
            collapsed,
            last_width: 0,
            quit: false,
            pending_editor: None,
            watch: None,
            autodiff: None,
            herdr: None,
            forge: None,
            review_sync: None,
            zoomed: false,
            status_deadline: None,
        }
    }

    /// Turn on watch mode: refresh the diff when the working tree changes.
    pub fn enable_watch(
        &mut self,
        cmd: String,
        interval: Duration,
        initial_diff: String,
    ) -> Result<()> {
        self.watch = Some(Watch::new(cmd, interval, initial_diff)?);
        Ok(())
    }

    pub fn is_watching(&self) -> bool {
        self.watch.is_some()
    }

    /// Enter auto-diff mode (bare launch): record which git-derived source is
    /// shown and the base branch it can compare against, so the header can label
    /// the view. The diff text itself was already loaded and parsed into `files`.
    pub fn enable_autodiff(&mut self, source: DiffSource, base: Option<String>) {
        self.autodiff = Some(AutoDiff { source, base });
    }

    /// The active auto-diff source's label (e.g. "all uncommitted"), or `None`
    /// when the diff came from stdin or a watch command.
    pub fn autodiff_label(&self) -> Option<&'static str> {
        self.autodiff.as_ref().map(|a| a.source.label())
    }

    pub fn is_autodiff(&self) -> bool {
        self.autodiff.is_some()
    }

    /// Cycle to the next auto-diff source (the `d` key): re-run the matching git
    /// command and reload the file set. Only reachable in auto-diff mode. The
    /// branch-vs-base view is skipped when no base was detected, and a source
    /// that yields nothing reloads to an empty set with an explanatory status.
    fn cycle_diff_source(&mut self) {
        let Some(auto) = &self.autodiff else { return };
        let next = auto.source.next(auto.base.is_some());
        let base = auto.base.clone();
        // The immutable borrow of `self.autodiff` ends here (next/base are owned),
        // freeing `self` for the mutable reload below.
        match crate::autodiff::load(next, base.as_deref()) {
            Ok(text) => {
                let files = crate::diff::parse(&text);
                self.reload_files(files);
                if let Some(auto) = &mut self.autodiff {
                    auto.source = next;
                }
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

    /// Whether there's a diff source that can be re-read, which is what makes the
    /// `r` refresh key worth binding: a git-derived bare launch, or watch mode's
    /// command. A diff piped in on stdin can only be read once.
    pub fn can_refresh(&self) -> bool {
        self.autodiff.is_some() || self.watch.is_some()
    }

    /// The `r` key: re-run the diff and reload it, so work done since launch
    /// shows up without restarting. In watch mode this forces the command now
    /// instead of waiting on the debounce or the interval.
    fn refresh_diff(&mut self) {
        let loaded = if let Some(auto) = &self.autodiff {
            let (source, base) = (auto.source, auto.base.clone());
            // The immutable borrow of `self.autodiff` ends here (source/base are
            // owned), freeing `self` for the mutable reload below.
            crate::autodiff::load(source, base.as_deref())
        } else if let Some(watch) = self.watch.as_mut() {
            watch.reload_now()
        } else {
            return;
        };
        match loaded {
            Ok(text) => self.reload_in_place(crate::diff::parse(&text)),
            // `{e:#}` includes the command's own message.
            Err(e) => self.set_status(format!("refresh: {e:#}")),
        }
    }

    /// Swap in a freshly loaded file set the way a refresh wants it: as
    /// [`App::reload_files`], but holding the scroll and the line cursor when the
    /// same file is still selected afterwards. Reloading rebuilds every render,
    /// yet the file on screen is usually the one you were just reading — only
    /// longer or shorter — and being thrown back to its top on every refresh is
    /// what would make `r` unusable mid-review.
    fn reload_in_place(&mut self, files: Vec<FileDiff>) {
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

    /// Detect whether riffnav is running inside herdr, enabling the `z` zoom key.
    /// A no-op (leaves `herdr` as `None`) when not inside herdr.
    pub fn enable_herdr(&mut self) {
        self.herdr = Herdr::detect();
    }

    pub fn in_herdr(&self) -> bool {
        self.herdr.is_some()
    }

    /// Detect a supported source-code forge (currently GitHub via `gh`), enabling
    /// the `W` key to open the branch's PR diff in the browser. Leaves `forge` as
    /// `None` — and the key inert — when none is available.
    pub fn enable_forge(&mut self) {
        self.forge = Forge::detect();
    }

    pub fn has_forge(&self) -> bool {
        self.forge.is_some()
    }

    /// Arm one-way "viewed" sync to the branch's GitHub PR (the `review_sync_github`
    /// config key). Only takes effect when a GitHub forge was detected; otherwise
    /// it's a no-op and marks stay purely local. Call after [`App::enable_forge`].
    pub fn enable_review_sync(&mut self, enabled: bool) {
        if enabled && self.forge.is_some() {
            self.review_sync = Some(ReviewSync::new());
        }
    }

    /// Whether a viewed mark for the selected file should be pushed to GitHub:
    /// sync is armed AND we're in the branch-vs-base view (the only view that
    /// mirrors the PR diff). The uncommitted/staged/unstaged views stay local.
    fn syncs_viewed_marks(&self) -> bool {
        self.review_sync.is_some()
            && matches!(
                self.autodiff.as_ref().map(|a| a.source),
                Some(DiffSource::Committed)
            )
    }

    /// Load persistent "viewed" review state for the current repo+branch (and
    /// garbage-collect stale state). A no-op outside a git repo, where the store
    /// stays session-only. Called once at startup, after `files` are in place.
    pub fn enable_review(&mut self, retention_days: u64) {
        self.review = ReviewStore::load(retention_days);
        // With viewed state now loaded, resume on the first file still needing
        // review instead of the top of the list. Opening straight onto already-
        // reviewed files would just make the user scroll past them.
        self.select_first_unviewed();
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
        self.bump_comments();
    }

    /// Publish what this window is showing, so `riffnav comment add` running in
    /// another terminal can validate a `--file`/`--line` against the diff on
    /// screen rather than guessing at one. Best-effort; the CLI falls back to
    /// re-deriving the diff from git when there's no session to read.
    fn publish_session(&self) {
        if !self.comments_on {
            return;
        }
        let source = self.autodiff_label().unwrap_or(if self.watch.is_some() {
            "watch"
        } else {
            "stdin"
        });
        let base = self.autodiff.as_ref().and_then(|a| a.base.clone());
        Session::new(&self.files, source, base).save();
    }

    pub fn comments_enabled(&self) -> bool {
        self.comments_on
    }

    /// The diff pane width the cached renders were built at, for looking one up
    /// outside the draw path.
    pub fn last_diff_width(&self) -> u16 {
        self.last_width
    }

    /// How many comments are anchored in the file at `diff_index`, for the tree's
    /// per-file badge.
    pub fn comment_count(&self, diff_index: usize) -> usize {
        if !self.comments_on {
            return 0;
        }
        self.files
            .get(diff_index)
            .map_or(0, |f| self.comments.count_for_file(f.path()))
    }

    /// Total comments across every file in the current diff.
    pub fn comment_total(&self) -> usize {
        if !self.comments_on {
            return 0;
        }
        (0..self.files.len()).map(|i| self.comment_count(i)).sum()
    }

    /// Note that the comment set changed, so cached renders re-splice.
    fn bump_comments(&mut self) {
        self.comment_rev = self.comment_rev.wrapping_add(1);
    }

    /// The comment layer for file `idx`, borrowed for one `RenderCache::ensure`.
    ///
    /// An associated function rather than a method: the layer borrows `files` and
    /// `comments` for as long as the call, and taking `&self` would keep `cache`
    /// borrowed too — which `ensure` needs mutably. Naming the fields lets the
    /// borrow checker see the three are disjoint.
    fn comment_layer<'a>(
        files: &'a [FileDiff],
        comments: &'a CommentStore,
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
            diff_hash,
            rev,
        }
    }

    /// Reload the store when the watcher reports the file changed, so a note an
    /// agent just wrote appears here. Also fires after our own save, where the
    /// reload is a harmless no-op beyond one re-splice.
    fn poll_comments(&mut self) {
        if !self
            .comment_watch
            .as_ref()
            .is_some_and(CommentWatch::changed)
        {
            return;
        }
        let before = self.comments.all().len();
        self.comments.reload();
        self.bump_comments();
        let after = self.comments.all().len();
        if after > before {
            self.set_status(format!("💬 {} new comment(s)", after - before));
        }
    }

    /// Move the selection to the first unviewed file, scanning from the top. A
    /// no-op when there are no files or every file is already viewed, so the
    /// initial first-file selection stands. Run once at startup, after viewed
    /// state loads.
    fn select_first_unviewed(&mut self) {
        if let Some(i) = self.rows.iter().position(
            |r| matches!(r.kind, RowKind::File { diff_index } if !self.is_viewed(diff_index)),
        ) {
            self.select(i);
        }
    }

    /// Whether the file at `diff_index` is marked viewed.
    pub fn is_viewed(&self, diff_index: usize) -> bool {
        self.file_hashes
            .get(diff_index)
            .is_some_and(|h| self.review.is_viewed(*h))
    }

    /// How many of the current files are marked viewed.
    pub fn viewed_count(&self) -> usize {
        self.review.count_viewed(&self.file_hashes)
    }

    /// Toggle the selected file's viewed mark, persisting the change and
    /// reporting the new state plus overall progress.
    fn toggle_viewed(&mut self) {
        let Some(idx) = self.selected_file() else {
            self.set_status("No file selected to mark viewed");
            return;
        };
        let path = self.files[idx].path().to_string();
        let now_viewed = self.review.toggle(self.file_hashes[idx], &path);
        self.review.save();
        let progress = format!("{}/{}", self.viewed_count(), self.files.len());
        // Queue a GitHub sync when armed and in the PR view; it runs in the
        // background, so show the mark's success now and let the event loop
        // replace it only if the sync later fails. The local mark always stands.
        self.queue_sync(&path, now_viewed);
        self.set_status(if now_viewed {
            format!("✓ Viewed {path}  ({progress})")
        } else {
            format!("Unviewed {path}  ({progress})")
        });
        // Flow to the next file to review — but only on marking, not unmarking,
        // and keep the status above so progress stays visible.
        if now_viewed
            && self.review_auto_advance
            && let Some(i) = self.next_unviewed_after(self.selected_index())
        {
            self.select(i);
        }
    }

    /// Queue a GitHub sync of `path`'s viewed mark when armed and in the PR view
    /// (see [`App::syncs_viewed_marks`]); a no-op otherwise. The `gh` round trip
    /// runs on a background thread, so this returns immediately — the mark's
    /// optimistic status stands until [`App::drain_review_sync`] reports a failure.
    fn queue_sync(&mut self, path: &str, viewed: bool) {
        if !self.syncs_viewed_marks() {
            return;
        }
        self.review_sync
            .as_mut()
            .expect("sync armed when syncs_viewed_marks is true")
            .enqueue(path, viewed);
    }

    /// Surface any GitHub sync that finished failing since the last tick. With
    /// the optimistic mark already shown, only a real `gh` failure replaces it —
    /// and the local viewed mark stands regardless. A no-op when sync isn't armed.
    fn drain_review_sync(&mut self) {
        let errors = match self.review_sync.as_mut() {
            Some(sync) => sync.drain(),
            None => return,
        };
        // The status line shows one message; the most recent failure is the
        // useful one (e.g. the same auth/PR error repeated across a burst).
        if let Some(msg) = errors.into_iter().next_back() {
            self.set_status(format!("GitHub sync failed: {msg}"));
        }
    }

    /// On shutdown, give queued GitHub syncs a brief, bounded chance to finish so
    /// a file marked moments before quitting still reaches the PR — without a slow
    /// `gh` hanging exit. A no-op when sync isn't armed or nothing is in flight.
    fn flush_review_sync(&mut self) {
        if let Some(sync) = self.review_sync.as_mut() {
            sync.flush(SYNC_FLUSH_GRACE);
        }
    }

    /// The next unviewed file row after `from`, wrapping around, or `None` when
    /// every file is viewed.
    fn next_unviewed_after(&self, from: usize) -> Option<usize> {
        let n = self.rows.len();
        if n == 0 {
            return None;
        }
        (1..=n).map(|off| (from + off) % n).find(|&i| {
            matches!(self.rows[i].kind, RowKind::File { diff_index } if !self.is_viewed(diff_index))
        })
    }

    /// Select the next unviewed file after the cursor, reporting when everything
    /// has been reviewed.
    fn jump_unviewed(&mut self) {
        match self.next_unviewed_after(self.selected_index()) {
            Some(i) => self.select(i),
            None => self.set_status("All files reviewed ✓"),
        }
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

    /// Drop a timed status message once its display window has elapsed.
    fn expire_status(&mut self) {
        if let Some(deadline) = self.status_deadline
            && Instant::now() >= deadline
        {
            self.clear_status();
        }
    }

    /// Ask herdr to toggle zoom on our pane, reporting the outcome in the status
    /// line. Only reachable when running inside herdr.
    fn toggle_herdr_zoom(&mut self) {
        let Some(herdr) = &self.herdr else { return };
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

    /// Open the current branch's PR diff on the detected forge in the browser,
    /// reporting the outcome in the status line. Only reachable when a forge was
    /// detected. The forge's CLI launches the browser, so this returns promptly.
    fn open_web_diff(&mut self) {
        let Some(forge) = &self.forge else { return };
        let msg = match forge.open_web_diff() {
            Ok(()) => format!("Opened {} PR diff in browser", forge.name()),
            // `{e:#}` includes the cause chain (e.g. gh's own message).
            Err(e) => format!("{}: {e:#}", forge.name()),
        };
        self.set_status(msg);
    }

    /// Undo a zoom we toggled on, so closing riffnav leaves herdr's layout the
    /// way we found it. Best-effort: we're shutting down, so a herdr error is
    /// ignored rather than surfaced.
    fn restore_herdr_zoom(&mut self) {
        if !self.zoomed {
            return;
        }
        if let Some(herdr) = &self.herdr {
            let _ = herdr.toggle_zoom();
        }
        self.zoomed = false;
    }

    pub fn run(&mut self) -> Result<()> {
        let mut terminal = ratatui::init();
        enable_mouse();
        let result = self.event_loop(&mut terminal);
        disable_mouse();
        ratatui::restore();
        self.restore_herdr_zoom();
        self.flush_review_sync(); // let in-flight GitHub marks finish (bounded)
        self.review.save(); // safety net; toggles already persist eagerly
        self.comments.save(); // ditto — comment writes persist as they're made
        if self.comments_on {
            Session::clear(); // don't leave the CLI validating against a dead window
        }
        result
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            let diff_width = self.diff_pane_width(terminal.size()?.width);

            // On resize, drop renders made at the old width so the cache stays
            // bounded and the current file re-renders to the new width.
            if diff_width != self.last_width {
                self.cache.clear();
                self.last_width = diff_width;
            }

            if let Some(idx) = self.selected_file() {
                let side_by_side = self.side_by_side_for(idx);
                let diff_hash = self.file_hashes.get(idx).copied().unwrap_or(0);
                let layer = Self::comment_layer(
                    &self.files,
                    &self.comments,
                    diff_hash,
                    self.comment_rev,
                    self.comments_on,
                    idx,
                );
                self.cache.ensure(
                    idx,
                    &self.files[idx].raw,
                    diff_width,
                    side_by_side,
                    self.diff_theme,
                    &layer,
                )?;
            }
            // The render may have changed shape (new comments, new theme, new
            // width), so put the cursor back on the line it was on.
            self.resync_cursor();

            terminal.draw(|frame| crate::ui::draw(frame, self, diff_width))?;
            let last_draw = Instant::now();

            if self.watch.is_some() {
                self.watch_tick()?;
            } else {
                self.wait_for_event()?;
                // Cap the redraw rate: keep applying buffered input (so scroll
                // distance is preserved) until this frame's budget elapses or the
                // input goes quiet, then loop back to a single redraw. Without
                // this a fast wheel triggers a full-screen repaint per event and
                // the terminal can't keep up. Quitting/editor break out at once.
                while !self.quit && self.pending_editor.is_none() {
                    let remaining = FRAME_MIN.saturating_sub(last_draw.elapsed());
                    if remaining.is_zero() || !event::poll(remaining)? {
                        break;
                    }
                    self.handle_event()?;
                }
            }

            // Suspending the TUI to run an editor needs the owned terminal.
            if let Some(path) = self.pending_editor.take() {
                self.open_editor(terminal, &path)?;
                // The file may have changed; re-read its diff from git.
                self.refresh_file(&path);
            }
            if let Some(pending) = self.pending_comment.take() {
                self.compose_comment(terminal, pending)?;
            }
        }
        Ok(())
    }

    /// Interactive (non-watch) input wait. Normally blocks for the next event,
    /// but bounds the wait when something needs servicing without a keypress: a
    /// timed status that must expire, or an in-flight GitHub sync whose result
    /// should be surfaced. Both are handled after the wait returns.
    fn wait_for_event(&mut self) -> Result<()> {
        match self.idle_timeout() {
            Some(timeout) => {
                if event::poll(timeout)? {
                    self.handle_event()?;
                }
            }
            None => self.handle_event()?,
        }
        self.expire_status();
        self.drain_review_sync();
        self.poll_comments();
        Ok(())
    }

    /// How long the interactive wait may block, or `None` to block until a key.
    /// Bounded by a showing status's remaining lifetime and, while a sync is in
    /// flight, by [`SYNC_POLL`] — whichever is sooner — so both self-service.
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
        match (status, bound) {
            (Some(s), Some(b)) => Some(s.min(b)),
            (Some(s), None) => Some(s),
            (None, b) => b,
        }
    }

    /// One watch-mode iteration: wait briefly for input, then service any due
    /// reload. The bounded wait keeps filesystem changes responsive even when no
    /// key is pressed.
    fn watch_tick(&mut self) -> Result<()> {
        let timeout = self.watch.as_ref().expect("watch present").poll_timeout();
        if event::poll(timeout)? {
            self.handle_event()?;
        }
        self.expire_status();
        match self.watch.as_mut().expect("watch present").poll_reload() {
            Some(Ok(text)) => {
                let files = crate::diff::parse(&text);
                self.reload_files(files);
            }
            Some(Err(e)) => self.status = Some(format!("watch error: {e}")),
            None => {}
        }
        Ok(())
    }

    /// Swap in a freshly parsed file set (a watch refresh), rebuilding the tree
    /// while preserving the selected file by path where it still exists.
    fn reload_files(&mut self, files: Vec<FileDiff>) {
        let prev_path = self
            .selected_file()
            .map(|i| self.files[i].path().to_string());

        self.files = files;
        self.file_hashes = self
            .files
            .iter()
            .map(|f| crate::review::file_hash(&f.raw))
            .collect();
        self.nodes = tree::build(&self.files);
        self.rows = tree::flatten(&self.nodes, &self.collapsed);
        self.cache.clear();
        self.last_width = 0; // force a re-render at the next draw
        self.finder = None; // indices changed; a stale finder would mislead

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
    /// open show on return. Only meaningful in auto-diff mode — a piped diff has
    /// no git source to re-read — so it's a no-op otherwise. A file whose changes
    /// were fully reverted drops out of the tree.
    fn refresh_file(&mut self, path: &str) {
        let Some(auto) = &self.autodiff else { return };
        let (source, base) = (auto.source, auto.base.clone());
        // The immutable borrow of `self.autodiff` ends here (source/base owned),
        // freeing `self` for the mutable splice below.
        let text = match crate::autodiff::load_file(source, base.as_deref(), path) {
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
            (Some(i), Some(file)) => {
                self.file_hashes[i] = crate::review::file_hash(&file.raw);
                self.files[i] = file;
                self.cache.invalidate(i);
                self.last_width = 0; // force a re-render at the next draw
                self.publish_session();
            }
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

    fn diff_pane_width(&self, total: u16) -> u16 {
        let used = if self.show_tree { self.tree_width } else { 0 };
        total.saturating_sub(used).max(MIN_DIFF_WIDTH)
    }

    pub fn selected_index(&self) -> usize {
        self.tree_state.selected().unwrap_or(0)
    }

    /// The diff index of the selected row, if it is a file (not a directory).
    pub fn selected_file(&self) -> Option<usize> {
        match self.rows.get(self.selected_index())?.kind {
            RowKind::File { diff_index } => Some(diff_index),
            RowKind::Dir { .. } => None,
        }
    }

    /// The view mode actually used to render `idx`. Added files always render
    /// unified: side-by-side would just show an empty left pane and waste the
    /// scarce horizontal space, so they ignore the global toggle.
    pub fn side_by_side_for(&self, idx: usize) -> bool {
        self.side_by_side && self.files[idx].status != FileStatus::Added
    }

    pub fn totals(&self) -> (u32, u32) {
        self.files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions))
    }

    fn select(&mut self, index: usize) {
        if index != self.selected_index() {
            self.tree_state.select(Some(index));
            self.diff_scroll = 0;
            self.reset_cursor();
        }
    }

    /// Send the line cursor back to the top. Called whenever the diff pane starts
    /// showing a different file, where a carried-over line index would be
    /// meaningless and the remembered anchor belongs to the old file.
    fn reset_cursor(&mut self) {
        self.diff_cursor = 0;
        self.cursor_anchor = None;
        self.cursor_token = None;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.select((self.selected_index() as isize + delta).clamp(0, max) as usize);
    }

    /// Jump to the next/previous file row, skipping directories.
    fn jump_file(&mut self, forward: bool) {
        let cur = self.selected_index();
        let is_file = |i: &usize| matches!(self.rows[*i].kind, RowKind::File { .. });
        let next = if forward {
            (cur + 1..self.rows.len()).find(is_file)
        } else {
            (0..cur).rev().find(is_file)
        };
        if let Some(i) = next {
            self.select(i);
        }
    }

    fn scroll_diff(&mut self, delta: i32) {
        self.diff_scroll = (self.diff_scroll as i32 + delta).max(0) as u16;
        // Scrolling drags the cursor along so it never drifts off-screen; the
        // cursor stays wherever it already is when that's still visible.
        self.park_cursor_in_view();
    }

    /// The render backing the current view, if it's already cached.
    fn current_render(&self) -> Option<&crate::delta::Rendered> {
        let idx = self.selected_file()?;
        self.cache.get(
            idx,
            self.last_width,
            self.side_by_side_for(idx),
            self.diff_theme,
        )
    }

    /// Re-resolve the cursor's line index after the render it pointed into was
    /// rebuilt — a comment spliced in above it, a theme or width change, a switch
    /// between unified and side-by-side. Anchoring to a diff line rather than a
    /// row is what makes this recoverable at all.
    fn resync_cursor(&mut self) {
        let Some(idx) = self.selected_file() else {
            return;
        };
        let token = (
            idx,
            self.last_width,
            self.side_by_side_for(idx),
            self.diff_theme,
            self.comment_rev,
        );
        if self.cursor_token == Some(token) {
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
            // line rather than delta's file-header decoration, so `c` works
            // straight away instead of reporting there's nothing to comment on.
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
    fn remember_cursor_anchor(&mut self) {
        if let Some(anchor) = self
            .current_render()
            .and_then(|r| r.line_map.get(self.diff_cursor).anchor())
        {
            self.cursor_anchor = Some(anchor);
        }
    }

    /// Move the line cursor by `delta` lines and scroll to keep it in view.
    fn move_cursor(&mut self, delta: i32) {
        let Some(max) = self.current_render().map(|r| r.lines().saturating_sub(1)) else {
            return;
        };
        self.diff_cursor = (self.diff_cursor as i32 + delta).clamp(0, max as i32) as usize;
        self.remember_cursor_anchor();
        self.scroll_cursor_into_view();
    }

    /// Put the cursor on a specific line and bring it on screen.
    fn set_cursor(&mut self, line: usize) {
        let Some(max) = self.current_render().map(|r| r.lines().saturating_sub(1)) else {
            return;
        };
        self.diff_cursor = line.min(max);
        self.remember_cursor_anchor();
        self.scroll_cursor_into_view();
    }

    /// Scroll the minimum needed to keep the cursor line — all of it, when it
    /// wraps onto several rows — inside the viewport, with [`CURSOR_PADDING`]
    /// rows of lead so the cursor never sits flush against an edge.
    fn scroll_cursor_into_view(&mut self) {
        let (Some(render), height) = (self.current_render(), self.diff_height) else {
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
        } else if bottom + pad > scroll + height {
            self.diff_scroll = (bottom + pad).saturating_sub(height);
        }
    }

    /// After a scroll or page jump, pull the cursor to the nearest visible line
    /// rather than letting it sit off-screen.
    fn park_cursor_in_view(&mut self) {
        let (Some(render), height) = (self.current_render(), self.diff_height) else {
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

    /// The rows the line cursor occupies inside the diff pane — `(top, bottom)`,
    /// bottom exclusive, counted from the pane's top edge — or `None` when it's
    /// off-screen or nothing is rendered yet. Lets the composer open beside the
    /// line it will hang on.
    pub fn cursor_rows(&self) -> Option<(u16, u16)> {
        let render = self.current_render()?;
        let top = render.row_of(self.diff_cursor);
        let bottom = render.row_of(self.diff_cursor + 1).max(top + 1);
        let top = top.checked_sub(self.diff_scroll)?;
        let bottom = bottom.saturating_sub(self.diff_scroll);
        (top < self.diff_height).then_some((top, bottom))
    }

    /// One PageUp/PageDown step: the diff viewport height less a line of overlap,
    /// so a line of context carries across the jump. At least one line.
    fn page(&self) -> i32 {
        i32::from(self.diff_height.saturating_sub(1)).max(1)
    }

    /// Page through the focused pane — scroll the diff, or jump the tree
    /// selection, by roughly one screenful.
    fn page_move(&mut self, down: bool) {
        let delta = if down { self.page() } else { -self.page() };
        if self.focus == Focus::Tree {
            self.move_selection(delta as isize);
        } else {
            self.scroll_diff(delta);
        }
    }

    /// The spliced comment block the cursor is inside, if any.
    fn block_at_cursor(&self) -> Option<&crate::comment::CommentBlock> {
        let cursor = self.diff_cursor;
        self.current_render()?
            .comment_rows
            .iter()
            .find(|b| cursor >= b.start && cursor < b.start + b.len)
    }

    /// Jump the cursor to the next or previous comment in this file, wrapping
    /// around. Reports when there's nothing to jump to.
    pub(crate) fn jump_comment(&mut self, forward: bool) {
        let Some(render) = self.current_render() else {
            return;
        };
        let starts: Vec<usize> = render.comment_rows.iter().map(|b| b.start).collect();
        if starts.is_empty() {
            self.set_status("No comments in this file");
            return;
        }
        let cursor = self.diff_cursor;
        let target = if forward {
            starts.iter().find(|&&s| s > cursor).copied()
        } else {
            starts.iter().rev().find(|&&s| s < cursor).copied()
        };
        // Wrap: past the last comment go back to the first, and vice versa.
        let target = target.unwrap_or(if forward {
            starts[0]
        } else {
            starts[starts.len() - 1]
        });
        self.set_cursor(target);
    }

    /// Start composing a note on whatever the cursor is over: a comment on the
    /// diff line, or — when the cursor is inside a thread, which is exactly where
    /// `]` parks it — a reply to the comment it's sitting on. What the cursor is
    /// on already says which was meant, so there's no second key for it.
    ///
    /// The note is typed into the composer drawn over the diff pane; `$EDITOR` is
    /// a `Ctrl-O` away from there.
    fn start_comment(&mut self) {
        let Some(idx) = self.selected_file() else {
            self.set_status("No file selected to comment on");
            return;
        };
        // A reply inherits the thread's anchor, so it lands beside the same code.
        let in_thread = self.block_at_cursor().map(|block| {
            (
                block.anchor,
                block.comment_at(self.diff_cursor).map(str::to_string),
            )
        });
        let (anchor, reply_to) = match in_thread {
            Some(threaded) => threaded,
            None => match self
                .current_render()
                .and_then(|r| r.line_map.get(self.diff_cursor).anchor())
            {
                Some(anchor) => (anchor, None),
                None => {
                    self.set_status("Put the cursor on a diff line to comment on it");
                    return;
                }
            },
        };
        self.composer = Some(Composer::new(PendingComment {
            file: self.files[idx].path().to_string(),
            anchor,
            reply_to,
            diff_hash: self.file_hashes.get(idx).copied().unwrap_or(0),
            context: self.cursor_context(),
            draft: String::new(),
        }));
    }

    /// The composer, for the draw path.
    pub fn composer(&self) -> Option<&Composer> {
        self.composer.as_ref()
    }

    /// Store what's in the composer and close it. An empty body aborts, exactly
    /// like an empty commit message.
    fn finish_comment(&mut self) {
        let Some(composer) = self.composer.take() else {
            return;
        };
        let body = composer.body();
        self.save_composed(composer.pending, body);
    }

    /// Abandon the note being typed.
    fn cancel_comment(&mut self) {
        if self.composer.take().is_some() {
            self.set_status("Comment discarded");
        }
    }

    /// `Ctrl-O` from the composer: reopen the same note in `$EDITOR`, carrying
    /// whatever has been typed so far. The editor runs once the event loop
    /// regains the terminal, next to the `o` key's editor handling.
    fn comment_to_editor(&mut self) {
        if let Some(composer) = self.composer.take() {
            self.pending_comment = Some(composer.into_draft());
        }
    }

    /// Route a keypress into the open composer. It owns all input while it's up,
    /// so every key either edits the body or ends the note.
    fn composer_key(&mut self, key: crossterm::event::KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // Enter breaks the line, so saving needs a key of its own. Alt-Enter
            // is the same key for terminals that report the modifier, and reads
            // as "done" to anyone who never finds Ctrl-S.
            KeyCode::Char('s') if ctrl => self.finish_comment(),
            KeyCode::Enter if alt || ctrl => self.finish_comment(),
            KeyCode::Esc => self.cancel_comment(),
            KeyCode::Char('c') if ctrl => self.cancel_comment(),
            KeyCode::Char('o') if ctrl => self.comment_to_editor(),
            _ => {
                let Some(c) = self.composer.as_mut() else {
                    return;
                };
                match key.code {
                    KeyCode::Enter => c.newline(),
                    KeyCode::Backspace => c.backspace(),
                    KeyCode::Delete => c.delete(),
                    KeyCode::Left => c.left(),
                    KeyCode::Right => c.right(),
                    KeyCode::Up => c.up(),
                    KeyCode::Down => c.down(),
                    KeyCode::Home => c.home(),
                    KeyCode::End => c.end(),
                    KeyCode::Char('a') if ctrl => c.home(),
                    KeyCode::Char('e') if ctrl => c.end(),
                    KeyCode::Char('u') if ctrl => c.delete_to_start(),
                    KeyCode::Char('w') if ctrl => c.delete_word(),
                    KeyCode::Char(ch) if !ctrl => c.insert(ch),
                    _ => {}
                }
            }
        }
    }

    /// A few rendered lines around the cursor, as plain text, to quote back in the
    /// editor so the author can see what they're commenting on.
    fn cursor_context(&self) -> Vec<String> {
        let Some(render) = self.current_render() else {
            return Vec::new();
        };
        let first = self.diff_cursor.saturating_sub(2);
        let last = (self.diff_cursor + 3).min(render.text.lines.len());
        render.text.lines[first..last]
            .iter()
            .map(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.trim_end().to_string()
            })
            .collect()
    }

    /// Delete the comment the cursor is on (and any replies beneath it).
    fn delete_comment(&mut self) {
        let Some(id) = self
            .block_at_cursor()
            .and_then(|b| b.comment_at(self.diff_cursor))
            .map(str::to_string)
        else {
            self.set_status("Put the cursor on a comment to delete it");
            return;
        };
        let removed = self.comments.remove(&id);
        self.comments.save();
        self.bump_comments();
        self.set_status(match removed {
            0 => "Nothing to delete".to_string(),
            1 => format!("Deleted comment #{id}"),
            n => format!("Deleted comment #{id} and {} repl(ies)", n - 1),
        });
    }

    /// Turn the text an author typed into a stored comment. Empty means abort,
    /// exactly like an empty commit message.
    fn save_composed(&mut self, pending: PendingComment, body: String) {
        let body = body.trim();
        if body.is_empty() {
            self.set_status("Empty comment — nothing saved");
            return;
        }
        let id = self.comments.add(Comment {
            id: String::new(),
            file: pending.file.clone(),
            side: pending.anchor.side,
            line: pending.anchor.line,
            body: body.to_string(),
            author: self.comment_author.clone(),
            created: crate::state::now_unix(),
            reply_to: pending.reply_to,
            diff_hash: Some(format!("{:032x}", pending.diff_hash)),
        });
        self.comments.save();
        self.bump_comments();
        self.set_status(format!(
            "💬 Commented on {}:{}  (#{id})",
            pending.file, pending.anchor.line
        ));
    }

    /// Expand/collapse the selected directory and re-flatten the visible rows.
    fn toggle_fold(&mut self) {
        let path = match self.rows.get(self.selected_index()) {
            Some(Row {
                kind: RowKind::Dir { path, .. },
                ..
            }) => path.clone(),
            _ => return,
        };
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path);
        }
        let sel = self.selected_index();
        self.rows = tree::flatten(&self.nodes, &self.collapsed);
        self.tree_state
            .select(Some(sel.min(self.rows.len().saturating_sub(1))));
    }

    fn open_finder(&mut self) {
        self.finder = Some(Finder {
            query: String::new(),
            matches: (0..self.files.len()).collect(),
            selected: 0,
        });
    }

    /// Recompute finder matches after the query changes.
    fn finder_recompute(&mut self) {
        let query = match &self.finder {
            Some(f) => f.query.clone(),
            None => return,
        };
        let matches: Vec<usize> = if query.is_empty() {
            (0..self.files.len()).collect()
        } else {
            let mut scored: Vec<(i64, usize)> = self
                .files
                .iter()
                .enumerate()
                .filter_map(|(i, f)| self.matcher.fuzzy_match(f.path(), &query).map(|s| (s, i)))
                .collect();
            scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
            scored.into_iter().map(|(_, i)| i).collect()
        };
        if let Some(f) = self.finder.as_mut() {
            f.selected = f.selected.min(matches.len().saturating_sub(1));
            f.matches = matches;
        }
    }

    /// Select a file by diff index, expanding any collapsed ancestor folders so
    /// its row is visible. Used when jumping from the finder.
    fn reveal_file(&mut self, diff_index: usize) {
        let path = self.files[diff_index].path().to_string();
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let mut acc = String::new();
        for part in &parts[..parts.len().saturating_sub(1)] {
            acc = if acc.is_empty() {
                part.to_string()
            } else {
                format!("{acc}/{part}")
            };
            self.collapsed.remove(&acc);
        }
        self.rows = tree::flatten(&self.nodes, &self.collapsed);
        if let Some(i) = self
            .rows
            .iter()
            .position(|r| matches!(r.kind, RowKind::File { diff_index: d } if d == diff_index))
        {
            self.tree_state.select(Some(i));
            self.diff_scroll = 0;
            self.reset_cursor();
        }
    }

    /// Cycle the diff color theme and report it. The render cache is keyed by
    /// theme, so the next draw re-renders (and caches) the new look; cycling back
    /// to a theme already seen is instant.
    fn cycle_theme(&mut self) {
        self.diff_theme = self.diff_theme.next();
        self.set_status(format!("Diff theme: {}", self.diff_theme.name()));
    }

    fn copy_path(&mut self) {
        let Some(idx) = self.selected_file() else {
            self.status = Some("No file selected to copy".into());
            return;
        };
        let path = self.files[idx].path().to_string();
        self.status = Some(
            match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(path.clone())) {
                Ok(()) => format!("Copied {path}"),
                Err(e) => format!("Copy failed: {e}"),
            },
        );
    }

    /// Suspend the TUI, run `$VISUAL`/`$EDITOR` on `path`, then resume.
    ///
    /// Returns the editor's name and how it exited, so callers can decide what to
    /// report — opening a file just says so, while composing a comment only reads
    /// the buffer back on success.
    fn run_editor(
        &mut self,
        terminal: &mut DefaultTerminal,
        path: &str,
    ) -> (String, std::io::Result<std::process::ExitStatus>) {
        disable_mouse();
        ratatui::restore();
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        let mut cmd = Command::new(&editor);
        cmd.arg(path);
        // A piped diff (`git diff | riffnav`) leaves our stdin a spent pipe; the
        // editor would inherit it and complain that input isn't a terminal. The
        // TUI already talks to /dev/tty, so hand the editor the same.
        if !std::io::stdin().is_terminal()
            && let Ok(tty) = std::fs::File::open("/dev/tty")
        {
            cmd.stdin(Stdio::from(tty));
        }
        let status = cmd.status();

        *terminal = ratatui::init();
        enable_mouse();
        let _ = terminal.clear();
        self.last_width = 0; // force a re-render into the fresh screen
        (editor, status)
    }

    /// The `o` key: edit the selected file in place.
    fn open_editor(&mut self, terminal: &mut DefaultTerminal, path: &str) -> Result<()> {
        let (editor, status) = self.run_editor(terminal, path);
        self.status = Some(match status {
            Ok(s) if s.success() => format!("Edited {path}"),
            Ok(s) => format!("{editor} exited: {s}"),
            Err(e) => format!("Couldn't launch {editor}: {e}"),
        });
        Ok(())
    }

    /// Compose a comment body in `$EDITOR`, git-commit style.
    ///
    /// The buffer opens with an empty first line and everything explanatory below
    /// a scissors marker, so a body containing `#` — a markdown heading, a shell
    /// snippet — survives verbatim. That's why this doesn't use git's plain `#`
    /// comment convention.
    fn compose_comment(
        &mut self,
        terminal: &mut DefaultTerminal,
        pending: PendingComment,
    ) -> Result<()> {
        let path = std::env::temp_dir().join(format!("riffnav-comment-{}.md", std::process::id()));
        let template = comment_template(&pending);
        if let Err(e) = std::fs::write(&path, template) {
            self.set_status(format!("Couldn't open a comment buffer: {e}"));
            return Ok(());
        }

        let (editor, status) = self.run_editor(terminal, &path.to_string_lossy());
        match status {
            Ok(s) if s.success() => {
                let typed = std::fs::read_to_string(&path).unwrap_or_default();
                self.save_composed(pending, strip_scissors(&typed));
            }
            Ok(s) => self.set_status(format!("{editor} exited: {s} — comment discarded")),
            Err(e) => self.set_status(format!("Couldn't launch {editor}: {e}")),
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    /// Route a mouse event to the pane under the cursor. Overlays own the whole
    /// screen, so clicks and scrolls beneath them are ignored.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.finder.is_some() || self.show_help || self.composer.is_some() {
            return;
        }
        let pos = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.on_divider(pos) {
                    self.dragging_divider = true;
                } else {
                    self.click(pos);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_divider => {
                self.drag_divider(pos.x);
            }
            MouseEventKind::Up(_) => self.dragging_divider = false,
            MouseEventKind::ScrollDown => self.scroll_at(pos, 1),
            MouseEventKind::ScrollUp => self.scroll_at(pos, -1),
            _ => {}
        }
    }

    /// Whether `pos` sits on the tree pane's right border, which doubles as the
    /// drag handle for resizing the panes.
    fn on_divider(&self, pos: Position) -> bool {
        self.tree_area
            .is_some_and(|a| a.contains(pos) && pos.x == a.right().saturating_sub(1))
    }

    /// Move the divider to screen column `x`: the tree grows or shrinks so its
    /// border lands under the cursor, clamped between the configured width (the
    /// floor) and whatever leaves the diff its minimum width.
    fn drag_divider(&mut self, x: u16) {
        let (Some(tree), Some(diff)) = (self.tree_area, self.diff_area) else {
            return;
        };
        let total = tree.width + diff.width;
        let max = total
            .saturating_sub(MIN_DIFF_WIDTH)
            .max(self.tree_width_min);
        self.tree_width = (x.saturating_sub(tree.x) + 1).clamp(self.tree_width_min, max);
    }

    /// Left-click: select the tree row under the cursor (folding/unfolding a
    /// directory, like a file explorer), or just move focus to the diff pane.
    fn click(&mut self, pos: Position) {
        if let Some(area) = self.tree_area
            && area.contains(pos)
        {
            // The list has no top border, so its first visible row sits at the
            // pane's top edge; add the scroll offset to map a screen row to a
            // row index.
            let line = (pos.y - area.y) as usize + self.tree_state.offset();
            if line < self.rows.len() {
                self.focus = Focus::Tree;
                self.select(line);
                if matches!(self.rows[line].kind, RowKind::Dir { .. }) {
                    self.toggle_fold();
                }
            }
        } else if let Some(area) = self.diff_area
            && area.contains(pos)
        {
            self.focus = Focus::Diff;
        }
    }

    /// Wheel scrolling acts on whichever pane the cursor is over, independent of
    /// keyboard focus: the tree moves its selection, the diff scrolls.
    fn scroll_at(&mut self, pos: Position, dir: i32) {
        if self.tree_area.is_some_and(|a| a.contains(pos)) {
            self.move_selection(dir as isize);
        } else {
            self.scroll_diff(dir * 3);
        }
    }

    fn handle_event(&mut self) -> Result<()> {
        let mut ev = event::read()?;
        // Coalesce a burst of resize events (e.g. a drag) into the last one.
        while matches!(ev, Event::Resize(..)) && event::poll(Duration::ZERO)? {
            ev = event::read()?;
        }
        if let Event::Mouse(mouse) = ev {
            self.handle_mouse(mouse);
            return Ok(());
        }
        let Event::Key(key) = ev else {
            return Ok(());
        };
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        self.dispatch_key(key)
    }

    /// Act on one keypress, routing it to whatever currently owns input: the
    /// composer, the finder, the help overlay, or the panes themselves.
    fn dispatch_key(&mut self, key: crossterm::event::KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // A note being typed captures all input, printable keys included.
        if self.composer.is_some() {
            self.composer_key(key);
            return Ok(());
        }

        // The fuzzy finder captures all input while open.
        if self.finder.is_some() {
            match key.code {
                KeyCode::Esc => self.finder = None,
                KeyCode::Enter => {
                    let target = self
                        .finder
                        .as_ref()
                        .and_then(|f| f.matches.get(f.selected).copied());
                    self.finder = None;
                    if let Some(idx) = target {
                        self.reveal_file(idx);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(f) = self.finder.as_mut() {
                        f.query.pop();
                    }
                    self.finder_recompute();
                }
                KeyCode::Up => {
                    if let Some(f) = self.finder.as_mut() {
                        f.selected = f.selected.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    if let Some(f) = self.finder.as_mut()
                        && f.selected + 1 < f.matches.len()
                    {
                        f.selected += 1;
                    }
                }
                KeyCode::Char('p') if ctrl => {
                    if let Some(f) = self.finder.as_mut() {
                        f.selected = f.selected.saturating_sub(1);
                    }
                }
                KeyCode::Char('n') if ctrl => {
                    if let Some(f) = self.finder.as_mut()
                        && f.selected + 1 < f.matches.len()
                    {
                        f.selected += 1;
                    }
                }
                KeyCode::Char(c) if !ctrl => {
                    if let Some(f) = self.finder.as_mut() {
                        f.query.push(c);
                    }
                    self.finder_recompute();
                }
                _ => {}
            }
            return Ok(());
        }

        // The help overlay swallows all input until dismissed.
        if self.show_help {
            if matches!(
                key.code,
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')
            ) {
                self.show_help = false;
            }
            return Ok(());
        }

        self.clear_status();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            // In the diff pane j/k move the line cursor (scrolling the pane once
            // it nears an edge), matching how the tree moves its selection. With
            // comments off there's no cursor, so they scroll as they always did.
            KeyCode::Char('j') | KeyCode::Down => match (self.focus, self.comments_on) {
                (Focus::Tree, _) => self.move_selection(1),
                (Focus::Diff, true) => self.move_cursor(1),
                (Focus::Diff, false) => self.scroll_diff(1),
            },
            KeyCode::Char('k') | KeyCode::Up => match (self.focus, self.comments_on) {
                (Focus::Tree, _) => self.move_selection(-1),
                (Focus::Diff, true) => self.move_cursor(-1),
                (Focus::Diff, false) => self.scroll_diff(-1),
            },
            KeyCode::Char('n') => self.jump_file(true),
            KeyCode::Char('p') | KeyCode::Char('N') => self.jump_file(false),
            KeyCode::Char('d') if ctrl => self.scroll_diff(HALF_PAGE),
            KeyCode::Char('u') if ctrl => self.scroll_diff(-HALF_PAGE),
            KeyCode::PageDown => self.page_move(true),
            KeyCode::PageUp => self.page_move(false),
            KeyCode::Char('g') => {
                self.diff_scroll = 0;
                self.set_cursor(0);
            }
            KeyCode::Char('G') => {
                self.diff_scroll = u16::MAX; // clamped on draw
                let last = self.current_render().map(|r| r.lines().saturating_sub(1));
                if let Some(last) = last {
                    self.diff_cursor = last;
                    self.remember_cursor_anchor();
                }
            }
            KeyCode::Enter => self.toggle_fold(),
            // less-style paging of the diff: Space forward, b back. Diff-focused
            // only — in the tree, Enter folds and paging the selection with Space
            // would surprise.
            KeyCode::Char(' ') if self.focus == Focus::Diff => self.page_move(true),
            KeyCode::Char('b') if self.focus == Focus::Diff => self.page_move(false),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Tree => Focus::Diff,
                    Focus::Diff => Focus::Tree,
                }
            }
            KeyCode::Left => {
                if self.show_tree {
                    self.focus = Focus::Tree;
                }
            }
            KeyCode::Right => self.focus = Focus::Diff,
            KeyCode::Char('s') => self.side_by_side = !self.side_by_side,
            KeyCode::Char('e') => {
                self.show_tree = !self.show_tree;
                if !self.show_tree {
                    self.focus = Focus::Diff;
                }
            }
            KeyCode::Char('t') | KeyCode::Char('/') => self.open_finder(),
            KeyCode::Char('i') => {
                self.icon_style = self.icon_style.next();
                self.status = Some(format!("Icons: {}", self.icon_style.name()));
            }
            KeyCode::Char('T') => self.cycle_theme(),
            KeyCode::Char('y') => self.copy_path(),
            KeyCode::Char('v') => self.toggle_viewed(),
            KeyCode::Char('V') => self.jump_unviewed(),
            // Only bound on a bare launch (auto-diff mode); inert otherwise.
            KeyCode::Char('d') if self.autodiff.is_some() => self.cycle_diff_source(),
            // Only bound where the diff can be re-read; a piped one can't be.
            KeyCode::Char('r') if self.can_refresh() => self.refresh_diff(),
            // Only bound inside herdr; an inert no-op elsewhere.
            KeyCode::Char('z') if self.herdr.is_some() => self.toggle_herdr_zoom(),
            // Only bound when a supported forge (e.g. GitHub) is detected.
            KeyCode::Char('W') if self.forge.is_some() => self.open_web_diff(),
            KeyCode::Char('o') => {
                if let Some(idx) = self.selected_file() {
                    self.pending_editor = Some(self.files[idx].path().to_string());
                }
            }
            // Comment keys, bound only when comments are on — like `d`/`z`/`W`,
            // they're inert rather than misleading when the feature is off.
            KeyCode::Char('c') if self.comments_on => self.start_comment(),
            KeyCode::Char('x') if self.comments_on => self.delete_comment(),
            KeyCode::Char(']') if self.comments_on => self.jump_comment(true),
            KeyCode::Char('[') if self.comments_on => self.jump_comment(false),
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
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
        let _ = self.dispatch_key(crossterm::event::KeyEvent::new(code, mods));
    }

    /// Seed the render cache for the selected file, splicing in whatever the
    /// installed store holds. Tests run without delta on PATH, so this stands in
    /// for the `RenderCache::ensure` the event loop would do.
    pub(crate) fn seed_render_for_test(&mut self, width: u16, text: ratatui::text::Text<'static>) {
        let idx = self.selected_file().expect("a file is selected");
        let layer = Self::comment_layer(
            &self.files,
            &self.comments,
            0,
            self.comment_rev,
            self.comments_on,
            idx,
        );
        self.cache
            .insert_for_test_with_comments(idx, width, false, self.diff_theme, text, &layer);
        self.last_width = width;
        self.cursor_token = None; // the render is new; let the cursor re-resolve
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::FileStatus;

    fn file(path: &str) -> FileDiff {
        FileDiff {
            old_path: None,
            new_path: Some(path.to_string()),
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
            raw: String::new(),
        }
    }

    fn app_with(files: Vec<FileDiff>) -> App {
        App::new(files, false, false, &Config::default())
    }

    /// Like `file`, but with a distinct `raw` so each file hashes differently —
    /// the viewed state is keyed on diff content, and the bare `file` helper
    /// leaves `raw` empty (all-identical hashes).
    fn file_with_raw(path: &str) -> FileDiff {
        FileDiff {
            raw: format!("diff --git a/{path} b/{path}\n@@ -1 +1 @@\n-old\n+new\n"),
            ..file(path)
        }
    }

    /// An app with auto-advance off, so `toggle_viewed` exercises only the
    /// mark/unmark logic without moving the selection out from under the test.
    fn app_no_advance(files: Vec<FileDiff>) -> App {
        let cfg = Config {
            review_auto_advance: false,
            ..Config::default()
        };
        App::new(files, false, false, &cfg)
    }

    #[test]
    fn toggle_viewed_marks_only_selected_and_counts() {
        let mut app = app_no_advance(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        assert_eq!(app.viewed_count(), 0);

        // The first file is selected on launch; mark it viewed.
        let first = app.selected_file().unwrap();
        app.toggle_viewed();
        assert_eq!(app.viewed_count(), 1);
        assert!(app.is_viewed(first));
        // The other file is untouched — content-keyed, not position-keyed.
        assert!(!app.is_viewed(if first == 0 { 1 } else { 0 }));

        // Toggling again clears it (selection didn't move: auto-advance off).
        app.toggle_viewed();
        assert_eq!(app.viewed_count(), 0);
        assert!(!app.is_viewed(first));
    }

    #[test]
    fn divider_drag_resizes_within_bounds() {
        let mut app = app_with(vec![file("a.rs")]);
        // Default config: tree is 32 wide, so its border sits on column 31.
        app.tree_area = Some(Rect::new(0, 0, 32, 10));
        app.diff_area = Some(Rect::new(32, 1, 68, 9));

        assert!(app.on_divider(Position::new(31, 4)));
        assert!(!app.on_divider(Position::new(30, 4)));
        assert!(!app.on_divider(Position::new(32, 4)));

        // Dragging right widens the tree so the border follows the cursor.
        app.drag_divider(50);
        assert_eq!(app.tree_width, 51);
        // The starting width is the floor…
        app.drag_divider(5);
        assert_eq!(app.tree_width, 32);
        // …and the diff keeps its minimum width at the other extreme.
        app.drag_divider(99);
        assert_eq!(app.tree_width, 100 - MIN_DIFF_WIDTH);
    }

    #[test]
    fn click_selects_the_file_row_under_the_cursor() {
        let mut app = app_with(vec![file("a.rs"), file("b.rs"), file("c.rs")]);
        app.tree_area = Some(Rect::new(0, 0, 30, 10));

        // The three files flatten to rows 0..3; a click on the second screen
        // row selects that row and pulls focus to the tree.
        app.click(Position::new(4, 1));
        assert_eq!(app.selected_index(), 1);
        assert_eq!(app.focus, Focus::Tree);
        assert!(app.selected_file().is_some());
    }

    #[test]
    fn click_honors_the_list_scroll_offset() {
        let mut app = app_with(vec![
            file("a.rs"),
            file("b.rs"),
            file("c.rs"),
            file("d.rs"),
            file("e.rs"),
        ]);
        app.tree_area = Some(Rect::new(0, 0, 30, 3));
        // The list is scrolled so row 2 is at the top of the pane.
        *app.tree_state.offset_mut() = 2;

        // Second visible screen row -> rows[2 + 1].
        app.click(Position::new(4, 1));
        assert_eq!(app.selected_index(), 3);
    }

    #[test]
    fn click_on_a_directory_toggles_its_fold() {
        // open_depth defaults to 64, so `dir/` starts expanded: rows are
        // [dir, a.rs, b.rs].
        let mut app = app_with(vec![file("dir/a.rs"), file("dir/b.rs")]);
        app.tree_area = Some(Rect::new(0, 0, 30, 10));
        assert_eq!(app.rows.len(), 3);

        // Clicking the directory row collapses it, hiding its children.
        app.click(Position::new(2, 0));
        assert_eq!(app.selected_index(), 0);
        assert_eq!(app.rows.len(), 1);

        // Clicking it again expands it back.
        app.click(Position::new(2, 0));
        assert_eq!(app.rows.len(), 3);
    }

    #[test]
    fn click_below_the_last_row_is_ignored() {
        let mut app = app_with(vec![file("a.rs")]);
        app.tree_area = Some(Rect::new(0, 0, 30, 10));
        app.focus = Focus::Diff;

        // Empty space well past the single row: nothing selected, focus stays.
        app.click(Position::new(4, 7));
        assert_eq!(app.selected_index(), 0);
        assert_eq!(app.focus, Focus::Diff);
    }

    #[test]
    fn click_in_the_diff_pane_focuses_it() {
        let mut app = app_with(vec![file("a.rs")]);
        app.tree_area = Some(Rect::new(0, 0, 30, 10));
        app.diff_area = Some(Rect::new(30, 1, 50, 9));
        app.focus = Focus::Tree;

        app.click(Position::new(40, 4));
        assert_eq!(app.focus, Focus::Diff);
    }

    #[test]
    fn syncs_viewed_marks_only_in_committed_view_when_armed() {
        let mut app = app_with(vec![file("a.rs")]);
        // Nothing armed and no auto-diff (e.g. a piped diff): purely local.
        assert!(!app.syncs_viewed_marks());

        // Arm sync, but a working-tree view doesn't mirror the PR: still local.
        app.review_sync = Some(ReviewSync::new());
        app.enable_autodiff(DiffSource::AllUncommitted, Some("origin/main".into()));
        assert!(!app.syncs_viewed_marks());

        // Branch-vs-base view with sync armed: this is the PR view, so it syncs.
        app.enable_autodiff(DiffSource::Committed, Some("origin/main".into()));
        assert!(app.syncs_viewed_marks());

        // Armed but not in auto-diff mode at all: nothing to sync against.
        app.autodiff = None;
        assert!(!app.syncs_viewed_marks());
    }

    #[test]
    fn added_files_render_unified_even_in_side_by_side() {
        let added = FileDiff {
            status: FileStatus::Added,
            ..file("new.rs")
        };
        // side_by_side enabled globally; only the added file overrides it.
        let app = App::new(vec![file("mod.rs"), added], true, false, &Config::default());
        assert!(app.side_by_side_for(0), "modified file honors the toggle");
        assert!(!app.side_by_side_for(1), "added file forces unified");
    }

    #[test]
    fn jump_unviewed_skips_viewed_files() {
        let mut app = app_no_advance(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        // Mark the selected (first) file viewed, then jump: lands on the other.
        app.toggle_viewed();
        app.jump_unviewed();
        assert!(!app.is_viewed(app.selected_file().unwrap()));

        // With everything viewed, the selection holds where it is.
        app.toggle_viewed();
        let before = app.selected_index();
        app.jump_unviewed();
        assert_eq!(app.selected_index(), before);
    }

    #[test]
    fn marking_viewed_auto_advances_to_next_unviewed() {
        // Default config has auto-advance on.
        let mut app = app_with(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        let first = app.selected_file().unwrap();
        app.toggle_viewed();
        // Selection moved off the just-viewed file to the remaining unviewed one.
        let now = app.selected_file().unwrap();
        assert_ne!(now, first);
        assert!(!app.is_viewed(now));

        // Marking the last file leaves the selection put (nothing left to go to).
        app.toggle_viewed();
        let before = app.selected_index();
        app.toggle_viewed(); // unmarking never advances either
        assert_eq!(app.selected_index(), before);
    }

    #[test]
    fn startup_opens_on_first_unviewed_file() {
        let mut app = app_no_advance(vec![
            file_with_raw("a.rs"),
            file_with_raw("b.rs"),
            file_with_raw("c.rs"),
        ]);
        // Fresh: selection sits on the first file.
        assert_eq!(app.selected_file(), Some(0));

        // Mark a.rs viewed (auto-advance off keeps the cursor put), then re-run
        // the startup selection: it skips the viewed file and lands on b.rs.
        app.toggle_viewed();
        app.select_first_unviewed();
        assert_eq!(
            app.selected_file().map(|i| app.files[i].path()),
            Some("b.rs")
        );
    }

    #[test]
    fn startup_holds_on_first_file_when_all_viewed() {
        let mut app = app_no_advance(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        // Mark both files viewed.
        app.toggle_viewed();
        app.jump_unviewed();
        app.toggle_viewed();

        // Back to the top, then run the startup selection: nothing is unviewed,
        // so it holds on the first file rather than jumping.
        app.tree_state.select(Some(0));
        app.select_first_unviewed();
        assert_eq!(app.selected_file(), Some(0));
    }

    #[test]
    fn finder_empty_query_lists_all_files() {
        let mut app = app_with(vec![file("a.rs"), file("b.rs")]);
        app.open_finder();
        app.finder_recompute();
        assert_eq!(app.finder.as_ref().unwrap().matches.len(), 2);
    }

    #[test]
    fn finder_ranks_best_match_first() {
        let files = vec![
            file("src/main.rs"),
            file("src/diff/parser.rs"),
            file("README.md"),
        ];
        let mut app = app_with(files);
        app.open_finder();
        for c in "parser".chars() {
            app.finder.as_mut().unwrap().query.push(c);
        }
        app.finder_recompute();
        let best = app.finder.as_ref().unwrap().matches[0];
        assert_eq!(app.files[best].path(), "src/diff/parser.rs");
    }

    #[test]
    fn reveal_file_expands_collapsed_ancestors() {
        let files = vec![file("src/diff/parser.rs")];
        let mut app = app_with(files);
        app.collapsed.insert("src".to_string());
        app.collapsed.insert("src/diff".to_string());
        app.rows = tree::flatten(&app.nodes, &app.collapsed);
        app.reveal_file(0);
        // The file's row is now visible and selected.
        assert!(matches!(
            app.rows[app.selected_index()].kind,
            RowKind::File { diff_index: 0 }
        ));
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

    #[test]
    fn open_depth_collapses_deep_folders() {
        // open_depth = 1: root dirs open, their subdirs collapsed.
        let cfg = Config {
            open_depth: 1,
            ..Config::default()
        };
        let app = App::new(vec![file("src/diff/parser.rs")], false, false, &cfg);
        assert!(!app.collapsed.contains("src"));
        assert!(app.collapsed.contains("src/diff"));
    }

    #[test]
    fn status_clears_once_its_deadline_passes() {
        let mut app = app_with(vec![file("a.rs")]);
        app.set_status("hi");
        assert!(app.status.is_some());

        // Still within the display window: the message stays.
        app.expire_status();
        assert!(app.status.is_some());

        // Past the deadline: the message clears itself.
        app.status_deadline = Some(Instant::now());
        app.expire_status();
        assert!(app.status.is_none());
        assert!(app.status_deadline.is_none());
    }

    #[test]
    fn page_keys_scroll_diff_by_a_screenful() {
        let mut app = app_with(vec![file("a.rs")]);
        app.focus = Focus::Diff;
        app.diff_height = 20;

        // PageDown advances by the viewport height less a line of overlap.
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

    #[test]
    fn start_focus_follows_config_but_yields_when_tree_hidden() {
        let diff_first = App::new(vec![file("a.rs")], false, false, &Config::default());
        assert_eq!(diff_first.focus, Focus::Diff); // default: single-file view

        let tree_cfg = Config {
            start_focus: Focus::Tree,
            ..Config::default()
        };
        let tree_first = App::new(vec![file("a.rs")], false, false, &tree_cfg);
        assert_eq!(tree_first.focus, Focus::Tree);

        // With the tree hidden there's nothing to focus but the diff.
        let hidden_cfg = Config {
            start_focus: Focus::Tree,
            show_tree: false,
            ..Config::default()
        };
        let hidden = App::new(vec![file("a.rs")], false, false, &hidden_cfg);
        assert_eq!(hidden.focus, Focus::Diff);
    }

    /// The editor buffer must round-trip a body containing `#`, which is why it
    /// uses git's scissors convention rather than git's `#`-comment convention:
    /// review notes routinely contain markdown headings and shell snippets.
    #[test]
    fn the_editor_template_round_trips_a_body_containing_hashes() {
        let pending = PendingComment {
            file: "src/app.rs".to_string(),
            anchor: Anchor {
                side: crate::comment::Side::New,
                line: 103,
            },
            reply_to: None,
            diff_hash: 0,
            context: vec!["  103 │ let x = 1;".to_string()],
            draft: String::new(),
        };
        let template = comment_template(&pending);
        // The footer orients the author without becoming part of the note.
        assert!(template.contains("src/app.rs:103 (new side)"));
        assert!(template.contains("let x = 1;"));
        assert_eq!(strip_scissors(&template), "", "an untouched buffer aborts");

        let typed = format!("# A heading\n\nAnd a `#!/bin/sh` line.\n{template}");
        assert_eq!(
            strip_scissors(&typed),
            "# A heading\n\nAnd a `#!/bin/sh` line."
        );

        // Handing a half-typed note to the editor pre-fills the buffer with it,
        // so `Ctrl-O` continues the note rather than starting it over.
        let carried = comment_template(&PendingComment {
            draft: "half a thought".to_string(),
            ..pending
        });
        assert_eq!(strip_scissors(&carried), "half a thought");
    }

    /// A three-line diff carrying the gutter delta emits under riffnav's pinned
    /// number formats, so the line map has real numbers to anchor against.
    fn seed_text() -> ratatui::text::Text<'static> {
        ratatui::text::Text::from(
            (1..=3)
                .map(|n| ratatui::text::Line::from(format!("{n:>5}⋮{n:>5}│let x{n} = {n};")))
                .collect::<Vec<_>>(),
        )
    }

    /// An app with comments on, that diff rendered, and the cursor on a
    /// commentable line — the state `c` is pressed from.
    fn app_ready_to_comment() -> App {
        let mut app = app_with(vec![file_with_raw("a.rs")]);
        app.install_comments_for_test(CommentStore::disabled());
        app.focus = Focus::Diff;
        app.diff_height = 20;
        app.seed_render_for_test(60, seed_text());
        app.resync_cursor();
        app
    }

    fn type_into(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.press_for_test(KeyCode::Char(ch), false);
        }
    }

    /// The point of the composer: a note is typed and saved without the screen
    /// ever being handed to `$EDITOR`.
    #[test]
    fn a_comment_is_typed_and_saved_in_place() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        assert!(app.composer.is_some(), "`c` opens the composer");

        type_into(&mut app, "no backoff");
        app.press_for_test(KeyCode::Enter, false); // a second line, not a save
        type_into(&mut app, "here");
        assert!(app.composer.is_some(), "Enter must not end the note");

        app.press_for_test(KeyCode::Char('s'), true);
        assert!(app.composer.is_none());
        assert!(app.pending_comment.is_none(), "no editor was involved");
        let saved = app.comments.all();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].body, "no backoff\nhere");
        assert_eq!(saved[0].line, 1, "anchored to the cursor's line");
    }

    /// While a note is being typed the composer owns every key, so the ones that
    /// normally act on the panes type instead.
    #[test]
    fn the_composer_swallows_the_pane_keys() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "quit v?");

        assert!(!app.quit, "`q` types rather than quitting");
        assert!(!app.show_help, "`?` types rather than opening help");
        assert_eq!(
            app.viewed_count(),
            0,
            "`v` types rather than marking viewed"
        );
        app.press_for_test(KeyCode::Char('s'), true);
        assert_eq!(app.comments.all()[0].body, "quit v?");
    }

    #[test]
    fn escape_discards_the_note_and_saves_nothing() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "never mind");
        app.press_for_test(KeyCode::Esc, false);
        assert!(app.composer.is_none());
        assert!(app.comments.all().is_empty());
        assert!(!app.quit, "Esc closed the composer, not riffnav");

        // An empty note is an abort too, exactly like an empty commit message.
        app.press_for_test(KeyCode::Char('c'), false);
        app.press_for_test(KeyCode::Char('s'), true);
        assert!(app.comments.all().is_empty());
    }

    /// There's one comment key, not two: on a diff line `c` starts a note, and
    /// inside a thread — where `]` parks the cursor — it replies to the comment
    /// it's sitting on, which is the only thing it could sensibly mean there.
    #[test]
    fn c_inside_a_thread_replies_to_the_comment_under_the_cursor() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "first");
        app.press_for_test(KeyCode::Char('s'), true);

        // Re-splice so the stored note occupies rows, then jump onto it.
        app.seed_render_for_test(60, seed_text());
        app.resync_cursor();
        app.jump_comment(true);
        assert!(
            app.current_render()
                .unwrap()
                .line_map
                .get(app.diff_cursor)
                .anchor()
                .is_none(),
            "the cursor is on a comment row, not a diff line"
        );

        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "second");
        app.press_for_test(KeyCode::Char('s'), true);
        let saved = app.comments.all();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[1].body, "second");
        assert_eq!(
            saved[1].line, saved[0].line,
            "a reply hangs on the same line as the thread"
        );
        assert_eq!(
            saved[1].reply_to.as_deref(),
            Some(saved[0].id.as_str()),
            "it threads under the comment the cursor was on"
        );

        // Back on a diff line, the same key starts a root note again.
        app.seed_render_for_test(60, seed_text());
        app.resync_cursor();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "third");
        app.press_for_test(KeyCode::Char('s'), true);
        assert!(app.comments.all()[2].reply_to.is_none());
    }

    /// `Ctrl-O` is the escape hatch for a note that outgrows the field: it hands
    /// the same anchor — and the text so far — to the editor path.
    #[test]
    fn ctrl_o_hands_the_half_written_note_to_the_editor() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "half a thought");
        app.press_for_test(KeyCode::Char('o'), true);

        assert!(app.composer.is_none());
        let pending = app.pending_comment.as_ref().expect("handed to the editor");
        assert_eq!(pending.draft, "half a thought");
        assert_eq!(pending.anchor.line, 1);
        assert!(
            app.comments.all().is_empty(),
            "nothing is stored until it's saved"
        );
    }

    /// Comments off means no line cursor, so j/k must scroll the pane exactly as
    /// they did before the feature existed.
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
