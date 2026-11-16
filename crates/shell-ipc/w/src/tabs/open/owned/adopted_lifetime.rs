use crate::tabs::{host::EditorSeedRollbackStore, state::TabsState};
use main_core::shell::tab_manager::ViewClosureWatch;
use shell_contract::{
    ViewId,
    ipc::{NativeBrowserOwner, NativeBrowserRetirement, NativeClosureReservation},
    runtime::RendererGeneration,
};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

type RetirementObserver = Box<dyn FnOnce(NativeBrowserRetirement)>;
#[derive(Default)]
struct RetirementTerminal {
    outcome: Option<NativeBrowserRetirement>,
    observers: Vec<RetirementObserver>,
}

pub struct CallerOwnedAdoptedLifetime {
    error: String,
    state: Weak<TabsState>,
    lifetime: AdoptedEditorLifetime,
}

pub struct RetainedAdoptedLifetime {
    error: String,
    handle: AdoptedLifetimeHandle,
}

impl RetainedAdoptedLifetime {
    pub fn handle(&self) -> &AdoptedLifetimeHandle {
        &self.handle
    }
    pub fn into_handle(self) -> AdoptedLifetimeHandle {
        self.handle
    }
}

#[derive(Clone)]
pub struct AdoptedLifetimeHandle {
    state: Weak<TabsState>,
    id: u64,
    terminal: Rc<RefCell<RetirementTerminal>>,
    view: ViewId,
    created: RendererGeneration,
    reservation: NativeClosureReservation,
    owner: NativeBrowserOwner,
}

pub enum AdoptedLifetimeFailure {
    CallerOwned(Box<CallerOwnedAdoptedLifetime>),
    Retained(RetainedAdoptedLifetime),
}

impl AdoptedLifetimeFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::CallerOwned(failure) => &failure.error,
            Self::Retained(failure) => &failure.error,
        }
    }
}

impl AdoptedLifetimeHandle {
    pub fn into_failure(self, error: String) -> AdoptedLifetimeFailure {
        AdoptedLifetimeFailure::Retained(RetainedAdoptedLifetime {
            error,
            handle: self,
        })
    }

    pub fn matches_retirement(&self, proof: &NativeBrowserRetirement) -> bool {
        match proof {
            NativeBrowserRetirement::Closed(receipt) => {
                receipt.matches(&self.reservation, self.view, &self.created)
                    && receipt
                        .terminal_owner()
                        .is_some_and(|owner| owner.same_owner(&self.owner))
            }
            NativeBrowserRetirement::Superseded(proof) => {
                proof.matches(&self.reservation, &self.owner)
            }
        }
    }

    pub fn observe_terminal(&self, done: Box<dyn FnOnce(NativeBrowserRetirement)>) {
        let mut terminal = self.terminal.borrow_mut();
        if let Some(outcome) = terminal.outcome.clone() {
            drop(terminal);
            done(outcome);
        } else {
            terminal.observers.push(done);
        }
    }

    pub fn retry(&self) -> Result<(), String> {
        if self.terminal.borrow().outcome.is_some() {
            return Ok(());
        }
        self.state
            .upgrade()
            .ok_or("The adopted editor manager is unavailable")?
            .retry_adopted_editor(self.id)
    }

    pub fn is_unclosed(&self) -> Result<bool, String> {
        if self.terminal.borrow().outcome.is_some() {
            return Ok(false);
        }
        let state = self
            .state
            .upgrade()
            .ok_or("The adopted editor manager is unavailable")?;
        let entries = state.adopted_editors.borrow();
        let entry = entries
            .get(&self.id)
            .ok_or("The adopted editor lifetime is unavailable")?;
        Ok(entry.receipt.is_none() && entry.watching && !entry.retiring)
    }
}

impl CallerOwnedAdoptedLifetime {
    pub fn matches_retirement(&self, proof: &NativeBrowserRetirement) -> bool {
        match proof {
            NativeBrowserRetirement::Closed(receipt) => {
                receipt.matches(
                    &self.lifetime.reservation,
                    self.lifetime.view,
                    &self.lifetime.created,
                ) && receipt
                    .terminal_owner()
                    .is_some_and(|owner| owner.same_owner(&self.lifetime.owner))
            }
            NativeBrowserRetirement::Superseded(proof) => {
                proof.matches(&self.lifetime.reservation, &self.lifetime.owner)
            }
        }
    }

    pub fn retry(mut self) -> Result<AdoptedLifetimeHandle, AdoptedLifetimeFailure> {
        let Some(state) = self.state.upgrade() else {
            self.error = "The adopted editor manager is unavailable".into();
            return Err(AdoptedLifetimeFailure::CallerOwned(Box::new(self)));
        };
        state.register_adopted_lifetime(self.lifetime)
    }
}

