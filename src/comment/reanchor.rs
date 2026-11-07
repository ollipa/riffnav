//! Moving comments with their code when the file is edited after they were
//! written.
//!
//! A comment records a snapshot of its side of the file (see [`super::snapshot`]).
//! When that side now reads differently, the snapshot is diffed against it: a line
//! that survived maps exactly, and the comment moves there. A line that was
//! rewritten moves to its replacement, one that was deleted to the nearest line
//! left, and either way the comment is marked outdated rather than passed off as
//! being about code it never saw. The new line and snapshot are stored, so
//! successive edits chain.
//!
//! A comment without a snapshot (written before snapshots, or swept) looks for its
//! line's text near where it was, and stays put if it isn't found.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use similar::{Algorithm, DiffOp, capture_diff_slices_deadline};

use super::content::{BlobReader, FileBlobs};
use super::snapshot::Text;
use super::{Comment, CommentStore, Side};

/// A diff of two versions of a file that takes longer than this is cut short;
/// the lines it didn't get to read as changed.
const DIFF_DEADLINE: Duration = Duration::from_millis(500);

/// How far from its old line a comment without a snapshot looks for its text.
const SEARCH_LINES: u32 = 100;

/// Where a line went.
#[derive(Debug, PartialEq, Eq)]
pub struct Moved {
    pub line: u32,
    /// The line itself didn't survive: this is its replacement, or the nearest
    /// line that did.
    pub outdated: bool,
}

/// Where line `line` (1-based) of `old` is in `new`. `None` if `old` has no such
/// line or `new` has no lines at all.
pub fn map_line(old: &str, new: &str, line: u32) -> Option<Moved> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let at = (line as usize).checked_sub(1).filter(|&i| i < old.len())?;
    if new.is_empty() {
        return None;
    }
    let deadline = Some(Instant::now() + DIFF_DEADLINE);
    let ops = capture_diff_slices_deadline(Algorithm::Patience, &old, &new, deadline);
    let op = ops.iter().find(|op| op.old_range().contains(&at))?;
    let (index, outdated) = match *op {
        DiffOp::Equal {
            old_index,
            new_index,
            ..
        } => (new_index + at - old_index, false),
        // Rewritten: to the same place in what replaced it, or its last line.
        DiffOp::Replace {
            old_index,
            new_index,
            new_len,
            ..
        } => (new_index + (at - old_index).min(new_len - 1), true),
        // Deleted: to the line that now follows, or precedes at the end.
        DiffOp::Delete { new_index, .. } => (new_index.min(new.len() - 1), true),
        DiffOp::Insert { .. } => unreachable!("an insert covers no old line"),
    };
    Some(Moved {
        line: index as u32 + 1,
        outdated,
    })
}

/// The line of `text` nearest `line` that reads as `code` does, within
/// [`SEARCH_LINES`]. A line too short to tell apart from others (a lone brace)
/// isn't looked for.
pub fn find_near(text: &str, code: &str, line: u32) -> Option<u32> {
    let code = code.trim();
    if code.chars().filter(|c| c.is_alphanumeric()).count() < 3 {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let at = line as i64;
    (1..=lines.len() as i64)
        .filter(|n| (n - at).unsigned_abs() <= u64::from(SEARCH_LINES))
        .filter(|&n| lines[n as usize - 1].trim() == code)
        .min_by_key(|n| (n - at).abs())
        .map(|n| n as u32)
}

/// How a comment changes to follow its code.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    line: u32,
    text: Arc<Text>,
    code: Option<String>,
    outdated: bool,
    diff_hash: String,
}

/// The files of the diff on screen, and what each side of them holds now.
pub struct Current<'r> {
    files: HashMap<String, FileBlobs>,
    reader: std::cell::RefCell<&'r mut BlobReader>,
}

impl<'r> Current<'r> {
    pub fn new(files: impl IntoIterator<Item = FileBlobs>, reader: &'r mut BlobReader) -> Self {
        Self {
            files: files.into_iter().map(|f| (f.path.clone(), f)).collect(),
            reader: std::cell::RefCell::new(reader),
        }
    }

