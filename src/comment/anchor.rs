//! Diff line numbers per rendered row.
//!
//! Comments anchor to `(file, side, line)` in diff-line space, but they have to
//! be drawn next to a *row*. The renderer knows which numbers each row carries,
//! so [`LineMap`] just keeps them, aligned with the rows as comment blocks are
//! spliced in.

use super::store::{Anchor, Side};

/// Per rendered line, the diff line numbers it shows.
/// Index-aligned with `Text::lines`, and kept aligned as comment blocks are
/// spliced in (spliced rows get a `LineNumbers::none()`).
#[derive(Debug, Clone, Default)]
pub struct LineMap {
    rows: Vec<LineNumbers>,
    /// Per rendered line, the code on each side of it: unified uses the first.
    sources: Vec<[Option<Source>; 2]>,
    /// Per row of [`crate::diff::rows`], the change it's in.
    changes: Vec<Option<u32>>,
}

/// Where a rendered line's code came from, for painting search matches onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Source {
    /// The row of [`crate::diff::rows`] behind the code.
    pub row: u32,
    /// The line's first span of that code.
    pub first_span: usize,
    /// Which of the row's characters this line shows, tabs expanded: not all of
    /// them when the row was wrapped.
    pub first_char: u32,
    pub end_char: u32,
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
}

impl LineMap {
    /// A map for a render that already knows each row's numbers.
    pub fn from_numbers(rows: Vec<LineNumbers>) -> Self {
        let sources = vec![[None; 2]; rows.len()];
        Self {
            rows,
            sources,
            changes: Vec::new(),
        }
    }

    /// Say where the code of the listed lines came from.
    pub fn with_sources(mut self, sources: Vec<(usize, [Option<Source>; 2])>) -> Self {
        for (line, source) in sources {
            self.sources[line] = source;
        }
        self
    }

    /// Say which change each row of the diff is in.
    pub fn with_changes(mut self, changes: Vec<Option<u32>>) -> Self {
        self.changes = changes;
        self
    }

    /// Read the numbers out of rows written `"    1⋮    2│code"`, for tests that
    /// seed exact text rather than rendering a diff.
    #[cfg(test)]
    pub fn from_gutters(text: &ratatui::text::Text<'_>) -> Self {
        let rows = text.lines.iter().map(|line| {
            let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let Some((old, rest)) = plain.split_once('⋮') else {
                return LineNumbers::none();
            };
            let new = rest.split_once('│').map_or("", |(new, _)| new);
            LineNumbers {
                old: old.trim().parse().ok(),
                new: new.trim().parse().ok(),
            }
        });
        Self::from_numbers(rows.collect())
    }

    pub fn get(&self, row: usize) -> LineNumbers {
        self.rows.get(row).copied().unwrap_or_default()
    }

    /// What the code on `line` came from.
    pub fn sources(&self, line: usize) -> [Option<Source>; 2] {
        self.sources.get(line).copied().unwrap_or_default()
    }

    /// Every line's sources, in order.
    pub fn all_sources(&self) -> &[[Option<Source>; 2]] {
        &self.sources
    }

    /// The change the code on `line` is part of. Both sides of a side-by-side line
    /// are in the same one, if either is in one at all.
    pub fn change_on(&self, line: usize) -> Option<u32> {
        let sources = self.sources.get(line)?.iter().flatten();
        sources
            .filter_map(|s| self.changes.get(s.row as usize).copied().flatten())
            .next()
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
    /// past the file header and hunk headers. Where the cursor
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
        self.sources
            .splice(at..at, std::iter::repeat_n([None; 2], count));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows as `(old, new)`, with `(None, None)` for a header.
    fn map(rows: &[(Option<u32>, Option<u32>)]) -> LineMap {
        LineMap::from_numbers(
            rows.iter()
                .map(|&(old, new)| LineNumbers { old, new })
                .collect(),
        )
    }

    const ROWS: &[(Option<u32>, Option<u32>)] = &[
        (None, None),
        (None, None),
        (Some(1), Some(1)),
        (Some(2), None),
        (None, Some(2)),
        (None, Some(3)),
        (Some(3), Some(4)),
    ];

    #[test]
    fn row_lookup_finds_the_anchored_line_on_each_side() {
        let m = map(ROWS);
        let new3 = Anchor {
            side: Side::New,
            line: 3,
        };
        assert_eq!(m.row_for(new3), Some(5));
        let old2 = Anchor {
            side: Side::Old,
            line: 2,
        };
        assert_eq!(m.row_for(old2), Some(3));
        // A line outside the diff has no row.
        assert_eq!(
            m.row_for(Anchor {
                side: Side::New,
                line: 900
            }),
            None
        );
    }

    #[test]
    fn first_code_row_skips_the_headers() {
        // The cursor opens here, so it must land past the file name and the rule
        // beneath it — otherwise `c` reports there's nothing to comment on.
        assert_eq!(map(ROWS).first_code_row(), Some(2));
        assert_eq!(map(&[(None, None)]).first_code_row(), None);
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
        let mut m = map(ROWS);
        let anchor = Anchor {
            side: Side::New,
            line: 4,
        };
        let before = m.row_for(anchor).unwrap();
        m.insert_blanks(3, 2);
        assert_eq!(m.len(), ROWS.len() + 2);
        assert_eq!(m.row_for(anchor), Some(before + 2));
    }

    #[test]
    fn test_gutters_read_back_as_numbers() {
        let text = ratatui::text::Text::from("    2⋮     │code\n     ⋮    3│code\nheader");
        let m = LineMap::from_gutters(&text);
        assert_eq!((m.get(0).old, m.get(0).new), (Some(2), None));
        assert_eq!((m.get(1).old, m.get(1).new), (None, Some(3)));
        assert_eq!(m.get(2), LineNumbers::none());
    }
}
