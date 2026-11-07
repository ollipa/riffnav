//! Plain-text search across every file of a diff.
//!
//! A [`Hit`] names a row of [`crate::diff::rows`] and the characters it covers as
//! that row is shown, with tabs expanded. Neither depends on the pane's width, the
//! layout or the theme, so a resize, `s` or `T` leaves the list as it is, and the
//! render cache never has to know a search exists: matches are painted over the
//! cached text when it's drawn (see `render::marks`).
//!
//! The scan runs on the UI thread in slices of a few milliseconds, so typing in a
//! big diff stays responsive and a new keystroke just starts over.

use std::ops::Range;
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthChar;

use crate::diff::{FileDiff, RowKind, RowWalk};

/// Bytes scanned between looks at the clock. Counted in bytes, not rows, as one
/// row of a minified file can be longer than the rest of the diff put together.
const BYTES_PER_CHECK: usize = 64 * 1024;
/// What a row costs towards that, beyond its text.
const ROW_COST: usize = 32;

/// One match. `start..end` counts characters of the row as shown, where a tab is
/// the spaces it expands to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub file: u32,
    pub row: u32,
    pub start: u32,
    pub end: u32,
}

/// The match to pick up again after the diff was reloaded, by what it's on rather
/// than where it was in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resume {
    pub file: usize,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub start: u32,
}

/// Finds a query in a line. Smart case: a query with a capital in it is exact,
/// anything else ignores case.
struct Matcher {
    needle: String,
    fold: bool,
    /// The line being searched, lower-cased, kept to save an allocation per line.
    lowered: String,
    /// Per byte of `lowered`, the bytes of the character of the line it came from.
    /// Only filled for a line that isn't ASCII, where lower-casing can change a
    /// character's length.
    origin: Vec<Range<u32>>,
}

impl Matcher {
    fn new(query: &str) -> Self {
        let fold = !query.chars().any(char::is_uppercase);
        Self {
            needle: if fold {
                query.chars().flat_map(char::to_lowercase).collect()
            } else {
                query.to_string()
            },
            fold,
            lowered: String::new(),
            origin: Vec::new(),
        }
    }

    /// The byte ranges of `text` matching the query, in order and not overlapping.
    fn find(&mut self, text: &str, found: &mut Vec<Range<usize>>) {
        found.clear();
        if !self.fold {
            let hits = text.match_indices(&self.needle);
            found.extend(hits.map(|(at, m)| at..at + m.len()));
        } else if text.is_ascii() {
            self.lowered.clear();
            self.lowered.push_str(text);
            self.lowered.make_ascii_lowercase();
            let hits = self.lowered.match_indices(&self.needle);
            found.extend(hits.map(|(at, m)| at..at + m.len()));
        } else {
            self.find_folded(text, found);
        }
    }

    fn find_folded(&mut self, text: &str, found: &mut Vec<Range<usize>>) {
        self.lowered.clear();
        self.origin.clear();
        for (at, c) in text.char_indices() {
            let from = at as u32..(at + c.len_utf8()) as u32;
            for lower in c.to_lowercase() {
                self.lowered.push(lower);
                self.origin.resize(self.lowered.len(), from.clone());
            }
        }
        let mut end = 0;
        for (at, m) in self.lowered.match_indices(&self.needle) {
            let start = self.origin[at].start as usize;
            // One character can lower to two, and each half can be matched.
            if start >= end {
                end = self.origin[at + m.len() - 1].end as usize;
                found.push(start..end);
            }
        }
    }
}

/// Turn byte ranges of `text` into ranges of its characters as shown, the way
/// `render::layout` expands tabs. An end is carried past any combining marks, so a
/// match doesn't cut a letter from its accent when it's painted.
fn as_shown(text: &str, found: &[Range<usize>], tab: usize, mut hit: impl FnMut(u32, u32)) {
    if text.is_ascii() && !text.as_bytes().contains(&b'\t') {
        for range in found {
            hit(range.start as u32, range.end as u32);
        }
        return;
    }
    let mut chars = text.char_indices().peekable();
    let (mut shown, mut column) = (0, 0);
    let mut step = |c: char| {
        if c == '\t' {
            let to_next = tab - column % tab;
            column += to_next;
            shown += to_next as u32;
        } else {
            column += c.width().unwrap_or(0);
            shown += 1;
        }
        shown
    };
    let mut at = 0;
    for range in found {
        while let Some((_, c)) = chars.next_if(|(i, _)| *i < range.start) {
            at = step(c);
        }
        let start = at;
        while let Some((_, c)) = chars.next_if(|(i, _)| *i < range.end) {
            at = step(c);
        }
        while let Some((_, c)) = chars.next_if(|(_, c)| c.width() == Some(0)) {
            at = step(c);
        }
        if at > start {
            hit(start, at);
        }
    }
}

