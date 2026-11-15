//! What the shell and the other batches call into sheets: the exported
//! functions of sheets-main.ts (`requestSheetsClose`, `sheetsFileRenamed`,
//! `sendSheetsMenuAction`, `writeSheetsCopiesNow`, …) as [`DocumentEditors`],
//! and the end of a view.

use std::path::{Path, PathBuf};

use jscompat::JsonValue;
use jscompat::node_path::host::{basename, dirname, join};
use main_core::packages::electron_utils::recovery_store::{RecoveryIdentity, RecoveryUntitledIdentity};
use main_core::sheets::close_guard::{CloseGuardDecision, close_guard_decision};
use main_core::sheets::session_rename::session_after_rename;
use main_core::sheets::temp_files::cleanup_expired_pasted_files;
use shell_contract::editors::{DocumentEditors, SaveMode, Then};
use shell_contract::{IpcError, IpcValue, ViewId, WindowId};

use super::core::{PendingBlank, QueueOutcome, QueuedWorkbook, TabState, UnsavedNew};
use super::core::{CloseWaiter, Core};
use super::dialogs::message_box;
use super::open::cleanup_session;
use super::session::SessionInfo;
use crate::value::{string, thrown};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RegistrationPhase { Prepared, Committed, Cancelled }

pub struct PreparedTabRegistration {
    core: Core,
    view: ViewId,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    tab: Option<TabState>,
    previous_active: std::rc::Rc<std::cell::Cell<Option<ViewId>>>,
    retired: std::rc::Rc<std::cell::Cell<bool>>,
    committed: bool,
    receipt: RegisteredTab,
}

#[derive(Clone)]
pub struct RegisteredTab {
    core: Core,
    view: ViewId,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    previous_active: std::rc::Rc<std::cell::Cell<Option<ViewId>>>,
    retired: std::rc::Rc<std::cell::Cell<bool>>,
    closed: std::rc::Rc<std::cell::Cell<bool>>,
    superseded: std::rc::Rc<std::cell::Cell<bool>>,
    phase: std::rc::Rc<std::cell::Cell<RegistrationPhase>>,
}

struct PreparedSavedWorkbook {
    registration: Option<PreparedTabRegistration>,
    slot: std::rc::Rc<std::cell::RefCell<Option<super::core::SavedLoad>>>,
    load: Option<super::core::SavedLoad>,
    receipt: std::rc::Rc<SavedWorkbookReceipt>,
}

struct SavedWorkbookReceipt {
    actor: std::cell::RefCell<RegisteredTab>,
    committed: std::cell::Cell<bool>,
    cancelled: std::cell::Cell<bool>,
}

impl shell_contract::editors::SavedActorRegistration for SavedWorkbookReceipt {
    fn retire_closed(&self, receipt: shell_contract::ipc::NativeClosureReceipt, then: Then<()>) {
        if !self.committed.get() { return then(Err(thrown("The workbook registration was not committed"))); }
        let actor = self.actor.borrow().clone();
        then(actor.retire_adopted(&receipt).map_err(thrown));
    }
    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, then: Then<()>) {
        if !self.committed.get() { return then(Err(thrown("The workbook registration was not committed"))); }
        let actor = self.actor.borrow().clone();
        then(actor.retire_superseded(&proof).map_err(thrown));
    }
    fn rollback(&self, then: Then<()>) {
        if !self.committed.get() {
            self.cancelled.set(true);
            return then(Ok(()));
        }
        let actor = self.actor.borrow().clone();
        then(actor.discard().map_err(thrown).and_then(|discarded|
            if discarded { Ok(()) } else { Err(thrown("The workbook registration cannot be rolled back")) }));
    }
}

impl shell_contract::editors::PreparedSavedDocument for PreparedSavedWorkbook {
    fn with_validated(
        &mut self, transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), IpcError>,
    ) -> Result<(), IpcError> {
        if self.receipt.cancelled.get() { return Err(thrown("The saved workbook registration was cancelled")); }
        let load = self.load.as_ref().ok_or_else(|| thrown("The saved workbook barrier was already committed"))?;
        if !transition.matches(load.target.reservation()) || !transition.matches_owner(load.target.browser()) {
            return Err(thrown("The saved workbook barrier belongs to another browser"));
        }
        let registration = self.registration.as_mut().ok_or_else(|| thrown("The workbook registration was consumed"))?;
        let validated = registration.validate_in_transition(transition).map_err(thrown)?;
        let mut slot = self.slot.try_borrow_mut().map_err(|_| thrown("The saved workbook barrier is busy"))?;
        if slot.is_some() { return Err(thrown("The saved workbook barrier is already installed")); }
        next()?;
        *slot = self.load.take();
        validated.commit();
        self.receipt.committed.set(true);
        Ok(())
    }

    fn finish(self: Box<Self>) -> std::rc::Rc<dyn shell_contract::editors::SavedActorRegistration> {
        if self.receipt.committed.get() {
            self.receipt.actor.borrow().core.start_pasted_temp_cleanup();
        }
        self.receipt.clone()
    }
}

pub struct PreparedSavedWorkbookBarrier {
    original: RegisteredTab,
    slot: std::rc::Rc<std::cell::RefCell<Option<super::core::SavedLoad>>>,
    document: shell_contract::runtime::RendererGeneration,
    load: Option<super::core::SavedLoad>,
}

impl PreparedSavedWorkbookBarrier {
    pub fn with_validated(
        &mut self, transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let load = self.load.as_ref().ok_or("The saved workbook barrier was already committed")?;
        let original = &self.original;
        if !transition.matches(&original.reservation) || !transition.matches_owner(&original.owner)
            || !transition.proposes_owner(load.target.browser()) {
            return Err("The saved workbook browser replacement does not match".into());
        }
        {
            let state = original.core.state.borrow();
            let tab = state.tab(original.view).ok_or("The saved workbook tab was closed")?;
            if original.closed.get() || original.superseded.get() || original.retired.get() || tab.browser_closing
                || !std::rc::Rc::ptr_eq(&tab.registration_retired, &original.retired)
                || !tab.document.same_generation(&self.document)
                || !tab.browser_owner.as_ref().is_some_and(|owner| owner.same_owner(&original.owner)) {
                return Err("The saved workbook registration changed".into());
            }
        }
        let mut slot = self.slot.try_borrow_mut().map_err(|_| "The saved workbook barrier is busy")?;
        if slot.is_some() { return Err("The saved workbook barrier is already installed".into()); }
        next()?;
        *slot = self.load.take();
        Ok(())
    }
}

pub struct ValidatedTabRegistration<'a> {
    state: std::cell::RefMut<'a, super::core::AppState>,
    tab: &'a mut Option<TabState>,
    previous_active: &'a std::cell::Cell<Option<ViewId>>,
    committed: &'a mut bool,
    view: ViewId,
    owner: shell_contract::ipc::NativeBrowserOwner,
    phase: &'a std::cell::Cell<RegistrationPhase>,
}

impl ValidatedTabRegistration<'_> {
    pub fn commit(mut self) {
        let mut tab = self.tab.take().expect("validated Sheets registration retains its tab");
        self.previous_active.set(self.state.active_view);
        tab.owned_registration = true;
        tab.browser_owner = Some(self.owner.clone());
        self.state.tabs.push((self.view, tab));
        self.state.active_view = Some(self.view);
        *self.committed = true;
        self.phase.set(RegistrationPhase::Committed);
    }
}

impl PreparedTabRegistration {
    pub fn prepare_receipt(&self) -> RegisteredTab { self.receipt.clone() }

    pub fn validate_in_transition<'a>(
        &'a mut self, transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
    ) -> Result<ValidatedTabRegistration<'a>, String> {
        if !transition.matches(&self.reservation) || !transition.matches_owner(&self.owner) {
            return Err("The Sheets registration has another browser owner".into());
        }
        let state = self.core.state.try_borrow_mut().map_err(|_| "The Sheets registration is busy")?;
        if self.committed || self.receipt.phase.get() != RegistrationPhase::Prepared || self.tab.is_none() || state.tab(self.view).is_some() {
            return Err("The Sheets tab registration state changed".into());
        }
        Ok(ValidatedTabRegistration { state, tab: &mut self.tab, previous_active: &self.previous_active,
            committed: &mut self.committed, view: self.view, owner: self.owner.clone(), phase: &self.receipt.phase })
    }

    pub fn commit_admitted(&mut self, transition: &shell_contract::ipc::NativeBrowserTransition<'_>) -> Result<(), String> {
        self.validate_in_transition(transition)?.commit();
        Ok(())
    }

    pub fn finish(self) -> Result<RegisteredTab, String> {
        if !self.committed { return Err("The Sheets registration was not committed".into()); }
        self.core.start_pasted_temp_cleanup();
        Ok(self.receipt.clone())
    }
}

impl Drop for PreparedTabRegistration {
    fn drop(&mut self) {
        if self.receipt.phase.get() == RegistrationPhase::Prepared {
            self.receipt.phase.set(RegistrationPhase::Cancelled);
        }
    }
}

pub struct PreparedTabTransfer {
    original: RegisteredTab,
    document: shell_contract::runtime::RendererGeneration,
    successor_closed: std::rc::Rc<std::cell::Cell<bool>>,
    successor_superseded: std::rc::Rc<std::cell::Cell<bool>>,
}

pub struct PreparedTabClose<'a> {
    original: &'a RegisteredTab,
    document: shell_contract::runtime::RendererGeneration,
}

pub struct ValidatedTabClose<'a, 'b, 'c> {
    prepared: PreparedTabClose<'a>,
    index: usize,
    _transition: &'b shell_contract::ipc::NativeBrowserTransition<'c>,
}

impl<'a> PreparedTabClose<'a> {
    pub fn validate_in_transition<'b, 'c>(self, transition: &'b shell_contract::ipc::NativeBrowserTransition<'c>) -> Result<ValidatedTabClose<'a, 'b, 'c>, String> {
        let original = self.original;
        original.validate_close_transition(transition)?;
        let state = original.core.state.borrow();
        let index = state.tabs.iter().position(|(view, _)| *view == original.view).ok_or("The Sheets tab was closed")?;
        if !state.tabs[index].1.document.same_generation(&self.document) {
            return Err("The Sheets document changed before close".into());
        }
        drop(state);
        Ok(ValidatedTabClose { prepared: self, index, _transition: transition })
    }
}

impl ValidatedTabClose<'_, '_, '_> {
    pub fn commit_in_transition(self) {
        self.prepared.original.core.state.borrow_mut().tabs[self.index].1.browser_closing = true;
    }
}

pub struct ValidatedTabTransfer<'b, 'c> {
    prepared: PreparedTabTransfer,
    successor: &'b shell_contract::ipc::NativeBrowserOwner,
    index: usize,
    _transition: &'b shell_contract::ipc::NativeBrowserTransition<'c>,
}

impl PreparedTabTransfer {
    pub fn validate_in_transition<'b, 'c>(self, transition: &'b shell_contract::ipc::NativeBrowserTransition<'c>, successor: &'b shell_contract::ipc::NativeBrowserOwner) -> Result<ValidatedTabTransfer<'b, 'c>, String> {
        let original = &self.original;
        if !transition.matches(&original.reservation) || !transition.matches_owner(&original.owner) || !transition.proposes_owner(successor) {
            return Err("The Sheets tab browser owner changed".into());
        }
        let state = original.core.state.borrow();
        let index = state.tabs.iter().position(|(view, _)| *view == original.view).ok_or("The Sheets tab was closed")?;
        let tab = &state.tabs[index].1;
        let current = original.core.env.services.views.renderer_generation(original.view);
        if original.retired.get() || original.superseded.get() || original.closed.get() || tab.browser_closing
            || !std::rc::Rc::ptr_eq(&tab.registration_retired, &original.retired)
            || !tab.document.same_generation(&self.document)
            || !original.created.is_active()
            || !current.as_ref().is_some_and(|generation| generation.same_generation(&original.created))
            || !tab.browser_owner.as_ref().is_some_and(|owner| owner.same_owner(&original.owner)) {
            return Err("The Sheets tab registration changed before transfer".into());
        }
        drop(state);
        Ok(ValidatedTabTransfer { prepared: self, successor, index, _transition: transition })
    }
}

impl ValidatedTabTransfer<'_, '_> {
    pub fn commit_in_transition(self) -> RegisteredTab {
        let PreparedTabTransfer { original, successor_closed, successor_superseded, .. } = self.prepared;
        let mut state = original.core.state.borrow_mut();
        state.tabs[self.index].1.browser_owner = Some(self.successor.clone());
        drop(state);
        RegisteredTab { core: original.core.clone(), view: original.view, created: original.created.clone(),
            reservation: original.reservation.clone(), owner: self.successor.clone(), previous_active: original.previous_active.clone(),
            retired: original.retired.clone(), closed: successor_closed, superseded: successor_superseded, phase: original.phase.clone() }
    }
}

impl RegisteredTab {
    pub fn prepare_saved_barrier(&self, target: shell_contract::ipc::NativeSavedTarget) -> Result<PreparedSavedWorkbookBarrier, String> {
        if target.surface() != shell_contract::Surface::Sheets || target.view() != self.view
            || !target.created().same_generation(&self.created)
            || !target.reservation().same_reservation(&self.reservation) {
            return Err("The saved workbook target does not match its existing registration".into());
        }
        let state = self.core.state.borrow();
        let tab = state.tab(self.view).ok_or("The saved workbook tab was closed")?;
        if self.closed.get() || self.superseded.get() || self.retired.get() || tab.browser_closing
            || !std::rc::Rc::ptr_eq(&tab.registration_retired, &self.retired)
            || !tab.browser_owner.as_ref().is_some_and(|owner| owner.same_owner(&self.owner))
            || tab.saved_load.borrow().is_some() {
            return Err("The saved workbook registration changed".into());
        }
        Ok(PreparedSavedWorkbookBarrier { original: self.clone(), slot: tab.saved_load.clone(),
            document: tab.document.clone(), load: Some(super::core::SavedLoad::new(target, self.core.env.uuid())) })
    }

