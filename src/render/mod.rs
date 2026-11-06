//! riffnav's own diff renderer: git's hunks in, styled ratatui text out.
//!
//! Each row is built knowing its line numbers, so the [`LineMap`] comments anchor
//! to comes straight from the rows rather than being read back out of output.

mod highlight;
mod layout;
mod marks;
mod moves;
mod starts;
mod tokens;
mod words;

use std::collections::HashMap;

use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use twox_hash::XxHash3_128;

use self::highlight::HunkStart;
use self::layout::{Content, MOVE_MARK, WRAP_MARK};
pub use self::marks::Marks;
pub use self::moves::Target;
use self::moves::{FileMoves, MoveIndex};
use self::starts::StartReader;
use crate::comment::anchor::Source;
use crate::comment::render::CommentBlock;
use crate::comment::store::{Anchor, Comment};
use crate::comment::{LineMap, render as comment_render};
use crate::diff::{self, FileDiff};
use crate::search::Hit;
use crate::theme::DiffTheme;

/// Word-wrap setting shared by the height measurement and the per-frame render,
/// so both lay text out identically. `trim: false` keeps leading whitespace
/// (diff indentation) on wrapped rows.
const WRAP: Wrap = Wrap { trim: false };

/// Render one file as a unified diff, `width` columns wide.
pub fn unified(
    file: &FileDiff,
    moves: &FileMoves,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
    starts: &[HunkStart],
) -> (Text<'static>, LineMap) {
    let rows = diff::rows(file);
    let syntax = highlight::highlight(file, &rows, theme, starts);
    let changes = words::changes(&rows, &syntax.tokens, moves);
    let content = Content {
        rows: &rows,
        syntax: &syntax.runs,
        changes: &changes,
        moves,
    };
    layout::unified(file, &content, width, theme, tab_width)
}

