//! riffnav's own diff renderer: git's hunks in, styled ratatui text out.
//!
//! Each row is built knowing its line numbers, so the [`LineMap`] comments anchor
//! to comes straight from the rows rather than being read back out of output.
//!
//! Rendering happens on [`jobs`], since a big file takes far longer than a key
//! can wait. [`RenderCache`] holds the results and is only ever touched by the UI
//! thread: a job works from its own copy of what it needs, and a result is only
//! taken if the job is still wanted, so one that a resize or a reload has
//! overtaken can't land.

mod copies;
mod highlight;
mod jobs;
mod layout;
mod marks;
mod moves;
mod starts;
mod tokens;
mod words;

use std::collections::HashMap;
use std::mem::size_of;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use twox_hash::XxHash3_128;

pub use self::copies::Jump;
use self::copies::{CopyIndex, Git};
pub use self::highlight::warm_up;
use self::highlight::{HunkStart, Run};
use self::jobs::Jobs;
use self::layout::{COPY_MARK, Content, MOVE_MARK, WRAP_MARK};
pub use self::marks::Marks;
use self::moves::MoveIndex;
pub use self::moves::Target;
use self::starts::StartReader;
use self::tokens::Tokens;
use self::words::Changes;
use crate::autodiff::Tree;
use crate::comment::anchor::Source;
use crate::comment::render::CommentBlock;
use crate::comment::store::{Anchor, Comment};
use crate::comment::{LineMap, render as comment_render};
use crate::diff::{self, FileDiff, Row};
use crate::search::Hit;
use crate::theme::DiffTheme;

/// Word-wrap setting shared by the height measurement and the per-frame render,
/// so both lay text out identically. `trim: false` keeps leading whitespace
/// (diff indentation) on wrapped rows.
const WRAP: Wrap = Wrap { trim: false };

/// How long a frame waits for a render before going ahead without it. Long enough
/// that an ordinary file is drawn once and correctly, and short enough not to be
/// felt.
#[cfg(not(test))]
pub const WAIT: Duration = Duration::from_millis(25);
/// Long enough to mean "until it's done", so a test reads a frame's render straight
/// after it. The tests of a render that's late say so with [`RenderCache::wait`].
#[cfg(test)]
pub const WAIT: Duration = Duration::from_secs(60);

/// Roughly what the cached renders may take up. Going over drops the ones looked at
/// longest ago, and the file on screen stays whatever its size.
const MAX_CACHE_BYTES: usize = 256 << 20;
/// A render bigger than this is left to a render thread to free.
const BIG_BYTES: usize = 1 << 20;
/// How long quitting gives the render threads to stop. They check often, so it
/// takes something stuck to use it up.
const STOP_GRACE: Duration = Duration::from_millis(50);

/// Lay a file out with nothing else going on, for the tests of the layout.
#[cfg(test)]
fn laid_out(
    file: &FileDiff,
    moves: &moves::FileMoves,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
    starts: &[HunkStart],
    side_by_side: bool,
) -> (Text<'static>, LineMap) {
    let cancelled = AtomicBool::new(false);
    let rows = diff::rows(file);
    let syntax = highlight::highlight(file, &rows, theme, starts, &cancelled);
    let changes = words::changes(&rows, &syntax.tokens, moves, &cancelled);
    let content = Content {
        rows: &rows,
        syntax: &syntax.runs,
        changes: &changes,
        moves,
        copies: &Default::default(),
        cancelled: &cancelled,
    };
    let lay_out = if side_by_side {
        layout::side_by_side
    } else {
        layout::unified
    };
    let (text, map) = lay_out(file, &content, width, theme, tab_width);
    (text, map.with_changes(diff::change_ids(&rows)))
}

/// Render one file as a unified diff, `width` columns wide.
#[cfg(test)]
pub fn unified(
    file: &FileDiff,
    moves: &moves::FileMoves,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
    starts: &[HunkStart],
) -> (Text<'static>, LineMap) {
    laid_out(file, moves, width, theme, tab_width, starts, false)
}

/// Render one file side by side, `width` columns wide.
#[cfg(test)]
pub fn side_by_side(
    file: &FileDiff,
    moves: &moves::FileMoves,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
    starts: &[HunkStart],
) -> (Text<'static>, LineMap) {
    laid_out(file, moves, width, theme, tab_width, starts, true)
}

/// A rendered file, ready to drop into the diff viewport.
pub struct Rendered {
    pub text: Text<'static>,
    /// Scrollable height in rows at the keyed `width`: the exact line count for
    /// side-by-side (each panel is wrapped when it's drawn), or the wrapped row
    /// count for unified (long lines are left whole for the viewer to wrap).
    pub height: u16,
    /// Unified only: cumulative wrapped-row offset before each source line, with
    /// a trailing total — `row_offsets[i]` is the first screen row of line `i`.
    /// `None` for side-by-side, where lines map one-to-one to rows.
    ///
    /// This is what keeps scrolling cheap. ratatui's `Paragraph` re-wraps every
    /// row from the top up to the scroll offset on each frame (`O(scroll)`), so
    /// deep scrolling in a long diff slows to a crawl. With these offsets the
    /// viewer seeks straight to the line under the scroll position and wraps only
    /// the visible window — see [`Rendered::visible`].
    row_offsets: Option<Vec<u32>>,
    /// The diff line numbers behind each line of `text`, kept aligned through
    /// comment splicing. Drives the line cursor and comment anchoring.
    pub line_map: LineMap,
    /// Where each spliced comment thread landed, in row order — what `]` and `[`
    /// step through, and how the cursor names the comment it's sitting on.
    pub comment_rows: Vec<CommentBlock>,
    /// Per row of [`diff::rows`], the line it starts on and the side it's on, or
    /// `None` for a row with no code. The way back from a search match to the line
    /// it's on, which is the other way round from `line_map`.
    row_starts: Vec<Option<RowStart>>,
    /// The first line of each change, in order: what `J` and `K` stop on. Worked
    /// out from the final lines, so comments, move notes and wrapping can't throw
    /// it off, and every new render has its own.
    pub changes: Vec<usize>,
}

/// Where a row's code starts in a render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RowStart {
    pub line: usize,
    /// 0 for the left panel or a unified line, 1 for the right panel.
    pub side: usize,
}

/// Index each row's first line. Runs after splicing, so the lines are final.
fn row_starts(map: &LineMap) -> Vec<Option<RowStart>> {
    let mut starts = Vec::new();
    for (line, sources) in map.all_sources().iter().enumerate() {
        for (side, source) in sources.iter().enumerate() {
            let Some(source) = source else { continue };
            let row = source.row as usize;
            if row >= starts.len() {
                starts.resize(row + 1, None);
            }
            starts[row].get_or_insert(RowStart { line, side });
        }
    }
    starts
}

/// The first line of each change. Side by side, a change's first line can show
/// any of its rows, so it's found by walking the lines rather than from its rows.
fn change_starts(map: &LineMap) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut last = None;
    for line in 0..map.all_sources().len() {
        let change = map.change_on(line);
        if change.is_some() && change != last {
            starts.push(line);
            last = change;
        }
    }
    starts
}

impl Rendered {
    /// The number of lines in the render (not screen rows; see [`Self::row_of`]).
    pub fn lines(&self) -> usize {
        self.text.lines.len()
    }

    /// The numbered line behind `line`, which may be `line` itself: the note above
    /// a moved or copied block stands for its first line, and a wrapped side-by-side
    /// row for the row it continues. Anything else unnumbered is left as it is.
    pub fn code_line(&self, line: usize) -> usize {
        let numbered = |at: usize| self.line_map.get(at).anchor().is_some();
        let has = |at: usize, wanted: &dyn Fn(&str) -> bool| {
            let line = self.text.lines.get(at);
            line.is_some_and(|l| l.spans.iter().any(|s| wanted(&s.content)))
        };
        if numbered(line) {
            return line;
        }
        if has(line, &|text| text.starts_with([MOVE_MARK, COPY_MARK])) {
            return line + 1;
        }
        let mut at = line;
        while at > 0 && !numbered(at) && has(at - 1, &|text| text.chars().eq([WRAP_MARK])) {
            at -= 1;
        }
        at
    }

    /// Where to scroll this render to show what `old` shows at `scroll`, going by
    /// the first numbered line in view. The top of the file stays the top.
    fn scroll_like(&self, old: &Rendered, scroll: u16) -> Option<u16> {
        if scroll == 0 {
            return None;
        }
        let top = old.line_at(scroll);
        let (line, anchor) =
            (top..old.lines()).find_map(|line| Some((line, old.line_map.get(line).anchor()?)))?;
        let here = self.row_of(self.line_map.row_for(anchor)?);
        let above = i32::from(old.row_of(line)) - i32::from(scroll);
        Some((i32::from(here) - above).clamp(0, i32::from(u16::MAX)) as u16)
    }

    /// The first screen row of line `line`, for scrolling a cursor into view.
    pub fn row_of(&self, line: usize) -> u16 {
        match &self.row_offsets {
            None => line.min(u16::MAX as usize) as u16,
            Some(offsets) => offsets
                .get(line)
                .or(offsets.last())
                .map_or(0, |r| (*r).min(u16::MAX as u32) as u16),
        }
    }

