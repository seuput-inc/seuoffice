use shell_contract::hosts::{
    GeneratedCleanupRegistration, GeneratedObserveFailure, GeneratedPathObserver,
    GeneratedPathOutcome, GeneratedPathPending, GeneratedPathWake, GeneratedTerminalAck,
    GeneratedTerminalDelivery,
};
use shell_contract::ipc::{NativeDrivePendingOutcome, NativeDrivePendingReceipt};
use shell_contract::runtime::CancelToken;
use std::sync::{Arc, Mutex};

type AttemptDone = Box<dyn FnOnce(Result<(), String>) + Send>;
type ReleaseAction = Arc<dyn Fn(AttemptDone) + Send + Sync>;

enum Phase {
    Preparing,
    Opening,
    Pending(Arc<dyn NativeDrivePendingReceipt>),
    Retrying,
    Indeterminate(String),
    Terminal,
}
struct State {
    phase: Phase,
    observer: Option<GeneratedPathObserver>,
    observed: bool,
    wake: Option<GeneratedPathWake>,
    delivered: bool,
    acknowledged: bool,
    terminal: Option<Result<GeneratedPathOutcome, String>>,
    registration: Option<Box<dyn GeneratedCleanupRegistration>>,
    release_retained: Option<ReleaseAction>,
    release_running: bool,
    shutting_down: bool,
    drains: Vec<AttemptDone>,
}
pub(super) struct PendingOutput {
    state: Mutex<State>,
    cancel: CancelToken,
}

struct InverseAttempt {
    operation: Arc<PendingOutput>,
    receipt: Arc<dyn NativeDrivePendingReceipt>,
    done: Option<AttemptDone>,
}
impl Drop for InverseAttempt {
    fn drop(&mut self) {
        let Some(done) = self.done.take() else {
            return;
        };
        let error = "The Drive cleanup discarded its completion".to_owned();
        let drains = match self.operation.state.lock() {
            Ok(mut state) => {
                if matches!(state.phase, Phase::Retrying) {
                    state.phase = Phase::Pending(self.receipt.clone());
                }
                std::mem::take(&mut state.drains)
            }
            Err(_) => Vec::new(),
        };
        for waiter in drains {
            waiter(Err(error.clone()));
        }
        done(Err(error));
    }
}

struct ReleaseAttempt {
    operation: Arc<PendingOutput>,
    pending: bool,
}
impl Drop for ReleaseAttempt {
    fn drop(&mut self) {
        if self.pending {
            self.operation.release_finished(Err(
                "The generated payload release discarded its completion".into(),
            ));
        }
    }
}
/// Shared owner handle. The contract traits are implemented here because
/// `Arc<PendingOutput>` is foreign to this crate.
#[derive(Clone)]
pub(super) struct PendingHandle(Arc<PendingOutput>);

