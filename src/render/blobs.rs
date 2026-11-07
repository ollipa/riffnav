//! Reading blobs and grepping through one git, which a cancelled job can stop.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{mem, thread};

use crate::autodiff::Tree;
use crate::diff::path;
use crate::git;

use super::highlight::{MAX_FILE_BYTES, MAX_LINE_BYTES};

/// How often a running git is checked on, to stop it if the job was cancelled.
const GIT_POLL: Duration = Duration::from_millis(2);

/// What reading a blob found.
pub(super) struct Blob {
    /// The whole text, if it isn't over the size limit.
    pub(super) text: Option<String>,
    /// The first line, if the blob is text and the line isn't too long to look
    /// at. Read even from a blob over the size limit, as it's all a language
    /// needs.
    pub(super) first: Option<String>,
}

/// A blob, if git has it. A cancelled job doesn't wait for git, and neither does
/// quitting, so nothing's left running.
pub(super) fn read_blob(repo: &Path, id: &str, cancelled: &AtomicBool) -> Option<Blob> {
    let mut git = git::piped(repo)
        .args(["cat-file", "--batch"])
        .spawn()
        .ok()?;
    let (stdin, stdout) = (git.stdin.take(), git.stdout.take());
    let text = thread::scope(|scope| {
        let reading = scope.spawn(|| blob_text(stdin?, stdout?, id));
        watch_git(&mut git, cancelled, || reading.is_finished());
        reading.join().ok().flatten()
    });
    // Stops git writing out a blob that turned out to be too big.
    let _ = git.kill();
    let _ = git.wait();
    text
}

/// Wait for `done`, stopping `git` if `cancelled` gets set first.
fn watch_git(git: &mut Child, cancelled: &AtomicBool, done: impl Fn() -> bool) {
    while !done() {
        if cancelled.load(Ordering::Relaxed) {
            let _ = git.kill();
            return;
        }
        thread::sleep(GIT_POLL);
    }
}

/// What each of `specs` names, in order, from one git. Each blob's size comes off
/// `budget`, and once that's used up the rest come back as `None`, as does anything
/// over the size limit, missing, or binary. `None` if `cancelled` was set first.
pub(super) fn read_blobs(
    repo: &Path,
    specs: &[String],
    budget: &mut usize,
    cancelled: &AtomicBool,
) -> Option<Vec<Option<String>>> {
    if specs.is_empty() {
        return Some(Vec::new());
    }
    let mut git = git::piped(repo)
        .args(["cat-file", "--batch"])
        .spawn()
        .ok()?;
    let (stdin, stdout) = (git.stdin.take(), git.stdout.take());
    let texts = thread::scope(|scope| {
        scope.spawn(|| {
            let mut stdin = stdin?;
            specs
                .iter()
                .try_for_each(|spec| {
                    stdin.write_all(path::to_os(spec).as_encoded_bytes())?;
                    stdin.write_all(b"\n")
                })
                .ok()
        });
        let reading = scope.spawn(|| blob_texts(stdout?, specs.len(), budget));
        watch_git(&mut git, cancelled, || reading.is_finished());
        reading.join().ok().flatten()
    });
    let _ = git.kill();
    let _ = git.wait();
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let mut texts = texts.unwrap_or_default();
    texts.resize(specs.len(), None);
    Some(texts)
}

