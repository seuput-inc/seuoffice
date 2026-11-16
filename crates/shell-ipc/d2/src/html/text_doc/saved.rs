//! A committed output opened as this editor's first document. The retained
//! bytes, never a reopened path, are what the renderer loads; the barrier
//! holds the renderer's first request until the exact document is registered.

use super::*;
use shell_contract::editors::{
    PreparedSavedDocument, SavedActorRegistration, SavedDocumentLoadOutcome,
};
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{
    CallContext, NativeBrowserSuperseded, NativeBrowserTransition, NativeClosureReceipt,
    NativeRequestOwner, NativeSavedDocument, NativeSavedTarget, Responder,
};
use shell_contract::runtime::CancelSubscription;
use std::cell::Cell;

const RELEASED: &str = "The saved document operation was released";

pub(super) struct SavedOpen {
    target: NativeSavedTarget,
    lease: RendererGeneration,
    document: Option<(NativeSavedDocument, NativeRequestOwner)>,
    done: Option<Then<SavedDocumentLoadOutcome>>,
    waiter: Option<(CallContext, Responder)>,
    stage: Option<super::super::SavedAssetStage>,
    disk: Option<SaveDiskState>,
    token: String,
    applying: bool,
    waiting: bool,
    failure: Option<String>,
    losses: Vec<CancelSubscription>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prepared,
    Committed,
    Closed,
    Superseded,
    RolledBack,
}

struct Registration {
    docs: Weak<TextDocs>,
    target: NativeSavedTarget,
    phase: Cell<Phase>,
}

struct Prepared {
    docs: Rc<TextDocs>,
    registration: Rc<Registration>,
    slot: Option<SavedOpen>,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if self.slot.is_some() {
            self.docs
                .saved_reservations
                .set(self.docs.saved_reservations.get() - 1);
        }
    }
}

impl PreparedSavedDocument for Prepared {
    fn with_validated(
        &mut self,
        transition: &NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), IpcError>,
    ) -> Result<(), IpcError> {
        let registration = self.registration.clone();
        let target = &registration.target;
        if registration.phase.get() != Phase::Prepared {
            return Err(kit::error(
                "The saved document actor was already committed or released",
            ));
        }
        if !transition.matches(target.reservation())
            || !(transition.matches_owner(target.browser())
                || transition.proposes_owner(target.browser()))
        {
            return Err(kit::error("The saved document belongs to another browser"));
        }
        let slot = self
            .slot
            .take()
            .ok_or_else(|| kit::error("The saved document barrier was consumed"))?;
        let tracked = self
            .docs
            .read_view(target.view(), |state| {
                state.tracked && state.lease.same_generation(&slot.lease)
            })
            .unwrap_or(false);
        let mut saved = self.docs.saved.borrow_mut();
        if !tracked
            || saved.contains_key(&target.view())
            || self.docs.saved_reservations.get() == 0
        {
            self.slot = Some(slot);
            return Err(kit::error("The saved document target is unavailable"));
        }
        if let Err(error) = next() {
            self.slot = Some(slot);
            return Err(error);
        }
        self.docs
            .saved_reservations
            .set(self.docs.saved_reservations.get() - 1);
        saved.insert(target.view(), slot);
        registration.phase.set(Phase::Committed);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Rc<dyn SavedActorRegistration> {
        self.registration.clone()
    }
}

impl SavedActorRegistration for Registration {
    fn retire_closed(&self, proof: NativeClosureReceipt, then: Then<()>) {
        if !matches!(self.phase.get(), Phase::Committed | Phase::Closed)
            || !proof.matches(
                self.target.reservation(),
                self.target.view(),
                self.target.created(),
            )
        {
            return then(Err(kit::error(
                "The saved document close proof does not match",
            )));
        }
        if let Some(docs) = self.docs.upgrade() {
            release(&docs, &self.target, "The document view was closed");
        }
        self.phase.set(Phase::Closed);
        then(Ok(()));
    }

    fn retire_superseded(&self, proof: NativeBrowserSuperseded, then: Then<()>) {
        if !matches!(self.phase.get(), Phase::Committed | Phase::Superseded)
            || !proof.matches(self.target.reservation(), self.target.browser())
        {
            return then(Err(kit::error(
                "The saved document supersession proof does not match",
            )));
        }
        if let Some(docs) = self.docs.upgrade() {
            release(&docs, &self.target, "The saved document browser was replaced");
        }
        self.phase.set(Phase::Superseded);
        then(Ok(()));
    }

