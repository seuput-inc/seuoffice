//! Core-thread registry for unloaded recovery tab openers.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use main_core::shell::recovery_shell::{RestoredTabOpener, UnloadedTabSpec};
use main_core::shell::tab_manager::{LoadedTabView, Opener, TabKind, UnloadedTabOpenContext};
use shell_contract::{ViewId, WindowId};

type PrepareOpen =
    dyn Fn(&RestoredTabOpener, &UnloadedTabOpenContext) -> Result<PreparedRecoveryOpen, String>;
pub(crate) struct Open {
    prepare: Rc<PrepareOpen>,
}

pub(crate) type BeforeLoad = Box<
    dyn FnOnce(
        ViewId,
        &shell_contract::ipc::RecoveryOpenTarget,
        main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ),
>;
type DirectDelivery = std::sync::Arc<
    std::sync::Mutex<Option<Result<shell_contract::ipc::NativeBrowserRetirement, String>>>,
>;
type Quarantined = (WindowId, NativeAttempt, Box<dyn FnOnce()>);
type PendingDrain = (WindowId, Box<dyn FnOnce(Result<(), String>)>);
type OpenerLookup = (
    RestoredTabOpener,
    UnloadedTabOpenContext,
    shell_contract::ipc::NativeInvocationOperation,
    Rc<Open>,
    Rc<NeedsUser>,
);

pub(crate) struct PreparedRecoveryOpen {
    pub(crate) kind: TabKind,
    pub(crate) path: Option<String>,
    pub(crate) untitled: bool,
    pub(crate) operation: Option<shell_contract::ipc::NativeInvocationOperation>,
    attempt: Option<NativeAttempt>,
    pub(crate) before_load: Option<BeforeLoad>,
    after: Box<dyn FnOnce(ViewId) -> Result<(), String>>,
}

impl PreparedRecoveryOpen {
    pub(crate) fn finish(self, view: ViewId) -> Result<LoadedTabView<ViewId>, String> {
        if self.before_load.is_some() {
            return Err("The recovery initializer has not completed".into());
        }
        (self.after)(view)?;
        Ok(LoadedTabView { view, ready: None })
    }
    pub(crate) fn take_attempt(
        &mut self,
        core: std::sync::Arc<dyn shell_contract::runtime::Thread>,
    ) -> Option<Rc<dyn shell_ipc_w::tabs::RecoveryAttempt>> {
        self.attempt.take().map(|attempt| {
            Rc::new(NativeAttemptBridge { attempt, core })
                as Rc<dyn shell_ipc_w::tabs::RecoveryAttempt>
        })
    }
}
pub(crate) type NeedsWork = Box<dyn FnOnce() -> Result<bool, String> + Send>;
pub(crate) type NeedsUser =
    dyn Fn(&RestoredTabOpener, &UnloadedTabOpenContext) -> Result<NeedsWork, String>;
pub(crate) type FailedAttemptDone =
    Box<dyn FnOnce(Result<shell_ipc_w::tabs::OwnedLoadedTabView, String>)>;
pub(crate) type RecoveryCleanup = shell_ipc_w::tabs::RecoveryCleanup;
enum RollbackProgress {
    Retired(Rc<dyn shell_ipc_w::tabs::host::EditorSeedRollback>),
    Finished(Rc<dyn shell_ipc_w::tabs::host::EditorSeedRollback>),
}
struct HeldFailure {
    error: String,
    done: FailedAttemptDone,
    cleanup: Option<RecoveryCleanup>,
    retirement: Option<shell_contract::ipc::NativeBrowserRetirement>,
    delivery: Option<DirectDelivery>,
    rollback_progress: Option<RollbackProgress>,
    indeterminate: Option<&'static str>,
    core: Option<std::sync::Arc<dyn shell_contract::runtime::Thread>>,
    in_flight: bool,
    on_cleanup_error: Option<Rc<dyn Fn(String)>>,
}

fn rollback_receipts(
    attempt: NativeAttempt,
    proof: shell_contract::ipc::NativeBrowserRetirement,
    done: Box<dyn FnOnce(Result<(), String>)>,
) {
    let next = {
        let entry = attempt.entry.borrow();
        match entry.held_failure.as_ref() {
            None => Err("The recovery failure is not retained".to_owned()),
            Some(held) => {
                let receipt = entry.rollback.borrow().last().cloned();
                match (receipt, held.rollback_progress.as_ref()) {
                    (None, None) => Ok(None),
                    (Some(receipt), None) => Ok(Some((receipt, 0))),
                    (Some(receipt), Some(RollbackProgress::Retired(previous)))
                        if Rc::ptr_eq(&receipt, previous) =>
                    {
                        Ok(Some((receipt, 1)))
                    }
                    (Some(receipt), Some(RollbackProgress::Finished(previous)))
                        if Rc::ptr_eq(&receipt, previous) =>
                    {
                        Ok(Some((receipt, 2)))
                    }
                    _ => Err("The recovery rollback sequence changed".to_owned()),
                }
            }
        }
    };
    let (receipt, stage) = match next {
        Ok(Some(next)) => next,
        Ok(None) => return done(Ok(())),
        Err(error) => return done(Err(error)),
    };
    if stage == 2 {
        let matched = {
            let mut entry = attempt.entry.borrow_mut();
            let matched = entry
                .rollback
                .borrow()
                .last()
                .is_some_and(|last| Rc::ptr_eq(last, &receipt));
            if matched {
                entry.rollback.borrow_mut().pop();
                if let Some(held) = entry.held_failure.as_mut() {
                    held.rollback_progress = None;
                }
            }
            matched
        };
        if !matched {
            return done(Err("The recovery rollback sequence changed".into()));
        }
        return rollback_receipts(attempt, proof, done);
    }
    let next_attempt = attempt.clone();
    let next_receipt = receipt.clone();
    let superseded = matches!(
        &proof,
        shell_contract::ipc::NativeBrowserRetirement::Superseded(_)
    );
    let next_proof = proof.clone();
    let completed = Box::new(move |result: Result<(), String>| {
        if let Err(error) = result {
            return done(Err(error));
        }
        let retained = {
            let mut entry = next_attempt.entry.borrow_mut();
            if !next_attempt.matches_entry(&entry) {
                false
            } else if let Some(held) = entry.held_failure.as_mut() {
                held.rollback_progress = Some(if stage == 1 || superseded {
                    RollbackProgress::Finished(next_receipt)
                } else {
                    RollbackProgress::Retired(next_receipt)
                });
                true
            } else {
                false
            }
        };
        if !retained {
            return done(Err("The exact recovery attempt is unavailable".into()));
        }
        rollback_receipts(next_attempt, next_proof, done);
    });
    if stage == 1 {
        receipt.rollback(completed);
    } else {
        match proof {
            shell_contract::ipc::NativeBrowserRetirement::Closed(proof) => {
                receipt.retire_closed(proof, completed)
            }
            shell_contract::ipc::NativeBrowserRetirement::Superseded(proof) => {
                receipt.retire_superseded(proof, completed)
            }
        }
    }
}

enum Attempt {
    Idle,
    Starting,
    Finishing,
    Active(shell_contract::ipc::NativeInvocationOperation),
}

struct Entry {
    owner: WindowId,
    kind: TabKind,
    tab_id: Option<String>,
    selected: UnloadedTabSpec,
    plan: Rc<super::recovery_launch::issuer::RecoveryPlan>,
    attempt: Attempt,
    target: Option<shell_contract::ipc::RecoveryOpenTarget>,
    held_failure: Option<HeldFailure>,
    rollback: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    retired: bool,
    open: Rc<Open>,
    needs_user: Rc<NeedsUser>,
}

impl Entry {
    fn opening(&self) -> bool {
        !matches!(self.attempt, Attempt::Idle)
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.plan.cancel();
        if let Attempt::Active(operation) = &self.attempt {
            operation.finish();
        }
    }
}

#[derive(Clone)]
struct NativeAttempt {
    entry: Rc<RefCell<Entry>>,
    opener: Opener,
    operation: shell_contract::ipc::NativeInvocationOperation,
}
struct NativeAttemptBridge {
    attempt: NativeAttempt,
    core: std::sync::Arc<dyn shell_contract::runtime::Thread>,
}

fn matches_retirement(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    proof: &shell_contract::ipc::NativeBrowserRetirement,
) -> bool {
    match proof {
        shell_contract::ipc::NativeBrowserRetirement::Closed(receipt) => {
            receipt.matches(
                target.closure_reservation(),
                target.view(),
                target.created(),
            ) && receipt
                .terminal_owner()
                .is_some_and(|owner| owner.same_owner(target.browser_owner()))
        }
        shell_contract::ipc::NativeBrowserRetirement::Superseded(proof) => {
            proof.matches(target.closure_reservation(), target.browser_owner())
        }
    }
}

fn retained_attempt(
    opener: Opener,
    generation: &shell_contract::runtime::RendererGeneration,
) -> Option<NativeAttempt> {
    REGISTRY.with(|registry| {
        let entry = registry.borrow().entries.get(&opener.id).cloned()?;
        let operation = {
            let current = entry.borrow();
            let Attempt::Active(operation) = &current.attempt else {
                return None;
            };
            if !operation.authority_generation().same_generation(generation) {
                return None;
            }
            operation.clone()
        };
        Some(NativeAttempt {
            entry,
            opener,
            operation,
        })
    })
}

