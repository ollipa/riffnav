//! Typing a comment inside riffnav.
//!
//! Handing the terminal to `$EDITOR` is a lot of ceremony for one sentence: the
//! screen tears down, the diff being annotated disappears, and the note can't be
//! seen next to the line it's about. So `c` opens this instead — a small text
//! field drawn over the diff pane at the anchored line — with the editor still
//! one keystroke away (`Ctrl-O`) for a note that outgrows it.
//!
//! Text is held as chars rather than bytes because every operation here is
//! cursor-relative (insert at, delete before, wrap between), and a char index is
//! the only position that's cheap to move and impossible to land mid-codepoint.

use unicode_width::UnicodeWidthChar;

use super::store::Anchor;

/// Everything a composed note needs to become a stored comment, carried from the
/// keypress that started it through to the save.
pub struct PendingComment {
    pub file: String,
    pub anchor: Anchor,
    /// Set when replying, so the new note threads under an existing one.
    pub reply_to: Option<String>,
    /// `review::file_hash` of the file's diff, recorded so the note can later be
    /// flagged if the code moves out from under it.
    pub diff_hash: u128,
    /// The diff lines quoted back below the scissors, for context while typing
    /// in `$EDITOR`.
    pub context: Vec<String>,
    /// Text already typed in the composer, pre-filled into the editor buffer when
    /// composing is handed off to `$EDITOR` mid-note.
    pub draft: String,
}

/// The composer's text laid out for a field `cols` columns wide: one entry per
/// display row, plus where the cursor lands among them. Wrapping and cursor
/// placement are computed together because they have to agree — the caller can't
/// re-derive one from the other.
pub struct Layout {
    pub rows: Vec<String>,
    /// `(row, column)` of the cursor within `rows`.
    pub cursor: (usize, usize),
}

/// A multi-line text field for one comment body.
pub struct Composer {
    pub pending: PendingComment,
    /// The body being typed, one entry per authored line. Never empty: an empty
    /// body is a single empty line, which is where the cursor sits.
    lines: Vec<Vec<char>>,
    /// Cursor line, indexing `lines`.
    row: usize,
    /// Cursor column, a char index into `lines[row]`.
    col: usize,
}

impl Composer {
    pub fn new(pending: PendingComment) -> Self {
        Self {
            pending,
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
        }
    }

    /// The typed body, as it would be stored.
    pub fn body(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Hand the note off to `$EDITOR`, carrying whatever has been typed so far.
    pub fn into_draft(self) -> PendingComment {
        let draft = self.body();
        PendingComment {
            draft,
            ..self.pending
        }
    }

    /// What the composer's frame is titled: which line the note lands on, and
    /// whether it answers an existing one.
    pub fn title(&self) -> String {
        match &self.pending.reply_to {
            Some(id) => format!(
                " Reply to #{id} · {}:{} ",
                self.pending.file, self.pending.anchor.line
            ),
            None => format!(
                " Comment on {}:{} ",
                self.pending.file, self.pending.anchor.line
            ),
        }
    }

    pub fn insert(&mut self, ch: char) {
        let col = self.col;
        self.lines[self.row].insert(col, ch);
        self.col += 1;
    }

    /// Split the current line at the cursor, as Enter does in any text field.
    pub fn newline(&mut self) {
        let tail = self.lines[self.row].split_off(self.col);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    /// Delete the char before the cursor, joining onto the previous line when the
    /// cursor is at the start of one.
    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            let col = self.col;
            self.lines[self.row].remove(col);
        } else if self.row > 0 {
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].extend(tail);
        }
    }

    /// Delete the char under the cursor, pulling the next line up at end of line.
    pub fn delete(&mut self) {
        if self.col < self.lines[self.row].len() {
            let col = self.col;
            self.lines[self.row].remove(col);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(next);
        }
    }

    /// `Ctrl-W`: delete the word before the cursor, plus the run of spaces in
    /// front of it, the way a shell prompt does.
    pub fn delete_word(&mut self) {
        let line = &mut self.lines[self.row];
        let mut at = self.col;
        while at > 0 && line[at - 1].is_whitespace() {
            at -= 1;
        }
        while at > 0 && !line[at - 1].is_whitespace() {
            at -= 1;
        }
        if at == self.col {
            // Nothing to eat on this line — fall back to joining the line above,
            // so the key never feels dead.
            return self.backspace();
        }
        line.drain(at..self.col);
        self.col = at;
    }

    /// `Ctrl-U`: clear from the cursor back to the start of the line.
    pub fn delete_to_start(&mut self) {
        self.lines[self.row].drain(..self.col);
        self.col = 0;
    }

    /// Step the cursor one char left, wrapping to the end of the previous line.
    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    /// Step the cursor one char right, wrapping to the start of the next line.
    pub fn right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    pub fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    /// Wrap the body to a `cols`-wide field and place the cursor in it.
    pub fn layout(&self, cols: usize) -> Layout {
        let cols = cols.max(1);
        let mut rows = Vec::new();
        let mut cursor = (0, 0);
        for (r, line) in self.lines.iter().enumerate() {
            let segments = segments(line, cols);
            let last = segments.len() - 1;
            for (s, &(start, end)) in segments.iter().enumerate() {
                // The cursor belongs to the last segment starting at or before it,
                // so typing at a wrap point continues on the row it will land on.
                if r == self.row && self.col >= start && (self.col < end || s == last) {
                    cursor = (rows.len(), width_of(&line[start..self.col]));
                }
                rows.push(line[start..end].iter().collect());
            }
        }
        Layout { rows, cursor }
    }
}

