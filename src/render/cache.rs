//! The renders kept for the UI thread, and the jobs making more.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(test)]
use ratatui::text::Text;
use twox_hash::XxHash3_128;

use super::copies::{CopyIndex, Git};
use super::jobs::Jobs;
use super::jump::Jump;
use super::moves::MoveIndex;
use super::rendered::Rendered;
#[cfg(test)]
use super::rendered::splice_and_measure;
use super::spec::{Base, CommentLayer, Done, Made, Origin, Outcome, Spec, Ticket, splice_again};
use super::starts::StartReader;
#[cfg(test)]
use super::test_support::{self, Hooks};
use super::{WAIT, copies};
use crate::autodiff::Tree;
#[cfg(test)]
use crate::comment::LineMap;
use crate::comment::store::Anchor;
use crate::diff::FileDiff;
use crate::theme::DiffTheme;

/// Roughly what the cached renders may take up. Going over drops the ones looked at
/// longest ago, and the file on screen stays whatever its size.
const MAX_CACHE_BYTES: usize = 256 << 20;

/// A render bigger than this is left to a render thread to free.
const BIG_BYTES: usize = 1 << 20;

/// How long quitting gives the render threads to stop. They check often, so it
/// takes something stuck to use it up.
const STOP_GRACE: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub file: usize,
    pub width: u16,
    pub side_by_side: bool,
    pub theme: DiffTheme,
}

/// One cached file render.
struct Entry {
    /// The lines to re-splice from, when they aren't the render's own. Keeping them
    /// means a comment added, edited, or deleted only re-splices — an `O(lines)`
    /// pass — instead of highlighting the file again. That matters because the
    /// filesystem watcher can fire repeatedly while an agent writes a batch of notes.
    base: Option<Arc<Base>>,
    rendered: Arc<Rendered>,
    /// The comment revision `rendered` was spliced at.
    rev: u64,
    /// Whether `rendered` has comments in it.
    commented: bool,
    /// Not the last word on this file, until the real render takes its place: it's
    /// without its syntax color, or from before the copies were known.
    preview: bool,
    /// The [`RenderCache::copies_rev`] it was made from.
    copies_rev: u64,
    /// Roughly what this takes up.
    bytes: usize,
    /// When it was last on screen, as a count of the cache's own.
    used: u64,
    /// What went wrong making it, until that's been reported. It waits for the file
    /// to be on screen: said during a prefetch, it would be about nothing in view.
    failure: Option<String>,
}

/// Which queue a job goes on: a prefetch is idle, and anything for the screen
/// comes before it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Priority {
    Urgent,
    Idle,
}

/// A job that's wanted. One that isn't in the list has had its result given up on.
struct Running {
    id: u64,
    key: Key,
    cancelled: Arc<AtomicBool>,
    priority: Priority,
    started: Instant,
    /// The [`RenderCache::copies_rev`] it's working from.
    copies_rev: u64,
}

/// The search for copied code, running on a thread of its own as it can take
/// seconds, which would keep a render thread from the file on screen.
struct CopyJob {
    cancelled: Arc<AtomicBool>,
    thread: JoinHandle<Option<CopyIndex>>,
}

impl CopyJob {
    fn start(
        files: Vec<FileDiff>,
        moves: Option<Arc<MoveIndex>>,
        git: Option<Git>,
    ) -> Option<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let search = move || {
            // A block that's a move is a move, whether moves are being shown or not.
            let moves = moves.unwrap_or_else(|| Arc::new(MoveIndex::build(&files)));
            copies::build(&files, &moves, git.as_ref(), &stop)
        };
        // The panic hook goes by the thread, so it can't take the app down.
        let thread = thread::Builder::new()
            .name("render-copies".to_string())
            .spawn(search)
            .ok()?;
        Some(Self { cancelled, thread })
    }
}

/// How a prefetch stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prefetch {
    /// The file is as ready as a prefetch makes it.
    Ready,
    /// It's being made.
    Started,
    /// It isn't being made: something else is, or it was made and didn't fit.
    Stopped,
}

/// What the file on screen is missing.
enum Need {
    Nothing,
    Render,
    Splice(Origin),
}

/// The file a render is of, and how it's to look: `ensure` and `prefetch` ask the
/// same thing about different files.
pub struct Request<'a> {
    pub file: usize,
    pub diff: &'a FileDiff,
    pub width: u16,
    pub side_by_side: bool,
    pub theme: DiffTheme,
    pub comments: CommentLayer<'a>,
}

impl Request<'_> {
    fn key(&self) -> Key {
        Key {
            file: self.file,
            width: self.width,
            side_by_side: self.side_by_side,
            theme: self.theme,
        }
    }
}

/// Caches renders keyed by `(file, width, side_by_side, theme)` so revisiting a
/// file — or redrawing at the same size and theme — never renders it again.
/// Switching themes re-renders (and caches separately), so toggling back is
/// instant. What's kept is bounded by [`MAX_CACHE_BYTES`].
pub struct RenderCache {
    entries: HashMap<Key, Entry>,
    tab_width: u8,
    detect_moves: bool,
    moves: Arc<MoveIndex>,
    detect_copies: bool,
    /// What's known so far of what was copied, which starts with nothing and is
    /// replaced when [`Self::copy_job`] is done. Width and theme don't come into it,
    /// so a new one of either only costs its render.
    copies: Arc<CopyIndex>,
    /// Counts the times [`Self::copies`] has changed what a file looks like.
    copies_rev: u64,
    /// The hash of the diff the copies are for.
    copies_of: u128,
    /// Whether [`Self::start_copies`] has anything to do.
    copies_wanted: bool,
    copy_job: Option<CopyJob>,
    /// Jobs given up on, kept so quitting can wait for their git to be stopped.
    copy_jobs_stopping: Vec<JoinHandle<Option<CopyIndex>>>,
    /// The render on screen before a copy search changed it, to keep the view on the
    /// same code. See [`Self::kept_scroll`].
    replaced: Option<(Key, Arc<Rendered>)>,
    /// Where git runs, when riffnav ran it for the diff.
    repo: Option<PathBuf>,
    /// Set when riffnav ran git itself, so there's a file behind the diff to read.
    /// Outlives [`Self::clear`]: a new width has the same hunks.
    starts: Option<Arc<StartReader>>,
    jobs: Jobs,
    running: Vec<Running>,
    next_id: u64,
    sender: Sender<Done>,
    results: Receiver<Done>,
    /// How long to wait for a render, from when it was asked for.
    wait: Duration,
    /// What the entries take up, going by their own counts.
    bytes: usize,
    /// Counts up each time a file is asked for, for [`Entry::used`].
    clock: u64,
    /// The file on screen, which isn't dropped however big it is.
    shown: Option<Key>,
    /// The file on screen before it, which a prefetch doesn't push out either.
    left: Option<Key>,
    /// The files prefetched since they were last on screen, so one too big to stay
    /// in the cache isn't made over and over.
    prefetched: HashSet<Key>,
    /// Whether to prefetch at all.
    prefetching: bool,
    #[cfg(test)]
    test: test_support::Hooks,
}

