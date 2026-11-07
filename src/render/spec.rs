//! One render, run on a job: what it's made from and what it hands back.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use ratatui::text::{Line, Span, Text};

use super::cache::Key;
use super::copies::CopyIndex;
use super::highlight::{Parsing, Run};
use super::layout::Content;
use super::moves::MoveIndex;
use super::rendered::{Rendered, splice_and_measure};
use super::starts::StartReader;
#[cfg(test)]
use super::test_support;
use super::tokens::Tokens;
use super::words::Changes;
use super::{highlight, layout, starts, words};
use crate::comment::LineMap;
use crate::comment::store::{Anchor, Comment};
use crate::diff::{self, FileDiff, Row};

/// The comments to draw into one file's render.
///
/// Bundled rather than passed loose so [`RenderCache::ensure`] keeps a readable
/// signature, and so the borrow of the comment store stays separate from the
/// mutable borrow of the cache.
pub struct CommentLayer<'a> {
    /// Threads for this file, from `CommentStore::threads`.
    pub threads: Vec<(Anchor, Vec<&'a Comment>)>,
    /// The file's tree path, for labelling a thread whose anchor has vanished.
    pub file: &'a str,
    /// The name this window writes its own comments under, so a note the reader
    /// left is colored apart from one an agent did.
    pub me: &'a str,
    /// `review::file_hash` of the file's current diff, for staleness marking.
    pub diff_hash: u128,
    /// Bumped whenever the comment store changes. A cached render built at an
    /// older revision is re-spliced, not rendered again, before it's handed out.
    pub rev: u64,
}

impl CommentLayer<'_> {
    /// A layer with nothing in it, for callers with comments turned off.
    pub fn none() -> Self {
        Self {
            threads: Vec::new(),
            file: "",
            me: "",
            diff_hash: 0,
            rev: 0,
        }
    }

    /// A copy for a job to take with it.
    pub(super) fn owned(&self) -> Comments {
        let thread = |(anchor, comments): &(Anchor, Vec<&Comment>)| {
            (*anchor, comments.iter().map(|&c| c.clone()).collect())
        };
        Comments {
            threads: self.threads.iter().map(thread).collect(),
            file: self.file.to_string(),
            me: self.me.to_string(),
            diff_hash: self.diff_hash,
            rev: self.rev,
        }
    }
}

/// A [`CommentLayer`] that owns what's in it, for a render thread.
pub(super) struct Comments {
    threads: Vec<(Anchor, Vec<Comment>)>,
    file: String,
    me: String,
    diff_hash: u128,
    rev: u64,
}

impl Comments {
    fn layer(&self) -> CommentLayer<'_> {
        CommentLayer {
            threads: self
                .threads
                .iter()
                .map(|(anchor, comments)| (*anchor, comments.iter().collect()))
                .collect(),
            file: &self.file,
            me: &self.me,
            diff_hash: self.diff_hash,
            rev: self.rev,
        }
    }
}

/// A file's lines before any comments go in.
pub(super) struct Base {
    pub(super) text: Text<'static>,
    pub(super) map: LineMap,
}

/// A render as a job hands it back.
pub(super) struct Made {
    /// The lines from before the comments, if any went in. Without any they're the
    /// render's own, and keeping a second copy would double what a file takes up.
    pub(super) base: Option<Arc<Base>>,
    pub(super) rendered: Rendered,
    /// Whether any comments went in.
    pub(super) commented: bool,
    /// The comment revision it was spliced at.
    pub(super) rev: u64,
    pub(super) bytes: usize,
}