fn settle_terminal(terminal: &Rc<RefCell<RetirementTerminal>>, outcome: NativeBrowserRetirement) {
    let observers = {
        let mut terminal = terminal.borrow_mut();
        if terminal.outcome.is_some() {
            return;
        }
        terminal.outcome = Some(outcome.clone());
        std::mem::take(&mut terminal.observers)
    };
    for observer in observers {
        observer(outcome.clone());
    }
}

pub(crate) struct AdoptedEditorLifetime {
    view: ViewId,
    created: RendererGeneration,
    reservation: NativeClosureReservation,
    owner: NativeBrowserOwner,
    watch: ViewClosureWatch<NativeBrowserRetirement>,
    rollback: EditorSeedRollbackStore,
    receipt: Option<NativeBrowserRetirement>,
    watching: bool,
    retiring: bool,
    terminal: Rc<RefCell<RetirementTerminal>>,
}

impl TabsState {
    pub fn retain_adopted_editor(
        &self,
        view: ViewId,
        created: RendererGeneration,
        reservation: NativeClosureReservation,
        owner: NativeBrowserOwner,
        watch: ViewClosureWatch<NativeBrowserRetirement>,
        rollback: EditorSeedRollbackStore,
    ) -> Result<AdoptedLifetimeHandle, AdoptedLifetimeFailure> {
        let lifetime = AdoptedEditorLifetime {
            view,
            created,
            reservation,
            owner,
            watch,
            rollback,
            receipt: None,
            watching: false,
            retiring: false,
            terminal: Rc::new(RefCell::new(RetirementTerminal::default())),
        };
        self.register_adopted_lifetime(lifetime)
    }

    fn register_adopted_lifetime(
        &self,
        lifetime: AdoptedEditorLifetime,
    ) -> Result<AdoptedLifetimeHandle, AdoptedLifetimeFailure> {
        if !lifetime
            .reservation
            .matches(lifetime.view, &lifetime.created)
            || !lifetime
                .owner
                .reservation()
                .same_reservation(&lifetime.reservation)
        {
            return Err(AdoptedLifetimeFailure::CallerOwned(Box::new(
                CallerOwnedAdoptedLifetime {
                    error: "The adopted editor reservation does not match its view".into(),
                    state: self.this.clone(),
                    lifetime,
                },
            )));
        }
        let id = match self.owned_id() {
            Ok(id) => id,
            Err(error) => {
                return Err(AdoptedLifetimeFailure::CallerOwned(Box::new(
                    CallerOwnedAdoptedLifetime {
                        error,
                        state: self.this.clone(),
                        lifetime,
                    },
                )));
            }
        };
        let terminal = lifetime.terminal.clone();
        let view = lifetime.view;
        let created = lifetime.created.clone();
        let reservation = lifetime.reservation.clone();
        let owner = lifetime.owner.clone();
        let handle = AdoptedLifetimeHandle {
            state: self.this.clone(),
            id,
            terminal,
            view,
            created,
            reservation,
            owner,
        };
        self.adopted_editors.borrow_mut().insert(id, lifetime);
        if let Err(error) = self.retry_adopted_editor(id) {
            let mut entries = self.adopted_editors.borrow_mut();
            if entries
                .get(&id)
                .is_some_and(|entry| entry.receipt.is_none() && !entry.watching && !entry.retiring)
            {
                let lifetime = entries.remove(&id).expect("the exact lifetime is present");
                return Err(AdoptedLifetimeFailure::CallerOwned(
                    Box::new(CallerOwnedAdoptedLifetime {
                        error,
                        state: self.this.clone(),
                        lifetime,
                    }),
                ));
            }
            return Err(AdoptedLifetimeFailure::Retained(RetainedAdoptedLifetime {
                error,
                handle,
            }));
        }
        Ok(handle)
    }

    fn retry_adopted_editor(&self, id: u64) -> Result<(), String> {
        let watch = {
            let mut entries = self.adopted_editors.borrow_mut();
            let entry = entries
                .get_mut(&id)
                .ok_or("The adopted editor lifetime is unavailable")?;
            if entry.receipt.is_some() {
                drop(entries);
                return self.finish_adopted_editor(id);
            }
            if entry.watching {
                return Ok(());
            }
            entry.watching = true;
            entry.watch.clone()
        };
        let state = self.rc();
        let callback = crate::home_files::back_to_core(
            self.services.core.clone(),
            move |receipt: NativeBrowserRetirement| {
                let valid = {
                    let mut entries = state.adopted_editors.borrow_mut();
                    let Some(entry) = entries.get_mut(&id) else {
                        return;
                    };
                    if entry.receipt.is_some() {
                        return;
                    }
                    entry.watching = false;
                    let matches = match &receipt {
                        NativeBrowserRetirement::Closed(proof) => {
                            proof.matches(&entry.reservation, entry.view, &entry.created)
                                && proof
                                    .terminal_owner()
                                    .is_some_and(|owner| owner.same_owner(&entry.owner))
                        }
                        NativeBrowserRetirement::Superseded(proof) => {
                            proof.matches(&entry.reservation, &entry.owner)
                        }
                    };
                    if !matches {
                        false
                    } else {
                        entry.receipt = Some(receipt);
                        true
                    }
                };
                let result = if valid {
                    state.finish_adopted_editor(id)
                } else {
                    Err("The native closure receipt does not match the adopted editor".into())
                };
                if let Err(error) = result {
                    state
                        .hooks
                        .warn("[shell] adopted editor retirement is pending:", &error);
                }
            },
        );
        if let Err(error) = watch(callback) {
            if let Some(entry) = self.adopted_editors.borrow_mut().get_mut(&id) {
                entry.watching = false;
            }
            return Err(error);
        }
        Ok(())
    }

