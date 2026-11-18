use std::{cell::RefCell, rc::Rc};

use main_core::shell::tab_manager::EditorSeed;
use shell_contract::{hosts::OwnedEffectsHost, ipc::{NativeBrowserOwner, NativeBrowserSuperseded, NativeBrowserTransition, NativeClosureReceipt, NativeRequestOwner}, runtime::CancelToken, IpcValue, ViewId};
use shell_ipc_d1::sheets::editors::{BlankSeedReceipt, PreparedBlankSeed, PreparedWorkbookQueue, WorkbookQueueReceipt};
use shell_ipc_w::tabs::host::{EditorSeedRollback, EditorSeedRollbackStore, NativeBrowserRegistration, RetainedSpareBinding, SpareSeedPlan};

use super::Hooks;

struct WorkbookReceipt(RefCell<Option<WorkbookQueueReceipt>>);
impl EditorSeedRollback for WorkbookReceipt {
    fn retire_closed(&self, proof: NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.0.borrow().as_ref().ok_or("The workbook queue was not committed".to_owned()).and_then(|receipt| receipt.retire_closed(&proof)));
    }
    fn retire_adopted(&self, proof: NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.0.borrow().as_ref().ok_or("The workbook queue was not committed".to_owned()).and_then(|receipt| receipt.retire_adopted(&proof)));
    }
    fn retire_superseded(&self, proof: NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.0.borrow().as_ref().ok_or("The workbook queue was not committed".to_owned()).and_then(|receipt| receipt.retire_superseded(&proof)));
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.0.borrow().as_ref().ok_or("The workbook queue was not committed".to_owned()).and_then(WorkbookQueueReceipt::rollback));
    }
}

struct BlankReceipt(RefCell<Option<BlankSeedReceipt>>);
impl EditorSeedRollback for BlankReceipt {
    fn retire_closed(&self, proof: NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.0.borrow().as_ref().ok_or("The blank workbook seed was not committed".to_owned()).and_then(|receipt| receipt.retire_closed(&proof)));
    }
    fn retire_adopted(&self, proof: NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.0.borrow().as_ref().ok_or("The blank workbook seed was not committed".to_owned()).and_then(|receipt| receipt.retire_adopted(&proof)));
    }
    fn retire_superseded(&self, proof: NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.0.borrow().as_ref().ok_or("The blank workbook seed was not committed".to_owned()).and_then(|receipt| receipt.retire_superseded(&proof)));
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.0.borrow().as_ref().ok_or("The blank workbook seed was not committed".to_owned()).and_then(BlankSeedReceipt::rollback));
    }
}

struct SeedPlan {
    workbook: Option<PreparedWorkbookQueue>,
    blank: Option<PreparedBlankSeed>,
    workbook_receipt: Rc<WorkbookReceipt>,
    blank_receipt: Rc<BlankReceipt>,
    rollback: EditorSeedRollbackStore,
    effects: OwnedEffectsHost,
    target: NativeRequestOwner,
    nudge: bool,
    committed: bool,
}

impl SpareSeedPlan for SeedPlan {
    fn with_validated(&mut self, transition: &NativeBrowserTransition<'_>, successor: &NativeBrowserOwner,
        next: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        if self.committed { return Err("The spare seed was already committed".into()); }
        let workbook = self.workbook.take().map(|plan| plan.validate_in_transition(transition, successor)).transpose()?;
        let blank = self.blank.take().map(|plan| plan.validate_in_transition(transition, successor)).transpose()?;
        next()?;
        if let Some(workbook) = workbook {
            *self.workbook_receipt.0.borrow_mut() = Some(workbook.commit_in_transition());
            self.rollback.borrow_mut().push(self.workbook_receipt.clone());
        }
        if let Some(blank) = blank {
            *self.blank_receipt.0.borrow_mut() = Some(blank.commit_in_transition());
            self.rollback.borrow_mut().push(self.blank_receipt.clone());
        }
        self.committed = true;
        Ok(())
    }

    fn after_commit(&mut self, cancel: &CancelToken) -> Result<(), String> {
        if !self.committed { return Err("The spare seed was not committed".into()); }
        if self.nudge {
            self.effects.emit_owned(&self.target, &self.target, cancel, cancel,
                "menu:action", vec![IpcValue::String("open".into())])?;
            self.nudge = false;
        }
        Ok(())
    }
}

pub(super) fn prepare(
    hooks: &Hooks, view: ViewId, seed: Option<EditorSeed>, docs_open_path: Option<String>,
    workbook: Option<String>, new_blank: bool, target: &NativeRequestOwner,
    binding: &RetainedSpareBinding, rollback: &EditorSeedRollbackStore,
) -> Result<Box<dyn SpareSeedPlan>, String> {
    if seed.is_some() || docs_open_path.is_some() { return Err("The Sheets spare cannot accept another editor's seed".into()); }
    let registered = match &binding.browser.registration {
        NativeBrowserRegistration::Registered(owner) => owner,
        NativeBrowserRegistration::Unregistered => return Err("The Sheets spare is not registered".into()),
    };
    if view != target.view || registered.view != view
        || !registered.loss_token().zip(target.loss_token()).is_some_and(|(a, b)| a.same_token(&b)) {
        return Err("The Sheets spare target changed".into());
    }
    let generation = registered.context.renderer().cloned().ok_or("The Sheets spare renderer is unavailable")?;
    if !target.context.renderer().is_some_and(|current| current.same_generation(&generation))
        || !binding.browser.reservation.matches(view, &generation) {
        return Err("The Sheets spare generation changed".into());
    }
    let effects = hooks.services.hosts.get::<OwnedEffectsHost>().cloned().ok_or("Owned Sheets effects are unavailable")?;
    let sheets = shell_ipc_d1::sheets::editors::views().ok_or("The Sheets actor is unavailable")?;
    let nudge = workbook.is_some();
    let workbook = workbook.map(|path| sheets.prepare_workbook_queue(view, path, generation.clone(), &binding.browser.reservation, &binding.browser.owner)).transpose()?;
    let blank = if new_blank { Some(sheets.prepare_blank_seed(view, generation, &binding.browser.reservation, &binding.browser.owner)?) } else { None };
    let count = usize::from(workbook.is_some()) + usize::from(blank.is_some()) + 1;
    rollback.borrow_mut().try_reserve(count).map_err(|_| "The Sheets receipts cannot be reserved")?;
    Ok(Box::new(SeedPlan { workbook, blank, workbook_receipt: Rc::new(WorkbookReceipt(RefCell::new(None))),
        blank_receipt: Rc::new(BlankReceipt(RefCell::new(None))), rollback: rollback.clone(), effects,
        target: target.clone(), nudge, committed: false }))
}