/// Put `comments` into a file's lines and measure the result.
fn made(
    text: Text<'static>,
    map: LineMap,
    comments: &Comments,
    key: &Key,
    keep_base: bool,
    cancelled: &AtomicBool,
) -> Made {
    let layer = comments.layer();
    let commented = !layer.threads.is_empty();
    let base = (commented && keep_base).then(|| {
        Arc::new(Base {
            text: text.clone(),
            map: map.clone(),
        })
    });
    let rendered = splice_and_measure(
        text,
        map,
        &layer,
        key.width,
        key.side_by_side,
        key.theme,
        cancelled,
    );
    let bytes = text_bytes(&rendered.text) + base.as_ref().map_or(0, |b| text_bytes(&b.text));
    Made {
        base,
        rendered,
        commented,
        rev: comments.rev,
        bytes,
    }
}

/// Roughly what `text` takes up. The line map and offsets are put at a flat rate.
fn text_bytes(text: &Text<'static>) -> usize {
    const ALLOCATION: usize = 16;
    let span = |s: &Span| size_of::<Span>() + s.content.len() + ALLOCATION;
    let line = |l: &Line| size_of::<Line>() + 64 + l.spans.iter().map(span).sum::<usize>();
    text.lines.iter().map(line).sum()
}

/// What came of a job.
pub(super) enum Outcome {
    /// The file in its final shape without its syntax color, with the real render
    /// still to come.
    Preview(Made),
    /// The render of the file.
    Final(Made),
    /// New comments in a render that already exists.
    Spliced(Made),
    /// Putting new comments in failed, so the file keeps the ones it has.
    NotSpliced(u64),
}

pub(super) struct Done {
    pub(super) id: u64,
    pub(super) outcome: Outcome,
    /// What went wrong, for a job that panicked.
    pub(super) failure: Option<String>,
}

/// A job's way back to the cache.
pub(super) struct Ticket {
    pub(super) id: u64,
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) results: Sender<Done>,
}

impl Ticket {
    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn send(&self, outcome: Outcome, failure: Option<String>) {
        let id = self.id;
        // Nobody listening means the cache has gone, and the app with it.
        let _ = self.results.send(Done {
            id,
            outcome,
            failure,
        });
    }
}

/// What a panic said.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    let text = panic.downcast_ref::<&str>().copied();
    let text = text.or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    text.unwrap_or("unknown panic").to_string()
}

/// Everything a render needs, owned, so nothing can change underneath it.
pub(super) struct Spec {
    pub(super) key: Key,
    pub(super) diff: FileDiff,
    pub(super) moves: Arc<MoveIndex>,
    pub(super) copies: Arc<CopyIndex>,
    pub(super) tab_width: u8,
    pub(super) comments: Comments,
    pub(super) reader: Option<Arc<StartReader>>,
    #[cfg(test)]
    pub(super) test: test_support::Hooks,
}

impl Spec {
    /// Render the file. Something always lands, however this goes, as a jump can
    /// be waiting on it: a panic is met with the text without its colors, or failing
    /// that a line saying so.
    pub(super) fn run(&self, ticket: &Ticket) {
        let render = AssertUnwindSafe(|| self.render(ticket));
        let Err(panic) = panic::catch_unwind(render) else {
            return;
        };
        let plain = AssertUnwindSafe(|| self.plain(&diff::rows(&self.diff), &ticket.cancelled));
        let made = panic::catch_unwind(plain).unwrap_or_else(|_| self.unrenderable());
        let failure = format!("{}: {}", self.diff.path(), panic_message(&*panic));
        ticket.send(Outcome::Final(made), Some(failure));
    }

    fn render(&self, ticket: &Ticket) {
        #[cfg(test)]
        self.test.at_start(self, &ticket.cancelled);
        let rows = diff::rows(&self.diff);
        // A file that won't be colored is as cheap to finish as to preview. One
        // whose old blob may name its language could be either.
        let might_color = highlight::language(&self.diff, &rows).is_some()
            || self.reader.is_some() && starts::reads_language(&self.diff, &rows);
        if might_color {
            let plain = self.plain(&rows, &ticket.cancelled);
            if ticket.stopped() {
                return;
            }
            ticket.send(Outcome::Preview(plain), None);
        }
        #[cfg(test)]
        self.test.before_final(&ticket.cancelled);

        let parsing = match &self.reader {
            Some(reader) => match reader.read(&self.diff, &ticket.cancelled) {
                Some(parsing) => parsing,
                None => return,
            },
            None => Arc::new(Parsing::of(&self.diff, &rows)),
        };
        let made = self.colored(&rows, &parsing, &ticket.cancelled);
        if !ticket.stopped() {
            ticket.send(Outcome::Final(made), None);
        }
    }

