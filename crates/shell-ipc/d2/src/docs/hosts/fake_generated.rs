//! Scripted generated-path host retaining its exact terminal delivery.
use shell_contract::hosts::{
    GeneratedObserveFailure, GeneratedPathObserver, GeneratedPathOutcome, GeneratedPathPending,
    GeneratedPathWake, GeneratedStartFailure, GeneratedTerminalAck, GeneratedTerminalDelivery,
    PreparedGeneratedPath,
};
use std::sync::{Arc, Mutex, Weak};

type ResultValue = Result<GeneratedPathOutcome, String>;
struct State {
    observer: Option<GeneratedPathObserver>,
    wake: Option<GeneratedPathWake>,
    result: Option<ResultValue>,
    observing: bool,
    started: bool,
    finished: bool,
    outstanding: bool,
    drainers: Vec<Box<dyn FnOnce(Result<(), String>) + Send>>,
}
struct Pending {
    own: Weak<Pending>,
    state: Mutex<State>,
}
impl Pending {
    fn create() -> Arc<Self> {
        Arc::new_cyclic(|own| Self {
            own: own.clone(),
            state: Mutex::new(State {
                observer: None,
                wake: None,
                result: None,
                observing: false,
                started: false,
                finished: false,
                outstanding: false,
                drainers: Vec::new(),
            }),
        })
    }
    fn publish(&self, result: ResultValue) {
        let observer = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.finished || state.outstanding || state.result.is_some() {
                return;
            }
            state.result = Some(result);
            state.observer.take()
        };
        if let Some(observer) = observer {
            if let Ok(Some(delivery)) = self.take_redelivery() {
                observer(delivery);
            }
        }
    }
}
impl GeneratedTerminalAck for Pending {
    fn complete(&self) -> Result<(), String> {
        let drainers = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.finished || !state.outstanding {
                return Err("The synthetic generated result was not pending".into());
            }
            state.finished = true;
            state.outstanding = false;
            std::mem::take(&mut state.drainers)
        };
        for done in drainers {
            done(Ok(()));
        }
        Ok(())
    }
    fn restore(&self, result: ResultValue) {
        let wake = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.finished || !state.outstanding {
                return;
            }
            state.outstanding = false;
            state.result = Some(result);
            state.wake.clone()
        };
        if let Some(wake) = wake {
            wake();
        }
    }
}
impl GeneratedPathPending for Pending {
    fn observe(
        &self,
        observer: GeneratedPathObserver,
        wake: GeneratedPathWake,
    ) -> Result<(), GeneratedObserveFailure> {
        let has_result = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.observing {
                return Err(GeneratedObserveFailure {
                    error: "The synthetic observer is already installed".into(),
                    observer,
                    wake,
                });
            }
            state.observing = true;
            state.wake = Some(wake);
            state.observer = Some(observer);
            state.result.is_some()
        };
        if has_result {
            let observer = self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .observer
                .take();
            if let (Some(observer), Ok(Some(delivery))) = (observer, self.take_redelivery()) {
                observer(delivery);
            }
        }
        Ok(())
    }
    fn wake_observer(&self) -> Result<(), String> {
        let wake = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .wake
            .clone();
        if let Some(wake) = wake {
            wake();
        }
        Ok(())
    }
    fn take_redelivery(&self) -> Result<Option<GeneratedTerminalDelivery>, String> {
        let result = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.outstanding || state.finished {
                return Ok(None);
            }
            let result = state.result.take();
            if result.is_some() {
                state.outstanding = true;
            }
            result
        };
        match result {
            Some(result) => {
                let own = self
                    .own
                    .upgrade()
                    .ok_or("The synthetic generated task was released")?;
                Ok(Some(GeneratedTerminalDelivery::new(result, own)))
            }
            None => Ok(None),
        }
    }
    fn is_cancelling(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        !state.finished && !state.drainers.is_empty()
    }
    fn retry(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        done(self.wake_observer());
    }
    fn cancel_and_drain(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        let publish = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.finished {
                drop(state);
                return done(Ok(()));
            }
            let publish = !state.outstanding && state.result.is_none();
            state.drainers.push(done);
            publish
        };
        if publish {
            self.publish(Err("The synthetic generated operation was cancelled".into()));
        }
    }
}

