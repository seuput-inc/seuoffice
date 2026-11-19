use super::{
    Editor,
    recovery::{RecoveryCleanup, Store, file_identity, matches_native_copy_selection},
};
use crate::kit;
use main_core::{
    node_fs::NodeError,
    packages::electron_utils::{
        recovery_contract::{
            RecoveryOpenDecision, RecoveryOpenDone, RecoveryOpenFileRequest,
            RecoveryOpenUntitledRequest,
        },
        recovery_store::{
            RecoveryCopyState, RecoveryStore, RecoveryWriteTicket, recovery_copy_path,
        },
    },
    shell::renderer_disposal::Failure,
};
use shell_contract::{
    ViewId,
    hosts::OwnedEffectsHost,
    ipc::{
        NativeInvocationSelection, NativeRecoveryDecision, RecoveryOpenContext, RecoveryOpenTarget,
    },
};
use std::{io::Write, path::Path, rc::Rc};

type RetirementDone = Box<dyn FnOnce(Result<(), String>)>;

#[derive(Clone)]
pub struct PdfRecoveryRetirement(Rc<RetirementState>);

struct RetirementState {
    editor: Rc<Editor>,
    target: RecoveryOpenTarget,
    running: std::cell::Cell<bool>,
    finished: std::cell::Cell<bool>,
}

struct RetirementCompletion {
    state: Rc<RetirementState>,
    done: Option<RetirementDone>,
}

impl RetirementCompletion {
    fn finish(mut self, result: Result<(), String>) {
        self.state.running.set(false);
        if result.is_ok() {
            self.state
                .editor
                .rec
                .borrow_mut()
                .finish_closed_cleanup(&self.state.target);
            self.state.finished.set(true);
        }
        if let Some(done) = self.done.take() {
            done(result);
        }
    }
}

impl Drop for RetirementCompletion {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            self.state.running.set(false);
            done(Err("PDF recovery cleanup worker was discarded".into()));
        }
    }
}

impl PdfRecoveryRetirement {
    pub(super) fn new(editor: Rc<Editor>, target: RecoveryOpenTarget) -> Self {
        Self(Rc::new(RetirementState {
            editor,
            target,
            running: std::cell::Cell::new(false),
            finished: std::cell::Cell::new(false),
        }))
    }

    pub fn claim_close_in_transition(
        &self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        owner: &shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<(), String> {
        let target = &self.0.target;
        if !self.0.editor.services.core.is_current()
            || !transition.matches(target.closure_reservation())
            || !transition.matches_owner(owner)
            || !owner.same_owner(target.browser_owner())
        {
            return Err("PDF recovery close transition does not match its reservation".into());
        }
        let mut state = self.0.editor.st.borrow_mut();
        let Some(slot) = state.views.get_mut(&target.view()) else {
            return Ok(());
        };
        let matches_target = slot
            .recovery_preparing
            .as_ref()
            .is_some_and(|current| current.same_binding(target))
            || slot
                .recovery_seed
                .as_ref()
                .and_then(PendingSeed::unclosed_target)
                .is_some_and(|current| current.same_binding(target));
        if !matches_target
            || !slot
                .browser_owner
                .as_ref()
                .is_some_and(|current| current.same_owner(owner))
            || slot.recovery_closing
        {
            return Err("PDF recovery slot is not the captured browser owner".into());
        }
        slot.recovery_closing = true;
        Ok(())
    }

    pub fn retire_closed(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.retire(proof, false, done);
    }

    pub fn retire_adopted(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.retire(proof, true, done);
    }

    pub fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if !proof.matches(
            self.0.target.closure_reservation(),
            self.0.target.browser_owner(),
        ) {
            return done(Err(
                "PDF recovery supersession does not match its original browser owner".into(),
            ));
        }
        if self.0.finished.get() {
            return done(Ok(()));
        }
        if self.0.running.replace(true) {
            return done(Err("PDF recovery cleanup is already running".into()));
        }
        let finish = Rc::new(std::cell::RefCell::new(Some(RetirementCompletion {
            state: self.0.clone(),
            done: Some(done),
        })));
        let pending = finish.clone();
        let back = kit::back_on_core(&self.0.editor.services, move |()| {
            if let Some(completion) = pending.borrow_mut().take() {
                let old_slot_still_present = completion
                    .state
                    .editor
                    .st
                    .borrow()
                    .views
                    .get(&completion.state.target.view())
                    .is_some_and(|slot| {
                        slot.recovery_preparing
                            .as_ref()
                            .is_some_and(|target| target.same_binding(&completion.state.target))
                            || slot
                                .recovery_seed
                                .as_ref()
                                .and_then(PendingSeed::unclosed_target)
                                .is_some_and(|target| target.same_binding(&completion.state.target))
                    });
                if old_slot_still_present {
                    return completion.finish(Err("PDF recovery slot was not superseded".into()));
                }
                let plan = completion
                    .state
                    .editor
                    .rec
                    .borrow()
                    .superseded_cleanup(&completion.state.target, &proof);
                match plan {
                    Ok(work) => cleanup_work(completion, work, None),
                    Err(error) => completion.finish(Err(error)),
                }
            }
        });
        if let Err(error) = self.0.target.drain_work(
            self.0.editor.services.core.clone(),
            Box::new(move || back(())),
        ) && let Some(completion) = finish.borrow_mut().take()
        {
            completion.finish(Err(error));
        }
    }