    pub fn prepare_transfer(&self) -> Result<PreparedTabTransfer, String> {
        let state = self.core.state.borrow();
        let tab = state.tab(self.view).ok_or("The Sheets tab was closed")?;
        if self.retired.get() || self.superseded.get() || self.closed.get() || tab.browser_closing
            || !std::rc::Rc::ptr_eq(&tab.registration_retired, &self.retired)
            || !tab.browser_owner.as_ref().is_some_and(|owner| owner.same_owner(&self.owner)) {
            return Err("The Sheets tab registration changed before preparation".into());
        }
        Ok(PreparedTabTransfer { original: self.clone(), document: tab.document.clone(),
            successor_closed: std::rc::Rc::new(std::cell::Cell::new(false)),
            successor_superseded: std::rc::Rc::new(std::cell::Cell::new(false)) })
    }

    pub fn prepare_close(&self) -> Result<PreparedTabClose<'_>, String> {
        let state = self.core.state.borrow();
        let tab = state.tab(self.view).ok_or("The Sheets tab was closed")?;
        if self.retired.get() || self.superseded.get() || self.closed.get() || tab.browser_closing
            || !std::rc::Rc::ptr_eq(&tab.registration_retired, &self.retired)
            || !tab.browser_owner.as_ref().is_some_and(|owner| owner.same_owner(&self.owner)) {
            return Err("The Sheets registration changed before close preparation".into());
        }
        Ok(PreparedTabClose { original: self, document: tab.document.clone() })
    }

    pub fn validate_close_transition(&self, transition: &shell_contract::ipc::NativeBrowserTransition<'_>) -> Result<(), String> {
        if !transition.matches(&self.reservation) || !transition.matches_owner(&self.owner) {
            return Err("The Sheets close belongs to another browser owner".into());
        }
        let state = self.core.state.borrow();
        let tab = state.tab(self.view).ok_or("The Sheets tab was closed")?;
        if self.retired.get() || self.superseded.get() || self.closed.get() || tab.browser_closing
            || !std::rc::Rc::ptr_eq(&tab.registration_retired, &self.retired)
            || !tab.browser_owner.as_ref().is_some_and(|owner| owner.same_owner(&self.owner)) {
            return Err("The Sheets registration changed before close".into());
        }
        Ok(())
    }

    pub fn retire_superseded(&self, proof: &shell_contract::ipc::NativeBrowserSuperseded) -> Result<(), String> {
        if self.phase.get() != RegistrationPhase::Committed { return Err("The Sheets registration was not committed".into()); }
        if !proof.matches(&self.reservation, &self.owner) {
            return Err("The browser replacement belongs to another Sheets registration".into());
        }
        let state = self.core.state.borrow();
        let tab = state.tab(self.view).ok_or("The successor Sheets registration is unavailable")?;
        if !std::rc::Rc::ptr_eq(&tab.registration_retired, &self.retired)
            || !tab.browser_owner.as_ref().is_some_and(|owner| proof.successor_matches(owner)) {
            return Err("The successor Sheets registration does not match".into());
        }
        self.superseded.set(true);
        Ok(())
    }

    pub fn retire_closed(&self, receipt: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if self.phase.get() != RegistrationPhase::Committed { return Err("The Sheets registration was not committed".into()); }
        if !receipt.matches(&self.reservation, self.view, &self.created) {
            return Err("The native closure belongs to a different Sheets registration".into());
        }
        self.closed.set(true);
        Ok(())
    }

    pub fn retire_adopted(&self, receipt: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if receipt.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed {
            return Err("An adopted Sheets registration requires a closed browser".into());
        }
        self.retire_exact(receipt)
    }

    fn retire_exact(&self, receipt: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if self.phase.get() != RegistrationPhase::Committed { return Err("The Sheets registration was not committed".into()); }
        if !receipt.matches(&self.reservation, self.view, &self.created) {
            return Err("The native closure belongs to a different Sheets registration".into());
        }
        if self.retired.get() { return Ok(()); }
        let (mut tab, recovery, reported, openings, saves, closes, paths, writes, save_as, close, headless) = {
            let mut state = self.core.state.borrow_mut();
            let index = state.tabs.iter().position(|(view, tab)| *view == self.view
                && std::rc::Rc::ptr_eq(&tab.registration_retired, &self.retired)
                && tab.browser_owner.as_ref().is_some_and(|owner| owner.same_owner(&self.owner)))
                .ok_or("The Sheets registration was replaced before closure")?;
            let (_, tab) = state.tabs.remove(index);
            self.retired.set(true);
            if state.active_view == Some(self.view) { state.active_view = None; }
            state.save_as_export_targets.forget(f64::from(self.view.0));
            state.decided_opens.remove(&self.view);
            let recovery = state.tab_recovery.remove(&self.view);
            let reported = state.pending_edit_counts.remove(&self.view).unwrap_or(0.0);
            state.tracked_edit_senders.remove(&self.view);
            if state.pending_new_blank.as_ref().and_then(|entry| entry.target.as_ref())
                .is_some_and(|(view, generation)| *view == self.view && generation.same_generation(&self.created)) {
                if let Some(entry) = state.pending_new_blank.take() { entry.identity.set(QueueOutcome::Retired); }
                state.blank_revision = std::rc::Rc::new(());
            }
            if let Some(entry) = state.remove_queued_workbook(self.view) { entry.identity.set(QueueOutcome::Retired); }
            state.mcp_writable.remove(&self.view);
            let opening_ids: Vec<_> = state.recovery_open_waiters.iter().filter_map(|(id, waiter)|
                (waiter.view == self.view).then_some(id.clone())).collect();
            let openings: Vec<_> = opening_ids.into_iter().filter_map(|id| state.recovery_open_waiters.remove(&id)).collect();
            let save_ids: Vec<_> = state.owned_saves.iter().filter_map(|(id, waiter)|
                (waiter.scope.destination.view == self.view).then_some(id.clone())).collect();
            let saves: Vec<_> = save_ids.into_iter().filter_map(|id| state.owned_saves.remove(&id)).collect();
            let close_ids: Vec<_> = state.owned_closes.iter().filter_map(|(id, waiter)|
                (waiter.scope.destination.view == self.view).then_some(*id)).collect();
            let closes: Vec<_> = close_ids.into_iter().filter_map(|id| state.owned_closes.remove(&id)).collect();
            let path_ids: Vec<_> = state.path_save_waiters.iter().filter_map(|(id, (view, _))|
                (*view == self.view).then_some(*id)).collect();
            let paths: Vec<_> = path_ids.into_iter().filter_map(|id| state.path_save_waiters.remove(&id)).collect();
            let write_ids: Vec<_> = state.write_now_waiters.iter().filter_map(|(id, (view, _))|
                (*view == self.view).then_some(id.clone())).collect();
            let writes: Vec<_> = write_ids.into_iter().filter_map(|id| state.write_now_waiters.remove(&id)).collect();
            let save_as = state.save_as_waiters.remove(&self.view);
            let close = state.close_waiters.remove(&self.view);
            state.headless_targets.remove(&self.view);
            let headless = state.headless_waiters.remove(&self.view);
            (tab, recovery, reported, openings, saves, closes, paths, writes, save_as, close, headless)
        };
        drop(headless);
        tab.document.revoke();
        let saved_load = tab.saved_load.borrow_mut().take();
        if let Some(mut load) = saved_load {
            if let Some(done) = load.completion.take() {
                done(Err(thrown("The saved workbook view was closed")));
            }
        }
        if let Some(mut recovery) = recovery {
            if !recovery.crashed && !recovery.discarded && reported == 0.0 {
                for (_, session) in &tab.sessions {
                    if self.core.session_has_unsaved_copy(session)
                        && let Some(identity) = super::recovery_policy::session_recovery_identity(session)
                        && let Some(key) = self.core.env.copy_key(&identity) {
                        recovery.keep.insert(key);
                    }
                }
                for (key, identity) in &recovery.written {
                    if !recovery.keep.contains(key) {
                        let _ = self.core.env.with_store(|store| store.delete_copy(identity,
                            main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason::ClosedClean));
                    }
                }
            }
        }
        tab.save_transfers.dispose();
        let sessions = std::mem::take(&mut tab.sessions);
        let env = self.core.env.clone();
        self.core.env.spawn(move || {
            for (id, session) in sessions { cleanup_session(&env, &id, &session, session.import_temp_dir.as_deref()); }
        });
        for waiter in openings {
            drop(waiter.subscriptions);
            (waiter.done)(Err(main_core::shell::renderer_disposal::Failure("The workbook view was closed".into())));
        }
        for waiter in saves {
            waiter.scope.pending.cancel();
            drop(waiter.subscriptions);
            (waiter.then)(Err(thrown("The target workbook was closed")));
        }
        for waiter in closes {
            waiter.scope.pending.cancel();
            drop(waiter.subscriptions);
            (waiter.then)(Err(thrown("The target workbook was closed")));
        }
        for (_, then) in paths { then(Err(thrown("the target workbook is no longer open"))); }
        for (_, finish) in writes { finish(Err(main_core::shell::renderer_disposal::Failure("The workbook view was closed".into()))); }
        if let Some(waiter) = save_as { (waiter.resume)(false); }
        if let Some(waiter) = close { (waiter.resume)(false); }
        Ok(())
    }

    pub fn discard(&self) -> Result<bool, String> {
        if self.phase.get() == RegistrationPhase::Prepared {
            self.phase.set(RegistrationPhase::Cancelled);
            return Ok(true);
        }
        if self.phase.get() == RegistrationPhase::Cancelled { return Ok(true); }
        if self.superseded.get() { return Ok(true); }
        if self.retired.get() { return Ok(true); }
        if !self.closed.get() && self.core.env.services.views.renderer_generation(self.view)
            .is_some_and(|current| !current.same_generation(&self.created)) { return Ok(false); }
        let mut state = self.core.state.borrow_mut();
        let Some(index) = state.tabs.iter().position(|(view, tab)| *view == self.view
            && std::rc::Rc::ptr_eq(&tab.registration_retired, &self.retired)
            && tab.browser_owner.as_ref().is_some_and(|owner| owner.same_owner(&self.owner))) else {
                return Ok(self.closed.get());
            };
        let tab = &state.tabs[index].1;
        if !tab.sessions.is_empty() || tab.current_path.is_some() || tab.metadata_revision != Some(0)
            || state.queued_workbooks.contains_key(&self.view) { return Ok(false); }
        let (_, tab) = state.tabs.remove(index);
        self.retired.set(true);
        if state.active_view == Some(self.view) {
            state.active_view = self.previous_active.get().filter(|previous| state.tab(*previous).is_some());
        }
        drop(state);
        tab.document.revoke();
        Ok(true)
    }
}

impl shell_contract::editors::SavedActorRegistration for RegisteredTab {
    fn retire_closed(&self, receipt: shell_contract::ipc::NativeClosureReceipt, then: Then<()>) {
        then(self.retire_adopted(&receipt).map_err(thrown));
    }

    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, then: Then<()>) {
        then(RegisteredTab::retire_superseded(self, &proof).map_err(thrown));
    }

    fn rollback(&self, then: Then<()>) {
        then(self.discard().map_err(thrown).and_then(|discarded|
            if discarded { Ok(()) } else { Err(thrown("The workbook registration cannot be rolled back")) }));
    }
}

/// how long a close waits for the renderer's save to finish
const CLOSE_SAVE_TIMEOUT_MS: u64 = 120_000;
/// how long an OS shutdown waits for one tab's renderer to write its copy
const RECOVERY_WRITE_NOW_TIMEOUT_MS: u64 = 8_000;

/// `shuttingDown()`
fn shutting_down(core: &Core) -> bool {
    core.state.borrow().shutdown.active() || core.env.app_shutting_down()
}

impl Core {
    pub(super) fn cancel_saved_open(&self, view: ViewId, id: &str, message: &str) {
        let (selection, complete, subscriptions, cleanup) = {
            let mut state = self.state.borrow_mut();
            let Some(tab) = state.tab_mut(view) else { return };
            let slot = tab.saved_load.clone();
            let mut saved = slot.borrow_mut();
            let Some(saved) = saved.as_mut().filter(|saved| saved.id == id && saved.failure.is_none()) else { return };
            saved.failure = Some(message.to_owned());
            let cleanup = saved.session_id.take().and_then(|id| tab.remove_session(&id).map(|session| (id, session)));
            (saved.selection.take(), saved.completion.take(), std::mem::take(&mut saved.subscriptions), cleanup)
        };
        drop(subscriptions);
        if let Some((id, session)) = cleanup {
            let env = self.env.clone();
            self.env.spawn(move || cleanup_session(&env, &id, &session, session.import_temp_dir.as_deref()));
        }
        if let Some(selection) = selection { selection.reply.reply(Err(thrown(message))); }
        if let Some(complete) = complete { complete(Err(thrown(message))); }
    }