    /// The line occupying screen row `row` — the inverse of [`Self::row_of`],
    /// used to park the cursor after a page jump.
    pub fn line_at(&self, row: u16) -> usize {
        match &self.row_offsets {
            None => (row as usize).min(self.text.lines.len().saturating_sub(1)),
            Some(offsets) => match offsets.binary_search(&u32::from(row)) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            }
            .min(self.text.lines.len().saturating_sub(1)),
        }
    }

    /// Where `row` of [`diff::rows`] starts in this render.
    pub fn row_start(&self, row: u32) -> Option<RowStart> {
        self.row_starts.get(row as usize).copied().flatten()
    }

    /// The screen row `hit` starts on: lower than [`Self::row_of`] its line when
    /// the line wraps, as a long one will. `width` is the width of the render.
    pub fn row_of_hit(&self, hit: Hit, width: u16) -> Option<u16> {
        let first = self.row_start(hit.row)?.line;
        match &self.row_offsets {
            // Side-by-side wrapped the row itself, onto lines of its own. Comments
            // can land between them, so a line that isn't this row's is stepped
            // over rather than taken for the end.
            None => (first..self.lines())
                .find(|&line| {
                    let shows = |s: &Source| s.row == hit.row && hit.start < s.end_char;
                    self.line_map.sources(line).iter().flatten().any(shows)
                })
                .map(|line| self.row_of(line)),
            // Unified leaves wrapping to ratatui, so ask how many rows the line
            // takes up to the match. That can be a row short, if the rest of the
            // word would have pushed it down, which the padding around a landing
            // absorbs. Laying out the whole line to find the cell would be exact,
            // but a minified line would need a screen thousands of rows tall.
            Some(_) => {
                let source = self.line_map.sources(first)[0]?;
                let line = &self.text.lines[first];
                let mut spans = line.spans[..source.first_span].to_vec();
                let mut left = hit.start as usize + 1;
                for span in &line.spans[source.first_span..] {
                    let chars = span.content.chars().count();
                    if chars >= left {
                        let end = span.content.char_indices().nth(left).map(|(i, _)| i);
                        let text = &span.content[..end.unwrap_or(span.content.len())];
                        spans.push(Span::styled(text.to_string(), span.style));
                        break;
                    }
                    spans.push(span.clone());
                    left -= chars;
                }
                let rows = Paragraph::new(Line::from(spans))
                    .wrap(WRAP)
                    .line_count(width)
                    .max(1);
                let below = (rows - 1).min(u16::MAX as usize) as u16;
                Some(self.row_of(first).saturating_add(below))
            }
        }
    }

    /// A `Paragraph` showing just the rows visible at `scroll` for a viewport
    /// `height` rows tall, so per-frame work is `O(height)` rather than
    /// `O(scroll)`. The returned widget owns its (cloned) slice of lines, so it
    /// outlives the borrow of `self`, and `marks` are painted on those copies.
    ///
    /// `scroll` must already be clamped to `self.height`.
    pub fn visible(&self, scroll: u16, height: u16, marks: Option<&Marks>) -> Paragraph<'static> {
        let lines = &self.text.lines;
        let shown = |from: usize, to: usize, reach: u32| -> Vec<Line<'static>> {
            let mut shown = lines[from..to].to_vec();
            if let Some(marks) = marks {
                for (at, line) in shown.iter_mut().enumerate() {
                    marks.paint(line, &self.line_map.sources(from + at), reach);
                }
            }
            shown
        };
        match &self.row_offsets {
            // Side-by-side: one source line per row, slice it directly.
            None => {
                let start = (scroll as usize).min(lines.len());
                let end = start.saturating_add(height as usize).min(lines.len());
                Paragraph::new(Text::from(shown(start, end, u32::MAX)))
            }
            // Unified: find the source line holding wrapped row `scroll`, take it
            // plus enough following lines to fill the viewport (each contributes
            // at least one row), and let ratatui wrap just that window. The
            // sub-offset skips the part of the first line already scrolled past.
            Some(offsets) => {
                let start = match offsets.binary_search(&u32::from(scroll)) {
                    Ok(i) => i,
                    Err(i) => i.saturating_sub(1),
                };
                // `offsets[start] <= scroll` by construction, so this can't wrap.
                let sub = (u32::from(scroll) - offsets[start]) as u16;
                let end = start.saturating_add(height as usize + 1).min(lines.len());
                let rows = sub.saturating_add(height).saturating_add(1);
                let reach = marks.map_or(0, |marks| marks.reach(rows));
                Paragraph::new(Text::from(shown(start, end, reach)))
                    .wrap(WRAP)
                    .scroll((sub, 0))
            }
        }
    }
}

/// The most columns `text` can take. A character is at most two wide, and what
/// counts as one here is sure to be one: laying out text is what's being avoided,
/// and its idea of width can go by more than a character at a time.
fn most_columns(text: &str) -> usize {
    if text.is_ascii() {
        return text.len();
    }
    let narrow =
        |c: char| c.is_ascii() || matches!(c, '\u{2200}'..='\u{22ff}' | '\u{2500}'..='\u{257f}');
    text.chars().map(|c| if narrow(c) { 1 } else { 2 }).sum()
}

/// The rows `line` wraps onto. Nearly every line fits, as lines are padded out to
/// the width, so that's checked for first: wrapping them all is most of the work
/// of a render that has no syntax to color.
fn rows_of(line: &Line<'static>, width: u16) -> u32 {
    let most: usize = line.spans.iter().map(|s| most_columns(&s.content)).sum();
    if most <= usize::from(width) {
        return 1;
    }
    Paragraph::new(Text::from(line.clone()))
        .wrap(WRAP)
        .line_count(width)
        .max(1) as u32
}

/// Measure a render at `width`: its scrollable height and, for unified output,
/// the per-line wrapped-row offsets (see [`Rendered::row_offsets`]). Side-by-side
/// is already wrapped, so each line is exactly one row. Stops short once
/// `cancelled` is set.
fn measure(
    text: &Text<'static>,
    width: u16,
    side_by_side: bool,
    cancelled: &AtomicBool,
) -> (u16, Option<Vec<u32>>) {
    if side_by_side {
        let height = text.lines.len().min(u16::MAX as usize) as u16;
        return (height, None);
    }
    // Wrap each line on its own — `Wrap` resets at every newline, so a line's
    // wrapped height is independent of its neighbours and the offsets sum exactly
    // to what the full paragraph would lay out.
    let mut offsets = Vec::with_capacity(text.lines.len() + 1);
    let mut acc: u32 = 0;
    for line in &text.lines {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        offsets.push(acc);
        acc = acc.saturating_add(rows_of(line, width));
    }
    offsets.push(acc);
    (acc.min(u16::MAX as u32) as u16, Some(offsets))
}

/// Splice `comments` into a fresh render and measure the result.
/// Measurement has to come after splicing: comment rows change the wrapped
/// height and shift every row offset below them.
fn splice_and_measure(
    mut text: Text<'static>,
    mut map: LineMap,
    comments: &CommentLayer<'_>,
    width: u16,
    side_by_side: bool,
    theme: DiffTheme,
    cancelled: &AtomicBool,
) -> Rendered {
    let comment_rows = comment_render::splice(
        &mut text,
        &mut map,
        &comments.threads,
        comments.file,
        comments.me,
        width,
        comments.diff_hash,
        theme,
    );
    let (height, row_offsets) = measure(&text, width, side_by_side, cancelled);
    let row_starts = row_starts(&map);
    let changes = change_starts(&map);
    Rendered {
        text,
        height,
        row_offsets,
        line_map: map,
        comment_rows,
        row_starts,
        changes,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    file: usize,
    width: u16,
    side_by_side: bool,
    theme: DiffTheme,
}

/// The comments to draw into one file's render.
///
/// Bundled rather than passed loose so [`RenderCache::ensure`] keeps a readable
/// signature, and so the borrow of the comment store stays separate from the
/// mutable borrow of the cache.
pub struct CommentLayer<'a> {
    /// Threads for this file, from `CommentStore::threads`.
    pub threads: Vec<(Anchor, Vec<&'a Comment>)>,
    /// The file's tree path, for labelling a thread whose anchor has vanished.
    pub file: &'a str,
    /// The name this window writes its own comments under, so a note the reader
    /// left is colored apart from one an agent did.
    pub me: &'a str,
    /// `review::file_hash` of the file's current diff, for staleness marking.
    pub diff_hash: u128,
    /// Bumped whenever the comment store changes. A cached render built at an
    /// older revision is re-spliced, not rendered again, before it's handed out.
    pub rev: u64,
}

impl CommentLayer<'_> {
    /// A layer with nothing in it, for callers with comments turned off.
    pub fn none() -> Self {
        Self {
            threads: Vec::new(),
            file: "",
            me: "",
            diff_hash: 0,
            rev: 0,
        }
    }

    /// A copy for a job to take with it.
    fn owned(&self) -> Comments {
        let thread = |(anchor, comments): &(Anchor, Vec<&Comment>)| {
            (*anchor, comments.iter().map(|&c| c.clone()).collect())
        };
        Comments {
            threads: self.threads.iter().map(thread).collect(),
            file: self.file.to_string(),
            me: self.me.to_string(),
            diff_hash: self.diff_hash,
            rev: self.rev,
        }
    }
}

/// A [`CommentLayer`] that owns what's in it, for a render thread.
struct Comments {
    threads: Vec<(Anchor, Vec<Comment>)>,
    file: String,
    me: String,
    diff_hash: u128,
    rev: u64,
}

impl Comments {
    fn layer(&self) -> CommentLayer<'_> {
        CommentLayer {
            threads: self
                .threads
                .iter()
                .map(|(anchor, comments)| (*anchor, comments.iter().collect()))
                .collect(),
            file: &self.file,
            me: &self.me,
            diff_hash: self.diff_hash,
            rev: self.rev,
        }
    }
}

/// A file's lines before any comments go in.
struct Base {
    text: Text<'static>,
    map: LineMap,
}

/// A render as a job hands it back.
struct Made {
    /// The lines from before the comments, if any went in. Without any they're the
    /// render's own, and keeping a second copy would double what a file takes up.
    base: Option<Arc<Base>>,
    rendered: Rendered,
    /// Whether any comments went in.
    commented: bool,
    /// The comment revision it was spliced at.
    rev: u64,
    bytes: usize,
}

/// Put `comments` into a file's lines and measure the result.
fn made(
    text: Text<'static>,
    map: LineMap,
    comments: &Comments,
    key: &Key,
    keep_base: bool,
    cancelled: &AtomicBool,
) -> Made {
    let layer = comments.layer();
    let commented = !layer.threads.is_empty();
    let base = (commented && keep_base).then(|| {
        Arc::new(Base {
            text: text.clone(),
            map: map.clone(),
        })
    });
    let rendered = splice_and_measure(
        text,
        map,
        &layer,
        key.width,
        key.side_by_side,
        key.theme,
        cancelled,
    );
    let bytes = text_bytes(&rendered.text) + base.as_ref().map_or(0, |b| text_bytes(&b.text));
    Made {
        base,
        rendered,
        commented,
        rev: comments.rev,
        bytes,
    }
}

