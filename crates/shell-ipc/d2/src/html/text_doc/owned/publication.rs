use super::{OwnedSave, WAITING};
use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::atomic_write::owned::{
    Admission, EffectValue, ShortEffect,
};
use shell_contract::runtime::Services;
use std::sync::{Arc, atomic::Ordering};

fn denied(message: impl Into<String>) -> NodeError {
    NodeError {
        code: None,
        message: message.into(),
    }
}

fn publish(request: &OwnedSave, effect: ShortEffect) -> Result<EffectValue, NodeError> {
    let waiting = WAITING.with(|waiting| waiting.borrow().get(&request.0.id).cloned());
    let (docs, view) = waiting.ok_or_else(|| denied("the document save is no longer pending"))?;
    let docs = docs
        .upgrade()
        .ok_or_else(|| denied("the document editor was closed"))?;
    request
        .effect(&docs, || {
            let active = docs.owned_saves.borrow().get(&view).is_some_and(|active| {
                active.0.id == request.0.id && Arc::ptr_eq(&active.0, &request.0)
            });
            if !active || !request.0.claimed.load(Ordering::Acquire) {
                return Err("the document save response is no longer active".into());
            }
            let matches = docs
                .read_view(view, |state| {
                    state.tracked
                        && state.save == request.0.original_save
                        && state.open == request.0.original_open
                        && request
                            .destination()
                            .context
                            .document_lease()
                            .is_some_and(|lease| {
                                lease.is_active() && lease.same_generation(&state.lease)
                            })
                })
                .unwrap_or(false);
            if !matches {
                return Err("the destination document changed before publication".into());
            }
            Ok(effect())
        })
        .map_err(denied)
}

impl OwnedSave {
    pub(crate) fn publication_admission(&self, services: &Services) -> Admission {
        let request = self.clone();
        let core = services.core.clone();
        Arc::new(move |effect| {
            if core.is_current() {
                return publish(&request, effect);
            }
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            let request = request.clone();
            core.post(Box::new(move || {
                let _ = send.send(publish(&request, effect));
            }));
            receive
                .recv()
                .map_err(|_| denied("the core stopped before document publication"))?
        })
    }
}
