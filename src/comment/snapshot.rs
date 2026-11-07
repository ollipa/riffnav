//! Snapshots of the files comments were written against, which let a comment
//! follow its code when the file is edited (see [`super::reanchor`]).
//!
//! Content-addressed and shared: one file per distinct text, named by its hash,
//! under riffnav's state directory and never in the user's repo. Comments refer
//! to a snapshot by that name; [`Snapshots::sweep`] removes the ones none do.
//! Outside a repo nothing persists and snapshots live as long as the process.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use twox_hash::XxHash3_128;

use crate::state;

/// A snapshot younger than this is never swept: it may have just been written
/// for a comment whose store file hasn't landed yet.
const GRACE: Duration = Duration::from_secs(3600);

/// The text of a file, and the name it's stored under.
#[derive(Debug, PartialEq, Eq)]
pub struct Text {
    pub text: String,
    pub key: String,
}

impl Text {
    /// `text` with line endings made `\n`, so a file checked out with CRLF lines
    /// numbers and compares the way its diff does.
    pub fn new(text: &str) -> Self {
        let text = text.replace("\r\n", "\n");
        let key = format!("{:032x}", XxHash3_128::oneshot(text.as_bytes()));
        Self { text, key }
    }

    /// Line `line` (1-based), if the text has one.
    pub fn line(&self, line: u32) -> Option<&str> {
        self.text.lines().nth((line as usize).checked_sub(1)?)
    }
}

/// Where snapshots are kept, and the ones read so far.
pub struct Snapshots {
    /// `None` keeps them in memory only.
    dir: Option<PathBuf>,
    /// Read or written by this process, by key. `None` when there's no such file.
    known: RefCell<HashMap<String, Option<Rc<str>>>>,
}

impl Snapshots {
    pub fn at(dir: PathBuf) -> Self {
        Self {
            dir: Some(dir),
            known: RefCell::default(),
        }
    }

    pub fn memory() -> Self {
        Self {
            dir: None,
            known: RefCell::default(),
        }
    }

    /// Keep `text` under its key, best-effort: a snapshot that can't be written
    /// only means the comment falls back to finding its line by its text.
    pub fn save(&self, text: &Text) {
        // Checked on disk even when known: another process's sweep may have
        // taken it since, while nothing here referred to it.
        if let Some(dir) = &self.dir {
            let path = dir.join(&text.key);
            if !path.exists() {
                state::write_atomic(&path, text.text.as_bytes());
            }
        }
        let mut known = self.known.borrow_mut();
        known.insert(text.key.clone(), Some(Rc::from(text.text.as_str())));
    }

    /// The text stored under `key`, if it's still there.
    pub fn load(&self, key: &str) -> Option<Rc<str>> {
        if let Some(known) = self.known.borrow().get(key) {
            return known.clone();
        }
        // A key is a hash, but it came from a file anyone could edit.
        let valid = key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit());
        let text = self
            .dir
            .as_ref()
            .filter(|_| valid)
            .and_then(|dir| std::fs::read_to_string(dir.join(key)).ok())
            .map(Rc::from);
        self.known
            .borrow_mut()
            .insert(key.to_string(), text.clone());
        text
    }

    /// Delete the snapshots no comment in any scope under `comments` refers to.
    /// Best-effort, and nothing at all goes when a scope file can't be read, as
    /// its comments might refer to any of them. Run with the comments' lock held.
    pub fn sweep(&self, comments: &Path) {
        let Some(dir) = &self.dir else { return };
        let Some(used) = referenced(comments) else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if used.contains(name.to_string_lossy().as_ref()) {
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| now.duration_since(at).ok())
                .is_some_and(|age| age > GRACE);
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Every snapshot key the scope files under `comments` mention, or `None` if one
/// of them couldn't be read.
fn referenced(comments: &Path) -> Option<HashSet<String>> {
    #[derive(Deserialize)]
    struct Scope {
        #[serde(default)]
        comments: Vec<Note>,
    }
    #[derive(Deserialize)]
    struct Note {
        #[serde(default)]
        snapshot: Option<String>,
    }

    let mut used = HashSet::new();
    let Ok(repos) = std::fs::read_dir(comments) else {
        // No comments anywhere, so nothing refers to any snapshot.
        return Some(used);
    };
    for repo in repos.flatten() {
        let path = repo.path();
        if !path.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&path).ok()?.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                // Swept or renamed over since it was listed.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            };
            let scope: Scope = serde_json::from_str(&text).ok()?;
            used.extend(scope.comments.into_iter().filter_map(|c| c.snapshot));
        }
    }
    Some(used)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn age(path: &Path, by: Duration) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - by).unwrap();
    }

    #[test]
    fn a_saved_snapshot_reads_back_from_another_store() {
        let dir = TempDir::new("snapshots");
        let text = Text::new("a\r\nb\n");
        assert_eq!(text.text, "a\nb\n", "line endings are made plain");
        Snapshots::at(dir.join("s")).save(&text);
        let other = Snapshots::at(dir.join("s"));
        assert_eq!(other.load(&text.key).as_deref(), Some("a\nb\n"));
        assert_eq!(other.load("../../etc/passwd"), None);
    }

    #[test]
    fn sweep_removes_only_old_snapshots_nothing_refers_to() {
        let dir = TempDir::new("snapshots-sweep");
        let comments = dir.join("comments");
        let snaps = Snapshots::at(dir.join("snapshots"));
        let (kept, young, gone) = (Text::new("kept"), Text::new("young"), Text::new("gone"));
        for t in [&kept, &young, &gone] {
            snaps.save(t);
        }
        age(&dir.join("snapshots").join(&kept.key), GRACE * 2);
        age(&dir.join("snapshots").join(&gone.key), GRACE * 2);
        let scope = comments.join("repo").join("branch.json");
        let json = format!(r#"{{"comments":[{{"snapshot":"{}"}}]}}"#, kept.key);
        state::write_atomic(&scope, json.as_bytes());

        snaps.sweep(&comments);
        let there = |t: &Text| dir.join("snapshots").join(&t.key).exists();
        assert!(there(&kept), "referenced");
        assert!(there(&young), "too new to tell");
        assert!(!there(&gone));

        // A scope that doesn't parse might refer to anything.
        state::write_atomic(&comments.join("repo").join("bad.json"), b"{");
        age(&dir.join("snapshots").join(&young.key), GRACE * 2);
        snaps.sweep(&comments);
        assert!(there(&young));
    }
}
