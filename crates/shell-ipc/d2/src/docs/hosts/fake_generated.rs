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

pub(super) struct Prepared {
    pending: Arc<Pending>,
    work: Option<Box<dyn FnOnce() -> ResultValue>>,
}
impl Prepared {
    pub(super) fn new(work: impl FnOnce() -> ResultValue + 'static) -> Self {
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
