//! Where sources are read from: the diff's own files, their old blobs, and git grep.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use super::corpus::{
    Added, Corpus, Kept, Key, Origin, Run, context_stretches, kept_stretches, key_of,
};
use super::filter::{extension, is_blank, is_source, says_generated};
use super::{
    GREP_LIMIT, Git, MAX_GREP_BYTES, MAX_OUTSIDE_FILES, MAX_PATTERNS, MAX_PLACES, MIN_LINES,
};
use crate::autodiff::Tree;
use crate::diff::{FileDiff, Row};
use crate::render::{blobs, starts};

/// What reading from git has to go on.
pub(super) struct Reading<'a> {
    pub(super) git: &'a Git,
    /// Blob bytes still to spend.
    pub(super) budget: usize,
    pub(super) cancelled: &'a AtomicBool,
}

impl Reading<'_> {
    fn blobs(&mut self, specs: &[String]) -> Option<Vec<Option<String>>> {
        blobs::read_blobs(&self.git.repo, specs, &mut self.budget, self.cancelled)
    }
}

/// Add the files of the diff as they are outside the changes: read in full when
/// their blobs can be had, as the context in the diff otherwise.
pub(super) fn add_diff_files(
    corpus: &mut Corpus,
    files: &[FileDiff],
    rows: &[Vec<Row>],
    extensions: &HashSet<String>,
    reading: Option<&mut Reading<'_>>,
    cancelled: &AtomicBool,
) -> Option<()> {
    let mut candidates = Vec::new();
    for (at, file) in files.iter().enumerate() {
        let path = file.path();
        let same_language = extension(path).is_some_and(|ext| extensions.contains(&ext));
        if file.new_path.is_some() && is_source(path) && same_language {
            candidates.push(at);
        }
    }
    let ids: Vec<(usize, String)> = candidates
        .iter()
        .filter_map(|&at| Some((at, starts::old_blob(&files[at].raw)?.to_string())))
        .collect();
    let mut blobs: HashMap<usize, String> = HashMap::new();
    if let Some(reading) = reading {
        let specs: Vec<String> = ids.iter().map(|(_, id)| id.clone()).collect();
        for ((at, _), text) in ids.iter().zip(reading.blobs(&specs)?) {
            if let Some(text) = text {
                blobs.insert(*at, text);
            }
        }
    }

    for at in candidates {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let file = &files[at];
        let kept = blobs
            .get(&at)
            .and_then(|old| kept_stretches(old, &file.raw, &rows[at]))
            .unwrap_or_else(|| context_stretches(&rows[at]));
        let origin = Origin {
            file: Some(at),
            path: file.path().to_string(),
        };
        corpus.add(origin, kept);
    }
    Some(())
}

/// What's left of the runs after `free`, as lines to look for elsewhere: the longest
/// of each half of [`MIN_LINES`] lines of code, as a block that long, wherever it
/// starts, holds at least one such half whole.
pub(super) fn rare_lines(
    runs: &[Run],
    rows: &[Vec<Row>],
    free: &[Vec<bool>],
    corpus: &Corpus,
) -> Vec<String> {
    let mut patterns = HashSet::new();
    for (at, run) in runs.iter().enumerate() {
        let open = run
            .lines
            .iter()
            .zip(&free[at])
            .filter(|(line, free)| **free && line.code && corpus.rare(line.key))
            .map(|(line, _)| line);
        let open: Vec<&Added> = open.collect();
        for window in open.chunks(MIN_LINES / 2) {
            if let Some(line) = window.iter().max_by_key(|line| line.alnum) {
                patterns.insert(rows[run.file][line.row].text.trim().to_string());
            }
        }
    }
    let mut patterns: Vec<String> = patterns.into_iter().collect();
    patterns.sort_by_key(|line| (Reverse(line.len()), line.clone()));
    patterns.truncate(MAX_PATTERNS);
    patterns
}

/// Add the files outside the diff that share lines with what's left of the runs.
pub(super) fn add_outside_files(
    corpus: &mut Corpus,
    reading: &mut Reading<'_>,
    tree: &Tree,
    patterns: &[String],
    files: &[FileDiff],
    extensions: &HashSet<String>,
) -> Option<()> {
    let globs: Vec<String> = extensions
        .iter()
        .map(|ext| format!(":(top,glob,icase)**/*.{ext}"))
        .collect();
    let limits = (GREP_LIMIT, MAX_GREP_BYTES);
    let repo = &reading.git.repo;
    let hits = blobs::grep(repo, tree, patterns, &globs, limits, reading.cancelled)?;

    let mut buffer = String::new();
    let mut counts: HashMap<Key, usize> = HashMap::new();
    for hit in &hits {
        *counts.entry(key_of(&hit.text, &mut buffer)).or_insert(0) += 1;
    }
    let sides = files
        .iter()
        .flat_map(|file| [&file.old_path, &file.new_path]);
    let touched: HashSet<&str> = sides.flatten().map(String::as_str).collect();
    let mut matched: HashMap<&str, usize> = HashMap::new();
    for hit in &hits {
        let key = key_of(&hit.text, &mut buffer);
        let rare = counts.get(&key).is_some_and(|&count| count <= MAX_PLACES);
        let path = hit.path.as_str();
        // `git cat-file` takes a name a line, so one with a newline can't be read.
        if rare && is_source(path) && !touched.contains(path) && !path.contains('\n') {
            *matched.entry(path).or_insert(0) += 1;
        }
    }
    corpus.seen_elsewhere.extend(counts);

    let mut paths: Vec<(&str, usize)> = matched.into_iter().collect();
    paths.sort_by_key(|&(path, count)| (Reverse(count), path));
    paths.truncate(MAX_OUTSIDE_FILES);
    let specs: Vec<String> = paths.iter().map(|(path, _)| tree.spec(path)).collect();
    for ((path, _), text) in paths.iter().zip(reading.blobs(&specs)?) {
        let Some(text) = text.filter(|text| !says_generated(text.lines())) else {
            continue;
        };
        let lines = text.lines().enumerate().filter(|(_, line)| !is_blank(line));
        let lines = lines.map(|(at, line)| Kept {
            key: key_of(line, &mut buffer),
            number: at as u32 + 1,
            shown: false,
        });
        let origin = Origin {
            file: None,
            path: (*path).to_string(),
        };
        corpus.add(origin, vec![lines.collect()]);
    }
    Some(())
}