    pub fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        if self.0.finished.get() {
            done(Ok(()));
        } else {
            done(Err(
                "PDF recovery rollback requires its exact native close receipt".into(),
            ));
        }
    }

    fn retire(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        adopted: bool,
        done: RetirementDone,
    ) {
        let target = &self.0.target;
        if !proof.matches(
            target.closure_reservation(),
            target.view(),
            target.created(),
        ) || (adopted && proof.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed)
        {
            return done(Err(
                "PDF recovery close receipt does not match its reservation".into(),
            ));
        }
        if self.0.finished.get() {
            return done(Ok(()));
        }
        if self.0.running.replace(true) {
            return done(Err("PDF recovery cleanup is already running".into()));
        }
        let finish = Rc::new(std::cell::RefCell::new(Some(RetirementCompletion {
            state: self.0.clone(),
            done: Some(done),
        })));
        let pending = finish.clone();
        let back = kit::back_on_core(&self.0.editor.services, move |()| {
            if let Some(completion) = pending.borrow_mut().take() {
                cleanup_closed(completion, proof, adopted);
            }
        });
        if let Err(error) = self.0.target.drain_work(
            self.0.editor.services.core.clone(),
            Box::new(move || back(())),
        ) && let Some(completion) = finish.borrow_mut().take()
        {
            completion.finish(Err(error));
        }
    }
}

fn cleanup_closed(
    completion: RetirementCompletion,
    proof: shell_contract::ipc::NativeClosureReceipt,
    adopted: bool,
) {
    let state = completion.state.clone();
    let ed = state.editor.clone();
    let view = state.target.view();
    let generation_matches = ed
        .services
        .views
        .renderer_generation(view)
        .is_none_or(|generation| generation.same_generation(state.target.created()));
    let matches = generation_matches
        && ed.st.borrow().views.get(&view).is_some_and(|slot| {
            slot.recovery_preparing
                .as_ref()
                .is_some_and(|target| target.same_binding(&state.target))
                || slot.recovery_seed.as_ref().is_some_and(|seed| {
                    seed.unclosed_target()
                        .is_some_and(|target| target.same_binding(&state.target))
                        && slot
                            .document_lease
                            .as_ref()
                            .is_some_and(|document| document.same_document(&seed.document))
                        && (seed.completed()
                            || slot
                                .document_revision
                                .as_ref()
                                .is_some_and(|revision| revision.same_document(&seed.revision)))
                })
        });
    if matches {
        ed.destroyed_after_recovery_closed(view);
    }
    let (work, deletion) = match ed
        .rec
        .borrow()
        .closed_cleanup(&state.target, &proof, adopted)
    {
        Ok(plan) => plan,
        Err(error) => return completion.finish(Err(error)),
    };
    cleanup_work(completion, work, deletion);
}

fn cleanup_work(
    completion: RetirementCompletion,
    work: Vec<String>,
    deletion: Option<(
        main_core::packages::electron_utils::recovery_store::RecoveryIdentity,
        main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason,
    )>,
) {
    let ed = completion.state.editor.clone();
    let store = ed.rec.borrow().store.clone();
    kit::on_worker(
        &ed.services,
        move || -> Result<(), String> {
            for path in work {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(main_core::node_fs::node_error(&error, "unlink", &path).message);
                    }
                }
            }
            if let Some((identity, reason)) = deletion {
                store
                    .with(|store| store.delete_copy(&identity, reason))
                    .map_err(|error| error.message)?;
            }
            Ok(())
        },
        move |result| completion.finish(result),
    );
}