    fn finish_adopted_editor(&self, id: u64) -> Result<(), String> {
        let (proof, store, terminal) = {
            let mut entries = self.adopted_editors.borrow_mut();
            let entry = entries
                .get_mut(&id)
                .ok_or("The adopted editor lifetime is unavailable")?;
            if entry.retiring {
                return Ok(());
            }
            let proof = entry
                .receipt
                .clone()
                .ok_or("The native closure has not been acknowledged")?;
            entry.retiring = true;
            (proof, entry.rollback.clone(), entry.terminal.clone())
        };
        let terminal_proof = proof.clone();
        let state = self.rc();
        let done = Box::new(move |result: Result<(), String>| match result {
            Ok(()) => {
                state.adopted_editors.borrow_mut().remove(&id);
                settle_terminal(&terminal, terminal_proof);
            }
            Err(error) => {
                if let Some(entry) = state.adopted_editors.borrow_mut().get_mut(&id) {
                    entry.retiring = false;
                }
                state
                    .hooks
                    .warn("[shell] adopted editor retirement is pending:", &error);
            }
        });
        match proof {
            NativeBrowserRetirement::Closed(proof) => {
                super::receipt_retirement::retire_adopted_editor_receipts(store, proof, done)
            }
            NativeBrowserRetirement::Superseded(proof) => {
                super::receipt_retirement::rollback_editor_receipts(
                    store,
                    Some(NativeBrowserRetirement::Superseded(proof)),
                    true,
                    done,
                )
            }
        }
        Ok(())
    }

    pub(crate) fn supersede_adopted_editor(
        &self,
        reservation: &NativeClosureReservation,
        owner: &NativeBrowserOwner,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        if !proof.matches(reservation, owner) {
            return Err("The browser replacement does not match its owner".into());
        }
        let id = {
            let mut entries = self.adopted_editors.borrow_mut();
            let (id, entry) = entries
                .iter_mut()
                .find(|(_, entry)| {
                    entry.reservation.same_reservation(reservation) && entry.owner.same_owner(owner)
                })
                .ok_or("The replaced editor lifetime is unavailable")?;
            if entry.receipt.is_some() {
                return Err("The editor retirement was already acknowledged".into());
            }
            entry.receipt = Some(NativeBrowserRetirement::Superseded(proof));
            *id
        };
        self.finish_adopted_editor(id)
    }

    pub fn retry_adopted_editor_lifetimes(&self) -> Result<(), String> {
        let ids = self
            .adopted_editors
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for id in ids {
            self.retry_adopted_editor(id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn retained_terminal_observers_receive_the_original_settlement_once() {
        let view = ViewId(71);
        let created = RendererGeneration::new();
        let (issuer, reservation, owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, created.clone());
        let terminal = Rc::new(RefCell::new(RetirementTerminal::default()));
        let handle = AdoptedLifetimeHandle {
            state: Weak::new(),
            id: 1,
            terminal: terminal.clone(),
            view,
            created: created.clone(),
            reservation: reservation.clone(),
            owner,
        };
        let calls = Rc::new(Cell::new(0));
        let first = calls.clone();
        let expected = reservation.clone();
        let generation = created.clone();
        handle.observe_terminal(Box::new(move |proof| {
            assert!(matches!(proof, NativeBrowserRetirement::Closed(receipt)
                if receipt.matches(&expected, view, &generation)));
            first.set(first.get() + 1);
        }));
        assert_eq!(calls.get(), 0);
        let proof = NativeBrowserRetirement::Closed(issuer.closed());
        settle_terminal(&terminal, proof.clone());
        settle_terminal(&terminal, proof);
        assert_eq!(calls.get(), 1);
        let late = calls.clone();
        handle.observe_terminal(Box::new(move |_| late.set(late.get() + 1)));
        assert_eq!(calls.get(), 2);
    }
}