    pub(super) fn saved_open_result(
        &self, cx: &shell_contract::ipc::CallContext, input: &IpcValue,
    ) -> Result<IpcValue, IpcError> {
        let id = match crate::value::field(input, "id") {
            IpcValue::String(id) if !id.is_empty() => id,
            _ => return Err(thrown("The saved workbook acknowledgement has no identity")),
        };
        let ok = match crate::value::field(input, "ok") {
            IpcValue::Bool(ok) => *ok,
            _ => return Err(thrown("The saved workbook acknowledgement has no result")),
        };
        let error = match crate::value::field(input, "error") {
            IpcValue::Undefined if ok => None,
            IpcValue::String(message) if !ok => Some(if message.is_empty() { "The workbook renderer failed to load".into() } else { message.clone() }),
            IpcValue::Undefined if !ok => Some("The workbook renderer failed to load".into()),
            _ => return Err(thrown("The saved workbook acknowledgement is invalid")),
        };
        let actual = shell_contract::ipc::NativeRequestOwner {
            view: cx.sender.view,
            context: cx.native.clone().ok_or_else(|| thrown("The saved workbook sender has no native authority"))?,
        };
        let slot = {
            let state = self.state.borrow();
            let tab = state.tab(cx.sender.view).ok_or_else(|| thrown("The saved workbook tab was closed"))?;
            tab.saved_load.clone()
        };
        let document = {
            let saved = slot.borrow();
            let saved = saved.as_ref().ok_or_else(|| thrown("The saved workbook opening is unavailable"))?;
            if saved.id != *id || saved.session_id.is_none() || saved.completion.is_none() {
                return Err(thrown("The saved workbook acknowledgement does not match an opening"));
            }
            saved.document.clone().ok_or_else(|| thrown("The saved workbook has not registered"))?
        };
        let effects = self.env.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
            .ok_or_else(|| thrown("The saved workbook effects are unavailable"))?;
        let target = document.target();
        let additional: Vec<_> = target.additional().iter().collect();
        let mut finished = None;
        let mut cleanup = None;
        effects.with_native_saved_document_same_origin(
            &document, &actual, &cx.cancel, target.participants(), target.cancel(), target.pending(), &additional,
            &mut |_| {
                let mut state = self.state.borrow_mut();
                let tab = state.tab_mut(cx.sender.view).ok_or("The saved workbook tab was closed")?;
                if tab.browser_closing || !std::rc::Rc::ptr_eq(&tab.saved_load, &slot)
                    || !tab.browser_owner.as_ref().is_some_and(|owner| owner.same_owner(target.browser())) {
                    return Err("The saved workbook registration changed".into());
                }
                let mut saved = slot.borrow_mut();
                let current = saved.as_ref().ok_or("The saved workbook opening was released")?;
                if current.id != *id || !current.target.same_target(target) || current.session_id.is_none()
                    || current.completion.is_none() {
                    return Err("The saved workbook acknowledgement was superseded".into());
                }
                if !ok {
                    if let Some(session_id) = current.session_id.as_ref() {
                        if let Some(session) = tab.remove_session(session_id) {
                            cleanup = Some((session_id.clone(), session));
                        }
                    }
                }
                finished = saved.take();
                Ok(())
            },
        ).map_err(thrown)?;
        if let Some((session_id, session)) = cleanup {
            let env = self.env.clone();
            self.env.spawn(move || cleanup_session(&env, &session_id, &session, session.import_temp_dir.as_deref()));
        }
        let mut finished = finished.ok_or_else(|| thrown("The saved workbook acknowledgement was not admitted"))?;
        drop(std::mem::take(&mut finished.subscriptions));
        if let Some(complete) = finished.completion.take() {
            complete(match error {
                Some(error) => Err(thrown(error)),
                None if finished.recovery_applied => Ok(shell_contract::editors::SavedDocumentLoadOutcome::RecoveryApplied),
                None => Ok(shell_contract::editors::SavedDocumentLoadOutcome::Applied),
            });
        }
        Ok(IpcValue::Bool(true))
    }

    fn finish_recovery_open(&self, id: &str, result: Result<(), String>) {
        let waiter = self.state.borrow_mut().recovery_open_waiters.remove(id);
        if let Some(waiter) = waiter {
            drop(waiter.subscriptions);
            (waiter.done)(result.map_err(main_core::shell::renderer_disposal::Failure));
        }
    }

    pub fn capture_owned_call(
        &self, cx: &shell_contract::ipc::CallContext,
    ) -> Result<(crate::owned_save::OwnedSave, u64), IpcError> {
        let context = cx.native.clone().ok_or_else(|| thrown("Native save authority is unavailable"))?;
        let origin = shell_contract::ipc::NativeRequestOwner { view: cx.sender.view, context };
        let expected = origin.context.renderer()
            .ok_or_else(|| thrown("The save request has no renderer generation"))?;
        let effects = self.env.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned().ok_or_else(|| thrown("Native owned save effects are unavailable"))?;
        let destination = effects.capture_owner(cx.sender.view, expected).map_err(thrown)?;
        let (document, revision) = self.state.borrow().tab(cx.sender.view)
            .and_then(|tab| tab.metadata_revision.map(|revision| (tab.document.clone(), revision)))
            .ok_or_else(|| thrown("The workbook metadata is unavailable"))?;
        let scope = crate::owned_save::OwnedSave::new(origin, destination, cx.cancel.clone(), document, effects)?;
        Ok((scope, revision))
    }

    pub fn bind_save_renderer(&self, cx: &shell_contract::ipc::CallContext) {
        let view = cx.sender.view;
        self.cancel_owned_saves(view, "The destination workbook was replaced");
        let old = {
            let mut state = self.state.borrow_mut();
            let Some(tab) = state.tab_mut(view) else { return };
            tab.native_owner = cx.native.clone().map(|context| shell_contract::ipc::NativeRequestOwner { view, context });
            std::mem::replace(&mut tab.document, shell_contract::runtime::RendererGeneration::new())
        };
        old.revoke();
    }

    fn cancel_owned_saves(&self, view: ViewId, message: &str) {
        let tokens = self.state.borrow().owned_saves.iter()
            .filter_map(|(token, waiter)| (waiter.scope.destination.view == view).then_some(token.clone()))
            .collect::<Vec<_>>();
        for token in tokens {
            self.finish_owned_save(&token, Err(thrown(message)));
        }
    }

    pub fn finish_owned_save(&self, token: &str, result: Result<bool, IpcError>) {
        let waiter = self.state.borrow_mut().owned_saves.remove(token);
        if let Some(waiter) = waiter { self.complete_owned_save(waiter, result); }
    }

    pub(super) fn complete_owned_save(&self, waiter: super::core::OwnedSaveWaiter, result: Result<bool, IpcError>) {
        waiter.scope.pending.cancel();
        if let Some((_, path)) = &waiter.export_target {
            self.state.borrow_mut().save_as_export_targets.take(
                f64::from(waiter.scope.destination.view.0), path,
            );
        }
        drop(waiter.subscriptions);
        (waiter.then)(result);
    }

    pub fn owned_save_for(
        &self,
        cx: &shell_contract::ipc::CallContext,
        token: &str,
        input: &IpcValue,
    ) -> Result<crate::owned_save::OwnedSave, IpcError> {
        let mut state = self.state.borrow_mut();
        let waiter = state.owned_saves.get_mut(token)
            .ok_or_else(|| thrown("The owned workbook save request is no longer pending"))?;
        waiter.scope.validate_sender(cx)?;
        if matches!(crate::value::field(input, "quiet"), IpcValue::Bool(true)) {
            return Err(thrown("An owned workbook save is not an automatic save"));
        }
        if waiter.in_flight || waiter.writes >= 2 {
            return Err(thrown("The workbook save phase was already submitted"));
        }
        if crate::value::field(input, "sessionId") != &string(waiter.session_id.clone()) {
            return Err(thrown("The workbook save belongs to a replaced session"));
        }
        let target = crate::value::field(input, "targetPath");
        if waiter.writes == 0 {
            match (&waiter.target, target) {
                (Some(expected), IpcValue::String(actual)) if expected == actual => {}
                (None, IpcValue::Undefined) => {}
                _ => return Err(thrown("The workbook save target does not match its request")),
            }
        } else if !matches!(target, IpcValue::Undefined)
            || crate::value::field(input, "mode") != &string("save") {
            return Err(thrown("The workbook save continuation is not an in-place phase"));
        }
        waiter.in_flight = true;
        waiter.failure = None;
        Ok(waiter.scope.clone())
    }

    pub fn owned_save_phase_finished(&self, token: &str, result: &Result<IpcValue, IpcError>) {
        let mut state = self.state.borrow_mut();
        let Some(waiter) = state.owned_saves.get_mut(token) else { return };
        waiter.in_flight = false;
        waiter.failure = result.as_ref().err().cloned();
        if let Ok(value) = result {
            if let IpcValue::String(session) = crate::value::field(crate::value::field(value, "file"), "sessionId") {
                waiter.session_id = session.clone();
                waiter.writes += 1;
            }
            for (kind, key) in [("csv", "csvSaveAsPath"), ("ods", "odsSaveAsPath"), ("pdf", "pdfSaveAsPath")] {
                if let IpcValue::String(path) = crate::value::field(value, key) {
                    waiter.export_target = Some((kind.to_owned(), path.clone()));
                }
            }
        }
    }

    pub fn owned_export_for(
        &self, cx: &shell_contract::ipc::CallContext, token: &str, kind: &str, path: &str,
    ) -> Result<(crate::owned_save::OwnedSave, super::core::GeneratedTransition), IpcError> {
        let mut state = self.state.borrow_mut();
        let waiter = state.owned_saves.get_mut(token)
            .ok_or_else(|| thrown("The workbook copy request is no longer pending"))?;
        waiter.scope.validate_sender(cx)?;
        if waiter.in_flight || waiter.export_target.as_ref()
            .is_none_or(|(expected_kind, expected_path)| expected_kind != kind || expected_path != path) {
            return Err(thrown("The workbook copy does not match the selected Save As target"));
        }
        waiter.export_target = None;
        waiter.in_flight = true;
        Ok((waiter.scope.clone(), waiter.transition.clone()))
    }