/// Render one file side by side, `width` columns wide.
pub fn side_by_side(
    file: &FileDiff,
    moves: &FileMoves,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
    starts: &[HunkStart],
) -> (Text<'static>, LineMap) {
    let rows = diff::rows(file);
    let syntax = highlight::highlight(file, &rows, theme, starts);
    let changes = words::changes(&rows, &syntax.tokens, moves);
    let content = Content {
        rows: &rows,
        syntax: &syntax.runs,
        changes: &changes,
        moves,
    };
    layout::side_by_side(file, &content, width, theme, tab_width)
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

impl Rendered {
    /// The number of lines in the render (not screen rows; see [`Self::row_of`]).
    pub fn lines(&self) -> usize {
        self.text.lines.len()
    }

    /// The numbered line behind `line`, which may be `line` itself: the note above
    /// a moved block stands for the block's first line, and a wrapped side-by-side
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
        if has(line, &|text| text.starts_with(MOVE_MARK)) {
            return line + 1;
        }
        let mut at = line;
        while at > 0 && !numbered(at) && has(at - 1, &|text| text.chars().eq([WRAP_MARK])) {
            at -= 1;
        }
        at
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

/// Measure a render at `width`: its scrollable height and, for unified output,
/// the per-line wrapped-row offsets (see [`Rendered::row_offsets`]). Side-by-side
/// is already wrapped, so each line is exactly one row.
fn measure(text: &Text<'static>, width: u16, side_by_side: bool) -> (u16, Option<Vec<u32>>) {
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
        offsets.push(acc);
        let rows = Paragraph::new(Text::from(line.clone()))
            .wrap(WRAP)
            .line_count(width)
            .max(1) as u32;
        acc = acc.saturating_add(rows);
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
    let (height, row_offsets) = measure(&text, width, side_by_side);
    let row_starts = row_starts(&map);
    Rendered {
        text,
        height,
        row_offsets,
        line_map: map,
        comment_rows,
        row_starts,
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
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
}

/// One cached file render.
///
/// The pre-splice `base` is kept alongside the finished render so a comment
/// added, edited, or deleted only re-splices — an `O(lines)` pass — instead of
/// highlighting the file again. That matters because the filesystem watcher can
/// fire repeatedly while an agent writes a batch of notes.
struct Entry {
    base: Text<'static>,
    base_map: LineMap,
    rendered: Rendered,
    /// The comment revision `rendered` was spliced at.
    rev: u64,
    /// Hash of the diff text this was made from, which is what [`StartReader`]
    /// knows it by. Only set when there is a reader.
    source: u128,
    /// Made before the reader had answered, so its hunks started afresh and it's
    /// rendered again if the answer changes that.
    provisional: bool,
}

/// Caches renders keyed by `(file, width, side_by_side, theme)` so revisiting a
/// file — or redrawing at the same size and theme — never renders it again.
/// Switching themes re-renders (and caches separately), so toggling back is
/// instant.
pub struct RenderCache {
    entries: HashMap<Key, Entry>,
    tab_width: u8,
    detect_moves: bool,
    moves: MoveIndex,
    /// Set when riffnav ran git itself, so there's a file behind the diff to read.
    /// Outlives [`Self::clear`]: a new width has the same hunks.
    starts: Option<StartReader>,
}

impl RenderCache {
    /// Columns between tab stops, which is what search counts tabs in.
    pub fn tab_width(&self) -> usize {
        usize::from(self.tab_width)
    }

    pub fn new(tab_width: u8, detect_moves: bool) -> Self {
        Self {
            entries: HashMap::new(),
            tab_width,
            detect_moves,
            moves: MoveIndex::default(),
            starts: None,
        }
    }

    /// Start each hunk's parser from the real file. Only for a diff from git run
    /// here: one piped in could be of anything.
    pub fn read_starts(&mut self) {
        self.starts.get_or_insert_with(|| StartReader::new("."));
    }

    #[cfg(test)]
    fn read_starts_in(&mut self, repo: &std::path::Path) {
        self.starts = Some(StartReader::new(repo));
    }

    /// Whether a file's hunk starts are being worked out, so the event loop has
    /// to keep waking up to pick them up.
    pub fn reading_starts(&self) -> bool {
        self.starts.as_ref().is_some_and(StartReader::busy)
    }

    /// Collect hunk starts that have finished. The next [`Self::ensure`] redraws
    /// the file if they change it.
    pub fn drain_starts(&mut self) {
        if let Some(reader) = &mut self.starts {
            reader.drain();
        }
    }

    /// Work out what moved across `files`, and drop the render of any file that
    /// shows it differently now. That can be a file other than the one that
    /// changed: a block moved out of it may no longer have anywhere to have gone.
    pub fn index_moves(&mut self, files: &[FileDiff]) {
        if !self.detect_moves {
            return;
        }
        let moves = MoveIndex::build(files);
        let old = &self.moves;
        self.entries
            .retain(|key, _| old.file(key.file) == moves.file(key.file));
        self.moves = moves;
    }

    /// The other end of the move `anchor` in `file` is part of, if it moved.
    pub fn move_target(&self, file: usize, anchor: Anchor) -> Option<Target> {
        self.moves.file(file).target(anchor)
    }

    /// Render `diff` for the given key if not already cached, and make sure the
    /// cached render carries `comments` at their current revision.
    pub fn ensure(
        &mut self,
        file: usize,
        diff: &FileDiff,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
        comments: &CommentLayer<'_>,
    ) {
        let key = Key {
            file,
            width,
            side_by_side,
            theme,
        };
        self.settle(&key, diff);
        if !self.entries.contains_key(&key) {
            let source = self
                .starts
                .as_ref()
                .map_or(0, |_| XxHash3_128::oneshot(diff.raw.as_bytes()));
            let starts = self.starts.as_mut().map(|r| r.wait_for(source, diff));
            let provisional = matches!(starts, Some(None));
            let starts = starts.flatten();
            let hunks = starts.as_ref().map_or(&[][..], |s| s.as_slice());
            let moves = self.moves.file(file);
            let render = if side_by_side {
                self::side_by_side
            } else {
                unified
            };
            let (base, base_map) = render(diff, moves, width, theme, self.tab_width, hunks);
            let rendered = splice_and_measure(
                base.clone(),
                base_map.clone(),
                comments,
                width,
                side_by_side,
                theme,
            );
            self.entries.insert(
                key,
                Entry {
                    base,
                    base_map,
                    rendered,
                    rev: comments.rev,
                    source,
                    provisional,
                },
            );
            return;
        }

        // Present but built before the comments last changed: re-splice from the
        // retained base rather than rendering again.
        let entry = self.entries.get_mut(&key).expect("occupied above");
        if entry.rev != comments.rev {
            entry.rendered = splice_and_measure(
                entry.base.clone(),
                entry.base_map.clone(),
                comments,
                width,
                side_by_side,
                theme,
            );
            entry.rev = comments.rev;
        }
    }

    /// Check on the hunk starts a provisional render is missing, and drop the
    /// render once they've arrived, so it's made again with them.
    fn settle(&mut self, key: &Key, diff: &FileDiff) {
        let (Some(reader), Some(entry)) = (&mut self.starts, self.entries.get_mut(key)) else {
            return;
        };
        if !entry.provisional {
            return;
        }
        match reader.ask(entry.source, diff) {
            Some(starts) if starts.is_empty() => entry.provisional = false,
            Some(_) => {
                self.entries.remove(key);
            }
            None => {}
        }
    }

    pub fn get(
        &self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
    ) -> Option<&Rendered> {
        self.entries
            .get(&Key {
                file,
                width,
                side_by_side,
                theme,
            })
            .map(|e| &e.rendered)
    }

    /// Drop all cached renders (e.g. after a resize changes the wrap width).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drop the cached renders for a single file (every width/theme variant),
    /// e.g. after its diff is refreshed in place. Other files stay cached.
    pub fn invalidate(&mut self, file: usize) {
        self.entries.retain(|key, _| key.file != file);
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
        let base_map = LineMap::from_gutters(&text);
        let rendered = splice_and_measure(
            text.clone(),
            base_map.clone(),
            comments,
            width,
            side_by_side,
            theme,
        );
        self.entries.insert(
            Key {
                file,
                width,
                side_by_side,
                theme,
            },
            Entry {
                base: text,
                base_map,
                rendered,
                rev: comments.rev,
                source: 0,
                provisional: false,
            },
        );
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

    fn rendered(cache: &mut RenderCache, files: &[FileDiff]) {
        for (at, file) in files.iter().enumerate() {
            cache.ensure(at, file, WIDTH, false, GitHubDark, &CommentLayer::none());
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
        let mut cache = RenderCache::new(4, true);
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
        let mut cache = RenderCache::new(4, false);
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
        let mut cache = RenderCache::new(4, false);
        cache.read_starts_in(&dir);
        let draw = |cache: &mut RenderCache, width, theme| {
            cache.ensure(0, &file, width, false, theme, &CommentLayer::none());
        };

        // The first render may go without, but the file comes right unprompted.
        draw(&mut cache, WIDTH, GitHubDark);
        while cache.reading_starts() {
            std::thread::sleep(std::time::Duration::from_millis(1));
            cache.drain_starts();
        }
        draw(&mut cache, WIDTH, GitHubDark);
        let right = colors_of_the_edit(&cache, WIDTH, GitHubDark);

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
        assert!(!cache.reading_starts());
    }

    #[test]
    fn a_cache_without_a_reader_leaves_hunks_starting_afresh() {
        let dir = std::env::temp_dir().join(format!("riffnav-piped-{}", std::process::id()));
        let file = edit_in_a_comment(&dir);
        let mut cache = RenderCache::new(4, false);
        cache.ensure(0, &file, WIDTH, false, GitHubDark, &CommentLayer::none());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!cache.reading_starts());
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

        let (height, row_offsets) = measure(&text, width, false);
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
}