/// Roughly what `text` takes up. The line map and offsets are put at a flat rate.
fn text_bytes(text: &Text<'static>) -> usize {
    const ALLOCATION: usize = 16;
    let span = |s: &Span| size_of::<Span>() + s.content.len() + ALLOCATION;
    let line = |l: &Line| size_of::<Line>() + 64 + l.spans.iter().map(span).sum::<usize>();
    text.lines.iter().map(line).sum()
}

/// What came of a job.
enum Outcome {
    /// The file in its final shape without its syntax color, with the real render
    /// still to come.
    Preview(Made),
    /// The render of the file.
    Final(Made),
    /// New comments in a render that already exists.
    Spliced(Made),
    /// Putting new comments in failed, so the file keeps the ones it has.
    NotSpliced(u64),
}

struct Done {
    id: u64,
    outcome: Outcome,
    /// What went wrong, for a job that panicked.
    failure: Option<String>,
}

/// A job's way back to the cache.
struct Ticket {
    id: u64,
    cancelled: Arc<AtomicBool>,
    results: Sender<Done>,
}

impl Ticket {
    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn send(&self, outcome: Outcome, failure: Option<String>) {
        let id = self.id;
        // Nobody listening means the cache has gone, and the app with it.
        let _ = self.results.send(Done {
            id,
            outcome,
            failure,
        });
    }
}

/// What a panic said.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    let text = panic.downcast_ref::<&str>().copied();
    let text = text.or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    text.unwrap_or("unknown panic").to_string()
}

/// Everything a render needs, owned, so nothing can change underneath it.
struct Spec {
    key: Key,
    diff: FileDiff,
    moves: Arc<MoveIndex>,
    copies: Arc<CopyIndex>,
    tab_width: u8,
    comments: Comments,
    reader: Option<Arc<StartReader>>,
    #[cfg(test)]
    test: test_support::Hooks,
}

impl Spec {
    /// Render the file. Something always lands, however this goes, as a jump can
    /// be waiting on it: a panic is met with the text without its colors, or failing
    /// that a line saying so.
    fn run(&self, ticket: &Ticket) {
        let render = AssertUnwindSafe(|| self.render(ticket));
        let Err(panic) = panic::catch_unwind(render) else {
            return;
        };
        let plain = AssertUnwindSafe(|| self.plain(&diff::rows(&self.diff), &ticket.cancelled));
        let made = panic::catch_unwind(plain).unwrap_or_else(|_| self.unrenderable());
        let failure = format!("{}: {}", self.diff.path(), panic_message(&*panic));
        ticket.send(Outcome::Final(made), Some(failure));
    }

    fn render(&self, ticket: &Ticket) {
        #[cfg(test)]
        self.test.at_start(self, &ticket.cancelled);
        let rows = diff::rows(&self.diff);
        // A file that won't be colored is as cheap to finish as to preview.
        if highlight::language(&self.diff, &rows).is_some() {
            let plain = self.plain(&rows, &ticket.cancelled);
            if ticket.stopped() {
                return;
            }
            ticket.send(Outcome::Preview(plain), None);
        }
        #[cfg(test)]
        self.test.before_final(&ticket.cancelled);

        let starts = match &self.reader {
            Some(reader) => match reader.read(&self.diff, &ticket.cancelled) {
                Some(starts) => starts,
                None => return,
            },
            None => Arc::default(),
        };
        let made = self.colored(&rows, &starts, &ticket.cancelled);
        if !ticket.stopped() {
            ticket.send(Outcome::Final(made), None);
        }
    }

    fn colored(&self, rows: &[Row], starts: &[HunkStart], cancelled: &AtomicBool) -> Made {
        let theme = self.key.theme;
        let syntax = highlight::highlight(&self.diff, rows, theme, starts, cancelled);
        let moves = self.moves.file(self.key.file);
        let changes = words::changes(rows, &syntax.tokens, moves, cancelled);
        self.lay_out(rows, &syntax.runs, &changes, cancelled)
    }

    /// The file without parsing it, which is most of the work saved. Nothing the
    /// parser finds changes a character or moves a line, so this is the final render
    /// but for its styles, and whatever points into one is good for the other.
    fn plain(&self, rows: &[Row], cancelled: &AtomicBool) -> Made {
        #[cfg(test)]
        self.test.at_plain(self);
        let moves = self.moves.file(self.key.file);
        let tokens = vec![Tokens::default(); rows.len()];
        let changes = words::changes(rows, &tokens, moves, cancelled);
        self.lay_out(rows, &vec![Vec::new(); rows.len()], &changes, cancelled)
    }

    fn lay_out(
        &self,
        rows: &[Row],
        syntax: &[Vec<Run>],
        changes: &Changes,
        cancelled: &AtomicBool,
    ) -> Made {
        let Key {
            file,
            width,
            side_by_side,
            theme,
        } = self.key;
        let content = Content {
            rows,
            syntax,
            changes,
            moves: self.moves.file(file),
            copies: self.copies.file(file),
            cancelled,
        };
        let lay_out = if side_by_side {
            layout::side_by_side
        } else {
            layout::unified
        };
        let (text, map) = lay_out(&self.diff, &content, width, theme, self.tab_width);
        let map = map.with_changes(diff::change_ids(rows));
        made(text, map, &self.comments, &self.key, true, cancelled)
    }

    fn unrenderable(&self) -> Made {
        let text = Text::from(format!("  Couldn't render {}", self.diff.path()));
        let none = Comments {
            threads: Vec::new(),
            file: String::new(),
            me: String::new(),
            diff_hash: 0,
            rev: self.comments.rev,
        };
        let cancelled = AtomicBool::new(false);
        made(
            text,
            LineMap::default(),
            &none,
            &self.key,
            false,
            &cancelled,
        )
    }
}

/// What a re-splice starts from: the lines from before the comments, which are the
/// render's own when it has none.
enum Origin {
    Base(Arc<Base>),
    Shown(Arc<Rendered>),
}

/// Put `comments` into a render that already exists.
fn splice_again(origin: Origin, comments: &Comments, key: &Key, ticket: &Ticket) {
    let splice = AssertUnwindSafe(|| {
        let (text, map, keep_base) = match &origin {
            Origin::Base(base) => (base.text.clone(), base.map.clone(), false),
            Origin::Shown(render) => (render.text.clone(), render.line_map.clone(), true),
        };
        made(text, map, comments, key, keep_base, &ticket.cancelled)
    });
    match panic::catch_unwind(splice) {
        Ok(made) if !ticket.stopped() => ticket.send(Outcome::Spliced(made), None),
        Ok(_) => {}
        Err(panic) => {
            let failure = format!("{}: {}", comments.file, panic_message(&*panic));
            ticket.send(Outcome::NotSpliced(comments.rev), Some(failure));
        }
    }
}

/// One cached file render.
struct Entry {
    /// The lines to re-splice from, when they aren't the render's own. Keeping them
    /// means a comment added, edited, or deleted only re-splices — an `O(lines)`
    /// pass — instead of highlighting the file again. That matters because the
    /// filesystem watcher can fire repeatedly while an agent writes a batch of notes.
    base: Option<Arc<Base>>,
    rendered: Arc<Rendered>,
    /// The comment revision `rendered` was spliced at.
    rev: u64,
    /// Whether `rendered` has comments in it.
    commented: bool,
    /// Not the last word on this file, until the real render takes its place: it's
    /// without its syntax color, or from before the copies were known.
    preview: bool,
    /// The [`RenderCache::copies_rev`] it was made from.
    copies_rev: u64,
    /// Roughly what this takes up.
    bytes: usize,
    /// When it was last on screen, as a count of the cache's own.
    used: u64,
    /// What went wrong making it, until that's been reported. It waits for the file
    /// to be on screen: said during a prefetch, it would be about nothing in view.
    failure: Option<String>,
}

/// A job that's wanted. One that isn't in the list has had its result given up on.
struct Running {
    id: u64,
    key: Key,
    cancelled: Arc<AtomicBool>,
    /// A prefetch, which anything for the screen comes before.
    idle: bool,
    started: Instant,
    /// The [`RenderCache::copies_rev`] it's working from.
    copies_rev: u64,
}

/// The search for copied code, running on a thread of its own as it can take
/// seconds, which would keep a render thread from the file on screen.
struct CopyJob {
    cancelled: Arc<AtomicBool>,
    thread: JoinHandle<Option<CopyIndex>>,
}

impl CopyJob {
    fn start(
        files: Vec<FileDiff>,
        moves: Option<Arc<MoveIndex>>,
        git: Option<Git>,
    ) -> Option<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let search = move || {
            // A block that's a move is a move, whether moves are being shown or not.
            let moves = moves.unwrap_or_else(|| Arc::new(MoveIndex::build(&files)));
            copies::build(&files, &moves, git.as_ref(), &stop)
        };
        // The panic hook goes by the thread, so it can't take the app down.
        let thread = thread::Builder::new()
            .name("render-copies".to_string())
            .spawn(search)
            .ok()?;
        Some(Self { cancelled, thread })
    }
}

/// What the file on screen is missing.
enum Need {
    Nothing,
    Render,
    Splice(Origin),
}

/// The file a render is of, and how it's to look: `ensure` and `prefetch` ask the
/// same thing about different files.
pub struct Request<'a> {
    pub file: usize,
    pub diff: &'a FileDiff,
    pub width: u16,
    pub side_by_side: bool,
    pub theme: DiffTheme,
    pub comments: CommentLayer<'a>,
}

impl Request<'_> {
    fn key(&self) -> Key {
        Key {
            file: self.file,
            width: self.width,
            side_by_side: self.side_by_side,
            theme: self.theme,
        }
    }
}