    pub(super) fn request_owned_save(
        &self,
        view: ViewId,
        mode: SaveMode,
        target: Option<String>,
        authority: crate::owned_save::SaveAuthority,
        cancel: shell_contract::runtime::CancelToken,
        native_pending: Option<shell_contract::runtime::CancelToken>,
        parent: Option<crate::owned_save::OwnedSave>,
        then: Then<bool>,
    ) {
        let captured = {
            let state = self.state.borrow();
            state.tab(view).and_then(|tab| tab.native_owner.clone()
                .and_then(|owner| tab.sessions.iter().find(|(_, session)| {
                    tab.current_path.as_deref().is_some_and(|path| {
                        session.path == path || session.restore_target.as_deref() == Some(path)
                            || session.csv_source_path.as_deref() == Some(path)
                            || session.ods_source_path.as_deref() == Some(path)
                    })
                }).map(|(id, _)| (owner, tab.document.clone(), id.clone()))))
        };
        let Some((destination, document, session_id)) = captured else {
            return then(Err(thrown("The workbook has no captured native renderer")));
        };
        let scope = match parent {
            Some(parent) => {
                if !document.same_generation(&parent.document)
                    || !destination.context.renderer().zip(parent.destination.context.renderer())
                        .is_some_and(|(actual, expected)| actual.same_generation(expected)) {
                    return then(Err(thrown("The workbook close target was replaced")));
                }
                let origin = match parent.authority.renderer() {
                    Ok(origin) => origin.clone(),
                    Err(error) => return then(Err(error)),
                };
                crate::owned_save::OwnedSave::new(
                    origin, parent.destination.clone(), parent.pending.clone(),
                    parent.document.clone(), parent.effects.clone(),
                )
            }
            None => {
                let Some(effects) = self.env.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
                    return then(Err(thrown("Native owned save effects are unavailable")));
                };
                match authority {
                    crate::owned_save::SaveAuthority::Renderer(origin) => {
                        let destination = match destination.context.renderer()
                            .ok_or_else(|| "The workbook has no renderer generation".to_owned())
                            .and_then(|expected| effects.capture_owner(view, expected)) {
                                Ok(owner) => owner,
                                Err(error) => return then(Err(thrown(error))),
                            };
                        crate::owned_save::OwnedSave::new(origin, destination, cancel, document, effects)
                    }
                    crate::owned_save::SaveAuthority::Native(binding) => {
                        if binding.owner().view != view || !binding.owner().context.renderer()
                            .zip(destination.context.renderer()).is_some_and(|(a, b)| a.same_generation(b)) {
                            return then(Err(thrown("The native workbook target was replaced")));
                        }
                        let Some(pending) = native_pending else { return then(Err(thrown("The native workbook request has no pending lifetime"))); };
                        crate::owned_save::OwnedSave::new_native(binding, cancel, pending, document, effects)
                    }
                }
            }
        };
        let scope = match scope {
            Ok(scope) => scope,
            Err(error) => return then(Err(error)),
        };
        self.cancel_owned_saves(view, "The workbook save request was replaced");
        let token = self.env.uuid();
        let loss_env = self.env.clone();
        let loss_key = token.clone();
        let loss_pending = scope.pending.clone();
        let transition = super::core::GeneratedTransition::new(move || {
            loss_pending.cancel();
            let key = loss_key.clone();
            loss_env.post(move |core| core.finish_owned_save(&key, Err(thrown("The generated document was released"))));
        });
        let mut subscriptions = Vec::new();
        let losses = [
            (scope.authority.loss_token(), true), (scope.destination.loss_token(), false),
            (Some(scope.cancel.clone()), false),
        ];
        for (loss, is_origin) in losses.into_iter().filter_map(|(loss, is_origin)| loss.map(|loss| (loss, is_origin))) {
            let env = self.env.clone();
            let key = token.clone();
            let pending = scope.pending.clone();
            let transition = transition.clone();
            let is_request_cancel = loss.same_token(&scope.cancel);
            let original_cancel = scope.cancel.clone();
            subscriptions.push(loss.on_cancel(move || {
                if is_origin && transition.retired_origin() { return; }
                if is_request_cancel && transition.expected_dispatcher_cancel(&original_cancel) { return; }
                pending.cancel();
                env.post(move |core| {
                    core.finish_owned_save(&key, Err(thrown("The workbook save owner was released")));
                });
            }));
        }
        let require_write = mode == SaveMode::SaveAs || self.pending_edit_count(view) > 0.0;
        self.state.borrow_mut().owned_saves.insert(token.clone(), super::core::OwnedSaveWaiter {
            scope: scope.clone(), target: target.clone(), then, subscriptions, transition,
            session_id, in_flight: false, writes: 0, export_target: None, require_write,
            failure: None,
        });
        let env = self.env.clone();
        let key = token.clone();
        self.env.shell.after(CLOSE_SAVE_TIMEOUT_MS, Box::new(move || env.post(move |core| {
            core.finish_owned_save(&key, Err(thrown("Timed out saving the workbook")));
        })));
        let mut request = vec![
            ("requestId".into(), string(token.clone())),
            ("mode".into(), string(if mode == SaveMode::SaveAs { "save-as" } else { "save" })),
        ];
        if let Some(target) = target { request.push(("targetPath".into(), string(target))); }
        if let Err(error) = scope.emit(&scope.destination, "workbook:close-save-request", vec![IpcValue::Object(request)]) {
            self.finish_owned_save(&token, Err(thrown(error)));
        }
    }

    /// `markSheetsShuttingDown`
    pub fn mark_shutting_down(&self) {
        self.state.borrow_mut().shutdown.mark();
        self.env.set_app_shutting_down(true);
    }

    /// `resetSheetsShuttingDown`: the quit was vetoed
    pub fn reset_shutting_down(&self) {
        self.state.borrow_mut().shutdown.reset();
        self.env.set_app_shutting_down(false);
        self.resume_discarded_tab_copies();
    }

    /// `queueWorkbookForView`: the path this tab's first select opens without a dialog
    pub fn queue_workbook(&self, view: ViewId, path: &str) {
        self.state.borrow_mut().replace_queued_workbook(view, QueuedWorkbook::new(path.to_owned()));
    }

    /// `setSheetsNewBlank`
    pub fn set_new_blank(&self) {
        let mut state = self.state.borrow_mut();
        state.pending_new_blank = Some(PendingBlank::new(None));
        state.blank_revision = std::rc::Rc::new(());
    }

    /// `markSheetsUntitledPath`
    pub fn mark_untitled_path(&self, path: &str) {
        self.state
            .borrow_mut()
            .untitled_workbook_paths
            .insert(path.to_owned());
    }

    /// `authorizeMcpSheetWrite`
    pub fn authorize_mcp_write(&self, view: ViewId, path: &str) {
        self.state
            .borrow_mut()
            .mcp_writable
            .entry(view)
            .or_default()
            .insert(path.to_owned());
    }

    /// `registerSheetsSession`'s tab: the shell made the view; the first one
    /// also starts the pasted-temp cleanup, as TS does
    pub fn register_tab(&self, view: ViewId) {
        let added = {
            let mut state = self.state.borrow_mut();
            let added = state.tab(view).is_none();
            if added {
                state.tabs.push((view, TabState::new()));
                state.active_view = Some(view);
            }
            added
        };
        if added {
            self.start_pasted_temp_cleanup();
        }
    }

    /// `discardSheetsRecovery`: the same as answering "Don't Save"
    pub fn discard_recovery(&self, view: ViewId) {
        if !self.env.alive(view) {
            return;
        }
        self.discard_tab_copies(view);
    }

    /// `requestSheetsClose`: true means proceed with the close
    pub fn request_close(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>) {
        let count = self.pending_edit_count(view);
        let decision = close_guard_decision(count, !self.env.alive(view), shutting_down(self));
        if decision == CloseGuardDecision::Proceed {
            return then(Ok(true));
        }
        let tm = &self.env.tm;
        let options = message_box(
            "warning",
            tm.with("closeUnsavedMsg", &[("count", JsonValue::Number(count))]),
            Some(tm.t("closeUnsavedDetail")),
            vec![tm.t("menuSave"), tm.t("btnDontSave"), tm.t("btnCancel")],
            0,
            Some(2),
        );
        let response = self.env.dialogs.message(parent, &options).response;
        if response == 2 {
            return then(Ok(false));
        }
        if response == 1 {
            self.discard_tab_copies(view);
            return then(Ok(true));
        }
        // The window went away (or a quit started) while the prompt was up: don't save
        if shutting_down(self) || !self.env.alive(view) {
            return then(Ok(true));
        }
        self.await_save(view, then);
    }

    /// Asks the renderer to run its journal save and resumes `then` with the
    /// outcome, or `false` after two minutes
    pub fn await_save(&self, view: ViewId, then: Then<bool>) {
        let token = {
            let mut state = self.state.borrow_mut();
            state.next_token += 1;
            let token = state.next_token;
            state.close_waiters.insert(
                view,
                CloseWaiter {
                    token,
                    resume: Box::new(move |ok| then(Ok(ok))),
                },
            );
            token
        };
        let env = self.env.clone();
        self.env.shell.after(
            CLOSE_SAVE_TIMEOUT_MS,
            Box::new(move || {
                env.post(move |core| {
                    let expired = {
                        let mut state = core.state.borrow_mut();
                        match state.close_waiters.get(&view) {
                            Some(waiter) if waiter.token == token => {
                                state.close_waiters.remove(&view)
                            }
                            _ => None,
                        }
                    };
                    if let Some(waiter) = expired {
                        (waiter.resume)(false);
                    }
                })
            }),
        );
        self.send(view, "workbook:close-save-request", Vec::new());
    }

    /// A Save As the shell asked for: the renderer is told with `menu:action`
    /// (`sendSheetsMenuAction('save-as')`) and runs its Save As, whose own
    /// `workbook:save` of mode `save-as` resumes `then` ([`Core::save_as_finished`]).
    /// A newer request replaces an older one (the older is not saved), and
    /// two minutes without a request from the renderer end it as not saved.
    pub fn await_save_as(&self, view: ViewId, then: Then<bool>) {
        let (token, older) = {
            let mut state = self.state.borrow_mut();
            state.next_token += 1;
            let token = state.next_token;
            let older = state.save_as_waiters.insert(
                view,
                CloseWaiter {
                    token,
                    resume: Box::new(move |ok| then(Ok(ok))),
                },
            );
            (token, older)
        };
        if let Some(older) = older {
            (older.resume)(false);
        }
        let env = self.env.clone();
        self.env.shell.after(
            CLOSE_SAVE_TIMEOUT_MS,
            Box::new(move || env.post(move |core| core.save_as_expired(view, token))),
        );
        self.send(view, "menu:action", vec![string("save-as")]);
    }

    /// The two minutes of Save As `token` are over; a Save As already
    /// answered (or replaced, or ended with its view) has no timer left to
    /// act, so only the waiter this timer was set for is dropped.
    fn save_as_expired(&self, view: ViewId, token: u64) {
        let expired = {
            let mut state = self.state.borrow_mut();
            match state.save_as_waiters.get(&view) {
                Some(waiter) if waiter.token == token => state.save_as_waiters.remove(&view),
                _ => None,
            }
        };
        if let Some(waiter) = expired {
            (waiter.resume)(false);
        }
    }

    /// The Save As the view waits for now, as a `workbook:save` of mode
    /// `save-as` arrives: that save answers it and no later one
    pub fn save_as_token(&self, view: ViewId) -> Option<u64> {
        self.state
            .borrow()
            .save_as_waiters
            .get(&view)
            .map(|waiter| waiter.token)
    }

    /// The `workbook:save` that arrived for Save As `token` ended: `saved`
    /// when the file was written (`canceled: false` in the answer), not when
    /// the person canceled (`canceled: true`) or the save failed (it
    /// rejected). A Save As that ended meanwhile (its two minutes, a newer
    /// request, its view) is not answered again, and a newer one is left
    /// waiting for its own save.
    pub fn save_as_finished(&self, view: ViewId, token: u64, saved: bool) {
        let waiter = {
            let mut state = self.state.borrow_mut();
            match state.save_as_waiters.get(&view) {
                Some(waiter) if waiter.token == token => state.save_as_waiters.remove(&view),
                _ => None,
            }
        };
        if let Some(waiter) = waiter {
            (waiter.resume)(saved);
        }
    }

    /// `writeSheetsCopiesNow`: every live tab's renderer writes its copy now;
    /// `done` runs once all answered or timed out
    pub fn write_copies_now(&self, done: Box<dyn FnOnce(Result<(), main_core::shell::renderer_disposal::Failure>)>) {
        let mut unavailable = false;
        let views: Vec<ViewId> = self.tab_views().into_iter()
            .filter(|view| {
                let crashed = self.state.borrow().tab_recovery.get(view).is_some_and(|r| r.crashed);
                let ready = self.env.alive(*view) && !crashed;
                unavailable |= !ready;
                ready
            })
            .collect();
        let initial_failure = unavailable.then(|| main_core::shell::renderer_disposal::Failure("A workbook view is unavailable".into()));
        if views.is_empty() {
            return done(initial_failure.map_or(Ok(()), Err));
        }
        let remaining = std::rc::Rc::new(std::cell::Cell::new(views.len()));
        let done = std::rc::Rc::new(std::cell::RefCell::new(Some(done)));
        let failure = std::rc::Rc::new(std::cell::RefCell::new(initial_failure));
        for view in views {
            let nonce = self.env.uuid();
            let settle = {
                let (remaining, done) = (remaining.clone(), done.clone());
                let failure = failure.clone();
                move |result: Result<(), main_core::shell::renderer_disposal::Failure>| {
                    if let Err(error) = result {
                        let mut first = failure.borrow_mut();
                        if first.is_none() { *first = Some(error); }
                    }
                    remaining.set(remaining.get() - 1);
                    if remaining.get() == 0 {
                        let callback = done.borrow_mut().take();
                        let result = failure.borrow_mut().take().map_or(Ok(()), Err);
                        if let Some(done) = callback { done(result); }
                    }
                }
            };
            let once = std::rc::Rc::new(std::cell::RefCell::new(Some(settle)));
            let on_answer = {
                let once = once.clone();
                move |result| {
                    let callback = once.borrow_mut().take();
                    if let Some(settle) = callback {
                        settle(result);
                    }
                }
            };
            self.state
                .borrow_mut()
                .write_now_waiters
                .insert(nonce.clone(), (view, Box::new(on_answer)));
            let (env, key) = (self.env.clone(), nonce.clone());
            self.env.shell.after(
                RECOVERY_WRITE_NOW_TIMEOUT_MS,
                Box::new(move || {
                    env.post(move |core| {
                        let waiter = core.state.borrow_mut().write_now_waiters.remove(&key);
                        if let Some((_, answer)) = waiter {
                            answer(Err(main_core::shell::renderer_disposal::Failure("Timed out writing the workbook recovery copy".into())));
                        }
                    })
                }),
            );
            self.send(view, "workbook:recovery-write-now", vec![string(nonce)]);
        }
    }

    /// `sheetsFileRenamed`: an open view's file was renamed outside the editor
    pub fn file_renamed(&self, view: ViewId, old_path: &str, new_path: &str) {
        if self.rename_sessions(view, old_path, new_path, None) {
            self.send(view, "workbook:renamed", vec![string(basename(new_path, None))]);
        }
    }

    fn rename_sessions(&self, view: ViewId, old_path: &str, new_path: &str, existing: Option<&std::collections::HashSet<String>>) -> bool {
        // A user-chosen name always wins: the file no longer qualifies for auto-rename
        let stem = |p: &str| {
            let name = basename(p, None);
            match name.rfind('.') {
                Some(at) if at + 1 < name.len() => {
                    let head = &name[..at];
                    let stripped = head
                        .strip_suffix(')')
                        .and_then(|rest| rest.rfind(" ("))
                        .filter(|open| {
                            head[open + 2..head.len() - 1]
                                .chars()
                                .all(|c| c.is_ascii_digit())
                                && head.len() - 1 > open + 2
                        })
                        .map_or(head, |open| &head[..open]);
                    format!("{stripped}{}", &name[at..])
                }
                _ => name,
            }
        };
        {
            let mut state = self.state.borrow_mut();
            if state.untitled_workbook_paths.remove(old_path) && stem(new_path) == stem(old_path) {
                state.untitled_workbook_paths.insert(new_path.to_owned());
            }
        }
        let sessions: Vec<(String, SessionInfo)> = match self.state.borrow().tab(view) {
            Some(tab) => tab.sessions.clone(),
            None => return false,
        };
        let mut matched = false;
        for (id, session) in sessions {
            let Some(map) = session_after_rename(&session.to_map(), old_path, new_path) else {
                continue;
            };
            let mut renamed = session.with_renamed_fields(&map);
            // An unsaved new workbook's tab renames its hidden backing file
            if session.unsaved_new
                && session.path == old_path
                && let Some(suggest) = &session.suggest_save_as
            {
                let suggested = join(&[&dirname(suggest), &basename(new_path, None)]);
                if &suggested != suggest
                    && !existing.map_or_else(|| Path::new(&suggested).exists(), |paths| paths.contains(&suggested))
                    && !self.suggested_path_taken(&suggested)
                {
                    renamed.suggest_save_as = Some(suggested);
                }
            }
            // A rename moves the same bytes: the split-view disk snapshot follows the path
            if let Some((path, state)) = &renamed.disk_state
                && path == old_path
            {
                renamed.disk_state = Some((new_path.to_owned(), state.clone()));
            }
            if let Some(tab) = self.state.borrow_mut().tab_mut(view) {
                tab.set_session(&id, renamed);
            }
            matched = true;
        }
        if matched {
            if let Some(tab) = self.state.borrow_mut().tab_mut(view)
                && tab.current_path.as_deref() == Some(old_path) {
                tab.set_current_path(Some(new_path.to_owned()));
            }
        }
        matched
    }

    fn rename_existing_paths(&self, view: ViewId, old_path: &str, new_path: &str) -> std::collections::HashSet<String> {
        let candidates: Vec<String> = self.state.borrow().tab(view).map(|tab| tab.sessions.iter()
            .filter_map(|(_, session)| {
                if !session.unsaved_new || session.path != old_path { return None; }
                session.suggest_save_as.as_ref().map(|suggest| join(&[&dirname(suggest), &basename(new_path, None)]))
            }).collect()).unwrap_or_default();
        candidates.into_iter().filter(|path| Path::new(path).exists()).collect()
    }

    /// The end of a view: the tab's sessions close, its copies settle, and
    /// what waited on it stops waiting
    pub fn view_gone(&self, view: ViewId) {
        if self.state.borrow().tab(view).is_some_and(|tab| tab.owned_registration) {
            return;
        }
        let openings: Vec<_> = self.state.borrow().recovery_open_waiters.iter()
            .filter_map(|(id, waiter)| (waiter.view == view).then_some(id.clone()))
            .collect();
        for id in openings {
            self.finish_recovery_open(&id, Err("The workbook view was closed".into()));
        }
        self.cancel_owned_saves(view, "The target workbook was closed");
        let document = self.state.borrow().tab(view).map(|tab| tab.document.clone());
        if let Some(document) = document { document.revoke(); }
        {
            let mut state = self.state.borrow_mut();
            if state.pending_new_blank.as_ref().and_then(|entry| entry.target.as_ref()).is_some_and(|(target, _)| *target == view) {
                if let Some(entry) = state.pending_new_blank.take() { entry.identity.set(QueueOutcome::Retired); }
                state.blank_revision = std::rc::Rc::new(());
            }
        }
        // registerSheetsSession's 'destroyed'
        let has_tab = self.state.borrow().tab(view).is_some();
        if has_tab {
            self.state
                .borrow_mut()
                .save_as_export_targets
                .forget(f64::from(view.0));
            self.settle_tab_recovery_on_close(view);
            let tab = {
                let mut state = self.state.borrow_mut();
                let at = state.tabs.iter().position(|(id, _)| *id == view);
                at.map(|at| state.tabs.remove(at).1)
            };
            if let Some(mut tab) = tab {
                // Free pending chunked-save uploads with the tab
                tab.save_transfers.dispose();
                let sessions = std::mem::take(&mut tab.sessions);
                let env = self.env.clone();
                self.env.spawn(move || {
                    for (id, session) in sessions {
                        cleanup_session(&env, &id, &session, session.import_temp_dir.as_deref());
                    }
                });
            }
        } else {
            self.state.borrow_mut().tab_recovery.remove(&view);
            self.state
                .borrow_mut()
                .save_as_export_targets
                .forget(f64::from(view.0));
        }
        {
            let mut state = self.state.borrow_mut();
            if state.active_view == Some(view) {
                state.active_view = None;
            }
            if let Some(entry) = state.remove_queued_workbook(view) {
                entry.identity.set(QueueOutcome::Retired);
            }
            state.mcp_writable.remove(&view);
        }
        let path_saves = {
            let mut state = self.state.borrow_mut();
            let tokens: Vec<_> = state.path_save_waiters.iter()
                .filter_map(|(token, (owner, _))| (*owner == view).then_some(*token))
                .collect();
            tokens.into_iter().filter_map(|token| state.path_save_waiters.remove(&token))
                .collect::<Vec<_>>()
        };
        for (_, then) in path_saves {
            then(Err(thrown("the target workbook is no longer open")));
        }
        let recovery_waiters = {
            let mut state = self.state.borrow_mut();
            let nonces: Vec<_> = state.write_now_waiters.iter()
                .filter_map(|(nonce, (owner, _))| (*owner == view).then_some(nonce.clone()))
                .collect();
            nonces.into_iter().filter_map(|nonce| state.write_now_waiters.remove(&nonce))
                .collect::<Vec<_>>()
        };
        for (_, finish) in recovery_waiters {
            finish(Err(main_core::shell::renderer_disposal::Failure("The workbook view was closed".into())));
        }
        // a Save As the shell asked for ends with its view
        let save_as = self.state.borrow_mut().save_as_waiters.remove(&view);
        if let Some(waiter) = save_as {
            (waiter.resume)(false);
        }
        // the pending-edits tracker's 'destroyed'
        let tracked = self.state.borrow_mut().tracked_edit_senders.remove(&view);
        if tracked {
            self.state.borrow_mut().pending_edit_counts.remove(&view);
            let waiter = self.state.borrow_mut().close_waiters.remove(&view);
            if let Some(waiter) = waiter {
                (waiter.resume)(false);
            }
        }
    }

    /// The first tab of a process starts the sweep of old pasted images
    pub fn start_pasted_temp_cleanup(&self) {
        if std::mem::replace(&mut self.state.borrow_mut().pasted_cleanup_started, true) {
            return;
        }
        cleanup_expired_pasted_files(&self.env.temp_dir(), self.env.shell.now_ms());
    }
}

