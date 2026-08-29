//! Drawing comment threads into a delta render.
//!
//! Blocks are spliced straight into the rendered `Text` rather than overlaid at
//! draw time: the diff pane scrolls by row, so a comment has to occupy real rows
//! or it would slide over the code. Splicing happens once per (file, comment
//! revision) in the render cache, never per frame.
//!
//! Bodies are pre-wrapped here rather than left to ratatui because side-by-side
//! renders are never wrapped downstream (`Rendered::row_offsets` is `None`), so a
//! long comment would otherwise run off the edge in exactly one of the two modes.
//! Pre-wrapping is also what lets a thread be boxed: every row is padded to the
//! pane width here, so the frame's right edge lines up on rows ratatui never
//! reflows.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use twox_hash::XxHash3_128;
use unicode_width::UnicodeWidthChar;

use super::anchor::LineMap;
use super::store::{Anchor, Comment};
use crate::state;
use crate::theme::DiffTheme;

/// Marks a reply beneath the comment it answers.
const REPLY: &str = "↳ ";
/// Columns the box frame and its padding consume: `│ ` on the left, ` │` on the
/// right.
const FRAME: usize = 4;

/// Where one thread's rows ended up, so the cursor can act on what it's over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentBlock {
    /// First rendered line of the block.
    pub start: usize,
    /// How many rendered lines it occupies.
    pub len: usize,
    /// The diff line the thread hangs on.
    pub anchor: Anchor,
    /// The line each individual comment's header landed on, with its id — what
    /// lets `c` and `x` name the one comment the cursor is sitting on rather
    /// than the whole thread.
    pub entries: Vec<(usize, String)>,
}

impl CommentBlock {
    /// The comment whose rows contain `line`: the last one whose header is at or
    /// above it. `None` when `line` isn't in this block at all.
    pub fn comment_at(&self, line: usize) -> Option<&str> {
        if line < self.start || line >= self.start + self.len {
            return None;
        }
        self.entries
            .iter()
            .rev()
            .find(|(at, _)| *at <= line)
            .map(|(_, id)| id.as_str())
    }
}

/// Splice each thread of `comments` into `text`, keeping `map` index-aligned.
///
/// Returns one block per thread, ascending by row — what `]` and `[` step
/// through. Threads whose anchor is no longer in the render are appended at the
/// end rather than dropped, so a comment can never go missing because the code
/// moved underneath it.
///
/// `me` is the name this window records as the author of its own comments, which
/// is what tells a note the reader wrote from one an agent left.
#[allow(clippy::too_many_arguments)]
pub fn splice(
    text: &mut Text<'static>,
    map: &mut LineMap,
    threads: &[(Anchor, Vec<&Comment>)],
    file: &str,
    me: &str,
    width: u16,
    diff_hash: u128,
    theme: DiffTheme,
) -> Vec<CommentBlock> {
    if threads.is_empty() {
        return Vec::new();
    }
    let now = state::now_unix();
    let palette = Palette::for_theme(theme);

    // Resolve every anchor first, then insert from the bottom up so the indices
    // computed here stay valid as earlier rows shift down.
    let mut placed: Vec<Placed> = Vec::new();
    for (anchor, comments) in threads {
        let orphaned = map.row_for(*anchor).is_none();
        // An orphan goes after the last line; several of them stack in order.
        let row = map.row_for(*anchor).map_or(text.lines.len(), |r| r + 1);
        let (lines, entries) = block(
            comments, file, me, *anchor, width, diff_hash, now, &palette, orphaned,
        );
        placed.push(Placed {
            row,
            lines,
            entries,
            anchor: *anchor,
        });
    }
    // A stable sort keeps several orphans (all sharing the end-of-text row) in
    // the order their anchors came in.
    placed.sort_by_key(|p| p.row);

    // The recorded rows are all pre-insert, so the final position of each block
    // is its row plus the length of every block above it.
    let mut running = 0usize;
    let blocks: Vec<CommentBlock> = placed
        .iter()
        .map(|p| {
            let start = p.row + running;
            running += p.lines.len();
            CommentBlock {
                start,
                len: p.lines.len(),
                anchor: p.anchor,
                entries: p
                    .entries
                    .iter()
                    .map(|(off, id)| (start + off, id.clone()))
                    .collect(),
            }
        })
        .collect();

    // Insert bottom-up so the pre-insert rows stay valid as we go.
    for p in placed.into_iter().rev() {
        map.insert_blanks(p.row, p.lines.len());
        text.lines.splice(p.row..p.row, p.lines);
    }
    blocks
}

