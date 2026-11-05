//! One file's diff as typed rows: what the renderer draws, and the single place
//! that decides which line number a row carries.

use super::model::FileDiff;
use super::parser::parse_hunk_header;

/// What a row of the diff is. Only code rows carry line numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// The start of a hunk. `text` holds git's enclosing-function context, which
    /// is often empty.
    HunkHeader {
        new_start: u32,
    },
    Context,
    Added,
    Removed,
    /// git's `\ No newline at end of file` marker, describing the row above it.
    NoNewline,
}

/// One row of a file's diff, with the leading `+`/`-`/space already removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    /// Pre-image line number, on context and removed rows.
    pub old: Option<u32>,
    /// Post-image line number, on context and added rows.
    pub new: Option<u32>,
    pub text: String,
}

/// Split a file's verbatim diff text into rows.
///
/// Hunk lengths from the `@@` header decide where a hunk ends, not the look of
/// the lines: a removed line reading `-- foo` is `--- foo` in the diff, which a
/// marker check would take for a file header. `str::lines` also drops the `\r`
/// of a CRLF file, so it never reaches the screen.
pub fn rows(file: &FileDiff) -> Vec<Row> {
    let mut rows = Vec::new();
    let (mut old, mut new) = (0, 0);
    let (mut old_left, mut new_left) = (0, 0);

    for line in file.raw.lines() {
        if line.starts_with('\\') && !rows.is_empty() {
            rows.push(Row {
                kind: RowKind::NoNewline,
                old: None,
                new: None,
                text: line.trim_start_matches("\\ ").to_string(),
            });
            continue;
        }
        if old_left == 0 && new_left == 0 {
            if let Some(hunk) = parse_hunk_header(line) {
                (old, new) = (hunk.old_start, hunk.new_start);
                (old_left, new_left) = (hunk.old_len, hunk.new_len);
                rows.push(Row {
                    kind: RowKind::HunkHeader {
                        new_start: hunk.new_start,
                    },
                    old: None,
                    new: None,
                    text: hunk.context().to_string(),
                });
            }
            continue;
        }
        // `diff.suppressBlankEmpty` writes an empty context line with no marker,
        // so a line with no first byte is context rather than the end of the hunk.
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (RowKind::Added, &line[1..]),
            Some(b'-') => (RowKind::Removed, &line[1..]),
            Some(b' ') => (RowKind::Context, &line[1..]),
            None => (RowKind::Context, ""),
            Some(_) => continue,
        };
        let on_old = kind != RowKind::Added;
        let on_new = kind != RowKind::Removed;
        rows.push(Row {
            kind,
            old: on_old.then_some(old),
            new: on_new.then_some(new),
            text: text.to_string(),
        });
        if on_old {
            old += 1;
            old_left = old_left.saturating_sub(1);
        }
        if on_new {
            new += 1;
            new_left = new_left.saturating_sub(1);
        }
    }
    rows
}

/// What a file's diff says beyond its hunks, for the header: `binary file`,
/// `mode +x`. Several of these changes have no hunks at all, so without a note
/// the pane would be empty.
pub fn notes(file: &FileDiff) -> Vec<String> {
    let mut notes = Vec::new();
    let (mut old_mode, mut new_mode) = (None, None);
    for line in file.raw.lines() {
        if line.starts_with("@@") {
            break;
        }
        if let Some(mode) = line.strip_prefix("old mode ") {
            old_mode = Some(mode);
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            new_mode = Some(mode);
        } else if line == "GIT binary patch"
            || (line.starts_with("Binary files ") && line.ends_with(" differ"))
        {
            notes.push("binary file".to_string());
        }
    }
    if let (Some(old), Some(new)) = (old_mode, new_mode) {
        notes.push(match (old, new) {
            ("100644", "100755") => "mode +x".to_string(),
            ("100755", "100644") => "mode -x".to_string(),
            _ => format!("mode {old} ⟶ {new}"),
        });
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse;

    fn file(raw: &str) -> FileDiff {
        parse(raw).remove(0)
    }

    fn numbers(rows: &[Row]) -> Vec<(Option<u32>, Option<u32>)> {
        rows.iter().map(|r| (r.old, r.new)).collect()
    }

    #[test]
    fn numbers_each_side_independently() {
        let rows = rows(&file(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
             @@ -10,3 +20,3 @@ fn outer()\n a\n-b\n+c\n d\n",
        ));
        assert_eq!(rows[0].kind, RowKind::HunkHeader { new_start: 20 });
        assert_eq!(rows[0].text, "fn outer()");
        assert_eq!(
            numbers(&rows[1..]),
            [
                (Some(10), Some(20)),
                (Some(11), None),
                (None, Some(21)),
                (Some(12), Some(22)),
            ]
        );
    }

    #[test]
    fn a_removed_line_shaped_like_a_file_header_stays_code() {
        let rows = rows(&file(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n--- old\n+++ new\n x\n",
        ));
        assert_eq!(rows[1].kind, RowKind::Removed);
        assert_eq!(rows[1].text, "-- old");
        assert_eq!(rows[2].kind, RowKind::Added);
        assert_eq!(rows[2].text, "++ new");
    }

    #[test]
    fn the_missing_newline_marker_takes_no_line_number() {
        let rows = rows(&file(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\
             \\ No newline at end of file\n+b\n\\ No newline at end of file\n",
        ));
        assert_eq!(rows[2].kind, RowKind::NoNewline);
        assert_eq!(rows[2].text, "No newline at end of file");
        assert_eq!(rows[3].new, Some(1));
        assert_eq!(rows[4].new, None);
    }

    #[test]
    fn a_blank_context_line_with_no_marker_still_counts() {
        let rows = rows(&file(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n",
        ));
        assert_eq!(rows[2].kind, RowKind::Context);
        assert_eq!((rows[2].old, rows[2].new), (Some(2), Some(2)));
        assert_eq!(rows[3].old, Some(3));
    }

    #[test]
    fn crlf_never_reaches_the_row_text() {
        let rows = rows(&file(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\r\n+b\r\n",
        ));
        assert_eq!(rows[1].text, "a");
        assert_eq!(rows[2].text, "b");
    }

    #[test]
    fn a_file_without_hunks_has_no_rows_but_says_why() {
        let binary = file(
            "diff --git a/i.png b/i.png\nindex 1..2 100644\n\
             Binary files a/i.png and b/i.png differ\n",
        );
        assert!(rows(&binary).is_empty());
        assert_eq!(notes(&binary), ["binary file"]);

        let mode = file("diff --git a/s b/s\nold mode 100644\nnew mode 100755\n");
        assert!(rows(&mode).is_empty());
        assert_eq!(notes(&mode), ["mode +x"]);
    }

    #[test]
    fn a_mode_line_inside_a_hunk_is_code_not_a_note() {
        let f = file("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n old mode 100644\n");
        assert!(notes(&f).is_empty());
    }
}