/// A search and its progress through the diff.
pub struct Search {
    pub query: String,
    matcher: Matcher,
    tab: usize,
    /// Diff indices in tree order, which is the order of the list and of `n`.
    order: Vec<usize>,
    /// Every match found so far, a file at a time in `order`, by row in a file.
    hits: Vec<Hit>,
    /// Per diff index, its share of `hits`.
    files: Vec<Range<usize>>,
    /// How far through `order` the scan has got, and where in that file.
    scanned: usize,
    walk: RowWalk,
    resume: Option<Resume>,
    /// The match `n` last landed on, as an index into `hits`.
    pub current: Option<usize>,
    /// `current`'s place in the order `n` visits matches, from 0, for the count.
    /// That's not its index in `hits`: side by side, the two columns interleave.
    pub place: usize,
}

impl Search {
    /// A search for `query`, or `None` when it's empty. `order` is the files in
    /// tree order, and `tab` the width tabs are expanded to.
    pub fn new(query: &str, files: usize, order: Vec<usize>, tab: usize) -> Option<Self> {
        (!query.is_empty()).then(|| Self {
            query: query.to_string(),
            matcher: Matcher::new(query),
            tab: tab.max(1),
            order,
            hits: Vec::new(),
            files: vec![0..0; files],
            scanned: 0,
            walk: RowWalk::default(),
            resume: None,
            current: None,
            place: 0,
        })
    }

    /// Have the scan make `resume` current when it comes across it.
    pub fn resume_at(&mut self, resume: Option<Resume>) {
        self.resume = resume;
    }

    /// Whether a reloaded diff's old match might still turn up.
    pub fn resuming(&self) -> bool {
        self.resume.is_some()
    }

    /// Carry the scan on for about `budget`.
    pub fn scan(&mut self, files: &[FileDiff], budget: Duration) {
        let deadline = Instant::now() + budget;
        let mut found = Vec::new();
        let mut scanned = 0;
        while let Some(&file) = self.order.get(self.scanned) {
            let raw = &files[file].raw;
            if self.walk.index == 0 {
                self.files[file] = self.hits.len()..self.hits.len();
            }
            loop {
                let index = self.walk.index;
                let Some(row) = self.walk.next(raw) else {
                    break;
                };
                if matches!(
                    row.kind,
                    RowKind::Context | RowKind::Added | RowKind::Removed
                ) {
                    self.matcher.find(row.text, &mut found);
                    as_shown(row.text, &found, self.tab, |start, end| {
                        let resume = Resume {
                            file,
                            old: row.old,
                            new: row.new,
                            start,
                        };
                        if self.resume == Some(resume) {
                            self.resume = None;
                            self.current = Some(self.hits.len());
                        }
                        self.hits.push(Hit {
                            file: file as u32,
                            row: index as u32,
                            start,
                            end,
                        });
                    });
                    self.files[file].end = self.hits.len();
                }
                scanned += row.text.len() + ROW_COST;
                if scanned >= BYTES_PER_CHECK {
                    scanned = 0;
                    if Instant::now() >= deadline {
                        return;
                    }
                }
            }
            self.scanned += 1;
            self.walk = RowWalk::default();
        }
        self.resume = None;
    }

    /// Whether the whole diff has been looked through.
    pub fn done(&self) -> bool {
        self.scanned >= self.order.len()
    }

    /// Matches found so far.
    pub fn total(&self) -> usize {
        self.hits.len()
    }

    pub fn hit(&self, index: usize) -> Option<Hit> {
        self.hits.get(index).copied()
    }

    /// The file at `rank` in tree order.
    pub fn file_at(&self, rank: usize) -> Option<usize> {
        self.order.get(rank).copied()
    }

    /// Files in the diff, whether or not they have matches.
    pub fn file_count(&self) -> usize {
        self.order.len()
    }

    /// Whether the file at `rank` has been read to its end, so what's known about
    /// it is everything there is.
    pub fn scanned(&self, rank: usize) -> bool {
        rank < self.scanned
    }

    /// `file`'s matches so far, by row.
    pub fn in_file(&self, file: usize) -> &[Hit] {
        self.files.get(file).map_or(&[], |r| &self.hits[r.clone()])
    }

    /// The index into the whole list of the first of `file`'s matches.
    pub fn first_in_file(&self, file: usize) -> usize {
        self.files.get(file).map_or(0, |r| r.start)
    }

