//! Ways for a test to get a job to stop where it can be seen, or to fail.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use super::spec::Spec;
use super::*;

/// A file that panics in the render, and one that goes on to panic in the
/// plain render that's the fallback for that.
pub const PANICS: &str = "panics-rendering.rs";
pub const ALWAYS_PANICS: &str = "panics-always.rs";

/// Holds jobs until it's opened, or they're cancelled.
#[derive(Clone, Default)]
pub struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    pub fn open(&self) {
        let (open, changed) = &*self.0;
        *open.lock().unwrap_or_else(PoisonError::into_inner) = true;
        changed.notify_all();
    }

    fn pass(&self, cancelled: &AtomicBool) {
        let (open, changed) = &*self.0;
        let mut is_open = open.lock().unwrap_or_else(PoisonError::into_inner);
        while !*is_open && !cancelled.load(Ordering::Relaxed) {
            let wait = changed.wait_timeout(is_open, Duration::from_millis(1));
            is_open = wait.unwrap_or_else(PoisonError::into_inner).0;
        }
    }
}

#[derive(Clone, Default)]
pub struct Hooks {
    /// Stops a job before it has made anything.
    pub start: Option<Gate>,
    /// Stops a job between its preview and its real render.
    pub real: Option<Gate>,
    /// Each file a job started on, in order.
    pub started: Arc<Mutex<Vec<usize>>>,
    /// Stands in for [`MAX_CACHE_BYTES`].
    pub limit: Option<usize>,
}

impl Hooks {
    pub(super) fn at_start(&self, spec: &Spec, cancelled: &AtomicBool) {
        let started = &self.started;
        let mut started = started.lock().unwrap_or_else(PoisonError::into_inner);
        started.push(spec.key.file);
        drop(started);
        if let Some(gate) = &self.start {
            gate.pass(cancelled);
        }
        let path = spec.diff.path();
        assert!(path != PANICS && path != ALWAYS_PANICS, "injected");
    }

    pub(super) fn before_final(&self, cancelled: &AtomicBool) {
        if let Some(gate) = &self.real {
            gate.pass(cancelled);
        }
    }

    pub(super) fn at_plain(&self, spec: &Spec) {
        assert_ne!(spec.diff.path(), ALWAYS_PANICS, "injected");
    }
}