/// Caches renders keyed by `(file, width, side_by_side, theme)` so revisiting a
/// file — or redrawing at the same size and theme — never renders it again.
/// Switching themes re-renders (and caches separately), so toggling back is
/// instant. What's kept is bounded by [`MAX_CACHE_BYTES`].
pub struct RenderCache {
    entries: HashMap<Key, Entry>,
    tab_width: u8,
    detect_moves: bool,
    moves: Arc<MoveIndex>,
    detect_copies: bool,
    /// What's known so far of what was copied, which starts with nothing and is
    /// replaced when [`Self::copy_job`] is done. Width and theme don't come into it,
    /// so a new one of either only costs its render.
    copies: Arc<CopyIndex>,
    /// Counts the times [`Self::copies`] has changed what a file looks like.
    copies_rev: u64,
    /// The hash of the diff the copies are for.
    copies_of: u128,
    /// Whether [`Self::start_copies`] has anything to do.
    copies_wanted: bool,
    copy_job: Option<CopyJob>,
    /// Jobs given up on, kept so quitting can wait for their git to be stopped.
    copy_jobs_stopping: Vec<JoinHandle<Option<CopyIndex>>>,
    /// The render on screen before a copy search changed it, to keep the view on the
    /// same code. See [`Self::kept_scroll`].
    replaced: Option<(Key, Arc<Rendered>)>,
    /// Where git runs, when riffnav ran it for the diff.
    repo: Option<PathBuf>,
    /// Set when riffnav ran git itself, so there's a file behind the diff to read.
    /// Outlives [`Self::clear`]: a new width has the same hunks.
    starts: Option<Arc<StartReader>>,
    jobs: Jobs,
    running: Vec<Running>,
    next_id: u64,
    sender: Sender<Done>,
    results: Receiver<Done>,
    /// How long to wait for a render, from when it was asked for.
    wait: Duration,
    /// What the entries take up, going by their own counts.
    bytes: usize,
    /// Counts up each time a file is asked for, for [`Entry::used`].
    clock: u64,
    /// The file on screen, which isn't dropped however big it is.
    shown: Option<Key>,
    /// The last file prefetched, so one too big to stay in the cache isn't made over
    /// and over.
    prefetched: Option<Key>,
    /// Whether to prefetch at all.
    prefetching: bool,
    #[cfg(test)]
    test: test_support::Hooks,
}

impl RenderCache {
    /// Columns between tab stops, which is what search counts tabs in.
    pub fn tab_width(&self) -> usize {
        usize::from(self.tab_width)
    }

    pub fn new(tab_width: u8, detect_moves: bool, detect_copies: bool) -> Self {
        let (sender, results) = mpsc::channel();
        Self {
            entries: HashMap::new(),
            tab_width,
            detect_moves,
            moves: Arc::default(),
            detect_copies,
            copies: Arc::default(),
            copies_rev: 0,
            copies_of: 0,
            copies_wanted: false,
            copy_job: None,
            copy_jobs_stopping: Vec::new(),
            replaced: None,
            repo: None,
            starts: None,
            jobs: Jobs::default(),
            running: Vec::new(),
            next_id: 0,
            sender,
            results,
            wait: WAIT,
            bytes: 0,
            clock: 0,
            shown: None,
            prefetched: None,
            // Tests want what they pressed for, with nothing else going on.
            prefetching: !cfg!(test),
            #[cfg(test)]
            test: test_support::Hooks::default(),
        }
    }

    /// Start each hunk's parser from the real file. Only for a diff from git run
    /// here: one piped in could be of anything.
    pub fn read_starts(&mut self) {
        self.starts
            .get_or_insert_with(|| Arc::new(StartReader::new(".")));
        self.repo.get_or_insert_with(|| PathBuf::from("."));
    }

    #[cfg(test)]
    fn read_starts_in(&mut self, repo: &std::path::Path) {
        self.starts = Some(Arc::new(StartReader::new(repo)));
        self.repo = Some(repo.to_path_buf());
    }

    /// Set how long a render is waited for, from when it was asked for.
    pub fn wait(&mut self, wait: Duration) {
        self.wait = wait;
    }

    /// How long a frame may wait for renders.
    pub fn waits(&self) -> Duration {
        self.wait
    }

    /// Whether anything is being rendered.
    pub fn busy(&self) -> bool {
        !self.running.is_empty()
    }

    /// Whether anything is on its way, so the event loop has to keep waking up to
    /// pick it up. The search for copies counts here and not in [`Self::busy`], as a
    /// prefetch has no reason to wait for it.
    pub fn expecting(&self) -> bool {
        self.busy() || self.copy_job.is_some()
    }

    /// What went wrong rendering the file on screen, the first time it's asked.
    pub fn take_failure(&mut self) -> Option<String> {
        self.entries.get_mut(&self.shown?)?.failure.take()
    }

    /// Take in whatever has finished. Whether anything did.
    pub fn collect(&mut self) -> bool {
        let mut any = self.collect_copies();
        while let Ok(done) = self.results.try_recv() {
            any |= self.land(done);
        }
        any
    }