    /// Read what every thread in `store` needs, in one go.
    pub fn prefetch(&self, store: &CommentStore) {
        let mut wants = Vec::new();
        for c in roots(store) {
            if let Some(f) = self.files.get(&c.file)
                && let Some((path, id)) = side_of(f, c.side)
            {
                wants.push((path, id));
            }
        }
        self.reader.borrow_mut().read(&wants);
    }

    /// What `side` of `file` holds now, if that can be told.
    pub fn text(&self, file: &str, side: Side) -> Option<Arc<Text>> {
        let (path, id) = side_of(self.files.get(file)?, side)?;
        self.reader.borrow_mut().get(path, id)
    }

    /// The hash of `file`'s diff, as comments record it.
    pub fn diff_hash(&self, file: &str) -> Option<&str> {
        self.files.get(file).map(|f| f.diff_hash.as_str())
    }

    /// Whether any thread in `store` would move.
    pub fn needs(&self, store: &CommentStore) -> bool {
        roots(store).any(|c| self.plan(store, c).is_some())
    }

    /// Move every thread in `store` whose code moved. Returns how many did. Run
    /// it inside [`CommentStore::update`], so it applies to the store as it is.
    pub fn apply(&self, store: &mut CommentStore) -> usize {
        let plans: Vec<(String, Change)> = roots(store)
            .filter_map(|c| Some((c.id.clone(), self.plan(store, c)?)))
            .collect();
        for (id, change) in &plans {
            store.snapshots().save(&change.text);
            store.edit(id, |c| {
                c.line = change.line;
                c.snapshot = Some(change.text.key.clone());
                c.code.clone_from(&change.code);
                c.outdated = change.outdated;
                c.diff_hash = Some(change.diff_hash.clone());
            });
        }
        plans.len()
    }

    /// How `c` changes to follow its code, or `None` if it stays as it is.
    fn plan(&self, store: &CommentStore, c: &Comment) -> Option<Change> {
        let now = self.text(&c.file, c.side)?;
        let diff_hash = self.diff_hash(&c.file)?.to_string();
        let change = |line: u32, code: Option<String>, outdated: bool| Change {
            line,
            text: now.clone(),
            code,
            outdated,
            diff_hash: diff_hash.clone(),
        };
        if c.snapshot.as_deref() == Some(now.key.as_str()) {
            // This side reads as it did, though the diff changed (the other side
            // did): the note is still on the code it was about, so it's no
            // longer flagged as on changed code.
            let stale = c.diff_hash.as_ref().is_some_and(|h| *h != diff_hash);
            return stale.then(|| change(c.line, c.code.clone(), c.outdated));
        }
        if let Some(then) = c
            .snapshot
            .as_deref()
            .and_then(|k| store.snapshots().load(k))
        {
            let moved = map_line(&then, &now.text, c.line)?;
            let code = match moved.outdated {
                // What was commented on, not what replaced it.
                true => c.code.clone(),
                false => now.line(moved.line).map(str::to_string),
            };
            return Some(change(moved.line, code, c.outdated || moved.outdated));
        }
        if let Some(code) = &c.code
            && let Some(line) = find_near(&now.text, code, c.line)
        {
            return Some(change(line, Some(code.clone()), c.outdated));
        }
        // Written before snapshots, against this very diff: the file is as the
        // comment saw it, so it can start following from here.
        if c.diff_hash.as_deref() == Some(diff_hash.as_str()) {
            let code = now.line(c.line)?.to_string();
            return Some(change(c.line, Some(code), c.outdated));
        }
        None
    }

    /// Fill in what a new comment on `side`/`line` of `file` follows its code by:
    /// the snapshot (saved to `store`'s) and the line's text.
    pub fn stamp(&self, store: &CommentStore, c: &mut Comment) {
        let Some(now) = self.text(&c.file, c.side) else {
            return;
        };
        store.snapshots().save(&now);
        c.snapshot = Some(now.key.clone());
        c.code = now.line(c.line).map(str::to_string);
    }
}

