//! Mapping delta's rendered output back to diff line numbers.
//!
//! Comments anchor to `(file, side, line)` in diff-line space, but they have to
//! be drawn next to a *row*. Nothing in delta's ANSI output identifies a row, so
//! riffnav reads the line numbers back out of delta's own gutter: `run()` pins
//! `--line-numbers` and both number formats to fixed-width fields, which makes
//! this parse arithmetic rather than guesswork.
//!
//! With `--line-numbers-left-format "{nm:>5}⋮"` and
//! `--line-numbers-right-format "{np:>5}│"`, delta 0.19 emits:
//!
//! ```text
//! unified        "    2⋮     │use std::process::Command;"   old=2  new=-
//!                "     ⋮    3│use std::process::{…};"       old=-  new=3
//! side-by-side   "    3⋮use std::time::…        4│use std::time::…"
//! ```
//!
//! Both modes put the pre-image field in the first five columns followed by `⋮`.
//! They differ only in where the post-image field sits: immediately after in
//! unified, at the panel boundary in side-by-side. delta pads the left panel to a
//! fixed width, so that column is constant for a whole render — [`LineMap::build`]
//! finds it once by consensus instead of assuming a geometry.
//!
//! Alternatives considered and rejected: correlating output rows with the parsed
//! diff by position breaks on delta's decoration rows (`--file-decoration-style
//! ul`, `--hunk-header-decoration-style box`), and rendering hunk-by-hunk costs a
//! subprocess spawn per hunk.

use ratatui::text::{Line, Text};

use super::store::{Anchor, Side};

/// Width of the fixed number field in both pinned formats.
const FIELD: usize = 5;
/// Delimiter closing the pre-image field.
const LEFT_DELIM: char = '⋮';
/// Delimiter closing the post-image field.
const RIGHT_DELIM: char = '│';

/// Per rendered line, the diff line numbers delta printed in its gutter.
/// Index-aligned with `Text::lines`, and kept aligned as comment blocks are
/// spliced in (spliced rows get a `LineNumbers::none()`).
#[derive(Debug, Clone, Default)]
pub struct LineMap {
    rows: Vec<LineNumbers>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineNumbers {
    pub old: Option<u32>,
    pub new: Option<u32>,
}

impl LineNumbers {
    pub fn none() -> Self {
        Self::default()
    }

    /// The anchor a cursor on this row should produce. Prefers the post-image
    /// side, the one a reviewer means by "line N" — a removed line has only an
    /// old number, so it falls through to `Old`.
    pub fn anchor(self) -> Option<Anchor> {
        match (self.new, self.old) {
            (Some(line), _) => Some(Anchor {
                side: Side::New,
                line,
            }),
            (None, Some(line)) => Some(Anchor {
                side: Side::Old,
                line,
            }),
            (None, None) => None,
        }
    }

    /// Whether this row is a diff line rather than a header, decoration, or a
    /// spliced comment row. Only the tests need to assert on it.
    #[cfg(test)]
    pub fn is_code(self) -> bool {
        self.old.is_some() || self.new.is_some()
    }
}

impl LineMap {
    /// Parse the gutter of every line in a delta render.
    pub fn build(text: &Text<'_>) -> Self {
        let plain: Vec<Vec<char>> = text.lines.iter().map(line_chars).collect();
        let right = consensus_right_column(&plain);

        let rows = plain
            .iter()
            .map(|chars| LineNumbers {
                old: field_at(chars, FIELD, LEFT_DELIM),
                new: right.and_then(|col| field_at(chars, col, RIGHT_DELIM)),
            })
            .collect::<Vec<_>>();

        Self { rows }
    }

    pub fn get(&self, row: usize) -> LineNumbers {
        self.rows.get(row).copied().unwrap_or_default()
    }

    /// Rows in the map. Splicing must keep this equal to the text's line count —
    /// the invariant the render tests assert — so it exists for them.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// The first rendered line carrying `anchor`. `None` when the anchored line
    /// isn't in this render — the diff changed, or the comment names a line
    /// outside any hunk.
    pub fn row_for(&self, anchor: Anchor) -> Option<usize> {
        self.rows.iter().position(|r| match anchor.side {
            Side::Old => r.old == Some(anchor.line),
            Side::New => r.new == Some(anchor.line),
        })
    }