pub(super) struct PendingSeed {
    receipt: Option<shell_contract::ipc::RecoverySeedReceipt>,
    document: shell_contract::ipc::PdfDocumentLease,
    revision: shell_contract::ipc::PdfDocumentLease,
    path: String,
    token: String,
    consumed: bool,
    created_loss: Option<shell_contract::runtime::CancelSubscription>,
    revision_loss: Option<shell_contract::runtime::CancelSubscription>,
    receiver_loss: Option<shell_contract::runtime::CancelSubscription>,
}

impl PendingSeed {
    pub(super) fn take_revision_loss(
        &mut self,
    ) -> Option<shell_contract::runtime::CancelSubscription> {
        self.revision_loss.take()
    }
    pub(super) fn completed(&self) -> bool {
        self.receipt
            .as_ref()
            .and_then(|receipt| receipt.settlement())
            .is_some_and(|result| result.is_ok())
    }
    pub(super) fn unclosed_target(&self) -> Option<RecoveryOpenTarget> {
        self.receipt
            .as_ref()
            .map(|receipt| receipt.target().clone())
    }
    pub(super) fn pending_target(&self) -> Option<RecoveryOpenTarget> {
        (!self.completed())
            .then(|| self.unclosed_target())
            .flatten()
    }
}

fn observe_loss(
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    loss: &shell_contract::runtime::CancelToken,
) -> shell_contract::runtime::CancelSubscription {
    let receipt = receipt.clone();
    loss.on_cancel(move || {
        if let Ok(notification) = receipt.fail_after_loss("PDF recovery document was lost".into()) {
            notification.notify();
        }
    })
}

pub(super) struct RevisionHandoff {
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    old: shell_contract::ipc::PdfDocumentLease,
    successor: shell_contract::ipc::PdfDocumentLease,
    successor_loss: Option<shell_contract::runtime::CancelSubscription>,
    old_loss: Option<shell_contract::runtime::CancelSubscription>,
    old_path: String,
    path: Option<String>,
}

impl RevisionHandoff {
    pub(super) fn prepare(
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        old: &shell_contract::ipc::PdfDocumentLease,
        old_path: String,
        path: String,
    ) -> Self {
        let successor = shell_contract::ipc::PdfDocumentLease::new();
        let successor_loss = Some(observe_loss(receipt, &successor.loss_token()));
        Self {
            receipt: receipt.clone(),
            old: old.clone(),
            successor,
            successor_loss,
            old_loss: None,
            old_path,
            path: Some(path),
        }
    }

    pub(super) fn successor(&self) -> &shell_contract::ipc::PdfDocumentLease {
        &self.successor
    }

    pub(super) fn transition<T>(
        &mut self,
        ed: &Editor,
        view: ViewId,
        commit: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = ed.st.borrow_mut();
        let slot = state
            .views
            .get_mut(&view)
            .ok_or("PDF recovery document is unavailable")?;
        let seed = slot
            .recovery_seed
            .as_mut()
            .ok_or("PDF recovery seed is unavailable")?;
        if !slot
            .document_revision
            .as_ref()
            .is_some_and(|revision| revision.same_document(&self.old))
            || !seed.revision.same_document(&self.old)
            || slot.open_path.as_deref() != Some(self.old_path.as_str())
            || seed.path != self.old_path
            || !seed
                .receipt
                .as_ref()
                .is_some_and(|receipt| receipt.matches(self.receipt.target()))
            || seed.revision_loss.is_none()
            || self.successor_loss.is_none()
            || self.path.is_none()
        {
            return Err("PDF recovery revision changed".into());
        }
        let path = self.path.take().ok_or("PDF recovery path is unavailable")?;
        let loss = self
            .successor_loss
            .take()
            .ok_or("PDF recovery loss is unavailable")?;
        let successor = match commit() {
            Ok(successor) => successor,
            Err(error) => {
                self.path = Some(path);
                self.successor_loss = Some(loss);
                return Err(error);
            }
        };
        slot.document_revision = Some(self.successor.clone());
        seed.revision = self.successor.clone();
        seed.path = path;
        self.old_loss = seed.revision_loss.replace(loss);
        Ok(successor)
    }

    pub(super) fn before_notify(&mut self) {
        drop(self.old_loss.take());
    }
}

