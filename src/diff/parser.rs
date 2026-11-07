use std::borrow::Cow;

use super::model::{FileDiff, FileStatus, Hunk};
use super::rows::count_changes;

/// Split a unified diff into per-file sections. Anything before the first file
/// header (e.g. a `git show` commit header) is ignored.
pub fn parse(input: &str) -> Vec<FileDiff> {
    // git colorizes diffs when piping to a pager, so `pager.diff = riffnav` feeds
    // us ANSI-wrapped lines the markers below wouldn't match. Strip the codes up
    // front; the renderer colors from the plain text anyway.
    let input = strip_ansi(input);
    let input = input.as_ref();

    // Byte offsets where each file section begins, so we can slice `raw` verbatim.
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in input.split_inclusive('\n') {
        if line.starts_with("diff --git ") || combined_path(line).is_some() {
            starts.push(offset);
        }
        offset += line.len();
    }

    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = starts.get(i + 1).copied().unwrap_or(input.len());
            parse_one(&input[start..end])
        })
        .collect()
}

fn parse_one(raw: &str) -> FileDiff {
    let mut status = FileStatus::Modified;
    let mut old_path = None;
    let mut new_path = None;
    let mut additions = 0;
    let mut deletions = 0;
    let mut in_hunk = false;
    // Track whether each file header was present so the `diff --git` fallback
    // doesn't overwrite an explicit `/dev/null` (added/deleted) side.
    let mut saw_minus = false;
    let mut saw_plus = false;

    for line in raw.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if in_hunk {
            // Within a hunk, a leading '+'/'-' marks an added/removed line. The
            // `+++`/`---` file headers live before the first `@@`, so they can't
            // be miscounted here.
            match line.as_bytes().first() {
                Some(b'+') => additions += 1,
                Some(b'-') => deletions += 1,
                _ => {}
            }
            continue;
        }

        if let Some(p) = line.strip_prefix("rename from ") {
            old_path = Some(p.to_string());
            status = FileStatus::Renamed;
        } else if let Some(p) = line.strip_prefix("rename to ") {
            new_path = Some(p.to_string());
            status = FileStatus::Renamed;
        } else if let Some(p) = line.strip_prefix("copy from ") {
            old_path = Some(p.to_string());
            status = FileStatus::Copied;
        } else if let Some(p) = line.strip_prefix("copy to ") {
            new_path = Some(p.to_string());
            status = FileStatus::Copied;
        } else if line.starts_with("new file mode") {
            status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            status = FileStatus::Deleted;
        } else if let Some(p) = line.strip_prefix("--- ") {
            // `--combined-all-paths` gives a line per parent, and the old side
            // follows the first.
            if !saw_minus {
                old_path = side_path(p).or(old_path);
            }
            saw_minus = true;
        } else if let Some(p) = line.strip_prefix("+++ ") {
            saw_plus = true;
            new_path = side_path(p).or(new_path);
        } else if let Some((a, b)) = binary_paths(line) {
            old_path = old_path.or(a);
            new_path = new_path.or(b);
        }
    }

    // Fall back to the `diff --git a/<old> b/<new>` line, but only for a side
    // whose `---`/`+++` header we never saw (e.g. a pure rename with no hunks).
    // Skipping seen sides preserves an explicit `/dev/null` as `None`.
    if (!saw_minus || !saw_plus)
        && let Some((a, b)) = diff_git_paths(raw.lines().next().unwrap_or(""))
    {
        if !saw_minus {
            old_path = old_path.or(Some(a));
        }
        if !saw_plus {
            new_path = new_path.or(Some(b));
        }
    }

    // The marker columns can't be read without knowing where a combined hunk ends.
    if is_combined(raw) {
        (additions, deletions) = count_changes(raw);
    }

    FileDiff {
        old_path,
        new_path,
        status,
        additions,
        deletions,
        raw: raw.to_string(),
    }
}

