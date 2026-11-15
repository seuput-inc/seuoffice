use super::app::{MindmapApp, Unopened};
use super::hosts::OpenStatus;
use crate::value::{buffer, obj, string, thrown};
use shell_contract::editors::{PreparedSavedDocument, Then};
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{
    CallContext, NativeBrowserTransition, NativeSavedDocument, NativeSavedTarget, Responder,
};
use shell_contract::runtime::CancelSubscription;
use shell_contract::{IpcError, IpcValue, Surface, ViewId};
use std::collections::HashSet;

pub(super) enum WaitKind {
    Pending,
    Open(String),
}
struct Waiter {
    id: u64,
    cx: CallContext,
    kind: WaitKind,
    reply: Responder,
    _loss: CancelSubscription,
}
pub(super) struct SavedLoad {
    pub(super) target: NativeSavedTarget,
    lease: shell_contract::runtime::RendererGeneration,
    document: Option<NativeSavedDocument>,
    lease_owner: Option<shell_contract::ipc::NativeRequestOwner>,
    response: Option<IpcValue>,
    token: String,
    waiters: Vec<Waiter>,
    next_waiter: u64,
    done: Option<Then<shell_contract::editors::SavedDocumentLoadOutcome>>,
    applying: bool,
    failure: Option<String>,
    _losses: Vec<CancelSubscription>,
}

struct Prepared {
    app: MindmapApp,
    target: NativeSavedTarget,
    slot: Option<SavedLoad>,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if self.slot.is_some() {
            let mut state = self.app.0.state.borrow_mut();
            state.saved_reservations -= 1;
        }
    }
}
impl PreparedSavedDocument for Prepared {
    fn commit(&mut self, transition: &NativeBrowserTransition<'_>) -> Result<(), IpcError> {
        if !transition.matches(self.target.reservation())
            || !(transition.matches_owner(self.target.browser())
                || transition.proposes_owner(self.target.browser()))
        {
            return Err(thrown("The saved map target belongs to another browser"));
        }
        let mut state = self.app.0.state.borrow_mut();
        if !state.live.contains(&self.target.view())
            || state.saved_loads.contains_key(&self.target.view())
            || state.saved_reservations == 0
            || self.slot.as_ref().is_none_or(|slot| {
                !state
                    .documents
                    .get(&self.target.view())
                    .is_some_and(|lease| lease.same_generation(&slot.lease))
            })
        {
            return Err(thrown("The saved map target is unavailable"));
        }
        let slot = self
            .slot
            .take()
            .ok_or_else(|| thrown("The saved map barrier was consumed"))?;
        state.saved_reservations -= 1;
        state.saved_loads.insert(self.target.view(), slot);
        Ok(())
    }
}

pub(super) fn prepare(
    app: &MindmapApp,
    target: NativeSavedTarget,
) -> Result<Box<dyn PreparedSavedDocument>, IpcError> {
    if target.surface() != Surface::Mindmap {
        return Err(thrown("The saved target is not a mind map"));
    }
    target
        .path()
        .path()
        .to_str()
        .ok_or_else(|| thrown("The saved map path is not valid Unicode"))?;
    let lease = app
        .0
        .state
        .borrow()
        .documents
        .get(&target.view())
        .cloned()
        .ok_or_else(|| thrown("The saved map document is unavailable"))?;
    let mut controls = vec![
        target.cancel().clone(),
        target.pending().clone(),
        target.created().loss_token(),
        lease.loss_token(),
    ];
    controls.extend_from_slice(target.additional());
    for participant in target.participants() {
        if let Some(loss) = participant.loss_token() {
            controls.push(loss);
        }
    }
    let mut losses = Vec::with_capacity(controls.len() + 1);
    for control in controls {
        let core = app.0.services.core.clone();
        let captured = target.clone();
        losses.push(control.on_cancel(move || {
            core.post(Box::new(move || {
                super::with_app(|app| fail(app, &captured, "The saved map operation was released"))
            }))
        }));
    }
    let slot = SavedLoad {
        target: target.clone(),
        lease,
        document: None,
        lease_owner: None,
        response: None,
        token: jscompat::random_uuid(),
        waiters: Vec::new(),
        next_waiter: 0,
        done: None,
        applying: false,
        failure: None,
        _losses: losses,
    };
    {
        let mut state = app.0.state.borrow_mut();
        let reservations = state
            .saved_reservations
            .checked_add(1)
            .ok_or_else(|| thrown("The saved map barrier capacity is exhausted"))?;
        state
            .saved_loads
            .try_reserve(reservations)
            .map_err(|_| thrown("The saved map barrier could not be allocated"))?;
        state.saved_reservations = reservations;
    }
    Ok(Box::new(Prepared {
        app: app.clone(),
        target,
        slot: Some(slot),
    }))
}

