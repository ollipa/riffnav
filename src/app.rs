use std::collections::HashSet;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::DefaultTerminal;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;
use serde::Deserialize;

use crate::autodiff::{AutoDiff, DiffSource};
use crate::config::Config;
use crate::delta::RenderCache;
use crate::diff::{FileDiff, FileStatus};
use crate::forge::{Forge, ReviewSync};
use crate::herdr::Herdr;
use crate::icons::IconStyle;
use crate::review::ReviewStore;
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

/// Best-effort terminal mouse reporting, toggled around screen ownership: on
/// while the TUI runs so clicks and the wheel reach us, off whenever we hand the
/// terminal back (teardown, or suspending for `$EDITOR`). Failures are ignored —
/// a terminal without mouse support just keeps working off the keyboard.
fn enable_mouse() {
    let _ = std::io::stdout().execute(EnableMouseCapture);
}

fn disable_mouse() {
    let _ = std::io::stdout().execute(DisableMouseCapture);
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
                let raw = &self.files[idx].raw;
                let side_by_side = self.side_by_side_for(idx);
                self.cache
                    .ensure(idx, raw, diff_width, side_by_side, self.diff_theme)?;
            }

            terminal.draw(|frame| crate::ui::draw(frame, self, diff_width))?;

            if self.watch.is_some() {
                self.watch_tick()?;
            } else {
                self.wait_for_event()?;
            }

            // Suspending the TUI to run an editor needs the owned terminal.
            if let Some(path) = self.pending_editor.take() {
                self.open_editor(terminal, &path)?;
                // The file may have changed; re-read its diff from git.
                self.refresh_file(&path);
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
        match (status, syncing) {
            (Some(s), true) => Some(s.min(SYNC_POLL)),
            (Some(s), false) => Some(s),
            (None, true) => Some(SYNC_POLL),
            (None, false) => None,
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
        }
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
    fn open_editor(&mut self, terminal: &mut DefaultTerminal, path: &str) -> Result<()> {
        disable_mouse();
        ratatui::restore();
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        let status = Command::new(&editor).arg(path).status();

        *terminal = ratatui::init();
        enable_mouse();
        let _ = terminal.clear();
        self.last_width = 0; // force a re-render into the fresh screen

        self.status = Some(match status {
            Ok(s) if s.success() => format!("Edited {path}"),
            Ok(s) => format!("{editor} exited: {s}"),
            Err(e) => format!("Couldn't launch {editor}: {e}"),
        });
        Ok(())
    }

    /// Route a mouse event to the pane under the cursor. Overlays own the whole
    /// screen, so clicks and scrolls beneath them are ignored.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.finder.is_some() || self.show_help {
            return;
        }
        let pos = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(pos),
            MouseEventKind::ScrollDown => self.scroll_at(pos, 1),
            MouseEventKind::ScrollUp => self.scroll_at(pos, -1),
            _ => {}
        }
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

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

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
            KeyCode::Char('j') | KeyCode::Down => {
                if self.focus == Focus::Tree {
                    self.move_selection(1)
                } else {
                    self.scroll_diff(1)
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.focus == Focus::Tree {
                    self.move_selection(-1)
                } else {
                    self.scroll_diff(-1)
                }
            }
            KeyCode::Char('n') => self.jump_file(true),
            KeyCode::Char('p') | KeyCode::Char('N') => self.jump_file(false),
            KeyCode::Char('d') if ctrl => self.scroll_diff(HALF_PAGE),
            KeyCode::Char('u') if ctrl => self.scroll_diff(-HALF_PAGE),
            KeyCode::PageDown => self.page_move(true),
            KeyCode::PageUp => self.page_move(false),
            KeyCode::Char('g') => self.diff_scroll = 0,
            KeyCode::Char('G') => self.diff_scroll = u16::MAX, // clamped on draw
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
            // Only bound inside herdr; an inert no-op elsewhere.
            KeyCode::Char('z') if self.herdr.is_some() => self.toggle_herdr_zoom(),
            // Only bound when a supported forge (e.g. GitHub) is detected.
            KeyCode::Char('W') if self.forge.is_some() => self.open_web_diff(),
            KeyCode::Char('o') => {
                if let Some(idx) = self.selected_file() {
                    self.pending_editor = Some(self.files[idx].path().to_string());
                }
            }
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
        Ok(())
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
}