fn commit(
    ed: &Editor,
    view: ViewId,
    target: &RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    effects: &OwnedEffectsHost,
    path: &str,
) -> Result<(), String> {
    let (document, revision) = {
        let state = ed.st.borrow();
        let slot = state
            .views
            .get(&view)
            .ok_or("PDF recovery document is unavailable")?;
        let document = slot
            .document_lease
            .clone()
            .filter(|document| document.is_live())
            .ok_or("PDF recovery document is unavailable")?;
        let revision = slot
            .document_revision
            .clone()
            .filter(|revision| revision.is_live())
            .ok_or("PDF recovery document is unavailable")?;
        (document, revision)
    };
    let token = jscompat::random_uuid();
    let receipt = effects.commit_recovery_seed_snapshot(target, snapshot, &mut || {
        target
            .closure_reservation()
            .with_owner(target.browser_owner(), |_| {
                let mut state = ed.st.borrow_mut();
                let slot = state
                    .views
                    .get_mut(&view)
                    .ok_or("PDF recovery document is unavailable")?;
                if slot.open_path.as_deref() != Some(path)
                    || !slot.document_lease.as_ref().is_some_and(|current| {
                        current.same_document(&document) && current.is_live()
                    })
                    || !slot.document_revision.as_ref().is_some_and(|current| {
                        current.same_document(&revision) && current.is_live()
                    })
                {
                    return Err("PDF recovery document was replaced".into());
                }
                if slot.recovery_seed.is_some() {
                    return Err("PDF recovery seed already exists".into());
                }
                if !slot
                    .recovery_preparing
                    .as_ref()
                    .is_some_and(|preparing| preparing.same_binding(target))
                {
                    return Err("PDF recovery preparation was replaced".into());
                }
                slot.recovery_preparing = None;
                slot.recovery_seed = Some(PendingSeed {
                    receipt: None,
                    document: document.clone(),
                    revision: revision.clone(),
                    path: path.to_owned(),
                    token: token.clone(),
                    consumed: false,
                    created_loss: None,
                    revision_loss: None,
                    receiver_loss: None,
                });
                Ok(())
            })
    })?;
    let created_loss = observe_loss(&receipt, &target.created().loss_token());
    let revision_loss = observe_loss(&receipt, &revision.loss_token());
    let mut state = ed.st.borrow_mut();
    let pending = state
        .views
        .get_mut(&view)
        .and_then(|slot| slot.recovery_seed.as_mut())
        .filter(|pending| pending.token == token)
        .ok_or("PDF recovery seed was replaced")?;
    pending.receipt = Some(receipt);
    pending.created_loss = Some(created_loss);
    pending.revision_loss = Some(revision_loss);
    Ok(())
}

fn owner(
    ed: &Editor,
    cx: &shell_contract::ipc::CallContext,
    revision: &shell_contract::ipc::PdfDocumentLease,
) -> Result<shell_contract::ipc::NativeRequestOwner, String> {
    let context = cx
        .native
        .clone()
        .ok_or("PDF recovery renderer is not authenticated")?;
    let native = shell_contract::ipc::NativeRequestOwner {
        view: cx.sender.view,
        context,
    };
    let current = ed
        .bind_sign_owner(cx.sender.view, native.context.clone())
        .ok_or("PDF recovery document is unavailable")?;
    if !current
        .pdf_revision()
        .is_some_and(|candidate| candidate.same_document(revision))
        || native
            .pdf_revision()
            .is_some_and(|candidate| !candidate.same_document(revision))
        || native.pdf_document().is_some_and(|candidate| {
            !current
                .pdf_document()
                .is_some_and(|document| candidate.same_document(document))
        })
    {
        return Err("PDF recovery document was replaced".into());
    }
    Ok(current)
}

#[derive(Clone)]
pub(super) struct ReadScope {
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    owner: shell_contract::ipc::NativeRequestOwner,
    effects: OwnedEffectsHost,
    cancel: shell_contract::runtime::CancelToken,
    _work: shell_contract::ipc::RecoveryWorkTicket,
}

impl ReadScope {
    pub(super) fn admit(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.effects
            .with_recovery_open_effect(&self.receipt, &self.owner, &self.cancel, effect)
    }
}

pub(super) fn read_scope(
    ed: &Editor,
    cx: &shell_contract::ipc::CallContext,
    path: &str,
) -> Result<Option<ReadScope>, String> {
    let (receipt, revision) = {
        let state = ed.st.borrow();
        if state
            .views
            .get(&cx.sender.view)
            .is_some_and(|slot| slot.recovery_preparing.is_some())
        {
            return Err("PDF recovery input is still preparing".into());
        }
        let Some(seed) = state
            .views
            .get(&cx.sender.view)
            .and_then(|slot| slot.recovery_seed.as_ref())
        else {
            return Ok(None);
        };
        if seed.completed() {
            return Ok(None);
        }
        if !seed.consumed || seed.path != path {
            return Err("PDF recovery input does not match its captured seed".into());
        }
        (
            seed.receipt
                .clone()
                .ok_or("PDF recovery seed has not committed")?,
            seed.revision.clone(),
        )
    };
    let owner = owner(ed, cx, &revision)?;
    let effects = ed
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or("PDF recovery admission unavailable")?;
    effects.with_recovery_open_effect(&receipt, &owner, &cx.cancel, &mut || Ok(()))?;
    let work = receipt.begin_work()?;
    Ok(Some(ReadScope {
        receipt,
        owner,
        effects,
        cancel: cx.cancel.clone(),
        _work: work,
    }))
}

