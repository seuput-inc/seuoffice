use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use shell_contract::editors::{PreparedSavedDocument, SavedActorRegistration, Then};
use shell_contract::ipc::{
    CallContext, NativeBrowserSuperseded, NativeBrowserTransition, NativeClosureReceipt,
    NativeSavedDocument, NativeSavedTarget, Responder,
};
use shell_contract::runtime::CancelSubscription;
use shell_contract::{IpcError, Surface, ViewId};

use super::state::App;
use crate::value::thrown;

pub(super) struct SavedLoad {
    pub target: NativeSavedTarget,
    pub identity: Arc<()>,
    pub initial_document: shell_contract::runtime::RendererGeneration,
    pub document: Option<NativeSavedDocument>,
    pub waiter: Option<(CallContext, f64, Responder)>,
    pub done: Option<Then<shell_contract::editors::SavedDocumentLoadOutcome>>,
    pub started: bool,
    pub applying: Option<Applying>,
    pub _losses: Vec<CancelSubscription>,
}

pub(super) struct Applying {
    token: String,
    session: u64,
    document: shell_contract::runtime::RendererGeneration,
    owner: shell_contract::ipc::NativeRequestOwner,
    _loss: CancelSubscription,
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
    app: App,
    target: NativeSavedTarget,
    identity: Arc<()>,
    phase: Cell<Phase>,
}

struct Prepared {
    registration: Rc<Registration>,
    slot: Option<SavedLoad>,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.initial_document.revoke();
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
            return Err(thrown(
                "The saved presentation actor was already committed or released",
            ));
        }
        if !transition.matches(target.reservation())
            || !(transition.matches_owner(target.browser())
                || transition.proposes_owner(target.browser()))
        {
            return Err(thrown(
                "The saved presentation belongs to another browser transition",
            ));
        }
        let mut state = registration.app.state.borrow_mut();
        if state.saved_loads.contains_key(&target.view()) {
            return Err(thrown(
                "A saved presentation is already pending for this view",
            ));
        }
        let pending = self
            .slot
            .take()
            .ok_or_else(|| thrown("The saved presentation barrier was already committed"))?;
        if let Err(error) = next() {
            self.slot = Some(pending);
            return Err(error);
        }
        state.saved_loads.insert(target.view(), pending);
        registration.phase.set(Phase::Committed);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Rc<dyn SavedActorRegistration> {
        self.registration.clone()
    }
}

/// Releases what an exactly closed native actor left behind: its owner entry, its
/// attached session and its queued seed. The drops run after the state borrow ends.
pub(super) fn release_closed_actor(app: &App, view: ViewId) {
    let released = {
        let mut state = app.state.borrow_mut();
        state.native_actor_owners.remove(&view);
        if state.active_view == Some(view) {
            state.active_view = None;
        }
        state.pending_by_view.remove(&view);
        let binding = state.detach_view_retiring(view);
        let seed = super::owned_seed::detach_seed_on_view_gone(&mut state, view);
        (binding, seed)
    };
    drop(released);
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
            return then(Err(thrown(
                "The saved presentation close proof does not match",
            )));
        }
        let first_close = self.phase.get() != Phase::Closed;
        fail(
            &self.app,
            self.target.view(),
            &self.identity,
            "The presentation view was closed",
        );
        self.phase.set(Phase::Closed);
        if first_close {
            release_closed_actor(&self.app, self.target.view());
        }
        then(Ok(()));
    }

    fn retire_superseded(&self, proof: NativeBrowserSuperseded, then: Then<()>) {
        if !matches!(self.phase.get(), Phase::Committed | Phase::Superseded)
            || !proof.matches(self.target.reservation(), self.target.browser())
        {
            return then(Err(thrown(
                "The saved presentation supersession proof does not match",
            )));
        }
        fail(
            &self.app,
            self.target.view(),
            &self.identity,
            "The saved presentation browser was replaced",
        );
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
            Phase::Committed => then(Err(thrown(
                "The saved presentation actor lacks its exact close proof",
            ))),
        }
    }
}

