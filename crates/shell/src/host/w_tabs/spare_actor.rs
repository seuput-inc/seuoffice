use std::cell::{Cell, OnceCell};
use std::rc::Rc;

use shell_contract::ipc::{NativeBrowserOwner, NativeBrowserSuperseded, NativeBrowserTransition, NativeClosureReceipt};
use shell_ipc_d1::sheets::editors::{PreparedTabClose, PreparedTabTransfer, RegisteredTab};
use shell_ipc_w::tabs::host::{EditorSeedRollback, SpareActor, SpareClosePlan, SpareTransferPlan};

pub(super) struct SheetsSpareActor {
    receipt: OnceCell<RegisteredTab>,
    cancelled: Cell<bool>,
}

impl SheetsSpareActor {
    pub(super) fn new(receipt: RegisteredTab) -> Rc<Self> {
        Rc::new(Self { receipt: OnceCell::from(receipt), cancelled: Cell::new(false) })
    }

    fn receipt(&self) -> Result<&RegisteredTab, String> {
        self.receipt.get().ok_or_else(|| "The successor Sheets registration is not committed".into())
    }
}

impl EditorSeedRollback for SheetsSpareActor {
    fn retire_closed(&self, proof: NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.receipt().and_then(|receipt| receipt.retire_closed(&proof)));
    }
    fn retire_adopted(&self, proof: NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.receipt().and_then(|receipt| receipt.retire_adopted(&proof)));
    }
    fn retire_superseded(&self, proof: NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.receipt().and_then(|receipt| receipt.retire_superseded(&proof)));
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        let result = match self.receipt.get() {
            Some(receipt) => receipt.discard().and_then(|removed|
                if removed { Ok(()) } else { Err("The captured Sheets registration cannot be rolled back".into()) }),
            None => { self.cancelled.set(true); Ok(()) }
        };
        done(result);
    }
}

struct ClosePlan<'a>(Option<PreparedTabClose<'a>>);
impl SpareClosePlan for ClosePlan<'_> {
    fn with_validated(&mut self, transition: &NativeBrowserTransition<'_>, next: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        let plan = self.0.take().ok_or("The Sheets close plan was already consumed")?;
        let validated = plan.validate_in_transition(transition)?;
        next()?;
        validated.commit_in_transition();
        Ok(())
    }
}

struct TransferPlan {
    plan: Option<PreparedTabTransfer>,
    successor: Rc<SheetsSpareActor>,
}
impl SpareTransferPlan for TransferPlan {
    fn successor(&self) -> Rc<dyn SpareActor> { self.successor.clone() }
    fn successor_receipt(&self) -> Rc<dyn EditorSeedRollback> { self.successor.clone() }
    fn with_validated(&mut self, transition: &NativeBrowserTransition<'_>, successor: &NativeBrowserOwner,
        next: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        if self.successor.cancelled.get() { return Err("The successor Sheets registration was cancelled".into()); }
        let plan = self.plan.take().ok_or("The Sheets transfer plan was already consumed")?;
        let validated = plan.validate_in_transition(transition, successor)?;
        next()?;
        let receipt = validated.commit_in_transition();
        assert!(self.successor.receipt.set(receipt).is_ok(), "Sheets successor was already committed");
        Ok(())
    }
}
struct SavedBarrier(shell_ipc_d1::sheets::editors::PreparedSavedWorkbookBarrier);
impl shell_ipc_w::tabs::host::SpareSavedBarrier for SavedBarrier {
    fn with_validated(&mut self, transition: &NativeBrowserTransition<'_>, next: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        self.0.with_validated(transition, next)
    }
}


impl SpareActor for SheetsSpareActor {
    fn prepare_close(&self) -> Result<Box<dyn SpareClosePlan + '_>, String> {
        Ok(Box::new(ClosePlan(Some(self.receipt()?.prepare_close()?))))
    }
    fn prepare_transfer(&self) -> Result<Box<dyn SpareTransferPlan>, String> {
        let plan = self.receipt()?.prepare_transfer()?;
        let successor = Rc::new(Self { receipt: OnceCell::new(), cancelled: Cell::new(false) });
        Ok(Box::new(TransferPlan { plan: Some(plan), successor }))
    }
    fn prepare_saved_barrier(&self, target: &shell_contract::ipc::NativeSavedTarget) -> Result<Box<dyn shell_ipc_w::tabs::host::SpareSavedBarrier>, String> {
        Ok(Box::new(SavedBarrier(self.receipt()?.prepare_saved_barrier(target.clone())?)))
    }
}