/// Parse the `@@` headers out of one file's verbatim diff text (`FileDiff::raw`).
///
/// Computed on demand rather than during [`parse`], since only the comment CLI
/// needs it — the TUI works from [`super::rows`].
/// A combined (merge) diff's old side is its first parent, as it is on its rows.
pub fn hunks(raw: &str) -> Vec<Hunk> {
    if is_combined(raw) {
        let headers = raw.lines().filter_map(parse_combined_header);
        return headers.map(|combined| combined.hunk).collect();
    }
    raw.lines().filter_map(parse_hunk_header).collect()
}

/// The path in a combined diff's file header, which has just the one, unprefixed.
fn combined_path(line: &str) -> Option<&str> {
    line.strip_prefix("diff --cc ")
        .or_else(|| line.strip_prefix("diff --combined "))
}

/// Whether a file's verbatim diff text is a merge's combined diff.
pub(super) fn is_combined(raw: &str) -> bool {
    combined_path(raw).is_some()
}

/// The start of a combined diff's hunk, from a header like
/// `@@@ -1,6 -1,6 +1,10 @@@ fn foo()`, with one more `@` and one more range for
/// each parent past the first.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct CombinedHunk<'a> {
    /// The hunk as the first parent and the result see it.
    pub hunk: Hunk,
    /// How many lines git says the hunk has in each parent.
    pub parents: Vec<u32>,
    /// The enclosing-function text after the closing `@@@`, or `""`.
    pub context: &'a str,
}

pub(super) fn parse_combined_header(line: &str) -> Option<CombinedHunk<'_>> {
    let ats = line.bytes().take_while(|b| *b == b'@').count();
    if ats < 3 {
        return None;
    }
    let (ranges, tail) = line[ats..].split_once(&line[..ats])?;
    let mut ranges: Vec<&str> = ranges.split_whitespace().collect();
    let (new_start, new_len) = combined_range(ranges.pop()?.strip_prefix('+')?)?;
    let parents = ranges
        .iter()
        .map(|range| combined_range(range.strip_prefix('-')?))
        .collect::<Option<Vec<_>>>()?;
    let (old_start, old_len) = *parents.first()?;
    (parents.len() == ats - 1).then(|| CombinedHunk {
        hunk: Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            header: line.to_string(),
        },
        parents: parents.iter().map(|(_, len)| *len).collect(),
        context: tail.trim_start(),
    })
}