fn effects(app: &MindmapApp) -> Result<OwnedEffectsHost, IpcError> {
    app.0
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or_else(|| thrown("The saved map effect host is unavailable"))
}

pub(super) fn fail(app: &MindmapApp, target: &NativeSavedTarget, message: &str) {
    let (waiters, done) = {
        let mut state = app.0.state.borrow_mut();
        let Some(slot) = state
            .saved_loads
            .get_mut(&target.view())
            .filter(|slot| slot.target.same_target(target))
        else {
            return;
        };
        if slot.failure.is_some() {
            return;
        }
        slot.failure = Some(message.to_owned());
        (std::mem::take(&mut slot.waiters), slot.done.take())
    };
    for waiter in waiters {
        waiter.reply.reply(Err(thrown(message)));
    }
    if let Some(done) = done {
        done(Err(thrown(message)));
    }
}

pub(super) fn load(
    app: &MindmapApp,
    document: NativeSavedDocument,
    done: Then<shell_contract::editors::SavedDocumentLoadOutcome>,
) {
    let target = document.target().clone();
    let view = target.view();
    let effects = match effects(app) {
        Ok(value) => value,
        Err(error) => return done(Err(error)),
    };
    let lease = match app
        .0
        .state
        .borrow()
        .saved_loads
        .get(&view)
        .filter(|slot| slot.target.same_target(&target))
        .map(|slot| slot.lease.clone())
    {
        Some(lease) => lease,
        None => return done(Err(thrown("The saved map barrier was released"))),
    };
    let mut lease_owner = document.owner().clone();
    if lease_owner
        .context
        .document_lease()
        .is_some_and(|existing| !existing.same_generation(&lease))
    {
        return done(Err(thrown("The saved map document was replaced")));
    }
    lease_owner.context = lease_owner.context.with_document_lease(lease);
    let Some(owner_loss) = document.owner().loss_token() else {
        return done(Err(thrown("The saved map owner lifetime is unavailable")));
    };
    let core = app.0.services.core.clone();
    let loss_target = target.clone();
    let mut owner_subscription = Some(owner_loss.on_cancel(move || {
        core.post(Box::new(move || {
            super::with_app(|app| fail(app, &loss_target, "The saved map owner was released"))
        }))
    }));
    let mut completion = Some(done);
    let admitted = effects.with_native_saved_document_input(
        &document,
        std::slice::from_ref(&lease_owner),
        target.cancel(),
        target.pending(),
        &[],
        &mut |_| {
            let mut state = app.0.state.borrow_mut();
            if !state
                .saved_loads
                .get(&view)
                .zip(state.documents.get(&view))
                .is_some_and(|(slot, lease)| slot.lease.same_generation(lease))
            {
                return Err("The saved map document was replaced".into());
            }
            let slot = state
                .saved_loads
                .get_mut(&view)
                .filter(|slot| slot.target.same_target(&target))
                .ok_or("The saved map barrier was released")?;
            if slot.failure.is_some() {
                return Err("The saved map operation was released".into());
            }
            if slot.document.is_some() {
                return Err("The saved map was already registered".into());
            }
            let owner_subscription = owner_subscription
                .take()
                .ok_or("The saved map owner subscription was consumed")?;
            slot.document = Some(document.clone());
            slot.lease_owner = Some(lease_owner.clone());
            slot._losses.push(owner_subscription);
            slot.done = completion.take();
            Ok(())
        },
    );
    if let Err(error) = admitted {
        if let Some(done) = completion {
            done(Err(thrown(error)));
        }
        return;
    }
    let input = target.input().clone();
    let engine = app.0.engine.clone();
    app.offload(
        move || {
            let opened = engine.open_xmind(input.bytes())?;
            if opened.status != OpenStatus::Ok {
                return Err(opened.status.as_str().to_owned());
            }
            Ok::<_, String>(obj([
                ("status", string("ok")),
                ("bytes", buffer(input.bytes().to_vec())),
            ]))
        },
        move |app, result| {
            let response = match result {
                Ok(response) => response,
                Err(error) => return fail(app, &target, &error),
            };
            let path = match target.path().path().to_str() {
                Some(path) => path.to_owned(),
                None => return fail(app, &target, "The saved map path is not valid Unicode"),
            };
            let mut response = Some(response);
            let admitted = effects.with_native_saved_document_input(
                &document,
                std::slice::from_ref(&lease_owner),
                target.cancel(),
                target.pending(),
                &[],
                &mut |_| {
                    let mut state = app.0.state.borrow_mut();
                    if !state
                        .saved_loads
                        .get(&view)
                        .zip(state.documents.get(&view))
                        .is_some_and(|(slot, lease)| slot.lease.same_generation(lease))
                    {
                        return Err("The saved map document was replaced".into());
                    }
                    let slot = state
                        .saved_loads
                        .get_mut(&view)
                        .filter(|slot| slot.target.same_target(&target))
                        .ok_or("The saved map barrier was released")?;
                    slot.response = response.take();
                    state.open_path.insert(view, path.clone());
                    state.allowed.insert(view, HashSet::from([path.clone()]));
                    state.unopened.insert(view, Unopened::Pending);
                    Ok(())
                },
            );
            if let Err(error) = admitted {
                return fail(app, &target, &error);
            }
            drain(app, view);
        },
    );
}

