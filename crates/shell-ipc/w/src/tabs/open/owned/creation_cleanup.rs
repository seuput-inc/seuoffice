use crate::tabs::host::{ViewCreationCleanup, ViewCreationCleanupState, ViewCreationFailure};
use crate::tabs::state::TabsState;
use shell_contract::ipc::NativeBrowserRetirement;
use std::sync::Arc;

pub(crate) struct OwnedCreationCleanup {
    cleanup: Option<ViewCreationCleanup>,
    receipt: Option<NativeBrowserRetirement>,
    rollback: Option<
        std::rc::Rc<dyn Fn(Option<NativeBrowserRetirement>, Box<dyn FnOnce(Result<(), String>)>)>,
    >,
    identity: Arc<()>,
    running: bool,
    failure: Option<Box<dyn FnMut(&str)>>,
    completed: Option<Box<dyn FnOnce()>>,
}

impl TabsState {
    /// Retain the exact failed reservation until its native close is acknowledged.
    pub(crate) fn retain_creation_failure(
        &self,
        failure: ViewCreationFailure,
        done: Box<dyn FnOnce(main_core::shell::tab_manager::TabError)>,
    ) {
        let ViewCreationFailure { error, cleanup } = failure;
        match cleanup {
            ViewCreationCleanupState::Complete => done(error),
            ViewCreationCleanupState::Acknowledged(_) => done(error),
            ViewCreationCleanupState::Pending(cleanup) => {
                let done = std::rc::Rc::new(std::cell::RefCell::new(Some(done)));
                let completed = done.clone();
                let failed = done;
                let original_message = error.0.clone();
                let failure = Box::new(move |message: &str| {
                    if let Some(done) = failed.borrow_mut().take() {
                        done(main_core::shell::tab_manager::TabError(format!(
                            "{original_message}; cleanup failed: {message}"
                        )));
                    }
                });
                let completed = Box::new(move || {
                    if let Some(done) = completed.borrow_mut().take() {
                        done(error);
                    }
                });
                self.retain_creation_cleanup(cleanup, failure, completed);
            }
        }
    }
    pub(crate) fn retain_creation_cleanup(
        &self,
        cleanup: ViewCreationCleanup,
        failure: Box<dyn FnMut(&str)>,
        done: Box<dyn FnOnce()>,
    ) {
        self.retain_creation_cleanup_with_rollback(Some(cleanup), None, None, failure, done);
    }

    pub(crate) fn retain_creation_cleanup_with_rollback(
        &self,
        cleanup: Option<ViewCreationCleanup>,
        receipt: Option<NativeBrowserRetirement>,
        rollback: Option<
            std::rc::Rc<
                dyn Fn(Option<NativeBrowserRetirement>, Box<dyn FnOnce(Result<(), String>)>),
            >,
        >,
        failure: Box<dyn FnMut(&str)>,
        done: Box<dyn FnOnce()>,
    ) {
        let identity = Arc::new(());
        let id = Arc::as_ptr(&identity) as usize;
        self.owned_creation_cleanup.borrow_mut().insert(
            id,
            OwnedCreationCleanup {
                cleanup,
                receipt,
                rollback,
                identity,
                running: false,
                failure: Some(failure),
                completed: Some(done),
            },
        );
        self.retry_creation_cleanup(id);
    }

    pub(crate) fn retry_creation_cleanup(&self, id: usize) {
        let (cleanup, identity) = {
            let mut pending = self.owned_creation_cleanup.borrow_mut();
            let Some(entry) = pending.get_mut(&id) else {
                return;
            };
            if entry.running {
                return;
            }
            entry.running = true;
            (entry.cleanup.clone(), entry.identity.clone())
        };
        let Some(cleanup) = cleanup else {
            self.finish_creation_rollback(id, &identity);
            return;
        };
        let state = self.rc();
        let callback =
            crate::home_files::back_to_core(
                self.services.core.clone(),
                move |result: Result<NativeBrowserRetirement, String>| match result {
                    Ok(receipt) => {
                        let mut pending = state.owned_creation_cleanup.borrow_mut();
                        let Some(entry) = pending
                            .get_mut(&id)
                            .filter(|entry| Arc::ptr_eq(&entry.identity, &identity))
                        else {
                            return;
                        };
                        entry.cleanup = None;
                        entry.receipt = Some(receipt);
                        drop(pending);
                        state.finish_creation_rollback(id, &identity);
                    }
                    Err(error) => {
                        state.report_creation_cleanup_error(id, &identity, &error);
                    }
                },
            );
        cleanup(callback);
    }

