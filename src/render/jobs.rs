//! The threads that rendering happens on, so a big file never holds up a key.
//!
//! There's no queue to fall behind. Each kind of work has one slot and a newer
//! request replaces what's in it, so stepping through a hundred files leaves one
//! file to render, not a hundred. The file on screen goes first, freeing memory
//! next, and a prefetch only starts when nothing else is running, which leaves the
//! second thread free for the moment the file on screen changes.
//!
//! A panic here must leave the app running, which takes more than catching it:
//! see the panic hook in `app::terminal`.

use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

const THREADS: usize = 2;
/// As much stack as the main thread gets, as parsers recurse.
const STACK_BYTES: usize = 8 << 20;

type Task = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queue {
    /// For the file on screen.
    urgent: Option<Task>,
    /// Dropping what a big render leaves behind.
    chores: Vec<Task>,
    /// A prefetch.
    idle: Option<Task>,
    running: usize,
    /// Threads not yet stopped.
    alive: usize,
    closing: bool,
}

impl Queue {
    fn next(&mut self) -> Option<Task> {
        self.urgent
            .take()
            .or_else(|| self.chores.pop())
            .or_else(|| self.idle.take_if(|_| self.running == 0))
    }
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    /// There's something for a thread to take, or they're to stop.
    work: Condvar,
    /// A thread stopped.
    stopped: Condvar,
}

impl Shared {
    /// Nothing panics holding the lock, as no task runs under it.
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The render threads, which start when there's first something for them to do.
#[derive(Default)]
pub struct Jobs {
    shared: Arc<Shared>,
    started: bool,
}

impl Jobs {
    /// Run `task` next, in place of any urgent task that hasn't started.
    pub fn urgent(&mut self, task: impl FnOnce() + Send + 'static) {
        self.submit(|queue| queue.urgent = Some(Box::new(task)));
    }

    /// Run `task` once nothing else is running, in place of any prefetch that
    /// hasn't started.
    pub fn idle(&mut self, task: impl FnOnce() + Send + 'static) {
        self.submit(|queue| queue.idle = Some(Box::new(task)));
    }

    /// Drop `value` on a thread of its own: a big render takes a while to free.
    pub fn discard(&mut self, value: impl Send + 'static) {
        self.submit(|queue| queue.chores.push(Box::new(move || drop(value))));
    }

    fn submit(&mut self, add: impl FnOnce(&mut Queue)) {
        if !self.started {
            self.started = true;
            self.spawn();
        }
        add(&mut self.shared.queue());
        self.shared.work.notify_all();
    }

    fn spawn(&self) {
        for at in 0..THREADS {
            let shared = self.shared.clone();
            shared.queue().alive += 1;
            thread::Builder::new()
                .name(format!("render-{at}"))
                .stack_size(STACK_BYTES)
                .spawn(move || work(&shared))
                .expect("a render thread can start");
        }
    }