impl RenderCache {
    /// Columns between tab stops, which is what search counts tabs in.
    pub fn tab_width(&self) -> usize {
        usize::from(self.tab_width)
    }

    pub fn new(tab_width: u8, detect_moves: bool, detect_copies: bool) -> Self {
        let (sender, results) = mpsc::channel();
        Self {
            entries: HashMap::new(),
            tab_width,
            detect_moves,
            moves: Arc::default(),
            detect_copies,
            copies: Arc::default(),
            copies_rev: 0,
            copies_of: 0,
            copies_wanted: false,
            copy_job: None,
            copy_jobs_stopping: Vec::new(),
            replaced: None,
            repo: None,
            starts: None,
            jobs: Jobs::default(),
            running: Vec::new(),
            next_id: 0,
            sender,
            results,
            wait: WAIT,
            bytes: 0,
            clock: 0,
            shown: None,
            left: None,
            prefetched: HashSet::new(),
            // Tests want what they pressed for, with nothing else going on.
            prefetching: !cfg!(test),
            #[cfg(test)]
            test: test_support::Hooks::default(),
        }
    }

    /// Start each hunk's parser from the real file. Only for a diff from git run
    /// here: one piped in could be of anything.
    pub fn read_starts(&mut self) {
        self.starts
            .get_or_insert_with(|| Arc::new(StartReader::new(".")));
        self.repo.get_or_insert_with(|| PathBuf::from("."));
    }

    /// Set how long a render is waited for, from when it was asked for.
    pub fn wait(&mut self, wait: Duration) {
        self.wait = wait;
    }

    /// How long a frame may wait for renders.
    pub fn waits(&self) -> Duration {
        self.wait
    }

    /// Whether anything is being rendered.
    pub fn busy(&self) -> bool {
        !self.running.is_empty()
    }

    /// Whether anything is on its way, so the event loop has to keep waking up to
    /// pick it up. The search for copies counts here and not in [`Self::busy`], as a
    /// prefetch has no reason to wait for it.
    pub fn expecting(&self) -> bool {
        self.busy() || self.copy_job.is_some()
    }

    /// What went wrong rendering the file on screen, the first time it's asked.
    pub fn take_failure(&mut self) -> Option<String> {
        self.entries.get_mut(&self.shown?)?.failure.take()
    }

    /// Take in whatever has finished. Whether anything did.
    pub fn collect(&mut self) -> bool {
        let mut any = self.collect_copies();
        while let Ok(done) = self.results.try_recv() {
            any |= self.land(done);
        }
        any
    }