    fn rollback(&self, then: Then<()>) {
        match self.phase.get() {
            Phase::Prepared | Phase::RolledBack => {
                self.phase.set(Phase::RolledBack);
                then(Ok(()));
            }
            Phase::Closed | Phase::Superseded => then(Ok(())),
            Phase::Committed => then(Err(kit::error(
                "The saved document actor lacks its exact close proof",
            ))),
        }
    }
}

fn release(docs: &TextDocs, target: &NativeSavedTarget, message: &str) {
    fail(docs, target, message);
    let removed = {
        let mut saved = docs.saved.borrow_mut();
        if saved
            .get(&target.view())
            .is_some_and(|slot| slot.target.same_target(target))
        {
            saved.remove(&target.view())
        } else {
            None
        }
    };
    drop(removed);
}

/// The barrier of a destroyed view fails its waiters and is dropped.
pub(super) fn release_view(docs: &TextDocs, view: ViewId) {
    let target = docs
        .saved
        .borrow()
        .get(&view)
        .map(|slot| slot.target.clone());
    if let Some(target) = target {
        release(docs, &target, "The document view was destroyed");
    }
}

fn fail(docs: &TextDocs, target: &NativeSavedTarget, message: &str) {
    let (waiter, done, stage) = {
        let mut saved = docs.saved.borrow_mut();
        let Some(slot) = saved
            .get_mut(&target.view())
            .filter(|slot| slot.target.same_target(target))
        else {
            return;
        };
        if slot.failure.is_some() {
            return;
        }
        slot.failure = Some(message.to_owned());
        (slot.waiter.take(), slot.done.take(), slot.stage.take())
    };
    if let Some(stage) = stage {
        let _ = (stage.revoke)();
    }
    if let Some((_, responder)) = waiter {
        responder.reply(Err(kit::error(message)));
    }
    if let Some(done) = done {
        done(Err(kit::error(message)));
    }
}

fn effects(docs: &TextDocs) -> Result<OwnedEffectsHost, String> {
    docs.services
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or_else(|| "Native saved document effects are unavailable".to_owned())
}

fn path_of(target: &NativeSavedTarget) -> Result<&str, String> {
    target
        .path()
        .path()
        .to_str()
        .ok_or_else(|| "The saved document path is not valid Unicode".to_owned())
}

impl TextDocs {
    pub(crate) fn prepare_saved(
        self: &Rc<Self>,
        surface: shell_contract::Surface,
        target: NativeSavedTarget,
    ) -> Result<Box<dyn PreparedSavedDocument>, IpcError> {
        if target.surface() != surface {
            return Err(kit::error("The saved target belongs to another editor"));
        }
        path_of(&target).map_err(kit::error)?;
        let view = target.view();
        let lease = self
            .read_view(view, |state| state.tracked.then(|| state.lease.clone()))
            .flatten()
            .ok_or_else(|| kit::error("The saved document view is not open"))?;
        let mut controls = vec![
            target.cancel().clone(),
            target.pending().clone(),
            target.created().loss_token(),
            lease.loss_token(),
        ];
        controls.extend_from_slice(target.additional());
        for owner in target.participants() {
            controls.push(
                owner
                    .loss_token()
                    .ok_or_else(|| kit::error("A saved document owner is unavailable"))?,
            );
        }
        let mut losses = Vec::with_capacity(controls.len() + 2);
        for control in controls {
            let weak = Rc::downgrade(self);
            let captured = target.clone();
            let notify = kit::back_on_core(&self.services, move |()| {
                if let Some(docs) = weak.upgrade() {
                    fail(&docs, &captured, RELEASED);
                }
            });
            losses.push(control.on_cancel(move || notify(())));
        }
        let count = self
            .saved_reservations
            .get()
            .checked_add(1)
            .ok_or_else(|| kit::error("The saved document barrier capacity is exhausted"))?;
        self.saved
            .try_borrow_mut()
            .map_err(|_| kit::error("The saved document barrier is busy"))?
            .try_reserve(count)
            .map_err(|_| kit::error("The saved document barrier could not be allocated"))?;
        self.saved_reservations.set(count);
        Ok(Box::new(Prepared {
            docs: Rc::clone(self),
            registration: Rc::new(Registration {
                docs: Rc::downgrade(self),
                target: target.clone(),
                phase: Cell::new(Phase::Prepared),
            }),
            slot: Some(SavedOpen {
                target,
                lease,
                document: None,
                done: None,
                waiter: None,
                stage: None,
                disk: None,
                token: jscompat::random_uuid(),
                applying: false,
                waiting: false,
                failure: None,
                losses,
            }),
        }))
    }