/// Read `count` answers from `git cat-file --batch`, stopping short at the budget.
fn blob_texts(
    stdout: ChildStdout,
    count: usize,
    budget: &mut usize,
) -> Option<Vec<Option<String>>> {
    let mut out = BufReader::new(stdout);
    let mut texts = Vec::with_capacity(count);
    for _ in 0..count {
        let mut header = String::new();
        if out.read_line(&mut header).ok()? == 0 {
            break;
        }
        // `<id> blob <size>`, or `<spec> missing`, where a spec can have spaces in.
        let mut fields = header.split_whitespace().rev();
        let (size, kind) = (fields.next()?, fields.next());
        let Ok(size) = size.parse::<usize>() else {
            texts.push(None);
            continue;
        };
        if size > *budget {
            break;
        }
        // The newline that ends every answer comes with the body.
        let mut bytes = Vec::with_capacity(size + 1);
        (&mut out)
            .take(size as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        bytes.truncate(size);
        let binary = bytes.iter().take(8000).any(|&byte| byte == 0);
        if kind == Some("blob") && size <= MAX_FILE_BYTES && !binary {
            *budget -= size;
            texts.push(Some(String::from_utf8_lossy(&bytes).into_owned()));
        } else {
            texts.push(None);
        }
    }
    Some(texts)
}

/// A line `git grep` found.
pub(super) struct GrepHit {
    pub path: String,
    pub text: String,
}

/// Search `tree` for any of `patterns`, as fixed strings, in the files `pathspecs`
/// allow. `limits` are how long it gets and how much output, and what it had found
/// by then is the answer. `None` if `cancelled` was set first.
pub(super) fn grep(
    repo: &Path,
    tree: &Tree,
    patterns: &[String],
    pathspecs: &[String],
    limits: (Duration, u64),
    cancelled: &AtomicBool,
) -> Option<Vec<GrepHit>> {
    let mut command = git::piped(repo);
    command
        .args(["grep", "--no-color", "--no-column", "-F", "-I", "-n", "-z"])
        .args(["--full-name", "-f", "-"]);
    match tree {
        Tree::Index => command.arg("--cached"),
        Tree::Rev(rev) => command.arg(rev),
    };
    let mut git = command.arg("--").args(pathspecs).spawn().ok()?;
    let (stdin, stdout) = (git.stdin.take(), git.stdout.take());
    let (limit, max_bytes) = limits;
    let deadline = Instant::now() + limit;
    let output = thread::scope(|scope| {
        scope.spawn(|| {
            let mut stdin = stdin?;
            patterns
                .iter()
                .try_for_each(|line| writeln!(stdin, "{line}"))
                .ok()
        });
        let reading = scope.spawn(|| {
            let mut bytes = Vec::new();
            stdout?.take(max_bytes).read_to_end(&mut bytes).ok()?;
            Some(bytes)
        });
        watch_git(&mut git, cancelled, || {
            reading.is_finished() || Instant::now() >= deadline
        });
        // Ends the read, which keeps what it has.
        let _ = git.kill();
        reading.join().ok().flatten()
    });
    let _ = git.kill();
    let _ = git.wait();
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let output = output.unwrap_or_default();
    let prefix = match tree {
        Tree::Index => String::new(),
        Tree::Rev(rev) => format!("{rev}:"),
    };
    let mut hits = Vec::new();
    let mut rest = output.as_slice();
    // `-z` gives names as they are, so one can hold a newline.
    while let (Some(name), Some(_)) = (field(&mut rest, 0), field(&mut rest, 0)) {
        let line = field(&mut rest, b'\n').unwrap_or_else(|| mem::take(&mut rest));
        let name = name.strip_prefix(prefix.as_bytes()).unwrap_or(name);
        hits.push(GrepHit {
            path: path::from_bytes(name),
            text: String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(line)).into_owned(),
        });
    }
    Some(hits)
}

/// Take the bytes before the next `end` off the front of `bytes`, and `end` itself.
fn field<'a>(bytes: &mut &'a [u8], end: u8) -> Option<&'a [u8]> {
    let (field, rest) = bytes.split_at(bytes.iter().position(|byte| *byte == end)?);
    *bytes = &rest[1..];
    Some(field)
}

