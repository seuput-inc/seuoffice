//! Completion delivery after every native lock is released.
//!
//! Every callback the native module owes a caller (revocation, close receipts, evaluation
//! results, events) is queued here and run in FIFO order on the UI thread, never under a
//! state lock. Before [`install_ui`] ran (unit tests) jobs run inline on the calling
//! thread, which still happens after the caller released its locks because callers queue
//! only after unlocking.
//!
//! A thread that runs an effect under a state gate calls [`hold`] first: completions queued
//! meanwhile wait for the gate to end, so an effect that completes synchronously cannot run
//! user code under the gate.

use std::cell::Cell;
use std::sync::OnceLock;

pub(crate) type Job = Box<dyn FnOnce() + Send + 'static>;

/// How the module reaches the UI thread. Installed once from `main.rs` setup.
pub struct UiPoster {
    /// Run the job on the UI thread, later.
    pub post: Box<dyn Fn(Job) + Send + Sync>,
    pub is_ui_thread: Box<dyn Fn() -> bool + Send + Sync>,
}

static POSTER: OnceLock<UiPoster> = OnceLock::new();

thread_local! {
    static HELD: Cell<u32> = const { Cell::new(0) };
}

/// The job queue. One process-wide queue in the app; one per thread in unit tests, which run
/// in parallel threads and must not drain each other's jobs.
#[cfg(not(test))]
mod queue {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::Job;

    static QUEUE: Mutex<VecDeque<Job>> = Mutex::new(VecDeque::new());
    static DRAINING: AtomicBool = AtomicBool::new(false);

    pub(super) fn push(job: Job) {
        QUEUE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(job);
    }

    pub(super) fn pop() -> Option<Job> {
        QUEUE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front()
    }

    pub(super) fn is_empty() -> bool {
        QUEUE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    }

    /// False when another drain is already running.
    pub(super) fn begin_drain() -> bool {
        !DRAINING.swap(true, Ordering::AcqRel)
    }

    pub(super) fn end_drain() {
        DRAINING.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod queue {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use super::Job;

    thread_local! {
        static QUEUE: RefCell<VecDeque<Job>> = const { RefCell::new(VecDeque::new()) };
        static DRAINING: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn push(job: Job) {
        QUEUE.with(|queue| queue.borrow_mut().push_back(job));
    }

    pub(super) fn pop() -> Option<Job> {
        QUEUE.with(|queue| queue.borrow_mut().pop_front())
    }

    pub(super) fn is_empty() -> bool {
        QUEUE.with(|queue| queue.borrow().is_empty())
    }

    pub(super) fn begin_drain() -> bool {
        !DRAINING.with(|draining| draining.replace(true))
    }

    pub(super) fn end_drain() {
        DRAINING.with(|draining| draining.set(false));
    }
}

/// Returns false when a poster was already installed.
pub fn install_ui(poster: UiPoster) -> bool {
    POSTER.set(poster).is_ok()
}

/// True on the UI thread. Before a poster exists every thread counts as the UI thread.
pub fn is_ui_thread() -> bool {
    POSTER.get().is_none_or(|poster| (poster.is_ui_thread)())
}

/// Queue `job`; it runs after the caller's locks are released, in submission order. While
/// this thread holds a gate (see [`hold`]) the job only queues and runs when the gate ends.
pub(crate) fn defer(job: impl FnOnce() + Send + 'static) {
    queue::push(Box::new(job));
    if HELD.with(Cell::get) > 0 {
        return;
    }
    schedule();
}

fn schedule() {
    match POSTER.get() {
        Some(poster) if !(poster.is_ui_thread)() => (poster.post)(Box::new(drain)),
        _ => drain(),
    }
}

/// Proof that this thread holds a state gate. Completions queued while it lives run when the
/// last one is dropped. Declare it before the guards it covers: it must drop after them.
pub(crate) struct Hold(());

pub(crate) fn hold() -> Hold {
    HELD.with(|held| held.set(held.get() + 1));
    Hold(())
}

impl Drop for Hold {
    fn drop(&mut self) {
        let remaining = HELD.with(|held| {
            held.set(held.get() - 1);
            held.get()
        });
        if remaining == 0 {
            schedule();
        }
    }
}

fn drain() {
    loop {
        if !queue::begin_drain() {
            return;
        }
        while let Some(job) = queue::pop() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                eprintln!("shell: a native completion callback panicked");
            }
        }
        queue::end_drain();
        if queue::is_empty() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn jobs_queued_from_a_job_run_after_it_in_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let outer = log.clone();
        defer(move || {
            outer.lock().unwrap().push("outer");
            let inner = outer.clone();
            defer(move || inner.lock().unwrap().push("inner"));
            outer.lock().unwrap().push("outer-end");
        });
        assert_eq!(*log.lock().unwrap(), ["outer", "outer-end", "inner"]);
    }

    #[test]
    fn a_job_queued_under_a_held_gate_waits_for_the_gate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        {
            let _hold = hold();
            let queued = log.clone();
            defer(move || queued.lock().unwrap().push("ran"));
            assert!(
                log.lock().unwrap().is_empty(),
                "nothing runs under the gate"
            );
        }
        assert_eq!(*log.lock().unwrap(), ["ran"]);
    }
}