pub(super) fn dispatch(
    app: &MindmapApp,
    cx: &CallContext,
    kind: WaitKind,
    reply: Responder,
) -> Result<(), Responder> {
    let view = cx.sender.view;
    let mut state = app.0.state.borrow_mut();
    let Some(slot) = state.saved_loads.get_mut(&view) else {
        return Err(reply);
    };
    if let Some(error) = slot.failure.clone() {
        drop(state);
        reply.reply(Err(thrown(error)));
        return Ok(());
    }
    let Some(id) = slot.next_waiter.checked_add(1) else {
        drop(state);
        reply.reply(Err(thrown("The saved map request sequence is exhausted")));
        return Ok(());
    };
    slot.next_waiter = id;
    let target = slot.target.clone();
    let core = app.0.services.core.clone();
    let loss = cx.cancel.on_cancel(move || {
        core.post(Box::new(move || {
            super::with_app(|app| cancel_waiter(app, &target, id))
        }))
    });
    slot.waiters.push(Waiter {
        id,
        cx: cx.clone(),
        kind,
        reply,
        _loss: loss,
    });
    drop(state);
    drain(app, view);
    Ok(())
}

fn cancel_waiter(app: &MindmapApp, target: &NativeSavedTarget, id: u64) {
    let waiter = {
        let mut state = app.0.state.borrow_mut();
        let Some(slot) = state
            .saved_loads
            .get_mut(&target.view())
            .filter(|slot| slot.target.same_target(target))
        else {
            return;
        };
        slot.waiters
            .iter()
            .position(|waiter| waiter.id == id)
            .map(|index| slot.waiters.swap_remove(index))
    };
    if let Some(waiter) = waiter {
        waiter
            .reply
            .reply(Err(thrown("The saved map request was cancelled")));
    }
}