/// A thread's rows and their pre-insert position.
struct Placed {
    row: usize,
    lines: Vec<Line<'static>>,
    /// Offsets *within* `lines` where each comment's header sits, with its id.
    entries: Vec<(usize, String)>,
    anchor: Anchor,
}

/// The rows for one anchor's thread, framed in a box that spans the pane.
///
/// Each comment's header rides on a rule — the box's top edge for the first, a
/// divider for every reply — so a thread reads as one card with its replies
/// inside it, rather than as loose rows the eye has to group.
///
/// `orphaned` means the anchored line is no longer in the render, so the header
/// spells out where the note used to live.
#[allow(clippy::too_many_arguments)]
fn block(
    comments: &[&Comment],
    file: &str,
    me: &str,
    anchor: Anchor,
    width: u16,
    diff_hash: u128,
    now: u64,
    palette: &Palette,
    orphaned: bool,
) -> (Vec<Line<'static>>, Vec<(usize, String)>) {
    let cols = width as usize;
    let body_cols = cols.saturating_sub(FRAME).max(1);
    let mut out = Vec::new();
    let mut entries = Vec::with_capacity(comments.len());
    for (i, c) in comments.iter().enumerate() {
        entries.push((out.len(), c.id.clone()));
        let mut header = Vec::new();
        if c.reply_to.is_some() {
            header.push(Span::styled(REPLY, Style::new().fg(palette.meta)));
        }
        header.push(Span::styled(
            c.author.clone(),
            Style::new()
                .fg(palette.author(&c.author, me))
                .add_modifier(Modifier::BOLD),
        ));
        header.push(Span::styled(
            format!(" · {}", ago(now, c.created)),
            Style::new().fg(palette.meta),
        ));
        header.push(Span::styled(
            format!("  #{}", c.id),
            Style::new().fg(palette.meta).add_modifier(Modifier::DIM),
        ));
        if c.is_stale(diff_hash) {
            header.push(Span::styled(
                "  (code changed since)",
                Style::new().fg(palette.stale),
            ));
        }
        // The first comment of an orphaned thread says where it used to live.
        if i == 0 && orphaned {
            header.push(Span::styled(
                format!("  ({file}:{} is no longer in this diff)", anchor.line),
                Style::new().fg(palette.stale),
            ));
        }
        let edge = if i == 0 { Edge::Top } else { Edge::Divider };
        out.push(rule(edge, header, cols, palette));

        for row in wrap(&c.body, body_cols) {
            out.push(framed(row, body_cols, palette));
        }
    }
    out.push(rule(Edge::Bottom, Vec::new(), cols, palette));
    (out, entries)
}

/// Which horizontal rule of the box is being drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Top,
    /// Between one comment and the reply below it.
    Divider,
    Bottom,
}

impl Edge {
    /// The corner glyphs closing this rule at either end.
    fn corners(self) -> (&'static str, &'static str) {
        match self {
            Self::Top => ("╭", "╮"),
            Self::Divider => ("├", "┤"),
            Self::Bottom => ("╰", "╯"),
        }
    }
}

/// One horizontal rule of the box, with `header` inlaid after the left corner.
/// The header is clipped to whatever the pane leaves after the frame, so a wide
/// one degrades to its most important spans instead of overflowing the rule.
fn rule(edge: Edge, header: Vec<Span<'static>>, cols: usize, palette: &Palette) -> Line<'static> {
    let (left, right) = edge.corners();
    let border = Style::new().fg(palette.accent);
    // "╭─ " … " ─╮": three columns of frame either side of the header.
    let header = truncate(header, cols.saturating_sub(6));
    let mut spans = vec![Span::styled(
        if header.is_empty() {
            left.to_string()
        } else {
            format!("{left}─ ")
        },
        border,
    )];
    let used: usize = header.iter().map(|s| display_width(&s.content)).sum();
    spans.extend(header);
    let lead = if used == 0 { 1 } else { 4 }; // corner, or corner + "─ " + " "
    let fill = cols.saturating_sub(used + lead + 1);
    spans.push(Span::styled(
        format!(
            "{}{}{right}",
            if used == 0 { "" } else { " " },
            "─".repeat(fill)
        ),
        border,
    ));
    Line::from(spans)
}

/// One body row, padded so the box's right edge lines up under the rules above.
fn framed(text: String, body_cols: usize, palette: &Palette) -> Line<'static> {
    let border = Style::new().fg(palette.accent);
    let pad = body_cols.saturating_sub(display_width(&text));
    Line::from(vec![
        Span::styled("│ ", border),
        Span::styled(text, Style::new().fg(palette.body)),
        Span::styled(format!("{} │", " ".repeat(pad)), border),
    ])
}

