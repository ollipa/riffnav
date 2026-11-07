//! What each side of a file in the diff holds now, for moving comments with
//! their code (see [`super::reanchor`]).
//!
//! A diff's `index` line names the blob on each side. The old side's is in the
//! object store whenever git could make the diff; the new side's is too for a
//! commit or the index, while a work-tree file is read from disk, and only when
//! it still hashes to the blob the diff names. Nothing is written to the repo.
//! A side that can't be read this way (a piped diff of another repo, a binary
//! file) has no current text, and comments on it stay where they are.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::snapshot::Text;
use crate::diff::FileDiff;
use crate::diff::path;
use crate::git;
use crate::render::{MAX_FILE_BYTES, read_blobs};

/// The blobs on each side of a diff, as its `index` line names them. A side
/// that doesn't exist (an added file's old side) is `None`.
pub fn blob_ids(raw: &str) -> (Option<String>, Option<String>) {
    let Some(ids) = raw
        .lines()
        .take_while(|line| !line.starts_with("@@"))
        .find_map(|line| line.strip_prefix("index "))
    else {
        return (None, None);
    };
    // `index <old>..<new>[ <mode>]`
    let ids = ids.split(' ').next().unwrap_or_default();
    let Some((old, new)) = ids.split_once("..") else {
        return (None, None);
    };
    let id = |id: &str| {
        let hex = !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit());
        (hex && id.bytes().any(|b| b != b'0')).then(|| id.to_string())
    };
    (id(old), id(new))
}

/// One file of the diff, as far as finding its current text goes.
#[derive(Debug, Clone)]
pub struct FileBlobs {
    /// The tree path comments name the file by.
    pub path: String,
    /// Where the new side is in the work tree.
    pub new_path: Option<String>,
    pub old_blob: Option<String>,
    pub new_blob: Option<String>,
    /// `review::file_hash` of the file's diff, as comments record it.
    pub diff_hash: String,
}

impl FileBlobs {
    pub fn of(f: &FileDiff) -> Self {
        let (old_blob, new_blob) = blob_ids(&f.raw);
        Self {
            path: f.path().to_string(),
            new_path: f.new_path.clone(),
            old_blob,
            new_blob,
            diff_hash: format!("{:032x}", crate::review::file_hash(&f.raw)),
        }
    }
}

/// How many blobs a [`BlobReader`] remembers before it forgets them all.
const MAX_KNOWN: usize = 256;

/// Reads blobs by id, remembering each: an id names its content for good.
pub struct BlobReader {
    /// The repository's top, which diff paths start from. `None` outside one.
    repo: Option<PathBuf>,
    known: HashMap<String, Option<Arc<Text>>>,
}

impl BlobReader {
    /// A reader for the repository the current directory is in.
    pub fn here() -> Self {
        Self::new(git::toplevel(Path::new(".")))
    }

    pub fn new(repo: Option<PathBuf>) -> Self {
        Self {
            repo,
            known: HashMap::new(),
        }
    }

    /// The text of blob `id`, reading `path` in the work tree when the store
    /// doesn't have it.
    pub fn get(&mut self, path: Option<&str>, id: &str) -> Option<Arc<Text>> {
        if !self.known.contains_key(id) {
            self.read(&[(path, id)]);
        }
        self.known.get(id).cloned().flatten()
    }

    /// Read every blob of `wants` not read yet, with one git for the object
    /// store and one more for any it doesn't have.
    pub fn read(&mut self, wants: &[(Option<&str>, &str)]) {
        // Every version of a file read in a long session is kept, so start over
        // now and then rather than grow without end.
        if self.known.len() > MAX_KNOWN {
            self.known.clear();
        }
        let mut fresh: Vec<(Option<&str>, &str)> = Vec::new();
        for &(path, id) in wants {
            if !self.known.contains_key(id) && !fresh.iter().any(|(_, i)| *i == id) {
                fresh.push((path, id));
            }
        }
        if fresh.is_empty() {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            for (_, id) in fresh {
                self.known.insert(id.to_string(), None);
            }
            return;
        };
        let specs: Vec<String> = fresh.iter().map(|(_, id)| id.to_string()).collect();
        let mut budget = usize::MAX;
        let texts = read_blobs(&repo, &specs, &mut budget, &AtomicBool::new(false))
            .unwrap_or_else(|| vec![None; specs.len()]);
        let mut missing = Vec::new();
        for ((path, id), text) in fresh.into_iter().zip(texts) {
            match text {
                Some(text) => {
                    self.known
                        .insert(id.to_string(), Some(Arc::new(Text::new(&text))));
                }
                None => missing.push((path, id)),
            }
        }
        let on_disk: Vec<(&str, &str)> = missing
            .iter()
            .filter_map(|&(path, id)| Some((path?, id)))
            .collect();
        let found = work_tree(&repo, &on_disk);
        for (_, id) in missing {
            let text = found.get(id).map(|text| Arc::new(Text::new(text)));
            self.known.insert(id.to_string(), text);
        }
    }
}

