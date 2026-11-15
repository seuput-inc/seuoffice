use std::any::Any;
use std::sync::Arc;

use main_core::docs::byte_handoff::{DeferredMediaLoss, MediaPermit, MediaPermitGatePlan, MediaViewLease, PreparedMediaCandidate};
use shell_contract::hosts::{DocsMediaStage, DocsMediaTransition};
use shell_contract::runtime::RendererGeneration;
use shell_contract::ViewId;

/// Exact actor-staged media transition shared by the native and fixture hosts.
pub struct DocsMediaTransitionAdapter {
    pub lease: MediaViewLease,
    pub old: MediaPermit,
    pub successor: MediaPermit,
}

impl DocsMediaTransition for DocsMediaTransitionAdapter {
    fn lease(&self) -> &(dyn Any + Send + Sync) { &self.lease }
    fn old(&self) -> &(dyn Any + Send + Sync) { &self.old }
    fn successor(&self) -> &(dyn Any + Send + Sync) { &self.successor }
    fn old_identity(&self) -> (ViewId, u64) {
        let owner = self.old.owner();
        (ViewId(owner.view), owner.generation)
    }
    fn successor_identity(&self) -> (ViewId, u64) {
        let owner = self.successor.owner();
        (ViewId(owner.view), owner.generation)
    }
    fn synthetic_successor(&self) -> Result<(RendererGeneration, Arc<dyn Any + Send + Sync>), String> {
        if !self.successor.is_active() { return Err("The staged Docs media permit was revoked.".into()); }
        let generation = RendererGeneration::new();
        let lost = generation.clone();
        let subscription = self.successor.on_revoke(move || lost.revoke());
        Ok((generation, Arc::new(subscription)))
    }
    fn synthetic_transition(
        &self, admit: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
        after_unlock: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut notifications = DeferredMediaLoss::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.lease.with_staged_transition(&self.old, &self.successor, |staged| {
                let mut plan = MediaPermitGatePlan::new(&[&self.old, &self.successor]);
                plan.with_active_retiring(&self.old, &self.successor, &mut notifications, |retirement| {
                    let mut staged = Some(staged);
                    admit(&mut || staged.take().ok_or("The Docs media transition was already admitted.")?.commit(retirement))
                }).ok_or_else(|| "The captured Docs media transition was revoked.".to_owned())?
            }).and_then(|result| result)
        }));
        let after = std::panic::catch_unwind(std::panic::AssertUnwindSafe(after_unlock));
        let notify = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(notifications)));
        match (result, after, notify) {
            (Err(panic), _, _) | (Ok(_), Err(panic), _) | (Ok(_), Ok(_), Err(panic)) => std::panic::resume_unwind(panic),
            (Ok(Err(error)), Ok(_), Ok(())) => Err(error),
            (Ok(Ok(())), Ok(after), Ok(())) => after,
        }
    }
}

pub struct DocsMediaStageAdapter {
    pub lease: MediaViewLease,
    pub old: MediaPermit,
    pub prior: Option<MediaPermit>,
    pub candidate: PreparedMediaCandidate,
}

impl DocsMediaStage for DocsMediaStageAdapter {
    fn lease(&self) -> &(dyn Any + Send + Sync) { &self.lease }
    fn old(&self) -> &(dyn Any + Send + Sync) { &self.old }
    fn old_identity(&self) -> (ViewId, u64) {
        let owner = self.old.owner();
        (ViewId(owner.view), owner.generation)
    }
    fn prior(&self) -> Option<&(dyn Any + Send + Sync)> {
        self.prior.as_ref().map(|permit| permit as &(dyn Any + Send + Sync))
    }
    fn candidate(&self) -> &(dyn Any + Send + Sync) { &self.candidate }
    fn synthetic_stage(&self, admit: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>) -> Result<(), String> {
        let mut notifications = DeferredMediaLoss::default();
        let result = self.lease.with_prepared_stage(&self.old, self.prior.as_ref(), &self.candidate, |prepared| {
            let target = prepared.retirement_target().clone();
            let mut plan = MediaPermitGatePlan::new(&[&self.old, &target, self.candidate.permit()]);
            plan.with_active_retiring(&target, self.candidate.permit(), &mut notifications, |retirement| {
                let mut prepared = Some(prepared);
                admit(&mut || prepared.take().ok_or("The Docs media stage was already admitted.")?.commit(retirement))
            }).ok_or_else(|| "The captured Docs media stage was revoked.".to_owned())?
        })?;
        drop(notifications);
        result
    }
}

impl super::Docs {
    pub(super) fn capture_load_authority(&self, cx: &shell_contract::ipc::CallContext) -> Result<super::open::LoadAuthority, shell_contract::IpcError> {
        let context = cx.native.clone().ok_or_else(|| crate::kit::error("the Docs request has no native owner"))?;
        let owner = self.checked_document_owner(cx.sender.view, shell_contract::ipc::NativeRequestOwner { view: cx.sender.view, context })?;
        Ok(super::open::LoadAuthority { owner, cancel: cx.cancel.clone() })
    }
}

#[cfg(test)]
mod transition_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn after_unlock_precedes_loss_notification_even_on_panic() {
        for panics in [false, true] {
            let lease = MediaViewLease::new(71).unwrap();
            let old = lease.current().unwrap();
            let successor = lease.stage_document().unwrap();
            let notified = Arc::new(AtomicUsize::new(0));
            let observed = notified.clone();
            let _subscription = old.on_revoke(move || { observed.fetch_add(1, Ordering::SeqCst); });
            let transition = DocsMediaTransitionAdapter { lease: lease.clone(), old, successor: successor.clone() };
            let mut after_calls = 0;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                transition.synthetic_transition(&mut |commit| {
                    commit()?;
                    if panics { panic!("admission panic"); }
                    Ok(())
                }, &mut || {
                    after_calls += 1;
                    assert!(lease.current().unwrap().same_session(&successor));
                    assert_eq!(notified.load(Ordering::SeqCst), 0);
                    Ok(())
                })
            }));
            assert_eq!(result.is_err(), panics);
            if let Ok(result) = result { result.unwrap(); }
            assert_eq!(after_calls, 1);
            assert_eq!(notified.load(Ordering::SeqCst), 1);
        }
    }
}