/// The sheets editors of every view, as other batches see them
pub struct SheetsEditors {
    pub core: Core,
    pub provided: std::rc::Weak<shell_contract::provided::Provided>,
}

fn unsupported(what: &str) -> IpcError {
    thrown(format!("The sheets editor has no {what}."))
}

impl DocumentEditors for SheetsEditors {
    fn saved_surface(&self) -> shell_contract::Surface { shell_contract::Surface::Sheets }
    fn owns(&self, view: ViewId) -> bool {
        self.core.state.borrow().tab(view).is_some()
    }
    fn prepare_saved_document(
        &self, target: shell_contract::ipc::NativeSavedTarget,
    ) -> Result<Box<dyn shell_contract::editors::PreparedSavedDocument>, IpcError> {
        if target.surface() != shell_contract::Surface::Sheets {
            return Err(thrown("The saved target is not a workbook"));
        }
        let registration = SheetsViews(self.core.clone()).prepare_tab_registration(
            target.view(), target.created().clone(), target.reservation(), target.browser(),
        ).map_err(thrown)?;
        let slot = registration.tab.as_ref().ok_or_else(|| thrown("The workbook registration was consumed"))?.saved_load.clone();
        let receipt = std::rc::Rc::new(SavedWorkbookReceipt {
            actor: std::cell::RefCell::new(registration.prepare_receipt()),
            committed: std::cell::Cell::new(false), cancelled: std::cell::Cell::new(false),
        });
        Ok(Box::new(PreparedSavedWorkbook {
            registration: Some(registration), slot,
            load: Some(super::core::SavedLoad::new(target, self.core.env.uuid())),
            receipt,
        }))
    }
    fn load_saved_document(
        &self, document: shell_contract::ipc::NativeSavedDocument,
        then: Then<shell_contract::editors::SavedDocumentLoadOutcome>,
    ) {
        let view = document.target().view();
        let prepared = (|| -> Result<_, IpcError> {
            let state = self.core.state.borrow();
            let tab = state.tab(view).ok_or_else(|| thrown("The workbook tab was closed"))?;
            let slot = tab.saved_load.clone();
            let id = {
                let saved = slot.borrow();
                let saved = saved.as_ref().ok_or_else(|| thrown("The saved workbook barrier is unavailable"))?;
                if !saved.target.same_target(document.target()) {
                    return Err(thrown("The saved workbook belongs to another target"));
                }
                if let Some(error) = &saved.failure { return Err(thrown(error.clone())); }
                saved.id.clone()
            };
            Ok((slot, id))
        })();
        let (slot, id) = match prepared { Ok(value) => value, Err(error) => return then(Err(error)) };
        let target = document.target();
        let mut losses = match target.output().loss_tokens() {
            Ok(losses) => losses,
            Err(error) => return then(Err(thrown(error))),
        };
        let Some(owner_loss) = document.owner().loss_token() else {
            return then(Err(thrown("The saved workbook owner has no lifetime")));
        };
        losses.extend([owner_loss, target.created().loss_token(), target.cancel().clone(), target.pending().clone()]);
        losses.extend(target.additional().iter().cloned());
        let subscriptions: Vec<_> = losses.into_iter().map(|loss| {
            let env = self.core.env.clone();
            let id = id.clone();
            loss.on_cancel(move || env.post(move |core| core.cancel_saved_open(view, &id, "The saved workbook opening was cancelled")))
        }).collect();
        let Some(effects) = self.core.env.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>() else {
            return then(Err(thrown("The saved workbook effects are unavailable")));
        };
        let extra: Vec<_> = target.additional().iter().collect();
        let mut then = Some(then);
        let mut subscriptions = Some(subscriptions);
        let admitted = effects.with_native_saved_document_input(
            &document, target.participants(), target.cancel(), target.pending(), &extra, &mut |_| {
                let state = self.core.state.borrow();
                let tab = state.tab(view).ok_or("The workbook tab was closed")?;
                if tab.browser_closing || !std::rc::Rc::ptr_eq(&tab.saved_load, &slot)
                    || !tab.browser_owner.as_ref().is_some_and(|owner| owner.same_owner(target.browser()))
                    || !document.owner().context.document_lease().is_some_and(|lease| lease.same_generation(&tab.document)) {
                    return Err("The saved workbook actor was replaced".into());
                }
                let mut saved = slot.borrow_mut();
                let saved = saved.as_mut().ok_or("The saved workbook barrier was released")?;
                if saved.id != id || !saved.target.same_target(target) || saved.document.is_some() || saved.failure.is_some() {
                    return Err("The saved workbook opening changed".into());
                }
                let completion = then.take().ok_or("The saved workbook completion was already consumed")?;
                let subscriptions = subscriptions.take().ok_or("The saved workbook subscriptions were already consumed")?;
                saved.document = Some(document.clone());
                saved.completion = Some(completion);
                saved.subscriptions = subscriptions;
                Ok(())
            },
        );
        if let Err(error) = admitted {
            if let Some(then) = then { then(Err(thrown(error))); }
            return;
        }
        self.core.start_saved_open(view);
    }


    fn capture_document_owner(
        &self, view: ViewId, mut native: shell_contract::ipc::NativeRequestOwner,
    ) -> Result<shell_contract::ipc::NativeRequestOwner, IpcError> {
        let state = self.core.state.borrow();
        let tab = state.tab(view).ok_or_else(|| thrown("The workbook tab was closed"))?;
        if native.view != view || !tab.document.is_active() {
            return Err(thrown("The workbook document capability does not match"));
        }
        if let Some(captured) = native.context.document_lease() {
            if !captured.same_generation(&tab.document) {
                return Err(thrown("The workbook document was replaced"));
            }
        }
        let actual = native.context.renderer();
        let saved = tab.saved_load.borrow();
        let expected = tab.native_owner.as_ref().and_then(|owner| owner.context.renderer())
            .or_else(|| saved.as_ref().filter(|load| tab.browser_owner.as_ref()
                .is_some_and(|owner| owner.same_owner(load.target.browser())))
                .map(|load| load.target.created()));
        if !actual.zip(expected).is_some_and(|(actual, expected)| actual.same_generation(expected)) {
            return Err(thrown("The workbook renderer was replaced"));
        }
        native.context = native.context.with_document_lease(tab.document.clone());
        Ok(native)
    }

    /// The renderer runs its journal save and answers on `workbook:close-save-result`
    /// (the save half of `requestSheetsClose`, without the prompt)
    ///
    /// `SaveMode::SaveAs` always asks for a file: the renderer is told to run
    /// its Save As and the answer is that save's outcome
    fn request_save(&self, view: ViewId, mode: SaveMode, then: Then<bool>) {
        if !self.core.env.alive(view) {
            return then(Ok(false));
        }
        match mode {
            SaveMode::Save => self.core.await_save(view, then),
            SaveMode::SaveAs => self.core.await_save_as(view, then),
        }
    }

    fn request_save_owned(
        &self, view: ViewId, mode: SaveMode,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken, then: Then<bool>,
    ) {
        self.core.request_owned_save(view, mode, None, crate::owned_save::SaveAuthority::Renderer(owner), cancel, None, None, then);
    }

    fn request_save_native(
        &self, view: ViewId, mode: SaveMode, binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        if !matches!(binding.operation().selection(), shell_contract::ipc::NativeInvocationSelection::NativeCommand(
            shell_contract::ipc::NativeEditorCommand::SaveDocument {
                surface: shell_contract::Surface::Sheets, view: selected,
                selection: shell_contract::ipc::NativeSaveSelection::Mode(selected_mode),
            }) if *selected == view && *selected_mode == mode) {
            return then(Err(thrown("The native workbook save selection changed")));
        }
        self.core.request_owned_save(view, mode, None, crate::owned_save::SaveAuthority::Native(binding),
            cancel, Some(pending), None, then);
    }

    fn save_to_path_owned(
        &self, view: ViewId, path: PathBuf,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken, then: Then<()>,
    ) {
        let Some(target) = path.to_str().filter(|_| path.is_absolute()) else {
            return then(Err(thrown("save_sheet requires an absolute path")));
        };
        self.core.request_owned_save(
            view, SaveMode::SaveAs, Some(target.to_owned()), crate::owned_save::SaveAuthority::Renderer(owner), cancel, None, None,
            Box::new(move |result| then(result.and_then(|saved| {
                if saved { Ok(()) } else { Err(thrown("The workbook was not saved")) }
            }))),
        );
    }

    fn save_to_path_native(
        &self, view: ViewId, path: PathBuf, binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        let Some(target) = path.to_str().filter(|_| path.is_absolute()) else {
            return then(Err(thrown("save_sheet requires an absolute path")));
        };
        if !matches!(binding.operation().selection(), shell_contract::ipc::NativeInvocationSelection::NativeCommand(
            shell_contract::ipc::NativeEditorCommand::SaveDocument {
                surface: shell_contract::Surface::Sheets, view: selected,
                selection: shell_contract::ipc::NativeSaveSelection::ExactPath(selected_path),
            }) if *selected == view && selected_path == &path) {
            return then(Err(thrown("The native workbook save selection changed")));
        }
        self.core.request_owned_save(view, SaveMode::SaveAs, Some(target.to_owned()),
            crate::owned_save::SaveAuthority::Native(binding), cancel, Some(pending), None,
            Box::new(move |result| then(result.and_then(|saved| {
                if saved { Ok(()) } else { Err(thrown("The workbook was not saved")) }
            }))));
    }

    fn request_close(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>) {
        self.core.request_close(view, parent, then);
    }

    fn request_close_owned(
        &self, view: ViewId, parent: Option<WindowId>,
        origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken, then: Then<bool>,
    ) {
        self.core.request_close_owned(view, parent, origin, cancel, then);
    }

    fn read_text(&self, _view: ViewId, then: Then<String>) {
        then(Err(unsupported("text export")));
    }

    fn save_to_path(&self, view: ViewId, path: PathBuf, then: Then<()>) {
        use jscompat::JsObject;
        use main_core::shell::mcp::tools::sheets_tools::SheetsCommandName;
        use shell_ipc_a::mcp::McpBridgeControl;

        if !self.owns(view) || !self.core.env.alive(view) {
            return then(Err(thrown("the target workbook is no longer open")));
        }
        let Some(target) = path.to_str().filter(|_| path.is_absolute()).map(str::to_owned) else {
            return then(Err(thrown("save_sheet requires an absolute path")));
        };
        let Some(bridge) = self.provided.upgrade()
            .and_then(|provided| provided.all::<dyn McpBridgeControl>().into_iter().next())
        else {
            return then(Err(thrown("the workbook MCP bridge is unavailable")));
        };
        let mut payload = JsObject::new();
        payload.set("path", JsonValue::String(target.clone()));
        payload.set("overwrite", JsonValue::Bool(true));
        let core = self.core.clone();
        bridge.sheets_run_command(i64::from(view.0), SheetsCommandName::SaveSheet,
            Some(JsonValue::Object(payload)), Box::new(move |result| {
                let result = match result {
                    Err(error) => return then(Err(error)),
                    Ok(Some(JsonValue::Object(result))) => result,
                    _ => return then(Err(thrown("the workbook save returned no result"))),
                };
                if result.get("ok") != Some(&JsonValue::Bool(true))
                    || result.get("path") != Some(&JsonValue::String(target))
                {
                    return then(Err(thrown("the workbook save returned an invalid result")));
                }
                if !core.env.alive(view) {
                    return then(Err(thrown("the target workbook is no longer open")));
                }
                let token = {
                    let mut state = core.state.borrow_mut();
                    state.next_token = state.next_token.wrapping_add(1);
                    let token = state.next_token;
                    state.path_save_waiters.insert(token, (view, then));
                    token
                };
                let env = core.env.clone();
                let worker = env.clone();
                env.spawn(move || {
                    let result = std::fs::metadata(path)
                        .map_err(|error| format!("saved workbook is unavailable: {error}"))
                        .and_then(|metadata| if metadata.is_file() {
                            Ok(())
                        } else {
                            Err("saved workbook is not a file".to_owned())
                        });
                    worker.post(move |core| {
                        let waiter = core.state.borrow_mut().path_save_waiters.remove(&token);
                        if let Some((view, then)) = waiter {
                            if !core.env.alive(view) {
                                then(Err(thrown("the target workbook is no longer open")));
                            } else {
                                then(result.map_err(thrown));
                            }
                        }
                    });
                });
            }));
    }