    /// Take in a finished job, if it's still wanted. Whether it was.
    fn land(&mut self, done: Done) -> bool {
        let Some(at) = self.running.iter().position(|job| job.id == done.id) else {
            self.jobs.discard(done);
            return false;
        };
        let job = &self.running[at];
        let (key, copies_rev) = (job.key, job.copies_rev);
        let idle = job.priority == Priority::Idle;
        // A prefetch keeps what it makes only where there's room for it.
        let spare = if idle { self.left } else { None };
        let roomy = |cache: &Self, made: &Made| !idle || cache.room_for(key, made);
        match done.outcome {
            Outcome::Preview(made) => {
                // A file only gets one, and it never replaces the real thing.
                if self.entries.contains_key(&key) || !roomy(self, &made) {
                    self.jobs.discard(made);
                } else {
                    self.keep(key, made, true, copies_rev, spare);
                }
                return true;
            }
            Outcome::Final(made) if !roomy(self, &made) => {
                self.jobs.discard(made);
                self.prefetched.insert(key);
                // The preview that made it in has nothing to follow it.
                if self.entries.get(&key).is_some_and(|entry| entry.preview) {
                    self.remove(|other| *other == key);
                }
            }
            Outcome::Final(made) => {
                self.keep(key, made, false, copies_rev, spare);
                if idle {
                    self.prefetched.insert(key);
                }
            }
            Outcome::Spliced(mut made) => {
                // Dropped to make room while this was on its way, and without what
                // to splice from next time there's no keeping it.
                if let Some(entry) = self.entries.get(&key) {
                    if made.commented && made.base.is_none() {
                        made.base = entry.base.clone();
                    }
                    let copies_rev = entry.copies_rev;
                    self.keep(key, made, false, copies_rev, None);
                }
            }
            Outcome::NotSpliced(rev) => {
                if let Some(entry) = self.entries.get_mut(&key) {
                    entry.rev = rev;
                }
            }
        }
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.failure = entry.failure.take().or(done.failure);
        }
        self.running.remove(at);
        true
    }

    /// Whether a prefetch of `key` can keep `made`, which can push out what was
    /// looked at a while ago, but not the file on screen or the one before it.
    fn room_for(&self, key: Key, made: &Made) -> bool {
        let size = |key: Key| self.entries.get(&key).map_or(0, |entry| entry.bytes);
        let kept = [self.shown, self.left].into_iter().flatten();
        let kept: usize = kept.filter(|&other| other != key).map(size).sum();
        kept + made.bytes <= self.limit()
    }

    /// Put `made` in the cache, and make room for it without dropping `spare`.
    fn keep(&mut self, key: Key, made: Made, preview: bool, copies_rev: u64, spare: Option<Key>) {
        let entry = Entry {
            base: made.base,
            rendered: Arc::new(made.rendered),
            rev: made.rev,
            commented: made.commented,
            preview,
            copies_rev,
            bytes: made.bytes,
            used: self.clock,
            failure: None,
        };
        self.bytes += entry.bytes;
        if let Some(old) = self.entries.insert(key, entry) {
            self.bytes -= old.bytes;
            if old.copies_rev != copies_rev && self.shown == Some(key) {
                self.replaced = Some((key, old.rendered.clone()));
            }
            self.retire(vec![old]);
        }
        self.trim(spare);
    }

    /// Drop the renders looked at longest ago until what's left fits, keeping the
    /// file on screen and `spare`.
    fn trim(&mut self, spare: Option<Key>) {
        let mut doomed = Vec::new();
        while self.bytes > self.limit() {
            let kept = [self.shown, spare];
            let oldest = self
                .entries
                .iter()
                .filter(|(key, _)| !kept.contains(&Some(**key)));
            let Some(key) = oldest
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.bytes;
                doomed.push(entry);
            }
        }
        self.retire(doomed);
    }

    fn limit(&self) -> usize {
        #[cfg(test)]
        if let Some(limit) = self.test.limit {
            return limit;
        }
        MAX_CACHE_BYTES
    }

    /// Free `entries`. A big render takes long enough that it's done elsewhere.
    fn retire(&mut self, entries: Vec<Entry>) {
        if entries.iter().any(|entry| entry.bytes > BIG_BYTES) {
            self.jobs.discard(entries);
        }
    }

    /// Give up on the jobs `doomed` picks. What they've done so far is lost, and
    /// whatever they still hand in is dropped.
    fn cancel(&mut self, doomed: impl Fn(&Running) -> bool) {
        self.running.retain(|job| {
            let dropped = doomed(job);
            if dropped {
                job.cancelled.store(true, Ordering::Relaxed);
            }
            !dropped
        });
    }

    /// Stop everything, for quitting.
    pub fn stop(&mut self) {
        let deadline = Instant::now() + STOP_GRACE;
        self.cancel(|_| true);
        self.cancel_copies();
        self.jobs.stop(STOP_GRACE);
        // Each stops its git before it ends, so that's waited for like the renders.
        while self.copy_jobs_stopping.iter().any(|job| !job.is_finished())
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Give up on what's being rendered for the screen, which has nothing to show.
    pub fn release(&mut self) {
        self.left = self.shown.take().or(self.left);
        self.cancel(|job| job.priority == Priority::Urgent);
    }

    /// Work out what moved across `files`, and drop the render of any file that
    /// shows it differently now. That can be a file other than the one that
    /// changed: a block moved out of it may no longer have anywhere to have gone.
    pub fn index_moves(&mut self, files: &[FileDiff]) {
        self.forget_copies(files);
        if !self.detect_moves {
            return;
        }
        let moves = MoveIndex::build(files);
        let old = self.moves.clone();
        self.remove(|key| old.file(key.file) != moves.file(key.file));
        self.moves = Arc::new(moves);
        self.cancel(|_| true); // They're working from the old moves.
    }

    /// Drop what's known of the copies in another diff, as its rows are no good for
    /// `files`. The same diff again, as `r` often brings, keeps it all.
    fn forget_copies(&mut self, files: &[FileDiff]) {
        if !self.detect_copies {
            return;
        }
        let diff = files.iter().fold(0_u128, |hash, file| {
            let file = XxHash3_128::oneshot(file.raw.as_bytes());
            hash.wrapping_mul(31).wrapping_add(file)
        });
        if diff != self.copies_of {
            self.copies_of = diff;
            self.copies_wanted = true;
            self.cancel_copies();
            self.take_copies(CopyIndex::default());
        }
    }

    /// Whether there's a new diff to look for copies in.
    pub fn wants_copies(&self) -> bool {
        self.copies_wanted
    }

    /// Start looking for copies in `files`. Not before the first frame has been
    /// drawn, which this must never hold up. `tree` is where the files they leave
    /// alone can be read.
    pub fn start_copies(&mut self, files: &[FileDiff], tree: Option<Tree>) {
        self.copies_wanted = false;
        if !copies::worth_trying(files) {
            return;
        }
        let moves = self.detect_moves.then(|| self.moves.clone());
        let git = self.repo.clone().map(|repo| Git { repo, tree });
        self.copy_job = CopyJob::start(files.to_vec(), moves, git);
    }

    fn cancel_copies(&mut self) {
        self.copy_jobs_stopping.retain(|job| !job.is_finished());
        if let Some(job) = self.copy_job.take() {
            job.cancelled.store(true, Ordering::Relaxed);
            self.copy_jobs_stopping.push(job.thread);
        }
    }

    /// Take in the copies if they're ready. Whether that changed anything.
    fn collect_copies(&mut self) -> bool {
        if self
            .copy_job
            .as_ref()
            .is_none_or(|job| !job.thread.is_finished())
        {
            return false;
        }
        let found = self.copy_job.take().and_then(|job| job.thread.join().ok());
        match found.flatten() {
            Some(index) => self.take_copies(index),
            None => false,
        }
    }

    /// Show `index` from now on. A file it changes keeps its render on screen until
    /// the new one is ready, so the note arrives without the file going blank.
    /// Whether it changed anything.
    fn take_copies(&mut self, index: CopyIndex) -> bool {
        let old = self.copies.clone();
        let changed = |file: usize| !old.file(file).looks_like(index.file(file));
        let any = self.entries.keys().any(|key| changed(key.file))
            || self.running.iter().any(|job| changed(job.key.file));
        self.copies_rev += 1;
        self.cancel(|job| changed(job.key.file));
        for (key, entry) in &mut self.entries {
            entry.preview |= changed(key.file);
        }
        if any {
            self.forget_prefetches();
        }
        self.copies = Arc::new(index);
        any
    }

    /// The other end of the move or copy `anchor` in `file` is part of, if any.
    pub fn jump(&self, file: usize, anchor: Anchor) -> Option<Jump> {
        let moved = self.moves.file(file).target(anchor).map(Jump::Moved);
        moved.or_else(|| self.copies.file(file).jump(anchor).cloned())
    }

    /// The scroll that keeps the view on the same code, if the render of what's on
    /// screen has just had its copies' notes put in.
    pub fn kept_scroll(&mut self, wanted: Key, scroll: u32) -> Option<u32> {
        let (key, old) = self.replaced.take()?;
        let kept =
            (key == wanted).then(|| self.entries.get(&key)?.rendered.scroll_like(&old, scroll));
        self.jobs.discard(old);
        kept.flatten()
    }

    /// Get the file on screen under way, and wait for it for up to [`Self::wait`]
    /// from when it was first asked for, so an ordinary file is here by the time
    /// the frame is drawn. What it's still missing after that lands later, and
    /// until then [`Self::get`] has whatever there is of it so far.
    pub fn ensure(&mut self, request: &Request<'_>) {
        let key = request.key();
        if self.shown != Some(key) {
            self.left = self.shown;
        }
        self.shown = Some(key);
        // It's an ordinary file now, which can be prefetched again once it's gone.
        self.prefetched.remove(&key);
        self.clock += 1;
        self.collect();
        self.cancel(|job| job.priority == Priority::Urgent && job.key != key);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.used = self.clock;
        }
        loop {
            match self.need(&key, request) {
                Need::Nothing => return,
                Need::Render => self.start_render(key, request, Priority::Urgent),
                Need::Splice(origin) => self.start_splice(key, origin, request),
            }
            if !self.wait_for(&key) {
                return;
            }
        }
    }

    /// What `key` is missing, given what's already being done about it.
    fn need(&mut self, key: &Key, request: &Request<'_>) -> Need {
        let comments = &request.comments;
        let need = match self.entries.get_mut(key) {
            None => Need::Render,
            Some(entry) if entry.preview => Need::Render,
            Some(entry) if entry.rev == comments.rev => Need::Nothing,
            // The revision is the whole store's, so it usually moves for a comment on
            // some other file.
            Some(entry) if !entry.commented && comments.threads.is_empty() => {
                entry.rev = comments.rev;
                Need::Nothing
            }
            Some(entry) => Need::Splice(match &entry.base {
                Some(base) => Origin::Base(base.clone()),
                None => Origin::Shown(entry.rendered.clone()),
            }),
        };
        if !matches!(need, Need::Nothing) {
            // The screen comes first, unless it's after the prefetch's own file.
            self.cancel(|job| job.priority == Priority::Idle && job.key != *key);
        }
        need
    }

    fn start_render(&mut self, key: Key, request: &Request<'_>, priority: Priority) {
        if let Some(job) = self.running.iter_mut().find(|job| job.key == key) {
            // Already on it, which a prefetch may be. It's urgent now.
            if job.priority == Priority::Idle && priority == Priority::Urgent {
                job.priority = Priority::Urgent;
                job.started = Instant::now();
            }
            return;
        }
        let spec = Spec {
            key,
            diff: request.diff.clone(),
            moves: self.moves.clone(),
            copies: self.copies.clone(),
            tab_width: self.tab_width,
            comments: request.comments.owned(),
            reader: self.starts.clone(),
            #[cfg(test)]
            test: self.test.clone(),
        };
        let ticket = self.ticket(key, priority);
        match priority {
            Priority::Urgent => self.jobs.urgent(move || spec.run(&ticket)),
            Priority::Idle => self.jobs.idle(move || spec.run(&ticket)),
        }
    }

    fn start_splice(&mut self, key: Key, origin: Origin, request: &Request<'_>) {
        if self.running.iter().any(|job| job.key == key) {
            return; // It lands at a revision that's looked at again then.
        }
        let comments = request.comments.owned();
        let ticket = self.ticket(key, Priority::Urgent);
        self.jobs
            .urgent(move || splice_again(origin, &comments, &key, &ticket));
    }

    fn ticket(&mut self, key: Key, priority: Priority) -> Ticket {
        let id = self.next_id;
        self.next_id += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.running.push(Running {
            id,
            key,
            cancelled: cancelled.clone(),
            priority,
            started: Instant::now(),
            copies_rev: self.copies_rev,
        });
        Ticket {
            id,
            cancelled,
            results: self.sender.clone(),
        }
    }

    /// Wait out what's left of [`Self::wait`] for `key`'s job. A preview doesn't end
    /// it, so a file that's nearly there is drawn once, in color. Whether the job
    /// finished.
    fn wait_for(&mut self, key: &Key) -> bool {
        while let Some(job) = self.running.iter().find(|job| job.key == *key) {
            let left = (job.started + self.wait).saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.results.recv_timeout(left) {
                Ok(done) => {
                    self.land(done);
                }
                Err(_) => return false,
            }
        }
        true
    }

    /// With the file on screen done, render one likely to be next. Anything for
    /// the screen stops it, and a prefetch for a file that has stopped being next
    /// goes the same way.
    pub fn prefetch(&mut self, request: &Request<'_>) -> Prefetch {
        if !self.prefetching {
            return Prefetch::Stopped;
        }
        let key = request.key();
        self.cancel(|job| job.priority == Priority::Idle && job.key != key);
        if self.entries.get(&key).is_some_and(|entry| !entry.preview) {
            Prefetch::Ready
        } else if self.running.iter().any(|job| job.key == key) {
            Prefetch::Started
        } else if self.prefetched.contains(&key) || self.busy() {
            Prefetch::Stopped
        } else {
            self.start_render(key, request, Priority::Idle);
            Prefetch::Started
        }
    }

    pub fn get(&self, key: Key) -> Option<&Rendered> {
        self.entry(key).map(|e| e.rendered.as_ref())
    }

    /// The copies behind what [`Self::get`] returns, which it can trail while its
    /// new render is being made. Its lines change when these do.
    pub fn copies_rev(&self, key: Key) -> Option<u64> {
        self.entry(key).map(|e| e.copies_rev)
    }

    /// The comment revision behind what [`Self::get`] returns. The render on show
    /// can trail the store's while the new one is being spliced.
    pub fn rev(&self, key: Key) -> Option<u64> {
        self.entry(key).map(|e| e.rev)
    }

    fn entry(&self, key: Key) -> Option<&Entry> {
        self.entries.get(&key)
    }

    /// Drop the renders `doomed` picks.
    fn remove(&mut self, doomed: impl Fn(&Key) -> bool) {
        let keys: Vec<Key> = self
            .entries
            .keys()
            .filter(|&key| doomed(key))
            .copied()
            .collect();
        let mut entries = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.bytes;
                entries.push(entry);
            }
        }
        self.retire(entries);
    }

    /// Drop all cached renders (e.g. after a resize changes the wrap width), along
    /// with any on the way, which were made for what's being replaced.
    pub fn clear(&mut self) {
        self.cancel(|_| true);
        self.remove(|_| true);
        self.forget_prefetches();
    }

    fn forget_prefetches(&mut self) {
        self.prefetched.clear();
    }

    /// Drop the cached renders for a single file (every width/theme variant),
    /// e.g. after its diff is refreshed in place. Other files stay cached.
    pub fn invalidate(&mut self, file: usize) {
        self.cancel(|job| job.key.file == file);
        self.remove(|key| key.file == file);
        self.forget_prefetches();
    }
}

