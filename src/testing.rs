//! Fixtures for tests across the crate.

use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// An empty directory of its own, removed when dropped, so a failing test leaves
/// nothing behind and no two tests share one.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("riffnav-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A git repository in a directory of its own.
pub struct Repo(pub TempDir);

impl Repo {
    pub fn new() -> Self {
        let repo = Self(TempDir::new("repo"));
        repo.git(&["init", "-q"]);
        repo
    }

    pub fn run(&self, args: &[&str]) -> Output {
        Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&*self.0)
            .output()
            .unwrap()
    }

    pub fn git(&self, args: &[&str]) {
        let out = self.run(args);
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    pub fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    pub fn commit(&self, path: &str, text: &str) {
        self.write(path, text);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", "x"]);
    }
}