pub(super) fn prepare(
    app: &App,
    target: NativeSavedTarget,
) -> Result<Box<dyn PreparedSavedDocument>, IpcError> {
    if target.surface() != Surface::Slides {
        return Err(thrown("The saved document is not a presentation"));
    }
    app.state
        .borrow_mut()
        .saved_loads
        .try_reserve(1)
        .map_err(|_| thrown("The presentation barrier could not be allocated"))?;
    let view = target.view();
    let identity = Arc::new(());
    let mut tokens = target.output().loss_tokens().map_err(thrown)?;
    tokens.extend([
        target.cancel().clone(),
        target.pending().clone(),
        target.created().loss_token(),
    ]);
    tokens.extend(target.additional().iter().cloned());
    for owner in target.participants() {
        tokens.push(
            owner
                .loss_token()
                .ok_or_else(|| thrown("A saved presentation owner is unavailable"))?,
        );
    }
    let mut losses: Vec<_> = tokens
        .into_iter()
        .map(|token| {
            let core = app.services.core.clone();
            let registry = app.registry_index;
            let identity = identity.clone();
            token.on_cancel(move || {
                core.post(Box::new(move || {
                    super::with_app(registry, |app| {
                        fail_before_publication(
                            app,
                            view,
                            &identity,
                            "The saved presentation was released",
                        )
                    });
                }))
            })
        })
        .collect();
    losses
        .try_reserve(1)
        .map_err(|_| thrown("The presentation lifetime could not be allocated"))?;
    Ok(Box::new(Prepared {
        registration: Rc::new(Registration {
            app: app.clone(),
            target: target.clone(),
            identity: identity.clone(),
            phase: Cell::new(Phase::Prepared),
        }),
        slot: Some(SavedLoad {
            target,
            identity,
            initial_document: shell_contract::runtime::RendererGeneration::new(),
            document: None,
            waiter: None,
            done: None,
            started: false,
            applying: None,
            _losses: losses,
        }),
    }))
}

pub(super) fn fail(app: &App, view: ViewId, identity: &Arc<()>, message: &str) {
    let slot = {
        let mut state = app.state.borrow_mut();
        if !state
            .saved_loads
            .get(&view)
            .is_some_and(|slot| Arc::ptr_eq(&slot.identity, identity))
        {
            return;
        }
        state.saved_loads.remove(&view)
    };
    if let Some(slot) = slot {
        finish_detached(slot, message);
    }
}

pub(super) fn finish_detached(mut slot: SavedLoad, message: &str) {
    if slot.applying.is_none() {
        slot.initial_document.revoke();
    }
    if let Some((_, _, waiter)) = slot.waiter.take() {
        waiter.reply(Err(thrown(message)));
    }
    if let Some(done) = slot.done.take() {
        done(Err(thrown(message)));
    }
}

fn fail_before_publication(app: &App, view: ViewId, identity: &Arc<()>, message: &str) {
    let pending = app
        .state
        .borrow()
        .saved_loads
        .get(&view)
        .is_some_and(|slot| Arc::ptr_eq(&slot.identity, identity) && slot.applying.is_none());
    if pending {
        fail(app, view, identity, message);
    }
}

pub(super) fn abandon(app: &App, view: ViewId) {
    let identity = app
        .state
        .borrow()
        .saved_loads
        .get(&view)
        .map(|slot| slot.identity.clone());
    if let Some(identity) = identity {
        fail(app, view, &identity, "The presentation view was closed");
    }
}

pub(super) fn load(
    app: &App,
    document: NativeSavedDocument,
    done: Then<shell_contract::editors::SavedDocumentLoadOutcome>,
) {
    let view = document.target().view();
    let (identity, occupied) = {
        let state = app.state.borrow();
        match state
            .saved_loads
            .get(&view)
            .filter(|slot| slot.target.same_target(document.target()))
        {
            Some(slot) => (
                Some(slot.identity.clone()),
                slot.document.is_some() || slot.done.is_some(),
            ),
            None => (None, false),
        }
    };
    if occupied {
        return done(Err(thrown("The saved presentation is already loading")));
    }
    let effects = match app
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .cloned()
    {
        Some(effects) => effects,
        None => {
            if let Some(identity) = identity {
                fail(
                    app,
                    view,
                    &identity,
                    "The saved presentation effect host is unavailable",
                );
            }
            return done(Err(thrown(
                "The saved presentation effect host is unavailable",
            )));
        }
    };
    let target = document.target();
    let extra = target.additional().iter().collect::<Vec<_>>();
    let mut queued = false;
    let result = effects.with_native_saved_document_input(
        &document,
        &[],
        target.cancel(),
        target.pending(),
        &extra,
        &mut |_| {
            let mut state = app.state.borrow_mut();
            let slot = state
                .saved_loads
                .get_mut(&view)
                .ok_or("The saved presentation barrier is unavailable")?;
            if !slot.target.same_target(target)
                || !document
                    .owner()
                    .context
                    .document_lease()
                    .is_some_and(|lease| lease.same_generation(&slot.initial_document))
                || slot.document.is_some()
                || slot.done.is_some()
            {
                return Err("The saved presentation barrier belongs to another load".into());
            }
            slot.document = Some(document.clone());
            queued = true;
            Ok(())
        },
    );
    if let Err(error) = result {
        if let Some(identity) = identity {
            fail(app, view, &identity, &error);
        }
        return done(Err(thrown(error)));
    }
    if !queued {
        if let Some(identity) = identity {
            fail(
                app,
                view,
                &identity,
                "The saved presentation was not admitted",
            );
        }
        return done(Err(thrown("The saved presentation was not admitted")));
    }
    let mut done = Some(done);
    let installed = {
        let mut state = app.state.borrow_mut();
        if let Some(slot) = state
            .saved_loads
            .get_mut(&view)
            .filter(|slot| slot.target.same_target(target))
        {
            slot.done = done.take();
            true
        } else {
            false
        }
    };
    if !installed {
        if let Some(done) = done {
            done(Err(thrown("The saved presentation barrier was released")));
        }
        return;
    }
    start_if_ready(app, view);
}

