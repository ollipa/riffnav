//! The search prompt, and moving between matches. The finding itself is in
//! [`crate::search`].

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, CursorToken, Focus};
use crate::comment::Anchor;
use crate::diff::RowWalk;
use crate::render::{Marks, Rendered};
use crate::search::{Hit, Resume, Search};
use crate::tree;

/// How long the scan may hold up each pass of the event loop.
const SCAN_SLICE: Duration = Duration::from_millis(8);
/// How long typing has to pause before the view follows a match into a file that
/// has yet to be rendered. Rendering a big file blocks, and the next keystroke
/// would usually move the match on anyway.
const PREVIEW_DELAY: Duration = Duration::from_millis(150);

/// Everything about the search, kept together.
#[derive(Default)]
pub(super) struct Find {
    search: Option<Search>,
    prompt: Option<Prompt>,
    /// The first match still to be shown.
    seek: Option<Seek>,
    /// A match waiting on the event loop to render the file it's in.
    landing: Option<Landing>,
    /// Where to put the view back if Enter was pressed before the scan got far
    /// enough to say there are no matches.
    fallback: Option<Origin>,
    /// The cursor's line when it arrived at the current match. While it's still
    /// there, a new render brings the match back into view.
    held: Option<Anchor>,
    /// The render `search.place` was worked out for.
    token: Option<CursorToken>,
}

/// The query being typed, and how to back out of it.
struct Prompt {
    query: String,
    origin: Origin,
    /// The search this one is replacing, which Esc brings back.
    before: Option<Search>,
    typed: Instant,
}

/// The view when the prompt opened. Its folders are in here because following a
/// match opens them.
#[derive(Clone)]
struct Origin {
    selected: usize,
    collapsed: HashSet<String>,
    scroll: u16,
    cursor: usize,
    anchor: Option<Anchor>,
    token: Option<CursorToken>,
    focus: Focus,
    /// The file on show, if a folder wasn't selected, and where it is in the list.
    file: Option<usize>,
    rank: usize,
}

/// Where to start looking from: at or after this line of this file.
#[derive(Clone, Copy)]
struct Seek {
    file: Option<usize>,
    rank: usize,
    line: usize,
}

/// Which of the selected file's matches to go to, once it's rendered.
#[derive(Clone, Copy)]
enum Landing {
    AtOrAfter(usize),
    First,
    Last,
}

/// The order `n` visits a file's matches in: down the screen, and across it. Side
/// by side that isn't the order of the rows, which have every removed line of a
/// change ahead of every added one.
fn position(render: &Rendered, hit: &Hit) -> Option<(usize, usize, u32)> {
    let start = render.row_start(hit.row)?;
    Some((start.line, start.side, hit.start))
}

impl App {
    /// Whether matches are showing, which is what gives `n`, `N` and Esc their
    /// search meanings.
    pub(super) fn searching(&self) -> bool {
        self.find.search.is_some()
    }

    /// The query in the open prompt.
    pub fn search_prompt(&self) -> Option<&str> {
        self.find.prompt.as_ref().map(|p| p.query.as_str())
    }

    /// The query of the search in force.
    pub fn search_query(&self) -> Option<&str> {
        self.find.search.as_ref().map(|s| s.query.as_str())
    }

    /// Where the search has got to, for the footer: `3/17`, with a `…` for as long
    /// as the total could still go up.
    pub fn search_count(&self) -> Option<String> {
        let Some(search) = &self.find.search else {
            // An empty prompt has nothing to count yet.
            return None;
        };
        let more = if search.done() { "" } else { "…" };
        Some(match (search.total(), search.current) {
            (0, _) if search.done() => "no matches".to_string(),
            (0, _) => "searching…".to_string(),
            (total, Some(_)) => format!("{}/{total}{more}", search.place + 1),
            (1, None) => format!("1 match{more}"),
            (total, None) => format!("{total} matches{more}"),
        })
    }

