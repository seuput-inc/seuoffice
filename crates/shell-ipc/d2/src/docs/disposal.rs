//! A one-shot core actor retirement, followed by worker recovery settlement.

use std::rc::Rc;

use main_core::docs::byte_handoff::{MediaPermit, MediaViewLease};
use shell_contract::ViewId;
use shell_contract::ipc::NativeRequestOwner;

use super::Docs;
use crate::kit;

pub type DisposalAdmission = Box<
    dyn FnOnce(
        &mut dyn FnMut(&shell_contract::ipc::NativeBrowserTransition<'_>) -> Result<(), String>,
    ) -> Result<(), String>,
>;
pub type DisposalDone = Box<dyn FnOnce(DocsDisposalOutcome)>;
enum ActorAdmission {
    Live(DisposalAdmission),
    Closed(shell_contract::ipc::NativeClosureReceipt),
}

pub enum DocsDisposalOutcome {
    NotRetired {
        actor: Box<DocsActorDisposal>,
        error: String,
    },
    Retired {
        cleanup: Result<(), String>,
    },
}

type RetirementWaiter = Box<dyn FnOnce(Result<(), String>)>;
enum RetirementState {
    Live,
    Pending(Vec<RetirementWaiter>),
    Settled(Result<(), String>),
}

pub(super) struct ActorRetirement(std::cell::RefCell<RetirementState>);
impl ActorRetirement {
    pub(super) fn new() -> Rc<Self> {
        Rc::new(Self(std::cell::RefCell::new(RetirementState::Live)))
    }
    fn begin(&self) -> Result<(), String> {
        let mut state = self
            .0
            .try_borrow_mut()
            .map_err(|_| "The Word retirement is busy.")?;
        if !matches!(*state, RetirementState::Live) {
            return Err("The Word retirement was already claimed.".into());
        }
        *state = RetirementState::Pending(Vec::new());
        Ok(())
    }
    pub(super) fn join(
        &self,
        waiter: RetirementWaiter,
    ) -> Result<Option<RetirementWaiter>, (String, RetirementWaiter)> {
        let mut state = match self.0.try_borrow_mut() {
            Ok(state) => state,
            Err(_) => return Err(("The Word retirement is busy.".into(), waiter)),
        };
        match &mut *state {
            RetirementState::Live => Ok(Some(waiter)),
            RetirementState::Pending(waiters) => {
                waiters.push(waiter);
                Ok(None)
            }
            RetirementState::Settled(result) => {
                let result = result.clone();
                drop(state);
                waiter(result);
                Ok(None)
            }
        }
    }
    fn finish(&self, result: Result<(), String>) {
        let waiters = {
            let mut state = self.0.borrow_mut();
            match std::mem::replace(&mut *state, RetirementState::Settled(result.clone())) {
                RetirementState::Pending(waiters) => waiters,
                _ => Vec::new(),
            }
        };
        for waiter in waiters {
            waiter(result.clone());
        }
    }
}

struct RetiredCompletion {
    done: Option<DisposalDone>,
    prior_error: Option<String>,
    retirement: Rc<ActorRetirement>,
}

impl RetiredCompletion {
    fn finish(mut self, cleanup: Result<(), String>) {
        self.retirement.finish(cleanup.clone());
        if let Some(done) = self.done.take() {
            done(DocsDisposalOutcome::Retired { cleanup });
        }
    }
}

impl Drop for RetiredCompletion {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let error = match self.prior_error.take() {
                Some(first) => format!("{first}; Word recovery cleanup worker was discarded"),
                None => "The Word recovery cleanup worker was discarded.".into(),
            };
            self.retirement.finish(Err(error.clone()));
            done(DocsDisposalOutcome::Retired {
                cleanup: Err(error),
            });
        }
    }
}

#[must_use]
pub struct DocsActorDisposal {
    docs: Rc<Docs>,
    view: ViewId,
    lease: MediaViewLease,
    permit: MediaPermit,
    browser_owner: shell_contract::ipc::NativeBrowserOwner,
    target: Option<NativeRequestOwner>,
    retirement: Rc<ActorRetirement>,
}

pub(super) fn capture(
    docs: &Rc<Docs>,
    view: ViewId,
    target: &NativeRequestOwner,
) -> Result<DocsActorDisposal, String> {
    docs.checked_document_owner(view, target.clone())
        .map_err(|error| error.to_string())?;
    let slot = docs
        .media_views
        .borrow()
        .get(&view)
        .cloned()
        .ok_or("The captured Word media view is unavailable.")?;
    let permit = slot
        .current()
        .ok_or("The captured Word document is unavailable.")?;
    Ok(DocsActorDisposal {
        docs: docs.clone(),
        view,
        lease: slot.lease,
        browser_owner: slot.owner,
        permit,
        target: Some(target.clone()),
        retirement: slot.retirement,
    })
}
pub(super) type CleanupCandidate = Rc<std::cell::RefCell<Option<DocsActorDisposal>>>;