pub(super) fn consume(app: &App, cx: &CallContext, fit: f64, responder: Responder) {
    let view = cx.sender.view;
    let (target, identity) = match app
        .state
        .borrow()
        .saved_loads
        .get(&view)
        .map(|slot| (slot.target.clone(), slot.identity.clone()))
    {
        Some(target) => target,
        None => return responder.reply(Err(thrown("The saved presentation barrier was released"))),
    };
    if cx.sender.surface != Surface::Slides
        || !cx
            .native
            .as_ref()
            .and_then(|native| native.renderer())
            .is_some_and(|renderer| renderer.same_generation(target.created()))
    {
        return responder.reply(Err(thrown(
            "The saved presentation request belongs to another renderer",
        )));
    }
    let effects = match app
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .cloned()
    {
        Some(effects) => effects,
        None => {
            return responder.reply(Err(thrown(
                "The saved presentation effect host is unavailable",
            )));
        }
    };
    let mut extra = target.additional().iter().collect::<Vec<_>>();
    extra.push(&cx.cancel);
    let core = app.services.core.clone();
    let registry = app.registry_index;
    let loss_identity = identity.clone();
    let mut loss = Some(cx.cancel.on_cancel(move || {
        core.post(Box::new(move || {
            super::with_app(registry, |app| {
                fail_before_publication(
                    app,
                    view,
                    &loss_identity,
                    "The saved presentation request was released",
                )
            });
        }))
    }));
    let mut responder = Some(responder);
    let result = effects.with_native_saved_target_seed(
        &target,
        &[],
        target.cancel(),
        target.pending(),
        &extra,
        &mut |_| {
            let mut state = app.state.borrow_mut();
            let slot = state
                .saved_loads
                .get_mut(&view)
                .filter(|slot| slot.target.same_target(&target))
                .ok_or("The saved presentation barrier was released")?;
            if slot.waiter.is_some() || slot.started {
                return Err("The saved presentation is already opening".into());
            }
            let loss = loss
                .take()
                .ok_or("The saved presentation lifetime was already consumed")?;
            let responder = responder
                .take()
                .ok_or("The saved presentation request was already consumed")?;
            slot._losses.push(loss);
            slot.waiter = Some((cx.clone(), fit, responder));
            Ok(())
        },
    );
    if let Err(error) = result {
        if let Some(responder) = responder {
            responder.reply(Err(thrown(error)));
        }
        return;
    }
    start_if_ready(app, view);
}

