//! A committed output opened as an editor's first document. The retained
//! bytes, never a reopened path, are what the renderer loads. The barrier
//! holds the renderer's first requests until the exact document is registered
//! and installed, and the renderer's receipt completes the load.
//!
//! EPUB, PDF and HWP share this: each supplies only what differs, the lease
//! of its document, its answer to a held request and its state change.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use shell_contract::editors::{
    PreparedSavedDocument, SavedActorRegistration, SavedDocumentLoadOutcome, Then,
};
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{
    CallContext, NativeBrowserSuperseded, NativeBrowserTransition, NativeClosureReceipt,
    NativeRequestOwner, NativeSavedDocument, NativeSavedInput, NativeSavedTarget, Responder,
};
use shell_contract::runtime::{CancelSubscription, CancelToken, Services};
use shell_contract::{IpcError, IpcValue, Surface, ViewId};

use crate::kit;

const RELEASED: &str = "The saved document operation was released";

/// What one editor family gives the shared barrier.
pub(crate) trait SavedHost: Sized + 'static {
    /// The identity of the document this load targets.
    type Lease: Clone;
    /// What a held renderer request asks for.
    type Kind;
    /// What the editor installed for the load.
    type Data: Clone + Default;

    fn surface() -> Surface;
    fn services(&self) -> &Services;
    fn slots(&self) -> &SavedSlots<Self>;
    /// The view's live document, `None` when it has none.
    fn lease(&self, view: ViewId) -> Option<Self::Lease>;
    fn lease_current(&self, view: ViewId, lease: &Self::Lease) -> bool;
    fn lease_loss(lease: &Self::Lease) -> CancelToken;
    fn capture_owner(
        &self,
        view: ViewId,
        native: NativeRequestOwner,
    ) -> Result<NativeRequestOwner, IpcError>;
    /// The answer to one held request, inside the request's admission. The
    /// flag says the answer starts the renderer applying the document.
    fn answer(
        &self,
        view: ViewId,
        kind: &Self::Kind,
        answer: &Answer<'_, Self::Data>,
    ) -> Result<(IpcValue, bool), String>;
    /// The renderer's receipt, inside its admission and before the barrier
    /// ends: the state change a failed load needs.
    fn settled(&self, view: ViewId, ok: bool, error: &str);
}

pub(crate) struct Answer<'a, D> {
    pub path: &'a str,
    pub token: &'a str,
    pub applying: bool,
    pub data: &'a D,
}

struct Waiter<K> {
    id: u64,
    cx: CallContext,
    kind: K,
    reply: Responder,
    _loss: CancelSubscription,
}

pub(crate) struct SavedLoad<H: SavedHost> {
    target: NativeSavedTarget,
    lease: H::Lease,
    document: Option<NativeSavedDocument>,
    lease_owner: Option<NativeRequestOwner>,
    installed: bool,
    token: String,
    waiters: Vec<Waiter<H::Kind>>,
    next_waiter: u64,
    done: Option<Then<SavedDocumentLoadOutcome>>,
    applying: bool,
    failure: Option<String>,
    data: H::Data,
    losses: Vec<CancelSubscription>,
}

/// The barriers of one editor, by view.
pub(crate) struct SavedSlots<H: SavedHost>(RefCell<HashMap<ViewId, SavedLoad<H>>>);

impl<H: SavedHost> Default for SavedSlots<H> {
    fn default() -> Self {
        Self(RefCell::default())
    }
}

impl<H: SavedHost> SavedSlots<H> {
    pub(crate) fn holds(&self, view: ViewId) -> bool {
        self.0.borrow().contains_key(&view)
    }

    /// The barrier fails its waiters and its load once.
    pub(crate) fn fail(&self, target: &NativeSavedTarget, message: &str) {
        let (waiters, done) = {
            let mut slots = self.0.borrow_mut();
            let Some(slot) = slots
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
            waiter.reply.reply(Err(kit::error(message)));
        }
        if let Some(done) = done {
            done(Err(kit::error(message)));
        }
    }