pub(super) fn consume(
    ed: &Editor,
    cx: &shell_contract::ipc::CallContext,
) -> Result<shell_contract::IpcValue, String> {
    use crate::kit::IntoIpc;
    let (receipt, revision) = {
        let state = ed.st.borrow();
        if state
            .views
            .get(&cx.sender.view)
            .is_some_and(|slot| slot.recovery_preparing.is_some())
        {
            return Err("PDF recovery input is still preparing".into());
        }
        let Some(seed) = state
            .views
            .get(&cx.sender.view)
            .and_then(|slot| slot.recovery_seed.as_ref())
        else {
            return Ok(shell_contract::IpcValue::Null);
        };
        if seed.completed() {
            return Ok(shell_contract::IpcValue::Null);
        }
        (
            seed.receipt
                .clone()
                .ok_or("PDF recovery seed has not committed")?,
            seed.revision.clone(),
        )
    };
    let owner = owner(ed, cx, &revision)?;
    let snapshot = receipt.target().authority_snapshot()?;
    let loss = snapshot
        .registered
        .as_ref()
        .and_then(|registered| registered.loss_token())
        .ok_or("PDF recovery renderer lifetime is unavailable")?;
    let effects = ed
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .ok_or("PDF recovery admission unavailable")?;
    let mut answer = None;
    effects.with_recovery_open_snapshot(
        &receipt,
        &snapshot,
        &owner,
        &[],
        &cx.cancel,
        &cx.cancel,
        &mut || {
            let mut state = ed.st.borrow_mut();
            let seed = state
                .views
                .get_mut(&cx.sender.view)
                .and_then(|slot| slot.recovery_seed.as_mut())
                .ok_or("PDF recovery seed is unavailable")?;
            if !seed
                .receipt
                .as_ref()
                .is_some_and(|current| current.matches(receipt.target()))
                || seed.consumed
            {
                return Err("PDF recovery seed was already consumed".into());
            }
            seed.consumed = true;
            ed.rec.borrow_mut().mark_consumed(cx.sender.view);
            answer = Some(kit::obj([
                ("path", seed.path.clone().into_ipc()),
                ("token", seed.token.clone().into_ipc()),
            ]));
            Ok(())
        },
    )?;
    let lost_receipt = receipt.clone();
    let subscription = loss.on_cancel(move || {
        if let Ok(notification) =
            lost_receipt.fail_after_loss("PDF recovery renderer was revoked".into())
        {
            notification.notify();
        }
    });
    let mut state = ed.st.borrow_mut();
    let seed = state
        .views
        .get_mut(&cx.sender.view)
        .and_then(|slot| slot.recovery_seed.as_mut())
        .ok_or("PDF recovery seed was released")?;
    if !seed
        .receipt
        .as_ref()
        .is_some_and(|current| current.matches(receipt.target()))
    {
        return Err("PDF recovery seed was replaced".into());
    }
    seed.receiver_loss = Some(subscription);
    answer.ok_or_else(|| "PDF recovery seed was not admitted".into())
}

pub(super) fn settle(
    ed: &Editor,
    cx: &shell_contract::ipc::CallContext,
    token: &str,
    result: Result<(), String>,
) -> Result<(), String> {
    let (receipt, revision) = {
        let state = ed.st.borrow();
        let seed = state
            .views
            .get(&cx.sender.view)
            .and_then(|slot| slot.recovery_seed.as_ref())
            .filter(|seed| seed.consumed && seed.token == token)
            .ok_or("PDF recovery receipt is unavailable")?;
        (
            seed.receipt
                .clone()
                .ok_or("PDF recovery seed has not committed")?,
            seed.revision.clone(),
        )
    };
    let owner = owner(ed, cx, &revision)?;
    let effects = ed
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .ok_or("PDF recovery admission unavailable")?;
    effects.settle_recovery_seed(&receipt, &owner, &cx.cancel, result)?;
    Ok(())
}

