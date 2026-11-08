use std::borrow::Cow;

use super::model::{FileDiff, FileStatus, Hunk};
use super::path;
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
        if line.starts_with("diff --git ")
            || combined_path(line).is_some()
            || unmerged_path(line).is_some()
        {
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
    if let Some(name) = unmerged_path(raw.lines().next().unwrap_or("")) {
        return FileDiff {
            old_path: Some(name.to_string()),
            new_path: Some(name.to_string()),
            status: FileStatus::Unmerged,
            additions: 0,
            deletions: 0,
            raw: raw.to_string(),
        };
    }
    let mut status = FileStatus::Modified;
    let mut old_path = None;
    let mut new_path = None;
    let mut in_hunk = false;
    // Track whether each file header was present so the `diff --git` fallback
    // doesn't overwrite an explicit `/dev/null` (added/deleted) side.
    let mut saw_minus = false;
    let mut saw_plus = false;
    let combined = is_combined(raw);

    for line in raw.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if in_hunk {
            continue;
        }

        if let Some(p) = line.strip_prefix("rename from ") {
            old_path = Some(whole_path(p).into_owned());
            status = FileStatus::Renamed;
        } else if let Some(p) = line.strip_prefix("rename to ") {
            new_path = Some(whole_path(p).into_owned());
            status = FileStatus::Renamed;
        } else if let Some(p) = line.strip_prefix("copy from ") {
            old_path = Some(whole_path(p).into_owned());
            status = FileStatus::Copied;
        } else if let Some(p) = line.strip_prefix("copy to ") {
            new_path = Some(whole_path(p).into_owned());
            status = FileStatus::Copied;
        } else if line.starts_with("new file mode") {
            status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            status = FileStatus::Deleted;
        } else if let Some(p) = line.strip_prefix("--- ") {
            // `--combined-all-paths` gives a line per parent, and the old side
            // follows the first.
            if !(combined && saw_minus) {
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

    // Counted by the hunks' own lengths: text after the last one, such as a
    // `format-patch` signature, isn't part of the diff, and a combined hunk's
    // marker columns can't be read without knowing where it ends.
    let (additions, deletions) = count_changes(raw);

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
/// A combined (merge) diff's old side is its first parent, as it is on its rows.
pub fn hunks(raw: &str) -> Vec<Hunk> {
    if is_combined(raw) {
        let headers = raw.lines().filter_map(parse_combined_header);
        return headers.map(|combined| combined.hunk).collect();
    }
    raw.lines().filter_map(parse_hunk_header).collect()
}

/// What identifies one file's change, whatever the context it was diffed with:
/// the file header without its `index <sha>..<sha>` line (blob-SHA churn alone
/// isn't a new change), and each changed line with its line number. Context
/// lines and hunk headers grow and shrink with `-U`, so they're left out.
pub fn change_identity(raw: &str) -> Vec<u8> {
    let combined = is_combined(raw);
    let mut out = Vec::with_capacity(raw.len());
    let mut in_hunks = false;
    // The next line number on the old and new side; a combined diff tracks
    // only the result's.
    let (mut old, mut new) = (0u32, 0u32);
    let mut parents = 1;
    let mut after_change = false;
    for line in raw.lines() {
        let header = if combined {
            parse_combined_header(line).map(|c| {
                parents = c.parents.len();
                c.hunk
            })
        } else {
            parse_hunk_header(line)
        };
        if let Some(hunk) = header {
            in_hunks = true;
            (old, new) = (hunk.old_start, hunk.new_start);
            after_change = false;
            continue;
        }
        if !in_hunks {
            if !line.starts_with("index ") {
                out.extend_from_slice(line.as_bytes());
                out.push(b'\n');
            }
            continue;
        }
        if line.starts_with('\\') {
            // `\ No newline at end of file` belongs to the line before it.
            if after_change {
                out.extend_from_slice(line.as_bytes());
                out.push(b'\n');
            }
            continue;
        }
        let width = if combined { parents } else { 1 };
        let marks = line.get(..width).unwrap_or(line);
        let text = line.get(width..).unwrap_or("");
        after_change = marks.bytes().any(|b| b != b' ');
        let removed = marks.contains('-');
        if after_change {
            let at = if removed && !combined { old } else { new };
            out.extend_from_slice(format!("{marks}{at}:{text}\n").as_bytes());
        }
        if !marks.contains('+') {
            old += 1;
        }
        if !removed {
            new += 1;
        }
    }
    out
}

/// The path in a combined diff's file header, which has just the one, unprefixed.
fn combined_path(line: &str) -> Option<&str> {
    line.strip_prefix("diff --cc ")
        .or_else(|| line.strip_prefix("diff --combined "))
}

/// The path in git's `* Unmerged path <name>` line, which stands for a file a
/// merge left in conflict in a way git can't diff. The name is written as is,
/// never quoted.
fn unmerged_path(line: &str) -> Option<&str> {
    let name = line.strip_prefix("* Unmerged path ")?;
    Some(name.strip_suffix('\n').unwrap_or(name))
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
/// borrowing it unchanged when there are none.
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
/// prefix and anything after the name: git ends a name holding a space with a tab,
/// and other tools add a timestamp after one. `/dev/null` becomes `None`.
fn side_path(s: &str) -> Option<String> {
    match path::unquote(s) {
        Some((name, _)) => named_path(&name),
        None => named_path(s.split_once('\t').map_or(s.trim_end(), |(name, _)| name)),
    }
}

/// A side's name without its prefix. `/dev/null` becomes `None`.
fn named_path(name: &str) -> Option<String> {
    (name != "/dev/null").then(|| strip_path_prefix(name).to_string())
}

/// `s` as a name that's all of it, quoted or not.
fn whole_path(s: &str) -> Cow<'_, str> {
    match path::unquote(s) {
        Some((name, "")) => Cow::Owned(name),
        _ => Cow::Borrowed(s),
    }
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
    let (a, b) = split_sides(rest, " and ").or_else(|| {
        let (a, b) = rest.split_once(" and ")?;
        Some((a.into(), b.into()))
    })?;
    Some((named_path(&a), named_path(&b)))
}

/// Split `<old><sep><new>`, where either side may be quoted, into its two names,
/// which keep their prefixes. Only a name with no quotes can hold `sep`, and a name
/// with no quotes can't hold `"`. When neither side is quoted the split is the one
/// that gives both sides the same path, as they have unless the file moved, or else
/// the one that leaves a side as `/dev/null`. `None` if there's no telling.
fn split_sides<'a>(rest: &'a str, sep: &str) -> Option<(Cow<'a, str>, Cow<'a, str>)> {
    if let Some((old, tail)) = path::unquote(rest) {
        return Some((old.into(), whole_path(tail.strip_prefix(sep)?)));
    }
    if rest.ends_with('"')
        && let Some(at) = rest.find(&format!("{sep}\""))
        && let Some((new, "")) = path::unquote(&rest[at + sep.len()..])
    {
        return Some((rest[..at].into(), new.into()));
    }
    let splits = || {
        let at = rest.match_indices(sep);
        at.map(|(at, _)| (&rest[..at], &rest[at + sep.len()..]))
    };
    let (old, new) = splits()
        .find(|(old, new)| strip_path_prefix(old) == strip_path_prefix(new))
        .or_else(|| splits().find(|sides| sides.0 == "/dev/null" || sides.1 == "/dev/null"))?;
    Some((old.into(), new.into()))
}

/// The two paths of a file's first line, for a side with no `---`/`+++` header. A
/// file that moved has its names on lines of their own, so this mostly sees a
/// file with the same path on both sides, and otherwise splits at the ` b/`.
fn diff_git_paths(line: &str) -> Option<(String, String)> {
    if let Some(name) = combined_path(line) {
        let name = whole_path(name).into_owned();
        return Some((name.clone(), name));
    }
    let rest = line.strip_prefix("diff --git ")?;
    let (a, b) = split_sides(rest, " ").or_else(|| {
        let idx = rest.find(" b/")?;
        Some((rest[..idx].into(), rest[idx + 1..].trim_end().into()))
    })?;
    Some((strip_ab(&a), strip_ab(&b)))
}

fn strip_ab(s: &str) -> String {
    strip_path_prefix(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path a merge left in conflict that git can't diff is a file of its own,
    /// with no hunks, and doesn't trail onto the combined diff before it.
    #[test]
    fn an_unmerged_path_is_a_file_with_no_hunks() {
        let diff = "diff --cc a.txt\nindex 1,2..0\n--- a/a.txt\n+++ b/a.txt\n\
                    @@@ -1,1 -1,1 +1,2 @@@\n +x\n+ y\n\
                    * Unmerged path sp ace.txt\n* Conflict: deleted by them\n\
                    * Unmerged path -d";
        let files = parse(diff);
        assert_eq!(files.len(), 3);
        assert!(!files[0].raw.contains("Unmerged"), "{:?}", files[0].raw);
        assert_eq!((files[0].additions, files[0].deletions), (2, 0));
        for (f, name) in files[1..].iter().zip(["sp ace.txt", "-d"]) {
            assert_eq!(f.status, FileStatus::Unmerged);
            assert_eq!(f.path(), name);
            assert_eq!(f.old_path.as_deref(), Some(name));
            assert_eq!((f.additions, f.deletions), (0, 0));
            assert!(hunks(&f.raw).is_empty());
        }
        assert_eq!(crate::diff::notes(&files[1]), ["deleted by them"]);
        assert!(crate::diff::notes(&files[2]).is_empty());
    }

    #[test]
    fn splits_multiple_files() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n\
                    diff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -0,0 +1 @@\n+x\n";
        let files = parse(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), "a.rs");
        assert_eq!(files[1].path(), "b.rs");
    }

    /// `git format-patch` ends the last file with a `-- ` signature line, and
    /// `git log -p` can put a subject starting with `-` or `+` between commits.
    #[test]
    fn text_after_the_last_hunk_is_not_counted() {
        let patch = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n-- \n2.43.0\n";
        let f = &parse(patch)[0];
        assert_eq!((f.additions, f.deletions), (1, 1));
        let log = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n\
                   commit 1\n\n    -fix the thing\n+++ not a header\n";
        let f = &parse(log)[0];
        assert_eq!((f.additions, f.deletions), (1, 1));
    }

    #[test]
    fn counts_additions_and_deletions() {
        let diff =
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,3 @@\n ctx\n-gone\n+added\n+more\n";
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
        // What a `diff.mnemonicPrefix = true` user pipes in. The path must come
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

    /// One file's paths, as (old, new).
    fn paths(raw: &str) -> (Option<String>, Option<String>) {
        let f = parse(raw).remove(0);
        (f.old_path, f.new_path)
    }

    fn same(path: &str) -> (Option<String>, Option<String>) {
        (Some(path.to_string()), Some(path.to_string()))
    }

    #[test]
    fn quoted_paths_are_read_from_every_header() {
        let modified = "diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\n\
                        --- \"a/caf\\303\\251.txt\"\n+++ \"b/caf\\303\\251.txt\"\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(paths(modified), same("café.txt"));

        let added = "diff --git \"a/t\\tn\" \"b/t\\tn\"\nnew file mode 100644\n--- /dev/null\n\
                     +++ \"b/t\\tn\"\n@@ -0,0 +1 @@\n+x\n";
        assert_eq!(paths(added), (None, Some("t\tn".to_string())));

        let renamed = "diff --git \"a/q\\\"1\" b/plain\nsimilarity index 100%\n\
                       rename from \"q\\\"1\"\nrename to plain\n";
        let f = &parse(renamed)[0];
        assert_eq!(f.status, FileStatus::Renamed);
        assert_eq!(f.old_path.as_deref(), Some("q\"1"));
        assert_eq!(f.path(), "plain");

        let copied = "diff --git a/plain \"b/n\\nl\"\nsimilarity index 100%\n\
                      copy from plain\ncopy to \"n\\nl\"\n";
        let f = &parse(copied)[0];
        assert_eq!(f.status, FileStatus::Copied);
        assert_eq!(f.path(), "n\nl");
    }

    #[test]
    fn a_quoted_name_with_a_space_still_ends_at_its_quote() {
        // git follows a name holding a space with a tab, quoted or not.
        let raw = "diff --git \"a/q\\\" r\" \"b/q\\\" r\"\n--- \"a/q\\\" r\"\t\n+++ \"b/q\\\" r\"\t\n\
                   @@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(paths(raw), same("q\" r"));
    }

    #[test]
    fn a_name_keeps_the_space_it_ends_in() {
        let raw = "diff --git a/x  b/x \n--- a/x \t\n+++ b/x \t\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(paths(raw), same("x "));
        assert_eq!(
            paths("diff --git a/x  b/x \nold mode 1\nnew mode 2\n"),
            same("x ")
        );
    }

    #[test]
    fn the_first_line_splits_where_both_sides_agree() {
        let line = |rest: &str| diff_git_paths(&format!("diff --git {rest}"));
        let both = |path: &str| Some((path.to_string(), path.to_string()));
        assert_eq!(line("a/x b/y.txt b/x b/y.txt"), both("x b/y.txt"));
        assert_eq!(line("a/sp ace.txt b/sp ace.txt"), both("sp ace.txt"));
        assert_eq!(line("c/m n w/m n"), both("m n"));
        assert_eq!(line(r#""a/t\tx" "b/t\tx""#), both("t\tx"));
        assert_eq!(
            line(r#""a/t\tx" b/plain y"#),
            Some(("t\tx".to_string(), "plain y".to_string()))
        );
        assert_eq!(
            line(r#"a/plain y "b/t\tx""#),
            Some(("plain y".to_string(), "t\tx".to_string()))
        );
        // A rename's sides differ, and its own lines settle it.
        assert_eq!(
            line("a/one b/two"),
            Some(("one".to_string(), "two".to_string()))
        );
    }

    #[test]
    fn a_mode_change_finds_its_path_on_the_first_line() {
        let raw = "diff --git a/x b/y.txt b/x b/y.txt\nold mode 100644\nnew mode 100755\n";
        assert_eq!(paths(raw), same("x b/y.txt"));
    }

    #[test]
    fn binary_lines_split_around_names_holding_the_word_and() {
        let binary = |sides: &str| binary_paths(&format!("Binary files {sides} differ")).unwrap();
        let some = |path: &str| Some(path.to_string());
        assert_eq!(
            binary("a/x and y and b/x and y"),
            (some("x and y"), some("x and y"))
        );
        assert_eq!(binary("/dev/null and b/x and y"), (None, some("x and y")));
        assert_eq!(binary("a/x and y and /dev/null"), (some("x and y"), None));
        assert_eq!(
            binary(r#""a/q\" and r" and "b/q\" and r""#),
            (some("q\" and r"), some("q\" and r"))
        );
        assert_eq!(
            binary(r#"/dev/null and "b/caf\303\251""#),
            (None, some("café"))
        );
        assert_eq!(binary(r#""a/t\tx" and /dev/null"#), (some("t\tx"), None));
        assert_eq!(
            binary(r#"a/x and y and "b/t\tx""#),
            (some("x and y"), some("t\tx"))
        );
        assert_eq!(binary("a/one and b/two"), (some("one"), some("two")));
    }

    #[test]
    fn a_merge_quotes_its_one_path_too() {
        let raw = "diff --cc \"caf\\303\\251.txt\"\nindex 1,2..3\nBinary files differ\n";
        assert_eq!(paths(raw), same("café.txt"));
        assert_eq!(
            paths("diff --combined \"t\\tx\"\nindex 1,2..3\n"),
            same("t\tx")
        );
    }

    #[test]
    fn a_name_that_only_starts_with_a_quote_is_left_alone() {
        assert_eq!(whole_path(r#""not closed"#), r#""not closed"#);
        assert_eq!(whole_path(r#""a" and more"#), r#""a" and more"#);
    }

    #[test]
    fn ignores_preamble_before_first_file() {
        let diff = "commit abc\nAuthor: x\n\n    msg\n\ndiff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "f");
    }
}