pub(super) fn candidate(docs: &Docs, view: ViewId) -> Result<Option<CleanupCandidate>, String> {
    let slots = docs.media_views.borrow();
    let slot = slots
        .get(&view)
        .ok_or("The captured Word actor is unavailable.")?;
    slot.cleanup
        .as_ref()
        .map(|candidate| {
            candidate
                .upgrade()
                .ok_or_else(|| "The captured Word cleanup candidate was released.".to_owned())
        })
        .transpose()
}

pub(super) fn retain_successor(
    candidate: Option<&CleanupCandidate>,
    lease: &MediaViewLease,
    old: &MediaPermit,
    permit: &MediaPermit,
    owner: &NativeRequestOwner,
    commit: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let Some(candidate) = candidate else {
        return commit();
    };
    let mut slot = candidate
        .try_borrow_mut()
        .map_err(|_| "The Word cleanup candidate is busy.")?;
    let current = slot
        .as_ref()
        .ok_or("The Word cleanup candidate was claimed.")?;
    let successor = current.successor(lease, old, permit.clone(), owner.clone())?;
    commit()?;
    *slot = Some(successor);
    Ok(())
}

impl DocsActorDisposal {
    pub(super) fn initial(
        docs: Rc<Docs>,
        view: ViewId,
        lease: MediaViewLease,
        permit: MediaPermit,
        browser_owner: shell_contract::ipc::NativeBrowserOwner,
        retirement: Rc<ActorRetirement>,
    ) -> Self {
        Self {
            docs,
            view,
            lease,
            permit,
            browser_owner,
            target: None,
            retirement,
        }
    }

    /// Prepares the exact successor without looking up a current owner.
    pub fn successor(
        &self,
        lease: &MediaViewLease,
        old: &MediaPermit,
        permit: MediaPermit,
        target: NativeRequestOwner,
    ) -> Result<Self, String> {
        if !self.lease.same_view(lease)
            || !self.permit.same_session(old)
            || target.view != self.view
        {
            return Err("The captured Word cleanup candidate was replaced.".into());
        }
        Ok(Self {
            docs: self.docs.clone(),
            view: self.view,
            lease: self.lease.clone(),
            permit,
            browser_owner: self.browser_owner.clone(),
            target: Some(target),
            retirement: self.retirement.clone(),
        })
    }

    pub fn run(self, admission: DisposalAdmission, done: DisposalDone) {
        self.run_inner(ActorAdmission::Live(admission), done, true);
    }

    /// Cleanup-only retirement of the already captured actor. The caller must
    /// admit the original browser close under its retained cleanup authority.
    pub fn run_cleanup(self, admission: DisposalAdmission, done: DisposalDone) {
        self.run_inner(ActorAdmission::Live(admission), done, false);
    }

    pub fn run_closed(
        self,
        receipt: shell_contract::ipc::NativeClosureReceipt,
        done: DisposalDone,
    ) {
        self.run_inner(ActorAdmission::Closed(receipt), done, false);
    }