fn recovery_attempt_abandoned(
    opener: Opener,
    generation: &shell_contract::runtime::RendererGeneration,
) {
    let Some(attempt) = retained_attempt(opener, generation) else {
        return;
    };
    let abandoned = {
        let mut entry = attempt.entry.borrow_mut();
        if let Some(held) = entry.held_failure.as_mut() {
            if held.in_flight {
                held.in_flight = false;
                let delivered = held.delivery.as_ref().is_some_and(|slot| {
                    slot.lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .is_some()
                });
                if !delivered {
                    held.indeterminate = Some("The recovery cleanup completion was discarded");
                }
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if abandoned {
        attempt.cleanup_failed("The recovery cleanup discarded its completion".into());
    }
}

struct RecoveryCleanupGuard {
    opener: Opener,
    generation: shell_contract::runtime::RendererGeneration,
    core: std::sync::Arc<dyn shell_contract::runtime::Thread>,
    pending: bool,
}
impl Drop for RecoveryCleanupGuard {
    fn drop(&mut self) {
        if self.pending {
            let opener = self.opener;
            let generation = self.generation.clone();
            self.core.post(Box::new(move || {
                recovery_attempt_abandoned(opener, &generation)
            }));
        }
    }
}
struct RecoveryRollbackGuard {
    attempt: NativeAttempt,
    pending: bool,
}
impl Drop for RecoveryRollbackGuard {
    fn drop(&mut self) {
        if self.pending {
            let abandoned = {
                let mut entry = self.attempt.entry.borrow_mut();
                if let Some(held) = entry.held_failure.as_mut() {
                    if held.in_flight {
                        held.in_flight = false;
                        held.indeterminate = Some("The recovery rollback completion was discarded");
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if abandoned {
                self.attempt
                    .cleanup_failed("The recovery rollback discarded its completion".into());
            }
        }
    }
}

impl NativeAttempt {
    fn matches_entry(&self, entry: &Entry) -> bool {
        matches!(&entry.attempt,Attempt::Active(operation)
            if operation.authority_generation().same_generation(self.operation.authority_generation()))
    }
    fn registered(&self) -> bool {
        REGISTRY.with(|registry| {
            registry
                .borrow()
                .entries
                .get(&self.opener.id)
                .is_some_and(|entry| Rc::ptr_eq(entry, &self.entry))
        })
    }
    fn retain_unbound(&self, cleanup: Box<dyn FnOnce()>) {
        let owner = self.entry.borrow().owner;
        REGISTRY.with(|registry| {
            registry
                .borrow_mut()
                .quarantined
                .push((owner, self.clone(), cleanup))
        });
        fail_drains(owner, "The recovery cleanup has no matching target".into());
    }
    fn quarantined(&self) -> bool {
        REGISTRY.with(|registry| {
            registry.borrow().quarantined.iter().any(|(_, attempt, _)| {
                Rc::ptr_eq(&attempt.entry, &self.entry)
                    && attempt
                        .operation
                        .authority_generation()
                        .same_generation(self.operation.authority_generation())
            })
        })
    }
    fn cleanup_failed(&self, error: String) {
        let (owner, observer) = {
            let entry = self.entry.borrow();
            (
                entry.owner,
                entry
                    .held_failure
                    .as_ref()
                    .and_then(|held| held.on_cleanup_error.clone()),
            )
        };
        fail_drains(owner, error.clone());
        if let Some(observer) = observer {
            observer(error);
        }
    }
    fn finish(&self, success: bool) -> Result<WindowId, String> {
        if !self.registered() || self.quarantined() {
            return Err("The exact recovery attempt is unavailable".into());
        }
        let owner = {
            let mut entry = self.entry.borrow_mut();
            if !self.matches_entry(&entry) || entry.held_failure.is_some() {
                return Err("The exact recovery attempt is unavailable".into());
            }
            entry.attempt = Attempt::Finishing;
            entry.owner
        };
        self.operation.finish();
        let remove = {
            let mut entry = self.entry.borrow_mut();
            entry.target = None;
            if success || entry.retired {
                entry.retired = true;
                true
            } else {
                entry.attempt = Attempt::Idle;
                false
            }
        };
        if remove {
            let removed = REGISTRY.with(|registry| {
                let mut registry = registry.borrow_mut();
                if registry
                    .entries
                    .get(&self.opener.id)
                    .is_some_and(|entry| Rc::ptr_eq(entry, &self.entry))
                {
                    registry.entries.remove(&self.opener.id)
                } else {
                    None
                }
            });
            drop(removed);
        }
        Ok(owner)
    }

    fn closed(&self) -> Result<(), String> {
        if !self.registered() {
            return Err("The exact recovery attempt is unavailable".into());
        }
        let proof = {
            let mut entry = self.entry.borrow_mut();
            if !self.matches_entry(&entry) {
                return Err("The exact recovery attempt is unavailable".into());
            }
            let held = entry
                .held_failure
                .as_ref()
                .ok_or("The recovery failure is not retained")?;
            if held.in_flight {
                return Ok(());
            }
            let proof = held
                .retirement
                .clone()
                .ok_or("The recovery retirement is unavailable")?;
            if !entry
                .target
                .as_ref()
                .is_some_and(|target| matches_retirement(target, &proof))
            {
                return Err("The retirement does not match the original recovery target".into());
            }
            entry
                .held_failure
                .as_mut()
                .ok_or("The recovery failure is not retained")?
                .in_flight = true;
            proof
        };
        let attempt = self.clone();
        let guard = RecoveryRollbackGuard {
            attempt: self.clone(),
            pending: true,
        };
        rollback_receipts(
            self.clone(),
            proof,
            Box::new(move |result| {
                let mut guard = guard;
                guard.pending = false;
                let owner = attempt.entry.borrow().owner;
                if let Some(held) = attempt.entry.borrow_mut().held_failure.as_mut() {
                    held.in_flight = false;
                }
                if let Err(error) = result {
                    attempt.cleanup_failed(error);
                    return;
                }
                let held = attempt.entry.borrow_mut().held_failure.take();
                let Some(held) = held else {
                    fail_drains(owner, "The recovery failure was lost".into());
                    return;
                };
                match attempt.finish(false) {
                    Ok(owner) => {
                        (held.done)(Err(held.error));
                        finish_drains(owner);
                    }
                    Err(error) => {
                        attempt.entry.borrow_mut().held_failure = Some(held);
                        attempt.cleanup_failed(error);
                    }
                }
            }),
        );
        Ok(())
    }
    fn retry_failure(&self) {
        enum Work {
            Retire,
            Indeterminate(&'static str),
            Delivered(Result<shell_contract::ipc::NativeBrowserRetirement, String>),
            Cleanup(
                RecoveryCleanup,
                std::sync::Arc<dyn shell_contract::runtime::Thread>,
                std::sync::Arc<
                    std::sync::Mutex<
                        Option<Result<shell_contract::ipc::NativeBrowserRetirement, String>>,
                    >,
                >,
                shell_contract::ipc::RecoveryOpenTarget,
            ),
            Unavailable,
        }
        if !self.registered() {
            return;
        }
        let state = {
            let mut entry = self.entry.borrow_mut();
            if !self.matches_entry(&entry) {
                return;
            }
            let Entry {
                target,
                held_failure,
                ..
            } = &mut *entry;
            let Some(held) = held_failure.as_mut() else {
                return;
            };
            if held.in_flight {
                None
            } else if let Some(error) = held.indeterminate {
                Some(Work::Indeterminate(error))
            } else if held.retirement.is_some() {
                Some(Work::Retire)
            } else {
                let delivered = held.delivery.as_ref().and_then(|slot| {
                    slot.lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take()
                });
                if let Some(result) = delivered {
                    held.delivery = None;
                    Some(Work::Delivered(result))
                } else if let (Some(cleanup), Some(core), Some(target)) =
                    (held.cleanup.clone(), held.core.clone(), target.as_ref())
                {
                    let delivery = std::sync::Arc::new(std::sync::Mutex::new(None));
                    held.delivery = Some(delivery.clone());
                    held.in_flight = true;
                    Some(Work::Cleanup(cleanup, core, delivery, target.clone()))
                } else {
                    Some(Work::Unavailable)
                }
            }
        };
        match state {
            Some(Work::Indeterminate(error)) => self.cleanup_failed(error.into()),
            Some(Work::Retire) => {
                if let Err(error) = self.closed() {
                    self.cleanup_failed(error);
                }
            }
            Some(Work::Delivered(result)) => {
                let failed = {
                    let mut entry = self.entry.borrow_mut();
                    let valid = match &result {
                        Ok(proof) => entry
                            .target
                            .as_ref()
                            .is_some_and(|target| matches_retirement(target, proof)),
                        Err(_) => true,
                    };
                    if let Some(held) = entry.held_failure.as_mut() {
                        match result {
                            Ok(proof) if valid => {
                                held.retirement = Some(proof);
                                held.cleanup = None;
                                None
                            }
                            Ok(_) => Some(
                                "The retirement does not match the original recovery target".into(),
                            ),
                            Err(error) => Some(error),
                        }
                    } else {
                        None
                    }
                };
                if let Some(error) = failed {
                    self.cleanup_failed(error);
                } else {
                    self.retry_failure();
                }
            }
            Some(Work::Cleanup(cleanup, core, delivery, target)) => {
                let opener = self.opener;
                let generation = self.operation.authority_generation().clone();
                let guard = RecoveryCleanupGuard {
                    opener,
                    generation: generation.clone(),
                    core: core.clone(),
                    pending: true,
                };
                cleanup(Box::new(move |result| {
                    let result = match result {
                        Ok(proof) if !matches_retirement(&target, &proof) => {
                            Err("The retirement does not match the original recovery target".into())
                        }
                        result => result,
                    };
                    *delivery.lock().unwrap_or_else(|error| error.into_inner()) = Some(result);
                    core.post(Box::new(move || {
                        let mut guard = guard;
                        guard.pending = false;
                        let Some(attempt) = retained_attempt(opener, &generation) else {
                            return;
                        };
                        if let Some(held) = attempt.entry.borrow_mut().held_failure.as_mut() {
                            held.in_flight = false;
                        }
                        attempt.retry_failure();
                    }));
                }));
            }
            Some(Work::Unavailable) => {
                self.cleanup_failed("The recovery cleanup is unavailable".into())
            }
            None => {}
        }
    }
}

impl shell_ipc_w::tabs::RecoveryAttempt for NativeAttemptBridge {
    fn rollback_store(&self) -> shell_ipc_w::tabs::host::EditorSeedRollbackStore {
        self.attempt.entry.borrow().rollback.clone()
    }
    fn bind_target(&self, target: &shell_contract::ipc::RecoveryOpenTarget) -> Result<(), String> {
        if !self.attempt.registered() {
            return Err("The exact recovery attempt is unavailable".into());
        }
        let invocation = target
            .invocation_target()
            .ok_or("The recovery target has no native invocation")?;
        if !invocation
            .operation()
            .authority_generation()
            .same_generation(self.attempt.operation.authority_generation())
            || invocation.view() != target.view()
            || !invocation.created().same_generation(target.created())
            || !target
                .closure_reservation()
                .matches(target.view(), target.created())
            || !target
                .browser_owner()
                .reservation()
                .same_reservation(target.closure_reservation())
        {
            return Err("The recovery target does not belong to the original attempt".into());
        }
        let mut entry = self.attempt.entry.borrow_mut();
        if !self.attempt.matches_entry(&entry) {
            return Err("The exact recovery attempt is unavailable".into());
        }
        if let Some(current) = entry.target.as_ref() {
            if !current.same_binding(target) {
                return Err("The recovery attempt already captured another target".into());
            }
        } else {
            entry.target = Some(target.clone());
        }
        Ok(())
    }
    fn retain_unbound_cleanup(&self, cleanup: Box<dyn FnOnce()>) {
        self.attempt.retain_unbound(cleanup);
    }
    fn fail(
        &self,
        error: String,
        cleanup: Option<shell_ipc_w::tabs::RecoveryCleanup>,
        on_cleanup_error: Rc<dyn Fn(String)>,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let valid = self.attempt.registered() && {
            let entry = self.attempt.entry.borrow();
            self.attempt.matches_entry(&entry)
                && entry.target.is_some()
                && entry.held_failure.is_none()
        };
        if !valid {
            self.attempt
                .retain_unbound(Box::new(move || drop((error, cleanup, done))));
            on_cleanup_error("The exact recovery attempt cannot retain this failure".into());
            return;
        }
        {
            let mut entry = self.attempt.entry.borrow_mut();
            entry.held_failure = Some(HeldFailure {
                error,
                cleanup,
                retirement: None,
                delivery: None,
                rollback_progress: None,
                indeterminate: None,
                core: Some(self.core.clone()),
                in_flight: false,
                on_cleanup_error: Some(on_cleanup_error),
                done: Box::new(move |result| match result {
                    Err(error) => done(Err(error)),
                    Ok(_) => done(Err("unexpected recovery cleanup result".into())),
                }),
            });
        }
        self.attempt.retry_failure();
    }

    fn fail_before_creation(&self, error: String, done: Box<dyn FnOnce(Result<(), String>)>) {
        match self.attempt.finish(false) {
            Ok(owner) => {
                done(Err(error));
                finish_drains(owner);
            }
            Err(failure) => done(Err(format!("{error}; {failure}"))),
        }
    }

    fn fail_after_retirement(
        &self,
        error: String,
        proof: shell_contract::ipc::NativeBrowserRetirement,
        on_cleanup_error: Rc<dyn Fn(String)>,
        done: Box<dyn FnOnce()>,
    ) {
        let valid = self.attempt.registered() && {
            let entry = self.attempt.entry.borrow();
            self.attempt.matches_entry(&entry)
                && entry.held_failure.is_none()
                && entry
                    .target
                    .as_ref()
                    .is_some_and(|target| matches_retirement(target, &proof))
        };
        if !valid {
            self.attempt
                .retain_unbound(Box::new(move || drop((error, proof, done))));
            on_cleanup_error("The retirement does not match the original recovery attempt".into());
            return;
        }
        {
            let mut entry = self.attempt.entry.borrow_mut();
            entry.held_failure = Some(HeldFailure {
                error,
                cleanup: None,
                retirement: Some(proof),
                delivery: None,
                rollback_progress: None,
                indeterminate: None,
                core: Some(self.core.clone()),
                in_flight: false,
                on_cleanup_error: Some(on_cleanup_error),
                done: Box::new(move |_| done()),
            });
        }
        self.attempt.retry_failure();
    }
    fn retry_cleanup(&self) {
        self.attempt.retry_failure();
    }

    fn adopted(&self) -> Result<(), String> {
        let owner = self.attempt.finish(true)?;
        finish_drains(owner);
        Ok(())
    }
}
struct RecoveryQuery {
    owner: WindowId,
    operation: shell_contract::ipc::NativeInvocationOperation,
    source: shell_contract::ipc::NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    done: Box<dyn FnOnce(Result<bool, String>)>,
    subscriptions: Vec<shell_contract::runtime::CancelSubscription>,
}
pub(crate) enum DirectLifetime {
    CallerOwned(shell_ipc_w::tabs::CallerOwnedAdoptedLifetime),
    Retained(shell_ipc_w::tabs::AdoptedLifetimeHandle),
}

struct DirectFailure {
    error: main_core::shell::renderer_disposal::Failure,
    done: Box<dyn FnOnce(Result<ViewId, main_core::shell::renderer_disposal::Failure>)>,
    cleanup: Option<shell_ipc_w::tabs::host::ViewCreationCleanup>,
    proof: Option<shell_contract::ipc::NativeBrowserRetirement>,
    lifetime: Option<DirectLifetime>,
    terminal: Option<shell_contract::ipc::NativeBrowserRetirement>,
    complete: bool,
    in_flight: bool,
    core: std::sync::Arc<dyn shell_contract::runtime::Thread>,
}
struct DirectEntry {
    owner: WindowId,
    operation: shell_contract::ipc::NativeInvocationOperation,
    rollback: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    failure: Option<DirectFailure>,
}

fn direct_attempt_abandoned(id: u64) {
    let owner = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let entry = registry.direct.get_mut(&id)?;
        let failure = entry.failure.as_mut()?;
        if !failure.in_flight {
            return None;
        }
        failure.in_flight = false;
        Some(entry.owner)
    });
    if let Some(owner) = owner {
        fail_drains(
            owner,
            "The direct recovery cleanup discarded its completion".into(),
        );
    }
}

struct DirectCleanupGuard {
    id: u64,
    core: std::sync::Arc<dyn shell_contract::runtime::Thread>,
    pending: bool,
}
impl Drop for DirectCleanupGuard {
    fn drop(&mut self) {
        if self.pending {
            let id = self.id;
            self.core
                .post(Box::new(move || direct_attempt_abandoned(id)));
        }
    }
}

struct DirectRollbackGuard {
    id: u64,
    pending: bool,
}
impl Drop for DirectRollbackGuard {
    fn drop(&mut self) {
        if self.pending {
            direct_attempt_abandoned(self.id);
        }
    }
}

#[derive(Default)]
struct Registry {
    next: u64,
    entries: HashMap<u64, Rc<RefCell<Entry>>>,
    direct: HashMap<u64, DirectEntry>,
    quarantined: Vec<Quarantined>,
    closing: std::collections::HashSet<WindowId>,
    shutting_down: bool,
    queries: HashMap<u64, RecoveryQuery>,
    drains: Vec<PendingDrain>,
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

pub(crate) fn register(
    owner: WindowId,
    kind: TabKind,
    selected: UnloadedTabSpec,
    plan: super::recovery_launch::issuer::RecoveryPlan,
    open: Rc<Open>,
    needs_user: Rc<NeedsUser>,
) -> Result<Opener, String> {
    let id = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        if registry.shutting_down || registry.closing.contains(&owner) {
            return Err("The recovery owner is closing".into());
        }
        let id = registry
            .next
            .checked_add(1)
            .ok_or("recovery opener ids exhausted")?;
        registry.next = id;
        Ok::<_, String>(id)
    });
    let id = id?;
    REGISTRY.with(|registry| {
        registry.borrow_mut().entries.insert(
            id,
            Rc::new(RefCell::new(Entry {
                owner,
                kind,
                tab_id: None,
                selected,
                plan: Rc::new(plan),
                attempt: Attempt::Idle,
                target: None,
                held_failure: None,
                rollback: Rc::default(),
                retired: false,
                open,
                needs_user,
            })),
        );
    });
    Ok(Opener {
        id,
        has_needs_user: true,
    })
}

pub(crate) fn bind(opener: Opener, tab_id: &str) -> Result<(), String> {
    REGISTRY.with(|registry| {
        let registry = registry.borrow();
        let entry = registry
            .entries
            .get(&opener.id)
            .ok_or("recovery opener not found")?;
        let mut entry = entry.borrow_mut();
        match entry.tab_id.as_deref() {
            Some(bound) if bound != tab_id => Err("recovery opener belongs to another tab".into()),
            _ => {
                entry.tab_id = Some(tab_id.to_owned());
                Ok(())
            }
        }
    })
}

fn lookup(opener: &Opener, context: &UnloadedTabOpenContext) -> Result<OpenerLookup, String> {
    REGISTRY.with(|registry| {
        let registry = registry.borrow();
        let entry = registry
            .entries
            .get(&opener.id)
            .ok_or("recovery opener not found")?;
        let mut entry = entry.borrow_mut();
        if entry.retired {
            return Err("recovery opener not found".into());
        }
        if entry.kind != context.kind {
            return Err("recovery opener belongs to another editor kind".into());
        }
        match entry.tab_id.as_deref() {
            Some(tab_id) if tab_id != context.tab_id => {
                return Err("recovery opener belongs to another tab".into());
            }
            None => entry.tab_id = Some(context.tab_id.clone()),
            _ => {}
        }
        Ok((
            entry.selected.opener.clone(),
            UnloadedTabOpenContext {
                tab_id: context.tab_id.clone(),
                kind: entry.kind,
                title: entry.selected.title.clone(),
                file_path: entry.selected.file_path.clone(),
            },
            entry.plan.guard(),
            entry.open.clone(),
            entry.needs_user.clone(),
        ))
    })
}

pub(crate) fn prepare_open(
    opener: &Opener,
    context: &UnloadedTabOpenContext,
) -> Result<PreparedRecoveryOpen, String> {
    prepare_open_with(opener, context, |plan| plan.attempt())
}

pub(crate) fn prepare_open_command(
    opener: &Opener,
    context: &UnloadedTabOpenContext,
    command: &shell_contract::ipc::NativeInvocationOperation,
    cancel: shell_contract::runtime::CancelToken,
) -> Result<PreparedRecoveryOpen, String> {
    prepare_open_with(opener, context, |plan| {
        if plan.guard().with_active(|| ()).is_none() {
            return Err("The selected recovery plan is no longer active".into());
        }
        let selection = plan.selection().clone();
        // a native load of one tab opens that tab's own selection; a native open, the
        // selection of its surface
        let child = match command.selection() {
            shell_contract::ipc::NativeInvocationSelection::NativeCommand(
                shell_contract::ipc::NativeEditorCommand::ActivateTab { .. }
                | shell_contract::ipc::NativeEditorCommand::HydrateTab { .. },
            ) => super::native_commands::issue_tab_recovery_child(
                command,
                &context.tab_id,
                selection,
                cancel,
            )?,
            _ => super::native_commands::issue_recovery_child(command, selection, cancel)?,
        };
        if plan.guard().with_active(|| ()).is_none() {
            child.finish();
            return Err("The selected recovery plan is no longer active".into());
        }
        Ok(child)
    })
}

fn prepare_open_with(
    opener: &Opener,
    context: &UnloadedTabOpenContext,
    issue: impl FnOnce(
        &super::recovery_launch::issuer::RecoveryPlan,
    ) -> Result<shell_contract::ipc::NativeInvocationOperation, String>,
) -> Result<PreparedRecoveryOpen, String> {
    let (restored, captured, _, callback, _) = lookup(opener, context)?;
    let mut prepared = (callback.prepare)(&restored, &captured)?;
    let entry = REGISTRY.with(|registry| {
        let registry = registry.borrow();
        if registry.shutting_down {
            return Err("The recovery owner is closing".to_owned());
        }
        registry
            .entries
            .get(&opener.id)
            .cloned()
            .ok_or_else(|| "recovery opener not found".to_owned())
    })?;
    let plan = {
        let mut entry = entry.borrow_mut();
        if entry.retired {
            return Err("recovery opener not found".into());
        }
        if entry.opening() {
            return Err("The recovery opener is already opening".into());
        }
        entry.attempt = Attempt::Starting;
        entry.plan.clone()
    };
    let operation = match issue(&plan) {
        Ok(operation) => operation,
        Err(error) => {
            abort_start(opener);
            return Err(error);
        }
    };
    let accepted = {
        let closing = REGISTRY.with(|registry| registry.borrow().shutting_down);
        let mut current = entry.borrow_mut();
        if closing || current.retired || !matches!(current.attempt, Attempt::Starting) {
            false
        } else {
            current.attempt = Attempt::Active(operation.clone());
            true
        }
    };
    if !accepted {
        operation.finish();
        abort_start(opener);
        return Err("The recovery owner is closing".into());
    }
    prepared.attempt = Some(NativeAttempt {
        entry,
        opener: *opener,
        operation: operation.clone(),
    });
    prepared.operation = Some(operation);
    Ok(prepared)
}

fn abort_start(opener: &Opener) {
    let (removed, owner) = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let Some(entry) = registry.entries.get(&opener.id) else {
            return (None, None);
        };
        let mut entry = entry.borrow_mut();
        if !matches!(entry.attempt, Attempt::Starting) {
            return (None, None);
        }
        entry.attempt = Attempt::Idle;
        let owner = entry.owner;
        let retired = entry.retired;
        drop(entry);
        let removed = if retired {
            registry.entries.remove(&opener.id)
        } else {
            None
        };
        (removed, Some(owner))
    });
    drop(removed);
    if let Some(owner) = owner {
        finish_drains(owner);
    }
}

pub(crate) fn finish_open(
    opener: &Opener,
    context: &UnloadedTabOpenContext,
    prepared: PreparedRecoveryOpen,
    view: ViewId,
) -> Result<LoadedTabView<ViewId>, String> {
    lookup(opener, context)?;
    prepared.finish(view)
}

pub(crate) fn prepare_needs_user(
    opener: &Opener,
    context: &UnloadedTabOpenContext,
) -> Result<
    (
        WindowId,
        shell_contract::ipc::NativeInvocationOperation,
        NeedsWork,
    ),
    String,
> {
    let (restored, captured, operation, _, callback) = lookup(opener, context)?;
    let owner = REGISTRY
        .with(|registry| {
            registry
                .borrow()
                .entries
                .get(&opener.id)
                .map(|entry| entry.borrow().owner)
        })
        .ok_or("recovery opener not found")?;
    Ok((owner, operation, callback(&restored, &captured)?))
}

struct QueryWorker(Option<u64>);
impl QueryWorker {
    fn deliver(mut self, result: Result<bool, String>) {
        if let Some(id) = self.0.take() {
            post_query(id, result);
        }
    }
}
impl Drop for QueryWorker {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            post_query(id, Err("Recovery query work was discarded".into()));
        }
    }
}
fn post_query(id: u64, result: Result<bool, String>) {
    crate::host::w::post_core_checked(
        move |_, _| finish_query(id, result),
        || eprintln!("[recovery] core stopped before query completion"),
    );
}
fn finish_query(id: u64, result: Result<bool, String>) {
    let query = REGISTRY.with(|registry| registry.borrow_mut().queries.remove(&id));
    let Some(query) = query else {
        return;
    };
    let RecoveryQuery {
        owner,
        operation,
        source,
        cancel,
        pending,
        done,
        subscriptions,
    } = query;
    drop(subscriptions);
    let result = crate::ipc::native_owner::with_native_invocation_operation_effect(
        &operation,
        &source,
        &[],
        &cancel,
        &pending,
        || result,
    )
    .unwrap_or_else(|| Err("The recovery query authority was released".into()));
    done(result);
    finish_drains(owner);
}

