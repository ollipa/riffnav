use std::borrow::Cow;

use super::model::{FileDiff, FileStatus};

/// Split a unified diff into per-file sections. Anything before the first
/// `diff --git` line (e.g. a `git show` commit header) is ignored.
pub fn parse(input: &str) -> Vec<FileDiff> {
    // git colorizes diffs when piping to a pager, so `pager.diff = riffnav` feeds
    // us ANSI-wrapped lines the markers below wouldn't match. Strip the codes up
    // front; delta re-colors from the plain text anyway.
    let input = strip_ansi(input);
    let input = input.as_ref();

    // Byte offsets where each file section begins, so we can slice `raw` verbatim.
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in input.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
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
            saw_minus = true;
            old_path = side_path(p).or(old_path);
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

    FileDiff {
        old_path,
        new_path,
        status,
        additions,
        deletions,
        raw: raw.to_string(),
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
        // raw is handed to delta, which re-colors, so it must be free of codes.
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
    fn ignores_preamble_before_first_file() {
        let diff = "commit abc\nAuthor: x\n\n    msg\n\ndiff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "f");
    }
}
