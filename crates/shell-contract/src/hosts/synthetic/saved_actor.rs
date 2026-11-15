//! Fixture-only saved actors. This models staged registration and explicit
//! parser acknowledgements; it does not attest a native renderer or parse files.
use crate::{
    IpcError, ViewId,
    editors::{PreparedSavedDocument, SavedActorRegistration, SavedDocumentLoadOutcome, Then},
    hosts::OwnedEffectsHost,
    ipc::{
        NativeBrowserSuperseded, NativeBrowserTransition, NativeClosureReceipt,
        NativeSavedDocument, NativeSavedTarget,
    },
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::Rc,
};

fn error(message: impl Into<String>) -> IpcError {
    IpcError {
        name: Some("Error".into()),
        message: message.into(),
    }
}
struct Slot {
    target: NativeSavedTarget,
    token: Rc<()>,
    settled: bool,
}
struct Script {
    target: NativeSavedTarget,
    expected: Vec<u8>,
    result: Result<SavedDocumentLoadOutcome, IpcError>,
}
#[derive(Default)]
struct State {
    slots: HashMap<ViewId, Slot>,
    scripts: VecDeque<Script>,
}

#[derive(Clone, Default)]
pub struct SyntheticSavedActors {
    state: Rc<RefCell<State>>,
}
impl SyntheticSavedActors {
    pub fn owns(&self, view: ViewId) -> bool {
        self.state.borrow().slots.contains_key(&view)
    }
    pub fn queue_completion(
        &self,
        target: NativeSavedTarget,
        expected: Vec<u8>,
        result: Result<SavedDocumentLoadOutcome, IpcError>,
    ) {
        self.state.borrow_mut().scripts.push_back(Script {
            target,
            expected,
            result,
        });
    }
    pub fn prepare(
        &self,
        target: NativeSavedTarget,
    ) -> Result<Box<dyn PreparedSavedDocument>, IpcError> {
        {
            let mut state = self.state.borrow_mut();
            if state.slots.contains_key(&target.view()) {
                return Err(error("Synthetic saved actor already exists"));
            }
            state
                .slots
                .try_reserve(1)
                .map_err(|e| error(e.to_string()))?;
        }
        Ok(Box::new(Prepared {
            receipt: Rc::new(Receipt {
                state: self.state.clone(),
                target,
                token: Rc::new(()),
                committed: Cell::new(false),
                retired: Cell::new(false),
            }),
        }))
    }
    pub fn load(
        &self,
        document: NativeSavedDocument,
        effects: &OwnedEffectsHost,
        then: Then<SavedDocumentLoadOutcome>,
    ) {
        let target = document.target();
        let extra: Vec<_> = target.additional().iter().collect();
        let mut result = None;
        let admitted = effects.with_native_saved_document_input(
            &document,
            &[],
            target.cancel(),
            target.pending(),
            &extra,
            &mut |input| {
                let mut state = self.state.borrow_mut();
                let slot = state
                    .slots
                    .get(&target.view())
                    .ok_or("Synthetic saved actor is unavailable")?;
                if !slot.target.same_target(target) || slot.settled {
                    return Err("Synthetic saved actor barrier changed".into());
                }
                let index = state
                    .scripts
                    .iter()
                    .position(|script| script.target.same_target(target))
                    .ok_or("Synthetic saved input requires a scripted completion")?;
                if state.scripts[index].expected != input.bytes() {
                    return Err(
                        "Synthetic saved input bytes do not match the scripted acknowledgement"
                            .into(),
                    );
                }
                let script = state
                    .scripts
                    .remove(index)
                    .expect("validated saved acknowledgement");
                state
                    .slots
                    .get_mut(&target.view())
                    .expect("validated saved actor slot")
                    .settled = true;
                result = Some(script.result);
                Ok(())
            },
        );
        then(match admitted {
            Err(message) => Err(error(message)),
            Ok(()) => {
                result.unwrap_or_else(|| Err(error("Synthetic saved input was not admitted")))
            }
        });
    }
}

struct Receipt {
    state: Rc<RefCell<State>>,
    target: NativeSavedTarget,
    token: Rc<()>,
    committed: Cell<bool>,
    retired: Cell<bool>,
}
impl Receipt {
    fn retire(&self) -> Result<(), IpcError> {
        if !self.committed.get() || self.retired.get() {
            return Err(error("Synthetic saved actor is not live"));
        }
        let mut state = self.state.borrow_mut();
        if !state.slots.get(&self.target.view()).is_some_and(|slot| {
            slot.target.same_target(&self.target) && Rc::ptr_eq(&slot.token, &self.token)
        }) {
            return Err(error("Synthetic saved actor was replaced"));
        }
        state.slots.remove(&self.target.view());
        self.retired.set(true);
        Ok(())
    }
}
impl SavedActorRegistration for Receipt {
    fn retire_closed(&self, receipt: NativeClosureReceipt, then: Then<()>) {
        if !receipt.matches(
            self.target.reservation(),
            self.target.view(),
            self.target.created(),
        ) {
            return then(Err(error("Synthetic saved close proof does not match")));
        }
        then(self.retire());
    }
    fn retire_superseded(&self, proof: NativeBrowserSuperseded, then: Then<()>) {
        if !proof.matches(self.target.reservation(), self.target.browser()) {
            return then(Err(error(
                "Synthetic saved replacement proof does not match",
            )));
        }
        then(self.retire());
    }
    fn rollback(&self, then: Then<()>) {
        if self.committed.get() {
            return then(Err(error(
                "Committed synthetic saved actor requires retirement proof",
            )));
        }
        self.retired.set(true);
        then(Ok(()));
    }
}
struct Prepared {
    receipt: Rc<Receipt>,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if !self.receipt.committed.get() {
            self.receipt.retired.set(true);
        }
    }
}
impl PreparedSavedDocument for Prepared {
    fn with_validated(
        &mut self,
        transition: &NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), IpcError>,
    ) -> Result<(), IpcError> {
        let receipt = &self.receipt;
        if receipt.committed.get()
            || receipt.retired.get()
            || !transition.matches(receipt.target.reservation())
            || !transition.matches_owner(receipt.target.browser())
        {
            return Err(error("Synthetic saved actor transition changed"));
        }
        if receipt
            .state
            .borrow()
            .slots
            .contains_key(&receipt.target.view())
        {
            return Err(error("Synthetic saved actor is already installed"));
        }
        next()?;
        receipt.state.borrow_mut().slots.insert(
            receipt.target.view(),
            Slot {
                target: receipt.target.clone(),
                token: receipt.token.clone(),
                settled: false,
            },
        );
        receipt.committed.set(true);
        Ok(())
    }
    fn finish(self: Box<Self>) -> Rc<dyn SavedActorRegistration> {
        self.receipt.clone()
    }
}