pub(crate) fn query_needs_user(
    services: &shell_contract::runtime::Services,
    opener: &Opener,
    context: &UnloadedTabOpenContext,
    source: shell_contract::ipc::NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    done: Box<dyn FnOnce(Result<bool, String>)>,
) {
    let (owner, operation, work) = match prepare_needs_user(opener, context) {
        Ok(value) => value,
        Err(error) => return done(Err(error)),
    };
    if crate::ipc::native_owner::with_native_invocation_operation_effect(
        &operation,
        &source,
        &[],
        &cancel,
        &pending,
        || (),
    )
    .is_none()
    {
        return done(Err("The recovery query authority was released".into()));
    }
    let Some(source_loss) = source.loss_token() else {
        return done(Err("The recovery query source has no lifetime".into()));
    };
    let id = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        if registry.shutting_down || registry.closing.contains(&owner) {
            return Err("The recovery owner is closing".to_owned());
        }
        let id = registry
            .next
            .checked_add(1)
            .ok_or("recovery query ids exhausted")?;
        registry.next = id;
        Ok::<_, String>(id)
    });
    let id = match id {
        Ok(id) => id,
        Err(error) => return done(Err(error)),
    };
    REGISTRY.with(|registry| {
        registry.borrow_mut().queries.insert(
            id,
            RecoveryQuery {
                owner,
                operation: operation.clone(),
                source,
                cancel: cancel.clone(),
                pending: pending.clone(),
                done,
                subscriptions: Vec::new(),
            },
        );
    });
    for loss in [source_loss, operation.loss_token(), cancel, pending] {
        let subscription = loss.on_cancel(move || {
            post_query(id, Err("The recovery query authority was released".into()))
        });
        REGISTRY.with(|registry| {
            if let Some(query) = registry.borrow_mut().queries.get_mut(&id) {
                query.subscriptions.push(subscription);
            }
        });
    }
    let worker = QueryWorker(Some(id));
    services
        .workers
        .spawn(Box::new(move || worker.deliver(work())));
}