    /// The match `n` last landed on.
    pub fn current_hit(&self) -> Option<Hit> {
        self.current.and_then(|i| self.hit(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse;
    use crate::diff::testing::{header, one};

    fn find(query: &str, text: &str) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        Matcher::new(query).find(text, &mut found);
        found.iter().map(|r| (r.start, r.end)).collect()
    }

    fn file(body: &str) -> FileDiff {
        let lines = body.lines().count();
        one(&format!("{}@@ -0,0 +1,{lines} @@\n{body}", header("f")))
    }

    fn scan(files: &[FileDiff], query: &str) -> Search {
        let order = (0..files.len()).collect();
        let mut search = Search::new(query, files.len(), order, 4).unwrap();
        search.scan(files, Duration::from_secs(60));
        assert!(search.done());
        search
    }

    fn shown(text: &str, query: &str) -> Vec<(u32, u32)> {
        let mut hits = Vec::new();
        let mut found = Vec::new();
        Matcher::new(query).find(text, &mut found);
        as_shown(text, &found, 4, |a, b| hits.push((a, b)));
        hits
    }

    #[test]
    fn a_lower_case_query_ignores_case_and_a_capital_makes_it_exact() {
        assert_eq!(find("foo", "Foo FOO foo"), [(0, 3), (4, 7), (8, 11)]);
        assert_eq!(find("Foo", "Foo FOO foo"), [(0, 3)]);
    }

    #[test]
    fn case_is_ignored_past_ascii_and_ranges_stay_in_the_original_text() {
        assert_eq!(find("äb", "xÄB"), [(1, 4)]);
        // Lower-casing `İ` gives two characters, so offsets can't be shared.
        assert_eq!(find("x", "İİx"), [(4, 5)]);
        assert_eq!(find("i", "İ"), [(0, 2)]);
        // An ASCII query reaches a character that only lowers to ASCII.
        assert_eq!(find("k", "\u{212a}"), [(0, 3)]);
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(find("aa", "aaaaa"), [(0, 2), (2, 4)]);
    }

    #[test]
    fn a_tab_counts_as_the_spaces_it_shows_as() {
        assert_eq!(shown("\tfoo", "foo"), [(4, 7)]);
        assert_eq!(shown("ab\tfoo\tfoo", "foo"), [(4, 7), (8, 11)]);
        // A tab inside a match widens it.
        assert_eq!(shown("a\tb", "a\tb"), [(0, 5)]);
    }

    #[test]
    fn wide_characters_take_one_character_but_two_columns_of_tab_stop() {
        assert_eq!(shown("日\tx", "x"), [(3, 4)]);
    }

    #[test]
    fn a_match_keeps_the_combining_marks_on_its_last_letter() {
        assert_eq!(shown("e\u{301}x", "e"), [(0, 2)]);
    }

    #[test]
    fn only_code_rows_are_searched() {
        let raw = "diff --git a/needle b/needle\n--- a/needle\n+++ b/needle\n\
                   @@ -1,2 +1,2 @@ fn needle()\n-needle\n+other\n needle\n\
                   \\ No newline at end of file\n";
        let files = [parse(raw).remove(0)];
        let rows: Vec<u32> = scan(&files, "needle").hits.iter().map(|h| h.row).collect();
        assert_eq!(rows, [1, 3]);
        assert_eq!(scan(&files, "newline").total(), 0);
    }

    #[test]
    fn the_list_follows_tree_order_not_diff_order() {
        let files = [file("+x\n"), file("+x\n")];
        let mut search = Search::new("x", 2, vec![1, 0], 4).unwrap();
        search.scan(&files, Duration::from_secs(60));
        let order: Vec<u32> = search.hits.iter().map(|h| h.file).collect();
        assert_eq!(order, [1, 0]);
        assert_eq!(search.in_file(0).len(), 1);
    }

    #[test]
    fn a_file_with_no_hunks_has_no_matches() {
        let binary = parse(
            "diff --git a/needle.png b/needle.png\nBinary files a/needle.png and b/needle.png differ\n",
        );
        assert_eq!(scan(&binary, "needle").total(), 0);
    }

    #[test]
    fn an_empty_query_is_no_search() {
        assert!(Search::new("", 0, Vec::new(), 4).is_none());
    }

    #[test]
    fn a_slice_at_a_time_finds_the_same_as_all_at_once() {
        let body = "+needle\n".repeat(BYTES_PER_CHECK / ROW_COST * 5);
        let files = [file(&body), file(&body)];
        let mut search = Search::new("needle", 2, vec![0, 1], 4).unwrap();
        let mut slices = 0;
        while !search.done() {
            search.scan(&files, Duration::ZERO);
            slices += 1;
        }
        assert!(slices > 5, "{slices}");
        assert_eq!(search.hits, scan(&files, "needle").hits);
    }

    #[test]
    fn a_reloaded_match_is_found_again_by_what_it_is_on() {
        let files = [file("+a\n+needle\n+needle\n")];
        let mut search = Search::new("needle", 1, vec![0], 4).unwrap();
        search.resume_at(Some(Resume {
            file: 0,
            old: None,
            new: Some(3),
            start: 0,
        }));
        search.scan(&files, Duration::from_secs(60));
        assert_eq!(search.current, Some(1));
    }
}