fn seed(
    effects: &OwnedEffectsHost,
    target: &RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let mut used = false;
    effects.with_recovery_seed_snapshot(target, snapshot, &mut || {
        if used {
            return Err("PDF recovery admission was invoked twice".into());
        }
        used = true;
        effect()
    })?;
    if used {
        Ok(())
    } else {
        Err("PDF recovery admission did not run".into())
    }
}

fn file_admission(
    effects: &OwnedEffectsHost,
    target: &RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    store: &Store,
    ticket: &RecoveryWriteTicket,
    effect: &mut dyn FnMut(),
) -> Result<(), NodeError> {
    seed(effects, target, snapshot, &mut || {
        store.try_with(|store| {
            if !store.write_ticket_is_current(ticket) {
                return Err("PDF recovery copy was superseded".to_owned());
            }
            effect();
            Ok(())
        })?
    })
    .map_err(|message| NodeError {
        code: Some("ECANCELED"),
        message,
    })
}

fn begin(
    ed: &Editor,
    view: ViewId,
    target: &RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    effects: &OwnedEffectsHost,
) -> Result<(), String> {
    seed(effects, target, snapshot, &mut || {
        // the host committed the browser's owner when it made the view: the slot is made
        // under that owner, not by a second first adoption
        target
            .closure_reservation()
            .with_owner(target.browser_owner(), |_| {
                if ed.rec.borrow().has_view_state(view) {
                    return Err("PDF recovery target already has document state".into());
                }
                let mut state = ed.st.borrow_mut();
                if state.views.contains_key(&view) {
                    return Err("PDF recovery target is already registered".into());
                }
                let slot = state.views.entry(view).or_default();
                slot.document_lease = Some(shell_contract::ipc::PdfDocumentLease::new());
                slot.document_revision = Some(shell_contract::ipc::PdfDocumentLease::new());
                slot.browser_owner = Some(target.browser_owner().clone());
                slot.recovery_preparing = Some(target.clone());
                Ok(())
            })
    })
}

pub(super) fn open_file(
    ed: &Rc<Editor>,
    view: ViewId,
    request: RecoveryOpenFileRequest,
    target: RecoveryOpenTarget,
    done: RecoveryOpenDone,
) {
    if target.view() != view {
        return done(Err(Failure("PDF recovery target mismatch".into())));
    }
    let snapshot = match target.authority_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => return done(Err(Failure(error))),
    };
    if let RecoveryOpenContext::NativeStartup(operation) = &snapshot.context {
        let matches = matches!(operation.selection(), NativeInvocationSelection::RecoveryFile { surface: shell_contract::Surface::Pdf, path, decision }
            if path == Path::new(&request.file_path) && (*decision == NativeRecoveryDecision::Restore) == (request.decision == RecoveryOpenDecision::Restore));
        if !matches {
            return done(Err(Failure("PDF recovery selection mismatch".into())));
        }
    }
    let Some(effects) = ed.services.hosts.get::<OwnedEffectsHost>().cloned() else {
        return done(Err(Failure("PDF recovery admission unavailable".into())));
    };
    if let Err(error) = begin(ed, view, &target, &snapshot, &effects) {
        return done(Err(Failure(error)));
    }
    let preparation = match target.begin_preparation_work() {
        Ok(ticket) => ticket,
        Err(error) => return done(Err(Failure(error))),
    };
    let worker_preparation = preparation.clone();
    let path = request.file_path.clone();
    let check_path = path.clone();
    let check_effects = effects.clone();
    let check_target = target.clone();
    let check_snapshot = snapshot.clone();
    let store = ed.rec.borrow().store.clone();
    let decision = request.decision;
    let editor = ed.clone();
    kit::on_worker(
        &ed.services,
        move || -> Result<Option<Vec<u8>>, String> {
            let _preparation = worker_preparation;
            seed(&check_effects, &check_target, &check_snapshot, &mut || {
                Ok(())
            })?;
            if !Path::new(&check_path).exists() {
                return Err("The recovery file is unavailable".into());
            }
            let identity = file_identity(&check_path);
            seed(&check_effects, &check_target, &check_snapshot, &mut || {
                store
                    .try_with(|store| store.clear_discarded(&identity))?
                    .map_err(|error| error.message)
            })?;
            if decision != RecoveryOpenDecision::Restore {
                return Ok(None);
            }
            let mut ticket = None;
            seed(&check_effects, &check_target, &check_snapshot, &mut || {
                ticket = Some(
                    store
                        .try_with(|store| store.begin_write(&identity))?
                        .map_err(|error| error.message)?,
                );
                Ok(())
            })?;
            let ticket = ticket.ok_or("PDF recovery ticket was not captured")?;
            let source = recovery_copy_path(&store.user_data.to_string_lossy(), &identity)
                .map_err(|error| error.message)?;
            let bytes = match std::fs::read(&source) {
                Ok(bytes) => bytes,
                Err(_) => return Ok(None),
            };
            if let Some(plan) = RecoveryStore::prepare_copy_state_from_path(
                &identity,
                &source,
                RecoveryCopyState::Undecided,
            ) {
                plan.write_authorized(&mut |effect| {
                    file_admission(
                        &check_effects,
                        &check_target,
                        &check_snapshot,
                        &store,
                        &ticket,
                        effect,
                    )
                })
                .map_err(|error| error.message)?;
            }
            seed(&check_effects, &check_target, &check_snapshot, &mut || {
                store.try_with(|store| {
                    if store.write_ticket_is_current(&ticket) {
                        Ok(())
                    } else {
                        Err("PDF recovery copy was superseded".to_owned())
                    }
                })?
            })?;
            Ok(Some(bytes))
        },
        move |restored| {
            let _preparation = preparation;
            let mut restored = match restored {
                Ok(restored) => restored,
                Err(error) => return done(Err(Failure(error))),
            };
            let result = seed(&effects, &target, &snapshot, &mut || {
                target
                    .closure_reservation()
                    .with_owner(target.browser_owner(), |_| {
                        editor.replace_recovery_document(view, Some(path.clone()), &target)?;
                        editor
                            .rec
                            .borrow_mut()
                            .accept_prepared_file(view, &path, restored.take());
                        let granted = editor.grants.0.with(view, |set| match set {
                            Some(set) => {
                                set.insert(path.clone());
                                true
                            }
                            None => false,
                        });
                        if !granted {
                            editor.grant_only(view, &path);
                        }
                        Ok(())
                    })
            })
            .and_then(|()| commit(&editor, view, &target, &snapshot, &effects, &path));
            done(result.map_err(Failure));
        },
    );
}