    fn file_renamed(&self, view: ViewId, old_path: &Path, new_path: &Path) {
        self.core.file_renamed(
            view,
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
        );
    }

    fn file_renamed_owned(
        &self, view: ViewId, old_path: &Path, new_path: &Path,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken, then: Then<()>,
    ) {
        self.file_renamed_many_owned_extra(view, old_path, new_path, origin, destination, &[], cancel, pending, &[], then);
    }

    fn file_renamed_many_owned_extra(
        &self, view: ViewId, old_path: &Path, new_path: &Path,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken, additional: &[shell_contract::runtime::CancelToken], then: Then<()>,
    ) {
        let capture = (|| {
            let document = destination.context.document_lease()
                .cloned().ok_or_else(|| thrown("The rename has no captured workbook document"))?;
            let destination = self.capture_document_owner(view, destination.clone())?;
            let effects = self.core.env.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
                .cloned().ok_or_else(|| thrown("Native owned save effects are unavailable"))?;
            let scope = crate::owned_save::OwnedSave {
                authority: crate::owned_save::SaveAuthority::Renderer(origin.clone()), destination, cancel: cancel.clone(),
                pending: pending.clone(), document, effects, additional: additional.to_vec(), callback_owner: None, prior_callback_owners: Vec::new(), revision: None, revision_snapshot: None,
            };
            let old = old_path.to_str().ok_or_else(|| thrown("The old workbook path is not UTF-8"))?;
            let new = new_path.to_str().ok_or_else(|| thrown("The new workbook path is not UTF-8"))?;
            let existing = self.core.rename_existing_paths(view, old, new);
            let changed = scope.effect_many(participants, || Ok(self.core.rename_sessions(view, old, new, Some(&existing))))?;
            Ok((scope, changed, basename(new, None)))
        })();
        let (scope, changed, name) = match capture {
            Ok(capture) => capture,
            Err(error) => return then(Err(error)),
        };
        if !changed { return then(Ok(())); }
        let mut destinations = participants.to_vec();
        let target = destinations.len();
        destinations.push(scope.destination.clone());
        let extra = additional.iter().collect::<Vec<_>>();
        then(scope.effects.emit_owned_many_extra(
            origin, &destinations, target, &scope.cancel, &scope.pending, &extra,
            "workbook:renamed", vec![string(name)],
        ).map_err(thrown));
    }

    fn file_renamed_existing_document(
        &self, view: ViewId, old_path: &Path, new_path: &Path,
        authority: &shell_contract::ipc::NativeSaveRevisionAuthority, expected_path: &Path,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken, additional: &[shell_contract::runtime::CancelToken], then: Then<Option<shell_contract::ipc::NativeSaveRevisionReceipt>>,
    ) {
        let result = (|| -> Result<(), IpcError> {
            let snapshot = authority.snapshot().map_err(thrown)?;
            if snapshot.selection().path() != expected_path { return Err(thrown("The native save path changed")); }
            let destination = self.capture_document_owner(view, destination.clone())?;
            let effects = self.core.env.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
                .cloned().ok_or_else(|| thrown("Native save effects are unavailable"))?;
            let old = old_path.to_str().ok_or_else(|| thrown("The old workbook path is not UTF-8"))?;
            let new = new_path.to_str().ok_or_else(|| thrown("The new workbook path is not UTF-8"))?;
            let existing = self.core.rename_existing_paths(view, old, new);
            let mut owners = participants.to_vec();
            let target = owners.len();
            owners.push(destination);
            let mut changed = false;
            let additional = additional.iter().collect::<Vec<_>>();
            effects.with_native_save_revision_snapshot_effect(&snapshot, &owners, cancel, pending, &additional, &mut || {
                changed = self.core.rename_sessions(view, old, new, Some(&existing));
                Ok(())
            }).map_err(thrown)?;
            if changed {
                effects.emit_native_save_revision_snapshot(&snapshot, &owners, target, cancel, pending,
                    &additional, "workbook:renamed", vec![string(basename(new, None))]).map_err(thrown)?;
            }
            Ok(())
        })();
        then(result.map(|()| None));
    }

    fn file_renamed_recovery(
        &self, view: ViewId, old_path: &Path, new_path: &Path,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken, then: Then<()>,
    ) {
        let result = (|| -> Result<(), IpcError> {
            let destination = self.capture_document_owner(view, destination.clone())?;
            let effects = self.core.env.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
                .cloned().ok_or_else(|| thrown("Native recovery effects are unavailable"))?;
            let old = old_path.to_str().ok_or_else(|| thrown("The old workbook path is not UTF-8"))?;
            let new = new_path.to_str().ok_or_else(|| thrown("The new workbook path is not UTF-8"))?;
            let existing = self.core.rename_existing_paths(view, old, new);
            let mut owners = participants.to_vec();
            let target = owners.len();
            owners.push(destination);
            let mut changed = false;
            effects.with_recovery_open_effect_many(receipt, receiver, &owners, cancel, pending, &mut || {
                changed = self.core.rename_sessions(view, old, new, Some(&existing));
                Ok(())
            }).map_err(thrown)?;
            if changed {
                effects.emit_recovery_open_many(receipt, receiver, &owners, target, cancel, pending,
                    "workbook:renamed", vec![string(basename(new, None))]).map_err(thrown)?;
            }
            Ok(())
        })();
        then(result);
    }

    /// `sendSheetsMenuAction`: `menu:action` with `export-<format>`
    fn export_request(&self, view: ViewId, format: &str) {
        self.core.send(
            view,
            "menu:action",
            vec![IpcValue::String(format!("export-{format}"))],
        );
    }

    /// the file this tab shows (`SheetsTabSession.currentPath`)
    fn document_path(&self, view: ViewId) -> Option<PathBuf> {
        self.core
            .state
            .borrow()
            .tab(view)
            .and_then(|tab| tab.current_path.clone())
            .map(PathBuf::from)
    }
}

thread_local! {
    static VIEWS: std::cell::RefCell<Option<SheetsViews>> = const { std::cell::RefCell::new(None) };
}

/// What the shell asks of the sheets tabs besides the renderer's own
/// channels: the exported functions of sheets-main.ts that no contract trait
/// names (`registerSheetsSession`, `queueWorkbookForView`, `setSheetsNewBlank`,
/// `markSheetsUntitledPath`, `authorizeMcpSheetWrite`, `writeSheetsCopiesNow`,
/// `markSheetsShuttingDown`, `resetSheetsShuttingDown`, `discardSheetsRecovery`).
/// Core thread only.
#[derive(Clone)]
pub struct SheetsViews(pub(super) Core);

pub struct PreparedWorkbookQueue {
    core: Core,
    view: ViewId,
    entry: QueuedWorkbook,
    commit_revision: std::rc::Rc<()>,
    rollback_revision: std::rc::Rc<()>,
    revision: std::rc::Rc<()>,
    document: shell_contract::runtime::RendererGeneration,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
}

pub struct ValidatedWorkbookQueue<'a, 'b> {
    prepared: PreparedWorkbookQueue,
    successor: &'a shell_contract::ipc::NativeBrowserOwner,
    _transition: &'a shell_contract::ipc::NativeBrowserTransition<'b>,
}

pub struct WorkbookQueueReceipt {
    core: Core,
    view: ViewId,
    document: shell_contract::runtime::RendererGeneration,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    inserted: std::rc::Rc<std::cell::Cell<QueueOutcome>>,
    previous: std::cell::RefCell<Option<QueuedWorkbook>>,
    rollback_revision: std::rc::Rc<()>,
    closed: std::cell::Cell<bool>,
    completed: std::cell::Cell<bool>,
}

impl PreparedWorkbookQueue {
    pub fn validate_in_transition<'a, 'b>(self, transition: &'a shell_contract::ipc::NativeBrowserTransition<'b>, successor: &'a shell_contract::ipc::NativeBrowserOwner) -> Result<ValidatedWorkbookQueue<'a, 'b>, String> {
        if !transition.matches(&self.reservation) || !transition.matches_owner(&self.owner)
            || !(transition.matches_owner(successor) || transition.proposes_owner(successor)) {
            return Err("The workbook queue browser owner changed".into());
        }
        let state = self.core.state.borrow();
        let tab = state.tab(self.view).ok_or("The workbook tab was closed")?;
        let current = self.core.env.services.views.renderer_generation(self.view);
        if tab.browser_closing || !tab.document.same_generation(&self.document)
            || !std::rc::Rc::ptr_eq(&tab.queue_revision, &self.revision)
            || !self.created.is_active()
            || !current.as_ref().is_some_and(|current| current.same_generation(&self.created)) {
            return Err("The workbook queue changed before publication".into());
        }
        drop(state);
        Ok(ValidatedWorkbookQueue { prepared: self, successor, _transition: transition })
    }
}

impl ValidatedWorkbookQueue<'_, '_> {
    pub fn commit_in_transition(self) -> WorkbookQueueReceipt {
        let Self { prepared, successor, .. } = self;
        let PreparedWorkbookQueue { core, view, entry, commit_revision, rollback_revision, document, created, reservation, .. } = prepared;
        let mut state = core.state.borrow_mut();
        let inserted = entry.identity.clone();
        let previous = state.set_queued_workbook_prepared(view, Some(entry), commit_revision);
        drop(state);
        WorkbookQueueReceipt { core, view, document, created, reservation, owner: successor.clone(), inserted, previous: std::cell::RefCell::new(previous), rollback_revision, completed: std::cell::Cell::new(false), closed: std::cell::Cell::new(false) }
    }
}

impl WorkbookQueueReceipt {
    pub fn retire_closed(&self, proof: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created) { return Err("The close receipt belongs to another workbook".into()); }
        self.closed.set(true);
        Ok(())
    }

    pub fn retire_adopted(&self, proof: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created) { return Err("The close receipt belongs to another workbook".into()); }
        if proof.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed { return Err("An adopted workbook requires a closed browser".into()); }
        if self.completed.get() { return Ok(()); }
        let mut state = self.core.state.borrow_mut();
        if state.queued_workbooks.get(&self.view).is_some_and(|entry| std::rc::Rc::ptr_eq(&entry.identity, &self.inserted)) {
            state.set_queued_workbook_prepared(self.view, None, self.rollback_revision.clone());
            self.inserted.set(QueueOutcome::Retired);
        }
        self.previous.borrow_mut().take();
        self.completed.set(true);
        Ok(())
    }

    pub fn retire_superseded(&self, proof: &shell_contract::ipc::NativeBrowserSuperseded) -> Result<(), String> {
        if !proof.matches(&self.reservation, &self.owner) { return Err("The browser replacement belongs to another workbook".into()); }
        if self.completed.get() { return Ok(()); }
        let mut state = self.core.state.borrow_mut();
        if state.queued_workbooks.get(&self.view).is_some_and(|entry| std::rc::Rc::ptr_eq(&entry.identity, &self.inserted)) {
            state.set_queued_workbook_prepared(self.view, None, self.rollback_revision.clone());
            self.inserted.set(QueueOutcome::Retired);
        }
        self.previous.borrow_mut().take();
        self.completed.set(true);
        Ok(())
    }

    pub fn rollback(&self) -> Result<(), String> {
        if self.completed.get() { return Ok(()); }
        let mut state = self.core.state.borrow_mut();
        if self.inserted.get() == QueueOutcome::Retired {
            return Err("The workbook queue was removed before rollback".into());
        }
        if self.inserted.get() == QueueOutcome::Consumed {
            self.previous.borrow_mut().take();
            self.completed.set(true);
            return Ok(());
        }
        let tab = state.tab(self.view).ok_or("The workbook tab was closed before rollback")?;
        let current = self.core.env.services.views.renderer_generation(self.view);
        if !tab.document.same_generation(&self.document) || (!self.closed.get()
            && (!self.created.is_active() || !current.as_ref().is_some_and(|current| current.same_generation(&self.created)))) {
            return Err("The workbook renderer changed before rollback".into());
        }
        let same = state.queued_workbooks.get(&self.view)
            .is_some_and(|entry| std::rc::Rc::ptr_eq(&entry.identity, &self.inserted));
        if !same { return Err("The workbook queue was replaced before rollback".into()); }
        let previous = self.previous.borrow_mut().take();
        state.set_queued_workbook_prepared(self.view, previous, self.rollback_revision.clone());
        self.completed.set(true);
        Ok(())
    }
}

pub struct PreparedBlankSeed {
    core: Core,
    view: ViewId,
    document: shell_contract::runtime::RendererGeneration,
    created: shell_contract::runtime::RendererGeneration,
    revision: std::rc::Rc<()>,
    commit_revision: std::rc::Rc<()>,
    rollback_revision: std::rc::Rc<()>,
    entry: PendingBlank,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
}

pub struct ValidatedBlankSeed<'a, 'b> {
    prepared: PreparedBlankSeed,
    successor: &'a shell_contract::ipc::NativeBrowserOwner,
    _transition: &'a shell_contract::ipc::NativeBrowserTransition<'b>,
}

pub struct BlankSeedReceipt {
    core: Core,
    view: ViewId,
    document: shell_contract::runtime::RendererGeneration,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    inserted: std::rc::Rc<std::cell::Cell<QueueOutcome>>,
    previous: std::cell::RefCell<Option<PendingBlank>>,
    rollback_revision: std::rc::Rc<()>,
    closed: std::cell::Cell<bool>,
    completed: std::cell::Cell<bool>,
}