/// Split one authored line into display segments of at most `cols` columns,
/// breaking at spaces where possible and mid-word when a word doesn't fit at all.
/// Returns half-open char ranges covering the line, always at least one (an empty
/// line still occupies a row).
fn segments(line: &[char], cols: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut used = 0;
    // Char index just past the most recent space, i.e. where a break would put
    // the following word at the start of the next row.
    let mut break_at: Option<usize> = None;
    for (i, ch) in line.iter().enumerate() {
        let w = ch.width().unwrap_or(0);
        if used + w > cols && i > start {
            let at = break_at.filter(|&b| b > start).unwrap_or(i);
            out.push((start, at));
            start = at;
            used = width_of(&line[start..=i]);
            break_at = None;
        } else {
            used += w;
        }
        if *ch == ' ' {
            break_at = Some(i + 1);
        }
    }
    out.push((start, line.len()));
    out
}

fn width_of(chars: &[char]) -> usize {
    chars.iter().map(|c| c.width().unwrap_or(0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::store::Side;

    fn composer() -> Composer {
        Composer::new(PendingComment {
            file: "src/app.rs".to_string(),
            anchor: Anchor {
                side: Side::New,
                line: 103,
            },
            reply_to: None,
            diff_hash: 0,
            context: Vec::new(),
            draft: String::new(),
        })
    }

    fn typed(text: &str) -> Composer {
        let mut c = composer();
        for ch in text.chars() {
            if ch == '\n' {
                c.newline();
            } else {
                c.insert(ch);
            }
        }
        c
    }

    #[test]
    fn typing_and_editing_build_the_body() {
        let mut c = typed("hello wrold");
        // Fix the typo the way a person would: back over it and retype.
        for _ in 0..4 {
            c.left();
        }
        c.delete(); // the stray "r"
        c.right(); // past the "o"
        c.insert('r');
        assert_eq!(c.body(), "hello world");
    }

    #[test]
    fn enter_splits_the_line_at_the_cursor() {
        let mut c = typed("one two");
        for _ in 0..3 {
            c.left();
        }
        c.backspace(); // the space between them
        c.newline();
        assert_eq!(c.body(), "one\ntwo");
        // Backspace at the start of a line joins it back onto the one above.
        c.home();
        c.backspace();
        assert_eq!(c.body(), "onetwo");
    }

    #[test]
    fn word_and_line_kills_only_eat_what_they_should() {
        let mut c = typed("a note about retries");
        c.delete_word();
        assert_eq!(c.body(), "a note about ");
        c.delete_to_start();
        assert_eq!(c.body(), "");

        // Ctrl-W with nothing left on the line falls back to joining upward,
        // rather than doing nothing at all.
        let mut c = typed("one\n");
        c.delete_word();
        assert_eq!(c.body(), "one");
    }

    #[test]
    fn delete_removes_forward_and_pulls_the_next_line_up() {
        let mut c = typed("ab\ncd");
        c.up();
        c.end();
        c.delete(); // at end of line 1: joins line 2
        assert_eq!(c.body(), "abcd");
        c.home();
        c.delete();
        assert_eq!(c.body(), "bcd");
    }

    #[test]
    fn layout_wraps_at_spaces_and_tracks_the_cursor() {
        let c = typed("the quick brown fox");
        let out = c.layout(10);
        assert_eq!(out.rows, vec!["the quick ", "brown fox"]);
        // The cursor sits at the end of what was typed, on the last row.
        assert_eq!(out.cursor, (1, 9));
    }

    #[test]
    fn layout_hard_splits_a_word_wider_than_the_field() {
        let c = typed("supercalifragilistic");
        let out = c.layout(8);
        assert_eq!(out.rows, vec!["supercal", "ifragili", "stic"]);
        assert_eq!(out.cursor, (2, 4));
    }

    #[test]
    fn layout_gives_an_empty_body_one_row_with_the_cursor_on_it() {
        let c = composer();
        let out = c.layout(20);
        assert_eq!(out.rows, vec![""]);
        assert_eq!(out.cursor, (0, 0));

        // An authored blank line keeps its own row too.
        let c = typed("one\n\ntwo");
        assert_eq!(c.layout(20).rows, vec!["one", "", "two"]);
    }

    #[test]
    fn the_cursor_follows_a_wrap_onto_the_row_the_next_char_lands_on() {
        let mut c = typed("abcd efgh");
        // Field is exactly the first word plus its space: the cursor is at the
        // start of the second row, where the next keystroke will show up.
        c.up(); // no-op: single line, exercises the clamp
        for _ in 0..4 {
            c.left();
        }
        let out = c.layout(5);
        assert_eq!(out.rows, vec!["abcd ", "efgh"]);
        assert_eq!(out.cursor, (1, 0));
    }

    /// The frame's title is the only thing that says which of the two things `c`
    /// did — started a note, or replied to the one under the cursor.
    #[test]
    fn the_title_names_the_line_and_says_when_it_is_a_reply() {
        let c = composer();
        assert_eq!(c.title(), " Comment on src/app.rs:103 ");

        let mut pending = composer().pending;
        pending.reply_to = Some("a3f1c2".to_string());
        assert_eq!(
            Composer::new(pending).title(),
            " Reply to #a3f1c2 · src/app.rs:103 "
        );
    }

    #[test]
    fn handing_off_to_the_editor_carries_the_typed_text() {
        let c = typed("half a thought");
        let pending = c.into_draft();
        assert_eq!(pending.draft, "half a thought");
        assert_eq!(pending.anchor.line, 103);
    }
}