/// Clip a header to `cols` display columns. Headers are built most-important
/// first (author, then age, then id, then any warning), so dropping from the end
/// degrades gracefully on a narrow pane instead of overflowing it.
fn truncate(spans: Vec<Span<'static>>, cols: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    for span in spans {
        let w = display_width(&span.content);
        if used + w <= cols {
            used += w;
            out.push(span);
            continue;
        }
        // Partially fit this span, then stop.
        let room = cols.saturating_sub(used);
        if room > 0 {
            let clipped: String = span
                .content
                .chars()
                .scan(0usize, |acc, ch| {
                    *acc += ch.width().unwrap_or(0);
                    (*acc <= room).then_some(ch)
                })
                .collect();
            if !clipped.is_empty() {
                out.push(Span::styled(clipped, span.style));
            }
        }
        break;
    }
    out
}

/// Word-wrap `text` to `cols` display columns, honoring the newlines the author
/// typed. A word longer than the whole width is hard-split rather than allowed
/// to overflow.
fn wrap(text: &str, cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0usize;
        let emitted_before = out.len();
        for word in paragraph.split_whitespace() {
            let w = display_width(word);
            if used > 0 && used + 1 + w > cols {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            if w > cols {
                // Hard-split an over-long word across as many rows as it needs.
                if used > 0 {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                }
                for chunk in hard_split(word, cols) {
                    out.push(chunk);
                }
                continue;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += w;
        }
        // Emit the tail, but not a phantom empty row after a hard-split word.
        // An authored blank line emits nothing else, so it still gets its row.
        if !line.is_empty() || out.len() == emitted_before {
            out.push(line);
        }
    }
    out
}

fn hard_split(word: &str, cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut used = 0;
    for ch in word.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > cols && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(ch);
        used += w;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Coarse relative time, in the spirit of a review UI. Anything older than a
/// week reads as a plain day count rather than a date, which keeps the header
/// short and needs no calendar arithmetic.
fn ago(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// The color a thread's frame is drawn in, so UI outside this module — the
/// composer's own frame — can match the boxes it will turn into.
pub fn accent(theme: DiffTheme) -> Color {
    Palette::for_theme(theme).accent
}

/// Colors for the comment block. Comments sit on top of a diff whose background
/// is theme-controlled, so the accent is picked per theme rather than inherited.
struct Palette {
    accent: Color,
    meta: Color,
    body: Color,
    stale: Color,
    /// Hues for authors other than the reader. None of them is the accent or the
    /// stale red, so a name can never be mistaken for the frame or a warning.
    names: &'static [Color],
}

impl Palette {
    fn for_theme(theme: DiffTheme) -> Self {
        match theme {
            // GitHub's "attention" amber reads as a note on either canvas.
            DiffTheme::GitHubLight => Self {
                accent: Color::Rgb(0x9a, 0x67, 0x00),
                meta: Color::Rgb(0x6e, 0x77, 0x81),
                body: Color::Rgb(0x1f, 0x23, 0x28),
                stale: Color::Rgb(0xcf, 0x22, 0x2e),
                names: &[
                    Color::Rgb(0x09, 0x69, 0xda), // blue
                    Color::Rgb(0x1a, 0x7f, 0x37), // green
                    Color::Rgb(0x82, 0x50, 0xdf), // purple
                    Color::Rgb(0xbf, 0x39, 0x89), // pink
                    Color::Rgb(0x1b, 0x7c, 0x83), // teal
                    Color::Rgb(0xbc, 0x4c, 0x00), // orange
                ],
            },
            DiffTheme::GitHubDark => Self {
                accent: Color::Rgb(0xd2, 0x99, 0x22),
                meta: Color::Rgb(0x6e, 0x76, 0x81),
                body: Color::Rgb(0xc9, 0xd1, 0xd9),
                stale: Color::Rgb(0xf8, 0x51, 0x49),
                names: &[
                    Color::Rgb(0x58, 0xa6, 0xff), // blue
                    Color::Rgb(0x3f, 0xb9, 0x50), // green
                    Color::Rgb(0xbc, 0x8c, 0xff), // purple
                    Color::Rgb(0xf7, 0x78, 0xba), // pink
                    Color::Rgb(0x39, 0xc5, 0xcf), // teal
                    Color::Rgb(0xf0, 0x88, 0x3e), // orange
                ],
            },
            // The baseline theme follows the user's own gitconfig colors, so use
            // terminal-palette names and let their scheme decide the shades.
            DiffTheme::Delta => Self {
                accent: Color::Yellow,
                meta: Color::DarkGray,
                body: Color::Reset,
                stale: Color::Red,
                names: &[
                    Color::Cyan,
                    Color::Green,
                    Color::Magenta,
                    Color::Blue,
                    Color::LightGreen,
                    Color::LightMagenta,
                ],
            },
        }
    }

    /// The color to draw one author's name in.
    ///
    /// The reader's own name keeps the frame's accent, so their notes read as the
    /// box's own voice. Every other name — an agent's, a second reviewer's — is
    /// hashed into [`Self::names`]: nobody has to configure anything, and a name
    /// keeps the same color across sessions, machines and themes, which is what
    /// makes a thread scannable by who is speaking.
    fn author(&self, author: &str, me: &str) -> Color {
        if author.eq_ignore_ascii_case(me) {
            return self.accent;
        }
        let key = author.to_ascii_lowercase();
        let slot = XxHash3_128::oneshot(key.as_bytes()) % self.names.len() as u128;
        self.names[slot as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::store::{CommentStore, Side};

    fn text_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn render(lines: &[&str]) -> (Text<'static>, LineMap) {
        let text = Text::from(
            lines
                .iter()
                .map(|s| Line::from(s.to_string()))
                .collect::<Vec<_>>(),
        );
        let map = LineMap::build(&text);
        (text, map)
    }

    fn store_with(entries: &[(u32, &str, &str)]) -> CommentStore {
        let mut store = CommentStore::disabled();
        for (line, author, body) in entries {
            store.add(Comment {
                id: String::new(),
                file: "f".to_string(),
                side: Side::New,
                line: *line,
                body: body.to_string(),
                author: author.to_string(),
                created: 0,
                reply_to: None,
                diff_hash: None,
            });
        }
        store
    }

    const DIFF: &[&str] = &[
        "    1⋮    1│first;",
        "    2⋮    2│second;",
        "    3⋮    3│third;",
    ];

    #[test]
    fn block_lands_directly_under_its_anchor() {
        let (mut text, mut map) = render(DIFF);
        let store = store_with(&[(2, "claude", "why?")]);
        let rows = splice(
            &mut text,
            &mut map,
            &store.threads("f"),
            "f",
            "me",
            40,
            0,
            DiffTheme::GitHubDark,
        );
        let out = text_of(&text.lines);
        assert_eq!(rows.iter().map(|b| b.start).collect::<Vec<_>>(), vec![2]);
        assert!(out[1].contains("second;"));
        // The thread is a box: the header rides the top edge, the body sits
        // inside it, and the bottom edge closes it before the code resumes.
        assert!(
            out[2].starts_with("╭─ ") && out[2].contains("claude") && out[2].ends_with('╮'),
            "header sits on the box's top edge under line 2: {out:?}"
        );
        assert!(out[3].starts_with("│ ") && out[3].contains("why?") && out[3].ends_with(" │"));
        assert!(out[4].starts_with('╰') && out[4].ends_with('╯'));
        assert!(out[5].contains("third;"), "the next code line follows");
        // Every row of the box is exactly as wide as the pane, so its right edge
        // lines up and no row wraps.
        for row in &out[2..5] {
            assert_eq!(
                display_width(row),
                40,
                "box row must fill the pane: {row:?}"
            );
        }
    }

    #[test]
    fn replies_are_divided_inside_one_box() {
        let (mut text, mut map) = render(DIFF);
        let mut store = store_with(&[(2, "claude", "why?")]);
        let parent = store.all()[0].id.clone();
        store.add(Comment {
            id: String::new(),
            file: "f".to_string(),
            side: Side::New,
            line: 2,
            body: "because of the timeout".to_string(),
            author: "me".to_string(),
            created: 0,
            reply_to: Some(parent),
            diff_hash: None,
        });
        splice(
            &mut text,
            &mut map,
            &store.threads("f"),
            "f",
            "me",
            40,
            0,
            DiffTheme::GitHubDark,
        );
        let out = text_of(&text.lines);
        // One box, opened once and closed once, with a divider carrying the
        // reply's header between the two comments.
        assert!(out[2].starts_with('╭'));
        assert!(
            out[4].starts_with("├─ ") && out[4].contains("↳ me") && out[4].ends_with('┤'),
            "the reply's header divides the box: {out:?}"
        );
        assert!(out[5].contains("because of the timeout"));
        assert!(out[6].starts_with('╰'));
    }

    #[test]
    fn multiple_threads_keep_their_anchors_after_earlier_inserts() {
        let (mut text, mut map) = render(DIFF);
        let store = store_with(&[(1, "a", "one"), (3, "b", "three")]);
        let rows = splice(
            &mut text,
            &mut map,
            &store.threads("f"),
            "f",
            "me",
            40,
            0,
            DiffTheme::GitHubDark,
        );
        let out = text_of(&text.lines);
        // Two boxes, three rows each; the later one is pushed down by the first.
        assert_eq!(rows.iter().map(|b| b.start).collect::<Vec<_>>(), vec![1, 6]);
        assert!(out[1].contains("a"));
        assert!(out[2].contains("one"));
        assert!(out[4].contains("second;"));
        assert!(out[5].contains("third;"));
        assert!(out[6].contains("b"));
        assert!(out[7].contains("three"));
        // The map still resolves the anchors it did before.
        assert_eq!(map.len(), text.lines.len());
        assert_eq!(
            map.row_for(Anchor {
                side: Side::New,
                line: 3
            }),
            Some(5)
        );
    }

    #[test]
    fn a_comment_on_a_vanished_line_is_appended_not_dropped() {
        let (mut text, mut map) = render(DIFF);
        let store = store_with(&[(99, "ghost", "still here")]);
        let before = text.lines.len();
        let rows = splice(
            &mut text,
            &mut map,
            &store.threads("f"),
            "f",
            "me",
            40,
            0,
            DiffTheme::GitHubDark,
        );
        let out = text_of(&text.lines);
        assert_eq!(rows[0].start, before);
        assert!(
            out[before].starts_with("╭─ ") && out[before].contains("ghost"),
            "{out:?}"
        );
        assert!(out[before + 1].contains("still here"), "{out:?}");
        assert_eq!(map.len(), text.lines.len());
    }

    #[test]
    fn long_bodies_wrap_to_the_pane_width() {
        let (mut text, mut map) = render(DIFF);
        let store = store_with(&[(1, "a", "the quick brown fox jumps over the lazy dog again")]);
        splice(
            &mut text,
            &mut map,
            &store.threads("f"),
            "f",
            "me",
            24,
            0,
            DiffTheme::GitHubDark,
        );
        for row in text_of(&text.lines) {
            assert!(display_width(&row) <= 24, "row overflows the pane: {row:?}");
        }
    }

    #[test]
    fn wrap_preserves_authored_newlines_and_splits_long_words() {
        assert_eq!(wrap("one\ntwo", 40), vec!["one", "two"]);
        assert_eq!(wrap("", 10), vec![""]);
        assert_eq!(wrap("ab cd ef", 5), vec!["ab cd", "ef"]);
        // A hard-split word must not leave a phantom empty row behind it...
        assert_eq!(wrap("aaaaaaaa", 3), vec!["aaa", "aaa", "aa"]);
        // ...but a blank line the author typed still gets one.
        assert_eq!(wrap("one\n\ntwo", 40), vec!["one", "", "two"]);
    }

    #[test]
    fn a_header_too_wide_for_the_pane_is_clipped_not_overflowed() {
        let spans = vec![
            Span::raw("author".to_string()),
            Span::raw(" · 3d ago".to_string()),
        ];
        // Wide enough for everything.
        assert_eq!(display_width(&joined(truncate(spans.clone(), 40))), 15);
        // Clips mid-span rather than spilling.
        assert_eq!(joined(truncate(spans.clone(), 9)), "author · ");
        // Drops the span entirely when nothing fits.
        assert_eq!(joined(truncate(spans, 0)), "");
    }

    fn joined(spans: Vec<Span<'_>>) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_name_keeps_one_color_and_mine_keeps_the_accent() {
        for theme in [
            DiffTheme::Delta,
            DiffTheme::GitHubDark,
            DiffTheme::GitHubLight,
        ] {
            let p = Palette::for_theme(theme);
            // My own notes stay in the frame's accent...
            assert_eq!(p.author("olli", "olli"), p.accent);
            assert_eq!(p.author("Olli", "olli"), p.accent, "case doesn't matter");
            // ...and nobody else can land on it, or on the staleness red.
            for name in ["claude", "codex", "agent", "reviewer", "ada"] {
                let c = p.author(name, "olli");
                assert_eq!(c, p.author(name, "olli"), "{name} must be stable");
                assert_eq!(c, p.author(&name.to_uppercase(), "olli"));
                assert_ne!(c, p.accent, "{name} must not read as mine");
                assert_ne!(c, p.stale);
            }
            // A handful of names should actually spread over the palette rather
            // than collapsing onto one hue.
            let names = ["claude", "codex", "gemini", "ada", "grace", "linus"];
            let used: std::collections::HashSet<_> =
                names.iter().map(|n| p.author(n, "olli")).collect();
            assert!(used.len() >= 4, "{theme:?} bunched names up: {used:?}");
        }
    }

    #[test]
    fn two_authors_in_one_thread_are_colored_apart() {
        let (mut text, mut map) = render(DIFF);
        let mut store = store_with(&[(2, "claude", "why?")]);
        let parent = store.all()[0].id.clone();
        store.add(Comment {
            id: String::new(),
            file: "f".to_string(),
            side: Side::New,
            line: 2,
            body: "because of the timeout".to_string(),
            author: "olli".to_string(),
            created: 0,
            reply_to: Some(parent),
            diff_hash: None,
        });
        splice(
            &mut text,
            &mut map,
            &store.threads("f"),
            "f",
            "olli",
            40,
            0,
            DiffTheme::GitHubDark,
        );
        // The name span of each header: the agent's is hashed, mine is the accent.
        let color_of = |row: usize, name: &str| {
            text.lines[row]
                .spans
                .iter()
                .find(|s| s.content == name)
                .unwrap_or_else(|| panic!("no {name} span in row {row}"))
                .style
                .fg
        };
        let agent = color_of(2, "claude");
        assert_eq!(color_of(4, "olli"), Some(accent(DiffTheme::GitHubDark)));
        assert_ne!(agent, Some(accent(DiffTheme::GitHubDark)));
    }

    #[test]
    fn relative_time_covers_each_bucket() {
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(60 * 5, 0), "5m ago");
        assert_eq!(ago(3600 * 2, 0), "2h ago");
        assert_eq!(ago(86_400 * 3, 0), "3d ago");
    }

    #[test]
    fn stale_comments_are_marked() {
        let (mut text, mut map) = render(DIFF);
        let mut store = CommentStore::disabled();
        store.add(Comment {
            id: String::new(),
            file: "f".to_string(),
            side: Side::New,
            line: 1,
            body: "old note".to_string(),
            author: "a".to_string(),
            created: 0,
            reply_to: None,
            diff_hash: Some(format!("{:032x}", 7u128)),
        });
        splice(
            &mut text,
            &mut map,
            &store.threads("f"),
            "f",
            "me",
            60,
            9, // the file's diff hash is no longer 7
            DiffTheme::GitHubDark,
        );
        assert!(text_of(&text.lines)[1].contains("code changed since"));
    }
}
