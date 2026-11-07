//! Matching added blocks to their sources, and claiming the best.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use super::corpus::{Added, Corpus, Key, Run};
use super::{MAX_BLOCK_PLACES, MIN_ALNUM, MIN_LINES, MIN_SHAPES};
use crate::diff::FileDiff;

/// `len` lines of a run from `from` matching a stretch from `at`.
#[derive(Debug, Clone, Copy)]
pub(super) struct Block {
    pub(super) run: usize,
    pub(super) from: usize,
    pub(super) len: usize,
    pub(super) stretch: usize,
    pub(super) at: usize,
}

/// How many places each `(run, from, len)` of added lines turned up in.
type BlockPlaces = HashMap<(usize, usize, usize), usize>;

/// Every maximal match between `runs` and `corpus` that starts on a line worth
/// starting on.
fn matches(runs: &[Run], free: &[Vec<bool>], corpus: &Corpus) -> (Vec<Block>, BlockPlaces) {
    let mut blocks = Vec::new();
    let mut counts = HashMap::new();
    let mut seen = HashSet::new();
    for (run_at, run) in runs.iter().enumerate() {
        for (from, line) in run.lines.iter().enumerate() {
            if !line.code || !free[run_at][from] || !corpus.rare(line.key) {
                continue;
            }
            let places = corpus.places.get(&line.key).map_or(&[][..], Vec::as_slice);
            for place in places {
                let stretch = &corpus.stretches[place.stretch].lines;
                // Lines are walked top down, so a pair already seen sits inside a
                // block found from further up.
                if !seen.insert((run_at, from, place.stretch, place.at)) {
                    continue;
                }
                let len = run.lines[from..]
                    .iter()
                    .zip(&stretch[place.at..])
                    .take_while(|(added, kept)| added.key == kept.key)
                    .count();
                seen.extend(
                    (1..len).map(|step| (run_at, from + step, place.stretch, place.at + step)),
                );
                *counts.entry((run_at, from, len)).or_insert(0) += 1;
                blocks.push(Block {
                    run: run_at,
                    from,
                    len,
                    stretch: place.stretch,
                    at: place.at,
                });
            }
        }
    }
    (blocks, counts)
}

/// How far `block`'s source is from the code that copied it, nearest first: what
/// the reader can jump to, then the same file, then the same folder.
fn distance(block: &Block, runs: &[Run], corpus: &Corpus, files: &[FileDiff]) -> (bool, usize) {
    let stretch = &corpus.stretches[block.stretch];
    let origin = &corpus.origins[stretch.origin];
    let file = runs[block.run].file;
    let folder = |path: &str| path.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
    let near = if origin.file == Some(file) {
        0
    } else if folder(&origin.path) == folder(files[file].path()) {
        1
    } else {
        2
    };
    (!stretch.lines[block.at].shown, near)
}

/// The blocks that are worth a note, each added line going to one of them.
fn claim(
    mut candidates: Vec<Block>,
    counts: &BlockPlaces,
    runs: &[Run],
    free: &mut [Vec<bool>],
    corpus: &Corpus,
    files: &[FileDiff],
) -> Vec<Block> {
    candidates.sort_by_cached_key(|block| {
        let origin = &corpus.origins[corpus.stretches[block.stretch].origin];
        let line = corpus.stretches[block.stretch].lines[block.at].number;
        let (hidden, near) = distance(block, runs, corpus, files);
        (
            Reverse(block.len),
            hidden,
            near,
            origin.path.clone(),
            line,
            block.run,
            block.from,
        )
    });
    let mut blocks = Vec::new();
    for candidate in candidates {
        let places = counts
            .get(&(candidate.run, candidate.from, candidate.len))
            .copied()
            .unwrap_or(1);
        if places > MAX_BLOCK_PLACES {
            continue;
        }
        let mut at = 0;
        while at < candidate.len {
            let room = (at..candidate.len)
                .take_while(|&i| free[candidate.run][candidate.from + i])
                .count();
            let piece = Block {
                from: candidate.from + at,
                at: candidate.at + at,
                len: room,
                ..candidate
            };
            at += room.max(1);
            if let Some(block) = qualified(piece, runs, corpus) {
                free[block.run][block.from..block.from + block.len].fill(false);
                blocks.push(block);
            }
        }
    }
    blocks
}

/// `block` from its first line of code to its last, if that's enough to call a copy.
fn qualified(block: Block, runs: &[Run], corpus: &Corpus) -> Option<Block> {
    let lines = &runs[block.run].lines[block.from..block.from + block.len];
    let counts = |line: &&Added| line.code && corpus.rare(line.key);
    let lead = lines.iter().position(|line| counts(&line))?;
    let lines = &lines[lead..];
    let code: Vec<&Added> = lines.iter().filter(counts).collect();
    let distinct: HashSet<Key> = code.iter().map(|line| line.key).collect();
    let shapes: HashSet<Key> = code.iter().map(|line| line.shape).collect();
    let alnum: usize = code.iter().map(|line| line.alnum).sum();
    let enough = distinct.len() >= MIN_LINES && shapes.len() >= MIN_SHAPES;
    (enough && alnum >= MIN_ALNUM).then_some(Block {
        from: block.from + lead,
        at: block.at + lead,
        len: lines.len(),
        ..block
    })
}

/// One pass over what's in `corpus`, claiming what it can explain.
pub(super) fn search(
    runs: &[Run],
    free: &mut [Vec<bool>],
    corpus: &Corpus,
    files: &[FileDiff],
) -> Vec<Block> {
    let (candidates, counts) = matches(runs, free, corpus);
    claim(candidates, &counts, runs, free, corpus, files)
}