fn start_if_ready(app: &App, view: ViewId) {
    let (document, cx, identity) = {
        let state = app.state.borrow();
        let Some(slot) = state.saved_loads.get(&view) else {
            return;
        };
        if slot.started || slot.done.is_none() {
            return;
        }
        let (Some(document), Some((cx, _, _))) = (&slot.document, &slot.waiter) else {
            return;
        };
        (document.clone(), cx.clone(), slot.identity.clone())
    };
    let actual = match super::session::render_owner(&cx) {
        Ok(owner) => owner,
        Err(error) => return fail(app, view, &identity, &error.message),
    };
    let effects = match app
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .cloned()
    {
        Some(effects) => effects,
        None => {
            return fail(
                app,
                view,
                &identity,
                "The saved presentation effect host is unavailable",
            );
        }
    };
    let target = document.target();
    let extra = target.additional().iter().collect::<Vec<_>>();
    let result = effects.with_native_saved_document_same_origin(
        &document,
        &actual,
        &cx.cancel,
        &[],
        target.cancel(),
        target.pending(),
        &extra,
        &mut |_| {
            let mut state = app.state.borrow_mut();
            let slot = state
                .saved_loads
                .get_mut(&view)
                .filter(|slot| Arc::ptr_eq(&slot.identity, &identity))
                .ok_or("The saved presentation barrier was released")?;
            if slot.started {
                return Err("The saved presentation is already opening".into());
            }
            slot.started = true;
            Ok(())
        },
    );
    if let Err(error) = result {
        return fail(app, view, &identity, &error);
    }
    let engine = app.engine.clone();
    let services = app.services.clone();
    let registry = app.registry_index;
    app.services.workers.spawn(Box::new(move || {
        let encrypted = engine
            .is_encrypted_ooxml(document.target().input().bytes())
            .map_err(|error| error.0);
        super::post_to(&services, registry, move |app| match encrypted {
            Err(error) => fail(app, view, &identity, &error),
            Ok(false) => parse(app, view, identity, document, None),
            Ok(true) => {
                let title = document
                    .target()
                    .path()
                    .path()
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let services = app.services.clone();
                let registry = app.registry_index;
                let saved = document.clone();
                app.shell.unlock_saved_document(
                    document,
                    title,
                    Box::new(move |result| {
                        super::post_to(&services, registry, move |app| match result {
                            Ok(Some((bytes, password))) => {
                                parse(app, view, identity, saved, Some((bytes, password)))
                            }
                            Ok(None) => fail(
                                app,
                                view,
                                &identity,
                                "The presentation password request was cancelled",
                            ),
                            Err(error) => fail(app, view, &identity, &error),
                        })
                    }),
                );
            }
        });
    }));
}

fn parse(
    app: &App,
    view: ViewId,
    identity: Arc<()>,
    document: NativeSavedDocument,
    unlocked: Option<(Vec<u8>, String)>,
) {
    let engine = app.engine.clone();
    let services = app.services.clone();
    let registry = app.registry_index;
    app.services.workers.spawn(Box::new(move || {
        let input = document.target().input();
        let disk = match input.source_commit().metadata() {
            Ok(metadata) => main_core::packages::electron_utils::save_conflict::record_observation(
                &metadata,
                input.bytes(),
            ),
            Err(error) => {
                super::post_to(&services, registry, move |app| {
                    fail(app, view, &identity, &error.to_string())
                });
                return;
            }
        };
        let password = unlocked.as_ref().map(|(_, password)| password.clone());
        let bytes = unlocked
            .as_ref()
            .map_or_else(|| input.bytes(), |(bytes, _)| bytes.as_slice());
        let deck = if bytes.is_empty() {
            engine
                .create_blank_pptx()
                .and_then(|bytes| engine.open(&bytes))
        } else {
            engine.open(bytes)
        };
        let deck = deck.map(|deck| super::state::DeckHandle::new(deck, engine.clone()));
        super::post_to(&services, registry, move |app| match deck {
            Ok(deck) => publish(app, view, identity, document, deck, password, disk),
            Err(error) => fail(app, view, &identity, &error.0),
        });
    }));
}