    pub(super) fn colored(&self, rows: &[Row], parsing: &Parsing, cancelled: &AtomicBool) -> Made {
        let theme = self.key.theme;
        let syntax = highlight::highlight(rows, theme, parsing, cancelled);
        let moves = self.moves.file(self.key.file);
        let changes = words::changes(rows, &syntax.tokens, moves, cancelled);
        self.lay_out(rows, &syntax.runs, &changes, cancelled)
    }

    /// The file without parsing it, which is most of the work saved. Nothing the
    /// parser finds changes a character or moves a line, so this is the final render
    /// but for its styles, and whatever points into one is good for the other.
    pub(super) fn plain(&self, rows: &[Row], cancelled: &AtomicBool) -> Made {
        #[cfg(test)]
        self.test.at_plain(self);
        let moves = self.moves.file(self.key.file);
        let tokens = vec![Tokens::default(); rows.len()];
        let changes = words::changes(rows, &tokens, moves, cancelled);
        self.lay_out(rows, &vec![Vec::new(); rows.len()], &changes, cancelled)
    }

    fn lay_out(
        &self,
        rows: &[Row],
        syntax: &[Vec<Run>],
        changes: &Changes,
        cancelled: &AtomicBool,
    ) -> Made {
        let Key {
            file,
            width,
            side_by_side,
            theme,
        } = self.key;
        let content = Content {
            rows,
            syntax,
            changes,
            moves: self.moves.file(file),
            copies: self.copies.file(file),
            cancelled,
        };
        let lay_out = if side_by_side {
            layout::side_by_side
        } else {
            layout::unified
        };
        let (text, map) = lay_out(&self.diff, &content, width, theme, self.tab_width);
        let map = map.with_changes(diff::change_ids(rows));
        made(text, map, &self.comments, &self.key, true, cancelled)
    }

    fn unrenderable(&self) -> Made {
        let text = Text::from(format!("  Couldn't render {}", self.diff.path()));
        let none = Comments {
            threads: Vec::new(),
            file: String::new(),
            me: String::new(),
            diff_hash: 0,
            rev: self.comments.rev,
        };
        let cancelled = AtomicBool::new(false);
        made(
            text,
            LineMap::default(),
            &none,
            &self.key,
            false,
            &cancelled,
        )
    }
}

/// What a re-splice starts from: the lines from before the comments, which are the
/// render's own when it has none.
pub(super) enum Origin {
    Base(Arc<Base>),
    Shown(Arc<Rendered>),
}

/// Put `comments` into a render that already exists.
pub(super) fn splice_again(origin: Origin, comments: &Comments, key: &Key, ticket: &Ticket) {
    let splice = AssertUnwindSafe(|| {
        let (text, map, keep_base) = match &origin {
            Origin::Base(base) => (base.text.clone(), base.map.clone(), false),
            Origin::Shown(render) => (render.text.clone(), render.line_map.clone(), true),
        };
        made(text, map, comments, key, keep_base, &ticket.cancelled)
    });
    match panic::catch_unwind(splice) {
        Ok(made) if !ticket.stopped() => ticket.send(Outcome::Spliced(made), None),
        Ok(_) => {}
        Err(panic) => {
            let failure = format!("{}: {}", comments.file, panic_message(&*panic));
            ticket.send(Outcome::NotSpliced(comments.rev), Some(failure));
        }
    }
}