/// The comments that place a thread: every other one renders at its root's.
fn roots(store: &CommentStore) -> impl Iterator<Item = &Comment> {
    store
        .all()
        .iter()
        .filter(move |c| std::ptr::eq(store.root_of(c), *c))
}

/// Where to read `side` of `f`: the work-tree path, and the blob.
fn side_of(f: &FileBlobs, side: Side) -> Option<(Option<&str>, &str)> {
    match side {
        Side::Old => Some((None, f.old_blob.as_deref()?)),
        Side::New => Some((f.new_path.as_deref(), f.new_blob.as_deref()?)),
    }
}

/// Copy `parent`'s thread anchor onto a reply to it, so the reply moves with it.
pub fn inherit(store: &CommentStore, parent: &Comment, reply: &mut Comment) {
    let root = store.root_of(parent);
    reply.file.clone_from(&root.file);
    reply.side = root.side;
    reply.line = root.line;
    reply.diff_hash.clone_from(&root.diff_hash);
    reply.snapshot.clone_from(&root.snapshot);
    reply.code.clone_from(&root.code);
    reply.outdated = root.outdated;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::testing::note;
    use crate::testing::Repo;

    const BEFORE: &str = "fn a() {\n    one();\n    two();\n    three();\n}\n";

    #[test]
    fn a_line_moves_down_when_lines_are_inserted_above_it() {
        let after = "// header\n// more\nfn a() {\n    one();\n    two();\n    three();\n}\n";
        assert_eq!(
            map_line(BEFORE, after, 3),
            Some(Moved {
                line: 5,
                outdated: false
            })
        );
    }

    #[test]
    fn a_line_moves_up_when_lines_above_it_are_deleted() {
        let after = "fn a() {\n    three();\n}\n";
        assert_eq!(
            map_line(BEFORE, after, 4),
            Some(Moved {
                line: 2,
                outdated: false
            })
        );
    }

    #[test]
    fn an_edited_line_moves_to_its_replacement_and_is_outdated() {
        let after = "fn a() {\n    one();\n    TWO(2);\n    three();\n}\n";
        assert_eq!(
            map_line(BEFORE, after, 3),
            Some(Moved {
                line: 3,
                outdated: true
            })
        );
        // Rewritten into more lines, further down.
        let after = "// x\nfn a() {\n    one();\n    t(\n      2);\n    three();\n}\n";
        assert_eq!(map_line(BEFORE, after, 3).unwrap().line, 4);
    }

    #[test]
    fn a_deleted_line_moves_to_the_nearest_survivor_and_is_outdated() {
        let after = "fn a() {\n    one();\n    three();\n}\n";
        assert_eq!(
            map_line(BEFORE, after, 3),
            Some(Moved {
                line: 3,
                outdated: true
            })
        );
        // The last line deleted: there's nothing after it, so the one before.
        let after = "fn a() {\n    one();\n    two();\n    three();\n";
        assert_eq!(map_line(BEFORE, after, 5).unwrap().line, 4);
        assert_eq!(map_line(BEFORE, "", 3), None, "nothing left to land on");
    }

    #[test]
    fn find_near_takes_the_closest_match_and_skips_bare_punctuation() {
        let text = "x();\nkeep();\ny();\nz();\nkeep();\n";
        assert_eq!(find_near(text, "  keep();", 4), Some(5));
        assert_eq!(find_near(text, "keep();", 1), Some(2));
        assert_eq!(find_near(text, "gone();", 1), None);
        assert_eq!(find_near("}\n}\n", "}", 1), None);
    }

    /// A store and a reader over files whose texts are given directly: the
    /// reader is primed so no git runs.
    struct Fixture {
        store: CommentStore,
        reader: BlobReader,
        files: Vec<FileBlobs>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                store: CommentStore::disabled(),
                reader: BlobReader::new(None),
                files: Vec::new(),
            }
        }

        /// Make `f.rs` read `old` and `new` on its two sides now.
        fn sides(&mut self, old: &str, new: &str) {
            let (o, n) = (Text::new(old), Text::new(new));
            self.reader.prime(&o.key, o.text.clone());
            self.reader.prime(&n.key, n.text.clone());
            self.files = vec![FileBlobs {
                path: "f.rs".to_string(),
                new_path: Some("f.rs".to_string()),
                old_blob: Some(o.key),
                new_blob: Some(n.key),
                diff_hash: format!("{:032x}", crate::review::file_hash(&format!("{old}{new}"))),
            }];
        }

        fn add(&mut self, side: Side, line: u32) -> String {
            let current = Current::new(self.files.clone(), &mut self.reader);
            let mut c = Comment {
                side,
                created: 1,
                ..note("f.rs", line, "x")
            };
            current.stamp(&self.store, &mut c);
            self.store.add(c)
        }

        fn follow(&mut self) -> usize {
            let current = Current::new(self.files.clone(), &mut self.reader);
            current.prefetch(&self.store);
            current.apply(&mut self.store)
        }

        fn at(&self, id: &str) -> (u32, bool) {
            let c = self.store.get(id).unwrap();
            (c.line, c.outdated)
        }
    }

    #[test]
    fn successive_edits_chain() {
        let mut fx = Fixture::new();
        fx.sides("", BEFORE);
        let id = fx.add(Side::New, 3);
        assert_eq!(fx.follow(), 0, "nothing moved yet");

        fx.sides("", &format!("// 1\n{BEFORE}"));
        assert_eq!(fx.follow(), 1);
        assert_eq!(fx.at(&id), (4, false));

        // The next edit maps from the last one, not from the first.
        fx.sides("", &format!("// 0\n// 1\n// 1b\n{BEFORE}"));
        fx.follow();
        assert_eq!(fx.at(&id), (6, false));
        assert_eq!(
            fx.store.get(&id).unwrap().code.as_deref(),
            Some("    two();")
        );

        // Now the line itself goes; the note stays, outdated, and stays so.
        fx.sides(
            "",
            "// 0\n// 1\n// 1b\nfn a() {\n    one();\n    three();\n}\n",
        );
        fx.follow();
        assert_eq!(fx.at(&id), (6, true));
        fx.sides("", "// 0\n// 1b\nfn a() {\n    one();\n    three();\n}\n");
        fx.follow();
        assert_eq!(fx.at(&id), (5, true));
    }

    /// Only the other side changed: the note stays put and stops being flagged
    /// as on changed code.
    #[test]
    fn an_unchanged_side_only_takes_the_new_diff_hash() {
        let mut fx = Fixture::new();
        fx.sides(BEFORE, "new\n");
        let id = fx.add(Side::Old, 4);
        fx.store
            .edit(&id, |c| c.diff_hash = Some("then".to_string()));
        fx.sides(BEFORE, "newer\n");
        assert_eq!(fx.follow(), 1);
        let c = fx.store.get(&id).unwrap();
        assert_eq!((c.line, c.outdated), (4, false));
        assert_eq!(c.diff_hash.as_deref(), Some(fx.files[0].diff_hash.as_str()));
        assert_eq!(fx.follow(), 0, "settled");
    }

    #[test]
    fn the_old_side_maps_against_the_pre_image() {
        let mut fx = Fixture::new();
        fx.sides(BEFORE, "new\n");
        let id = fx.add(Side::Old, 4);
        fx.sides(&format!("// moved\n{BEFORE}"), "new\n");
        assert_eq!(fx.follow(), 1);
        assert_eq!(fx.at(&id), (5, false));
    }

    #[test]
    fn a_reply_moves_with_its_thread() {
        let mut fx = Fixture::new();
        fx.sides("", BEFORE);
        let root = fx.add(Side::New, 3);
        let mut reply = note("f.rs", 1, "reply");
        inherit(&fx.store, fx.store.get(&root).unwrap(), &mut reply);
        reply.reply_to = Some(root.clone());
        fx.store.add(reply);
        fx.sides("", &format!("// 1\n{BEFORE}"));
        assert_eq!(fx.follow(), 1, "only the root is moved");
        assert_eq!(fx.store.threads("f.rs")[0].0.line, 4);
        assert_eq!(fx.store.threads("f.rs")[0].1.len(), 2);
    }

    /// No snapshot: the stored line text is looked for near the old line, and a
    /// comment with neither stays put.
    #[test]
    fn a_comment_without_a_snapshot_finds_its_line_by_text() {
        let mut fx = Fixture::new();
        fx.sides("", &format!("// 1\n// 2\n{BEFORE}"));
        let found = fx.store.add(Comment {
            code: Some("    two();".to_string()),
            created: 1,
            ..note("f.rs", 3, "a")
        });
        let lost = fx.store.add(Comment {
            created: 2,
            diff_hash: Some("stale".to_string()),
            ..note("f.rs", 3, "b")
        });
        assert_eq!(fx.follow(), 1);
        assert_eq!(fx.at(&found), (5, false));
        assert!(
            fx.store.get(&found).unwrap().snapshot.is_some(),
            "and now follows"
        );
        assert_eq!(fx.at(&lost), (3, false));
        assert_eq!(fx.store.get(&lost).unwrap().snapshot, None);
    }

    /// One written before snapshots, against the diff still on screen, starts
    /// following from where it is.
    #[test]
    fn a_comment_on_an_unchanged_diff_is_adopted() {
        let mut fx = Fixture::new();
        fx.sides("", BEFORE);
        let hash = fx.files[0].diff_hash.clone();
        let id = fx.store.add(Comment {
            diff_hash: Some(hash),
            ..note("f.rs", 3, "a")
        });
        assert_eq!(fx.follow(), 1);
        assert_eq!(
            fx.store.get(&id).unwrap().code.as_deref(),
            Some("    two();")
        );
        fx.sides("", &format!("// 1\n{BEFORE}"));
        fx.follow();
        assert_eq!(fx.at(&id), (4, false));
    }

    /// Through a real repo: the comment written against the work tree follows an
    /// edit of it, and the store on disk says so.
    #[test]
    fn a_comment_follows_an_edit_of_the_work_tree_through_the_store() {
        let repo = Repo::new();
        repo.commit("f.rs", BEFORE);
        repo.write("f.rs", &BEFORE.replace("one", "uno"));
        let diff = |repo: &Repo| {
            let raw = String::from_utf8(repo.run(&["diff"]).stdout).unwrap();
            crate::diff::parse(&raw)
                .iter()
                .map(FileBlobs::of)
                .collect::<Vec<_>>()
        };
        let path = repo.0.join("state").join("scope.json");
        let mut store = CommentStore::with_path(path.clone());
        let mut reader = BlobReader::new(Some(repo.0.to_path_buf()));
        let current = Current::new(diff(&repo), &mut reader);
        let mut c = note("f.rs", 3, "why two?");
        current.stamp(&store, &mut c);
        let id = store.update(|s| s.add(c)).unwrap();

        repo.write(
            "f.rs",
            &format!("use x;\n\n{}", BEFORE.replace("one", "uno")),
        );
        let mut fresh = CommentStore::with_path(path.clone());
        fresh.reload();
        let mut reader = BlobReader::new(Some(repo.0.to_path_buf()));
        let current = Current::new(diff(&repo), &mut reader);
        assert!(current.needs(&fresh));
        assert_eq!(fresh.update(|s| current.apply(s)).unwrap(), 1);
        assert!(!current.needs(&fresh), "and it's settled");

        let mut disk = CommentStore::with_path(path);
        disk.reload();
        let c = disk.get(&id).unwrap();
        assert_eq!((c.line, c.outdated), (5, false));
        assert_eq!(c.code.as_deref(), Some("    two();"));
    }
}