    /// Registers the exact document under the barrier, then prepares its
    /// asset grant off the core; the reply waits for the renderer's request.
    pub(crate) fn load_saved(
        self: &Rc<Self>,
        document: NativeSavedDocument,
        done: Then<SavedDocumentLoadOutcome>,
    ) {
        let target = document.target().clone();
        let view = target.view();
        let effects = match effects(self) {
            Ok(effects) => effects,
            Err(error) => return done(Err(kit::error(error))),
        };
        let lease_owner = match self.capture_document_owner(view, document.owner().clone()) {
            Ok(owner) => owner,
            Err(error) => return done(Err(error)),
        };
        let Some(owner_loss) = document.owner().loss_token() else {
            return done(Err(kit::error(
                "The saved document owner lifetime is unavailable",
            )));
        };
        let weak = Rc::downgrade(self);
        let loss_target = target.clone();
        let notify = kit::back_on_core(&self.services, move |()| {
            if let Some(docs) = weak.upgrade() {
                fail(&docs, &loss_target, "The saved document owner was released");
            }
        });
        let mut owner_subscription = Some(owner_loss.on_cancel(move || notify(())));
        let mut completion = Some(done);
        let extra = target.additional().iter().collect::<Vec<_>>();
        let admitted = effects.with_native_saved_document_input(
            &document,
            std::slice::from_ref(&lease_owner),
            target.cancel(),
            target.pending(),
            &extra,
            &mut |_| {
                let mut saved = self.saved.borrow_mut();
                let slot = saved
                    .get_mut(&view)
                    .filter(|slot| slot.target.same_target(&target))
                    .ok_or("The saved document barrier was released")?;
                if slot.failure.is_some()
                    || slot.document.is_some()
                    || !lease_owner
                        .context
                        .document_lease()
                        .is_some_and(|lease| lease.same_generation(&slot.lease))
                {
                    return Err("The saved document cannot be registered".into());
                }
                let subscription = owner_subscription
                    .take()
                    .ok_or("The saved document owner subscription was consumed")?;
                slot.document = Some((document.clone(), lease_owner.clone()));
                slot.done = completion.take();
                slot.losses.push(subscription);
                Ok(())
            },
        );
        if let Err(error) = admitted {
            if let Some(done) = completion {
                done(Err(kit::error(error)));
            }
            return;
        }
        let path = match path_of(&target) {
            Ok(path) => path.to_owned(),
            Err(error) => return fail(self, &target, &error),
        };
        let prepare = self.hooks.saved_asset_paths.clone();
        let owner = lease_owner.clone();
        let docs = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                let stage = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    prepare(&owner, Some(path.as_str()), Some(path.as_str()))
                }))
                .unwrap_or_else(|_| Err("saved asset preparation panicked".into()))?;
                Ok::<_, String>((stage, record_disk_state(&path)))
            },
            move |prepared| {
                let (stage, disk) = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => return fail(&docs, &target, &error),
                };
                let kept = {
                    let mut saved = docs.saved.borrow_mut();
                    match saved
                        .get_mut(&view)
                        .filter(|slot| slot.target.same_target(&target) && slot.failure.is_none())
                    {
                        Some(slot) => {
                            slot.stage = Some(stage);
                            slot.disk = disk;
                            None
                        }
                        None => Some(stage),
                    }
                };
                if let Some(stage) = kept {
                    let _ = (stage.revoke)();
                    return;
                }
                docs.saved_ready(view);
            },
        );
    }

    /// The renderer's first request: held until the document is registered.
    /// `Err` hands the responder back when this view has no saved barrier.
    pub(crate) fn consume_saved(
        self: &Rc<Self>,
        cx: &CallContext,
        responder: Responder,
    ) -> Result<(), Responder> {
        let view = cx.sender.view;
        let mut saved = self.saved.borrow_mut();
        let Some(slot) = saved.get_mut(&view) else {
            return Err(responder);
        };
        if let Some(error) = slot.failure.clone() {
            drop(saved);
            responder.reply(Err(kit::error(error)));
            return Ok(());
        }
        let same_renderer = cx
            .native
            .as_ref()
            .and_then(|native| native.renderer())
            .is_some_and(|renderer| renderer.same_generation(slot.target.created()));
        if cx.sender.surface != slot.target.surface() || !same_renderer || slot.waiter.is_some() {
            drop(saved);
            responder.reply(Err(kit::error(
                "The saved document request belongs to another renderer",
            )));
            return Ok(());
        }
        let weak = Rc::downgrade(self);
        let target = slot.target.clone();
        let notify = kit::back_on_core(&self.services, move |()| {
            if let Some(docs) = weak.upgrade() {
                fail(&docs, &target, "The saved document request was released");
            }
        });
        slot.losses.push(cx.cancel.on_cancel(move || notify(())));
        slot.waiter = Some((cx.clone(), responder));
        drop(saved);
        self.saved_ready(view);
        Ok(())
    }

    fn saved_ready(self: &Rc<Self>, view: ViewId) {
        let ready = {
            let mut saved = self.saved.borrow_mut();
            let Some(slot) = saved.get_mut(&view) else {
                return;
            };
            if slot.failure.is_some()
                || slot.applying
                || slot.waiting
                || slot.stage.is_none()
                || slot.waiter.is_none()
            {
                return;
            }
            let Some((document, lease_owner)) = slot.document.clone() else {
                return;
            };
            let Some((cx, responder)) = slot.waiter.take() else {
                return;
            };
            (slot.target.clone(), document, lease_owner, cx, responder)
        };
        let (target, document, lease_owner, cx, responder) = ready;
        let effects = match effects(self) {
            Ok(effects) => effects,
            Err(error) => {
                responder.reply(Err(kit::error(error.clone())));
                return fail(self, &target, &error);
            }
        };
        let Some(context) = cx.native.clone() else {
            responder.reply(Err(kit::error(
                "The saved document request has no native owner",
            )));
            return fail(self, &target, "The saved document request has no native owner");
        };
        let actual = NativeRequestOwner { view, context };
        let path = match path_of(&target) {
            Ok(path) => path.to_owned(),
            Err(error) => {
                responder.reply(Err(kit::error(error.clone())));
                return fail(self, &target, &error);
            }
        };
        let text = String::from_utf8_lossy(target.input().bytes()).into_owned();
        let mut value = None;
        let mut busy = None;
        let extra = target.additional().iter().collect::<Vec<_>>();
        let admitted = effects.with_native_saved_document_same_origin(
            &document,
            &actual,
            &cx.cancel,
            std::slice::from_ref(&lease_owner),
            target.cancel(),
            target.pending(),
            &extra,
            &mut |_| {
                let mut saved = self.saved.borrow_mut();
                let slot = saved
                    .get_mut(&view)
                    .filter(|slot| slot.target.same_target(&target) && slot.failure.is_none())
                    .ok_or("The saved document barrier was released")?;
                let mut stage = slot
                    .stage
                    .take()
                    .ok_or("The saved document asset grant was consumed")?;
                let mut views = self.views.borrow_mut();
                let state = views
                    .get_mut(&view)
                    .filter(|state| {
                        state.tracked
                            && state.lease.same_generation(&slot.lease)
                            && state.open.is_none()
                            && state.save.is_none()
                    })
                    .ok_or("The document changed before its saved input applied")?;
                let wake = (stage.stage.subscribe)();
                if !(stage.stage.commit)()? {
                    slot.stage = Some(stage);
                    busy = Some(wake);
                    return Ok(());
                }
                state.open = Some(path.clone());
                state.save = Some(path.clone());
                state.allowed.insert(path.clone());
                if let Some(disk) = slot.disk.take() {
                    state.disk.insert(path.clone(), disk);
                }
                slot.applying = true;
                value = Some(obj([
                    ("path", path.clone().into_ipc()),
                    ("text", text.clone().into_ipc()),
                    ("savedInputToken", slot.token.clone().into_ipc()),
                    ("recovered", false.into_ipc()),
                ]));
                Ok(())
            },
        );
        match admitted {
            Ok(()) => match (value, busy) {
                (Some(value), _) => responder.reply(Ok(value)),
                (None, Some(wake)) => {
                    self.park_saved(view, target, lease_owner, cx, responder, wake)
                }
                (None, None) => responder.reply(Err(kit::error(
                    "The saved document response was not admitted",
                ))),
            },
            Err(error) => {
                responder.reply(Err(kit::error(error.clone())));
                fail(self, &target, &error);
            }
        }
    }

    /// The asset grant is held by another operation: the request waits on a
    /// worker for it to free up, then the load tries again. Any lost owner
    /// ends the wait and fails the load.
    fn park_saved(
        self: &Rc<Self>,
        view: ViewId,
        target: NativeSavedTarget,
        lease_owner: NativeRequestOwner,
        cx: CallContext,
        responder: Responder,
        wake: crate::html::RecoveryAssetWaiter,
    ) {
        let parked = {
            let mut saved = self.saved.borrow_mut();
            match saved.get_mut(&view).filter(|slot| {
                slot.target.same_target(&target) && slot.failure.is_none() && !slot.waiting
            }) {
                Some(slot) => {
                    slot.waiting = true;
                    slot.waiter = Some((cx.clone(), responder));
                    None
                }
                None => Some(responder),
            }
        };
        if let Some(responder) = parked {
            return responder.reply(Err(kit::error(RELEASED)));
        }
        let mut tokens = vec![
            cx.cancel.clone(),
            target.cancel().clone(),
            target.pending().clone(),
            target.created().loss_token(),
        ];
        tokens.extend(target.additional().iter().cloned());
        for owner in std::iter::once(&lease_owner).chain(target.participants().iter()) {
            if let Some(loss) = owner.loss_token() {
                tokens.push(loss);
            }
        }
        let docs = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wake(tokens)))
                    .unwrap_or_else(|_| Err("the saved asset wait worker panicked".into()))
            },
            move |result| {
                if let Err(error) = result {
                    return fail(&docs, &target, &error);
                }
                if let Some(slot) = docs
                    .saved
                    .borrow_mut()
                    .get_mut(&view)
                    .filter(|slot| slot.target.same_target(&target))
                {
                    slot.waiting = false;
                }
                docs.saved_ready(view);
            },
        );
    }

    /// The renderer applied (or refused) the retained text it was given.
    pub(crate) fn saved_input_applied(
        self: &Rc<Self>,
        cx: &CallContext,
        token: &str,
        outcome: &str,
        error: &str,
    ) -> Result<(), IpcError> {
        let view = cx.sender.view;
        let found = self
            .saved
            .borrow()
            .get(&view)
            .filter(|slot| slot.token == token && slot.applying && slot.failure.is_none())
            .and_then(|slot| slot.document.clone());
        let Some((document, lease_owner)) = found else {
            return Err(kit::error("The saved document receipt is invalid"));
        };
        let target = document.target().clone();
        let context = cx
            .native
            .clone()
            .ok_or_else(|| kit::error("The saved document receipt has no native owner"))?;
        let actual = NativeRequestOwner { view, context };
        let accepted = match outcome {
            "applied" | "recovered" => true,
            "failed" => false,
            _ => return Err(kit::error("The saved document outcome is invalid")),
        };
        let mut completed = None;
        let extra = target.additional().iter().collect::<Vec<_>>();
        effects(self)
            .and_then(|effects| {
                effects.with_native_saved_document_same_origin(
                    &document,
                    &actual,
                    &cx.cancel,
                    std::slice::from_ref(&lease_owner),
                    target.cancel(),
                    target.pending(),
                    &extra,
                    &mut |_| {
                        let mut saved = self.saved.borrow_mut();
                        let slot = saved
                            .get_mut(&view)
                            .filter(|slot| {
                                slot.target.same_target(&target)
                                    && slot.token == token
                                    && slot.applying
                                    && slot.failure.is_none()
                            })
                            .ok_or("The saved document receipt was replaced")?;
                        if accepted {
                            completed = slot.done.take();
                            saved.remove(&view);
                        } else {
                            slot.failure = Some(error.to_owned());
                            completed = slot.done.take();
                        }
                        Ok(())
                    },
                )
            })
            .map_err(kit::error)?;
        if let Some(done) = completed {
            done(if accepted {
                Ok(if outcome == "recovered" {
                    SavedDocumentLoadOutcome::RecoveryApplied
                } else {
                    SavedDocumentLoadOutcome::Applied
                })
            } else {
                Err(kit::error(error))
            });
        }
        Ok(())
    }
}