fn drain(app: &MindmapApp, view: ViewId) {
    let ready = {
        let mut state = app.0.state.borrow_mut();
        let Some(slot) = state.saved_loads.get_mut(&view) else {
            return;
        };
        let (Some(document), Some(lease_owner)) = (slot.document.clone(), slot.lease_owner.clone())
        else {
            return;
        };
        if slot.response.is_none() && !slot.applying {
            return;
        }
        (
            slot.target.clone(),
            document,
            lease_owner,
            std::mem::take(&mut slot.waiters),
        )
    };
    let (target, document, lease_owner, waiters) = ready;
    let effects = match effects(app) {
        Ok(value) => value,
        Err(error) => {
            for waiter in waiters {
                waiter.reply.reply(Err(error.clone()));
            }
            return fail(app, &target, &error.message);
        }
    };
    for waiter in waiters {
        let actual = match super::owned_save::native_owner(&waiter.cx) {
            Ok(owner) => owner,
            Err(error) => {
                waiter.reply.reply(Err(error));
                continue;
            }
        };
        let mut value = None;
        let admitted = effects.with_native_saved_document_same_origin(
            &document,
            &actual,
            &waiter.cx.cancel,
            std::slice::from_ref(&lease_owner),
            target.cancel(),
            target.pending(),
            &[],
            &mut |_| {
                let mut state = app.0.state.borrow_mut();
                if !state
                    .saved_loads
                    .get(&view)
                    .zip(state.documents.get(&view))
                    .is_some_and(|(slot, lease)| slot.lease.same_generation(lease))
                {
                    return Err("The saved map document was replaced".into());
                }
                let slot = state
                    .saved_loads
                    .get_mut(&view)
                    .filter(|slot| slot.target.same_target(&target))
                    .ok_or("The saved map barrier was released")?;
                let path = target
                    .path()
                    .path()
                    .to_str()
                    .ok_or("The saved map path is not valid Unicode")?;
                value = Some(match &waiter.kind {
                    WaitKind::Pending => string(path),
                    WaitKind::Open(requested) => {
                        if requested != path || slot.applying {
                            return Err("The saved map open does not match its target".into());
                        }
                        let mut value = slot
                            .response
                            .take()
                            .ok_or("The saved map response was consumed")?;
                        let IpcValue::Object(fields) = &mut value else {
                            return Err("The saved map response is invalid".into());
                        };
                        fields.push(("recoveryToken".into(), string(slot.token.clone())));
                        slot.applying = true;
                        value
                    }
                });
                Ok(())
            },
        );
        waiter.reply.reply(
            admitted.map_err(thrown).and_then(|()| {
                value.ok_or_else(|| thrown("The saved map response was not admitted"))
            }),
        );
    }
}

pub(super) fn initialized(
    app: &MindmapApp,
    cx: &CallContext,
    token: &str,
    ok: bool,
    error: &str,
) -> Result<bool, IpcError> {
    let view = cx.sender.view;
    let found = app
        .0
        .state
        .borrow()
        .saved_loads
        .get(&view)
        .filter(|slot| slot.token == token && slot.applying)
        .and_then(|slot| Some((slot.document.clone()?, slot.lease_owner.clone()?)));
    let Some((document, lease_owner)) = found else {
        return Ok(false);
    };
    let target = document.target().clone();
    let actual = super::owned_save::native_owner(cx)?;
    let mut completed = None;
    effects(app)?
        .with_native_saved_document_same_origin(
            &document,
            &actual,
            &cx.cancel,
            std::slice::from_ref(&lease_owner),
            target.cancel(),
            target.pending(),
            &[],
            &mut |_| {
                let mut state = app.0.state.borrow_mut();
                if !state
                    .saved_loads
                    .get(&view)
                    .zip(state.documents.get(&view))
                    .is_some_and(|(slot, lease)| slot.lease.same_generation(lease))
                {
                    return Err("The saved map document was replaced".into());
                }
                let slot = state
                    .saved_loads
                    .get(&view)
                    .filter(|slot| {
                        slot.target.same_target(&target) && slot.token == token && slot.applying
                    })
                    .ok_or("The saved map initialization was released")?;
                let path = slot
                    .target
                    .path()
                    .path()
                    .to_str()
                    .ok_or("The saved map path is not valid Unicode")?
                    .to_owned();
                if ok {
                    state.unopened.remove(&view);
                    state.save_path.insert(view, path);
                    completed = state
                        .saved_loads
                        .remove(&view)
                        .and_then(|mut slot| slot.done.take());
                } else {
                    state.unopened.insert(view, Unopened::Failed);
                    let slot = state
                        .saved_loads
                        .get_mut(&view)
                        .ok_or("The saved map initialization was released")?;
                    slot.failure = Some(error.to_owned());
                    completed = slot.done.take();
                }
                Ok(())
            },
        )
        .map_err(thrown)?;
    if let Some(done) = completed {
        done(if ok {
            Ok(shell_contract::editors::SavedDocumentLoadOutcome::Applied)
        } else {
            Err(thrown(error))
        });
    }
    Ok(true)
}