    /// Take in a finished job, if it's still wanted. Whether it was.
    fn land(&mut self, done: Done) -> bool {
        let Some(at) = self.running.iter().position(|job| job.id == done.id) else {
            self.jobs.discard(done);
            return false;
        };
        let (key, copies_rev) = (self.running[at].key, self.running[at].copies_rev);
        match done.outcome {
            Outcome::Preview(made) => {
                // A file only gets one, and it never replaces the real thing.
                if self.entries.contains_key(&key) {
                    self.jobs.discard(made);
                } else {
                    self.keep(key, made, true, copies_rev);
                }
                return true;
            }
            Outcome::Final(made) => {
                self.keep(key, made, false, copies_rev);
                if self.running[at].idle {
                    self.prefetched = Some(key);
                }
            }
            Outcome::Spliced(mut made) => {
                // Dropped to make room while this was on its way, and without what
                // to splice from next time there's no keeping it.
                if let Some(entry) = self.entries.get(&key) {
                    if made.commented && made.base.is_none() {
                        made.base = entry.base.clone();
                    }
                    let copies_rev = entry.copies_rev;
                    self.keep(key, made, false, copies_rev);
                }
            }
            Outcome::NotSpliced(rev) => {
                if let Some(entry) = self.entries.get_mut(&key) {
                    entry.rev = rev;
                }
            }
        }
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.failure = entry.failure.take().or(done.failure);
        }
        self.running.remove(at);
        true
    }

    /// Put `made` in the cache, and make room for it.
    fn keep(&mut self, key: Key, made: Made, preview: bool, copies_rev: u64) {
        let entry = Entry {
            base: made.base,
            rendered: Arc::new(made.rendered),
            rev: made.rev,
            commented: made.commented,
            preview,
            copies_rev,
            bytes: made.bytes,
            used: self.clock,
            failure: None,
        };
        self.bytes += entry.bytes;
        if let Some(old) = self.entries.insert(key, entry) {
            self.bytes -= old.bytes;
            if old.copies_rev != copies_rev && self.shown == Some(key) {
                self.replaced = Some((key, old.rendered.clone()));
            }
            self.retire(vec![old]);
        }
        self.trim();
    }

    /// Drop the renders looked at longest ago until what's left fits.
    fn trim(&mut self) {
        let mut doomed = Vec::new();
        while self.bytes > self.limit() {
            let shown = self.shown;
            let oldest = self.entries.iter().filter(|(key, _)| Some(**key) != shown);
            let Some(key) = oldest
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.bytes;
                doomed.push(entry);
            }
        }
        self.retire(doomed);
    }

    fn limit(&self) -> usize {
        #[cfg(test)]
        if let Some(limit) = self.test.limit {
            return limit;
        }
        MAX_CACHE_BYTES
    }

    /// Free `entries`. A big render takes long enough that it's done elsewhere.
    fn retire(&mut self, entries: Vec<Entry>) {
        if entries.iter().any(|entry| entry.bytes > BIG_BYTES) {
            self.jobs.discard(entries);
        }
    }

    /// Give up on the jobs `doomed` picks. What they've done so far is lost, and
    /// whatever they still hand in is dropped.
    fn cancel(&mut self, doomed: impl Fn(&Running) -> bool) {
        self.running.retain(|job| {
            let dropped = doomed(job);
            if dropped {
                job.cancelled.store(true, Ordering::Relaxed);
            }
            !dropped
        });
    }

    /// Stop everything, for quitting.
    pub fn stop(&mut self) {
        let deadline = Instant::now() + STOP_GRACE;
        self.cancel(|_| true);
        self.cancel_copies();
        self.jobs.stop(STOP_GRACE);
        // Each stops its git before it ends, so that's waited for like the renders.
        while self.copy_jobs_stopping.iter().any(|job| !job.is_finished())
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Give up on what's being rendered for the screen, which has nothing to show.
    pub fn release(&mut self) {
        self.shown = None;
        self.cancel(|job| !job.idle);
    }

    /// Work out what moved across `files`, and drop the render of any file that
    /// shows it differently now. That can be a file other than the one that
    /// changed: a block moved out of it may no longer have anywhere to have gone.
    pub fn index_moves(&mut self, files: &[FileDiff]) {
        self.forget_copies(files);
        if !self.detect_moves {
            return;
        }
        let moves = MoveIndex::build(files);
        let old = self.moves.clone();
        self.remove(|key| old.file(key.file) != moves.file(key.file));
        self.moves = Arc::new(moves);
        self.cancel(|_| true); // They're working from the old moves.
    }

    /// Drop what's known of the copies in another diff, as its rows are no good for
    /// `files`. The same diff again, as `r` often brings, keeps it all.
    fn forget_copies(&mut self, files: &[FileDiff]) {
        if !self.detect_copies {
            return;
        }
        let diff = files.iter().fold(0_u128, |hash, file| {
            let file = XxHash3_128::oneshot(file.raw.as_bytes());
            hash.wrapping_mul(31).wrapping_add(file)
        });
        if diff != self.copies_of {
            self.copies_of = diff;
            self.copies_wanted = true;
            self.cancel_copies();
            self.take_copies(CopyIndex::default());
        }
    }

    /// Whether there's a new diff to look for copies in.
    pub fn wants_copies(&self) -> bool {
        self.copies_wanted
    }

    /// Start looking for copies in `files`. Not before the first frame has been
    /// drawn, which this must never hold up. `tree` is where the files they leave
    /// alone can be read.
    pub fn start_copies(&mut self, files: &[FileDiff], tree: Option<Tree>) {
        self.copies_wanted = false;
        if !copies::worth_trying(files) {
            return;
        }
        let moves = self.detect_moves.then(|| self.moves.clone());
        let git = self.repo.clone().map(|repo| Git { repo, tree });
        self.copy_job = CopyJob::start(files.to_vec(), moves, git);
    }

    fn cancel_copies(&mut self) {
        self.copy_jobs_stopping.retain(|job| !job.is_finished());
        if let Some(job) = self.copy_job.take() {
            job.cancelled.store(true, Ordering::Relaxed);
            self.copy_jobs_stopping.push(job.thread);
        }
    }

    /// Take in the copies if they're ready. Whether that changed anything.
    fn collect_copies(&mut self) -> bool {
        if self
            .copy_job
            .as_ref()
            .is_none_or(|job| !job.thread.is_finished())
        {
            return false;
        }
        let found = self.copy_job.take().and_then(|job| job.thread.join().ok());
        match found.flatten() {
            Some(index) => self.take_copies(index),
            None => false,
        }
    }

    /// Show `index` from now on. A file it changes keeps its render on screen until
    /// the new one is ready, so the note arrives without the file going blank.
    /// Whether it changed anything.
    fn take_copies(&mut self, index: CopyIndex) -> bool {
        let old = self.copies.clone();
        let changed = |file: usize| !old.file(file).looks_like(index.file(file));
        let any = self.entries.keys().any(|key| changed(key.file))
            || self.running.iter().any(|job| changed(job.key.file));
        self.copies_rev += 1;
        self.cancel(|job| changed(job.key.file));
        for (key, entry) in &mut self.entries {
            entry.preview |= changed(key.file);
        }
        if any {
            self.prefetched = None;
        }
        self.copies = Arc::new(index);
        any
    }

    /// Wait for the search for copies, for a test to say when it lands.
    #[cfg(test)]
    pub(crate) fn wait_for_copies(&mut self) {
        while self.copy_job.is_some() {
            self.collect_copies();
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Say where `anchor` jumps to, for what a test can't get from a diff.
    #[cfg(test)]
    pub(crate) fn set_jump_for_test(&mut self, file: usize, anchor: Anchor, jump: Jump) {
        let mut index = CopyIndex::default();
        index.set_jump_for_test(file, anchor, jump);
        self.copies = Arc::new(index);
    }

    /// The other end of the move or copy `anchor` in `file` is part of, if any.
    pub fn jump(&self, file: usize, anchor: Anchor) -> Option<Jump> {
        let moved = self.moves.file(file).target(anchor).map(Jump::Moved);
        moved.or_else(|| self.copies.file(file).jump(anchor).cloned())
    }

    /// The scroll that keeps the view on the same code, if the render of what's on
    /// screen has just had its copies' notes put in.
    pub fn kept_scroll(
        &mut self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
        scroll: u16,
    ) -> Option<u16> {
        let (key, old) = self.replaced.take()?;
        let kept = (key.file == file
            && key.width == width
            && key.side_by_side == side_by_side
            && key.theme == theme)
            .then(|| self.entries.get(&key)?.rendered.scroll_like(&old, scroll));
        self.jobs.discard(old);
        kept.flatten()
    }

    /// Get the file on screen under way, and wait for it for up to [`Self::wait`]
    /// from when it was first asked for, so an ordinary file is here by the time
    /// the frame is drawn. What it's still missing after that lands later, and
    /// until then [`Self::get`] has whatever there is of it so far.
    pub fn ensure(&mut self, request: &Request<'_>) {
        let key = request.key();
        self.shown = Some(key);
        self.clock += 1;
        self.collect();
        self.cancel(|job| !job.idle && job.key != key);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.used = self.clock;
        }
        loop {
            match self.need(&key, request) {
                Need::Nothing => return,
                Need::Render => self.start_render(key, request, false),
                Need::Splice(origin) => self.start_splice(key, origin, request),
            }
            if !self.wait_for(&key) {
                return;
            }
        }
    }

    /// What `key` is missing, given what's already being done about it.
    fn need(&mut self, key: &Key, request: &Request<'_>) -> Need {
        let comments = &request.comments;
        let need = match self.entries.get_mut(key) {
            None => Need::Render,
            Some(entry) if entry.preview => Need::Render,
            Some(entry) if entry.rev == comments.rev => Need::Nothing,
            // The revision is the whole store's, so it usually moves for a comment on
            // some other file.
            Some(entry) if !entry.commented && comments.threads.is_empty() => {
                entry.rev = comments.rev;
                Need::Nothing
            }
            Some(entry) => Need::Splice(match &entry.base {
                Some(base) => Origin::Base(base.clone()),
                None => Origin::Shown(entry.rendered.clone()),
            }),
        };
        if !matches!(need, Need::Nothing) {
            // The screen comes first, unless it's after the prefetch's own file.
            self.cancel(|job| job.idle && job.key != *key);
        }
        need
    }

    fn start_render(&mut self, key: Key, request: &Request<'_>, idle: bool) {
        if let Some(job) = self.running.iter_mut().find(|job| job.key == key) {
            // Already on it, which a prefetch may be. It's urgent now.
            if job.idle && !idle {
                job.idle = false;
                job.started = Instant::now();
            }
            return;
        }
        let spec = Spec {
            key,
            diff: request.diff.clone(),
            moves: self.moves.clone(),
            copies: self.copies.clone(),
            tab_width: self.tab_width,
            comments: request.comments.owned(),
            reader: self.starts.clone(),
            #[cfg(test)]
            test: self.test.clone(),
        };
        let ticket = self.ticket(key, idle);
        if idle {
            self.jobs.idle(move || spec.run(&ticket));
        } else {
            self.jobs.urgent(move || spec.run(&ticket));
        }
    }

    fn start_splice(&mut self, key: Key, origin: Origin, request: &Request<'_>) {
        if self.running.iter().any(|job| job.key == key) {
            return; // It lands at a revision that's looked at again then.
        }
        let comments = request.comments.owned();
        let ticket = self.ticket(key, false);
        self.jobs
            .urgent(move || splice_again(origin, &comments, &key, &ticket));
    }

    fn ticket(&mut self, key: Key, idle: bool) -> Ticket {
        let id = self.next_id;
        self.next_id += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.running.push(Running {
            id,
            key,
            cancelled: cancelled.clone(),
            idle,
            started: Instant::now(),
            copies_rev: self.copies_rev,
        });
        Ticket {
            id,
            cancelled,
            results: self.sender.clone(),
        }
    }

    /// Wait out what's left of [`Self::wait`] for `key`'s job. A preview doesn't end
    /// it, so a file that's nearly there is drawn once, in color. Whether the job
    /// finished.
    fn wait_for(&mut self, key: &Key) -> bool {
        while let Some(job) = self.running.iter().find(|job| job.key == *key) {
            let left = (job.started + self.wait).saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.results.recv_timeout(left) {
                Ok(done) => {
                    self.land(done);
                }
                Err(_) => return false,
            }
        }
        true
    }

    /// With the file on screen done, render the one likely to be next. Anything for
    /// the screen stops it, and a prefetch for a file that has stopped being next
    /// goes the same way.
    pub fn prefetch(&mut self, request: &Request<'_>) {
        if !self.prefetching {
            return;
        }
        let key = request.key();
        self.cancel(|job| job.idle && job.key != key);
        let done = self.entries.get(&key).is_some_and(|entry| !entry.preview);
        if !done && self.prefetched != Some(key) && !self.busy() {
            self.start_render(key, request, true);
        }
    }

    pub fn get(
        &self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
    ) -> Option<&Rendered> {
        self.entry(file, width, side_by_side, theme)
            .map(|e| e.rendered.as_ref())
    }

    /// The copies behind what [`Self::get`] returns, which it can trail while its
    /// new render is being made. Its lines change when these do.
    pub fn copies_rev(
        &self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
    ) -> Option<u64> {
        self.entry(file, width, side_by_side, theme)
            .map(|e| e.copies_rev)
    }

    /// The comment revision behind what [`Self::get`] returns. The render on show
    /// can trail the store's while the new one is being spliced.
    pub fn rev(
        &self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
    ) -> Option<u64> {
        self.entry(file, width, side_by_side, theme).map(|e| e.rev)
    }

    fn entry(
        &self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
    ) -> Option<&Entry> {
        self.entries.get(&Key {
            file,
            width,
            side_by_side,
            theme,
        })
    }

    /// Drop the renders `doomed` picks.
    fn remove(&mut self, doomed: impl Fn(&Key) -> bool) {
        let keys: Vec<Key> = self
            .entries
            .keys()
            .filter(|&key| doomed(key))
            .copied()
            .collect();
        let mut entries = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.bytes;
                entries.push(entry);
            }
        }
        self.retire(entries);
    }

    /// Drop all cached renders (e.g. after a resize changes the wrap width), along
    /// with any on the way, which were made for what's being replaced.
    pub fn clear(&mut self) {
        self.cancel(|_| true);
        self.remove(|_| true);
        self.prefetched = None;
    }

    /// Drop the cached renders for a single file (every width/theme variant),
    /// e.g. after its diff is refreshed in place. Other files stay cached.
    pub fn invalidate(&mut self, file: usize) {
        self.cancel(|job| job.key.file == file);
        self.remove(|key| key.file == file);
        self.prefetched = None;
    }

    /// Seed a render directly, for tests that want exact text rather than a diff.
    #[cfg(test)]
    pub(crate) fn insert_for_test(
        &mut self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
        text: Text<'static>,
    ) {
        self.insert_for_test_with_comments(
            file,
            width,
            side_by_side,
            theme,
            text,
            &CommentLayer::none(),
        );
    }

    /// As [`Self::insert_for_test`], but splicing a comment layer in — the seam
    /// for snapshotting comment rendering.
    #[cfg(test)]
    pub(crate) fn insert_for_test_with_comments(
        &mut self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
        text: Text<'static>,
        comments: &CommentLayer<'_>,
    ) {
        let map = LineMap::from_gutters(&text);
        let rendered = splice_and_measure(
            text.clone(),
            map.clone(),
            comments,
            width,
            side_by_side,
            theme,
            &AtomicBool::new(false),
        );
        self.entries.insert(
            Key {
                file,
                width,
                side_by_side,
                theme,
            },
            Entry {
                base: Some(Arc::new(Base { text, map })),
                rendered: Arc::new(rendered),
                rev: comments.rev,
                commented: true,
                preview: false,
                copies_rev: self.copies_rev,
                bytes: 0,
                used: 0,
                failure: None,
            },
        );
    }
}