    /// `file`'s matches, for painting over its render in a pane `width` wide.
    pub fn search_marks(&self, file: usize, width: u16) -> Option<Marks<'_>> {
        let search = self.find.search.as_ref()?;
        let hits = search.in_file(file);
        (!hits.is_empty()).then(|| Marks {
            hits,
            current: search.current_hit(),
            bg: self.diff_theme.match_bg(),
            current_fg: self.diff_theme.current_match_fg(),
            current_bg: self.diff_theme.current_match_bg(),
            width,
        })
    }

    /// How soon the event loop has to come back to carry on with the search.
    pub(super) fn search_wait(&self) -> Option<Duration> {
        if self.find.search.as_ref().is_some_and(|s| !s.done()) {
            return Some(Duration::ZERO);
        }
        self.find.seek?;
        let typed = self.find.prompt.as_ref()?.typed;
        Some(PREVIEW_DELAY.saturating_sub(typed.elapsed()))
    }

    /// The `/` key: open the prompt on a blank query. Whatever search was in force
    /// stops showing, and comes back if this one is cancelled.
    pub(super) fn open_search(&mut self) {
        let rank = self.start_rank();
        let origin = Origin {
            selected: self.selected_index(),
            collapsed: self.collapsed.clone(),
            scroll: self.diff_scroll,
            cursor: self.diff_cursor,
            anchor: self.cursor_anchor,
            token: self.cursor_token,
            focus: self.focus,
            file: self.selected_file(),
            rank,
        };
        self.find = Find {
            prompt: Some(Prompt {
                query: String::new(),
                origin,
                before: self.find.search.take(),
                typed: Instant::now(),
            }),
            ..Find::default()
        };
    }

    /// Where in tree order the view is: the selected file, or the first file
    /// inside a selected folder.
    fn start_rank(&self) -> usize {
        let order = self.tree_order();
        if let Some(file) = self.selected_file() {
            return order.iter().position(|&f| f == file).unwrap_or(0);
        }
        let Some(tree::RowKind::Dir { path, .. }) =
            self.rows.get(self.selected_index()).map(|r| &r.kind)
        else {
            return 0;
        };
        let inside = format!("{path}/");
        order
            .iter()
            .position(|&f| self.files[f].path().starts_with(&inside))
            .unwrap_or(0)
    }

    /// A keypress while the prompt is open. It owns all input, like the finder.
    pub(super) fn prompt_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let Some(prompt) = self.find.prompt.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => return self.cancel_prompt(),
            KeyCode::Char('c') if ctrl => return self.cancel_prompt(),
            KeyCode::Enter => return self.commit_prompt(),
            KeyCode::Backspace if prompt.query.is_empty() => return self.cancel_prompt(),
            KeyCode::Backspace => {
                prompt.query.pop();
            }
            KeyCode::Char('u') if ctrl => prompt.query.clear(),
            KeyCode::Char('w') if ctrl => {
                let word = prompt.query.trim_end();
                let word = word.rfind(char::is_whitespace).map_or(0, |at| at + 1);
                prompt.query.truncate(word);
            }
            KeyCode::Char(c) if !ctrl && !alt => prompt.query.push(c),
            _ => return,
        }
        self.query_changed();
    }

    /// Start over with what's in the prompt. The scan and the jump to its first
    /// match both happen in the event loop, so a burst of keys costs one scan.
    fn query_changed(&mut self) {
        let Some(prompt) = self.find.prompt.as_mut() else {
            return;
        };
        prompt.typed = Instant::now();
        let query = prompt.query.clone();
        let origin = prompt.origin.clone();
        self.find.search = Search::new(
            &query,
            self.files.len(),
            self.tree_order(),
            self.cache.tab_width(),
        );
        self.find.landing = None;
        self.find.seek = self.find.search.as_ref().map(|_| Seek {
            file: origin.file,
            rank: origin.rank,
            line: origin.cursor,
        });
        if self.find.search.is_none() {
            self.restore_view(&origin);
        }
    }

    /// Esc on the prompt: as if it had never opened.
    fn cancel_prompt(&mut self) {
        let Some(prompt) = self.find.prompt.take() else {
            return;
        };
        self.restore_view(&prompt.origin);
        self.find = Find {
            search: prompt.before,
            ..Find::default()
        };
    }

    /// Enter on the prompt: keep the search, and go to the match it's found. There
    /// may be none yet, in which case the event loop carries on looking.
    fn commit_prompt(&mut self) {
        let Some(query) = self.search_prompt().map(str::to_string) else {
            return;
        };
        if query.is_empty() {
            return self.cancel_prompt();
        }
        let Some(prompt) = self.find.prompt.take() else {
            return;
        };
        self.focus = Focus::Diff;
        self.find.fallback = Some(prompt.origin);
        self.settle_search();
    }

    /// Give up on a search that turned out to match nothing, or stop waiting on
    /// one that has found what Enter was after.
    fn settle_search(&mut self) {
        let Some(search) = &self.find.search else {
            return;
        };
        if search.done() && search.total() == 0 {
            let note = format!("No matches for {}", search.query);
            let origin = self.find.fallback.take();
            self.clear_search();
            if let Some(origin) = origin {
                self.restore_view(&origin);
            }
            self.set_status(note);
        } else if self.find.seek.is_none() && self.find.landing.is_none() {
            self.find.fallback = None;
        }
    }

    /// Esc with a search in force: drop it, and leave the view where it is.
    pub(super) fn clear_search(&mut self) {
        self.find = Find::default();
    }

    fn restore_view(&mut self, origin: &Origin) {
        self.collapsed = origin.collapsed.clone();
        self.rows = tree::flatten(&self.nodes, &self.collapsed);
        self.tree_state.select(Some(origin.selected));
        self.diff_scroll = origin.scroll;
        self.diff_cursor = origin.cursor;
        self.cursor_anchor = origin.anchor;
        self.cursor_token = origin.token;
        self.focus = origin.focus;
    }

    /// Carry the search on, once per pass of the event loop: scan some more, and
    /// follow the first match if it has come into sight.
    pub(super) fn advance_search(&mut self) {
        let Some(search) = self.find.search.as_mut() else {
            return;
        };
        if !search.done() {
            let resuming = search.resuming();
            search.scan(&self.files, SCAN_SLICE);
            if resuming && !search.resuming() {
                self.find.token = None; // It found its match, and that needs a place.
            }
        }
        self.seek_match();
        if self.find.prompt.is_none() {
            self.settle_search();
        }
    }

    /// Go to the first match at or after where the search started, once the scan
    /// has got far enough to be sure which that is.
    fn seek_match(&mut self) {
        let (Some(search), Some(seek)) = (&self.find.search, self.find.seek) else {
            return;
        };
        let Some(target) = self.seek_target(search, seek) else {
            return;
        };
        let Some((file, landing)) = target else {
            // Nothing matches, so nothing should be left looking as though it did.
            self.find.seek = None;
            if let Some(prompt) = &self.find.prompt {
                let origin = prompt.origin.clone();
                self.restore_view(&origin);
            }
            return;
        };
        if self.selected_file() != Some(file) {
            let paused = self
                .find
                .prompt
                .as_ref()
                .is_none_or(|p| p.typed.elapsed() >= PREVIEW_DELAY);
            if !paused && self.render_of(file).is_none() {
                return;
            }
            self.reveal_file(file);
        }
        self.find.seek = None;
        self.find.landing = Some(landing);
    }

    /// The file `seek` leads to and which of its matches to land on: `None` when
    /// it's too early to say, `Some(None)` when there's no match at all.
    fn seek_target(&self, search: &Search, seek: Seek) -> Option<Option<(usize, Landing)>> {
        for rank in seek.rank..search.file_count() {
            if !search.scanned(rank) {
                return None;
            }
            let file = search.file_at(rank)?;
            let hits = search.in_file(file);
            if hits.is_empty() {
                continue;
            }
            if Some(file) != seek.file {
                return Some(Some((file, Landing::First)));
            }
            // The render can be missing if the pane was resized on the way here.
            let Some(render) = self.render_of(file) else {
                return Some(Some((file, Landing::First)));
            };
            let after = |hit: &Hit| {
                render
                    .row_start(hit.row)
                    .is_some_and(|start| start.line >= seek.line)
            };
            if hits.iter().any(after) {
                return Some(Some((file, Landing::AtOrAfter(seek.line))));
            }
        }
        if !search.done() {
            return None;
        }
        let wrapped = (0..search.file_count())
            .filter_map(|rank| search.file_at(rank))
            .find(|&file| !search.in_file(file).is_empty());
        Some(wrapped.map(|file| (file, Landing::First)))
    }

    /// Finish going to a match, now the event loop has rendered its file, and keep
    /// the current match right if the render changed under it.
    pub(super) fn land_on_match(&mut self) {
        self.follow_match();
        let Some(landing) = self.find.landing else {
            return;
        };
        let (Some(file), Some(render)) = (self.selected_file(), self.current_render()) else {
            return;
        };
        let Some(search) = &self.find.search else {
            return;
        };
        let ranked = search
            .in_file(file)
            .iter()
            .enumerate()
            .filter_map(|(at, hit)| Some((position(render, hit)?, at)));
        let first = search.first_in_file(file);
        let picked = match landing {
            Landing::First => ranked.min(),
            Landing::Last => ranked.max(),
            Landing::AtOrAfter(line) => ranked.filter(|(at, _)| at.0 >= line).min(),
        };
        self.find.landing = None;
        if let Some((_, at)) = picked {
            self.show_match(first + at);
        }
    }

    /// After a resize, `s` or `T` the current match is on different rows, and side
    /// by side it can be in a different place in the order. If the cursor hasn't
    /// left it, take it back there, rather than to wherever its line number went:
    /// a removed line and the added line beside it share only one.
    fn follow_match(&mut self) {
        if self.find.token == self.cursor_token {
            return;
        }
        self.find.token = self.cursor_token;
        let Some(search) = &self.find.search else {
            return;
        };
        let (Some(index), Some(hit)) = (search.current, search.current_hit()) else {
            return;
        };
        if self.selected_file() != Some(hit.file as usize) {
            return;
        }
        if self.find.held.is_some() && self.find.held == self.cursor_anchor {
            self.show_match(index);
        } else if let Some(place) = self.place_of(hit)
            && let Some(search) = self.find.search.as_mut()
        {
            search.place = place;
        }
    }

    /// `hit`'s place in the order `n` visits matches, from 0.
    fn place_of(&self, hit: Hit) -> Option<usize> {
        let render = self.current_render()?;
        let search = self.find.search.as_ref()?;
        let file = hit.file as usize;
        let key = position(render, &hit)?;
        let hits = search.in_file(file).iter();
        let before = hits.filter(|h| position(render, h).is_some_and(|at| at < key));
        Some(search.first_in_file(file) + before.count())
    }

    /// Make match `index`, in the selected file, the current one: the line cursor
    /// goes to its line, so `c` and `m` act on it, and it's brought on screen. Like
    /// `m`, a match that was off screen lands a third of the way down.
    fn show_match(&mut self, index: usize) {
        let Some(hit) = self.find.search.as_ref().and_then(|s| s.hit(index)) else {
            return;
        };
        let place = self.place_of(hit);
        let Some(render) = self.current_render() else {
            return;
        };
        let Some(start) = render.row_start(hit.row) else {
            return;
        };
        // The line it starts on, not the wrapped one it's on, which has no number
        // for `c` to anchor to.
        let line = start.line;
        let row = render.row_of_hit(hit, self.last_width);
        let row = row.unwrap_or_else(|| render.row_of(line));
        let height = self.diff_height;
        // Short of the last row, as a match wrapped there would go on out of sight.
        let bottom = height.saturating_sub(1).max(1);
        let in_view = |scroll: u16| (scroll..scroll.saturating_add(bottom)).contains(&row);
        let was_in_view = in_view(self.diff_scroll);
        self.set_cursor(line);
        if !was_in_view || !in_view(self.diff_scroll) {
            self.diff_scroll = row.saturating_sub(height / 3);
        }
        self.find.held = self.cursor_anchor;
        self.find.token = self.cursor_token;
        if let Some(search) = self.find.search.as_mut() {
            search.current = Some(index);
            search.place = place.unwrap_or(index);
        }
    }

    /// The `n` and `N` keys. Starts from the current match if the cursor is still
    /// on it, otherwise from the cursor, and walks off the end of a file into the
    /// next one with a match, wrapping at the ends of the diff.
    pub(super) fn step_match(&mut self, forward: bool) {
        if self.find.landing.is_some() || self.find.seek.is_some() {
            return; // Still on the way: the render this would read is out of date.
        }
        let Some(search) = &self.find.search else {
            return;
        };
        if search.total() == 0 {
            let note = if search.done() {
                "No matches"
            } else {
                "Still searching…"
            };
            return self.set_status(note);
        }
        self.focus = Focus::Diff;
        if let Some(index) = self.next_in_file(forward) {
            return self.show_match(index);
        }
        self.step_file(forward);
    }

    /// The next match in the selected file, as an index into the whole list.
    fn next_in_file(&self, forward: bool) -> Option<usize> {
        let file = self.selected_file()?;
        let render = self.current_render()?;
        let search = self.find.search.as_ref()?;
        let cursor = self.diff_cursor;
        let from = search
            .current_hit()
            .filter(|hit| hit.file as usize == file)
            .and_then(|hit| position(render, &hit))
            .filter(|at| at.0 == cursor);
        let ranked = search
            .in_file(file)
            .iter()
            .enumerate()
            .filter_map(|(at, hit)| Some((position(render, hit)?, at)));
        let line_start = (cursor, 0, 0);
        let picked = match (forward, from) {
            (true, Some(from)) => ranked.filter(|(at, _)| *at > from).min(),
            (true, None) => ranked.filter(|(at, _)| *at >= line_start).min(),
            (false, Some(from)) => ranked.filter(|(at, _)| *at < from).max(),
            (false, None) => ranked.filter(|(at, _)| *at < line_start).max(),
        };
        picked.map(|(_, at)| search.first_in_file(file) + at)
    }

    /// Go on to the nearest file with matches in the direction of travel, which
    /// may be this one again, the long way round.
    fn step_file(&mut self, forward: bool) {
        let Some(search) = &self.find.search else {
            return;
        };
        let count = search.file_count();
        let here = self.selected_file();
        let from = self.start_rank();
        // With a folder selected, the file at `from` hasn't been visited yet.
        let skip = usize::from(here.is_some() || !forward);
        let ranks = (0..count).map(|step| {
            let step = step + skip;
            if forward {
                (from + step) % count
            } else {
                (from + count - step % count) % count
            }
        });
        let target = ranks
            .filter_map(|rank| Some((rank, search.file_at(rank)?)))
            .find(|&(_, file)| !search.in_file(file).is_empty());
        let Some((rank, file)) = target else {
            return;
        };
        let wrapped = match (forward, here) {
            (true, Some(_)) => rank <= from,
            (true, None) => rank < from,
            (false, _) => rank >= from,
        };
        // A match past the end of what's been scanned would come before the wrap.
        if wrapped && !search.done() {
            return self.set_status("Still searching…");
        }
        if here != Some(file) {
            self.reveal_file(file);
        }
        self.find.landing = Some(if forward {
            Landing::First
        } else {
            Landing::Last
        });
        if wrapped {
            let end = if forward { "top" } else { "bottom" };
            self.set_status(format!("Search wrapped to the {end}"));
        }
    }

    /// What identifies the current match across a reload, along with the path of
    /// its file. Taken before `files` changes, since it reads the row back out.
    pub(super) fn search_resume(&self) -> Option<(String, Resume)> {
        let hit = self.find.search.as_ref()?.current_hit()?;
        let file = self.files.get(hit.file as usize)?;
        let mut walk = RowWalk::default();
        let row = loop {
            let at = walk.index;
            let row = walk.next(&file.raw)?;
            if at == hit.row as usize {
                break row;
            }
        };
        let resume = Resume {
            file: hit.file as usize,
            old: row.old,
            new: row.new,
            start: hit.start,
        };
        Some((file.path().to_string(), resume))
    }

    /// Search a changed diff again, and find the match the view was on in it.
    /// Every list index, and so every jump in progress, is out of date.
    pub(super) fn restart_search(&mut self, resume: Option<(String, Resume)>) {
        let Some(query) = self.search_query().map(str::to_string) else {
            return;
        };
        let held = self.find.held;
        let order = self.tree_order();
        let tab = self.cache.tab_width();
        let mut search = Search::new(&query, self.files.len(), order, tab);
        if let (Some(search), Some((path, resume))) = (&mut search, resume) {
            let file = self.files.iter().position(|f| f.path() == path);
            search.resume_at(file.map(|file| Resume { file, ..resume }));
        }
        self.find = Find {
            search,
            held,
            ..Find::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crossterm::event::KeyCode::{self, Backspace, Char, Enter, Esc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;

    use super::*;
    use crate::comment::CommentStore;
    use crate::config::Config;
    use crate::diff::parse;
    use crate::theme::DiffTheme;

    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 16;

    fn hunk(path: &str, body: &str) -> String {
        let old = body.lines().filter(|l| !l.starts_with('+')).count();
        let new = body.lines().filter(|l| !l.starts_with('-')).count();
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,{old} +1,{new} @@\n{body}"
        )
    }

    fn app_of(raw: &str, side_by_side: bool, cfg: &Config) -> App {
        let mut app = App::new(parse(raw), side_by_side, cfg);
        app.install_comments_for_test(CommentStore::disabled());
        app
    }

    fn app(raw: &str) -> App {
        app_of(raw, false, &Config::default())
    }

    /// One pass of the event loop at a given size.
    fn frame_at(app: &mut App, width: u16, height: u16) -> Buffer {
        let diff_width = app.diff_pane_width(width);
        // Twice, as the event loop would: the first pass may select a file that
        // the jump into it only lands on once it's rendered.
        for _ in 0..2 {
            app.prepare_frame(diff_width);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| crate::ui::draw(f, app, diff_width))
                .unwrap();
        }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| crate::ui::draw(f, app, diff_width))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn frame(app: &mut App) -> Buffer {
        frame_at(app, WIDTH, HEIGHT)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.press_for_test(code, false);
        frame(app);
    }

    fn type_query(app: &mut App, query: &str) {
        press(app, Char('/'));
        for c in query.chars() {
            press(app, Char(c));
        }
        // Past the wait on a file that has yet to be rendered.
        if let Some(prompt) = app.find.prompt.as_mut() {
            prompt.typed = Instant::now() - PREVIEW_DELAY;
        }
        frame(app);
    }

    fn search_for(app: &mut App, query: &str) {
        frame(app);
        type_query(app, query);
        press(app, Enter);
    }

    /// The text of each run of cells with background `bg`, top to bottom.
    fn runs(buffer: &Buffer, bg: Color) -> Vec<String> {
        let mut runs = Vec::new();
        for y in 0..buffer.area.height {
            let mut run = String::new();
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.bg == bg {
                    run.push_str(cell.symbol());
                } else if !run.is_empty() {
                    runs.push(std::mem::take(&mut run));
                }
            }
            if !run.is_empty() {
                runs.push(run);
            }
        }
        runs
    }

    fn matches(app: &mut App) -> Vec<String> {
        let bg = app.diff_theme.match_bg();
        runs(&frame(app), bg)
    }

    fn current(app: &mut App) -> Vec<String> {
        let bg = app.diff_theme.current_match_bg();
        runs(&frame(app), bg)
    }

    fn text(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                let cells = 0..buffer.area.width;
                cells.map(|x| buffer[(x, y)].symbol()).collect::<String>()
            })
            .collect()
    }

    fn footer(app: &mut App) -> String {
        text(&frame(app)).pop().unwrap()
    }

    fn file_name(app: &App) -> &str {
        app.files[app.selected_file().unwrap()].path()
    }

    fn cursor_new_line(app: &App) -> Option<u32> {
        let render = app.current_render().unwrap();
        render.line_map.get(app.diff_cursor).new
    }

    const TWO_FILES: &str = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n\
         @@ -1,3 +1,3 @@\n one\n-needle old\n+needle new\n three\n\
         diff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n\
         @@ -1,2 +1,2 @@\n x\n-y\n+a needle b needle\n";

    #[test]
    fn slash_opens_the_prompt_and_t_stays_the_finder() {
        let mut app = app(TWO_FILES);
        frame(&mut app);
        press(&mut app, Char('t'));
        assert!(app.finder.is_some());
        press(&mut app, Esc);
        press(&mut app, Char('/'));
        assert_eq!(app.search_prompt(), Some(""));
        assert!(app.finder.is_none());
    }

    #[test]
    fn matches_show_as_you_type_with_a_count() {
        let mut app = app(TWO_FILES);
        frame(&mut app);
        press(&mut app, Char('/'));
        press(&mut app, Char('n'));
        press(&mut app, Char('e'));
        assert!(footer(&mut app).contains("/ne"));
        assert!(footer(&mut app).contains("1/6"), "{}", footer(&mut app));
        assert_eq!(current(&mut app), ["ne"]);
        assert_eq!(matches(&mut app), ["ne", "ne", "ne"]);
    }

    #[test]
    fn typing_never_touches_the_cached_render() {
        let mut app = app(TWO_FILES);
        frame(&mut app);
        let lines = |app: &App| app.current_render().unwrap().text.lines.as_ptr();
        let before = lines(&app);
        type_query(&mut app, "needle");
        press(&mut app, Enter);
        press(&mut app, Char('n'));
        assert_eq!(lines(&app), before);
    }

    #[test]
    fn smart_case_decides_on_the_query_as_typed() {
        let raw = hunk("f", "+Needle\n+needle\n");
        let mut lower = app(&raw);
        search_for(&mut lower, "needle");
        assert!(footer(&mut lower).contains("/2"));
        let mut exact = app(&raw);
        search_for(&mut exact, "Needle");
        assert!(footer(&mut exact).contains("1/1"));
    }

    #[test]
    fn stepping_crosses_files_and_says_when_it_wraps() {
        let mut app = app(TWO_FILES);
        search_for(&mut app, "needle");
        let seen = |app: &mut App| {
            let at = (file_name(app).to_string(), footer(app));
            at.1.split_whitespace()
                .nth(1)
                .map(|c| (at.0, c.to_string()))
        };
        assert_eq!(seen(&mut app).unwrap(), ("a.txt".into(), "1/4".into()));
        press(&mut app, Char('n'));
        assert_eq!(seen(&mut app).unwrap(), ("a.txt".into(), "2/4".into()));
        press(&mut app, Char('n'));
        assert_eq!(seen(&mut app).unwrap(), ("b.txt".into(), "3/4".into()));
        press(&mut app, Char('n'));
        assert_eq!(seen(&mut app).unwrap(), ("b.txt".into(), "4/4".into()));
        assert!(!footer(&mut app).contains("wrapped"));
        press(&mut app, Char('n'));
        assert_eq!(seen(&mut app).unwrap(), ("a.txt".into(), "1/4".into()));
        assert!(footer(&mut app).contains("wrapped to the top"));
        press(&mut app, Char('N'));
        assert_eq!(seen(&mut app).unwrap(), ("b.txt".into(), "4/4".into()));
        assert!(footer(&mut app).contains("wrapped to the bottom"));
    }

    #[test]
    fn p_stays_previous_file_during_a_search() {
        let mut app = app(TWO_FILES);
        search_for(&mut app, "needle");
        press(&mut app, Char('n'));
        press(&mut app, Char('n'));
        assert_eq!(file_name(&app), "b.txt");
        press(&mut app, Char('p'));
        assert_eq!(file_name(&app), "a.txt");
        assert!(app.searching());
    }

    #[test]
    fn with_no_search_n_and_shift_n_change_file() {
        let mut app = app(TWO_FILES);
        frame(&mut app);
        press(&mut app, Char('n'));
        assert_eq!(file_name(&app), "b.txt");
        press(&mut app, Char('N'));
        assert_eq!(file_name(&app), "a.txt");
    }

    #[test]
    fn esc_clears_the_search_before_it_quits() {
        let mut app = app(TWO_FILES);
        search_for(&mut app, "needle");
        press(&mut app, Char('n'));
        let line = app.diff_cursor;
        press(&mut app, Esc);
        assert!(!app.searching() && !app.quit);
        assert!(matches(&mut app).is_empty() && current(&mut app).is_empty());
        assert_eq!(app.diff_cursor, line);
        press(&mut app, Esc);
        assert!(app.quit);
    }

    #[test]
    fn enter_goes_to_the_first_match_at_or_after_the_cursor() {
        let raw = hunk("f", " needle 1\n x\n needle 3\n y\n needle 5\n");
        let mut app = app(&raw);
        frame(&mut app);
        for _ in 0..3 {
            press(&mut app, Char('j'));
        }
        assert_eq!(cursor_new_line(&app), Some(4));
        type_query(&mut app, "needle");
        press(&mut app, Enter);
        assert_eq!(cursor_new_line(&app), Some(5));
        assert!(footer(&mut app).contains("3/3"));
    }

    #[test]
    fn enter_past_the_last_match_comes_round_to_the_first() {
        let raw = hunk("f", " needle\n x\n y\n");
        let mut app = app(&raw);
        frame(&mut app);
        press(&mut app, Char('j'));
        type_query(&mut app, "needle");
        press(&mut app, Enter);
        assert_eq!(cursor_new_line(&app), Some(1));
    }

    #[test]
    fn esc_on_the_prompt_puts_the_view_back_even_across_a_folded_folder() {
        let raw = format!(
            "{}{}",
            hunk("a.txt", " nothing\n"),
            hunk("dir/b.txt", " needle\n")
        );
        let cfg = Config {
            open_depth: 0,
            ..Config::default()
        };
        let mut app = app_of(&raw, false, &cfg);
        frame(&mut app);
        let (rows, scroll, cursor) = (app.rows.len(), app.diff_scroll, app.diff_cursor);
        let selected = app.selected_index();
        type_query(&mut app, "needle");
        assert_eq!(
            file_name(&app),
            "dir/b.txt",
            "the preview follows the match"
        );
        assert!(app.rows.len() > rows);
        press(&mut app, Esc);
        assert_eq!(app.rows.len(), rows);
        assert_eq!(app.selected_index(), selected);
        assert_eq!((app.diff_scroll, app.diff_cursor), (scroll, cursor));
        assert!(!app.searching() && !app.quit);
    }

    #[test]
    fn esc_on_the_prompt_brings_back_the_search_before_it() {
        let mut app = app(TWO_FILES);
        search_for(&mut app, "needle");
        press(&mut app, Char('n'));
        type_query(&mut app, "three");
        press(&mut app, Esc);
        assert_eq!(app.search_query(), Some("needle"));
        assert!(footer(&mut app).contains("2/4"));
    }

    #[test]
    fn an_empty_query_keeps_nothing() {
        let mut app = app(TWO_FILES);
        frame(&mut app);
        press(&mut app, Char('/'));
        press(&mut app, Enter);
        assert!(app.search_prompt().is_none() && !app.searching());

        press(&mut app, Char('/'));
        press(&mut app, Char('n'));
        press(&mut app, Backspace);
        assert!(matches(&mut app).is_empty() && current(&mut app).is_empty());
        press(&mut app, Backspace);
        assert!(app.search_prompt().is_none());
    }

    #[test]
    fn a_query_with_no_matches_says_so_and_goes_no_further() {
        let mut app = app(TWO_FILES);
        frame(&mut app);
        let cursor = app.diff_cursor;
        type_query(&mut app, "needlf");
        assert!(footer(&mut app).contains("no matches"));
        assert_eq!(app.diff_cursor, cursor, "the view went back");
        press(&mut app, Enter);
        assert!(!app.searching());
        assert!(footer(&mut app).contains("No matches for needlf"));
        assert_eq!(app.diff_cursor, cursor);
    }

    #[test]
    fn empty_diffs_and_binary_files_have_nothing_to_find() {
        let mut empty = app("");
        search_for(&mut empty, "x");
        assert!(!empty.searching());

        let raw = "diff --git a/x.png b/x.png\nBinary files a/x.png and b/x.png differ\n";
        let mut binary = app(raw);
        search_for(&mut binary, "x");
        assert!(!binary.searching());
    }

    #[test]
    fn a_binary_file_between_matches_is_stepped_over() {
        let raw = format!(
            "{}diff --git a/b.png b/b.png\nBinary files a/b.png and b/b.png differ\n{}",
            hunk("a", " x\n"),
            hunk("c", " x\n")
        );
        let mut app = app(&raw);
        search_for(&mut app, "x");
        press(&mut app, Char('n'));
        assert_eq!(file_name(&app), "c");
    }

    #[test]
    fn folded_folders_open_on_the_way_to_a_match() {
        let raw = format!("{}{}", hunk("a/x", " needle\n"), hunk("b/y", " needle\n"));
        let cfg = Config {
            open_depth: 0,
            ..Config::default()
        };
        let mut app = app_of(&raw, false, &cfg);
        frame(&mut app);
        assert!(app.selected_file().is_none(), "a folder is selected");
        search_for(&mut app, "needle");
        assert_eq!(file_name(&app), "a/x");
        press(&mut app, Char('n'));
        assert_eq!(file_name(&app), "b/y");
    }

    #[test]
    fn a_hidden_tree_changes_nothing() {
        let cfg = Config {
            show_tree: false,
            ..Config::default()
        };
        let mut app = app_of(TWO_FILES, false, &cfg);
        search_for(&mut app, "needle");
        press(&mut app, Char('n'));
        press(&mut app, Char('n'));
        assert_eq!(file_name(&app), "b.txt");
        assert_eq!(current(&mut app), ["needle"]);
    }

    #[test]
    fn it_works_with_comments_off() {
        let mut app = App::new(parse(TWO_FILES), false, &Config::default());
        search_for(&mut app, "needle");
        assert_eq!(current(&mut app), ["needle"]);
        press(&mut app, Char('n'));
        assert_eq!(current(&mut app), ["needle"]);
    }

    #[test]
    fn it_shows_with_the_footer_turned_off() {
        let cfg = Config {
            show_footer: false,
            ..Config::default()
        };
        let mut app = app_of(TWO_FILES, false, &cfg);
        frame(&mut app);
        press(&mut app, Char('/'));
        press(&mut app, Char('x'));
        assert!(footer(&mut app).contains("/x"));
        press(&mut app, Enter);
        assert!(footer(&mut app).contains("1/1"));
        press(&mut app, Esc);
        assert!(!footer(&mut app).contains("1/1"));
    }

    #[test]
    fn the_composer_and_the_finder_keep_their_keys() {
        let mut app = app(TWO_FILES);
        frame(&mut app);
        press(&mut app, Char('t'));
        press(&mut app, Char('/'));
        assert_eq!(app.finder.as_ref().unwrap().query, "/");
        press(&mut app, Esc);

        press(&mut app, Char('c'));
        press(&mut app, Char('/'));
        assert!(app.search_prompt().is_none());
        assert_eq!(app.composer().unwrap().body(), "/");
    }

    #[test]
    fn the_prompt_keeps_every_key_for_itself() {
        let mut app = app(TWO_FILES);
        frame(&mut app);
        let selected = app.selected_index();
        press(&mut app, Char('/'));
        for key in "qcstnNp?".chars() {
            press(&mut app, Char(key));
        }
        assert_eq!(app.search_prompt(), Some("qcstnNp?"));
        assert!(!app.quit && !app.show_help && app.composer().is_none());
        assert_eq!(app.selected_index(), selected);
    }

    #[test]
    fn the_cursor_follows_so_c_acts_on_the_match() {
        let mut app = app(TWO_FILES);
        search_for(&mut app, "new");
        assert_eq!(cursor_new_line(&app), Some(2));
        press(&mut app, Char('c'));
        assert_eq!(app.composer().unwrap().pending.anchor.line, 2);
    }

    #[test]
    fn the_line_cursor_does_not_paint_over_the_match() {
        let mut app = app(TWO_FILES);
        search_for(&mut app, "needle");
        let buffer = frame(&mut app);
        let theme = app.diff_theme;
        assert_eq!(runs(&buffer, theme.current_match_bg()), ["needle"]);
        assert!(!runs(&buffer, theme.cursor_line_bg()).is_empty());
        let cells = buffer.content.iter();
        let current = cells.filter(|c| c.bg == theme.current_match_bg());
        assert!(
            current
                .into_iter()
                .all(|c| c.fg == theme.current_match_fg())
        );
    }

    /// Every way a match can be laid out, in every layout: what's painted has to
    /// be the text of the match, the rest of the screen has to read as it did with
    /// no search, and the code has to keep its foreground.
    #[test]
    fn matches_line_up_however_the_text_is_laid_out() {
        let long = "pad ".repeat(30);
        let body = format!(
            " \tneedle\tneedle\n-日本語needle語\n+日本語needle語\n-a\tb\n+a\tneedle\n \
             {long}needle tail\n+{}needle\n+e\u{301}needle\u{301} x\n",
            "x".repeat(90)
        );
        let raw = hunk("f.rs", &body);
        for side_by_side in [false, true] {
            for width in [60, 77, 100, 140, 203] {
                let cfg = Config {
                    show_tree: false,
                    ..Config::default()
                };
                let mut plain = app_of(&raw, side_by_side, &cfg);
                let before = frame_at(&mut plain, width, 60);

                let mut app = app_of(&raw, side_by_side, &cfg);
                frame_at(&mut app, width, 60);
                search_for(&mut app, "needle");
                app.find.held = None;
                app.diff_scroll = 0;
                let after = frame_at(&mut app, width, 60);

                let (a, b) = (text(&before), text(&after));
                let body = 1..a.len() - 1;
                assert_eq!(a[body.clone()], b[body], "w{width} sbs{side_by_side}");

                let theme = app.diff_theme;
                let mut painted = runs(&after, theme.match_bg());
                painted.extend(runs(&after, theme.current_match_bg()));
                let shown = painted.concat();
                assert!(
                    painted.len() >= 6,
                    "w{width} sbs{side_by_side}: {painted:?}"
                );
                // A match cut by a wrap comes as pieces, but every piece is part
                // of `needle`, and they add up to whole ones.
                assert!(
                    painted.iter().all(|p| "needle\u{301}".contains(p.as_str())),
                    "w{width} sbs{side_by_side}: {painted:?}"
                );
                let letters = shown.chars().filter(|c| *c != '\u{301}').count();
                assert_eq!(letters % 6, 0, "{painted:?}");

                for y in 1..after.area.height - 1 {
                    for x in 0..after.area.width {
                        let (was, now) = (&before[(x, y)], &after[(x, y)]);
                        if now.bg == theme.match_bg() {
                            assert_eq!(was.fg, now.fg, "the match lost its syntax color");
                        } else if now.bg == theme.current_match_bg() {
                            assert_eq!(now.fg, theme.current_match_fg());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_trailing_space_match_is_not_drawn_inside_out() {
        let raw = hunk("f", "+needle   \n");
        let mut app = app(&raw);
        search_for(&mut app, "  ");
        let buffer = frame(&mut app);
        let theme = app.diff_theme;
        assert_eq!(runs(&buffer, theme.current_match_bg()), ["  "]);
    }

    #[test]
    fn a_comment_above_a_match_does_not_move_its_paint() {
        let raw = hunk("f.rs", " a\n needle\n");
        let mut app = app(&raw);
        frame(&mut app);
        press(&mut app, Char('c'));
        for c in "needle in a comment".chars() {
            press(&mut app, Char(c));
        }
        press(&mut app, Enter);
        search_for(&mut app, "needle");
        assert!(
            footer(&mut app).contains("1/1"),
            "comment bodies aren't code"
        );
        assert_eq!(current(&mut app), ["needle"]);
    }

    #[test]
    fn a_moved_block_hint_is_not_searched_and_does_not_shift_the_paint() {
        let raw = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +0,0 @@\n\
                   -fn alpha_value() {\n-    first_call(1);\n\
                   diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -0,0 +1,2 @@\n\
                   +fn alpha_value() {\n+    first_call(1);\n";
        let mut app = app(raw);
        search_for(&mut app, "moved");
        assert!(!app.searching());
        search_for(&mut app, "first_call");
        assert!(footer(&mut app).contains("1/2"));
        assert_eq!(current(&mut app), ["first_call"]);
        let hint = text(&frame(&mut app)).concat();
        assert!(hint.contains("moved to"), "{hint}");
    }

    #[test]
    fn side_by_side_steps_down_the_screen() {
        let raw = hunk(
            "f",
            "-needle first\n-needle second\n+needle first!\n+needle second!\n x\n",
        );
        let mut app = app_of(&raw, true, &Config::default());
        search_for(&mut app, "needle");
        let mut lines = Vec::new();
        for _ in 0..4 {
            lines.push(app.diff_cursor);
            press(&mut app, Char('n'));
        }
        assert!(lines.is_sorted(), "{lines:?}");
        assert_eq!(lines[0], lines[1], "both sides of a row, left first");
        assert!(footer(&mut app).contains("1/4"));
    }

    #[test]
    fn a_context_row_is_one_match_painted_in_both_panels() {
        let raw = hunk("f", " needle\n-a\n+b\n");
        let mut app = app_of(&raw, true, &Config::default());
        search_for(&mut app, "needle");
        assert!(footer(&mut app).contains("1/1"));
        assert_eq!(current(&mut app), ["needle", "needle"]);
    }

    #[test]
    fn side_by_side_wrapped_rows_keep_the_cursor_on_the_numbered_line() {
        let raw = hunk("f", &format!(" {}needle\n", "x".repeat(120)));
        let mut app = app_of(&raw, true, &Config::default());
        search_for(&mut app, "needle");
        let render = app.current_render().unwrap();
        assert!(render.line_map.get(app.diff_cursor).anchor().is_some());
        assert_eq!(current(&mut app).concat(), "needleneedle");
    }

    #[test]
    fn a_match_deep_in_a_wrapped_line_comes_into_view() {
        // Far taller than the pane, in words and in one unbroken run.
        for filler in ["word ".repeat(400), "x".repeat(2000)] {
            let raw = hunk("f", &format!(" {filler}needle end\n"));
            for side_by_side in [false, true] {
                let mut app = app_of(&raw, side_by_side, &Config::default());
                search_for(&mut app, "needle");
                assert!(!current(&mut app).is_empty(), "sbs {side_by_side}");
                let render = app.current_render().unwrap();
                assert!(render.line_map.get(app.diff_cursor).anchor().is_some());
            }
        }
    }

    #[test]
    fn the_current_match_scrolls_into_view() {
        let body: String = (0..200).map(|i| format!(" line {i}\n")).collect();
        let raw = hunk("f", &format!("{body} needle\n{body} needle\n"));
        let mut app = app(&raw);
        search_for(&mut app, "needle");
        assert_eq!(current(&mut app), ["needle"]);
        assert_eq!(cursor_new_line(&app), Some(201));
        press(&mut app, Char('n'));
        assert_eq!(current(&mut app), ["needle"]);
        assert_eq!(cursor_new_line(&app), Some(402));
    }

    #[test]
    fn the_search_survives_a_resize_a_layout_switch_and_a_theme_change() {
        let filler: String = (0..100).map(|i| format!(" line {i}\n")).collect();
        let raw = hunk(
            "f.rs",
            &format!("{filler}-needle one\n+needle two here\n{filler}+needle three\n"),
        );
        let mut app = app(&raw);
        search_for(&mut app, "needle");
        press(&mut app, Char('n'));
        let count = |app: &mut App| footer(app).contains("2/3");
        assert!(count(&mut app));

        assert_eq!(
            runs(
                &frame_at(&mut app, 111, 24),
                app.diff_theme.current_match_bg()
            ),
            ["needle"]
        );
        assert!(count(&mut app) || footer(&mut app).contains("2/3"));

        press(&mut app, Char('s'));
        assert_eq!(current(&mut app), ["needle"], "side by side");
        press(&mut app, Char('T'));
        assert_eq!(current(&mut app), ["needle"], "light");
        assert!(count(&mut app));
        press(&mut app, Char('s'));
        assert_eq!(current(&mut app), ["needle"], "unified");
        // Still on the added line, though side by side it shared a row.
        assert_eq!(cursor_new_line(&app), Some(101));
        press(&mut app, Char('n'));
        assert!(footer(&mut app).contains("3/3"));
    }

    #[test]
    fn a_repaint_from_the_hunk_starts_leaves_the_search_alone() {
        let mut app = app(TWO_FILES);
        search_for(&mut app, "needle");
        press(&mut app, Char('n'));
        let (cursor, scroll) = (app.diff_cursor, app.diff_scroll);
        // What a finished read does: the entry goes, and is made again.
        app.cache.clear();
        assert_eq!(current(&mut app), ["needle"]);
        assert_eq!((app.diff_cursor, app.diff_scroll), (cursor, scroll));
        assert!(footer(&mut app).contains("2/4"));
    }

    #[test]
    fn a_reload_finds_the_same_match_in_the_new_diff() {
        let before = hunk("f", " x\n needle a\n needle b\n");
        let mut app = app(&before);
        search_for(&mut app, "needle");
        press(&mut app, Char('n'));
        assert_eq!(cursor_new_line(&app), Some(3));

        // A line added above it: the row moves, and the number is what's kept.
        let after = hunk("f", " z\n needle q\n needle b\n needle c\n");
        app.reload_in_place(parse(&after));
        frame(&mut app);
        assert_eq!(cursor_new_line(&app), Some(3));
        assert!(footer(&mut app).contains("2/3"), "{}", footer(&mut app));
    }

    #[test]
    fn a_reload_that_removes_every_match_ends_the_search() {
        let mut app = app(TWO_FILES);
        search_for(&mut app, "needle");
        app.reload_in_place(parse(&hunk("a.txt", " nothing\n")));
        frame(&mut app);
        assert!(!app.searching());
    }

    #[test]
    fn stepping_from_a_moved_cursor_starts_at_the_cursor() {
        let raw = hunk("f", " needle\n a\n b\n needle\n c\n needle\n");
        let mut app = app(&raw);
        search_for(&mut app, "needle");
        for _ in 0..4 {
            press(&mut app, Char('j'));
        }
        assert_eq!(cursor_new_line(&app), Some(5));
        press(&mut app, Char('n'));
        assert_eq!(cursor_new_line(&app), Some(6));
        for _ in 0..4 {
            press(&mut app, Char('k'));
        }
        assert_eq!(cursor_new_line(&app), Some(2));
        press(&mut app, Char('N'));
        assert_eq!(cursor_new_line(&app), Some(1));
    }

    /// Painting stops short of what can't be on screen, which mustn't be noticed.
    #[test]
    fn every_row_of_a_very_long_line_is_painted_however_far_down_it_is() {
        let raw = hunk("f", &format!(" {}\n", "needle ".repeat(4000)));
        let mut app = app(&raw);
        search_for(&mut app, "needle");
        for _ in 0..3000 {
            app.press_for_test(Char('n'), false);
        }
        let shown = matches(&mut app);
        assert!(shown.len() > 40, "{}", shown.len());
        assert_eq!(current(&mut app), ["needle"]);
    }

    #[test]
    fn a_big_diff_is_scanned_in_slices_and_typing_starts_it_over() {
        let body = " needle in a haystack of rows\n".repeat(400_000);
        let mut app = app(&hunk("f", &body));
        frame(&mut app);
        press(&mut app, Char('/'));
        app.press_for_test(Char('n'), false);
        app.advance_search();
        let search = app.find.search.as_ref().unwrap();
        assert!(!search.done(), "one slice shouldn't have reached the end");
        assert_eq!(app.search_wait(), Some(Duration::ZERO));
        assert!(app.search_count().unwrap().ends_with('…'));
        app.press_for_test(Char('e'), false);
        assert_eq!(app.find.search.as_ref().unwrap().total(), 0);
    }

    #[test]
    fn the_prompt_shows_the_end_of_a_query_too_long_for_the_footer() {
        let mut app = app(TWO_FILES);
        frame(&mut app);
        press(&mut app, Char('/'));
        for _ in 0..100 {
            press(&mut app, Char('a'));
        }
        press(&mut app, Char('语'));
        assert!(footer(&mut app).contains('语'));
    }

    #[test]
    fn the_two_themes_have_their_own_match_colors() {
        let colors = |theme: DiffTheme| [theme.match_bg(), theme.current_match_bg()];
        let all: HashMap<_, _> = [DiffTheme::GitHubDark, DiffTheme::GitHubLight]
            .into_iter()
            .flat_map(colors)
            .map(|color| (format!("{color:?}"), ()))
            .collect();
        assert_eq!(all.len(), 4);
    }
}