    /// Stop the threads, giving what they're on `grace` to notice it's cancelled.
    /// One that's still going then ends with the process.
    pub fn stop(&mut self, grace: Duration) {
        let deadline = Instant::now() + grace;
        let mut queue = self.shared.queue();
        queue.closing = true;
        self.shared.work.notify_all();
        while queue.alive > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            queue = self
                .shared
                .stopped
                .wait_timeout(queue, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.shared.queue().closing = true;
        self.shared.work.notify_all();
    }
}

fn work(shared: &Shared) {
    while let Some(task) = take(shared) {
        // Tasks catch their own panics to report them. This is what's left, so a
        // thread can't be lost to one that gets past that.
        let _ = panic::catch_unwind(AssertUnwindSafe(task));
        shared.queue().running -= 1;
        shared.work.notify_all();
    }
    shared.queue().alive -= 1;
    shared.stopped.notify_all();
}

/// Wait for a task, or `None` once the threads are to stop.
fn take(shared: &Shared) -> Option<Task> {
    let mut queue = shared.queue();
    loop {
        if queue.closing {
            return None;
        }
        if let Some(task) = queue.next() {
            queue.running += 1;
            return Some(task);
        }
        queue = shared
            .work
            .wait(queue)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};

    use super::*;

    const LONG: Duration = Duration::from_secs(10);

    /// A task that holds a thread until the returned sender drops.
    fn blocker(jobs: &mut Jobs, urgent: bool) -> (mpsc::Sender<()>, Receiver<()>) {
        let (release, wait) = mpsc::channel::<()>();
        let (began, started) = mpsc::channel();
        let task = move || {
            began.send(()).unwrap();
            let _ = wait.recv();
        };
        if urgent {
            jobs.urgent(task);
        } else {
            jobs.idle(task);
        }
        (release, started)
    }

    #[test]
    fn a_newer_urgent_task_replaces_one_that_has_not_started() {
        let mut jobs = Jobs::default();
        let (a, a_began) = blocker(&mut jobs, true);
        a_began.recv_timeout(LONG).unwrap();
        let (b, b_began) = blocker(&mut jobs, true);
        b_began.recv_timeout(LONG).unwrap();

        // Both threads are taken, so these wait, and only the last will run.
        let (done, finished) = mpsc::channel();
        for at in 0..5 {
            let done = done.clone();
            jobs.urgent(move || done.send(at).unwrap());
        }
        drop((a, b, done));
        assert_eq!(finished.recv_timeout(LONG), Ok(4));
        assert!(
            finished.recv_timeout(LONG).is_err(),
            "the others were dropped"
        );
    }

    #[test]
    fn a_prefetch_waits_for_everything_else() {
        let mut jobs = Jobs::default();
        let (release, began) = blocker(&mut jobs, true);
        began.recv_timeout(LONG).unwrap();

        let (done, finished) = mpsc::channel();
        jobs.idle(move || done.send(()).unwrap());
        // A thread is free, but it's for the next urgent task.
        assert!(finished.recv_timeout(Duration::from_millis(50)).is_err());
        drop(release);
        finished.recv_timeout(LONG).unwrap();
    }

    #[test]
    fn urgent_work_gets_a_thread_while_a_prefetch_runs() {
        let mut jobs = Jobs::default();
        let (_held, began) = blocker(&mut jobs, false);
        began.recv_timeout(LONG).unwrap();
        let (done, finished) = mpsc::channel();
        jobs.urgent(move || done.send(()).unwrap());
        finished.recv_timeout(LONG).unwrap();
    }

    #[test]
    fn a_panic_costs_the_task_and_not_the_thread() {
        let mut jobs = Jobs::default();
        for _ in 0..THREADS * 2 {
            jobs.urgent(|| panic!("injected"));
        }
        let (done, finished) = mpsc::channel();
        jobs.urgent(move || done.send(()).unwrap());
        finished.recv_timeout(LONG).unwrap();
    }

    #[test]
    fn stopping_ends_the_threads() {
        let mut jobs = Jobs::default();
        jobs.urgent(|| {});
        jobs.stop(LONG);
        assert_eq!(jobs.shared.queue().alive, 0);
    }

    #[test]
    fn stopping_gives_up_on_a_stuck_task() {
        let mut jobs = Jobs::default();
        let (_held, began) = blocker(&mut jobs, true);
        began.recv_timeout(LONG).unwrap();
        let start = Instant::now();
        jobs.stop(Duration::from_millis(20));
        assert!(start.elapsed() < LONG);
        assert_eq!(jobs.shared.queue().alive, 1);
    }

    #[test]
    fn dropped_values_go_to_a_thread() {
        struct Noted(mpsc::Sender<Option<String>>);
        impl Drop for Noted {
            fn drop(&mut self) {
                let name = thread::current().name().map(str::to_string);
                let _ = self.0.send(name);
            }
        }
        let mut jobs = Jobs::default();
        let (noted, dropped) = mpsc::channel();
        jobs.discard(Noted(noted));
        let name = dropped.recv_timeout(LONG).unwrap();
        assert!(name.is_some_and(|name| name.starts_with("render-")));
    }
}