impl PreparedBlankSeed {
    pub fn validate_in_transition<'a, 'b>(self, transition: &'a shell_contract::ipc::NativeBrowserTransition<'b>, successor: &'a shell_contract::ipc::NativeBrowserOwner) -> Result<ValidatedBlankSeed<'a, 'b>, String> {
        if !transition.matches(&self.reservation) || !transition.matches_owner(&self.owner)
            || !(transition.matches_owner(successor) || transition.proposes_owner(successor)) {
            return Err("The workbook blank browser owner changed".into());
        }
        let state = self.core.state.borrow();
        let tab = state.tab(self.view).ok_or("The workbook tab was closed")?;
        let current = self.core.env.services.views.renderer_generation(self.view);
        if tab.browser_closing || !tab.document.same_generation(&self.document)
            || !std::rc::Rc::ptr_eq(&state.blank_revision, &self.revision)
            || !self.created.is_active()
            || !current.as_ref().is_some_and(|current| current.same_generation(&self.created)) {
            return Err("The workbook blank seed changed before publication".into());
        }
        drop(state);
        Ok(ValidatedBlankSeed { prepared: self, successor, _transition: transition })
    }
}

impl ValidatedBlankSeed<'_, '_> {
    pub fn commit_in_transition(self) -> BlankSeedReceipt {
        let Self { prepared, successor, .. } = self;
        let PreparedBlankSeed { core, view, entry, commit_revision, rollback_revision, document, created, reservation, .. } = prepared;
        let mut state = core.state.borrow_mut();
        let inserted = entry.identity.clone();
        let previous = state.pending_new_blank.replace(entry);
        state.blank_revision = commit_revision;
        drop(state);
        BlankSeedReceipt { core, view, document, created, reservation, owner: successor.clone(), inserted,
            previous: std::cell::RefCell::new(previous), rollback_revision,
            completed: std::cell::Cell::new(false), closed: std::cell::Cell::new(false) }
    }
}

impl BlankSeedReceipt {
    pub fn retire_closed(&self, proof: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created) { return Err("The close receipt belongs to another workbook".into()); }
        self.closed.set(true);
        Ok(())
    }

    pub fn retire_adopted(&self, proof: &shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created) { return Err("The close receipt belongs to another workbook".into()); }
        if proof.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed { return Err("An adopted workbook requires a closed browser".into()); }
        if self.completed.get() { return Ok(()); }
        let mut state = self.core.state.borrow_mut();
        if state.pending_new_blank.as_ref().is_some_and(|entry| std::rc::Rc::ptr_eq(&entry.identity, &self.inserted)) {
            state.pending_new_blank = None;
            state.blank_revision = self.rollback_revision.clone();
            self.inserted.set(QueueOutcome::Retired);
        }
        self.previous.borrow_mut().take();
        self.completed.set(true);
        Ok(())
    }

    pub fn retire_superseded(&self, proof: &shell_contract::ipc::NativeBrowserSuperseded) -> Result<(), String> {
        if !proof.matches(&self.reservation, &self.owner) { return Err("The browser replacement belongs to another workbook".into()); }
        if self.completed.get() { return Ok(()); }
        let mut state = self.core.state.borrow_mut();
        if state.pending_new_blank.as_ref().is_some_and(|entry| std::rc::Rc::ptr_eq(&entry.identity, &self.inserted)) {
            state.pending_new_blank = None;
            state.blank_revision = self.rollback_revision.clone();
            self.inserted.set(QueueOutcome::Retired);
        }
        self.previous.borrow_mut().take();
        self.completed.set(true);
        Ok(())
    }

    pub fn rollback(&self) -> Result<(), String> {
        if self.completed.get() { return Ok(()); }
        let mut state = self.core.state.borrow_mut();
        if self.inserted.get() == QueueOutcome::Retired {
            return Err("The workbook blank seed was removed before rollback".into());
        }
        if self.inserted.get() == QueueOutcome::Consumed {
            self.previous.borrow_mut().take();
            self.completed.set(true);
            return Ok(());
        }
        let tab = state.tab(self.view).ok_or("The workbook tab was closed before rollback")?;
        let current = self.core.env.services.views.renderer_generation(self.view);
        if !tab.document.same_generation(&self.document) || (!self.closed.get()
            && (!self.created.is_active() || !current.as_ref().is_some_and(|current| current.same_generation(&self.created)))) {
            return Err("The workbook renderer changed before rollback".into());
        }
        if !state.pending_new_blank.as_ref().is_some_and(|entry| std::rc::Rc::ptr_eq(&entry.identity, &self.inserted)) {
            return Err("The workbook blank seed was replaced before rollback".into());
        }
        state.pending_new_blank = self.previous.borrow_mut().take();
        state.blank_revision = self.rollback_revision.clone();
        self.completed.set(true);
        Ok(())
    }
}


/// The sheets tabs of the module registered on this thread
pub fn views() -> Option<SheetsViews> {
    VIEWS.with(|views| views.borrow().clone())
}

pub(super) fn install_views(core: &Core) {
    VIEWS.with(|views| *views.borrow_mut() = Some(SheetsViews(core.clone())));
}

fn schedule_workbook_nudge(core: &Core, generation: u64, started: f64) {
    let env = core.env.clone();
    core.env.shell.after(700, Box::new(move || {
        env.post(move |core| {
            let active = {
                let state = core.state.borrow();
                if state.workbook_nudge_generation != generation
                    || core.env.shell.now_ms() - started > 30_000.0
                {
                    return;
                }
                state.active_view.filter(|view| {
                    state.tab(*view).is_some()
                        && state.queued_workbooks.contains_key(view)
                        && core.env.alive(*view)
                })
            };
            if let Some(view) = active {
                core.send(view, "menu:action", vec![IpcValue::String("open".into())]);
                schedule_workbook_nudge(core, generation, started);
            }
        });
    }));
}