fn blob_text(mut stdin: ChildStdin, stdout: ChildStdout, id: &str) -> Option<Blob> {
    writeln!(stdin, "{id}").ok()?;
    drop(stdin);
    let mut out = BufReader::new(stdout);
    let mut header = String::new();
    out.read_line(&mut header).ok()?;
    // `<id> blob <size>`, or `<id> missing` or `<id> ambiguous`.
    let mut fields = header.split_whitespace().skip(1);
    let size: usize = (fields.next()? == "blob")
        .then(|| fields.next()?.parse().ok())
        .flatten()?;
    // A blob too big to read whole is read as far as a first line can go.
    let whole = size <= MAX_FILE_BYTES;
    let wanted = if whole {
        size
    } else {
        size.min(MAX_LINE_BYTES + 1)
    };
    let mut bytes = Vec::with_capacity(wanted);
    out.take(wanted as u64).read_to_end(&mut bytes).ok()?;
    if bytes.len() != wanted {
        return None;
    }
    let binary = bytes.iter().take(8000).any(|&byte| byte == 0);
    let line = bytes
        .split(|&byte| byte == b'\n')
        .next()
        .unwrap_or_default();
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    // A line cut short by the read is too long to be looked at anyway.
    let first = (!binary && line.len() <= MAX_LINE_BYTES)
        .then(|| String::from_utf8_lossy(line).into_owned());
    Some(Blob {
        text: whole.then(|| String::from_utf8_lossy(&bytes).into_owned()),
        first,
    })
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::render::starts::tests::Repo;

    fn read_all(repo: &Repo, specs: &[&str], budget: &mut usize) -> Vec<Option<String>> {
        let specs: Vec<String> = specs.iter().map(|spec| spec.to_string()).collect();
        read_blobs(&repo.0, &specs, budget, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn reading_blobs_gives_an_answer_for_every_spec() {
        let repo = Repo::new();
        repo.write("binary", "abc\0def");
        repo.commit("a.rs", "first\n");
        let got = read_all(
            &repo,
            &[
                "HEAD:a.rs",
                "HEAD:gone.rs",
                "HEAD:binary",
                "HEAD",
                "HEAD:a.rs",
            ],
            &mut 100_000,
        );
        let text = Some("first\n".to_string());
        assert_eq!(got, [text.clone(), None, None, None, text]);
    }

    #[test]
    fn reading_blobs_stops_at_the_budget() {
        let repo = Repo::new();
        repo.write("b.rs", "second\n");
        repo.commit("a.rs", "first\n");
        let mut budget = 10;
        let got = read_all(&repo, &["HEAD:a.rs", "HEAD:b.rs", "HEAD:a.rs"], &mut budget);
        assert_eq!(got, [Some("first\n".to_string()), None, None]);
        assert_eq!(budget, 4);
    }

    #[test]
    fn reading_blobs_skips_one_over_the_size_limit() {
        let repo = Repo::new();
        repo.write("big.rs", &"x".repeat(MAX_FILE_BYTES + 1));
        repo.commit("a.rs", "first\n");
        let got = read_all(&repo, &["HEAD:big.rs", "HEAD:a.rs"], &mut (64 << 20));
        assert_eq!(got, [None, Some("first\n".to_string())]);
    }

    #[test]
    fn a_grep_out_of_time_keeps_what_it_has_and_a_cancelled_one_nothing() {
        let repo = Repo::new();
        repo.commit("a.rs", "needle\n");
        let tree = Tree::Rev("HEAD".to_string());
        let patterns = ["needle".to_string()];
        let grep = |limits, cancelled: bool| {
            grep(
                &repo.0,
                &tree,
                &patterns,
                &[],
                limits,
                &AtomicBool::new(cancelled),
            )
        };
        let hits = grep((Duration::from_secs(30), 1 << 20), false).unwrap();
        assert_eq!(hits[0].path, "a.rs");
        assert_eq!(hits[0].text, "needle");
        assert!(grep((Duration::ZERO, 1 << 20), false).is_some());
        assert!(grep((Duration::from_secs(30), 1 << 20), true).is_none());
    }

    #[test]
    fn a_cancelled_job_stops_a_git_that_has_gone_quiet() {
        let mut git = Command::new("sleep").arg("30").spawn().unwrap();
        let cancelled = AtomicBool::new(true);
        watch_git(&mut git, &cancelled, || false);
        assert!(!git.wait().unwrap().success(), "killed, not left to finish");
    }
}