#[cfg(test)]
impl BlobReader {
    /// Answer `id` with `text` without asking git.
    pub fn prime(&mut self, id: &str, text: String) {
        self.known
            .insert(id.to_string(), Some(Arc::new(Text::new(&text))));
    }
}

/// The work-tree files of `wants` that hash to the blob named beside them, by
/// that blob. `git hash-object` hashes without writing anything.
fn work_tree<'a>(repo: &Path, wants: &[(&str, &'a str)]) -> HashMap<&'a str, String> {
    let mut found = HashMap::new();
    if wants.is_empty() {
        return found;
    }
    let Ok(mut git) = git::piped(repo)
        .args(["hash-object", "--stdin-paths"])
        .stderr(Stdio::null())
        .spawn()
    else {
        return found;
    };
    if let Some(mut stdin) = git.stdin.take() {
        for (path, _) in wants {
            // `--stdin-paths` reads a path a line; one with a newline can't go.
            let name = if path.contains('\n') { "" } else { path };
            let _ = stdin.write_all(path::to_os(name).as_encoded_bytes());
            let _ = stdin.write_all(b"\n");
        }
    }
    let Ok(out) = git.wait_with_output() else {
        return found;
    };
    let hashes = String::from_utf8_lossy(&out.stdout);
    for ((path, id), hash) in wants.iter().zip(hashes.lines()) {
        if path.contains('\n') || !hash.starts_with(id) {
            continue;
        }
        let file = repo.join(path::to_os(path));
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        let binary = bytes.iter().take(8000).any(|&b| b == 0);
        if bytes.len() <= MAX_FILE_BYTES && !binary {
            found.insert(*id, String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Repo;

    #[test]
    fn blob_ids_come_off_the_index_line() {
        let raw = "diff --git a/x b/x\nindex 1a2b..3c4d 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n";
        assert_eq!(
            blob_ids(raw),
            (Some("1a2b".to_string()), Some("3c4d".to_string()))
        );
        let added = "diff --git a/x b/x\nnew file mode 100644\nindex 0000000..3c4d\n";
        assert_eq!(blob_ids(added), (None, Some("3c4d".to_string())));
        assert_eq!(blob_ids("diff --git a/x b/x\n"), (None, None));
    }

    /// The old side comes from the store, the new side from the work tree, and a
    /// work-tree file that has moved on from the diff isn't taken for it.
    #[test]
    fn both_sides_are_read_and_a_changed_work_tree_is_not_trusted() {
        let repo = Repo::new();
        repo.commit("x.rs", "one\ntwo\n");
        repo.write("x.rs", "one\r\nthree\n");
        let raw = String::from_utf8(repo.run(&["diff"]).stdout).unwrap();
        let (old, new) = blob_ids(&raw);
        let (old, new) = (old.unwrap(), new.unwrap());

        let mut reader = BlobReader::new(Some(repo.0.to_path_buf()));
        reader.read(&[(Some("x.rs"), &old), (Some("x.rs"), &new)]);
        assert_eq!(reader.get(Some("x.rs"), &old).unwrap().text, "one\ntwo\n");
        assert_eq!(reader.get(Some("x.rs"), &new).unwrap().text, "one\nthree\n");

        repo.write("x.rs", "moved on\n");
        let mut fresh = BlobReader::new(Some(repo.0.to_path_buf()));
        assert_eq!(fresh.get(Some("x.rs"), &new), None);
    }
}
