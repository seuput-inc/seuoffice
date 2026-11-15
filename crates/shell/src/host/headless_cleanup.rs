//! Process-owned headless cleanup, drained before the core and workers stop.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

pub(crate) type Done = Box<dyn FnOnce(Result<(), String>) + Send>;

/// Retry only the retained original cleanup. Complete its registration before
/// returning Ok, and only after actual native terminal and actor acknowledgments.
pub(crate) trait CleanupTask: Send + Sync {
    fn retry(&self, done: Done);
}

struct State {
    accepting: bool,
    next: u64,
    tasks: BTreeMap<u64, Arc<dyn CleanupTask>>,
    draining: bool,
    remaining: usize,
    error: Option<String>,
    waiters: Vec<Done>,
}

pub(crate) struct Supervisor(Mutex<State>);

/// Dropping a caller cannot discard unfinished cleanup. The process supervisor
/// retains its task until the leaf explicitly observes its real terminal join.
pub(crate) struct Registration {
    supervisor: Weak<Supervisor>,
    id: u64,
}

impl Registration {
    pub(crate) fn complete(self) {
        if let Some(supervisor) = self.supervisor.upgrade() {
            let task = supervisor.0.lock().unwrap_or_else(|error| error.into_inner()).tasks.remove(&self.id);
            drop(task);
        }
    }
}

struct GeneratedTask(Arc<dyn shell_contract::hosts::GeneratedPathPending>);
impl CleanupTask for GeneratedTask {
    fn retry(&self, done: Done) { self.0.cancel_and_drain(done); }
}

impl shell_contract::hosts::GeneratedCleanupRegistration for Registration {
    fn complete(self: Box<Self>) { (*self).complete(); }
}

struct Attempt {
    supervisor: Arc<Supervisor>,
    id: u64,
    pending: bool,
}

impl Attempt {
    fn finish(mut self, result: Result<(), String>) {
        self.pending = false;
        self.supervisor.retried(self.id, result);
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if self.pending {
            self.supervisor.retried(self.id, Err("Headless cleanup discarded its completion.".into()));
        }
    }
}