    fn run_inner(self, admission: ActorAdmission, done: DisposalDone, require_live_request: bool) {
        let docs = self.docs.clone();
        let view = self.view;
        if require_live_request {
            let Some(target) = self.target.clone() else {
                return done(DocsDisposalOutcome::NotRetired {
                    actor: Box::new(self),
                    error: "The Word actor has no captured request owner.".into(),
                });
            };
            if let Err(error) = docs.checked_document_owner(view, target) {
                return done(DocsDisposalOutcome::NotRetired {
                    actor: Box::new(self),
                    error: error.to_string(),
                });
            }
        }
        if !docs.media_views.borrow().get(&view).is_some_and(|current| {
            current.same_view(&self.lease) && current.owner.same_owner(&self.browser_owner)
        }) || !self
            .lease
            .current()
            .is_some_and(|current| current.same_session(&self.permit))
        {
            return done(DocsDisposalOutcome::NotRetired {
                actor: Box::new(self),
                error: "The captured Word actor was replaced.".into(),
            });
        }
        let pending_media = self.lease.pending();
        docs.torn_down.borrow_mut().reserve(1);
        let mut retired_writes = None;
        let mut detached = None;
        let mut accepted = false;
        let mut detach = || {
            if accepted {
                return Err("The Word retirement was admitted more than once.".into());
            }
            if docs.is_torn_down(view) {
                return Err("The captured Word actor is already retired.".into());
            }
            if !docs.media_views.borrow().get(&view).is_some_and(|current| {
                current.same_view(&self.lease) && current.owner.same_owner(&self.browser_owner)
            }) || !self
                .lease
                .current()
                .is_some_and(|current| current.same_session(&self.permit))
            {
                return Err("The captured Word actor was replaced.".into());
            }
            let saved = super::saved_load::prepare_disposal(&docs, view, &self.browser_owner)?;
            self.retirement.begin()?;
            let lease = docs
                .media_views
                .borrow_mut()
                .remove(&view)
                .ok_or("The captured Word actor is unavailable.")?;
            let saved = saved.commit();
            docs.torn_down.borrow_mut().insert(view);
            retired_writes = Some((
                docs.doc_writable.remove(view),
                docs.pdf_writable.remove(view),
            ));
            let recovery = docs.book.borrow_mut().take_disposal(view);
            let disk = docs.disk_states.remove(view);
            docs.passwords.borrow_mut().forget(view);
            let recovery_seed = docs.take_recovery_seed(view);
            let recovery_waiters = docs.close.take_recovery_view(view);
            detached = Some((
                recovery,
                lease,
                disk,
                recovery_seed,
                recovery_waiters,
                saved,
            ));
            accepted = true;
            Ok(())
        };
        let admitted = match admission {
            ActorAdmission::Live(admission) => admission(&mut |transition| {
                if !transition.matches_owner(&self.browser_owner) {
                    return Err("The captured Word browser was replaced.".into());
                }
                detach()
            }),
            ActorAdmission::Closed(receipt) => {
                receipt.with_terminal_owner(&self.browser_owner, |transition| {
                    if !transition.matches(self.browser_owner.reservation(), &self.browser_owner) {
                        return Err("The closed Word browser was replaced.".into());
                    }
                    detach()
                })
            }
        };
        drop(detach);
        if !accepted {
            return done(DocsDisposalOutcome::NotRetired {
                actor: Box::new(self),
                error: admitted
                    .err()
                    .unwrap_or_else(|| "The Word retirement was not admitted.".into()),
            });
        }
        // The captured actor is terminal. Never reacquire its native admission.
        drop(retired_writes);
        let Some((mut recovery, lease, disk, recovery_seed, recovery_waiters, saved)) = detached
        else {
            let error = "The retired Word actor was not captured.".to_owned();
            self.retirement.finish(Err(error.clone()));
            return done(DocsDisposalOutcome::Retired {
                cleanup: Err(error),
            });
        };
        drop(disk);
        lease.mark_retired();
        let permit = self.permit;
        if let Some(seed) = recovery_seed {
            seed.fail("the Docs recovery actor was retired");
        }
        recovery_waiters.fail("Docs recovery view destroyed");
        if let Some(saved) = saved {
            saved.fail("the Docs saved document actor was retired");
        }
        lease.revoke();
        #[cfg(any(test, feature = "probe"))]
        {
            let binding = docs.synthetic_media.borrow_mut().remove(&view);
            drop(binding);
        }
        let mut failure = admitted.err();
        if let Some(pending) = pending_media
            && let Err(error) = docs.engine.forget_lazy_media_owner(pending)
            && failure.is_none()
        {
            failure = Some(error);
        }
        if let Err(error) = docs.engine.forget_lazy_media_owner(permit)
            && failure.is_none()
        {
            failure = Some(error);
        }
        let shared = docs.recovery_shared.clone();
        let user_data = docs.user_data(&[]);
        let shell = docs.shell.clone();
        let completed = RetiredCompletion {
            done: Some(done),
            prior_error: failure.clone(),
            retirement: self.retirement.clone(),
        };
        kit::on_worker(
            &docs.services,
            move || {
                super::recovery::with_store(&shared, &user_data, &shell, |store| {
                    recovery.settle_closed_result(store, view, false)
                })
            },
            move |result| {
                let result = match (failure, result) {
                    (Some(first), Err(error)) => Err(format!(
                        "{first}; Word recovery cleanup failed: {}",
                        error.message
                    )),
                    (Some(first), Ok(())) => Err(first),
                    (None, result) => result.map_err(|error| error.message),
                };
                completed.finish(result);
            },
        );
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn observers_receive_only_the_actual_worker_settlement() {
        let retirement = ActorRetirement::new();
        retirement.begin().unwrap();
        let results = Rc::new(RefCell::new(Vec::new()));
        for _ in 0..2 {
            let results = results.clone();
            assert!(matches!(
                retirement.join(Box::new(move |result| results.borrow_mut().push(result))),
                Ok(None)
            ));
        }
        assert!(results.borrow().is_empty());
        retirement.finish(Err("recovery failed".into()));
        assert_eq!(
            &*results.borrow(),
            &[Err("recovery failed".into()), Err("recovery failed".into())]
        );
        let results2 = results.clone();
        assert!(matches!(
            retirement.join(Box::new(move |result| results2.borrow_mut().push(result))),
            Ok(None)
        ));
        assert_eq!(results.borrow().len(), 3);
    }
}