impl SheetsViews {
    pub fn begin_headless_export(
        &self, view: ViewId, target: shell_contract::ipc::NativeInvocationTarget,
        input: &str, out_path: &str, settle: Box<dyn FnOnce(super::core::HeadlessReport)>,
        reservation: &shell_contract::ipc::NativeClosureReservation, owner: &shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<RegisteredTab, String> {
        use shell_contract::ipc::NativeInvocationSelection;
        if target.view() != view || !matches!(target.operation().selection(),
            NativeInvocationSelection::HeadlessExport { surface: shell_contract::Surface::Sheets, input: selected_input, output, format, .. }
                if selected_input == Path::new(input) && output == Path::new(out_path) && format == "pdf") {
            return Err("The headless workbook request does not match its native reservation".into());
        }
        let mut registration = self.prepare_tab_registration(view, target.created().clone(), reservation, owner)?;
        let captured = target.clone();
        target.with_active(|| reservation.with_initial_owner(owner, |transition| {
            let mut state = self.0.state.borrow_mut();
            if state.tab(view).is_some() || state.headless_waiters.contains_key(&view)
                || state.headless_targets.contains_key(&view) {
                return Err("The headless workbook view is already registered".into());
            }
            drop(state);
            registration.commit_admitted(transition)?;
            let mut state = self.0.state.borrow_mut();
            let tab = state.tab_mut(view).ok_or("The headless workbook registration disappeared")?;
            tab.headless_target = Some(captured);
            tab.headless_work = Some(std::sync::Arc::new(super::core::HeadlessWork::default()));
            state.replace_queued_workbook(view, QueuedWorkbook::new(input.to_owned()));
            state.headless_targets.insert(view, out_path.to_owned());
            state.headless_waiters.insert(view, settle);
            Ok(())
        })).ok_or("The headless workbook reservation expired")??;
        registration.finish()
    }

    pub fn finish_headless_export(
        &self, registration: std::rc::Rc<RegisteredTab>, receipt: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if !std::rc::Rc::ptr_eq(&self.0.state, &registration.core.state)
            || !receipt.matches(&registration.reservation, registration.view, &registration.created) {
            return done(Err("The headless closure belongs to another workbook".into()));
        }
        let view = registration.view;
        let prepared = {
            let state = self.0.state.borrow();
            match state.tab(view).filter(|tab| std::rc::Rc::ptr_eq(&tab.registration_retired, &registration.retired)) {
                None => Err("The headless workbook was replaced".to_owned()),
                Some(tab) if tab.headless_drain.is_some() => Err("The headless workbook is already closing".to_owned()),
                Some(tab) => tab.headless_work.clone().ok_or_else(|| "The headless workbook work tracker is unavailable".to_owned()),
            }
        };
        let work = match prepared { Ok(work) => work, Err(error) => return done(Err(error)) };
        let finish: Box<dyn FnOnce(Result<(), String>)> = Box::new(move |result| {
            done(result.and_then(|()| registration.retire_exact(&receipt)));
        });
        let missing = {
            let mut state = self.0.state.borrow_mut();
            match state.tab_mut(view) {
                Some(tab) => { tab.headless_drain = Some(finish); None }
                None => Some(finish),
            }
        };
        if let Some(finish) = missing { return finish(Err("The headless workbook was replaced".into())); }
        let env = self.0.env.clone();
        let expected = work.clone();
        let result = work.drain(Box::new(move || env.post(move |core| {
            let finish = {
                let mut state = core.state.borrow_mut();
                state.tab_mut(view).filter(|tab| tab.headless_work.as_ref().is_some_and(|work| std::sync::Arc::ptr_eq(work, &expected)))
                    .and_then(|tab| tab.headless_drain.take())
            };
            if let Some(finish) = finish { finish(Ok(())); }
        })));
        if let Err(error) = result {
            let finish = self.0.state.borrow_mut().tab_mut(view).and_then(|tab| tab.headless_drain.take());
            if let Some(finish) = finish { finish(Err(error)); }
        }
    }

    /// a tab the shell created: known to the picker and the close path before its first call
    pub fn register_tab(&self, view: ViewId) {
        self.0.register_tab(view);
    }
    pub fn prepare_tab_registration(
        &self, view: ViewId, created: shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation, owner: &shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<PreparedTabRegistration, String> {
        if !reservation.matches(view, &created) { return Err("The Sheets closure reservation does not match the view".into()); }
        if !owner.reservation().same_reservation(reservation) { return Err("The Sheets browser owner does not match the reservation".into()); }
        if !self.0.env.services.views.renderer_generation(view)
            .is_some_and(|current| current.same_generation(&created) && current.is_active()) {
            return Err("The Sheets view is no longer available".into());
        }
        let mut state = self.0.state.borrow_mut();
        if state.tab(view).is_some() { return Err("The Sheets tab is already registered".into()); }
        let previous_active = std::rc::Rc::new(std::cell::Cell::new(state.active_view));
        state.tabs.reserve(1);
        drop(state);
        let tab = TabState::new();
        let retired = tab.registration_retired.clone();
        let receipt = RegisteredTab { core: self.0.clone(), view, created: created.clone(),
            reservation: reservation.clone(), owner: owner.clone(), previous_active: previous_active.clone(),
            retired: retired.clone(), closed: std::rc::Rc::new(std::cell::Cell::new(false)),
            superseded: std::rc::Rc::new(std::cell::Cell::new(false)),
            phase: std::rc::Rc::new(std::cell::Cell::new(RegistrationPhase::Prepared)) };
        Ok(PreparedTabRegistration { core: self.0.clone(), view, created, reservation: reservation.clone(), owner: owner.clone(), tab: Some(tab),
            previous_active, retired, committed: false, receipt })
    }
    pub fn is_tab(&self, view: ViewId) -> bool {
        self.0.state.borrow().tab(view).is_some()
    }
    pub fn set_active_view(&self, view: Option<ViewId>) {
        self.0.state.borrow_mut().active_view = view;
    }

    pub fn start_queued_workbook_nudge(&self) {
        let (generation, active) = {
            let mut state = self.0.state.borrow_mut();
            state.workbook_nudge_generation = state.workbook_nudge_generation.wrapping_add(1);
            (state.workbook_nudge_generation, state.active_view)
        };
        if let Some(view) = active.filter(|view| self.0.env.alive(*view)) {
            self.nudge_queued_workbook(view);
        }
        schedule_workbook_nudge(&self.0, generation, self.0.env.shell.now_ms());
    }


    pub fn external_link_session(&self, view: ViewId, id: &str) -> Option<(String, String)> {
        let session = self.0.session_of(view, id)?;
        let base = dirname(session.restore_target.as_deref().unwrap_or(&session.path));
        Some((session.path, base))
    }

    pub fn suggested_path_taken(&self, path: &str) -> bool {
        self.0.suggested_path_taken(path)
    }

    pub fn mark_unsaved_new(&self, backing: &str, suggested: &str, temp_dir: &str) {
        let recovery = RecoveryIdentity::Untitled(RecoveryUntitledIdentity {
            kind: "sheets".into(),
            untitled_id: self.0.env.uuid(),
            ext: super::recovery_policy::untitled_workbook_ext(suggested).into(),
        });
        self.0.state.borrow_mut().unsaved_new_workbooks.insert(backing.to_owned(), UnsavedNew {
            suggest_save_as: suggested.to_owned(),
            temp_dir: temp_dir.to_owned(),
            recovery,
            restored: false,
            original_path: None,
            ods_converted: false,
            formulas_dropped: None,
            ods_source_path: None,
        });
    }

    pub fn unmark_unsaved_new(&self, backing: &str) {
        self.0.state.borrow_mut().unsaved_new_workbooks.remove(backing);
    }

    pub fn pending_edit_count(&self, view: ViewId) -> f64 {
        self.0.pending_edit_count(view)
    }

    pub fn nudge_queued_workbook(&self, view: ViewId) {
        self.0.send(view, "menu:action", vec![IpcValue::String("open".into())]);
    }


    pub fn queue_workbook(&self, view: ViewId, path: &str) {
        self.0.queue_workbook(view, path);
    }

    pub fn prepare_workbook_queue(&self, view: ViewId, path: String, created: shell_contract::runtime::RendererGeneration, reservation: &shell_contract::ipc::NativeClosureReservation, owner: &shell_contract::ipc::NativeBrowserOwner) -> Result<PreparedWorkbookQueue, String> {
        let mut state = self.0.state.borrow_mut();
        state.queued_workbooks.try_reserve(1).map_err(|error| error.to_string())?;
        let tab = state.tab(view).ok_or("The workbook tab is not registered")?;
        let current = self.0.env.services.views.renderer_generation(view);
        if tab.browser_closing || !reservation.matches(view, &created) || !owner.reservation().same_reservation(reservation) || !created.is_active() || !current.as_ref().is_some_and(|current| current.same_generation(&created)) {
            return Err("The workbook renderer changed before preparation".into());
        }
        Ok(PreparedWorkbookQueue { core: self.0.clone(), view, entry: QueuedWorkbook::new(path), commit_revision: std::rc::Rc::new(()), rollback_revision: std::rc::Rc::new(()), revision: tab.queue_revision.clone(), document: tab.document.clone(), created, reservation: reservation.clone(), owner: owner.clone() })
    }
    pub fn prepare_blank_seed(&self, view: ViewId, created: shell_contract::runtime::RendererGeneration, reservation: &shell_contract::ipc::NativeClosureReservation, owner: &shell_contract::ipc::NativeBrowserOwner) -> Result<PreparedBlankSeed, String> {
        let state = self.0.state.borrow();
        let tab = state.tab(view).ok_or("The workbook tab is not registered")?;
        let current = self.0.env.services.views.renderer_generation(view);
        if tab.browser_closing || !reservation.matches(view, &created) || !owner.reservation().same_reservation(reservation) || !created.is_active() || !current.as_ref().is_some_and(|current| current.same_generation(&created)) {
            return Err("The workbook renderer changed before preparation".into());
        }
        Ok(PreparedBlankSeed {
            core: self.0.clone(), view, document: tab.document.clone(), created: created.clone(),
            revision: state.blank_revision.clone(), commit_revision: std::rc::Rc::new(()), reservation: reservation.clone(), owner: owner.clone(),
            rollback_revision: std::rc::Rc::new(()), entry: PendingBlank::new(Some((view, created))),
        })
    }


    pub fn set_new_blank(&self) {
        self.0.set_new_blank();
    }

    pub fn mark_untitled_path(&self, path: &str) {
        self.0.mark_untitled_path(path);
    }

    pub fn authorize_mcp_write(&self, view: ViewId, path: &str) {
        self.0.authorize_mcp_write(view, path);
    }

    /// the OS is ending the session: every live tab writes its copy now
    pub fn write_copies_now(&self, done: Box<dyn FnOnce(Result<(), main_core::shell::renderer_disposal::Failure>)>) {
        self.0.write_copies_now(done);
    }

    pub fn mark_shutting_down(&self) {
        self.0.mark_shutting_down();
    }

    pub fn reset_shutting_down(&self) {
        self.0.reset_shutting_down();
    }

    pub fn discard_recovery(&self, view: ViewId) {
        self.0.discard_recovery(view);
    }

    pub fn recovery_identity(&self, view: ViewId) -> Option<RecoveryIdentity> {
        let state = self.0.state.borrow();
        let tab = state.tab(view)?;
        let path = tab.current_path.as_deref()?;
        let session = tab.sessions.iter().rev().find(|(_, session)| {
            session.path == path || session.restore_target.as_deref() == Some(path)
                || session.suggest_save_as.as_deref() == Some(path)
        })?;
        super::recovery_policy::session_recovery_identity(&session.1)
    }

    /// `webContents.on('render-process-gone')`: its copies are what the shell reopens from
    pub fn render_process_gone(&self, view: ViewId) {
        self.0.state.borrow_mut().recovery_of(view).crashed = true;
        let pending = {
            let mut state = self.0.state.borrow_mut();
            let nonces: Vec<_> = state.write_now_waiters.iter()
                .filter_map(|(nonce, (owner, _))| (*owner == view).then_some(nonce.clone()))
                .collect();
            nonces.into_iter().filter_map(|nonce| state.write_now_waiters.remove(&nonce))
                .collect::<Vec<_>>()
        };
        for (_, finish) in pending {
            finish(Err(main_core::shell::renderer_disposal::Failure("The workbook renderer crashed".into())));
        }
    }

    /// `exportSheetsPdfHeadless`: the hidden view's target, and what settles the run
    pub fn register_headless_export(
        &self,
        view: ViewId,
        out_path: &str,
        settle: Box<dyn FnOnce(super::core::HeadlessReport)>,
    ) {
        let mut state = self.0.state.borrow_mut();
        state.headless_targets.insert(view, out_path.to_owned());
        state.headless_waiters.insert(view, settle);
    }
}

impl main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget> for SheetsViews {
    fn kind(&self) -> &str { "sheets" }

    fn write_copies_now(&self, done: Box<dyn FnOnce(Result<(), main_core::shell::renderer_disposal::Failure>)>) {
        SheetsViews::write_copies_now(self, done);
    }

    fn open_file_with_decision(
        &self, view: &ViewId, request: &main_core::packages::electron_utils::recovery_contract::RecoveryOpenFileRequest,
        authority: &shell_contract::ipc::RecoveryOpenTarget,
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        use shell_contract::ipc::{NativeInvocationSelection, NativeRecoveryDecision, RecoveryOpenContext};
        let snapshot = match authority.authority_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return done(Err(main_core::shell::renderer_disposal::Failure(error))),
        };
        let valid = authority.view() == *view && match &snapshot.context {
            RecoveryOpenContext::Renderer { .. } => true,
            RecoveryOpenContext::NativeStartup(operation) => matches!(operation.selection(),
                NativeInvocationSelection::RecoveryFile { surface: shell_contract::Surface::Sheets, path, decision }
                    if path == Path::new(&request.file_path)
                        && matches!((decision, request.decision),
                            (NativeRecoveryDecision::Restore, main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision::Restore)
                            | (NativeRecoveryDecision::Original, main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision::Original))),
        };
        if !valid { return done(Err(main_core::shell::renderer_disposal::Failure("The recovery request does not match its captured target".into()))); }
        let Some(effects) = self.0.env.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>() else {
            return done(Err(main_core::shell::renderer_disposal::Failure("Native recovery effects are unavailable".into())));
        };
        let mut applied = false;
        let result = effects.with_recovery_seed_snapshot(authority, &snapshot, &mut || {
            let mut state = self.0.state.borrow_mut();
            if state.tab(*view).is_none() { return Err("The recovery workbook tab is unavailable".into()); }
            state.decided_opens.insert(*view, (request.file_path.clone(), request.decision));
            state.replace_queued_workbook(*view, QueuedWorkbook::new(request.file_path.clone()));
            applied = true;
            Ok(())
        }).and_then(|()| if applied { Ok(()) } else { Err("The recovery seed was not admitted".into()) });
        done(result.map_err(main_core::shell::renderer_disposal::Failure));
    }

    fn open_untitled_copy(
        &self, view: &ViewId, request: &main_core::packages::electron_utils::recovery_contract::RecoveryOpenUntitledRequest,
        authority: &shell_contract::ipc::RecoveryOpenTarget,
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        use shell_contract::ipc::{NativeInvocationSelection, NativeRecoveryCopy, RecoveryOpenContext};
        let snapshot = match authority.authority_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return done(Err(main_core::shell::renderer_disposal::Failure(error))),
        };
        let view = *view;
        let valid = authority.view() == view && request.copy.kind() == "sheets" && match &snapshot.context {
            RecoveryOpenContext::Renderer { .. } => true,
            RecoveryOpenContext::NativeStartup(operation) => match operation.selection() {
                NativeInvocationSelection::RecoveryCopy { surface: shell_contract::Surface::Sheets, copy, title, save_as_default } => {
                    let identity_matches = match (&request.copy, copy) {
                        (RecoveryIdentity::File(a), NativeRecoveryCopy::File { path }) => path == Path::new(&a.file_path),
                        (RecoveryIdentity::Untitled(a), NativeRecoveryCopy::Untitled { id, ext }) => id == &a.untitled_id && ext == &a.ext,
                        _ => false,
                    };
                    identity_matches && title == &request.title
                        && save_as_default.as_deref() == request.save_as_default_path.as_deref().map(Path::new)
                }
                _ => false,
            },
        };
        if !valid { return done(Err(main_core::shell::renderer_disposal::Failure("The recovery copy does not match its captured target".into()))); }
        let Some(effects) = self.0.env.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return done(Err(main_core::shell::renderer_disposal::Failure("Native recovery effects are unavailable".into())));
        };
        let mut losses = vec![authority.created().loss_token(), snapshot.revision_loss_token()];
        match &snapshot.context {
            RecoveryOpenContext::Renderer { source, participants, cancel, pending } => {
                losses.extend([cancel.clone(), pending.clone()]);
                for owner in std::iter::once(source).chain(participants.iter()) {
                    let Some(loss) = owner.loss_token() else {
                        return done(Err(main_core::shell::renderer_disposal::Failure("A recovery owner has no lifetime".into())));
                    };
                    losses.push(loss);
                }
            }
            RecoveryOpenContext::NativeStartup(operation) => {
                losses.extend([operation.loss_token(), operation.cancel().clone(), operation.shutdown().clone()]);
            }
        }
        let id = self.0.env.uuid();
        {
            let mut state = self.0.state.borrow_mut();
            if state.recovery_open_waiters.contains_key(&id) {
                drop(state);
                return done(Err(main_core::shell::renderer_disposal::Failure("Duplicate recovery opening identity".into())));
            }
            state.recovery_open_waiters.insert(id.clone(), super::core::RecoveryOpenWaiter {
                view, done, subscriptions: Vec::new(),
            });
        }
        let subscriptions = losses.into_iter().map(|loss| {
            let env = self.0.env.clone();
            let id = id.clone();
            loss.on_cancel(move || env.post(move |core| core.finish_recovery_open(&id, Err("The recovery opening was cancelled".into()))))
        }).collect();
        if let Some(waiter) = self.0.state.borrow_mut().recovery_open_waiters.get_mut(&id) {
            waiter.subscriptions = subscriptions;
        } else { return; }
        let request = request.clone();
        let authority = authority.clone();
        let env = self.0.env.clone();
        let worker = env.clone();
        env.spawn(move || {
            let result = (|| -> Result<_, String> {
                use std::io::Write;
                use main_core::packages::electron_utils::recovery_store::RecoveryPinnedRead;
                let mut pinned = None;
                effects.with_recovery_seed_snapshot(&authority, &snapshot, &mut || {
                    pinned = Some(RecoveryPinnedRead::open(&worker.user_data_dir(), &request.copy).map_err(|error| error.message)?);
                    Ok(())
                })?;
                let snapshot = pinned.ok_or("The recovery read was not admitted")?.read().map_err(|error| error.message)?;
                let ext = match &request.copy {
                    RecoveryIdentity::Untitled(copy) => copy.ext.clone(),
                    RecoveryIdentity::File(copy) => super::recovery_policy::untitled_workbook_ext(&copy.file_path).to_owned(),
                };
                let original = match &request.copy {
                    RecoveryIdentity::Untitled(_) => request.save_as_default_path.clone().or(snapshot.original_path),
                    RecoveryIdentity::File(_) => None,
                };
                let offered = original.as_deref().map(super::recovery_policy::save_as_default_for)
                    .or_else(|| request.save_as_default_path.clone());
                let base = offered.as_deref().map(|path| basename(path, None)).unwrap_or_else(|| request.title.clone());
                let stem = base.rsplit_once('.').map_or(base.as_str(), |(stem, _)| stem);
                let stem = super::save::sanitize_auto_rename_base(stem).unwrap_or_else(|| "Workbook".into());
                let name = format!("{stem}{ext}");
                let suggested = match offered {
                    Some(path) => path,
                    None => join(&[&super::dialogs::default_save_dir(&worker).map_err(|error| error.message)?, &name]),
                };
                let temp_dir = super::open::import_dir(&worker);
                let backing = join(&[&temp_dir, &name]);
                let mut file = None;
                let mut created = false;
                let staged = effects.with_recovery_seed_snapshot(&authority, &snapshot, &mut || {
                    let parent = Path::new(&temp_dir).parent().ok_or("Invalid recovery staging directory")?;
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                    std::fs::create_dir(&temp_dir).map_err(|error| error.to_string())?;
                    created = true;
                    file = Some(std::fs::OpenOptions::new().write(true).create_new(true).open(&backing).map_err(|error| error.to_string())?);
                    Ok(())
                });
                if let Err(error) = staged {
                    if created {
                        main_core::sheets::temp_files::cleanup_import_temp_directory(&worker.temp_dir(), &temp_dir);
                    }
                    return Err(error);
                }
                if let Err(error) = file.ok_or("The recovery staging file was not admitted")?.write_all(&snapshot.bytes) {
                    main_core::sheets::temp_files::cleanup_import_temp_directory(&worker.temp_dir(), &temp_dir);
                    return Err(error.to_string());
                }
                let ods = original.as_ref().is_some_and(|path| path.to_lowercase().ends_with(".ods"));
                let mark = UnsavedNew {
                    suggest_save_as: suggested, temp_dir: temp_dir.clone(), recovery: request.copy,
                    restored: true, original_path: original.clone(), ods_converted: ods,
                    formulas_dropped: if ods { Some(snapshot.formulas_dropped.unwrap_or(0.0)) } else { None },
                    ods_source_path: if ods { original } else { None },
                };
                Ok((backing, temp_dir, mark))
            })();
            worker.post(move |core| match result {
                Ok((backing, temp_dir, mark)) => {
                    let mut mark = Some(mark);
                    let result = effects.with_recovery_seed_snapshot(&authority, &snapshot, &mut || {
                        let mut state = core.state.borrow_mut();
                        if !state.recovery_open_waiters.contains_key(&id) || state.tab(view).is_none() {
                            return Err("The recovery workbook is no longer pending".into());
                        }
                        state.unsaved_new_workbooks.insert(backing.clone(), mark.take().ok_or("The recovery seed was already consumed")?);
                        state.replace_queued_workbook(view, QueuedWorkbook::new(backing.clone()));
                        Ok(())
                    });
                    if result.is_err() {
                        let env = core.env.clone();
                        let root = env.temp_dir();
                        env.spawn(move || main_core::sheets::temp_files::cleanup_import_temp_directory(&root, &temp_dir));
                    }
                    core.finish_recovery_open(&id, result);
                }
                Err(error) => core.finish_recovery_open(&id, Err(error)),
            });
        });
    }

    fn is_unprotectable(&self, view: &ViewId) -> bool {
        self.0.state.borrow().tab(*view).is_some_and(|tab| {
            tab.sessions.iter().any(|(_, session)| session.password.is_some())
        })
    }
}