#[cfg(test)]
impl RenderCache {
    pub fn hooks(&mut self) -> &mut Hooks {
        &mut self.test
    }

    pub fn prefetch_for_test(&mut self) {
        self.prefetching = true;
    }

    pub fn is_preview(&self, file: usize, width: u16, side_by_side: bool) -> bool {
        let theme = DiffTheme::GitHubDark;
        let entry = self.entry(Key {
            file,
            width,
            side_by_side,
            theme,
        });
        entry.is_some_and(|e| e.preview)
    }

    pub fn stored_bytes(&self) -> usize {
        self.bytes
    }

    /// Block until no job is left running.
    pub fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.busy() {
            assert!(Instant::now() < deadline, "a job never finished");
            if let Ok(done) = self.results.recv_timeout(Duration::from_millis(10)) {
                self.land(done);
            }
        }
    }

    fn read_starts_in(&mut self, repo: &std::path::Path) {
        self.starts = Some(Arc::new(StartReader::new(repo)));
        self.repo = Some(repo.to_path_buf());
    }

    /// Wait for the search for copies, for a test to say when it lands.
    pub(crate) fn wait_for_copies(&mut self) {
        while self.copy_job.is_some() {
            self.collect_copies();
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Say where `anchor` jumps to, for what a test can't get from a diff.
    pub(crate) fn set_jump_for_test(&mut self, file: usize, anchor: Anchor, jump: Jump) {
        let mut index = CopyIndex::default();
        index.set_jump_for_test(file, anchor, jump);
        self.copies = Arc::new(index);
    }

    /// Seed a render directly, for tests that want exact text rather than a diff.
    pub(crate) fn insert_for_test(
        &mut self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
        text: Text<'static>,
    ) {
        self.insert_for_test_with_comments(
            file,
            width,
            side_by_side,
            theme,
            text,
            &CommentLayer::none(),
        );
    }

    /// As [`Self::insert_for_test`], but splicing a comment layer in — the seam
    /// for snapshotting comment rendering.
    pub(crate) fn insert_for_test_with_comments(
        &mut self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
        text: Text<'static>,
        comments: &CommentLayer<'_>,
    ) {
        let map = LineMap::from_gutters(&text);
        let rendered = splice_and_measure(
            text.clone(),
            map.clone(),
            comments,
            width,
            side_by_side,
            theme,
            &AtomicBool::new(false),
        );
        self.entries.insert(
            Key {
                file,
                width,
                side_by_side,
                theme,
            },
            Entry {
                base: Some(Arc::new(Base { text, map })),
                rendered: Arc::new(rendered),
                rev: comments.rev,
                commented: true,
                preview: false,
                copies_rev: self.copies_rev,
                bytes: 0,
                used: 0,
                failure: None,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::testing;
    use crate::diff::testing::{hunk, one};
    use crate::testing::Repo;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use ratatui::text::Line;

    use crate::comment::store::{Anchor, Comment};
    use crate::diff::{self, FileDiff, parse};
    use crate::render::highlight::Parsing;
    use crate::render::moves::MoveIndex;
    use crate::render::rendered::Rendered;
    use crate::render::spec::{CommentLayer, Spec};
    use crate::render::test_support::{ALWAYS_PANICS, Gate, PANICS};
    use crate::theme::DiffTheme::{self, GitHubDark};

    const WIDTH: u16 = 60;

    fn removal() -> FileDiff {
        parse(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +0,0 @@\n\
             -fn alpha_value() {\n-    first_call(1);\n",
        )
        .remove(0)
    }

    fn addition(line: u32) -> FileDiff {
        let raw = format!(
            "diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -0,0 +{line},2 @@\n\
             +fn alpha_value() {{\n+    first_call(1);\n"
        );
        parse(&raw).remove(0)
    }

    fn unrelated() -> FileDiff {
        parse("diff --git a/c b/c\n--- a/c\n+++ b/c\n@@ -1 +1 @@\n-old\n+new\n").remove(0)
    }

    fn request(file: usize, diff: &FileDiff, width: u16, theme: DiffTheme) -> Request<'_> {
        Request {
            file,
            diff,
            width,
            side_by_side: false,
            theme,
            comments: CommentLayer::none(),
        }
    }

    fn rendered(cache: &mut RenderCache, files: &[FileDiff]) {
        for (at, file) in files.iter().enumerate() {
            cache.ensure(&request(at, file, WIDTH, GitHubDark));
        }
    }

    fn cached(cache: &RenderCache, file: usize) -> bool {
        cache.get(key(file, WIDTH, false, GitHubDark)).is_some()
    }

    fn says(cache: &RenderCache, file: usize, text: &str) -> bool {
        let rendered = cache.get(key(file, WIDTH, false, GitHubDark)).unwrap();
        rendered
            .text
            .lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.content.contains(text))
    }

    #[test]
    fn a_change_in_one_file_redraws_the_other_end_of_its_moves() {
        let mut files = [removal(), addition(1), unrelated()];
        let mut cache = RenderCache::new(4, true, false);
        cache.index_moves(&files);
        rendered(&mut cache, &files);
        assert!(says(&cache, 0, "moved to b:1"));

        files[1] = addition(7);
        cache.invalidate(1);
        cache.index_moves(&files);
        assert!(!cached(&cache, 0), "its hint points at the old line");
        assert!(cached(&cache, 2), "nothing about this file changed");

        rendered(&mut cache, &files);
        assert!(says(&cache, 0, "moved to b:7"));
    }

    #[test]
    fn moves_can_be_turned_off() {
        let files = [removal(), addition(1)];
        let mut cache = RenderCache::new(4, false, false);
        cache.index_moves(&files);
        rendered(&mut cache, &files);
        assert!(!says(&cache, 0, "moved to"));
    }

    /// A repository whose last commit changes a line inside a block comment, and
    /// that commit's diff.
    fn edit_in_a_comment(repo: &Repo) -> FileDiff {
        let text = format!("/*\n{}*/\n", "prose\n".repeat(40));
        edit_of(repo, "a.rs", &text)
    }

    /// `name` committed as `text` and then with its first `prose` made code.
    fn edit_of(repo: &Repo, name: &str, text: &str) -> FileDiff {
        repo.commit(name, text);
        repo.commit(name, &text.replacen("prose", "let x = 1;", 1));
        one(&String::from_utf8_lossy(
            &repo.run(&["show", "HEAD"]).stdout,
        ))
    }

    /// The colors of the `let x = 1;` row, which are all one if it's a comment.
    fn colors_of_the_edit(cache: &RenderCache, width: u16, theme: DiffTheme) -> usize {
        let rendered = cache.get(key(0, width, false, theme)).unwrap();
        let line = rendered.text.lines.iter().find(|line| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            text.contains("let x")
        });
        let mut colors: Vec<_> = line.unwrap().spans.iter().map(|s| s.style.fg).collect();
        colors.retain(|fg| fg.is_some());
        colors.dedup();
        colors.len()
    }

    #[test]
    fn hunk_starts_reach_the_render_and_outlive_a_resize_and_a_theme_change() {
        let repo = Repo::new();
        let file = edit_in_a_comment(&repo);
        let mut cache = RenderCache::new(4, false, false);
        cache.read_starts_in(&repo.0);
        let draw = |cache: &mut RenderCache, width, theme| {
            cache.ensure(&request(0, &file, width, theme));
        };

        draw(&mut cache, WIDTH, GitHubDark);
        let right = colors_of_the_edit(&cache, WIDTH, GitHubDark);
        assert_eq!(right, 3, "the gutter, one color of comment, the padding");

        // With the repository gone, the only way to stay right is the cache.
        cache.clear();
        draw(&mut cache, WIDTH + 10, GitHubDark);
        assert_eq!(colors_of_the_edit(&cache, WIDTH + 10, GitHubDark), right);
        draw(&mut cache, WIDTH, DiffTheme::GitHubLight);
        assert_eq!(
            colors_of_the_edit(&cache, WIDTH, DiffTheme::GitHubLight),
            right
        );
        assert!(!cache.busy());
    }

    #[test]
    fn a_script_whose_first_line_is_out_of_sight_is_colored_from_its_blob() {
        let repo = Repo::new();
        let text = format!(
            "#!/bin/sh\n{}",
            "echo hi\n".repeat(20) + &"prose\n".repeat(20)
        );
        let file = edit_of(&repo, "run", &text);
        let colors = |reader: bool| {
            let mut cache = RenderCache::new(4, false, false);
            if reader {
                cache.read_starts_in(&repo.0);
            }
            cache.ensure(&request(0, &file, WIDTH, GitHubDark));
            colors_of_the_edit(&cache, WIDTH, GitHubDark)
        };
        let piped = colors(false);
        assert!(colors(true) > piped, "colored only once the blob is read");
    }

    #[test]
    fn a_cache_without_a_reader_leaves_hunks_starting_afresh() {
        let repo = Repo::new();
        let file = edit_in_a_comment(&repo);
        let mut cache = RenderCache::new(4, false, false);
        cache.ensure(&request(0, &file, WIDTH, GitHubDark));
        assert!(!cache.busy());
        assert!(colors_of_the_edit(&cache, WIDTH, GitHubDark) > 1);
    }

    fn key(file: usize, width: u16, side_by_side: bool, theme: DiffTheme) -> Key {
        Key {
            file,
            width,
            side_by_side,
            theme,
        }
    }

    fn rust(path: &str, body: &str) -> FileDiff {
        one(&hunk(path, body))
    }

    fn file(path: &str) -> FileDiff {
        rust(path, " let a = 1;\n-let b = 2;\n+let b = 3;\n")
    }

    /// A cache that doesn't wait for anything, with its jobs held at the start.
    fn held() -> (RenderCache, Gate) {
        let mut cache = RenderCache::new(4, false, false);
        let gate = Gate::default();
        cache.wait(Duration::ZERO);
        cache.hooks().start = Some(gate.clone());
        (cache, gate)
    }

    fn started(cache: &mut RenderCache) -> Vec<usize> {
        cache.hooks().started.lock().unwrap().clone()
    }

    /// Wait until `files` jobs have got going.
    fn until_started(cache: &mut RenderCache, files: usize) {
        for _ in 0..10_000 {
            if started(cache).len() >= files {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the job never started");
    }

    fn note(file: &str, line: u32, body: &str) -> Comment {
        Comment {
            id: format!("id{line}"),
            author: "agent".to_string(),
            ..testing::note(file, line, body)
        }
    }

    fn with_comments<'a>(diff: &'a FileDiff, notes: &'a [Comment], rev: u64) -> Request<'a> {
        let anchor = |c: &Comment| Anchor {
            side: c.side,
            line: c.line,
        };
        Request {
            comments: CommentLayer {
                threads: notes.iter().map(|c| (anchor(c), vec![c])).collect(),
                file: diff.path(),
                me: "me",
                diff_hash: 0,
                rev,
            },
            ..request(0, diff, WIDTH, GitHubDark)
        }
    }

    fn all_text(cache: &RenderCache, file: usize) -> String {
        let render = cache.get(key(file, WIDTH, false, GitHubDark)).unwrap();
        let lines = render.text.lines.iter();
        lines
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn a_late_render_lands_on_its_own() {
        let (mut cache, gate) = held();
        let diff = file("a.rs");
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        assert!(!cached(&cache, 0));
        assert!(cache.busy());

        gate.open();
        cache.finish();
        assert!(cached(&cache, 0));
        assert!(!cache.is_preview(0, WIDTH, false));
        assert!(!cache.busy());
    }

    #[test]
    fn a_slow_file_shows_its_plain_text_until_the_real_render() {
        let mut cache = RenderCache::new(4, false, false);
        let gate = Gate::default();
        cache.wait(Duration::ZERO);
        cache.hooks().real = Some(gate.clone());
        let diff = file("a.rs");
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        while !cached(&cache, 0) {
            std::thread::sleep(Duration::from_millis(1));
            cache.collect();
        }
        assert!(cache.is_preview(0, WIDTH, false));
        let plain = all_text(&cache, 0);

        gate.open();
        cache.finish();
        assert!(!cache.is_preview(0, WIDTH, false));
        assert_eq!(all_text(&cache, 0), plain);
    }

    #[test]
    fn a_file_that_will_not_be_colored_has_no_preview() {
        let mut cache = RenderCache::new(4, false, false);
        let gate = Gate::default();
        cache.wait(Duration::ZERO);
        cache.hooks().real = Some(gate.clone());
        let diff = file("notes.unknownformat");
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        gate.open();
        cache.finish();
        assert!(cached(&cache, 0));
    }

    /// Everything anything keeps an index into: if these agree, the cursor, scroll
    /// and search can't tell the plain render from the real one.
    fn assert_same_shape(plain: &Rendered, real: &Rendered, what: &str) {
        let text = |r: &Rendered| -> Vec<String> {
            let line = |l: &Line| l.spans.iter().map(|s| s.content.to_string()).collect();
            r.text.lines.iter().map(line).collect()
        };
        assert_eq!(text(plain), text(real), "{what}");
        assert_eq!(plain.height, real.height, "{what}");
        assert_eq!(plain.row_offsets, real.row_offsets, "{what}");
        assert_eq!(plain.changes, real.changes, "{what}");
        assert_eq!(plain.row_starts, real.row_starts, "{what}");
        for line in 0..real.lines() {
            assert_eq!(
                plain.line_map.get(line),
                real.line_map.get(line),
                "{what}: line {line}"
            );
            assert_eq!(
                plain
                    .line_map
                    .sources(line)
                    .map(|s| s.map(|s| (s.row, s.first_char, s.end_char))),
                real.line_map
                    .sources(line)
                    .map(|s| s.map(|s| (s.row, s.first_char, s.end_char))),
                "{what}: line {line}"
            );
        }
    }

    /// Lines laid out by what the parser found would move when the colors arrive.
    #[test]
    fn the_plain_render_has_the_shape_of_the_real_one() {
        let long = "let long = call(argument, another_argument); ".repeat(20);
        let bodies = [
            // Paired by similarity, opposite each other side by side.
            " fn a() {\n-    let total = sum(values);\n-    other();\n+    let total = sum(all_values);\n+    log();\n+    other();\n }\n",
            // Tabs, whose stops depend on what's before them.
            "-\tlet a\t= 1;\n+\tlet ab\t= 1;\n",
            // Wide and combining characters, in strings and out.
            "-let s = \"日本語のテキスト\";\n+let s = \"日本語の文章だよ\"; // コメント 🙂\n",
            // A line too long to compare, or to color.
            &format!("-{long}\n+{long}x\n"),
            &format!("-{}\n+{}y\n", "z".repeat(3000), "z".repeat(3000)),
            // Whitespace at the end, flagged on the added side.
            "-let a = 1;\n+let a = 1;   \n",
            // A rewrite with nothing to pair.
            "-aaaa\n-bbbb\n+cccc\n+dddd\n+eeee\n",
            // A string opened on one side only.
            "-let a = \"x;\n+let a = 1;\n let b = 2;\n",
            // No newline at the end, on each side.
            "-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n",
        ];
        let notes = [note("a.rs", 1, "a note")];
        let moved = [
            rust(
                "a.rs",
                "-fn moved_away() {\n-    first_call(1);\n-    second_call(2);\n-    third_call(3);\n-}\n",
            ),
            rust(
                "b.rs",
                "+fn moved_away() {\n+    first_call(1);\n+    second_call(2);\n+    third_call(3);\n+}\n",
            ),
        ];
        let index = Arc::new(MoveIndex::build(&moved));
        let cases: Vec<(FileDiff, Arc<MoveIndex>, usize)> = bodies
            .iter()
            .map(|body| (rust("a.rs", body), Arc::default(), 0))
            .chain([
                (moved[0].clone(), index.clone(), 0),
                (moved[1].clone(), index.clone(), 1),
            ])
            .collect();
        let cancelled = AtomicBool::new(false);
        for (diff, moves, at) in cases {
            for width in [24, 25, 60, 61, 120] {
                for side_by_side in [false, true] {
                    let key = Key {
                        file: at,
                        width,
                        side_by_side,
                        theme: GitHubDark,
                    };
                    let layer = with_comments(&diff, &notes, 1);
                    let spec = Spec {
                        key,
                        diff: diff.clone(),
                        moves: moves.clone(),
                        copies: Arc::default(),
                        tab_width: 4,
                        comments: layer.comments.owned(),
                        reader: None,
                        test: Default::default(),
                    };
                    let rows = diff::rows(&diff);
                    let plain = spec.plain(&rows, &cancelled).rendered;
                    let real = spec
                        .colored(&rows, &Parsing::of(&diff, &rows), &cancelled)
                        .rendered;
                    let what = format!("{} at {width}, sbs {side_by_side}", diff.raw);
                    assert_same_shape(&plain, &real, &what);
                }
            }
        }
    }

    #[test]
    fn a_job_a_resize_overtook_never_lands() {
        let (mut cache, gate) = held();
        let diff = file("a.rs");
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        until_started(&mut cache, 1);
        cache.clear();
        assert!(!cache.busy());
        gate.open();
        std::thread::sleep(Duration::from_millis(20));
        assert!(!cache.collect());
        assert!(!cached(&cache, 0));
    }

    #[test]
    fn a_change_of_width_or_theme_leaves_the_old_job_behind() {
        let (mut cache, gate) = held();
        let diff = file("a.rs");
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        cache.ensure(&request(0, &diff, WIDTH + 2, GitHubDark));
        cache.ensure(&request(0, &diff, WIDTH + 2, DiffTheme::GitHubLight));
        gate.open();
        cache.finish();
        assert!(!cached(&cache, 0));
        assert!(cache.get(key(0, WIDTH + 2, false, GitHubDark)).is_none());
        assert!(
            cache
                .get(key(0, WIDTH + 2, false, DiffTheme::GitHubLight))
                .is_some()
        );
    }

    #[test]
    fn invalidating_a_file_drops_what_was_being_made_of_it() {
        let (mut cache, gate) = held();
        let old = file("a.rs");
        cache.ensure(&request(0, &old, WIDTH, GitHubDark));
        cache.invalidate(0);
        gate.open();
        let new = rust("a.rs", "-before\n+after\n");
        cache.ensure(&request(0, &new, WIDTH, GitHubDark));
        cache.finish();
        assert!(says(&cache, 0, "after"));
    }

    /// An agent's note arriving during a render isn't lost, and doesn't cost a second
    /// render either.
    #[test]
    fn a_comment_that_arrives_during_a_render_is_spliced_in_after_it() {
        let (mut cache, gate) = held();
        let diff = file("a.rs");
        cache.ensure(&with_comments(&diff, &[], 1));
        until_started(&mut cache, 1);
        let notes = [note("a.rs", 2, "arrived late")];
        cache.ensure(&with_comments(&diff, &notes, 2));
        gate.open();
        cache.finish();
        assert!(!says(&cache, 0, "arrived late"), "made before it existed");

        cache.ensure(&with_comments(&diff, &notes, 2));
        cache.finish();
        assert!(says(&cache, 0, "arrived late"));
        assert_eq!(cache.rev(key(0, WIDTH, false, GitHubDark)), Some(2));
        assert_eq!(started(&mut cache).len(), 1, "one render, and a splice");
    }

    #[test]
    fn comments_can_come_and_go_without_doubling_up() {
        let mut cache = RenderCache::new(4, false, false);
        let diff = file("a.rs");
        let one = [note("a.rs", 2, "first")];
        let two = [note("a.rs", 2, "first"), note("a.rs", 1, "second")];
        let count = |cache: &RenderCache, what: &str| all_text(cache, 0).matches(what).count();

        cache.ensure(&with_comments(&diff, &[], 1));
        cache.ensure(&with_comments(&diff, &one, 2));
        assert_eq!(count(&cache, "first"), 1);
        cache.ensure(&with_comments(&diff, &two, 3));
        assert_eq!((count(&cache, "first"), count(&cache, "second")), (1, 1));
        cache.ensure(&with_comments(&diff, &one, 4));
        assert_eq!((count(&cache, "first"), count(&cache, "second")), (1, 0));
        cache.ensure(&with_comments(&diff, &[], 5));
        assert_eq!(count(&cache, "first"), 0);
        cache.ensure(&with_comments(&diff, &one, 6));
        assert_eq!(count(&cache, "first"), 1);
        assert_eq!(started(&mut cache).len(), 1);
    }

    #[test]
    fn a_comment_on_another_file_costs_this_one_nothing() {
        let mut cache = RenderCache::new(4, false, false);
        let diff = file("a.rs");
        cache.ensure(&with_comments(&diff, &[], 1));
        let before = cache
            .get(key(0, WIDTH, false, GitHubDark))
            .unwrap()
            .text
            .lines
            .as_ptr();
        cache.ensure(&with_comments(&diff, &[], 2));
        assert!(!cache.busy());
        let after = cache
            .get(key(0, WIDTH, false, GitHubDark))
            .unwrap()
            .text
            .lines
            .as_ptr();
        assert_eq!(before, after);
        assert_eq!(cache.rev(key(0, WIDTH, false, GitHubDark)), Some(2));
    }

    #[test]
    fn a_panic_leaves_the_plain_text_and_says_so() {
        let mut cache = RenderCache::new(4, false, false);
        let diff = file(PANICS);
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        assert!(says(&cache, 0, "let b"));
        assert!(!cache.is_preview(0, WIDTH, false));
        let failure = cache.take_failure().unwrap();
        assert!(
            failure.contains(PANICS) && failure.contains("injected"),
            "{failure}"
        );

        // It's done with, or every frame would panic all over again.
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        assert_eq!(started(&mut cache).len(), 1);
    }

    #[test]
    fn a_panic_in_the_fallback_still_lands_something() {
        let mut cache = RenderCache::new(4, false, false);
        let diff = file(ALWAYS_PANICS);
        cache.ensure(&request(0, &diff, WIDTH, GitHubDark));
        assert!(says(&cache, 0, "Couldn't render"));
        assert!(!cache.busy());

        let next = file("next.rs");
        cache.ensure(&request(1, &next, WIDTH, GitHubDark));
        assert!(cached(&cache, 1), "the threads survived it");
    }

    #[test]
    fn the_cache_drops_what_was_looked_at_longest_ago() {
        let mut cache = RenderCache::new(4, false, false);
        let files: Vec<FileDiff> = (0..4).map(|at| file(&format!("{at}.rs"))).collect();
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        let one = cache.stored_bytes();
        cache.hooks().limit = Some(one * 2 + one / 2);
        cache.ensure(&request(1, &files[1], WIDTH, GitHubDark));
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        cache.ensure(&request(2, &files[2], WIDTH, GitHubDark));
        assert!(cached(&cache, 0), "seen more recently than 1");
        assert!(!cached(&cache, 1));
        assert!(cached(&cache, 2));
        assert!(cache.stored_bytes() <= one * 2 + one / 2);
    }

    #[test]
    fn the_file_on_screen_stays_however_big_it_is() {
        let mut cache = RenderCache::new(4, false, false);
        cache.hooks().limit = Some(1);
        let files = [file("a.rs"), file("b.rs")];
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        cache.ensure(&request(1, &files[1], WIDTH, GitHubDark));
        assert!(!cached(&cache, 0));
        assert!(cached(&cache, 1));
    }

    #[test]
    fn a_prefetch_waits_for_the_file_on_screen_and_then_stops() {
        let (mut cache, gate) = held();
        cache.prefetch_for_test();
        let files = [file("a.rs"), file("b.rs")];
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
        gate.open();
        cache.finish();
        assert_eq!(started(&mut cache), [0], "not while a is under way");

        cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
        cache.finish();
        assert_eq!(started(&mut cache), [0, 1]);
        assert!(cached(&cache, 1));
        cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
        assert!(!cache.busy());
    }

    #[test]
    fn a_prefetch_gives_way_to_the_file_on_screen() {
        let (mut cache, gate) = held();
        cache.prefetch_for_test();
        let files = [file("a.rs"), file("b.rs"), file("c.rs")];
        cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
        until_started(&mut cache, 1);

        // Held at the gate, and it's dropped for a file the prefetch isn't of.
        cache.hooks().start = None;
        cache.wait(Duration::from_secs(60));
        cache.ensure(&request(2, &files[2], WIDTH, GitHubDark));
        assert!(cached(&cache, 2));
        gate.open();
        std::thread::sleep(Duration::from_millis(20));
        cache.collect();
        assert!(!cached(&cache, 1), "cancelled, not finished");
    }

    #[test]
    fn a_prefetch_of_the_file_asked_for_carries_on_as_its_render() {
        let (mut cache, gate) = held();
        cache.prefetch_for_test();
        let files = [file("a.rs"), file("b.rs")];
        cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
        until_started(&mut cache, 1);
        cache.ensure(&request(1, &files[1], WIDTH, GitHubDark));
        gate.open();
        cache.finish();
        assert!(cached(&cache, 1));
        assert_eq!(started(&mut cache), [1], "no second job");
    }

    #[test]
    fn a_prefetch_keeps_the_file_just_left() {
        let mut cache = RenderCache::new(4, false, false);
        cache.prefetch_for_test();
        let files: Vec<FileDiff> = (0..4).map(|at| file(&format!("{at}.rs"))).collect();
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        let one = cache.stored_bytes();
        cache.ensure(&request(1, &files[1], WIDTH, GitHubDark));
        // 2, prefetched, has been in the cache longer than 1 was on screen.
        cache.prefetch(&request(2, &files[2], WIDTH, GitHubDark));
        cache.finish();
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        cache.ensure(&request(1, &files[1], WIDTH, GitHubDark));
        cache.ensure(&request(2, &files[2], WIDTH, GitHubDark));
        cache.hooks().limit = Some(one * 3 + one / 2);

        cache.prefetch(&request(3, &files[3], WIDTH, GitHubDark));
        cache.finish();
        assert!(cached(&cache, 3));
        assert!(cached(&cache, 2), "on screen");
        assert!(cached(&cache, 1), "just left");
        assert!(!cached(&cache, 0));

        // With no room but what those two take up, it isn't kept at all.
        cache.hooks().limit = Some(one * 2 + one / 2);
        cache.ensure(&request(1, &files[1], WIDTH, GitHubDark));
        cache.prefetch(&request(0, &files[0], WIDTH, GitHubDark));
        cache.finish();
        assert!(!cached(&cache, 0));
        assert!(cached(&cache, 1) && cached(&cache, 2));
    }

    #[test]
    fn a_prefetch_too_big_to_keep_is_not_made_over_and_over() {
        let mut cache = RenderCache::new(4, false, false);
        cache.prefetch_for_test();
        let files = [file("a.rs"), file("b.rs")];
        cache.ensure(&request(0, &files[0], WIDTH, GitHubDark));
        cache.hooks().limit = Some(1);
        for _ in 0..3 {
            cache.prefetch(&request(1, &files[1], WIDTH, GitHubDark));
            cache.finish();
        }
        assert_eq!(started(&mut cache), [0, 1]);
    }

    /// Stepping through files quickly leaves one file to render. How little the
    /// others cost before they stop is for [`jobs`] and the cancel checks.
    #[test]
    fn stepping_through_files_only_keeps_the_last() {
        let (mut cache, gate) = held();
        let files: Vec<FileDiff> = (0..50).map(|at| file(&format!("{at}.rs"))).collect();
        for (at, diff) in files.iter().enumerate() {
            cache.ensure(&request(at, diff, WIDTH, GitHubDark));
        }
        assert_eq!(cache.running.len(), 1);
        gate.open();
        cache.finish();
        assert!(cached(&cache, 49));
        assert_eq!(cache.entries.len(), 1);
    }
}