/// One range of a combined header. With `-U0` git can wrap a length below zero and
/// print 18446744073709551615, which is read as nothing.
fn combined_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().unwrap_or(0))),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// `@@ -1,5 +1,7 @@ fn foo()` -> its four ranges. A side written without a comma
/// (`-1`) has an implicit length of 1.
pub(super) fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let (old, new) = rest[..end].split_once(' ')?;
    let (old_start, old_len) = parse_range(old.strip_prefix('-')?)?;
    let (new_start, new_len) = parse_range(new.strip_prefix('+')?)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        header: line.to_string(),
    })
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// Remove ANSI CSI escape sequences (SGR color codes and friends) from `input`,
/// borrowing it unchanged when there are none — the common case of a plain diff
/// piped in. git emits these only when it thinks it's writing to a terminal,
/// which includes feeding its configured pager.
fn strip_ansi(input: &str) -> Cow<'_, str> {
    if !input.contains('\x1b') {
        return Cow::Borrowed(input);
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // A CSI sequence is ESC '[' then params/intermediates, terminated by
            // a final byte in 0x40..=0x7e. Anything else after ESC we drop alone.
            if bytes.get(i + 1) == Some(&b'[') {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1; // consume the final byte (no-op past end)
            } else {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Only whole ASCII escape runs were removed, so what remains is valid UTF-8.
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

/// Parse the path from a `--- `/`+++ ` header side, stripping the leading path
/// prefix and any trailing tab-timestamp. `/dev/null` becomes `None`.
fn side_path(s: &str) -> Option<String> {
    let s = s.split('\t').next().unwrap_or(s).trim_end();
    if s == "/dev/null" {
        return None;
    }
    Some(strip_path_prefix(s).to_string())
}

/// Strip git's leading path prefix. The default is `a/`/`b/`, but a diff piped
/// in from a `diff.mnemonicPrefix` user instead carries `c/` (commit), `i/`
/// (index), `o/` (object), or `w/` (working tree). riffnav's own git calls pin
/// `a/`/`b/` (see `autodiff::PREFIX_ARGS`), so this mainly covers external pipes.
fn strip_path_prefix(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && bytes[1] == b'/'
        && matches!(bytes[0], b'a' | b'b' | b'c' | b'i' | b'o' | b'w')
    {
        &s[2..]
    } else {
        s
    }
}

/// `Binary files a/x and b/y differ` -> (old, new) paths.
fn binary_paths(line: &str) -> Option<(Option<String>, Option<String>)> {
    let rest = line
        .strip_prefix("Binary files ")?
        .strip_suffix(" differ")?;
    let (a, b) = rest.split_once(" and ")?;
    Some((side_path(a), side_path(b)))
}

/// Heuristic fallback: split `diff --git a/<old> b/<new>` at the ` b/` boundary.
/// Good enough for paths without spaces; quoted/spaced paths are a known gap.
fn diff_git_paths(line: &str) -> Option<(String, String)> {
    if let Some(path) = combined_path(line) {
        return Some((path.to_string(), path.to_string()));
    }
    let rest = line.strip_prefix("diff --git ")?;
    let idx = rest.find(" b/")?;
    let a = strip_ab(&rest[..idx]);
    let b = strip_ab(rest[idx + 1..].trim_end());
    Some((a, b))
}

fn strip_ab(s: &str) -> String {
    strip_path_prefix(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_multiple_files() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n\
                    diff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -0,0 +1 @@\n+x\n";
        let files = parse(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), "a.rs");
        assert_eq!(files[1].path(), "b.rs");
    }

    #[test]
    fn counts_additions_and_deletions() {
        let diff =
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n ctx\n-gone\n+added\n+more\n";
        let f = &parse(diff)[0];
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!(f.additions, 2);
        assert_eq!(f.deletions, 1);
    }

    #[test]
    fn detects_added_file() {
        let diff = "diff --git a/new.rs b/new.rs\nnew file mode 100644\nindex 0..1\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1 @@\n+hi\n";
        let f = &parse(diff)[0];
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.old_path, None);
        assert_eq!(f.path(), "new.rs");
    }

    #[test]
    fn detects_deleted_file_and_uses_old_path() {
        let diff = "diff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\nindex 1..0\n--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n";
        let f = &parse(diff)[0];
        assert_eq!(f.status, FileStatus::Deleted);
        assert_eq!(f.new_path, None);
        assert_eq!(f.path(), "gone.rs");
    }

    #[test]
    fn detects_rename_without_hunks() {
        let diff = "diff --git a/old/name.rs b/new/name.rs\nsimilarity index 100%\nrename from old/name.rs\nrename to new/name.rs\n";
        let f = &parse(diff)[0];
        assert_eq!(f.status, FileStatus::Renamed);
        assert_eq!(f.old_path.as_deref(), Some("old/name.rs"));
        assert_eq!(f.path(), "new/name.rs");
    }

    #[test]
    fn parses_binary_file() {
        let diff = "diff --git a/img.png b/img.png\nindex 1..2 100644\nBinary files a/img.png and b/img.png differ\n";
        let f = &parse(diff)[0];
        assert_eq!(f.path(), "img.png");
        assert_eq!(f.additions, 0);
        assert_eq!(f.deletions, 0);
    }

    #[test]
    fn parses_colorized_diff_and_strips_codes() {
        // What git feeds a pager: every line wrapped in SGR color codes.
        let diff = "\x1b[1mdiff --git a/a.rs b/a.rs\x1b[m\n\
                    \x1b[1m--- a/a.rs\x1b[m\n\x1b[1m+++ b/a.rs\x1b[m\n\
                    \x1b[36m@@ -1 +1 @@\x1b[m\n\x1b[31m-old\x1b[m\n\x1b[32m+new\x1b[m\n";
        let files = parse(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "a.rs");
        assert_eq!(files[0].additions, 1);
        assert_eq!(files[0].deletions, 1);
        // raw is what gets rendered, so it must be free of codes.
        assert!(!files[0].raw.contains('\x1b'));
    }

    #[test]
    fn strips_mnemonic_prefixes_from_piped_diff() {
        // What a `diff.mnemonicPrefix = true` user pipes in: `c/` (commit) and
        // `w/` (working tree) prefixes instead of `a/`/`b/`. The path must come
        // out clean so the `o` key opens the real file.
        let diff = "diff --git c/src/file.rs w/src/file.rs\n\
                    index 1..2 100644\n--- c/src/file.rs\n+++ w/src/file.rs\n\
                    @@ -1 +1 @@\n-old\n+new\n";
        let f = &parse(diff)[0];
        assert_eq!(f.path(), "src/file.rs");
        assert_eq!(f.old_path.as_deref(), Some("src/file.rs"));
    }

    #[test]
    fn parses_hunk_ranges_including_trailing_context() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
                   @@ -10,5 +12,7 @@ fn outer() {\n ctx\n-gone\n+added\n\
                   @@ -40 +44 @@\n-x\n+y\n";
        let hs = hunks(raw);
        assert_eq!(hs.len(), 2);
        assert_eq!((hs[0].old_start, hs[0].old_len), (10, 5));
        assert_eq!((hs[0].new_start, hs[0].new_len), (12, 7));
        assert_eq!(hs[0].header, "@@ -10,5 +12,7 @@ fn outer() {");
        // A side with no comma means a length of exactly one line.
        assert_eq!((hs[1].old_start, hs[1].old_len), (40, 1));
        assert_eq!((hs[1].new_start, hs[1].new_len), (44, 1));
    }

    #[test]
    fn hunk_contains_respects_ranges_and_empty_sides() {
        let h = &hunks("@@ -0,0 +1,3 @@\n+a\n+b\n+c\n")[0];
        // An added file's pre-image side is empty, so it contains no old line.
        assert!(!h.contains_old(0));
        assert!(!h.contains_old(1));
        assert!(h.contains_new(1));
        assert!(h.contains_new(3));
        assert!(!h.contains_new(4));
    }

    #[test]
    fn hunk_parser_skips_malformed_headers() {
        // Outside a combined diff, `@@@` means nothing.
        assert!(hunks("@@@ -1,2 -1,2 +1,3 @@@\n").is_empty());
        assert!(hunks("@@ not a range @@\n").is_empty());
        // A bare `@@` inside a hunk body (as file content) must not parse.
        assert!(hunks("@@ -1 +1 @@\n-@@ oops\n").len() == 1);
    }

    /// What `git show` prints for a merge, around the body of its one hunk.
    fn merge(body: &str) -> String {
        format!(
            "commit abc\nMerge: 1 2\n\n    merge\n\ndiff --cc f\nindex 1,2..3\n--- a/f\n+++ b/f\n\
             @@@ -1,9 -1,9 +1,9 @@@ fn top()\n{body}"
        )
    }

    #[test]
    fn a_combined_diff_has_one_file_per_header() {
        let raw = format!("{}diff --combined g h\nindex 1,2..3\n", merge("  a\n"));
        let files = parse(&raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), "f");
        assert_eq!(files[0].status, FileStatus::Modified);
        assert_eq!(files[1].path(), "g h");
        assert_eq!(files[1].old_path.as_deref(), Some("g h"));
    }

    #[test]
    fn a_combined_diff_counts_the_rows_it_shows() {
        let f = &parse(&merge("  a\n- b\n -c\n--d\n +e\n+ f\n++g\n\ncommit def\n"))[0];
        assert_eq!((f.additions, f.deletions), (3, 3));
    }

    #[test]
    fn a_merge_can_add_and_delete_a_file() {
        let added = "diff --cc n\nindex 0,0..1\nnew file mode 100644\n--- /dev/null\n+++ b/n\n\
                     @@@ -1,0 -1,0 +1,1 @@@\n++x\n";
        let f = &parse(added)[0];
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!((f.old_path.as_deref(), f.path()), (None, "n"));
        assert_eq!(f.additions, 1);

        let deleted = "diff --cc d\nindex 1,2..0\ndeleted file mode 100644,100644\n--- a/d\n\
                       +++ /dev/null\n@@@ -1,1 -1,2 +1,0 @@@\n--x\n -y\n";
        let f = &parse(deleted)[0];
        assert_eq!(f.status, FileStatus::Deleted);
        assert_eq!((f.new_path.as_deref(), f.path()), (None, "d"));
        assert_eq!(f.deletions, 2);
    }

    #[test]
    fn the_first_parent_names_the_old_side() {
        let raw = "diff --cc new\nindex 1,2..3\n--- a/one\n--- a/two\n+++ b/new\n";
        let f = &parse(raw)[0];
        assert_eq!(f.old_path.as_deref(), Some("one"));
        assert_eq!(f.path(), "new");

        let raw = "diff --cc new\nindex 0,2..3\n--- /dev/null\n--- a/two\n+++ b/new\n";
        assert_eq!(parse(raw)[0].old_path, None);
    }

    #[test]
    fn a_combined_binary_file_takes_its_path_from_the_header() {
        let raw = "diff --cc i.png\nindex 1,2..3\nBinary files differ\n";
        assert_eq!(parse(raw)[0].path(), "i.png");
    }

    #[test]
    fn combined_hunks_are_the_first_parents() {
        let hs = hunks(
            "diff --cc f\n@@@ -2,7 -3,8 +4,9 @@@ fn top()\n  a\n\
             @@@@ -10 -20,0 -30,2 +40,5 @@@@\n   b\n",
        );
        assert_eq!(hs.len(), 2);
        assert_eq!((hs[0].old_start, hs[0].old_len), (2, 7));
        assert_eq!((hs[0].new_start, hs[0].new_len), (4, 9));
        assert_eq!((hs[1].old_start, hs[1].old_len), (10, 1));
        assert_eq!((hs[1].new_start, hs[1].new_len), (40, 5));
    }

    #[test]
    fn a_combined_header_gives_each_parent_a_range() {
        let parsed = parse_combined_header("@@@@ -1,2 -3 -5,6 +7,8 @@@@ fn top() @@@@").unwrap();
        assert_eq!(parsed.parents, [2, 1, 6]);
        assert_eq!(parsed.context, "fn top() @@@@");
        // One `@` past the first for each parent, or it isn't a header.
        assert_eq!(parse_combined_header("@@@ -1 -2 -3 +4 @@@"), None);
        assert_eq!(parse_combined_header("@@ -1 +1 @@"), None);
        assert_eq!(parse_combined_header("@@@ -1 -2 +x @@@"), None);
        assert_eq!(parse_combined_header("@@@ -1 -2 +3"), None);
    }

    #[test]
    fn a_length_git_wrapped_below_zero_reads_as_none() {
        let parsed = parse_combined_header("@@@ -13,1 -11,1 +9,18446744073709551615 @@@").unwrap();
        assert_eq!((parsed.hunk.new_start, parsed.hunk.new_len), (9, 0));
    }

    #[test]
    fn ignores_preamble_before_first_file() {
        let diff = "commit abc\nAuthor: x\n\n    msg\n\ndiff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "f");
    }
}