    fn release(&self, target: &NativeSavedTarget, message: &str) {
        self.fail(target, message);
        let removed = {
            let mut slots = self.0.borrow_mut();
            if slots
                .get(&target.view())
                .is_some_and(|slot| slot.target.same_target(target))
            {
                slots.remove(&target.view())
            } else {
                None
            }
        };
        drop(removed);
    }

    /// The barrier of a destroyed view fails its waiters and is dropped.
    pub(crate) fn release_view(&self, view: ViewId) {
        let target = self.0.borrow().get(&view).map(|slot| slot.target.clone());
        if let Some(target) = target {
            self.release(&target, "The document view was destroyed");
        }
    }

    fn cancel_waiter(&self, target: &NativeSavedTarget, id: u64) {
        let waiter = {
            let mut slots = self.0.borrow_mut();
            let Some(slot) = slots
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
                .reply(Err(kit::error("The saved document request was cancelled")));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prepared,
    Committed,
    Closed,
    Superseded,
    RolledBack,
}

struct Registration<H: SavedHost> {
    host: Weak<H>,
    target: NativeSavedTarget,
    phase: Cell<Phase>,
}

struct Prepared<H: SavedHost> {
    host: Rc<H>,
    registration: Rc<Registration<H>>,
    slot: Option<SavedLoad<H>>,
}

impl<H: SavedHost> PreparedSavedDocument for Prepared<H> {
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
        if self.host.slots().holds(target.view())
            || !self.host.lease_current(target.view(), &slot.lease)
        {
            self.slot = Some(slot);
            return Err(kit::error("The saved document target is unavailable"));
        }
        if let Err(error) = next() {
            self.slot = Some(slot);
            return Err(error);
        }
        self.host.slots().0.borrow_mut().insert(target.view(), slot);
        registration.phase.set(Phase::Committed);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Rc<dyn SavedActorRegistration> {
        self.registration.clone()
    }
}

impl<H: SavedHost> SavedActorRegistration for Registration<H> {
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
        if let Some(host) = self.host.upgrade() {
            host.slots()
                .release(&self.target, "The document view was closed");
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
        if let Some(host) = self.host.upgrade() {
            host.slots()
                .release(&self.target, "The saved document browser was replaced");
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

pub(crate) fn path_of(target: &NativeSavedTarget) -> Result<&str, String> {
    target
        .path()
        .path()
        .to_str()
        .ok_or_else(|| "The saved document path is not valid Unicode".to_owned())
}

fn effects<H: SavedHost>(host: &H) -> Result<OwnedEffectsHost, String> {
    host.services()
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or_else(|| "Native saved document effects are unavailable".to_owned())
}

fn owner_of(cx: &CallContext) -> Result<NativeRequestOwner, String> {
    let context = cx
        .native
        .clone()
        .ok_or("The saved document request has no native owner")?;
    Ok(NativeRequestOwner {
        view: cx.sender.view,
        context,
    })
}

fn watch<H: SavedHost>(
    host: &Rc<H>,
    target: &NativeSavedTarget,
    control: CancelToken,
    message: &'static str,
) -> CancelSubscription {
    let weak = Rc::downgrade(host);
    let captured = target.clone();
    let notify = kit::back_on_core(host.services(), move |()| {
        if let Some(host) = weak.upgrade() {
            host.slots().fail(&captured, message);
        }
    });
    control.on_cancel(move || notify(()))
}

/// Prepare outside admission: validate the target, reserve nothing global and
/// watch every owner that can end the load.
pub(crate) fn prepare<H: SavedHost>(
    host: &Rc<H>,
    target: NativeSavedTarget,
) -> Result<Box<dyn PreparedSavedDocument>, IpcError> {
    if target.surface() != H::surface() {
        return Err(kit::error("The saved target belongs to another editor"));
    }
    path_of(&target).map_err(kit::error)?;
    let view = target.view();
    let lease = host
        .lease(view)
        .ok_or_else(|| kit::error("The saved document view is not open"))?;
    let mut controls = vec![
        target.cancel().clone(),
        target.pending().clone(),
        target.created().loss_token(),
        H::lease_loss(&lease),
    ];
    controls.extend_from_slice(target.additional());
    for owner in target.participants() {
        controls.push(
            owner
                .loss_token()
                .ok_or_else(|| kit::error("A saved document owner is unavailable"))?,
        );
    }
    let losses = controls
        .into_iter()
        .map(|control| watch(host, &target, control, RELEASED))
        .collect();
    Ok(Box::new(Prepared {
        host: Rc::clone(host),
        registration: Rc::new(Registration {
            host: Rc::downgrade(host),
            target: target.clone(),
            phase: Cell::new(Phase::Prepared),
        }),
        slot: Some(SavedLoad {
            target,
            lease,
            document: None,
            lease_owner: None,
            installed: false,
            token: jscompat::random_uuid(),
            waiters: Vec::new(),
            next_waiter: 0,
            done: None,
            applying: false,
            failure: None,
            data: H::Data::default(),
            losses,
        }),
    }))
}

/// Register the exact document under the barrier, then hand it to `install`,
/// which parses off gate and calls [`installed`] or fails the barrier.
pub(crate) fn load<H: SavedHost>(
    host: &Rc<H>,
    document: NativeSavedDocument,
    done: Then<SavedDocumentLoadOutcome>,
    install: impl FnOnce(&Rc<H>, NativeSavedDocument, NativeRequestOwner),
) {
    let target = document.target().clone();
    let view = target.view();
    let effects = match effects(&**host) {
        Ok(effects) => effects,
        Err(error) => return done(Err(kit::error(error))),
    };
    let lease_owner = match host.capture_owner(view, document.owner().clone()) {
        Ok(owner) => owner,
        Err(error) => return done(Err(error)),
    };
    let Some(owner_loss) = document.owner().loss_token() else {
        return done(Err(kit::error(
            "The saved document owner lifetime is unavailable",
        )));
    };
    let mut owner_subscription = Some(watch(
        host,
        &target,
        owner_loss,
        "The saved document owner was released",
    ));
    let mut completion = Some(done);
    let extra = target.additional().iter().collect::<Vec<_>>();
    let admitted = effects.with_native_saved_document_input(
        &document,
        std::slice::from_ref(&lease_owner),
        target.cancel(),
        target.pending(),
        &extra,
        &mut |_| {
            let mut slots = host.slots().0.borrow_mut();
            let slot = slots
                .get_mut(&view)
                .filter(|slot| slot.target.same_target(&target))
                .ok_or("The saved document barrier was released")?;
            if slot.failure.is_some()
                || slot.document.is_some()
                || !host.lease_current(view, &slot.lease)
            {
                return Err("The saved document cannot be registered".into());
            }
            let subscription = owner_subscription
                .take()
                .ok_or("The saved document owner subscription was consumed")?;
            slot.document = Some(document.clone());
            slot.lease_owner = Some(lease_owner.clone());
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
    install(host, document, lease_owner);
}

/// The editor has the document ready: `apply` makes its state change inside
/// the load's admission, and the held requests are answered.
pub(crate) fn installed<H: SavedHost>(
    host: &Rc<H>,
    document: &NativeSavedDocument,
    lease_owner: &NativeRequestOwner,
    data: H::Data,
    apply: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let target = document.target().clone();
    let view = target.view();
    let effects = effects(&**host)?;
    let extra = target.additional().iter().collect::<Vec<_>>();
    let mut data = Some(data);
    effects.with_native_saved_document_input(
        document,
        std::slice::from_ref(lease_owner),
        target.cancel(),
        target.pending(),
        &extra,
        &mut |_| {
            {
                let slots = host.slots().0.borrow();
                let slot = slots
                    .get(&view)
                    .filter(|slot| slot.target.same_target(&target) && slot.failure.is_none())
                    .ok_or("The saved document barrier was released")?;
                if !host.lease_current(view, &slot.lease) {
                    return Err("The saved document was replaced".into());
                }
            }
            apply()?;
            let mut slots = host.slots().0.borrow_mut();
            let slot = slots
                .get_mut(&view)
                .filter(|slot| slot.target.same_target(&target))
                .ok_or("The saved document barrier was released")?;
            slot.data = data
                .take()
                .ok_or("The saved document was already installed")?;
            slot.installed = true;
            Ok(())
        },
    )?;
    drain(host, view);
    Ok(())
}

/// A renderer request the barrier holds until the document is installed.
/// `Err` hands the responder back when this view has no barrier.
pub(crate) fn dispatch<H: SavedHost>(
    host: &Rc<H>,
    cx: &CallContext,
    kind: H::Kind,
    reply: Responder,
) -> Result<(), Responder> {
    let view = cx.sender.view;
    let mut slots = host.slots().0.borrow_mut();
    let Some(slot) = slots.get_mut(&view) else {
        return Err(reply);
    };
    if let Some(error) = slot.failure.clone() {
        drop(slots);
        reply.reply(Err(kit::error(error)));
        return Ok(());
    }
    let Some(id) = slot.next_waiter.checked_add(1) else {
        drop(slots);
        reply.reply(Err(kit::error(
            "The saved document request sequence is exhausted",
        )));
        return Ok(());
    };
    slot.next_waiter = id;
    let target = slot.target.clone();
    let weak = Rc::downgrade(host);
    let notify = kit::back_on_core(host.services(), move |()| {
        if let Some(host) = weak.upgrade() {
            host.slots().cancel_waiter(&target, id);
        }
    });
    let loss = cx.cancel.on_cancel(move || notify(()));
    slot.waiters.push(Waiter {
        id,
        cx: cx.clone(),
        kind,
        reply,
        _loss: loss,
    });
    drop(slots);
    drain(host, view);
    Ok(())
}

fn drain<H: SavedHost>(host: &Rc<H>, view: ViewId) {
    let ready = {
        let mut slots = host.slots().0.borrow_mut();
        let Some(slot) = slots.get_mut(&view) else {
            return;
        };
        if !slot.installed || slot.failure.is_some() || slot.waiters.is_empty() {
            return;
        }
        let (Some(document), Some(lease_owner)) = (slot.document.clone(), slot.lease_owner.clone())
        else {
            return;
        };
        (
            slot.target.clone(),
            document,
            lease_owner,
            slot.token.clone(),
            slot.data.clone(),
            std::mem::take(&mut slot.waiters),
        )
    };
    let (target, document, lease_owner, token, data, waiters) = ready;
    let effects = match effects(&**host) {
        Ok(effects) => effects,
        Err(error) => {
            for waiter in waiters {
                waiter.reply.reply(Err(kit::error(error.clone())));
            }
            return host.slots().fail(&target, &error);
        }
    };
    let path = match path_of(&target) {
        Ok(path) => path.to_owned(),
        Err(error) => {
            for waiter in waiters {
                waiter.reply.reply(Err(kit::error(error.clone())));
            }
            return host.slots().fail(&target, &error);
        }
    };
    let extra = target.additional().iter().collect::<Vec<_>>();
    for waiter in waiters {
        let actual = match owner_of(&waiter.cx) {
            Ok(actual) => actual,
            Err(error) => {
                waiter.reply.reply(Err(kit::error(error)));
                continue;
            }
        };
        let applying = host
            .slots()
            .0
            .borrow()
            .get(&view)
            .is_some_and(|slot| slot.applying);
        let mut value = None;
        let admitted = effects.with_native_saved_document_same_origin(
            &document,
            &actual,
            &waiter.cx.cancel,
            std::slice::from_ref(&lease_owner),
            target.cancel(),
            target.pending(),
            &extra,
            &mut |_| {
                let live =
                    host.slots().0.borrow().get(&view).is_some_and(|slot| {
                        slot.target.same_target(&target) && slot.failure.is_none()
                    });
                if !live {
                    return Err("The saved document barrier was released".into());
                }
                value = Some(host.answer(
                    view,
                    &waiter.kind,
                    &Answer {
                        path: &path,
                        token: &token,
                        applying,
                        data: &data,
                    },
                )?);
                Ok(())
            },
        );
        let answer = admitted.map_err(kit::error).and_then(|()| {
            value.ok_or_else(|| kit::error("The saved document response was not admitted"))
        });
        match answer {
            Ok((value, starts)) => {
                if starts {
                    if let Some(slot) = host.slots().0.borrow_mut().get_mut(&view) {
                        slot.applying = true;
                    }
                }
                waiter.reply.reply(Ok(value));
            }
            Err(error) => waiter.reply.reply(Err(error)),
        }
    }
}

/// The renderer's receipt. `Ok(false)` when this view has no barrier or the
/// token is not this load's, so the caller may treat it as another receipt.
pub(crate) fn initialized<H: SavedHost>(
    host: &Rc<H>,
    cx: &CallContext,
    token: &str,
    ok: bool,
    error: &str,
) -> Result<bool, IpcError> {
    let view = cx.sender.view;
    let found = host
        .slots()
        .0
        .borrow()
        .get(&view)
        .filter(|slot| slot.token == token && slot.applying)
        .and_then(|slot| Some((slot.document.clone()?, slot.lease_owner.clone()?)));
    let Some((document, lease_owner)) = found else {
        return Ok(false);
    };
    let target = document.target().clone();
    let actual = owner_of(cx).map_err(kit::error)?;
    let mut completed = None;
    let extra = target.additional().iter().collect::<Vec<_>>();
    effects(&**host)
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
                    let mut slots = host.slots().0.borrow_mut();
                    let slot = slots
                        .get_mut(&view)
                        .filter(|slot| {
                            slot.target.same_target(&target)
                                && slot.token == token
                                && slot.applying
                                && slot.failure.is_none()
                        })
                        .ok_or("The saved document receipt was replaced")?;
                    host.settled(view, ok, error);
                    if ok {
                        completed = slot.done.take();
                        slots.remove(&view);
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
        done(if ok {
            Ok(SavedDocumentLoadOutcome::Applied)
        } else {
            Err(kit::error(error))
        });
    }
    Ok(true)
}

/// The retained input and the authority a read of it runs under.
#[derive(Clone)]
pub(crate) struct SavedRead {
    input: NativeSavedInput,
    document: NativeSavedDocument,
    actual: NativeRequestOwner,
    lease_owner: NativeRequestOwner,
    cancel: CancelToken,
    effects: OwnedEffectsHost,
}

impl SavedRead {
    pub(crate) fn input(&self) -> &NativeSavedInput {
        &self.input
    }

    pub(crate) fn admit(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let target = self.document.target();
        let extra = target.additional().iter().collect::<Vec<_>>();
        self.effects.with_native_saved_document_same_origin(
            &self.document,
            &self.actual,
            &self.cancel,
            std::slice::from_ref(&self.lease_owner),
            target.cancel(),
            target.pending(),
            &extra,
            &mut |_| effect(),
        )
    }
}

/// A renderer read of the saved path: the retained input, admitted. `Ok(None)`
/// when this view has no barrier, so the caller reads the file as before.
pub(crate) fn read_scope<H: SavedHost>(
    host: &H,
    cx: &CallContext,
    path: &str,
) -> Result<Option<SavedRead>, String> {
    let view = cx.sender.view;
    let (document, lease_owner) = {
        let slots = host.slots().0.borrow();
        let Some(slot) = slots.get(&view) else {
            return Ok(None);
        };
        if let Some(error) = &slot.failure {
            return Err(error.clone());
        }
        if !slot.applying || path_of(&slot.target)? != path {
            return Err("The saved document read does not match its target".into());
        }
        match (&slot.document, &slot.lease_owner) {
            (Some(document), Some(owner)) => (document.clone(), owner.clone()),
            _ => return Err("The saved document is not registered".into()),
        }
    };
    let scope = SavedRead {
        input: document.target().input().clone(),
        document,
        actual: owner_of(cx)?,
        lease_owner,
        cancel: cx.cancel.clone(),
        effects: effects(host)?,
    };
    scope.admit(&mut || Ok(()))?;
    Ok(Some(scope))
}