fn publish(
    app: &App,
    view: ViewId,
    identity: Arc<()>,
    document: NativeSavedDocument,
    deck: super::state::DeckHandle,
    password: Option<String>,
    disk: main_core::packages::electron_utils::save_conflict::SaveDiskState,
) {
    let waiting = {
        let state = app.state.borrow();
        state
            .saved_loads
            .get(&view)
            .filter(|slot| Arc::ptr_eq(&slot.identity, &identity))
            .and_then(|slot| slot.waiter.as_ref().map(|(cx, fit, _)| (cx.clone(), *fit)))
    };
    let Some((cx, fit)) = waiting else {
        return fail(
            app,
            view,
            &identity,
            "The saved presentation renderer was released",
        );
    };
    let result = (|| -> Result<(shell_contract::IpcValue, super::engine::RenderOwner, u64, String), IpcError> {
        let owner = super::session::render_owner(&cx)?;
        let effects = app.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned().ok_or_else(|| thrown("The saved presentation effect host is unavailable"))?;
        let target = document.target(); let extra = target.additional().iter().collect::<Vec<_>>();
        effects.with_native_saved_document_same_origin(&document, &owner, &cx.cancel, &[], target.cancel(), target.pending(), &extra, &mut |_| Ok(())).map_err(thrown)?;
        let authority = if app.state.borrow().has_session(view) {
            super::open_authority::OpenAuthority::capture(app, &owner, cx.cancel.clone())?
        } else {
            super::open_authority::OpenAuthority { source: document.owner().clone(), cancel: cx.cancel.clone() }
        };
        let path = target.path().path().to_string_lossy().into_owned();
        let mut session = super::state::SlideSession::new(&path, deck, fit);
        session.password = password;
        session.disk_state = Some(disk);
        session.core.schedule_history_notify();
        let session_id = session.id;
        let staged = super::state::StagedBinding::new(app.engine.clone(), session.deck(), &authority.source).map_err(|error| thrown(error.0))?;
        app.engine.adopt_embedded_fonts(session.deck(), &authority.source).map_err(|error| thrown(error.0))?;
        let slides = app.render_all(&session, &authority.source)?;
        let mut result = app.open_result(&session, slides)?;
        let token = app.shell.random_uuid();
        let shell_contract::IpcValue::Object(fields) = &mut result else { return Err(thrown("The presentation open result is invalid")); };
        fields.push(("recoveryToken".into(), shell_contract::IpcValue::String(token.clone())));
        let owner = authority.commit_saved(app, &owner, &document, session, staged)?;
        Ok((result, owner, session_id, token))
    })();
    let (result, owner, session, token) = match result {
        Ok(result) => result,
        Err(error) => return fail(app, view, &identity, &error.message),
    };
    let lease = owner.context.document_lease().cloned();
    let Some(lease) = lease else {
        return fail(
            app,
            view,
            &identity,
            "The presentation document was released",
        );
    };
    let core = app.services.core.clone();
    let registry = app.registry_index;
    let loss_identity = identity.clone();
    let loss = lease.loss_token().on_cancel(move || {
        core.post(Box::new(move || {
            super::with_app(registry, |app| {
                fail(
                    app,
                    view,
                    &loss_identity,
                    "The presentation document was replaced",
                )
            });
        }))
    });
    let waiter = {
        let mut state = app.state.borrow_mut();
        let Some(slot) = state
            .saved_loads
            .get_mut(&view)
            .filter(|slot| Arc::ptr_eq(&slot.identity, &identity))
        else {
            return;
        };
        slot.applying = Some(Applying {
            token,
            session,
            document: lease,
            owner,
            _loss: loss,
        });
        slot._losses.clear();
        slot.waiter.take().map(|(_, _, waiter)| waiter)
    };
    if let Some(waiter) = waiter {
        waiter.reply(Ok(result));
    } else {
        fail(
            app,
            view,
            &identity,
            "The saved presentation renderer was released",
        );
    }
}

pub(super) fn initialized(
    app: &App,
    cx: &CallContext,
    args: &crate::value::Args,
) -> Option<Result<shell_contract::IpcValue, IpcError>> {
    let token = args.str(0)?;
    let (identity, owner, document, session) = {
        let state = app.state.borrow();
        let slot = state.saved_loads.get(&cx.sender.view)?;
        let applying = slot.applying.as_ref()?;
        if applying.token != token {
            return None;
        }
        (
            slot.identity.clone(),
            applying.owner.clone(),
            applying.document.clone(),
            applying.session,
        )
    };
    let actual = match super::session::render_owner(cx) {
        Ok(owner) => owner,
        Err(error) => return Some(Err(error)),
    };
    let effects = match app
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .cloned()
    {
        Some(effects) => effects,
        None => return Some(Err(thrown("The presentation effect host is unavailable"))),
    };
    let mut completed = None;
    let result = effects
        .with_same_native_origin_effect(
            &owner,
            &actual,
            &[],
            &cx.cancel,
            &cx.cancel,
            &[],
            &mut || {
                let mut state = app.state.borrow_mut();
                let current = state
                    .saved_loads
                    .get(&cx.sender.view)
                    .filter(|slot| Arc::ptr_eq(&slot.identity, &identity))
                    .and_then(|slot| slot.applying.as_ref())
                    .ok_or("The saved presentation acknowledgement was released")?;
                if current.token != token
                    || !state
                        .session(cx.sender.view)
                        .is_some_and(|value| value.id == session)
                    || !state
                        .save_lifetime(cx.sender.view)
                        .is_some_and(|value| value.same_generation(&document))
                {
                    return Err(
                        "The saved presentation acknowledgement belongs to another document".into(),
                    );
                }
                completed = state.saved_loads.remove(&cx.sender.view);
                Ok(())
            },
        )
        .map_err(thrown);
    if let Err(error) = result {
        return Some(Err(error));
    }
    let slot = completed;
    let ok = matches!(args.get(1), shell_contract::IpcValue::Bool(true));
    if let Some(mut slot) = slot
        && let Some(done) = slot.done.take()
    {
        done(if ok {
            Ok(shell_contract::editors::SavedDocumentLoadOutcome::Applied)
        } else {
            Err(thrown(
                args.str(2)
                    .unwrap_or("The presentation could not be initialized"),
            ))
        });
    }
    Some(Ok(shell_contract::IpcValue::Null))
}