impl Supervisor {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(State {
            accepting: true, next: 1, tasks: BTreeMap::new(), draining: false,
            remaining: 0, error: None, waiters: Vec::new(),
        })))
    }

    /// Register before dispatching a native constructor or other owned effect.
    pub(crate) fn register(self: &Arc<Self>, task: Arc<dyn CleanupTask>) -> Result<Registration, String> {
        let mut state = self.0.lock().map_err(|_| "Headless cleanup state is unavailable.")?;
        if !state.accepting { return Err("Headless cleanup is quiescing.".into()); }
        let id = state.next;
        state.next = id.checked_add(1).ok_or("Headless cleanup identity exhausted.")?;
        state.tasks.insert(id, task);
        Ok(Registration { supervisor: Arc::downgrade(self), id })
    }

    pub(crate) fn register_generated(
        self: &Arc<Self>, operation: Arc<dyn shell_contract::hosts::GeneratedPathPending>,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String> {
        self.register(Arc::new(GeneratedTask(operation)))
            .map(|registration| Box::new(registration) as Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>)
    }

    /// Concurrent drains join the same attempt. A later attempt retries only
    /// still-retained tasks; callbacks and native operations never hold this lock.
    pub(crate) fn quiesce_and_retry(self: &Arc<Self>, done: Done) {
        let tasks = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            state.accepting = false;
            if state.draining { state.waiters.push(done); return; }
            if state.tasks.is_empty() { drop(state); done(Ok(())); return; }
            state.waiters.push(done);
            state.draining = true;
            state.error = None;
            state.remaining = state.tasks.len();
            state.tasks.iter().map(|(id, task)| (*id, task.clone())).collect::<Vec<_>>()
        };
        for (id, task) in tasks {
            let attempt = Attempt { supervisor: self.clone(), id, pending: true };
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                task.retry(Box::new(move |result| attempt.finish(result)));
            })).is_err() {
                eprintln!("[quit] headless cleanup retry panicked; its retained task must be retried");
            }
        }
    }

    /// A rejected quit leaves the application running; do not disable new work.
    pub(crate) fn resume_after_failed_quit(&self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if !state.draining { state.accepting = true; }
    }

    /// Listener delivery can enqueue actor cleanup. Do not confuse an empty
    /// native event queue with the actual terminal and actor-ACK join.
    pub(crate) fn ensure_drained(&self) -> Result<(), String> {
        let state = self.0.lock().map_err(|_| "Headless cleanup state is unavailable.")?;
        if state.draining || !state.tasks.is_empty() {
            Err("Native actor cleanup remains outstanding.".into())
        } else if state.accepting {
            Err("Native actor cleanup has not quiesced.".into())
        } else {
            Ok(())
        }
    }

    fn retried(&self, id: u64, result: Result<(), String>) {
        let (waiters, result) = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            let result = result.and_then(|()| {
                if state.tasks.contains_key(&id) {
                    Err("Headless cleanup returned before its terminal join.".into())
                } else { Ok(()) }
            });
            if let Err(error) = result {
                if state.error.is_none() { state.error = Some(error); }
            }
            state.remaining -= 1;
            if state.remaining != 0 { return; }
            state.draining = false;
            let result = state.error.take().map_or(Ok(()), Err);
            (std::mem::take(&mut state.waiters), result)
        };
        for done in waiters { done(result.clone()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Task { calls: AtomicUsize, done: Mutex<Option<Done>> }
    impl CleanupTask for Task {
        fn retry(&self, done: Done) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(self.done.lock().unwrap().replace(done).is_none());
        }
    }
    impl Task {
        fn reply(&self, result: Result<(), String>) {
            let done = self.done.lock().unwrap().take().unwrap();
            done(result);
        }
    }
    fn record(results: Arc<Mutex<Vec<Result<(), String>>>>) -> Done {
        Box::new(move |result| results.lock().unwrap().push(result))
    }

    #[test]
    fn worker_shutdown_requires_quiescence_and_the_actual_actor_join() {
        let supervisor = Supervisor::new();
        let task = Arc::new(Task::default());
        let registration = supervisor.register(task.clone()).unwrap();
        assert!(supervisor.ensure_drained().is_err());
        let results = Arc::new(Mutex::new(Vec::new()));
        supervisor.quiesce_and_retry(record(results.clone()));
        assert!(supervisor.ensure_drained().is_err());
        registration.complete();
        // Native terminal delivery alone has not settled the actor callback.
        assert!(supervisor.ensure_drained().is_err());
        task.reply(Ok(()));
        assert_eq!(*results.lock().unwrap(), vec![Ok(())]);
        assert!(supervisor.ensure_drained().is_ok());
        supervisor.resume_after_failed_quit();
        assert!(supervisor.ensure_drained().is_err());
    }

    #[test]
    fn drain_waits_for_real_terminal_and_joins_an_existing_attempt() {
        let supervisor = Supervisor::new();
        let task = Arc::new(Task::default());
        let registration = supervisor.register(task.clone()).unwrap();
        let results = Arc::new(Mutex::new(Vec::new()));
        supervisor.quiesce_and_retry(record(results.clone()));
        supervisor.quiesce_and_retry(record(results.clone()));
        assert_eq!(task.calls.load(Ordering::SeqCst), 1);
        assert!(results.lock().unwrap().is_empty());
        assert!(supervisor.register(Arc::new(Task::default())).is_err());
        registration.complete();
        task.reply(Ok(()));
        assert_eq!(*results.lock().unwrap(), vec![Ok(()), Ok(())]);
    }

    #[test]
    fn failure_retains_task_and_next_drain_retries_without_replaying_completed_work() {
        let supervisor = Supervisor::new();
        let task = Arc::new(Task::default());
        let registration = supervisor.register(task.clone()).unwrap();
        let results = Arc::new(Mutex::new(Vec::new()));
        supervisor.quiesce_and_retry(record(results.clone()));
        task.reply(Err("dispatch failed".into()));
        supervisor.quiesce_and_retry(record(results.clone()));
        registration.complete();
        task.reply(Ok(()));
        supervisor.quiesce_and_retry(record(results.clone()));
        assert_eq!(task.calls.load(Ordering::SeqCst), 2);
        assert_eq!(*results.lock().unwrap(), vec![Err("dispatch failed".into()), Ok(()), Ok(())]);
    }

    #[test]
    fn dropped_caller_or_early_success_cannot_abandon_cleanup() {
        let supervisor = Supervisor::new();
        let task = Arc::new(Task::default());
        drop(supervisor.register(task.clone()).unwrap());
        let results = Arc::new(Mutex::new(Vec::new()));
        supervisor.quiesce_and_retry(record(results.clone()));
        task.reply(Ok(()));
        assert!(results.lock().unwrap()[0].is_err());
        supervisor.quiesce_and_retry(record(results));
        assert_eq!(task.calls.load(Ordering::SeqCst), 2);
        task.reply(Err("still pending".into()));
    }

    #[test]
    fn discarded_completion_fails_without_releasing_the_retained_task() {
        struct Discard;
        impl CleanupTask for Discard { fn retry(&self, _done: Done) {} }
        let supervisor = Supervisor::new();
        let registration = supervisor.register(Arc::new(Discard)).unwrap();
        let results = Arc::new(Mutex::new(Vec::new()));
        supervisor.quiesce_and_retry(record(results.clone()));
        assert!(results.lock().unwrap()[0].is_err());
        assert_eq!(supervisor.0.lock().unwrap().tasks.len(), 1);
        supervisor.resume_after_failed_quit();
        assert!(supervisor.register(Arc::new(Task::default())).is_ok());
        registration.complete();
    }

    #[test]
    fn panicking_dispatch_does_not_skip_other_cleanup_or_report_success() {
        struct Panic;
        impl CleanupTask for Panic {
            fn retry(&self, _done: Done) { panic!("dispatch failed"); }
        }
        let supervisor = Supervisor::new();
        let _registration = supervisor.register(Arc::new(Panic)).unwrap();
        let task = Arc::new(Task::default());
        let registration = supervisor.register(task.clone()).unwrap();
        let results = Arc::new(Mutex::new(Vec::new()));
        supervisor.quiesce_and_retry(record(results.clone()));
        assert_eq!(task.calls.load(Ordering::SeqCst), 1);
        assert!(results.lock().unwrap().is_empty());
        registration.complete();
        task.reply(Ok(()));
        assert!(results.lock().unwrap()[0].is_err());
    }
}