pub(super) fn open_copy(
    ed: &Rc<Editor>,
    view: ViewId,
    request: RecoveryOpenUntitledRequest,
    target: RecoveryOpenTarget,
    done: RecoveryOpenDone,
) {
    if target.view() != view || request.copy.kind() != "pdf" {
        return done(Err(Failure("Invalid PDF recovery target".into())));
    }
    let snapshot = match target.authority_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => return done(Err(Failure(error))),
    };
    if let RecoveryOpenContext::NativeStartup(operation) = &snapshot.context
        && !matches_native_copy_selection(operation.selection(), &request)
    {
        return done(Err(Failure("PDF recovery selection mismatch".into())));
    }
    let Some(effects) = ed.services.hosts.get::<OwnedEffectsHost>().cloned() else {
        return done(Err(Failure("PDF recovery admission unavailable".into())));
    };
    if let Err(error) = begin(ed, view, &target, &snapshot, &effects) {
        return done(Err(Failure(error)));
    }
    let preparation = match target.begin_preparation_work() {
        Ok(ticket) => ticket,
        Err(error) => return done(Err(Failure(error))),
    };
    let worker_preparation = preparation.clone();
    let store = ed.rec.borrow().store.clone();
    let work = ed.rec.borrow().next_work_path();
    let source = match recovery_copy_path(&store.user_data.to_string_lossy(), &request.copy) {
        Ok(source) => source,
        Err(error) => return done(Err(Failure(error.message))),
    };
    let mut staged = None;
    if let Err(error) = seed(&effects, &target, &snapshot, &mut || {
        staged = Some(ed.rec.borrow_mut().stage_work(&target, work.clone()));
        Ok(())
    }) {
        return done(Err(Failure(error)));
    }
    let Some(staged) = staged else {
        return done(Err(Failure("PDF recovery work was not staged".into())));
    };
    let worker_target = target.clone();
    let worker_effects = effects.clone();
    let worker_snapshot = snapshot.clone();
    let worker_store = store.clone();
    let identity = request.copy.clone();
    let worker_work = work.clone();
    let editor = ed.clone();
    kit::on_worker(
        &ed.services,
        move || -> Result<(RecoveryWriteTicket, String), String> {
            let _preparation = worker_preparation;
            seed(
                &worker_effects,
                &worker_target,
                &worker_snapshot,
                &mut || Ok(()),
            )?;
            let bytes = std::fs::read(&source).map_err(|error| error.to_string())?;
            let parent = Path::new(&worker_work)
                .parent()
                .ok_or("PDF recovery work directory is unavailable")?;
            let missing = parent
                .ancestors()
                .take_while(|path| !path.is_dir())
                .map(Path::to_path_buf)
                .collect::<Vec<_>>();
            for directory in missing.into_iter().rev() {
                let mut result = None;
                seed(
                    &worker_effects,
                    &worker_target,
                    &worker_snapshot,
                    &mut || {
                        result = Some(std::fs::create_dir(&directory));
                        Ok(())
                    },
                )?;
                match result {
                    Some(Ok(())) => {}
                    Some(Err(error))
                        if error.kind() == std::io::ErrorKind::AlreadyExists
                            && directory.is_dir() => {}
                    Some(Err(error)) => return Err(error.to_string()),
                    None => return Err("PDF recovery directory was not admitted".into()),
                }
            }
            let mut created = None;
            seed(
                &worker_effects,
                &worker_target,
                &worker_snapshot,
                &mut || {
                    created = Some(
                        std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&worker_work),
                    );
                    if created.as_ref().is_some_and(|result| result.is_ok()) {
                        staged.store(true, std::sync::atomic::Ordering::Release);
                    }
                    Ok(())
                },
            )?;
            let mut file = created
                .ok_or("PDF recovery work file was not admitted")?
                .map_err(|error| error.to_string())?;
            file.write_all(&bytes).map_err(|error| error.to_string())?;
            drop(file);
            (|| {
                let mut ticket = None;
                seed(
                    &worker_effects,
                    &worker_target,
                    &worker_snapshot,
                    &mut || {
                        ticket = Some(
                            worker_store
                                .try_with(|store| {
                                    store.clear_discarded(&identity)?;
                                    store.begin_write(&identity)
                                })?
                                .map_err(|error| error.message)?,
                        );
                        Ok(())
                    },
                )?;
                let ticket = ticket.ok_or("PDF recovery ticket was not captured")?;
                seed(
                    &worker_effects,
                    &worker_target,
                    &worker_snapshot,
                    &mut || {
                        worker_store.try_with(|store| {
                            if store.write_ticket_is_current(&ticket) {
                                Ok(())
                            } else {
                                Err("PDF recovery copy was superseded".to_owned())
                            }
                        })?
                    },
                )?;
                if let Some(plan) = RecoveryStore::prepare_copy_state_from_path(
                    &identity,
                    &source,
                    RecoveryCopyState::Undecided,
                ) {
                    plan.write_authorized(&mut |effect| {
                        file_admission(
                            &worker_effects,
                            &worker_target,
                            &worker_snapshot,
                            &worker_store,
                            &ticket,
                            effect,
                        )
                    })
                    .map_err(|error| error.message)?;
                }
                Ok((ticket, worker_work.clone()))
            })()
        },
        move |result| {
            let core_preparation = preparation;
            let (ticket, path) = match result {
                Ok(result) => result,
                Err(error) => return done(Err(Failure(error))),
            };
            let result = seed(&effects, &target, &snapshot, &mut || {
                target
                    .closure_reservation()
                    .with_owner(target.browser_owner(), |_| {
                        store.try_with(|store| {
                            if store.write_ticket_is_current(&ticket) {
                                Ok(())
                            } else {
                                Err("PDF recovery copy was superseded".to_owned())
                            }
                        })??;
                        editor.replace_recovery_document(view, Some(path.clone()), &target)?;
                        {
                            let mut recovery = editor.rec.borrow_mut();
                            let cleanup = recovery.acknowledge_untitled_owned(
                                view,
                                &request.copy,
                                &request.title,
                                request.save_as_default_path.as_deref(),
                                &path,
                            );
                            recovery.adopt_work(&target, &path);
                            for effect in cleanup {
                                match effect {
                                    RecoveryCleanup::WorkFile(old) => {
                                        recovery.stage_existing_work(&target, old)
                                    }
                                    RecoveryCleanup::Copy(_, _) => {
                                        return Err("Unexpected PDF recovery cleanup".into());
                                    }
                                }
                            }
                        }
                        {
                            let mut state = editor.st.borrow_mut();
                            state.untitled_copy.insert(path.clone());
                            state.unsaved_untitled_copy.insert(path.clone());
                        }
                        let granted = editor.grants.0.with(view, |set| match set {
                            Some(set) => {
                                set.insert(path.clone());
                                true
                            }
                            None => false,
                        });
                        if !granted {
                            editor.grant_only(view, &path);
                        }
                        Ok(())
                    })
            });
            if let Err(error) = result {
                return done(Err(Failure(error)));
            }
            let _preparation = core_preparation;
            let result = commit(&editor, view, &target, &snapshot, &effects, &path);
            done(result.map_err(Failure));
        },
    );
}

#[cfg(test)]
mod tests;