pub(crate) struct Prepared {
    pending: Arc<Pending>,
    work: Option<Box<dyn FnOnce() -> ResultValue>>,
}
impl Prepared {
    pub(crate) fn new(work: impl FnOnce() -> ResultValue + 'static) -> Self {
        Self {
            pending: Pending::create(),
            work: Some(Box::new(work)),
        }
    }
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if self.work.is_some() {
            self.pending
                .publish(Err("The synthetic generated operation was cancelled".into()));
        }
    }
}
impl PreparedGeneratedPath for Prepared {
    fn task(&self) -> Arc<dyn GeneratedPathPending> {
        self.pending.clone()
    }
    fn start(mut self: Box<Self>) -> Result<(), GeneratedStartFailure> {
        {
            let mut state = self
                .pending
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !state.observing || state.started {
                drop(state);
                return Err(GeneratedStartFailure {
                    error: "The synthetic generated task is not ready".into(),
                    prepared: self,
                });
            }
            state.started = true;
        }
        match self.work.take() {
            Some(work) => {
                self.pending.publish(work());
                Ok(())
            }
            None => Err(GeneratedStartFailure {
                error: "The synthetic generated operation was consumed".into(),
                prepared: self,
            }),
        }
    }
}

/// A scripted open of a published output. `open` runs inside the output's
/// admission with its path and answers whether the shell opened it; a
/// refusal is `NotOpened`, which the editor answers by revealing the file.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scripted_open(
    output: shell_contract::ipc::SavedPublication,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    additional: Vec<shell_contract::runtime::CancelToken>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    open: impl FnOnce(&str) -> Result<bool, String> + 'static,
) -> Box<dyn PreparedGeneratedPath> {
    Box::new(Prepared::new(move || {
        let path = output
            .path()
            .to_str()
            .ok_or("The synthetic generated path is not Unicode")?
            .to_owned();
        let extra: Vec<_> = additional.iter().collect();
        let mut open = Some(open);
        let mut opened = None;
        output.with_effect(
            effects.as_ref(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || {
                let open = open.take().ok_or("The synthetic open ran twice")?;
                opened = Some(open(&path)?);
                Ok(())
            },
        )?;
        match opened {
            Some(false) => Ok(GeneratedPathOutcome::NotOpened),
            Some(true) => {
                let receipt = match &output {
                    shell_contract::ipc::SavedPublication::Native(native) => {
                        shell_contract::hosts::CommittedGeneratedPath::from_existing_document_save(
                            native.selection().clone(),
                            native.selection().binding().owner().clone(),
                            participants,
                            cancel,
                            pending,
                            additional,
                            effects,
                        )
                    }
                    shell_contract::ipc::SavedPublication::Renderer(renderer) => {
                        let mut captured = participants;
                        captured.extend_from_slice(renderer.participants());
                        let mut controls = additional;
                        controls.push(renderer.cancel().clone());
                        controls.push(renderer.pending().clone());
                        controls.extend_from_slice(renderer.additional());
                        shell_contract::hosts::CommittedGeneratedPath::from_loaded_target(
                            renderer.origin().clone(),
                            renderer.destination().clone(),
                            renderer.destination().clone(),
                            cancel,
                            pending,
                            effects,
                        )
                        .and_then(|receipt| receipt.with_captured_participants(&captured))
                        .and_then(|receipt| receipt.with_captured_cancellations(&controls))
                    }
                }?;
                Ok(GeneratedPathOutcome::Opened(Arc::new(receipt)))
            }
            None => Err("The synthetic generated output was not admitted".into()),
        }
    }))
}

/// The fixture shell has no process-owned retry to register.
pub(crate) struct NoCleanup;
impl shell_contract::hosts::GeneratedCleanupRegistration for NoCleanup {
    fn complete(self: Box<Self>) {}
}