pub(crate) fn unregister(opener: Opener) {
    let (removed, plan, operation, owner) = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let Some(entry) = registry.entries.get(&opener.id).cloned() else {
            return (None, None, None, None);
        };
        let mut current = entry.borrow_mut();
        let owner = current.owner;
        if current.opening() {
            current.retired = true;
            let plan = Some(current.plan.clone());
            let operation = match &current.attempt {
                Attempt::Active(operation) => Some(operation.clone()),
                _ => None,
            };
            (None, plan, operation, Some(owner))
        } else {
            drop(current);
            (registry.entries.remove(&opener.id), None, None, Some(owner))
        }
    });
    if let Some(plan) = plan {
        plan.cancel();
    }
    if let Some(operation) = operation {
        operation.finish();
    }
    drop(removed);
    if let Some(owner) = owner {
        finish_drains(owner);
    }
}

pub(crate) fn close_owner(owner: WindowId) {
    cancel_and_drain(
        owner,
        Box::new(|result| {
            if let Err(error) = result {
                eprintln!("[recovery] owner cleanup remains pending: {error}");
            }
        }),
    );
}

fn finish_drains(owner: WindowId) {
    let waiters = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        if registry.entries.values().any(|entry| {
            let entry = entry.borrow();
            entry.owner == owner && entry.opening()
        }) || registry.direct.values().any(|entry| entry.owner == owner)
            || registry.queries.values().any(|query| query.owner == owner)
            || registry
                .quarantined
                .iter()
                .any(|(current, _, _)| *current == owner)
        {
            return Vec::new();
        }
        let mut ready = Vec::new();
        let mut waiting = Vec::new();
        for (current, done) in registry.drains.drain(..) {
            if current == owner {
                ready.push(done);
            } else {
                waiting.push((current, done));
            }
        }
        registry.drains = waiting;
        ready
    });
    for done in waiters {
        done(Ok(()));
    }
}
fn fail_drains(owner: WindowId, error: String) {
    let waiters = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let mut ready = Vec::new();
        let mut waiting = Vec::new();
        for (current, done) in registry.drains.drain(..) {
            if current == owner {
                ready.push(done)
            } else {
                waiting.push((current, done))
            }
        }
        registry.drains = waiting;
        ready
    });
    for done in waiters {
        done(Err(error.clone()));
    }
}

pub(crate) fn cancel_and_drain(owner: WindowId, done: Box<dyn FnOnce(Result<(), String>)>) {
    let (plans, operations, removed) = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        registry.closing.insert(owner);
        registry.drains.push((owner, done));
        let mut plans = Vec::new();
        let mut operations = Vec::new();
        for entry in registry.entries.values() {
            let mut entry = entry.borrow_mut();
            if entry.owner != owner {
                continue;
            }
            entry.retired = true;
            plans.push(entry.plan.clone());
            if let Attempt::Active(operation) = &entry.attempt {
                operations.push(operation.clone());
            }
        }
        operations.extend(
            registry
                .direct
                .values()
                .filter(|entry| entry.owner == owner)
                .map(|entry| entry.operation.clone()),
        );
        let ids: Vec<_> = registry
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                let entry = entry.borrow();
                (entry.owner == owner && !entry.opening()).then_some(*id)
            })
            .collect();
        let removed = ids
            .into_iter()
            .filter_map(|id| registry.entries.remove(&id))
            .collect::<Vec<_>>();
        (plans, operations, removed)
    });
    for plan in plans {
        plan.cancel();
    }
    for operation in operations {
        operation.finish();
    }
    drop(removed);
    let (direct, attempts) = REGISTRY.with(|registry| {
        let registry = registry.borrow();
        let direct = registry
            .direct
            .iter()
            .filter_map(|(id, entry)| {
                (entry.owner == owner && entry.failure.is_some()).then_some(*id)
            })
            .collect::<Vec<_>>();
        let attempts = registry
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                let current = entry.borrow();
                if current.owner != owner || current.held_failure.is_none() {
                    return None;
                }
                let Attempt::Active(operation) = &current.attempt else {
                    return None;
                };
                Some(NativeAttempt {
                    entry: entry.clone(),
                    opener: Opener {
                        id: *id,
                        has_needs_user: false,
                    },
                    operation: operation.clone(),
                })
            })
            .collect::<Vec<_>>();
        (direct, attempts)
    });
    for id in direct {
        retry_direct(id);
    }
    for attempt in attempts {
        attempt.retry_failure();
    }
    if REGISTRY.with(|registry| {
        registry
            .borrow()
            .quarantined
            .iter()
            .any(|(current, _, _)| *current == owner)
    }) {
        fail_drains(owner, "The recovery cleanup has no matching target".into());
    } else {
        finish_drains(owner);
    }
}

pub(crate) fn register_direct(
    owner: WindowId,
    operation: shell_contract::ipc::NativeInvocationOperation,
    rollback: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
) -> Result<u64, String> {
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        if registry.shutting_down || registry.closing.contains(&owner) {
            return Err("The recovery owner is closing".into());
        }
        let id = registry
            .next
            .checked_add(1)
            .ok_or("recovery opener ids exhausted")?;
        registry.next = id;
        registry.direct.insert(
            id,
            DirectEntry {
                owner,
                operation,
                rollback,
                failure: None,
            },
        );
        Ok(id)
    })
}

pub(crate) fn complete_direct(id: u64) {
    let removed = REGISTRY.with(|registry| registry.borrow_mut().direct.remove(&id));
    if let Some(entry) = removed {
        let owner = entry.owner;
        entry.operation.finish();
        finish_drains(owner);
    }
}