impl std::ops::Deref for PendingHandle {
    type Target = Arc<PendingOutput>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl PendingHandle {
    pub(super) fn new(wake: GeneratedPathWake) -> Self {
        Self(Arc::new(PendingOutput {
            state: Mutex::new(State {
                phase: Phase::Preparing,
                observer: None,
                observed: false,
                wake: Some(wake),
                delivered: false,
                acknowledged: false,
                terminal: None,
                registration: None,
                release_retained: None,
                release_running: false,
                shutting_down: false,
                drains: Vec::new(),
            }),
            cancel: CancelToken::default(),
        }))
    }
}

impl PendingOutput {
    pub(super) fn cancellation(&self) -> CancelToken {
        self.cancel.clone()
    }
    pub(super) fn install_registration(
        &self,
        registration: Box<dyn GeneratedCleanupRegistration>,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "The generated cleanup state is unavailable")?;
        if state.registration.is_some() {
            return Err("The generated cleanup was already registered".into());
        }
        state.registration = Some(registration);
        Ok(())
    }
    fn release_finished(&self, result: Result<(), String>) {
        let (registration, drains) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.release_running = false;
            match result {
                Ok(()) => {
                    state.release_retained = None;
                    (state.registration.take(), std::mem::take(&mut state.drains))
                }
                Err(error) => {
                    let drains = std::mem::take(&mut state.drains);
                    drop(state);
                    for done in drains {
                        done(Err(error.clone()));
                    }
                    return;
                }
            }
        };
        if let Some(registration) = registration {
            registration.complete();
        }
        for done in drains {
            done(Ok(()));
        }
    }
    fn retry_release(self: &Arc<Self>) {
        let release = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.release_running {
                return;
            }
            match state.release_retained.clone() {
                Some(release) => {
                    state.release_running = true;
                    Some(release)
                }
                None => None,
            }
        };
        match release {
            Some(release) => {
                let mut attempt = ReleaseAttempt {
                    operation: self.clone(),
                    pending: true,
                };
                let callback = Box::new(move |result| {
                    attempt.pending = false;
                    attempt.operation.release_finished(result);
                });
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| release(callback)))
                    .is_err()
                {
                    eprintln!(
                        "[shell] generated payload release panicked; the original release remains pending"
                    );
                }
            }
            None => self.release_finished(Ok(())),
        }
    }
    pub(super) fn mark_indeterminate(&self, error: String) {
        let drains = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if !matches!(state.phase, Phase::Preparing | Phase::Opening) {
                return;
            }
            state.phase = Phase::Indeterminate(error.clone());
            std::mem::take(&mut state.drains)
        };
        for done in drains {
            done(Err(error.clone()));
        }
    }
    pub(super) fn ready_to_start(&self) -> Result<(), String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "The generated cleanup state is unavailable")?;
        if !state.observed
            || state.observer.is_none()
            || state.registration.is_none()
            || state.shutting_down
            || !matches!(state.phase, Phase::Preparing)
        {
            return Err("The generated operation has no live registered observer".into());
        }
        Ok(())
    }
    pub(super) fn shutting_down(&self) -> bool {
        self.state.lock().map_or(true, |state| state.shutting_down)
    }
    pub(super) fn begin_open(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "The generated cleanup state is unavailable")?;
        if state.shutting_down || !matches!(state.phase, Phase::Preparing) {
            return Err("The generated operation was cancelled".into());
        }
        state.phase = Phase::Opening;
        Ok(())
    }
    pub(super) fn retain_inverse(
        &self,
        receipt: Arc<dyn NativeDrivePendingReceipt>,
        release: ReleaseAction,
    ) -> Result<bool, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "The generated cleanup state is unavailable")?;
        if !matches!(state.phase, Phase::Preparing) || state.release_retained.is_some() {
            return Err("The generated operation changed before cleanup".into());
        }
        state.release_retained = Some(release);
        state.phase = Phase::Pending(receipt);
        Ok(state.shutting_down)
    }
    #[cfg(test)]
    pub(super) fn finish_initial(
        self: &Arc<Self>,
        result: Result<GeneratedPathOutcome, String>,
        done: Box<
            dyn FnOnce(Result<shell_contract::hosts::GeneratedPathDispatchOutcome, String>) + Send,
        >,
    ) {
        let result = {
            let Ok(mut state) = self.state.lock() else {
                return done(Err("The generated cleanup state is unavailable".into()));
            };
            if matches!(state.phase, Phase::Terminal) {
                drop(state);
                return done(Err("The generated operation already settled".into()));
            }
            state.phase = Phase::Terminal;
            state.delivered = true;
            state.observed = true;
            if state.shutting_down {
                Err("The generated operation was cancelled".into())
            } else {
                result
            }
        };
        done(Ok(
            shell_contract::hosts::GeneratedPathDispatchOutcome::Terminal(
                GeneratedTerminalDelivery::new(result, Arc::new(PendingHandle(self.clone()))),
            ),
        ));
    }
    pub(super) fn finish(self: &Arc<Self>, result: Result<GeneratedPathOutcome, String>) {
        let (observer, result) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if matches!(state.phase, Phase::Terminal) {
                return;
            }
            state.phase = Phase::Terminal;
            let result = if state.shutting_down {
                Err("The generated operation was cancelled".into())
            } else {
                result
            };
            let observer = state.observer.take();
            let result = if observer.is_some() {
                state.delivered = true;
                Some(result)
            } else {
                state.terminal = Some(result);
                None
            };
            (observer, result)
        };
        if let (Some(observer), Some(result)) = (observer, result) {
            observer(GeneratedTerminalDelivery::new(
                result,
                Arc::new(PendingHandle(self.clone())),
            ));
        }
    }
    fn start_inverse(
        self: &Arc<Self>,
        receipt: Arc<dyn NativeDrivePendingReceipt>,
        done: AttemptDone,
    ) {
        let mut attempt = InverseAttempt {
            operation: self.clone(),
            receipt: receipt.clone(),
            done: Some(done),
        };
        receipt.retry(Box::new(move |outcome| {
            let Some(done) = attempt.done.take() else {
                return;
            };
            match outcome {
                NativeDrivePendingOutcome::Restored { original_error } => {
                    attempt.operation.finish(Err(original_error));
                    done(Ok(()));
                }
                NativeDrivePendingOutcome::StillPending { receipt, error } => {
                    let drains = {
                        let Ok(mut state) = attempt.operation.state.lock() else {
                            return done(Err("The generated cleanup state is unavailable".into()));
                        };
                        state.phase = Phase::Pending(receipt);
                        std::mem::take(&mut state.drains)
                    };
                    for waiter in drains {
                        waiter(Err(error.clone()));
                    }
                    done(Err(error));
                }
            }
        }));
    }
}
impl GeneratedTerminalAck for PendingHandle {
    fn complete(&self) -> Result<(), String> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "The generated cleanup state is unavailable")?;
            if !matches!(state.phase, Phase::Terminal) || !state.delivered || state.acknowledged {
                return Err("The generated terminal acknowledgement is unavailable".into());
            }
            state.acknowledged = true;
        }
        self.retry_release();
        Ok(())
    }
    fn restore(&self, result: Result<GeneratedPathOutcome, String>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.acknowledged
            || !matches!(state.phase, Phase::Terminal)
            || !state.delivered
            || state.terminal.is_some()
        {
            panic!("the generated terminal lease does not match its retained operation");
        }
        state.delivered = false;
        state.terminal = Some(result);
    }
}
impl GeneratedPathPending for PendingHandle {
    fn observe(
        &self,
        observer: GeneratedPathObserver,
        wake: GeneratedPathWake,
    ) -> Result<(), GeneratedObserveFailure> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => {
                return Err(GeneratedObserveFailure {
                    error: "The generated cleanup state is unavailable".into(),
                    observer,
                    wake,
                });
            }
        };
        if state.observed {
            return Err(GeneratedObserveFailure {
                error: "The generated operation already has an observer".into(),
                observer,
                wake,
            });
        }
        state.observed = true;
        state.wake = Some(wake);
        if let Some(result) = state.terminal.take() {
            state.delivered = true;
            drop(state);
            observer(GeneratedTerminalDelivery::new(
                result,
                Arc::new(self.clone()),
            ));
        } else {
            state.observer = Some(observer);
        }
        Ok(())
    }
    fn wake_observer(&self) -> Result<(), String> {
        let wake = {
            let state = self
                .state
                .lock()
                .map_err(|_| "The generated cleanup state is unavailable")?;
            if (!state.delivered && state.terminal.is_none()) || state.acknowledged {
                return Err("The generated terminal delivery is unavailable".into());
            }
            state
                .wake
                .clone()
                .ok_or("The generated observer wake is unavailable")?
        };
        wake();
        Ok(())
    }
    fn is_cancelling(&self) -> bool {
        self.shutting_down()
    }
    fn take_redelivery(&self) -> Result<Option<GeneratedTerminalDelivery>, String> {
        let result = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "The generated cleanup state is unavailable")?;
            if state.acknowledged {
                return Err("The generated terminal was already acknowledged".into());
            }
            let result = state.terminal.take();
            if result.is_some() {
                state.delivered = true;
            }
            result
        };
        Ok(result.map(|result| GeneratedTerminalDelivery::new(result, Arc::new(self.clone()))))
    }
    fn retry(&self, done: AttemptDone) {
        let receipt = {
            let Ok(mut state) = self.state.lock() else {
                return done(Err("The generated cleanup state is unavailable".into()));
            };
            if state.shutting_down {
                drop(state);
                return done(Err("The generated operation is shutting down".into()));
            }
            match std::mem::replace(&mut state.phase, Phase::Retrying) {
                Phase::Terminal if state.acknowledged && state.registration.is_some() => {
                    state.phase = Phase::Terminal;
                    state.drains.push(done);
                    drop(state);
                    self.retry_release();
                    return;
                }
                Phase::Terminal
                    if (state.delivered || state.terminal.is_some()) && !state.acknowledged =>
                {
                    state.phase = Phase::Terminal;
                    let wake = state.wake.clone();
                    drop(state);
                    return match wake {
                        Some(wake) => {
                            wake();
                            done(Ok(()));
                        }
                        None => done(Err("The generated observer wake is unavailable".into())),
                    };
                }
                Phase::Pending(receipt) => receipt,
                previous => {
                    state.phase = previous;
                    drop(state);
                    return done(Err("The generated cleanup is not retryable".into()));
                }
            }
        };
        self.start_inverse(receipt, done);
    }
    fn cancel_and_drain(&self, done: AttemptDone) {
        let (receipt, wake, release) = {
            let Ok(mut state) = self.state.lock() else {
                return done(Err("The generated cleanup state is unavailable".into()));
            };
            state.shutting_down = true;
            match std::mem::replace(&mut state.phase, Phase::Retrying) {
                Phase::Indeterminate(error) => {
                    state.phase = Phase::Indeterminate(error.clone());
                    drop(state);
                    self.cancel.cancel();
                    return done(Err(error));
                }
                Phase::Preparing => {
                    state.phase = Phase::Preparing;
                    let wake = state.wake.clone();
                    state.drains.push(done);
                    (None, wake, false)
                }
                Phase::Pending(receipt) => {
                    state.drains.push(done);
                    (Some(receipt), None, false)
                }
                Phase::Terminal if state.acknowledged && state.registration.is_none() => {
                    state.phase = Phase::Terminal;
                    drop(state);
                    return done(Ok(()));
                }
                Phase::Terminal if state.acknowledged => {
                    state.phase = Phase::Terminal;
                    state.drains.push(done);
                    (None, None, true)
                }
                Phase::Terminal => {
                    state.phase = Phase::Terminal;
                    let wake = state.wake.clone();
                    state.drains.push(done);
                    (None, wake, false)
                }
                previous => {
                    state.phase = previous;
                    state.drains.push(done);
                    (None, None, false)
                }
            }
        };
        self.cancel.cancel();
        if let Some(wake) = wake {
            wake();
        }
        if release {
            self.retry_release();
        }
        if let Some(receipt) = receipt {
            self.start_inverse(
                receipt,
                Box::new(|result| {
                    if let Err(error) = result {
                        eprintln!("[shell] generated cleanup remains pending: {error}");
                    }
                }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Registration(Arc<AtomicUsize>);
    impl GeneratedCleanupRegistration for Registration {
        fn complete(self: Box<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn terminal_delivery_keeps_registration_and_drain_until_actor_ack() {
        let operation = PendingHandle::new(Arc::new(|| {}));
        let completed = Arc::new(AtomicUsize::new(0));
        operation
            .install_registration(Box::new(Registration(completed.clone())))
            .unwrap();
        let delivery = Arc::new(Mutex::new(None));
        let captured = delivery.clone();
        let wakes = Arc::new(AtomicUsize::new(0));
        let notified = wakes.clone();
        assert!(
            operation
                .observe(
                    Box::new(move |value| *captured.lock().unwrap() = Some(value)),
                    Arc::new(move || {
                        notified.fetch_add(1, Ordering::SeqCst);
                    })
                )
                .is_ok()
        );
        operation.finish(Err("original failure".into()));
        let drained = Arc::new(AtomicUsize::new(0));
        let observed = drained.clone();
        operation.cancel_and_drain(Box::new(move |result| {
            result.unwrap();
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        assert_eq!(drained.load(Ordering::SeqCst), 0);
        let mut delivery = delivery.lock().unwrap().take().unwrap();
        assert!(delivery.result().is_err());
        delivery.acknowledge().unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(drained.load(Ordering::SeqCst), 1);
        assert!(delivery.acknowledge().is_err());
    }

    #[test]
    fn second_observer_is_returned_without_invocation() {
        let operation = PendingHandle::new(Arc::new(|| {}));
        assert!(operation.observe(Box::new(|_| {}), Arc::new(|| {})).is_ok());
        let called = Arc::new(AtomicUsize::new(0));
        let callback = called.clone();
        let rejected = operation.observe(
            Box::new(move |_| {
                callback.fetch_add(1, Ordering::SeqCst);
            }),
            Arc::new(|| {}),
        );
        assert!(rejected.is_err());
        assert_eq!(called.load(Ordering::SeqCst), 0);
        drop(rejected);
    }

    #[test]
    fn dropped_terminal_delivery_is_restored_for_exact_redelivery() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let notified = wakes.clone();
        let operation = PendingHandle::new(Arc::new(move || {
            notified.fetch_add(1, Ordering::SeqCst);
        }));
        let completed = Arc::new(AtomicUsize::new(0));
        operation
            .install_registration(Box::new(Registration(completed.clone())))
            .unwrap();
        operation.finish_initial(
            Err("original failure".into()),
            Box::new(|result| {
                assert!(result.is_ok());
            }),
        );
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        operation.wake_observer().unwrap();
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        let mut restored = operation.take_redelivery().unwrap().unwrap();
        assert!(matches!(restored.result(), Err(error) if error == "original failure"));
        restored.acknowledge().unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn discarded_release_callback_retains_registration_until_release_retry() {
        let operation = PendingHandle::new(Arc::new(|| {}));
        let completed = Arc::new(AtomicUsize::new(0));
        operation
            .install_registration(Box::new(Registration(completed.clone())))
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        operation.state.lock().unwrap().release_retained = Some(Arc::new(move |done| {
            if invoked.fetch_add(1, Ordering::SeqCst) != 0 {
                done(Ok(()));
            }
        }));
        let delivery = Arc::new(Mutex::new(None));
        let captured = delivery.clone();
        assert!(
            operation
                .observe(
                    Box::new(move |value| *captured.lock().unwrap() = Some(value)),
                    Arc::new(|| {})
                )
                .is_ok()
        );
        operation.finish(Err("original failure".into()));
        let mut delivery = delivery.lock().unwrap().take().unwrap();
        delivery.acknowledge().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        let drained = Arc::new(AtomicUsize::new(0));
        let observed = drained.clone();
        operation.cancel_and_drain(Box::new(move |result| {
            result.unwrap();
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(drained.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn indeterminate_shutdown_cancels_original_operation_without_completing_registration() {
        let operation = PendingHandle::new(Arc::new(|| {}));
        let completed = Arc::new(AtomicUsize::new(0));
        operation
            .install_registration(Box::new(Registration(completed.clone())))
            .unwrap();
        operation.mark_indeterminate("producer completion was discarded".into());
        let result = Arc::new(Mutex::new(None));
        let captured = result.clone();
        operation.cancel_and_drain(Box::new(move |value| {
            *captured.lock().unwrap() = Some(value)
        }));
        assert!(operation.cancellation().is_cancelled());
        assert!(
            matches!(&*result.lock().unwrap(), Some(Err(error)) if error == "producer completion was discarded")
        );
        assert_eq!(completed.load(Ordering::SeqCst), 0);
    }
}