    fn finish_creation_rollback(&self, id: usize, identity: &Arc<()>) {
        let (rollback, receipt) = {
            let pending = self.owned_creation_cleanup.borrow();
            let Some(entry) = pending
                .get(&id)
                .filter(|entry| Arc::ptr_eq(&entry.identity, identity))
            else {
                return;
            };
            (entry.rollback.clone(), entry.receipt.clone())
        };
        let state = self.rc();
        let identity = identity.clone();
        let done = Box::new(move |result: Result<(), String>| match result {
            Ok(()) => {
                let done = {
                    let mut pending = state.owned_creation_cleanup.borrow_mut();
                    if !pending
                        .get(&id)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.identity, &identity))
                    {
                        return;
                    }
                    pending
                        .remove(&id)
                        .and_then(|mut entry| entry.completed.take())
                };
                if let Some(done) = done {
                    done();
                }
                state.finish_creation_drain_if_ready();
            }
            Err(error) => state.report_creation_cleanup_error(id, &identity, &error),
        });
        match rollback {
            Some(rollback) => rollback(receipt, done),
            None => done(Ok(())),
        }
    }

    fn report_creation_cleanup_error(&self, id: usize, identity: &Arc<()>, error: &str) {
        let failure = {
            let mut pending = self.owned_creation_cleanup.borrow_mut();
            let Some(entry) = pending
                .get_mut(&id)
                .filter(|entry| Arc::ptr_eq(&entry.identity, identity))
            else {
                return;
            };
            entry.failure.take()
        };
        let mut waiters = std::mem::take(&mut *self.creation_cleanup_waiters.borrow_mut());
        let Some(mut failure) = failure else {
            return;
        };
        failure(error);
        let mut pending = self.owned_creation_cleanup.borrow_mut();
        if let Some(entry) = pending
            .get_mut(&id)
            .filter(|entry| Arc::ptr_eq(&entry.identity, identity))
        {
            entry.failure = Some(failure);
            entry.running = false;
        }
        drop(pending);
        waiters.extend(std::mem::take(
            &mut *self.creation_cleanup_waiters.borrow_mut(),
        ));
        for waiter in waiters {
            waiter(Err(error.to_owned()));
        }
    }

    pub fn drain_creation_cleanups(&self) {
        let ids = self
            .owned_creation_cleanup
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for id in ids {
            self.retry_creation_cleanup(id);
        }
        if let Err(error) = self.retry_adopted_editor_lifetimes() {
            self.hooks
                .warn("[shell] adopted editor retirement is pending:", &error);
        }
        self.retry_recovery_cleanups();
        self.retry_activation_cleanup();
    }

    pub(crate) fn begin_creation(&self, id: u64) -> Result<(), String> {
        if self.creation_shutdown.get() {
            return Err("The application is shutting down".into());
        }
        if !self.creations_inflight.borrow_mut().insert(id) {
            return Err("The creation operation is already registered".into());
        }
        Ok(())
    }

    pub(crate) fn finish_creation(&self, id: u64) {
        self.creations_inflight.borrow_mut().remove(&id);
        self.finish_creation_drain_if_ready();
    }

    pub(crate) fn finish_creation_drain_if_ready(&self) {
        if !self.owned_creation_cleanup.borrow().is_empty()
            || !self.creations_inflight.borrow().is_empty()
            || !self.retained_recovery_cleanups.borrow().is_empty()
        {
            return;
        }
        let error = self.activation_retry_error.borrow().clone();
        if self.activation_running.get() && error.is_none() {
            return;
        }
        let waiters = std::mem::take(&mut *self.creation_cleanup_waiters.borrow_mut());
        for waiter in waiters {
            match &error {
                Some(error) => waiter(Err(error.clone())),
                None => waiter(Ok(())),
            }
        }
    }

    pub fn cancel_creations_and_drain(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        self.creation_shutdown.set(true);
        let state = self.rc();
        self.creation_cleanup_waiters
            .borrow_mut()
            .push(Box::new(move |result| {
                done(result);
                drop(state);
            }));
        self.abort_owned_opens("application is shutting down");
        self.drain_creation_cleanups();
        self.finish_creation_drain_if_ready();
    }
}