pub(crate) fn fail_direct(
    id: u64,
    error: main_core::shell::renderer_disposal::Failure,
    cleanup: shell_ipc_w::tabs::host::ViewCreationCleanupState,
    lifetime: Option<DirectLifetime>,
    core: std::sync::Arc<dyn shell_contract::runtime::Thread>,
    done: Box<dyn FnOnce(Result<ViewId, main_core::shell::renderer_disposal::Failure>)>,
) {
    use shell_ipc_w::tabs::host::ViewCreationCleanupState;
    let (cleanup, proof, complete) = match cleanup {
        ViewCreationCleanupState::Complete => (None, None, true),
        ViewCreationCleanupState::Acknowledged(proof) => (None, Some(proof), false),
        ViewCreationCleanupState::Pending(cleanup) => (Some(cleanup), None, false),
    };
    let retained = match &lifetime {
        Some(DirectLifetime::Retained(handle)) => Some(handle.clone()),
        _ => None,
    };
    let mut failure = Some(DirectFailure {
        error,
        done,
        cleanup,
        proof,
        lifetime,
        terminal: None,
        complete,
        in_flight: false,
        core,
    });
    let inserted = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let Some(entry) = registry.direct.get_mut(&id) else {
            return false;
        };
        if entry.failure.is_some() {
            return false;
        }
        entry.failure = failure.take();
        true
    });
    if !inserted {
        if let Some(failure) = failure {
            (failure.done)(Err(main_core::shell::renderer_disposal::Failure(format!(
                "{}; exact direct recovery failure is unavailable",
                failure.error.0
            ))));
        }
        return;
    }
    if let Some(handle) = retained {
        handle.observe_terminal(Box::new(move |proof| {
            let retained = REGISTRY.with(|registry| {
                let mut registry = registry.borrow_mut();
                let Some(entry) = registry.direct.get_mut(&id) else {
                    return false;
                };
                let Some(failure) = entry.failure.as_mut() else {
                    return false;
                };
                failure.terminal = Some(proof);
                true
            });
            if retained {
                retry_direct(id);
            }
        }));
        if let Err(error) = handle.retry() {
            let owner = REGISTRY
                .with(|registry| registry.borrow().direct.get(&id).map(|entry| entry.owner));
            if let Some(owner) = owner {
                fail_drains(owner, error);
            }
        }
    }
    retry_direct(id);
}

fn finish_direct_failure(id: u64) {
    let failure = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let entry = registry.direct.get_mut(&id)?;
        if !entry.rollback.borrow().is_empty() {
            return None;
        }
        entry.failure.take()
    });
    if let Some(failure) = failure {
        (failure.done)(Err(failure.error));
        complete_direct(id);
    }
}

pub(crate) fn retry_direct(id: u64) {
    enum Work {
        Cleanup(
            shell_ipc_w::tabs::host::ViewCreationCleanup,
            std::sync::Arc<dyn shell_contract::runtime::Thread>,
        ),
        Rollback(
            shell_contract::ipc::NativeBrowserRetirement,
            shell_ipc_w::tabs::host::EditorSeedRollbackStore,
            WindowId,
        ),
        Retained(shell_ipc_w::tabs::AdoptedLifetimeHandle, WindowId),
        Finish,
        Error(WindowId, String),
    }
    let work = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let entry = registry.direct.get_mut(&id)?;
        let failure = entry.failure.as_mut()?;
        if failure.in_flight {
            return None;
        }
        if failure.proof.is_none() && !failure.complete {
            if let Some(cleanup) = failure.cleanup.clone() {
                failure.in_flight = true;
                return Some(Work::Cleanup(cleanup, failure.core.clone()));
            }
            return Some(Work::Error(
                entry.owner,
                "The direct recovery cleanup is unavailable".into(),
            ));
        }
        if let Some(DirectLifetime::Retained(handle)) = &failure.lifetime {
            let Some(terminal) = failure.terminal.as_ref() else {
                return Some(Work::Retained(handle.clone(), entry.owner));
            };
            if !handle.matches_retirement(terminal)
                || failure.proof.as_ref().is_some_and(|proof| {
                    !handle.matches_retirement(proof)
                        || std::mem::discriminant(proof) != std::mem::discriminant(terminal)
                })
            {
                return Some(Work::Error(
                    entry.owner,
                    "The direct recovery retirement does not match its lifetime".into(),
                ));
            }
            if !entry.rollback.borrow().is_empty() {
                return Some(Work::Error(
                    entry.owner,
                    "The retained recovery retirement has not settled its receipts".into(),
                ));
            }
            return Some(Work::Finish);
        }
        if matches!(&failure.lifetime, Some(DirectLifetime::CallerOwned(_)))
            && failure.proof.is_none()
        {
            return Some(Work::Error(
                entry.owner,
                "The caller-owned recovery lifetime has no native retirement".into(),
            ));
        }
        if let (Some(DirectLifetime::CallerOwned(lifetime)), Some(proof)) =
            (&failure.lifetime, &failure.proof)
            && !lifetime.matches_retirement(proof)
        {
            return Some(Work::Error(
                entry.owner,
                "The direct recovery cleanup belongs to another lifetime".into(),
            ));
        }
        if let Some(proof) = failure.proof.clone() {
            failure.in_flight = true;
            return Some(Work::Rollback(proof, entry.rollback.clone(), entry.owner));
        }
        if entry.rollback.borrow().is_empty() {
            Some(Work::Finish)
        } else {
            Some(Work::Error(
                entry.owner,
                "Missing close proof for an admitted direct seed".into(),
            ))
        }
    });
    match work {
        Some(Work::Cleanup(cleanup, core)) => {
            let guard = DirectCleanupGuard {
                id,
                core: core.clone(),
                pending: true,
            };
            cleanup(Box::new(move |result| {
                core.post(Box::new(move || {
                    let mut guard = guard;
                    guard.pending = false;
                    let mut failed = None;
                    REGISTRY.with(|registry| {
                        if let Some(entry) = registry.borrow_mut().direct.get_mut(&id)
                            && let Some(failure) = entry.failure.as_mut()
                        {
                            failure.in_flight = false;
                            match result {
                                Ok(proof) => failure.proof = Some(proof),
                                Err(error) => failed = Some((entry.owner, error)),
                            }
                        }
                    });
                    if let Some((owner, error)) = failed {
                        fail_drains(owner, error);
                    } else {
                        retry_direct(id);
                    }
                }));
            }));
        }
        Some(Work::Rollback(proof, rollback, owner)) => {
            let guard = DirectRollbackGuard { id, pending: true };
            shell_ipc_w::tabs::rollback_editor_receipts(
                rollback,
                Some(proof),
                true,
                Box::new(move |result| {
                    let mut guard = guard;
                    guard.pending = false;
                    REGISTRY.with(|registry| {
                        if let Some(failure) = registry
                            .borrow_mut()
                            .direct
                            .get_mut(&id)
                            .and_then(|entry| entry.failure.as_mut())
                        {
                            failure.in_flight = false;
                        }
                    });
                    match result {
                        Ok(()) => finish_direct_failure(id),
                        Err(error) => fail_drains(owner, error),
                    }
                }),
            );
        }
        Some(Work::Retained(handle, owner)) => {
            if let Err(error) = handle.retry() {
                fail_drains(owner, error);
            }
        }
        Some(Work::Finish) => finish_direct_failure(id),
        Some(Work::Error(owner, error)) => fail_drains(owner, error),
        None => {}
    }
}

pub(crate) fn retain_bound(owner: WindowId, live_tab_ids: &[String]) {
    let (plans, operations, removed) = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let is_removed = |entry: &Entry| {
            entry.owner == owner
                && entry
                    .tab_id
                    .as_ref()
                    .is_some_and(|id| !live_tab_ids.contains(id))
        };
        let mut plans = Vec::new();
        let mut operations = Vec::new();
        for entry in registry.entries.values() {
            let mut entry = entry.borrow_mut();
            if !is_removed(&entry) {
                continue;
            }
            entry.retired = true;
            plans.push(entry.plan.clone());
            if let Attempt::Active(operation) = &entry.attempt {
                operations.push(operation.clone());
            }
        }
        let ids: Vec<_> = registry
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                let entry = entry.borrow();
                (is_removed(&entry) && !entry.opening()).then_some(*id)
            })
            .collect();
        let removed = ids
            .into_iter()
            .filter_map(|id| registry.entries.remove(&id))
            .collect::<Vec<_>>();
        (plans, operations, removed)
    });
    for plan in plans {
        plan.cancel();
    }
    for operation in operations {
        operation.finish();
    }
    drop(removed);
    finish_drains(owner);
}

/// Runs on core before worker and CEF shutdown. All callbacks have settled when done runs.
pub(crate) fn cancel_all_and_drain(done: Box<dyn FnOnce(Result<(), String>)>) {
    let remaining = Rc::new(Cell::new(3));
    let completion = Rc::new(RefCell::new(Some(done)));
    let signal = || {
        let remaining = remaining.clone();
        let completion = completion.clone();
        Box::new(move |result: Result<(), String>| {
            if let Err(error) = result {
                let done = completion.borrow_mut().take();
                if let Some(done) = done {
                    done(Err(error));
                }
                return;
            }
            let count = remaining.get() - 1;
            remaining.set(count);
            if count == 0 {
                let done = completion.borrow_mut().take();
                if let Some(done) = done {
                    done(Ok(()));
                }
            }
        }) as Box<dyn FnOnce(Result<(), String>)>
    };
    REGISTRY.with(|registry| registry.borrow_mut().shutting_down = true);
    let runtime_done = signal();
    super::recovery_launch::cancel_live_and_drain(Box::new(move || runtime_done(Ok(()))));
    let registered_done = signal();
    cancel_registered_and_drain(registered_done);
    let mut tabs = None;
    super::w::on_core_local(|provided| {
        tabs = provided
            .all::<shell_ipc_w::tabs::TabsState>()
            .into_iter()
            .next();
    });
    let completed = signal();
    if let Some(tabs) = tabs {
        let retained = tabs.clone();
        tabs.cancel_creations_and_drain(Box::new(move |result| {
            drop(retained);
            completed(result);
        }));
    } else {
        completed(Ok(()));
    }
}

fn cancel_registered_and_drain(done: Box<dyn FnOnce(Result<(), String>)>) {
    let owners = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        registry.shutting_down = true;
        registry
            .entries
            .values()
            .map(|entry| entry.borrow().owner)
            .chain(registry.direct.values().map(|entry| entry.owner))
            .chain(registry.queries.values().map(|query| query.owner))
            .chain(registry.quarantined.iter().map(|(owner, _, _)| *owner))
            .collect::<std::collections::HashSet<_>>()
    });
    if owners.is_empty() {
        return done(Ok(()));
    }
    let remaining = Rc::new(std::cell::Cell::new(owners.len()));
    let completion = Rc::new(RefCell::new(Some(done)));
    for owner in owners {
        let remaining = remaining.clone();
        let completion = completion.clone();
        cancel_and_drain(
            owner,
            Box::new(move |result| {
                if let Err(error) = result {
                    let done = completion.borrow_mut().take();
                    if let Some(done) = done {
                        done(Err(error));
                    }
                    return;
                }
                let count = remaining.get() - 1;
                remaining.set(count);
                if count == 0 {
                    let done = completion.borrow_mut().take();
                    if let Some(done) = done {
                        done(Ok(()));
                    }
                }
            }),
        );
    }
}