/// Ways for a test to get a job to stop where it can be seen, or to fail.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Condvar, Mutex, PoisonError};

    use super::*;

    /// A file that panics in the render, and one that goes on to panic in the
    /// plain render that's the fallback for that.
    pub const PANICS: &str = "panics-rendering.rs";
    pub const ALWAYS_PANICS: &str = "panics-always.rs";

    /// Holds jobs until it's opened, or they're cancelled.
    #[derive(Clone, Default)]
    pub struct Gate(Arc<(Mutex<bool>, Condvar)>);

    impl Gate {
        pub fn open(&self) {
            let (open, changed) = &*self.0;
            *open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            changed.notify_all();
        }

        fn pass(&self, cancelled: &AtomicBool) {
            let (open, changed) = &*self.0;
            let mut is_open = open.lock().unwrap_or_else(PoisonError::into_inner);
            while !*is_open && !cancelled.load(Ordering::Relaxed) {
                let wait = changed.wait_timeout(is_open, Duration::from_millis(1));
                is_open = wait.unwrap_or_else(PoisonError::into_inner).0;
            }
        }
    }

    #[derive(Clone, Default)]
    pub struct Hooks {
        /// Stops a job before it has made anything.
        pub start: Option<Gate>,
        /// Stops a job between its preview and its real render.
        pub real: Option<Gate>,
        /// Each file a job started on, in order.
        pub started: Arc<Mutex<Vec<usize>>>,
        /// Stands in for [`MAX_CACHE_BYTES`].
        pub limit: Option<usize>,
    }

    impl Hooks {
        pub(super) fn at_start(&self, spec: &Spec, cancelled: &AtomicBool) {
            let started = &self.started;
            let mut started = started.lock().unwrap_or_else(PoisonError::into_inner);
            started.push(spec.key.file);
            drop(started);
            if let Some(gate) = &self.start {
                gate.pass(cancelled);
            }
            let path = spec.diff.path();
            assert!(path != PANICS && path != ALWAYS_PANICS, "injected");
        }

        pub(super) fn before_final(&self, cancelled: &AtomicBool) {
            if let Some(gate) = &self.real {
                gate.pass(cancelled);
            }
        }

        pub(super) fn at_plain(&self, spec: &Spec) {
            assert_ne!(spec.diff.path(), ALWAYS_PANICS, "injected");
        }
    }

    impl RenderCache {
        pub fn hooks(&mut self) -> &mut Hooks {
            &mut self.test
        }

        pub fn prefetch_for_test(&mut self) {
            self.prefetching = true;
        }

        pub fn is_preview(&self, file: usize, width: u16, side_by_side: bool) -> bool {
            let theme = DiffTheme::GitHubDark;
            let entry = self.entry(file, width, side_by_side, theme);
            entry.is_some_and(|e| e.preview)
        }

        pub fn stored_bytes(&self) -> usize {
            self.bytes
        }

        /// Block until no job is left running.
        pub fn finish(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(60);
            while self.busy() {
                assert!(Instant::now() < deadline, "a job never finished");
                if let Ok(done) = self.results.recv_timeout(Duration::from_millis(10)) {
                    self.land(done);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use crate::diff::parse;
    use crate::theme::DiffTheme::GitHubDark;

    const WIDTH: u16 = 60;

    fn removal() -> FileDiff {
        parse(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +0,0 @@\n\
             -fn alpha_value() {\n-    first_call(1);\n",
        )
        .remove(0)
    }

    fn addition(line: u32) -> FileDiff {
        let raw = format!(
            "diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -0,0 +{line},2 @@\n\
             +fn alpha_value() {{\n+    first_call(1);\n"
        );
        parse(&raw).remove(0)
    }

    fn unrelated() -> FileDiff {
        parse("diff --git a/c b/c\n--- a/c\n+++ b/c\n@@ -1 +1 @@\n-old\n+new\n").remove(0)
    }

    fn request(file: usize, diff: &FileDiff, width: u16, theme: DiffTheme) -> Request<'_> {
        Request {
            file,
            diff,
            width,
            side_by_side: false,
            theme,
            comments: CommentLayer::none(),
        }
    }

    fn rendered(cache: &mut RenderCache, files: &[FileDiff]) {
        for (at, file) in files.iter().enumerate() {
            cache.ensure(&request(at, file, WIDTH, GitHubDark));
        }
    }

    fn cached(cache: &RenderCache, file: usize) -> bool {
        cache.get(file, WIDTH, false, GitHubDark).is_some()
    }

    fn says(cache: &RenderCache, file: usize, text: &str) -> bool {
        let rendered = cache.get(file, WIDTH, false, GitHubDark).unwrap();
        rendered
            .text
            .lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains(text))
    }

    #[test]
    fn a_change_in_one_file_redraws_the_other_end_of_its_moves() {
        let mut files = [removal(), addition(1), unrelated()];
        let mut cache = RenderCache::new(4, true, false);
        cache.index_moves(&files);
        rendered(&mut cache, &files);
        assert!(says(&cache, 0, "moved to b:1"));

        files[1] = addition(7);
        cache.invalidate(1);
        cache.index_moves(&files);
        assert!(!cached(&cache, 0), "its hint points at the old line");
        assert!(cached(&cache, 2), "nothing about this file changed");

        rendered(&mut cache, &files);
        assert!(says(&cache, 0, "moved to b:7"));
    }

    #[test]
    fn moves_can_be_turned_off() {
        let files = [removal(), addition(1)];
        let mut cache = RenderCache::new(4, false, false);
        cache.index_moves(&files);
        rendered(&mut cache, &files);
        assert!(!says(&cache, 0, "moved to"));
    }

    /// A repository whose last commit changes a line inside a block comment, and
    /// that commit's diff.
    fn edit_in_a_comment(dir: &std::path::Path) -> FileDiff {
        let git = |args: &[&str]| {
            let mut git = std::process::Command::new("git");
            git.args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(dir);
            git.output().unwrap()
        };
        std::fs::create_dir_all(dir).unwrap();
        git(&["init", "-q"]);
        let text = format!("/*\n{}*/\n", "prose\n".repeat(40));
        std::fs::write(dir.join("a.rs"), &text).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "x"]);
        std::fs::write(dir.join("a.rs"), text.replacen("prose", "let x = 1;", 1)).unwrap();
        git(&["commit", "-qam", "x"]);
        let out = git(&["show", "HEAD"]);
        parse(&String::from_utf8_lossy(&out.stdout)).remove(0)
    }

    /// The colors of the `let x = 1;` row, which are all one if it's a comment.
    fn colors_of_the_edit(cache: &RenderCache, width: u16, theme: DiffTheme) -> usize {
        let rendered = cache.get(0, width, false, theme).unwrap();
        let line = rendered.text.lines.iter().find(|line| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            text.contains("let x")
        });
        let mut colors: Vec<_> = line.unwrap().spans.iter().map(|s| s.style.fg).collect();
        colors.retain(|fg| fg.is_some());
        colors.dedup();
        colors.len()
    }

    #[test]
    fn hunk_starts_reach_the_render_and_outlive_a_resize_and_a_theme_change() {
        let dir = std::env::temp_dir().join(format!("riffnav-render-{}", std::process::id()));
        let file = edit_in_a_comment(&dir);
        let mut cache = RenderCache::new(4, false, false);
        cache.read_starts_in(&dir);
        let draw = |cache: &mut RenderCache, width, theme| {
            cache.ensure(&request(0, &file, width, theme));
        };

        draw(&mut cache, WIDTH, GitHubDark);
        let right = colors_of_the_edit(&cache, WIDTH, GitHubDark);
        assert_eq!(right, 3, "the gutter, one color of comment, the padding");

        // With the repository gone, the only way to stay right is the cache.
        std::fs::remove_dir_all(&dir).unwrap();
        cache.clear();
        draw(&mut cache, WIDTH + 10, GitHubDark);
        assert_eq!(colors_of_the_edit(&cache, WIDTH + 10, GitHubDark), right);
        draw(&mut cache, WIDTH, DiffTheme::GitHubLight);
        assert_eq!(
            colors_of_the_edit(&cache, WIDTH, DiffTheme::GitHubLight),
            right
        );
        assert!(!cache.busy());
    }

    #[test]
    fn a_cache_without_a_reader_leaves_hunks_starting_afresh() {
        let dir = std::env::temp_dir().join(format!("riffnav-piped-{}", std::process::id()));
        let file = edit_in_a_comment(&dir);
        let mut cache = RenderCache::new(4, false, false);
        cache.ensure(&request(0, &file, WIDTH, GitHubDark));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!cache.busy());
        assert!(colors_of_the_edit(&cache, WIDTH, GitHubDark) > 1);
    }

    /// The windowed render (`Rendered::visible`) must be pixel-for-pixel identical
    /// to the naive "wrap the whole text, then scroll" it replaces — at every
    /// scroll offset. Equivalence here is what lets us swap the `O(scroll)` render
    /// for an `O(viewport)` one without changing what the user sees.
    #[test]
    fn visible_window_matches_full_wrap_at_every_scroll() {
        let (width, view_h) = (24u16, 6u16);
        // A mix of blank, short, and several-widths-long lines so wrapping varies
        // line to line and the per-line offsets get a real workout.
        let lines: Vec<Line> = (0..40)
            .map(|i: usize| Line::from(format!("{i:02}:{}", "w".repeat(i * 7 % 61))))
            .collect();
        let text = Text::from(lines);

        let (height, row_offsets) = measure(&text, width, false, &AtomicBool::new(false));
        // The per-line offsets must sum to the same height a single full wrap gives.
        let full = Paragraph::new(text.clone()).wrap(WRAP).line_count(width) as u16;
        assert_eq!(height, full, "summed offsets disagree with a full wrap");

        let rendered = Rendered {
            text: text.clone(),
            height,
            row_offsets,
            line_map: LineMap::default(),
            comment_rows: Vec::new(),
            row_starts: Vec::new(),
            changes: Vec::new(),
        };
        let draw = |paragraph: Paragraph| -> Buffer {
            let mut term = Terminal::new(TestBackend::new(width, view_h)).unwrap();
            term.draw(|f| f.render_widget(paragraph, f.area())).unwrap();
            term.backend().buffer().clone()
        };

        for scroll in 0..=height {
            let got = draw(rendered.visible(scroll, view_h, None));
            let want = draw(Paragraph::new(text.clone()).wrap(WRAP).scroll((scroll, 0)));
            assert_eq!(
                got, want,
                "visible window differs from full wrap at scroll {scroll}"
            );
        }
    }

    use std::time::Duration;

    use crate::comment::{Comment, Side};

    use super::test_support::{ALWAYS_PANICS, Gate, PANICS};

    fn rust(path: &str, body: &str) -> FileDiff {
        let old = body.lines().filter(|l| !l.starts_with('+')).count();
        let new = body.lines().filter(|l| !l.starts_with('-')).count();
        parse(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,{old} +1,{new} @@\n{body}"
        ))
        .remove(0)
    }

    fn file(path: &str) -> FileDiff {
        rust(path, " let a = 1;\n-let b = 2;\n+let b = 3;\n")
    }

    /// A cache that doesn't wait for anything, with its jobs held at the start.
    fn held() -> (RenderCache, Gate) {
        let mut cache = RenderCache::new(4, false, false);
        let gate = Gate::default();
        cache.wait(Duration::ZERO);
        cache.hooks().start = Some(gate.clone());
        (cache, gate)
    }

    fn started(cache: &mut RenderCache) -> Vec<usize> {
        cache.hooks().started.lock().unwrap().clone()
    }

    /// Wait until `files` jobs have got going.
    fn until_started(cache: &mut RenderCache, files: usize) {
        for _ in 0..10_000 {
            if started(cache).len() >= files {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the job never started");
    }

    fn note(file: &str, line: u32, body: &str) -> Comment {
        Comment {
            id: format!("id{line}"),
            file: file.to_string(),
            side: Side::New,
            line,
            body: body.to_string(),
            author: "agent".to_string(),
            created: 0,
            reply_to: None,
            diff_hash: None,
        }
    }

    fn with_comments<'a>(diff: &'a FileDiff, notes: &'a [Comment], rev: u64) -> Request<'a> {
        let anchor = |c: &Comment| Anchor {
            side: c.side,
            line: c.line,
        };
        Request {
            comments: CommentLayer {
                threads: notes.iter().map(|c| (anchor(c), vec![c])).collect(),
                file: diff.path(),
                me: "me",
                diff_hash: 0,
                rev,
            },
            ..request(0, diff, WIDTH, GitHubDark)
        }
    }

    fn all_text(cache: &RenderCache, file: usize) -> String {
        let render = cache.get(file, WIDTH, false, GitHubDark).unwrap();
        let lines = render.text.lines.iter();
        lines
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn a_late_render_lands_on_its_own() {
        let (mut cache, gate) = held();
        let diff = file("a.rs");
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        assert!(!cached(&cache, 0));
        assert!(cache.busy());

        gate.open();
        cache.finish();
        assert!(cached(&cache, 0));
        assert!(!cache.is_preview(0, WIDTH, false));
        assert!(!cache.busy());
    }

    #[test]
    fn a_slow_file_shows_its_plain_text_until_the_real_render() {
        let mut cache = RenderCache::new(4, false, false);
        let gate = Gate::default();
        cache.wait(Duration::ZERO);
        cache.hooks().real = Some(gate.clone());
        let diff = file("a.rs");
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        while !cached(&cache, 0) {
            std::thread::sleep(Duration::from_millis(1));
            cache.collect();
        }
        assert!(cache.is_preview(0, WIDTH, false));
        let plain = all_text(&cache, 0);

        gate.open();
        cache.finish();
        assert!(!cache.is_preview(0, WIDTH, false));
        assert_eq!(all_text(&cache, 0), plain);
    }

    #[test]
    fn a_file_that_will_not_be_colored_has_no_preview() {
        let mut cache = RenderCache::new(4, false, false);
        let gate = Gate::default();
        cache.wait(Duration::ZERO);
        cache.hooks().real = Some(gate.clone());
        let diff = file("notes.unknownformat");
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        gate.open();
        cache.finish();
        assert!(cached(&cache, 0));
    }

    /// Everything anything keeps an index into: if these agree, the cursor, scroll
    /// and search can't tell the plain render from the real one.
    fn assert_same_shape(plain: &Rendered, real: &Rendered, what: &str) {
        let text = |r: &Rendered| -> Vec<String> {
            let line = |l: &Line| l.spans.iter().map(|s| s.content.to_string()).collect();
            r.text.lines.iter().map(line).collect()
        };
        assert_eq!(text(plain), text(real), "{what}");
        assert_eq!(plain.height, real.height, "{what}");
        assert_eq!(plain.row_offsets, real.row_offsets, "{what}");
        assert_eq!(plain.changes, real.changes, "{what}");
        assert_eq!(plain.row_starts, real.row_starts, "{what}");
        for line in 0..real.lines() {
            assert_eq!(
                plain.line_map.get(line),
                real.line_map.get(line),
                "{what}: line {line}"
            );
            assert_eq!(
                plain
                    .line_map
                    .sources(line)
                    .map(|s| s.map(|s| (s.row, s.first_char, s.end_char))),
                real.line_map
                    .sources(line)
                    .map(|s| s.map(|s| (s.row, s.first_char, s.end_char))),
                "{what}: line {line}"
            );
        }
    }

    /// Lines laid out by what the parser found would move when the colors arrive.
    #[test]
    fn the_plain_render_has_the_shape_of_the_real_one() {
        let long = "let long = call(argument, another_argument); ".repeat(20);
        let bodies = [
            // Paired by similarity, opposite each other side by side.
            " fn a() {\n-    let total = sum(values);\n-    other();\n+    let total = sum(all_values);\n+    log();\n+    other();\n }\n",
            // Tabs, whose stops depend on what's before them.
            "-\tlet a\t= 1;\n+\tlet ab\t= 1;\n",
            // Wide and combining characters, in strings and out.
            "-let s = \"日本語のテキスト\";\n+let s = \"日本語の文章だよ\"; // コメント 🙂\n",
            // A line too long to compare, or to color.
            &format!("-{long}\n+{long}x\n"),
            &format!("-{}\n+{}y\n", "z".repeat(3000), "z".repeat(3000)),
            // Whitespace at the end, flagged on the added side.
            "-let a = 1;\n+let a = 1;   \n",
            // A rewrite with nothing to pair.
            "-aaaa\n-bbbb\n+cccc\n+dddd\n+eeee\n",
            // A string opened on one side only.
            "-let a = \"x;\n+let a = 1;\n let b = 2;\n",
            // No newline at the end, on each side.
            "-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n",
        ];
        let notes = [note("a.rs", 1, "a note")];
        let moved = [
            rust(
                "a.rs",
                "-fn moved_away() {\n-    first_call(1);\n-    second_call(2);\n-    third_call(3);\n-}\n",
            ),
            rust(
                "b.rs",
                "+fn moved_away() {\n+    first_call(1);\n+    second_call(2);\n+    third_call(3);\n+}\n",
            ),
        ];
        let index = Arc::new(MoveIndex::build(&moved));
        let cases: Vec<(FileDiff, Arc<MoveIndex>, usize)> = bodies
            .iter()
            .map(|body| (rust("a.rs", body), Arc::default(), 0))
            .chain([
                (moved[0].clone(), index.clone(), 0),
                (moved[1].clone(), index.clone(), 1),
            ])
            .collect();
        let cancelled = AtomicBool::new(false);
        for (diff, moves, at) in cases {
            for width in [24, 25, 60, 61, 120] {
                for side_by_side in [false, true] {
                    let key = Key {
                        file: at,
                        width,
                        side_by_side,
                        theme: GitHubDark,
                    };
                    let layer = with_comments(&diff, &notes, 1);
                    let spec = Spec {
                        key,
                        diff: diff.clone(),
                        moves: moves.clone(),
                        copies: Arc::default(),
                        tab_width: 4,
                        comments: layer.comments.owned(),
                        reader: None,
                        test: Default::default(),
                    };
                    let rows = diff::rows(&diff);
                    let plain = spec.plain(&rows, &cancelled).rendered;
                    let real = spec.colored(&rows, &[], &cancelled).rendered;
                    let what = format!("{} at {width}, sbs {side_by_side}", diff.raw);
                    assert_same_shape(&plain, &real, &what);
                }
            }
        }
    }

    #[test]
    fn a_job_a_resize_overtook_never_lands() {
        let (mut cache, gate) = held();
        let diff = file("a.rs");
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        until_started(&mut cache, 1);
        cache.clear();
        assert!(!cache.busy());
        gate.open();
        std::thread::sleep(Duration::from_millis(20));
        assert!(!cache.collect());
        assert!(!cached(&cache, 0));
    }

    #[test]
    fn a_change_of_width_or_theme_leaves_the_old_job_behind() {
        let (mut cache, gate) = held();
        let diff = file("a.rs");
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        cache.ensure(&request(0, &diff, WIDTH + 2, GitHubDark));
        cache.ensure(&request(0, &diff, WIDTH + 2, DiffTheme::GitHubLight));
        gate.open();
        cache.finish();
        assert!(!cached(&cache, 0));
        assert!(cache.get(0, WIDTH + 2, false, GitHubDark).is_none());
        assert!(
            cache
                .get(0, WIDTH + 2, false, DiffTheme::GitHubLight)
                .is_some()
        );
    }

    #[test]
    fn invalidating_a_file_drops_what_was_being_made_of_it() {
        let (mut cache, gate) = held();
        let old = file("a.rs");
        cache.ensure(&request(0, &old, WIDTH, GitHubDark));
        cache.invalidate(0);
        gate.open();
        let new = rust("a.rs", "-before\n+after\n");
        cache.ensure(&request(0, &new, WIDTH, GitHubDark));
        cache.finish();
        assert!(says(&cache, 0, "after"));
    }

    /// An agent's note arriving during a render isn't lost, and doesn't cost a second
    /// render either.
    #[test]
    fn a_comment_that_arrives_during_a_render_is_spliced_in_after_it() {
        let (mut cache, gate) = held();
        let diff = file("a.rs");
        cache.ensure(&with_comments(&diff, &[], 1));
        until_started(&mut cache, 1);
        let notes = [note("a.rs", 2, "arrived late")];
        cache.ensure(&with_comments(&diff, &notes, 2));
        gate.open();
        cache.finish();
        assert!(!says(&cache, 0, "arrived late"), "made before it existed");

        cache.ensure(&with_comments(&diff, &notes, 2));
        cache.finish();
        assert!(says(&cache, 0, "arrived late"));
        assert_eq!(cache.rev(0, WIDTH, false, GitHubDark), Some(2));
        assert_eq!(started(&mut cache).len(), 1, "one render, and a splice");
    }

    #[test]
    fn comments_can_come_and_go_without_doubling_up() {
        let mut cache = RenderCache::new(4, false, false);
        let diff = file("a.rs");
        let one = [note("a.rs", 2, "first")];
        let two = [note("a.rs", 2, "first"), note("a.rs", 1, "second")];
        let count = |cache: &RenderCache, what: &str| all_text(cache, 0).matches(what).count();

        cache.ensure(&with_comments(&diff, &[], 1));
        cache.ensure(&with_comments(&diff, &one, 2));
        assert_eq!(count(&cache, "first"), 1);
        cache.ensure(&with_comments(&diff, &two, 3));
        assert_eq!((count(&cache, "first"), count(&cache, "second")), (1, 1));
        cache.ensure(&with_comments(&diff, &one, 4));
        assert_eq!((count(&cache, "first"), count(&cache, "second")), (1, 0));
        cache.ensure(&with_comments(&diff, &[], 5));
        assert_eq!(count(&cache, "first"), 0);
        cache.ensure(&with_comments(&diff, &one, 6));
        assert_eq!(count(&cache, "first"), 1);
        assert_eq!(started(&mut cache).len(), 1);
    }

    #[test]
    fn a_comment_on_another_file_costs_this_one_nothing() {
        let mut cache = RenderCache::new(4, false, false);
        let diff = file("a.rs");
        cache.ensure(&with_comments(&diff, &[], 1));
        let before = cache
            .get(0, WIDTH, false, GitHubDark)
            .unwrap()
            .text
            .lines
            .as_ptr();
        cache.ensure(&with_comments(&diff, &[], 2));
        assert!(!cache.busy());
        let after = cache
            .get(0, WIDTH, false, GitHubDark)
            .unwrap()
            .text
            .lines
            .as_ptr();
        assert_eq!(before, after);
        assert_eq!(cache.rev(0, WIDTH, false, GitHubDark), Some(2));
    }

    #[test]
    fn a_panic_leaves_the_plain_text_and_says_so() {
        let mut cache = RenderCache::new(4, false, false);
        let diff = file(PANICS);
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        assert!(says(&cache, 0, "let b"));
        assert!(!cache.is_preview(0, WIDTH, false));
        let failure = cache.take_failure().unwrap();
        assert!(
            failure.contains(PANICS) && failure.contains("injected"),
            "{failure}"
        );

        // It's done with, or every frame would panic all over again.
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        assert_eq!(started(&mut cache).len(), 1);
    }

    #[test]
    fn a_panic_in_the_fallback_still_lands_something() {
        let mut cache = RenderCache::new(4, false, false);
        let diff = file(ALWAYS_PANICS);
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        assert!(says(&cache, 0, "Couldn't render"));
        assert!(!cache.busy());

        let next = file("next.rs");
        cache.ensure(&request(1, &next, WIDTH, GitHubDark));
        assert!(cached(&cache, 1), "the threads survived it");
    }

    #[test]
    fn the_cache_drops_what_was_looked_at_longest_ago() {
        let mut cache = RenderCache::new(4, false, false);
        let files: Vec<FileDiff> = (0..4).map(|at| file(&format!("{at}.rs"))).collect();
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        let one = cache.stored_bytes();
        cache.hooks().limit = Some(one * 2 + one / 2);
        cache.ensure(&request(1, &files[1], WIDTH, GitHubDark));
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        cache.ensure(&request(2, &files[2], WIDTH, GitHubDark));
        assert!(cached(&cache, 0), "seen more recently than 1");
        assert!(!cached(&cache, 1));
        assert!(cached(&cache, 2));
        assert!(cache.stored_bytes() <= one * 2 + one / 2);
    }

    #[test]
    fn the_file_on_screen_stays_however_big_it_is() {
        let mut cache = RenderCache::new(4, false, false);
        cache.hooks().limit = Some(1);
        let files = [file("a.rs"), file("b.rs")];
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        cache.ensure(&request(1, &files[1], WIDTH, GitHubDark));
        assert!(!cached(&cache, 0));
        assert!(cached(&cache, 1));
    }

    #[test]
    fn a_prefetch_waits_for_the_file_on_screen_and_then_stops() {
        let (mut cache, gate) = held();
        cache.prefetch_for_test();
        let files = [file("a.rs"), file("b.rs")];
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
        gate.open();
        cache.finish();
        assert_eq!(started(&mut cache), [0], "not while a is under way");

        cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
        cache.finish();
        assert_eq!(started(&mut cache), [0, 1]);
        assert!(cached(&cache, 1));
        cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
        assert!(!cache.busy());
    }

    #[test]
    fn a_prefetch_gives_way_to_the_file_on_screen() {
        let (mut cache, gate) = held();
        cache.prefetch_for_test();
        let files = [file("a.rs"), file("b.rs"), file("c.rs")];
        cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
        until_started(&mut cache, 1);

        // Held at the gate, and it's dropped for a file the prefetch isn't of.
        cache.hooks().start = None;
        cache.wait(Duration::from_secs(60));
        cache.ensure(&request(2, &files[2], WIDTH, GitHubDark));
        assert!(cached(&cache, 2));
        gate.open();
        std::thread::sleep(Duration::from_millis(20));
        cache.collect();
        assert!(!cached(&cache, 1), "cancelled, not finished");
    }

    #[test]
    fn a_prefetch_of_the_file_asked_for_carries_on_as_its_render() {
        let (mut cache, gate) = held();
        cache.prefetch_for_test();
        let files = [file("a.rs"), file("b.rs")];
        cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
        until_started(&mut cache, 1);
        cache.ensure(&request(1, &files[1], WIDTH, GitHubDark));
        gate.open();
        cache.finish();
        assert!(cached(&cache, 1));
        assert_eq!(started(&mut cache), [1], "no second job");
    }

    #[test]
    fn a_prefetch_too_big_to_keep_is_not_made_over_and_over() {
        let mut cache = RenderCache::new(4, false, false);
        cache.prefetch_for_test();
        let files = [file("a.rs"), file("b.rs")];
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        cache.hooks().limit = Some(1);
        for _ in 0..3 {
            cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
            cache.finish();
        }
        assert_eq!(started(&mut cache), [0, 1]);
    }

    /// Stepping through files quickly leaves one file to render. How little the
    /// others cost before they stop is for [`jobs`] and the cancel checks.
    #[test]
    fn stepping_through_files_only_keeps_the_last() {
        let (mut cache, gate) = held();
        let files: Vec<FileDiff> = (0..50).map(|at| file(&format!("{at}.rs"))).collect();
        for (at, diff) in files.iter().enumerate() {
            cache.ensure(&request(at, diff, WIDTH, GitHubDark));
        }
        assert_eq!(cache.running.len(), 1);
        gate.open();
        cache.finish();
        assert!(cached(&cache, 49));
        assert_eq!(cache.entries.len(), 1);
    }

    /// A wrapped line counted wrong puts every row below it out by one.
    #[test]
    fn counting_a_lines_rows_quickly_agrees_with_wrapping_it() {
        let samples = [
            "",
            "plain ascii that goes on for a while, past most widths",
            "   ⋮   2│code",
            "│".repeat(30).as_str().to_string().leak(),
            "日本語のテキストがここに続きます",
            "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}",
            "❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}",
            "👩\u{200d}💻👩\u{200d}💻👩\u{200d}💻👩\u{200d}💻",
            "لالالالالالالالالالا",
            "🇫🇮🇫🇮🇫🇮🇫🇮🇫🇮",
            "a\u{7}b\tc\u{1b}d\re",
            "   leading and   spaced   out    words     ",
        ];
        for sample in samples {
            for width in 1..48 {
                let line = Line::from(vec![Span::raw("  1⋮  1│"), Span::raw(sample)]);
                let slow = Paragraph::new(Text::from(line.clone()))
                    .wrap(WRAP)
                    .line_count(width)
                    .max(1) as u32;
                assert_eq!(rows_of(&line, width), slow, "{sample:?} at {width}");
            }
        }
    }
}
