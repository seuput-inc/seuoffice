//! The engine page is a private native service, never an editor ViewId.
use std::sync::{Arc, Mutex};
use shell_contract::runtime::RendererGeneration;
use crate::native::{DocumentContext, DocumentEvaluation, DocumentRevocation};

struct Incarnation { generation: RendererGeneration }
#[derive(Clone)]
pub(super) struct ServiceProof {
    incarnation: Arc<Incarnation>,
    pub origin: DocumentContext,
}
struct Binding { proof: ServiceProof, _loss: DocumentRevocation }
pub(super) struct NativeService {
    incarnation: Arc<Incarnation>,
    binding: Mutex<Option<Binding>>,
}

impl NativeService {
    pub fn new() -> Self {
        Self { incarnation: Arc::new(Incarnation { generation: RendererGeneration::new() }), binding: Mutex::new(None) }
    }

    /// Called only by the private dispatcher bound to the actual built Webview.
    pub fn bind(&self, origin: &DocumentContext, on_loss: impl FnOnce() + Send + 'static) -> bool {
        if !origin.is_registered() || !self.incarnation.generation.is_active() { return false }
        let Some(loss) = origin.on_revoke(on_loss) else { return false };
        let proof = ServiceProof { incarnation: self.incarnation.clone(), origin: origin.clone() };
        proof.with_active(|| {
            let mut binding = self.binding.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(binding) = binding.as_ref() {
                // Repeated confirmation of this exact native registration is
                // harmless; a second active context can never take its role.
                return binding.proof.same_context(&proof);
            }
            *binding = Some(Binding { proof: proof.clone(), _loss: loss });
            true
        }).unwrap_or(false)
    }

    pub fn capture(&self) -> Option<ServiceProof> {
        let proof = self.binding.lock().ok()?.as_ref()?.proof.clone();
        proof.with_active(|| ())?;
        Some(proof)
    }

    pub fn authenticate(&self, origin: DocumentContext) -> Option<ServiceProof> {
        let proof = self.capture()?;
        if proof.origin != origin || !Arc::ptr_eq(&proof.incarnation, &self.incarnation) { return None }
        proof.with_active(|| ())?;
        Some(proof)
    }

    /// Claim loss only for the registered document the service is bound to.
    pub fn close_if_matches(&self, origin: &DocumentContext) -> bool {
        let (binding, loss) = {
            let mut binding = self.binding.lock().unwrap_or_else(|error| error.into_inner());
            if !binding.as_ref().is_some_and(|bound| bound.proof.origin == *origin) {
                return false;
            }
            (binding.take(), self.incarnation.generation.revoke_deferred())
        };
        drop(binding);
        loss.cancel();
        true
    }

    pub fn close(&self) {
        self.incarnation.generation.revoke();
        let binding = self.binding.lock().unwrap_or_else(|error| error.into_inner()).take();
        drop(binding);
    }
}

impl ServiceProof {
    pub fn generation(&self) -> &RendererGeneration { &self.incarnation.generation }
    pub fn same_context(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.incarnation, &other.incarnation) && self.origin == other.origin
    }
    pub fn with_active<T>(&self, effect: impl FnOnce() -> T) -> Option<T> {
        self.origin.with_live(|| self.generation().with_active(effect)).flatten()
    }
}

type CancelDone = Box<dyn FnOnce(Result<(), String>) + Send>;
struct Cancellation {
    completed: bool,
    evaluation: Option<DocumentEvaluation>,
    loss: Option<shell_contract::runtime::CancelSubscription>,
    done: Option<CancelDone>,
}

fn finish_cancellation(state: &Mutex<Cancellation>, result: Result<(), String>) {
    let (evaluation, loss, done) = {
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        if state.completed { return; }
        state.completed = true;
        (state.evaluation.take(), state.loss.take(), state.done.take())
    };
    drop(evaluation);
    drop(loss);
    if let Some(done) = done { done(result); }
}

/// Completes after the captured realm processes cancellation, or reports
/// evaluator failure or loss of the service generation.
pub(super) fn cancel_complete(proof: &ServiceProof, id: u64, done: impl FnOnce(Result<(), String>) + Send + 'static) {
    let state = Arc::new(Mutex::new(Cancellation {
        completed: false, evaluation: None, loss: None, done: Some(Box::new(done)),
    }));
    let lost = Arc::downgrade(&state);
    let loss = proof.generation().loss_token().on_cancel(move || {
        if let Some(state) = lost.upgrade() {
            finish_cancellation(&state, Err("engine service was destroyed during cancellation".into()));
        }
    });
    {
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        if state.completed { return; }
        state.loss = Some(loss);
    }
    let completed = state.clone();
    let evaluation = proof.origin.evaluate(format!("(window.__seuofficeEngineCancel({id}), null)"), move |result| {
        finish_cancellation(&completed, result.map(|_| ()).map_err(|error| format!("engine cancellation failed: {error:?}")));
    });
    match evaluation {
        Ok(evaluation) => {
            let mut evaluation = Some(evaluation);
            {
                let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                if !state.completed { state.evaluation = evaluation.take(); }
            }
            drop(evaluation);
        }
        Err(error) => finish_cancellation(&state, Err(format!("engine cancellation failed: {error:?}"))),
    }
}

pub(super) fn cancel(proof: &ServiceProof, id: u64) {
    cancel_complete(proof, id, |_| {});
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn cancellation_settles_once_when_loss_and_completion_race() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let state = Arc::new(Mutex::new(Cancellation {
            completed: false, evaluation: None, loss: None,
            done: Some(Box::new(move |_| { observed.fetch_add(1, Ordering::SeqCst); })),
        }));
        let other = state.clone();
        let thread = std::thread::spawn(move || finish_cancellation(&other, Err("service lost".into())));
        finish_cancellation(&state, Ok(()));
        thread.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