pub(crate) fn live_callbacks(
    services: shell_contract::runtime::Services,
    provided: Rc<shell_contract::Provided>,
    gate: Rc<shell_ipc_u::launch_list::LaunchList>,
    shared: std::sync::Arc<
        std::sync::Mutex<main_core::packages::electron_utils::recovery_store::SharedRecoveryState>,
    >,
    user_data: String,
) -> (Rc<Open>, Rc<NeedsUser>) {
    let core = services.core.clone();
    let after_sheets_open = Rc::new(move || {
        if shell_ipc_d1::sheets::views().is_none() {
            return Err("Sheets editor is not registered".to_owned());
        }
        core.post(Box::new(|| {
            if let Err(error) = super::w_tabs::nudge_recovery_sheets() {
                eprintln!("[recovery] {error}");
            }
        }));
        Ok(())
    });
    callbacks(provided, gate, shared, user_data, after_sheets_open)
}

pub(crate) fn callbacks(
    provided: Rc<shell_contract::Provided>,
    gate: Rc<shell_ipc_u::launch_list::LaunchList>,
    shared: std::sync::Arc<
        std::sync::Mutex<main_core::packages::electron_utils::recovery_store::SharedRecoveryState>,
    >,
    user_data: String,
    after_sheets_open: Rc<dyn Fn() -> Result<(), String>>,
) -> (Rc<Open>, Rc<NeedsUser>) {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
        RecoveryOpenUntitledRequest,
    };
    use main_core::packages::electron_utils::recovery_store::{
        RecoveryStoreOptions, create_recovery_store,
    };
    use main_core::shell::recovery_shell::{OpenContext, RestoredTabOpenerDeps};

    struct Unblocked;
    impl RestoredTabOpenerDeps for Unblocked {
        fn blocked(&self) -> bool {
            false
        }
    }

    let open_gate = gate.clone();
    let prepare: Rc<PrepareOpen> = Rc::new(move |restored, context| {
        if open_gate.blocking() {
            return Err("the launch recovery list is not answered yet".into());
        }
        match restored {
            RestoredTabOpener::File => {
                let after_sheets_open = after_sheets_open.clone();
                let kind = context.kind;
                let path = context.file_path.clone();
                let editor = provided.all::<dyn RecoveryEditorContract<ViewId,shell_contract::ipc::RecoveryOpenTarget>>().into_iter()
                    .find(|editor| editor.kind() == kind.as_str());
                let request = path.as_ref().filter(|path| !path.is_empty()).map(|path| {
                    RecoveryOpenFileRequest {
                        file_path: path.clone(),
                        decision: RecoveryOpenDecision::Original,
                    }
                });
                if request.is_some()
                    && editor.is_none()
                    && main_core::packages::electron_utils::recovery_store::is_recovery_editor_kind(
                        kind.as_str(),
                    )
                {
                    return Err(format!(
                        "the {} recovery editor is unavailable",
                        kind.as_str()
                    ));
                }
                let blank_docs = if kind == TabKind::Docs && request.is_none() {
                    Some(
                        provided
                            .all::<shell_ipc_d2::docs::editors::DocsEditors>()
                            .into_iter()
                            .next()
                            .ok_or("the docs editor is unavailable")?,
                    )
                } else {
                    None
                };
                let epub = if kind == TabKind::Epub && request.is_some() {
                    Some(
                        provided
                            .all::<shell_ipc_d2::epub::EpubEditors>()
                            .into_iter()
                            .next()
                            .ok_or("the EPUB editor is unavailable")?,
                    )
                } else {
                    None
                };
                let title = context.title.clone();
                Ok(PreparedRecoveryOpen {
                    kind,
                    path,
                    untitled: false,
                    operation: None,
                    attempt: None,
                    before_load: Some(Box::new(move |view, target, done| {
                        match (editor, request, blank_docs, epub) {
                            (Some(editor), Some(request), _, _) => {
                                editor.open_file_with_decision(&view, &request, target, done)
                            }
                            (_, Some(request), _, Some(editor)) => {
                                editor.queue_recovery_direct(view, target, request.file_path, done)
                            }
                            (_, None, Some(editor), _) => {
                                editor.queue_blank_recovery(view, target, title, done)
                            }
                            _ => done(Err(main_core::shell::renderer_disposal::Failure(format!(
                                "the {} recovery initialization is unavailable",
                                kind.as_str()
                            )))),
                        }
                    })),
                    after: Box::new(move |_| {
                        if kind == TabKind::Sheets {
                            after_sheets_open()
                        } else {
                            Ok(())
                        }
                    }),
                })
            }
            RestoredTabOpener::UntitledCopy {
                copy,
                save_as_default_path,
            } => {
                let editor = provided.all::<dyn RecoveryEditorContract<ViewId,shell_contract::ipc::RecoveryOpenTarget>>().into_iter()
                    .find(|editor| editor.kind() == context.kind.as_str())
                    .ok_or_else(|| format!("the {} editor cannot open a recovery copy", context.kind.as_str()))?;
                let request = RecoveryOpenUntitledRequest {
                    copy: copy.clone(),
                    title: context.title.clone(),
                    save_as_default_path: save_as_default_path
                        .clone()
                        .filter(|path| !path.is_empty()),
                };
                Ok(PreparedRecoveryOpen {
                    kind: context.kind,
                    path: None,
                    untitled: true,
                    operation: None,
                    attempt: None,
                    before_load: Some(Box::new(move |view, target, done| {
                        editor.open_untitled_copy(&view, &request, target, done)
                    })),
                    after: Box::new(|_| Ok(())),
                })
            }
        }
    });
    let open = Rc::new(Open { prepare });
    let needs_user: Rc<NeedsUser> = Rc::new(move |restored, context| {
        if gate.blocking() {
            return Ok(Box::new(|| Ok(true)) as NeedsWork);
        }
        let restored = restored.clone();
        let context = OpenContext {
            kind: context.kind.as_str().to_owned(),
            title: context.title.clone(),
            file_path: context.file_path.clone(),
        };
        let shared = shared.clone();
        let user_data = user_data.clone();
        Ok(Box::new(move || {
            let shared = shared
                .lock()
                .map_err(|_| "recovery state is unavailable".to_owned())?;
            let dir = || user_data.clone();
            let now = || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |duration| duration.as_millis() as f64)
            };
            let uuid = || jscompat::random_uuid();
            let store = create_recovery_store(
                RecoveryStoreOptions {
                    user_data_dir: &dir,
                    now_ms: &now,
                    random_uuid: &uuid,
                },
                &shared,
            );
            Ok(restored.needs_user(&store, &Unblocked, &context))
        }) as NeedsWork)
    });
    (open, needs_user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    use shell_contract::runtime::CancelToken;
    use shell_ipc_w::tabs::RecoveryAttempt;
    use std::cell::Cell;

    struct Core;
    impl shell_contract::runtime::Thread for Core {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            job()
        }
        fn is_current(&self) -> bool {
            true
        }
    }

    struct DropFirstPost(std::sync::atomic::AtomicBool);
    impl shell_contract::runtime::Thread for DropFirstPost {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            if self.0.swap(false, std::sync::atomic::Ordering::SeqCst) {
                drop(job);
            } else {
                job();
            }
        }
        fn is_current(&self) -> bool {
            true
        }
    }

    fn bind_attempt(
        attempt: &NativeAttempt,
        view: ViewId,
    ) -> (
        NativeAttemptBridge,
        shell_contract::ipc::NativeClosureIssuer,
        shell_contract::ipc::RecoveryOpenTarget,
    ) {
        let created = shell_contract::runtime::RendererGeneration::new();
        let (issuer, reservation, owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, created.clone());
        let source = shell_contract::ipc::NativeRequestOwner {
            view: ViewId(900),
            context: shell_contract::ipc::NativeContext::with_owner_loss(
                (),
                shell_contract::runtime::RendererGeneration::new(),
                CancelToken::default(),
            ),
        };
        let request = shell_contract::ipc::RecoveryRequestConstraint {
            source,
            participants: Vec::new(),
            cancel: CancelToken::default(),
            pending: CancelToken::default(),
        };
        let target = shell_contract::ipc::RecoveryOpenTarget::bind_request(
            shell_contract::ipc::RecoveryOpenContext::NativeStartup(attempt.operation.clone()),
            view,
            created,
            reservation,
            owner,
            request,
        )
        .unwrap();
        let bridge = NativeAttemptBridge {
            attempt: attempt.clone(),
            core: std::sync::Arc::new(Core),
        };
        bridge.bind_target(&target).unwrap();
        (bridge, issuer, target)
    }

    struct TrackedReceipt {
        target: shell_contract::ipc::RecoveryOpenTarget,
        closed: Cell<u32>,
        superseded: Cell<u32>,
        rollback: Cell<u32>,
        fail_first: bool,
    }
    impl shell_ipc_w::tabs::host::EditorSeedRollback for TrackedReceipt {
        fn retire_closed(
            &self,
            proof: shell_contract::ipc::NativeClosureReceipt,
            done: Box<dyn FnOnce(Result<(), String>)>,
        ) {
            assert!(matches_retirement(
                &self.target,
                &shell_contract::ipc::NativeBrowserRetirement::Closed(proof)
            ));
            self.closed.set(self.closed.get() + 1);
            done(Ok(()));
        }
        fn retire_adopted(
            &self,
            _: shell_contract::ipc::NativeClosureReceipt,
            done: Box<dyn FnOnce(Result<(), String>)>,
        ) {
            done(Err("unexpected adoption".into()));
        }
        fn retire_superseded(
            &self,
            proof: shell_contract::ipc::NativeBrowserSuperseded,
            done: Box<dyn FnOnce(Result<(), String>)>,
        ) {
            assert!(proof.matches(
                self.target.closure_reservation(),
                self.target.browser_owner()
            ));
            self.superseded.set(self.superseded.get() + 1);
            done(Ok(()));
        }
        fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
            self.rollback.set(self.rollback.get() + 1);
            if self.fail_first && self.rollback.get() == 1 {
                done(Err("rollback failed".into()));
            } else {
                done(Ok(()));
            }
        }
    }

    fn context(id: &str) -> UnloadedTabOpenContext {
        UnloadedTabOpenContext {
            tab_id: id.into(),
            kind: TabKind::Docs,
            title: "later title".into(),
            file_path: None,
        }
    }
    fn finish(attempt: NativeAttempt, success: bool) {
        let owner = attempt.finish(success).unwrap();
        finish_drains(owner);
    }
    fn make(
        owner: WindowId,
        fail_first: bool,
    ) -> (
        Opener,
        shell_contract::ipc::NativeInvocationOperation,
        Rc<Cell<u32>>,
    ) {
        let spec = UnloadedTabSpec {
            kind: "docs".into(),
            title: "selected title".into(),
            file_path: None,
            opener: RestoredTabOpener::File,
        };
        let issuer = Rc::new(NativeInvocationIssuer::new(CancelToken::default()));
        let root = super::super::recovery_launch::issuer::test_startup(&issuer);
        let factory: super::super::recovery_launch::issuer::RecoveryIssuer =
            Rc::new(move |selection, cancel| issuer.issue_recovery_child(&root, selection, cancel));
        let plan =
            super::super::recovery_launch::issuer::RecoveryPlan::capture(&factory, &spec).unwrap();
        let operation = plan.guard();
        let calls = Rc::new(Cell::new(0));
        let observed = calls.clone();
        let open = Rc::new(Open {
            prepare: Rc::new(move |_, context| {
                observed.set(observed.get() + 1);
                assert_eq!(context.title, "selected title");
                if fail_first && observed.get() == 1 {
                    return Err("editor not ready".into());
                }
                Ok(PreparedRecoveryOpen {
                    kind: TabKind::Docs,
                    path: None,
                    untitled: false,
                    operation: None,
                    attempt: None,
                    before_load: None,
                    after: Box::new(|_| Ok(())),
                })
            }),
        });
        let opener = register(
            owner,
            TabKind::Docs,
            spec,
            plan,
            open,
            Rc::new(|_, _| Ok(Box::new(|| Ok(false)) as NeedsWork)),
        )
        .unwrap();
        (opener, operation, calls)
    }
    #[test]
    fn command_child_uses_captured_selection_and_exact_attempt() {
        let (opener, _, _) = make(WindowId(99), false);
        let tab = context("command");
        let issuer = NativeInvocationIssuer::new(CancelToken::default());
        let root = issuer.issue_command(
            shell_contract::ipc::NativeEditorCommand::OpenFile {
                surface: shell_contract::Surface::Docs,
                path: "/selected.docx".into(),
            },
            CancelToken::default(),
        );
        let mut prepared = prepare_open_with(&opener, &tab, |plan| {
            issuer.issue_command_recovery_child(
                &root,
                plan.selection().clone(),
                CancelToken::default(),
            )
        })
        .unwrap();
        let operation = prepared.operation.take().unwrap();
        assert!(operation.is_child_of(&root));
        assert!(
            matches!(operation.selection(),NativeInvocationSelection::RecoveryBlank{title,..} if title=="selected title")
        );
        let attempt = prepared.attempt.take().unwrap();
        assert!(
            attempt
                .operation
                .authority_generation()
                .same_generation(operation.authority_generation())
        );
        finish(attempt, false);
    }
    #[test]
    fn failures_remain_retryable_and_success_consumes_selected_opener() {
        let (opener, _, calls) = make(WindowId(10), true);
        let tab = context("tab-a");
        assert!(!(prepare_needs_user(&opener, &tab).unwrap().2)().unwrap());
        assert!(prepare_open(&opener, &context("tab-b")).is_err());
        assert!(prepare_open(&opener, &tab).is_err());
        let mut prepared = prepare_open(&opener, &tab).unwrap();
        let attempt = prepared.attempt.take().unwrap();
        assert_eq!(
            finish_open(&opener, &tab, prepared, ViewId(5))
                .unwrap()
                .view,
            ViewId(5)
        );
        finish(attempt, true);
        assert!(prepare_open(&opener, &tab).is_err());
        assert_eq!(calls.get(), 2);
    }
    #[test]
    fn failed_attempt_retries_only_after_cleanup_with_a_fresh_selected_operation() {
        let (opener, plan, calls) = make(WindowId(11), false);
        let tab = context("tab");
        let mut first = prepare_open(&opener, &tab).unwrap();
        let first_op = first.operation.take().unwrap();
        let first_attempt = first.attempt.take().unwrap();
        let first_target = shell_contract::runtime::RendererGeneration::new();
        first_op.reserve_target(ViewId(1), first_target).unwrap();
        assert!(prepare_open(&opener, &tab).is_err());
        assert!(first_op.with_active(|| ()).is_some());
        // The caller invokes this only after the failed browser's cleanup acknowledgement.
        finish(first_attempt, false);
        assert!(first_op.with_active(|| ()).is_none());
        assert!(plan.with_active(|| ()).is_some());
        let mut second = prepare_open(&opener, &tab).unwrap();
        let second_op = second.operation.take().unwrap();
        let second_attempt = second.attempt.take().unwrap();
        assert_eq!(first_op.selection(), second_op.selection());
        assert!(
            second_op
                .reserve_target(
                    ViewId(2),
                    shell_contract::runtime::RendererGeneration::new()
                )
                .is_ok()
        );
        assert_eq!(
            finish_open(&opener, &tab, second, ViewId(2)).unwrap().view,
            ViewId(2)
        );
        finish(second_attempt, true);
        assert!(second_op.with_active(|| ()).is_none());
        assert!(plan.with_active(|| ()).is_none());
        assert!(prepare_open(&opener, &tab).is_err());
        assert_eq!(calls.get(), 3);
    }
    #[test]
    fn unconfirmed_cleanup_retains_failure_and_blocks_retry_and_owner_drain() {
        let (opener, _, _) = make(WindowId(12), false);
        let tab = context("tab");
        let mut prepared = prepare_open(&opener, &tab).unwrap();
        let attempt = prepared.attempt.take().unwrap();
        let (_bridge, issuer, _target) = bind_attempt(&attempt, ViewId(12));
        let answered = Rc::new(Cell::new(false));
        let answer = answered.clone();
        let observed = Rc::new(Cell::new(0));
        let notifications = observed.clone();
        attempt.entry.borrow_mut().held_failure = Some(HeldFailure {
            error: "close failed".into(),
            cleanup: None,
            retirement: None,
            delivery: None,
            rollback_progress: None,
            indeterminate: None,
            core: None,
            in_flight: false,
            on_cleanup_error: Some(Rc::new(move |_| notifications.set(notifications.get() + 1))),
            done: Box::new(move |result| {
                assert!(matches!(result,Err(error) if error=="close failed"));
                answer.set(true);
            }),
        });
        assert!(attempt.finish(false).is_err());
        assert!(prepare_open(&opener, &tab).is_err());
        let failed = Rc::new(Cell::new(false));
        let failure = failed.clone();
        cancel_and_drain(
            WindowId(12),
            Box::new(move |result| {
                assert!(result.is_err());
                failure.set(true)
            }),
        );
        assert_eq!(observed.get(), 1);
        assert!(!answered.get());
        assert!(failed.get());
        let drained = Rc::new(Cell::new(false));
        let proof = shell_contract::ipc::NativeBrowserRetirement::Closed(issuer.closed());
        attempt
            .entry
            .borrow_mut()
            .held_failure
            .as_mut()
            .unwrap()
            .retirement = Some(proof);
        attempt.closed().unwrap();
        let flag = drained.clone();
        cancel_and_drain(
            WindowId(12),
            Box::new(move |result| {
                result.unwrap();
                flag.set(true)
            }),
        );
        assert!(answered.get());
        assert!(drained.get());
        assert!(prepare_open(&opener, &tab).is_err());
    }
    #[test]
    fn foreign_retirement_does_not_finish_the_original_attempt() {
        let (opener, _, _) = make(WindowId(81), false);
        let attempt = prepare_open(&opener, &context("foreign"))
            .unwrap()
            .attempt
            .take()
            .unwrap();
        let (bridge, _issuer, target) = bind_attempt(&attempt, ViewId(81));
        let foreign =
            shell_contract::ipc::NativeClosureIssuer::new(ViewId(81), target.created().clone())
                .0
                .closed();
        let reported = Rc::new(Cell::new(false));
        let report = reported.clone();
        let finished = Rc::new(Cell::new(false));
        let done = finished.clone();
        bridge.fail_after_retirement(
            "original".into(),
            shell_contract::ipc::NativeBrowserRetirement::Closed(foreign),
            Rc::new(move |_| report.set(true)),
            Box::new(move || done.set(true)),
        );
        assert!(reported.get());
        assert!(!finished.get());
        assert!(attempt.entry.borrow().held_failure.is_none());
        assert!(attempt.finish(false).is_err());
    }

    #[test]
    fn successor_closure_cannot_finish_the_original_attempt() {
        let (opener, _, _) = make(WindowId(85), false);
        let attempt = prepare_open(&opener, &context("next-owner"))
            .unwrap()
            .attempt
            .take()
            .unwrap();
        let (bridge, issuer, target) = bind_attempt(&attempt, ViewId(85));
        let reservation = target.closure_reservation().clone();
        let owner = target.browser_owner().clone();
        reservation.with_initial_owner(&owner, |_| Ok(())).unwrap();
        let replacement = reservation.prepare_replacement(&owner).unwrap();
        let (_successor, _, ()) = replacement.commit(|_, _| Ok(())).unwrap();
        let proof = shell_contract::ipc::NativeBrowserRetirement::Closed(issuer.closed());
        let reported = Rc::new(Cell::new(false));
        let report = reported.clone();
        let finished = Rc::new(Cell::new(false));
        let done = finished.clone();
        bridge.fail_after_retirement(
            "original".into(),
            proof,
            Rc::new(move |_| report.set(true)),
            Box::new(move || done.set(true)),
        );
        assert!(reported.get());
        assert!(!finished.get());
        assert!(attempt.entry.borrow().held_failure.is_none());
        assert!(attempt.finish(false).is_err());
    }

    #[test]
    fn superseded_retirement_keeps_the_successor_and_does_not_roll_it_back() {
        let (opener, _, _) = make(WindowId(82), false);
        let attempt = prepare_open(&opener, &context("successor"))
            .unwrap()
            .attempt
            .take()
            .unwrap();
        let (bridge, _issuer, target) = bind_attempt(&attempt, ViewId(82));
        let reservation = target.closure_reservation().clone();
        let owner = target.browser_owner().clone();
        reservation.with_initial_owner(&owner, |_| Ok(())).unwrap();
        let replacement = reservation.prepare_replacement(&owner).unwrap();
        let (successor, proof, ()) = replacement.commit(|_, _| Ok(())).unwrap();
        let receipt = Rc::new(TrackedReceipt {
            target,
            closed: Cell::new(0),
            superseded: Cell::new(0),
            rollback: Cell::new(0),
            fail_first: false,
        });
        attempt
            .entry
            .borrow()
            .rollback
            .borrow_mut()
            .push(receipt.clone());
        let finished = Rc::new(Cell::new(false));
        let done = finished.clone();
        bridge.fail_after_retirement(
            "original".into(),
            shell_contract::ipc::NativeBrowserRetirement::Superseded(proof),
            Rc::new(|error| panic!("{error}")),
            Box::new(move || done.set(true)),
        );
        assert!(finished.get());
        assert_eq!(receipt.superseded.get(), 1);
        assert_eq!(receipt.rollback.get(), 0);
        assert!(reservation.with_owner(&successor, |_| Ok(())).is_ok());
    }

    #[test]
    fn rollback_retry_does_not_repeat_the_acknowledged_actor_retirement() {
        let (opener, _, _) = make(WindowId(83), false);
        let attempt = prepare_open(&opener, &context("retry"))
            .unwrap()
            .attempt
            .take()
            .unwrap();
        let (bridge, issuer, target) = bind_attempt(&attempt, ViewId(83));
        let receipt = Rc::new(TrackedReceipt {
            target,
            closed: Cell::new(0),
            superseded: Cell::new(0),
            rollback: Cell::new(0),
            fail_first: true,
        });
        attempt
            .entry
            .borrow()
            .rollback
            .borrow_mut()
            .push(receipt.clone());
        let reported = Rc::new(Cell::new(false));
        let report = reported.clone();
        let finished = Rc::new(Cell::new(false));
        let done = finished.clone();
        bridge.fail_after_retirement(
            "original".into(),
            shell_contract::ipc::NativeBrowserRetirement::Closed(issuer.closed()),
            Rc::new(move |_| report.set(true)),
            Box::new(move || done.set(true)),
        );
        assert!(reported.get());
        assert!(!finished.get());
        bridge.retry_cleanup();
        assert!(finished.get());
        assert_eq!(receipt.closed.get(), 1);
        assert_eq!(receipt.rollback.get(), 2);
    }
    #[test]
    fn discarded_actor_completion_is_not_replayed() {
        struct Dropping(Cell<u32>);
        impl shell_ipc_w::tabs::host::EditorSeedRollback for Dropping {
            fn retire_closed(
                &self,
                _: shell_contract::ipc::NativeClosureReceipt,
                done: Box<dyn FnOnce(Result<(), String>)>,
            ) {
                self.0.set(self.0.get() + 1);
                drop(done);
            }
            fn retire_adopted(
                &self,
                _: shell_contract::ipc::NativeClosureReceipt,
                done: Box<dyn FnOnce(Result<(), String>)>,
            ) {
                done(Err("unexpected adoption".into()));
            }
            fn retire_superseded(
                &self,
                _: shell_contract::ipc::NativeBrowserSuperseded,
                done: Box<dyn FnOnce(Result<(), String>)>,
            ) {
                done(Err("unexpected supersession".into()));
            }
            fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
                done(Err("unexpected rollback".into()));
            }
        }
        let (opener, _, _) = make(WindowId(86), false);
        let attempt = prepare_open(&opener, &context("discarded"))
            .unwrap()
            .attempt
            .take()
            .unwrap();
        let (bridge, issuer, _target) = bind_attempt(&attempt, ViewId(86));
        let receipt = Rc::new(Dropping(Cell::new(0)));
        attempt
            .entry
            .borrow()
            .rollback
            .borrow_mut()
            .push(receipt.clone());
        let errors = Rc::new(Cell::new(0));
        let reported = errors.clone();
        let finished = Rc::new(Cell::new(false));
        let done = finished.clone();
        bridge.fail_after_retirement(
            "original".into(),
            shell_contract::ipc::NativeBrowserRetirement::Closed(issuer.closed()),
            Rc::new(move |_| reported.set(reported.get() + 1)),
            Box::new(move || done.set(true)),
        );
        assert_eq!(errors.get(), 1);
        bridge.retry_cleanup();
        assert_eq!(errors.get(), 2);
        assert_eq!(receipt.0.get(), 1);
        assert!(!finished.get());
    }
    #[test]
    fn dropped_core_delivery_keeps_the_real_retirement_without_reclosing() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        let (opener, _, _) = make(WindowId(84), false);
        let attempt = prepare_open(&opener, &context("delivery"))
            .unwrap()
            .attempt
            .take()
            .unwrap();
        let (mut bridge, issuer, _target) = bind_attempt(&attempt, ViewId(84));
        bridge.core = Arc::new(DropFirstPost(AtomicBool::new(true)));
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        let issuer = Mutex::new(Some(issuer));
        let cleanup: RecoveryCleanup = Arc::new(move |done| {
            invoked.fetch_add(1, Ordering::SeqCst);
            let issuer = issuer.lock().unwrap().take().unwrap();
            done(Ok(shell_contract::ipc::NativeBrowserRetirement::Closed(
                issuer.closed(),
            )));
        });
        let reported = Rc::new(Cell::new(false));
        let report = reported.clone();
        let finished = Rc::new(Cell::new(false));
        let done = finished.clone();
        bridge.fail(
            "original".into(),
            Some(cleanup),
            Rc::new(move |_| report.set(true)),
            Box::new(move |result| {
                assert!(result.is_err());
                done.set(true)
            }),
        );
        assert!(reported.get());
        assert!(!finished.get());
        bridge.retry_cleanup();
        assert!(finished.get());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn closing_owner_revokes_only_its_selected_operation() {
        let (first, first_op, _) = make(WindowId(20), false);
        let (second, second_op, _) = make(WindowId(21), false);
        close_owner(WindowId(20));
        assert!(first_op.with_active(|| ()).is_none());
        assert!(second_op.with_active(|| ()).is_some());
        assert!(prepare_open(&first, &context("tab")).is_err());
        let mut prepared = prepare_open(&second, &context("tab")).unwrap();
        let child = prepared.operation.take().unwrap();
        let attempt = prepared.attempt.take().unwrap();
        close_owner(WindowId(21));
        assert!(second_op.with_active(|| ()).is_none());
        assert!(child.with_active(|| ()).is_none());
        assert!(prepare_open(&second, &context("tab")).is_err());
        finish(attempt, false);
        assert!(prepare_open(&second, &context("tab")).is_err());
    }
    #[test]
    fn removing_bound_tabs_revokes_only_their_operations() {
        let (removed, removed_op, _) = make(WindowId(30), false);
        let (kept, kept_op, _) = make(WindowId(30), false);
        let (unbound, unbound_op, _) = make(WindowId(30), false);
        bind(removed, "closed").unwrap();
        bind(kept, "live").unwrap();
        retain_bound(WindowId(30), &["live".into()]);
        assert!(removed_op.with_active(|| ()).is_none());
        assert!(kept_op.with_active(|| ()).is_some());
        assert!(unbound_op.with_active(|| ()).is_some());
        assert!(prepare_open(&removed, &context("closed")).is_err());
        let kept_attempt = prepare_open(&kept, &context("live"))
            .unwrap()
            .attempt
            .take()
            .unwrap();
        let unbound_attempt = prepare_open(&unbound, &context("new"))
            .unwrap()
            .attempt
            .take()
            .unwrap();
        close_owner(WindowId(30));
        finish(kept_attempt, false);
        finish(unbound_attempt, false);
    }
    #[test]
    fn shutdown_waits_for_started_openers_and_direct_callbacks() {
        let (opener, operation, _) = make(WindowId(40), false);
        let attempt = prepare_open(&opener, &context("live"))
            .unwrap()
            .attempt
            .take()
            .unwrap();
        let issuer = NativeInvocationIssuer::new(CancelToken::default());
        let root = super::super::recovery_launch::issuer::test_startup(&issuer);
        let direct = issuer
            .issue_recovery_child(
                &root,
                NativeInvocationSelection::RecoveryBlank {
                    surface: shell_contract::Surface::Docs,
                    title: "direct".into(),
                },
                CancelToken::default(),
            )
            .unwrap();
        assert!(direct.is_child_of(&root));
        let direct_id = register_direct(WindowId(41), direct.clone(), Rc::default()).unwrap();
        let settled = Rc::new(Cell::new(false));
        let result = settled.clone();
        cancel_all_and_drain(Box::new(move |outcome| {
            outcome.unwrap();
            result.set(true);
        }));
        assert!(!settled.get());
        assert!(operation.with_active(|| ()).is_none());
        assert!(direct.with_active(|| ()).is_none());
        finish(attempt, false);
        assert!(!settled.get());
        complete_direct(direct_id);
        assert!(settled.get());
    }
    #[test]
    fn direct_failure_waits_for_close_and_each_async_rollback() {
        struct Core;
        impl shell_contract::runtime::Thread for Core {
            fn post(&self, job: Box<dyn FnOnce() + Send>) {
                job()
            }
            fn is_current(&self) -> bool {
                true
            }
        }
        type DelayedCall = Box<dyn FnOnce(Result<(), String>)>;
        struct Delayed {
            reservation: shell_contract::ipc::NativeClosureReservation,
            generation: shell_contract::runtime::RendererGeneration,
            retire: RefCell<Option<DelayedCall>>,
            rollback: RefCell<Option<DelayedCall>>,
        }
        impl shell_ipc_w::tabs::host::EditorSeedRollback for Delayed {
            fn retire_closed(
                &self,
                proof: shell_contract::ipc::NativeClosureReceipt,
                done: Box<dyn FnOnce(Result<(), String>)>,
            ) {
                assert!(proof.matches(&self.reservation, ViewId(77), &self.generation));
                *self.retire.borrow_mut() = Some(done);
            }
            fn retire_adopted(
                &self,
                _: shell_contract::ipc::NativeClosureReceipt,
                done: Box<dyn FnOnce(Result<(), String>)>,
            ) {
                done(Err("unexpected adoption".into()))
            }
            fn retire_superseded(
                &self,
                _: shell_contract::ipc::NativeBrowserSuperseded,
                done: Box<dyn FnOnce(Result<(), String>)>,
            ) {
                done(Err("unexpected supersession".into()))
            }
            fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
                *self.rollback.borrow_mut() = Some(done);
            }
        }
        let authority = NativeInvocationIssuer::new(CancelToken::default());
        let root = super::super::recovery_launch::issuer::test_startup(&authority);
        let operation = authority
            .issue_recovery_child(
                &root,
                NativeInvocationSelection::RecoveryBlank {
                    surface: shell_contract::Surface::Docs,
                    title: "direct".into(),
                },
                CancelToken::default(),
            )
            .unwrap();
        let generation = shell_contract::runtime::RendererGeneration::new();
        let (issuer, reservation) =
            shell_contract::ipc::NativeClosureIssuer::new(ViewId(77), generation.clone());
        let actor = Rc::new(Delayed {
            reservation,
            generation,
            retire: RefCell::new(None),
            rollback: RefCell::new(None),
        });
        let store: shell_ipc_w::tabs::host::EditorSeedRollbackStore = Rc::default();
        store.borrow_mut().push(actor.clone());
        let id = register_direct(WindowId(77), operation, store).unwrap();
        let answered = Rc::new(Cell::new(false));
        let answer = answered.clone();
        fail_direct(
            id,
            main_core::shell::renderer_disposal::Failure("failure".into()),
            shell_ipc_w::tabs::host::ViewCreationCleanupState::Acknowledged(
                shell_contract::ipc::NativeBrowserRetirement::Closed(issuer.closed()),
            ),
            None,
            std::sync::Arc::new(Core),
            Box::new(move |result| {
                assert!(result.is_err());
                answer.set(true)
            }),
        );
        assert!(!answered.get());
        let done = actor.retire.borrow_mut().take().unwrap();
        done(Ok(()));
        assert!(!answered.get());
        let done = actor.rollback.borrow_mut().take().unwrap();
        done(Err("transient rollback failure".into()));
        assert!(!answered.get());
        let drained = Rc::new(Cell::new(false));
        let result = drained.clone();
        cancel_and_drain(
            WindowId(77),
            Box::new(move |outcome| {
                outcome.unwrap();
                result.set(true)
            }),
        );
        assert!(!answered.get());
        assert!(!drained.get());
        let done = actor.retire.borrow_mut().take().unwrap();
        done(Ok(()));
        let done = actor.rollback.borrow_mut().take().unwrap();
        done(Ok(()));
        assert!(answered.get());
        assert!(drained.get());
    }
}