    /// The first row carrying a line number, i.e. the first actual diff line
    /// past delta's file header and hunk-header decorations. Where the cursor
    /// starts, so `c` works without hunting for a commentable row.
    pub fn first_code_row(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| r.old.is_some() || r.new.is_some())
    }

    /// Insert `count` unnumbered rows at `at`, keeping the map index-aligned with
    /// the text as comment blocks are spliced in.
    pub fn insert_blanks(&mut self, at: usize, count: usize) {
        let at = at.min(self.rows.len());
        self.rows
            .splice(at..at, std::iter::repeat_n(LineNumbers::none(), count));
    }
}

/// Flatten a styled line to the characters a terminal would show.
fn line_chars(line: &Line<'_>) -> Vec<char> {
    line.spans.iter().flat_map(|s| s.content.chars()).collect()
}

/// Read the `FIELD`-wide number ending just before `delim_col`, requiring the
/// delimiter to actually be there. An all-blank field (the side this row doesn't
/// touch) yields `None`, as does anything non-numeric.
fn field_at(chars: &[char], delim_col: usize, delim: char) -> Option<u32> {
    if delim_col < FIELD || chars.get(delim_col) != Some(&delim) {
        return None;
    }
    let digits: String = chars[delim_col - FIELD..delim_col]
        .iter()
        .filter(|c| !c.is_whitespace())
        .collect();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The column holding the post-image delimiter, agreed by majority across the
/// render.
///
/// Deciding once per render rather than per line is what makes side-by-side safe:
/// the column is constant because delta pads the left panel, so a stray `│` in
/// the code itself can never outvote it. In unified mode the consensus lands on
/// `FIELD * 2 + 1` on its own, so both modes share this path.
fn consensus_right_column(plain: &[Vec<char>]) -> Option<usize> {
    let mut tally: Vec<(usize, usize)> = Vec::new();
    for chars in plain {
        // Only rows that already look like code lines get a vote.
        if field_at(chars, FIELD, LEFT_DELIM).is_none() && chars.get(FIELD) != Some(&LEFT_DELIM) {
            continue;
        }
        for (col, _) in chars
            .iter()
            .enumerate()
            .filter(|&(col, c)| *c == RIGHT_DELIM && col > FIELD)
        {
            if field_at(chars, col, RIGHT_DELIM).is_some() {
                match tally.iter_mut().find(|(c, _)| *c == col) {
                    Some((_, n)) => *n += 1,
                    None => tally.push((col, 1)),
                }
            }
        }
    }
    tally
        .into_iter()
        .max_by_key(|&(col, n)| (n, usize::MAX - col))
        .map(|(col, _)| col)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim delta 0.19.2 output (ANSI stripped) for
    /// `--line-numbers --line-numbers-left-format "{nm:>5}⋮"
    ///  --line-numbers-right-format "{np:>5}│"`.
    const UNIFIED: &[&str] = &[
        "src/app.rs",
        "────────────",
        "    1⋮    1│use std::collections::HashSet;",
        "    2⋮     │use std::process::Command;",
        "     ⋮    2│use std::io::IsTerminal;",
        "     ⋮    3│use std::process::{Command, Stdio};",
        "    3⋮    4│use std::time::{Duration, Instant};",
    ];

    const SIDE_BY_SIDE: &[&str] = &[
        "src/app.rs",
        "────────────",
        "    1⋮use std::collections::HashSet;                  1│use std::collections::HashSet;",
        "    2⋮use std::process::Command;                      2│use std::io::IsTerminal;",
        "     ⋮                                                3│use std::process::{Command, Stdio};",
        "    3⋮use std::time::{Duration, Instant};             4│use std::time::{Duration, Instant};",
    ];

    fn map(lines: &[&str]) -> LineMap {
        LineMap::build(&Text::from(
            lines
                .iter()
                .map(|s| Line::from(s.to_string()))
                .collect::<Vec<_>>(),
        ))
    }

    #[test]
    fn unified_gutter_yields_both_sides() {
        let m = map(UNIFIED);
        // Header and decoration rows carry no numbers.
        assert_eq!(m.get(0), LineNumbers::none());
        assert_eq!(m.get(1), LineNumbers::none());
        // A context line has both sides.
        assert_eq!(
            m.get(2),
            LineNumbers {
                old: Some(1),
                new: Some(1)
            }
        );
        // A removed line has only the pre-image side...
        assert_eq!(
            m.get(3),
            LineNumbers {
                old: Some(2),
                new: None
            }
        );
        // ...and an added line only the post-image side.
        assert_eq!(
            m.get(4),
            LineNumbers {
                old: None,
                new: Some(2)
            }
        );
    }

    #[test]
    fn side_by_side_gutter_yields_both_sides() {
        let m = map(SIDE_BY_SIDE);
        assert_eq!(
            m.get(2),
            LineNumbers {
                old: Some(1),
                new: Some(1)
            }
        );
        assert_eq!(
            m.get(3),
            LineNumbers {
                old: Some(2),
                new: Some(2)
            }
        );
        // A pure insertion leaves the left panel blank.
        assert_eq!(
            m.get(4),
            LineNumbers {
                old: None,
                new: Some(3)
            }
        );
        assert_eq!(
            m.get(5),
            LineNumbers {
                old: Some(3),
                new: Some(4)
            }
        );
    }

    #[test]
    fn row_lookup_finds_the_anchored_line_in_both_modes() {
        for lines in [UNIFIED, SIDE_BY_SIDE] {
            let m = map(lines);
            let new3 = Anchor {
                side: Side::New,
                line: 3,
            };
            assert!(m.get(m.row_for(new3).expect("line 3 is rendered")).new == Some(3));
            let old2 = Anchor {
                side: Side::Old,
                line: 2,
            };
            assert!(m.get(m.row_for(old2).expect("old line 2 is rendered")).old == Some(2));
            // A line outside the diff has no row.
            assert_eq!(
                m.row_for(Anchor {
                    side: Side::New,
                    line: 900
                }),
                None
            );
        }
    }

    #[test]
    fn a_pipe_inside_code_cannot_outvote_the_real_gutter() {
        // Rust code drawing a box char preceded by digits would be a valid-looking
        // field on its own line; consensus across the render must ignore it.
        let m = map(&[
            "    1⋮    1│let border = \"    9│\";",
            "    2⋮    2│let other = 1;",
            "    3⋮    3│let third = 2;",
        ]);
        assert_eq!(
            m.get(0),
            LineNumbers {
                old: Some(1),
                new: Some(1)
            }
        );
        assert_eq!(
            m.get(2),
            LineNumbers {
                old: Some(3),
                new: Some(3)
            }
        );
    }

    #[test]
    fn no_gutter_at_all_anchors_nothing() {
        // delta produced no line numbers, so every anchor misses and the caller
        // falls back to appending the comments (see `render::splice`).
        let m = map(&["diff --git a/f b/f", "-old", "+new"]);
        assert!(!m.get(0).is_code());
        assert_eq!(
            m.row_for(Anchor {
                side: Side::New,
                line: 1
            }),
            None
        );
    }

    #[test]
    fn first_code_row_skips_deltas_header_decorations() {
        // The cursor opens here, so it must land past the file name and the rule
        // beneath it — otherwise `c` reports there's nothing to comment on.
        assert_eq!(map(UNIFIED).first_code_row(), Some(2));
        assert_eq!(map(SIDE_BY_SIDE).first_code_row(), Some(2));
        assert_eq!(map(&["no gutter here"]).first_code_row(), None);
    }

    #[test]
    fn anchor_prefers_the_post_image_side() {
        let ctx = LineNumbers {
            old: Some(4),
            new: Some(9),
        };
        assert_eq!(
            ctx.anchor(),
            Some(Anchor {
                side: Side::New,
                line: 9
            })
        );
        let removed = LineNumbers {
            old: Some(4),
            new: None,
        };
        assert_eq!(
            removed.anchor(),
            Some(Anchor {
                side: Side::Old,
                line: 4
            })
        );
        assert_eq!(LineNumbers::none().anchor(), None);
    }

    #[test]
    fn inserting_blanks_keeps_later_rows_findable() {
        let mut m = map(UNIFIED);
        let before = m
            .row_for(Anchor {
                side: Side::New,
                line: 4,
            })
            .unwrap();
        m.insert_blanks(3, 2);
        assert_eq!(m.len(), UNIFIED.len() + 2);
        assert_eq!(
            m.row_for(Anchor {
                side: Side::New,
                line: 4
            }),
            Some(before + 2)
        );
        // The inserted rows themselves carry nothing.
        assert_eq!(m.get(3), LineNumbers::none());
        assert_eq!(m.get(4), LineNumbers::none());
    }
}
