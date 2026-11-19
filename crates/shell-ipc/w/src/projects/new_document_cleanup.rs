use main_core::shell::app_settings::queue::Completion;
use shell_contract::runtime::{CancelToken, Thread};
use std::sync::{Arc, Condvar, Mutex};

/// Installed by the native project host, not a renderer request.
#[derive(Clone)]
pub struct ProjectCleanupShutdown(pub CancelToken);

struct Entry {
    #[cfg(unix)]
    cleanup: main_core::shell::projects::write_guard::ProjectCleanup,
    completion: Completion,
    error: main_core::shell::projects::write_guard::WriteGuardError,
}
#[derive(Default)]
struct State {
    active: usize,
    closing: bool,
    retrying: bool,
    pending: Vec<Entry>,
    ready: Vec<(u64, Completion)>,
    acknowledging: usize,
    next_id: u64,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    shutdown: CancelToken,
    core: Arc<dyn Thread>,
}
#[derive(Clone)]
pub struct ProjectCleanupSink(Arc<Shared>);
pub struct ProjectOperation {
    sink: ProjectCleanupSink,
}

impl ProjectCleanupSink {
    pub(super) fn new(shutdown: CancelToken, core: Arc<dyn Thread>) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            shutdown,
            core,
        }))
    }
    /// Called on a worker before joining the settings FIFO.
    pub fn begin(&self) -> Result<ProjectOperation, String> {
        self.retry_pending()?;
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.closing || self.0.shutdown.is_cancelled() {
            return Err("Project cleanup is shutting down".into());
        }
        state.active += 1;
        Ok(ProjectOperation { sink: self.clone() })
    }
    #[cfg(unix)]
    pub fn retain(
        &self,
        cleanup: main_core::shell::projects::write_guard::ProjectCleanup,
        completion: Completion,
        error: main_core::shell::projects::write_guard::WriteGuardError,
    ) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.pending.push(Entry {
            cleanup,
            completion,
            error,
        });
        self.0.changed.notify_all();
    }
    /// An already-admitted successor cannot reach the held settings FIFO.
    /// Retry on the current worker, outside that FIFO, when one is waiting.
    #[cfg(unix)]
    pub fn retry_for_successor(&self) -> Result<(), String> {
        let waiting = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active
            != 0;
        if waiting {
            self.retry_pending()
        } else {
            Ok(())
        }
    }
    fn retry_pending(&self) -> Result<(), String> {
        loop {
            let entry = {
                let mut state = self
                    .0
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                while state.retrying {
                    state = self
                        .0
                        .changed
                        .wait(state)
                        .unwrap_or_else(|error| error.into_inner());
                }
                if self.0.shutdown.is_cancelled() {
                    return Err("Project cleanup shutdown reached".into());
                }
                let Some(entry) = state.pending.pop() else {
                    return Ok(());
                };
                state.retrying = true;
                entry
            };
            #[cfg(unix)]
            let result = match self.0.shutdown.with_active(|| entry.cleanup.retry()) {
                Some(result) => result.map_err(|error| {
                    format!("{}; cleanup retry failed: {error}", entry.error.message())
                }),
                None => Err("Project cleanup shutdown reached".into()),
            };
            #[cfg(not(unix))]
            let result: Result<(), String> = Err(entry.error.message());
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.retrying = false;
            match result {
                Ok(()) => {
                    let id = state.next_id;
                    state.next_id = state
                        .next_id
                        .checked_add(1)
                        .expect("project cleanup id exhausted");
                    state.ready.push((id, entry.completion));
                    drop(state);
                    let sink = self.clone();
                    self.0.core.post(Box::new(move || {
                        let completion = {
                            let mut state = sink
                                .0
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            let completion = state
                                .ready
                                .iter()
                                .position(|(candidate, _)| *candidate == id)
                                .map(|index| state.ready.swap_remove(index).1);
                            if completion.is_some() {
                                state.acknowledging += 1;
                            }
                            completion
                        };
                        if let Some(completion) = completion {
                            drop(completion);
                            let mut state = sink
                                .0
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            state.acknowledging -= 1;
                            sink.0.changed.notify_all();
                        }
                    }));
                    self.0.changed.notify_all();
                }
                Err(error) => {
                    state.pending.push(entry);
                    self.0.changed.notify_all();
                    return Err(error);
                }
            }
        }
    }
    /// Runs off-core. Once quiescing begins no new project write is admitted.
    pub(super) fn quiesce_and_drain(&self) -> Result<(), String> {
        let wake = self.clone();
        let _subscription = self.0.shutdown.on_cancel(move || {
            let _state = wake
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            wake.0.changed.notify_all();
        });
        {
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.closing = true;
        }
        loop {
            self.retry_pending()?;
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if self.0.shutdown.is_cancelled() {
                return Err("Project cleanup shutdown reached".into());
            }
            if state.active == 0
                && state.pending.is_empty()
                && state.ready.is_empty()
                && state.acknowledging == 0
                && !state.retrying
            {
                return Ok(());
            }
            if !state.pending.is_empty() && !state.retrying {
                drop(state);
                continue;
            }
            state = self
                .0
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
            drop(state);
        }
    }
}
impl Drop for ProjectOperation {
    fn drop(&mut self) {
        let mut state = self
            .sink
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active -= 1;
        self.sink.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct InlineThread;
    impl Thread for InlineThread {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            job();
        }
        fn is_current(&self) -> bool {
            false
        }
    }

    #[test]
    fn shutdown_waits_for_actual_operation_and_rejects_new_work() {
        let sink = ProjectCleanupSink::new(CancelToken::default(), Arc::new(InlineThread));
        let operation = sink.begin().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = sink.clone();
        std::thread::spawn(move || {
            tx.send(worker.quiesce_and_drain()).unwrap();
        });
        assert!(rx.try_recv().is_err());
        drop(operation);
        assert!(rx.recv().unwrap().is_ok());
        assert!(sink.begin().is_err());
    }

    #[test]
    fn shutdown_cancellation_does_not_claim_an_active_operation_finished() {
        let shutdown = CancelToken::default();
        let sink = ProjectCleanupSink::new(shutdown.clone(), Arc::new(InlineThread));
        let operation = sink.begin().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = sink.clone();
        std::thread::spawn(move || {
            tx.send(worker.quiesce_and_drain()).unwrap();
        });
        shutdown.cancel();
        assert!(rx.recv().unwrap().is_err());
        drop(operation);
    }
}
