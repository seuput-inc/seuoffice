//! The renderer round trips: the close guard (`docs:close-check` ->
//! `docs:close-check-result`, `docs:close-save-request` ->
//! `docs:close-save-result`, `requestDocsClose`), the session-end copies
//! (`docs:write-recovery-now` -> `docs:write-recovery-now-done`), the
//! View-menu state, and the teardown and rename the shell sends.
//!
//! The TypeScript awaited each reply with a timer racing it. Here the
//! continuation waits in [`Close`] keyed as the TypeScript keyed it, the
//! event goes out, and the reply's handler (or the timer) takes and runs it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use main_core::shell::renderer_disposal::Failure;
use shell_contract::hosts::MessageBox;
use shell_contract::ipc::Target;
use shell_contract::{IpcValue, ViewId, WindowId};

use super::Docs;
use crate::kit::{self, Entry, Timer};

/// `queryCloseState`'s 2 s, after which a silent renderer counts as dirty
const CLOSE_CHECK_TIMEOUT: Duration = Duration::from_millis(2000);
/// `requestRendererSave`'s 120 s
const CLOSE_SAVE_TIMEOUT: Duration = Duration::from_millis(120_000);
/// `writeDocsCopiesNow`'s 5 s per renderer
const RECOVERY_NOW_TIMEOUT: Duration = Duration::from_millis(5000);

/// `ODT_OFF_MENU_IDS`
const ODT_OFF_MENU_IDS: [&str; 2] = ["docs-menu-insert-equation", "docs-menu-insert-comment"];

/// `DocsCloseState`
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CloseState {
    pub dirty: bool,
    pub auto_save: bool,
    pub file_path: Option<String>,
    pub unresponsive: bool,
}

type StateThen = Box<dyn FnOnce(CloseState)>;
type BoolThen = Box<dyn FnOnce(bool)>;
/// a write-copies-now token's timer and what its answer settles
type TokenWait = (ViewId, shell_contract::runtime::RendererGeneration, Timer, Box<dyn FnOnce(Result<(), Failure>)>);

pub(super) struct DetachedRecoveryWaiters(Vec<TokenWait>);

impl DetachedRecoveryWaiters {
    pub(super) fn fail(self, reason: &str) {
        for (_, _, timer, done) in self.0 {
            kit::clear(timer);
            done(Err(Failure(reason.to_owned())));
        }
    }
}

/// A close check in flight: every caller that asked meanwhile shares it
struct Check {
    timer: Timer,
    waiters: Vec<StateThen>,
}

struct OwnedClose {
    scope: super::owned_save::Scope,
    subscriptions: Vec<shell_contract::runtime::CancelSubscription>,
    then: shell_contract::editors::Then<bool>,
}

/// The waiters of the renderer round trips
#[derive(Default)]
pub(crate) struct Close {
    pub(super) owned_queries: super::dirty_owned::Queries,
    /// `closeCheckWaiters` with `closeStateQueries`
    checks: RefCell<HashMap<ViewId, Check>>,
    /// `closeSaveWaiters`: which request the view's reply answers
    save_routes: RefCell<HashMap<ViewId, u64>>,
    /// each save request's own continuation and timer
    pub(super) saves: RefCell<HashMap<u64, super::owned_save::SaveWait>>,
    /// `docsCloseRequests`: callers joining a close in progress
    closes: RefCell<HashMap<ViewId, Vec<BoolThen>>>,
    owned_closes: RefCell<HashMap<u64, OwnedClose>>,
    /// `recoveryNowWaiters`, by token
    recovery_now: RefCell<HashMap<String, TokenWait>>,
}

impl Close {
    /// `recoveryNowWaiters.get(token)?.()`
    pub fn recovery_now_done(&self, view: ViewId, generation: &shell_contract::runtime::RendererGeneration, token: &str, result: Result<(), Failure>) {
        let waiter = {
            let mut waiters = self.recovery_now.borrow_mut();
            if waiters.get(token).is_some_and(|(owner, expected, _, _)| *owner == view && expected.is_active() && expected.same_generation(generation)) {
                waiters.remove(token)
            } else {
                None
            }
        };
        if let Some((_, _, timer, done)) = waiter {
            kit::clear(timer);
            done(result);
        }
    }

    pub(super) fn take_recovery_view(&self, view: ViewId) -> DetachedRecoveryWaiters {
        let mut pending = self.recovery_now.borrow_mut();
        let tokens: Vec<_> = pending.iter().filter(|(_, (owner, _, _, _))| *owner == view)
            .map(|(token, _)| token.clone()).collect();
        DetachedRecoveryWaiters(tokens.into_iter().filter_map(|token| pending.remove(&token)).collect())
    }

    pub(super) fn fail_recovery_view(&self, view: ViewId, reason: &str) {
        self.take_recovery_view(view).fail(reason);
    }

    pub(super) fn renderer_replaced(&self, view: ViewId, old: &shell_contract::runtime::RendererGeneration) {
        self.owned_queries.fail_generation(view, old);
        let saves = self.saves.borrow().iter().filter_map(|(id, waiter)| {
            waiter.scope.as_ref().filter(|scope| scope.origin().into_iter().chain(scope.participants().iter()).any(|owner| {
                owner.view == view && !scope.retires_expected_origin(owner)
                    && owner.context.renderer().is_some_and(|generation| generation.same_generation(old))
            })).map(|_| *id)
        }).collect::<Vec<_>>();
        for id in saves { self.finish_save(id, Err(kit::error("the document save renderer was replaced"))); }
        let waiters = {
            let mut pending = self.recovery_now.borrow_mut();
            let tokens: Vec<_> = pending.iter().filter(|(_, (owner, generation, _, _))| *owner == view && generation.same_generation(old))
                .map(|(token, _)| token.clone()).collect();
            tokens.into_iter().filter_map(|token| pending.remove(&token)).collect::<Vec<_>>()
        };
        for (_, _, timer, done) in waiters {
            kit::clear(timer);
            done(Err(Failure("Docs recovery renderer was replaced".into())));
        }
    }

    fn answer_save(&self, view: ViewId, ok: bool) {
        let Some(id) = self.save_routes.borrow_mut().remove(&view) else {
            return;
        };
        self.finish_save(id, Ok(ok));
    }

    pub(super) fn finish_save(&self, id: u64, result: Result<bool, shell_contract::IpcError>) {
        if let Some(waiter) = self.take_save(id) { Self::complete_save(waiter, result); }
    }

    pub(super) fn take_save(&self, id: u64) -> Option<super::owned_save::SaveWait> {
        let waiter = self.saves.borrow_mut().remove(&id)?;
        let mut routes = self.save_routes.borrow_mut();
        if routes.get(&waiter.view) == Some(&id) { routes.remove(&waiter.view); }
        Some(waiter)
    }

    pub(super) fn complete_save(waiter: super::owned_save::SaveWait, result: Result<bool, shell_contract::IpcError>) {
        kit::clear(waiter.timer);
        if let Some(scope) = &waiter.scope { scope.stop_after_save(); }
        (waiter.then)(result);
    }

    fn fail_owned_save_view(&self, view: ViewId) {
        let ids = self.saves.borrow().iter().filter(|(_, waiter)| {
            waiter.scope.as_ref().is_some_and(|scope| scope.origin().into_iter().chain(scope.participants().iter())
                .any(|owner| owner.view == view && !scope.retires_expected_origin(owner)))
        }).map(|(id, _)| *id).collect::<Vec<_>>();
        for id in ids { self.finish_save(id, Err(kit::error("the document save view was destroyed"))); }
    }
}

impl Docs {
    fn finish_owned_close(&self, id: u64, result: Result<bool, shell_contract::IpcError>) {
        let waiter = self.close.owned_closes.borrow_mut().remove(&id);
        if let Some(waiter) = waiter {
            waiter.scope.stop();
            drop(waiter.subscriptions);
            (waiter.then)(result);
        }
    }

    pub(crate) fn request_close_owned(
        self: &Rc<Self>, view: ViewId, parent: Option<WindowId>,
        origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: shell_contract::editors::Then<bool>,
    ) {
        let scope = match super::owned_save::capture_close(self, view, parent, origin, cancel) {
            Ok(scope) => scope,
            Err(error) => return then(Err(error)),
        };
        self.start_owned_close(scope, None, then);
    }

    pub(crate) fn request_close_captured_many(
        self: &Rc<Self>, view: ViewId, parent: Option<WindowId>,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: shell_contract::editors::Then<bool>,
    ) {
        let scope = match super::owned_save::capture_close_many(self, view, parent, origin, participants, destination, cancel) {
            Ok(scope) => scope,
            Err(error) => return then(Err(error)),
        };
        self.start_owned_close(scope, Some(pending), then);
    }

    fn start_owned_close(
        self: &Rc<Self>, scope: super::owned_save::Scope,
        parent_pending: Option<shell_contract::runtime::CancelToken>,
        then: shell_contract::editors::Then<bool>,
    ) {
        let Some(origin) = scope.origin().cloned() else {
            return then(Err(kit::error("The document close requires a renderer owner")));
        };
        let id = kit::unique();
        self.close.owned_closes.borrow_mut().insert(id, OwnedClose {
            scope: scope.clone(), subscriptions: Vec::new(), then,
        });
        let mut subscriptions = Vec::new();
        let mut losses = vec![(scope.cancel().clone(), None), (scope.pending().clone(), None)];
        if let Some(pending) = parent_pending { losses.push((pending, None)); }
        for owner in std::iter::once(&origin).chain(scope.participants().iter()) {
            let Some(loss) = owner.loss_token() else {
                return self.finish_owned_close(id, Err(kit::error("The document close owner has no lease")));
            };
            let Some(renderer) = owner.context.renderer() else {
                return self.finish_owned_close(id, Err(kit::error("The document close owner has no renderer")));
            };
            losses.push((loss, Some(owner.clone())));
            losses.push((renderer.loss_token(), Some(owner.clone())));
        }
        for (loss, owner) in losses {
            let pending = scope.pending().clone();
            let observed_scope = scope.clone();
            let is_pending = loss.same_token(scope.pending());
            let is_request_cancel = loss.same_token(scope.cancel());
            let weak = Rc::downgrade(self);
            let callback = kit::back_on_core(&self.services, move |_: ()| {
                if let Some(docs) = weak.upgrade() {
                    docs.finish_owned_close(id, Err(kit::error("The document close owner was released")));
                }
            });
            subscriptions.push(loss.on_cancel(move || {
                if owner.as_ref().is_some_and(|owner| observed_scope.retires_expected_origin(owner)) { return; }
                if is_request_cancel && observed_scope.expected_dispatcher_cancel() { return; }
                if !is_pending { pending.cancel(); }
                callback(());
            }));
        }
        if let Some(waiter) = self.close.owned_closes.borrow_mut().get_mut(&id) {
            waiter.subscriptions = subscriptions;
        } else { return; }
        if let Err(error) = scope.check() {
            return self.finish_owned_close(id, Err(kit::error(error)));
        }
        let docs = Rc::clone(self);
        super::dirty_owned::start_captured(
            self, origin, scope.participants()[..scope.participants().len() - 1].to_vec(), scope.destination().clone(),
            scope.cancel().clone(), scope.pending().clone(), true,
            Box::new(move |state| match state {
                Ok(state) => docs.perform_owned_close(id, state),
                Err(error) => docs.finish_owned_close(id, Err(error)),
            }),
        );
    }

    fn owned_close_scope(&self, id: u64) -> Option<super::owned_save::Scope> {
        self.close.owned_closes.borrow().get(&id).map(|waiter| waiter.scope.clone())
    }

    fn perform_owned_close(self: &Rc<Self>, id: u64, state: CloseState) {
        let Some(scope) = self.owned_close_scope(id) else { return };
        let view = scope.destination().view;
        if let Err(error) = scope.effect(|| self.book.borrow_mut().note_close_check(view, state.dirty)) {
            return self.finish_owned_close(id, Err(kit::error(error)));
        }
        if !state.dirty { return self.finish_owned_close(id, Ok(true)); }
        if state.unresponsive {
            let options = MessageBox {
                kind: Some("warning".into()), message: self.tm("closeNoReplyMsg"),
                detail: Some(self.tm("closeNoReplyDetail")),
                buttons: vec![self.tm("btnCloseAnyway"), self.tm("btnCancel")],
                default_id: Some(1), cancel_id: Some(1), ..Default::default()
            };
            let docs = Rc::clone(self);
            super::owned_save::message(self, scope, options, Box::new(move |result| match result {
                Ok(0) => docs.owned_close_recovery(id, state.file_path, true),
                Ok(_) => docs.finish_owned_close(id, Ok(false)),
                Err(error) => docs.finish_owned_close(id, Err(kit::error(error))),
            }));
            return;
        }
        if state.auto_save {
            let docs = Rc::clone(self);
            return super::owned_save::start_with_captured_scope(
                self, scope, shell_contract::editors::SaveMode::Save,
                Box::new(move |result| match result {
                    Ok(true) => docs.owned_close_saved(id),
                    Ok(false) => docs.owned_close_prompt(id, state),
                    Err(error) => docs.finish_owned_close(id, Err(error)),
                }),
            );
        }
        self.owned_close_prompt(id, state);
    }

    fn owned_close_prompt(self: &Rc<Self>, id: u64, state: CloseState) {
        let Some(scope) = self.owned_close_scope(id) else { return };
        let options = MessageBox {
            kind: Some("warning".into()), message: self.tm("closeUnsavedMsg"),
            detail: Some(self.tm("closeUnsavedDetail")),
            buttons: vec![self.tm("menuSave"), self.tm("btnDontSave"), self.tm("btnCancel")],
            default_id: Some(0), cancel_id: Some(2), ..Default::default()
        };
        let docs = Rc::clone(self);
        let save_scope = scope.clone();
        super::owned_save::message(self, scope, options, Box::new(move |result| match result {
            Ok(2) => docs.finish_owned_close(id, Ok(false)),
            Ok(1) => docs.owned_close_recovery(id, state.file_path, false),
            Ok(_) => {
                let after = Rc::clone(&docs);
                super::owned_save::start_with_captured_scope(
                    &docs, save_scope, shell_contract::editors::SaveMode::Save,
                    Box::new(move |result| match result {
                        Ok(true) => after.owned_close_saved(id),
                        Ok(false) => after.finish_owned_close(id, Ok(false)),
                        Err(error) => after.finish_owned_close(id, Err(error)),
                    }),
                );
            }
            Err(error) => docs.finish_owned_close(id, Err(kit::error(error))),
        }));
    }

    fn owned_close_saved(&self, id: u64) {
        let Some(scope) = self.owned_close_scope(id) else { return };
        if scope.has_generated_receipt() {
            let mut waiter = None;
            let result = scope.with_generated_terminal_commit(&mut || {
                waiter = self.close.owned_closes.borrow_mut().remove(&id);
                if waiter.is_none() { return Err("The document close is no longer pending".into()); }
                self.book.borrow_mut().note_closed_saved(scope.destination().view);
                Ok(())
            });
            match (result, waiter) {
                (Ok(()), Some(waiter)) => {
                    waiter.scope.stop();
                    drop(waiter.subscriptions);
                    (waiter.then)(Ok(true));
                }
                (Err(error), _) => self.finish_owned_close(id, Err(kit::error(error))),
                _ => self.finish_owned_close(id, Err(kit::error("The document close was not admitted"))),
            }
            return;
        }
        let result = scope.effect(|| self.book.borrow_mut().note_closed_saved(scope.destination().view))
            .map(|()| true).map_err(kit::error);
        self.finish_owned_close(id, result);
    }
    fn owned_close_recovery(self: &Rc<Self>, id: u64, path: Option<String>, abandon: bool) {
        let Some(scope) = self.owned_close_scope(id) else { return };
        let view = scope.destination().view;
        let path = if abandon {
            match scope.effect(|| self.open_doc.get(view)) {
                Ok(path) => path,
                Err(error) => return self.finish_owned_close(id, Err(kit::error(error))),
            }
        } else { path };
        let identities = match scope.effect(|| self.book.borrow_mut().begin_owned_close(view, path.as_deref(), abandon)) {
            Ok(identities) => identities,
            Err(error) => return self.finish_owned_close(id, Err(kit::error(error))),
        };
        let shared = self.recovery_shared.clone();
        let directory = self.user_data(&[]);
        let shell = self.shell.clone();
        let worker_scope = scope.clone();
        let completed = Rc::clone(self);
        kit::on_worker(&self.services, move || -> Result<(), String> {
            use main_core::packages::electron_utils::recovery_store::{
                RecoveryCopyState, RecoveryDeleteReason, RecoveryStoreOptions, create_recovery_store,
            };
            for identity in identities {
                if abandon {
                    let plan = super::recovery::with_store(&shared, &directory, &shell, |store| {
                        store.prepare_copy_state_update(&identity, RecoveryCopyState::Later)
                    }).map_err(|error| error.message)?;
                    if let Some(plan) = plan {
                        plan.write_authorized(&mut |effect| worker_scope.admit_file_effect(effect))
                            .map_err(|error| error.message)?;
                    }
                } else {
                    let plan = loop {
                        let attempt = worker_scope.effect(|| {
                            let guard = match shared.try_lock() {
                                Ok(guard) => guard,
                                Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                                Err(std::sync::TryLockError::WouldBlock) => return None,
                            };
                            let dir = || directory.clone();
                            let now = || shell.now_ms();
                            let uuid = || shell.random_uuid();
                            let store = create_recovery_store(RecoveryStoreOptions {
                                user_data_dir: &dir, now_ms: &now, random_uuid: &uuid,
                            }, &guard);
                            Some(store.prepare_delete(&identity, RecoveryDeleteReason::DontSave))
                        })?;
                        match attempt {
                            Some(result) => break result.map_err(|error| error.message)?,
                            None => std::thread::yield_now(),
                        }
                    };
                    if let Some(plan) = plan {
                        plan.remove_authorized(&mut |effect| worker_scope.admit_file_effect(effect))
                            .map_err(|error| error.message)?;
                    }
                }
            }
            Ok(())
        }, move |result| {
            let Some(scope) = completed.owned_close_scope(id) else { return };
            let result = result.and_then(|()| {
                if !abandon {
                    let origin = scope.origin().ok_or_else(|| "The document close requires a renderer owner".to_owned())?;
                    scope.effects().emit_owned_many(
                        origin, scope.participants(), scope.participants().len() - 1, scope.cancel(), scope.pending(),
                        "docs:recovery-copy-dropped", Vec::new(),
                    )?;
                }
                scope.check()
            }).map(|()| true).map_err(kit::error);
            completed.finish_owned_close(id, result);
        });
    }


    fn emit(&self, view: ViewId, channel: &'static str, args: Vec<IpcValue>) {
        self.services.events.emit(Target::View(view), channel, args)
    }

    /// `queryCloseState(contents)`
    pub(crate) fn query_close_state(self: &Rc<Self>, view: ViewId, then: StateThen) {
        if !self.alive(view) {
            return then(CloseState::default());
        }
        if let Some(check) = self.close.checks.borrow_mut().get_mut(&view) {
            check.waiters.push(then);
            return;
        }
        let docs = Rc::clone(self);
        let timer = kit::after(&self.services, CLOSE_CHECK_TIMEOUT, move || {
            let check = docs.close.checks.borrow_mut().remove(&view);
            // no reply: a busy or wedged renderer may hold unsaved changes
            let state = CloseState {
                dirty: true,
                unresponsive: true,
                ..Default::default()
            };
            for waiter in check.map(|c| c.waiters).unwrap_or_default() {
                waiter(state.clone())
            }
        });
        self.close.checks.borrow_mut().insert(
            view,
            Check {
                timer,
                waiters: vec![then],
            },
        );
        self.emit(view, "docs:close-check", Vec::new());
    }

    /// `requestRendererSave(contents)`: failure and timeout are `false`
    pub(crate) fn request_renderer_save(self: &Rc<Self>, view: ViewId, then: BoolThen) {
        let id = kit::unique();
        let docs = Rc::clone(self);
        let timer = kit::after(&self.services, CLOSE_SAVE_TIMEOUT, move || {
            docs.close.finish_save(id, Ok(false));
        });
        self.close.save_routes.borrow_mut().insert(view, id);
        self.close.saves.borrow_mut().insert(id, super::owned_save::SaveWait {
            view, timer, then: Box::new(move |result| then(result.unwrap_or(false))), scope: None, loss: None,
        });
        self.emit(view, "docs:close-save-request", Vec::new());
    }

    /// `requestDocsClose(contents, parent)`: `true` means the view may
    /// close; a second call while one runs joins it
    pub(crate) fn request_close(
        self: &Rc<Self>,
        view: ViewId,
        parent: Option<WindowId>,
        then: BoolThen,
    ) {
        if let Some(joined) = self.close.closes.borrow_mut().get_mut(&view) {
            joined.push(then);
            return;
        }
        self.close.closes.borrow_mut().insert(view, vec![then]);
        let docs = Rc::clone(self);
        let done: BoolThen = Box::new(move |proceed| {
            let waiters = docs
                .close
                .closes
                .borrow_mut()
                .remove(&view)
                .unwrap_or_default();
            for w in waiters {
                w(proceed)
            }
        });
        let docs = Rc::clone(self);
        self.query_close_state(
            view,
            Box::new(move |state| docs.perform_close(view, parent, state, done)),
        );
    }

    /// `performDocsClose` once the renderer answered the close check
    fn perform_close(
        self: &Rc<Self>,
        view: ViewId,
        parent: Option<WindowId>,
        state: CloseState,
        done: BoolThen,
    ) {
        if !state.dirty || !self.alive(view) {
            return done(true);
        }
        if state.unresponsive {
            let response = self.message(
                parent,
                MessageBox {
                    kind: Some("warning".into()),
                    message: self.tm("closeNoReplyMsg"),
                    detail: Some(self.tm("closeNoReplyDetail")),
                    buttons: vec![self.tm("btnCloseAnyway"), self.tm("btnCancel")],
                    default_id: Some(1),
                    cancel_id: Some(1),
                    ..Default::default()
                },
            );
            if response != 0 {
                return done(false);
            }
            // a hung renderer cannot save: its last copy is the only record, kept for later
            let open = self.open_doc.get(view);
            self.with_store(|store| self.book.borrow_mut().abandon(store, view, open.as_deref()));
            return done(true);
        }
        if state.auto_save {
            let docs = Rc::clone(self);
            return self.request_renderer_save(
                view,
                Box::new(move |ok| {
                    if ok {
                        docs.book.borrow_mut().note_closed_saved(view);
                        done(true)
                    } else {
                        docs.close_prompt(view, parent, state, done)
                    }
                }),
            );
        }
        self.close_prompt(view, parent, state, done)
    }

    /// Save / Don't Save / Cancel
    fn close_prompt(
        self: &Rc<Self>,
        view: ViewId,
        parent: Option<WindowId>,
        state: CloseState,
        done: BoolThen,
    ) {
        let response = self.message(
            parent,
            MessageBox {
                kind: Some("warning".into()),
                message: self.tm("closeUnsavedMsg"),
                detail: Some(self.tm("closeUnsavedDetail")),
                buttons: vec![
                    self.tm("menuSave"),
                    self.tm("btnDontSave"),
                    self.tm("btnCancel"),
                ],
                default_id: Some(0),
                cancel_id: Some(2),
                ..Default::default()
            },
        );
        match response {
            2 => done(false),
            1 => {
                // explicitly discarded: nothing offers the dropped edits back
                self.discard_tab_copies(view, state.file_path.as_deref());
                done(true)
            }
            _ => {
                let docs = Rc::clone(self);
                self.request_renderer_save(
                    view,
                    Box::new(move |saved| {
                        if saved {
                            docs.book.borrow_mut().note_closed_saved(view);
                        }
                        done(saved)
                    }),
                );
            }
        }
    }

    /// Ask every booted renderer for a recovery copy and wait for its reply.
    pub(crate) fn write_copies_now(self: &Rc<Self>, then: Box<dyn FnOnce(Result<(), Failure>)>) {
        let targets: Vec<ViewId> = self.booted.borrow().iter().copied()
            .filter(|v| !self.is_torn_down(*v)).collect();
        if targets.is_empty() {
            return then(Ok(()));
        }
        let left = Rc::new(Cell::new(targets.len()));
        let error = Rc::new(RefCell::new(None::<Failure>));
        let then = Rc::new(RefCell::new(Some(then)));
        for view in targets {
            self.close.fail_recovery_view(view, "Docs recovery request was replaced");
            let (left, error, then) = (left.clone(), error.clone(), then.clone());
            let one_done = move |result: Result<(), Failure>| {
                if let Err(failure) = result {
                    if error.borrow().is_none() { *error.borrow_mut() = Some(failure); }
                }
                left.set(left.get() - 1);
                if left.get() == 0 {
                    let result = error.borrow_mut().take().map_or(Ok(()), Err);
                    let done = then.borrow_mut().take();
                    if let Some(done) = done { done(result); }
                }
            };
            if !self.alive(view) {
                one_done(Err(Failure("Docs recovery view destroyed".into())));
                continue;
            }
            let Some(generation) = self.services.views.renderer_generation(view).filter(|generation| generation.is_active()) else {
                one_done(Err(Failure("Docs recovery renderer generation unavailable".into())));
                continue;
            };
            let token = kit::unique().to_string();
            let docs = Rc::clone(self);
            let key = token.clone();
            let timer = kit::after(&self.services, RECOVERY_NOW_TIMEOUT, move || {
                let waiter = docs.close.recovery_now.borrow_mut().remove(&key);
                if let Some((_, _, _, done)) = waiter {
                    done(Err(Failure("Docs recovery flush timed out".into())));
                }
            });
            let owner = shell_contract::ipc::NativeRequestOwner { view, context: shell_contract::ipc::NativeContext::with_renderer((), generation.clone()) };
            self.close.recovery_now.borrow_mut().insert(token.clone(), (view, generation, timer, Box::new(one_done)));
            self.book.borrow_mut().note_still_open(view);
            if let Err(error) = self.services.events.emit_to_owner_checked(&owner, "docs:write-recovery-now", vec![IpcValue::String(token.clone())]) {
                let waiter = self.close.recovery_now.borrow_mut().remove(&token);
                if let Some((_, _, timer, done)) = waiter {
                    kit::clear(timer);
                    done(Err(Failure(error)));
                }
            }
        }
    }

    /// `teardownDocsRenderer(contents)`: the closed tab loses its writes and
    /// its recovery copies settle by how the close was decided
    pub(crate) fn teardown(&self, view: ViewId, crashed: bool) {
        self.fail_recovery_seed(view, "the Docs recovery view was closed");
        self.close.fail_recovery_view(view, "Docs recovery view destroyed");
        self.torn_down.borrow_mut().insert(view);
        self.revoke_media_view(view);
        self.with_store(|store| self.book.borrow_mut().settle_closed(store, view, crashed));
        self.doc_writable.remove(view);
        self.pdf_writable.remove(view);
        self.disk_states.remove(view);
        self.passwords.borrow_mut().forget(view);
        if self.alive(view) {
            self.emit(view, "docs:teardown", Vec::new());
        }
    }

    pub(super) fn rename_document_state(&self, view: ViewId, old: &str, new: &str) -> Result<(), String> {
        if let Some(mut set) = self.doc_writable.get(view) {
            set.remove(old);
            self.doc_writable.insert(view, set);
        }
        self.allow_doc_write(view, new);
        self.remember_open_doc(view, new);
        if let Some(mut states) = self.disk_states.get(view)
            && let Some(recorded) = states.remove(old)
        {
            states.insert(new.to_owned(), recorded);
            self.disk_states.insert(view, states);
        }
        // the password follows the path, or the next save writes plaintext
        self.passwords.borrow_mut().rename(view, old, new);
        self.engine.move_lazy_media_source(old, new)
    }

    /// `docsFileRenamed(wc, oldPath, newPath)`
    pub(crate) fn file_renamed(&self, view: ViewId, old: &str, new: &str) {
        let media_result = self.rename_document_state(view, old, new);
        self.emit(
            view,
            "docs:renamed",
            vec![
                super::Obj::new()
                    .set("oldPath", old)
                    .set("newPath", new)
                    .done(),
            ],
        );
        if let Err(error) = media_result {
            // No safe save is possible with media still pointing at the old path.
            // Keep the renderer and existing recovery copy intact for the user.
            self.torn_down.borrow_mut().insert(view);
            self.doc_writable.remove(view);
            self.pdf_writable.remove(view);
            self.dialogs.error(
                &self.tm("dlgOpenDoc"),
                &format!("Document saving was disabled because the media service failed: {error}"),
            );
        }
    }
}

/// The `destroyed` listeners: a close check answers clean, a save `false`
pub(super) fn view_destroyed(docs: &Docs, view: ViewId) {
    docs.close.owned_queries.fail_view(view);
    docs.close.fail_owned_save_view(view);
    docs.close.fail_recovery_view(view, "Docs recovery view destroyed");
    let check = docs.close.checks.borrow_mut().remove(&view);
    if let Some(check) = check {
        kit::clear(check.timer);
        for waiter in check.waiters {
            waiter(CloseState::default())
        }
    }
    docs.close.answer_save(view, false);
}

pub(super) fn entries(docs: &Rc<Docs>) -> Vec<Entry> {
    let mut all = Vec::new();

    let d = docs.clone();
    all.push(kit::invoke_later(
        "docs:confirm-document-replace",
        move |cx, _args, responder| {
            let view = cx.sender.view;
            let d2 = d.clone();
            d.request_close(
                view,
                d.parent(view),
                Box::new(move |proceed| {
                    // another document takes this tab: the leaving one's copies settle
                    if proceed {
                        d2.with_store(|store| {
                            d2.book.borrow_mut().settle_closed(store, view, false)
                        });
                    }
                    responder.reply(Ok(IpcValue::Bool(proceed)))
                }),
            );
        },
    ));

    let d = docs.clone();
    all.push(kit::send("docs:close-check-result", move |cx, args| {
        let view = cx.sender.view;
        if let Some(token) = kit::as_str(args.get(1)) {
            super::dirty_owned::answer(&d, view, cx.native.as_ref().and_then(|context| context.renderer()), token, args.get(0));
            return Ok(());
        }
        let check = d.close.checks.borrow_mut().remove(&view);
        let Some(check) = check else {
            return Ok(());
        };
        kit::clear(check.timer);
        let s = args.get(0);
        let state = match s {
            IpcValue::Bool(dirty) => CloseState {
                dirty: *dirty,
                ..Default::default()
            },
            other => CloseState {
                dirty: kit::field(other, "dirty") == &IpcValue::Bool(true),
                auto_save: kit::field(other, "autoSave") == &IpcValue::Bool(true),
                file_path: kit::as_str(kit::field(other, "filePath")).map(str::to_owned),
                unresponsive: false,
            },
        };
        // a clean answer lets this tab's close drop the copies it wrote
        d.book.borrow_mut().note_close_check(view, state.dirty);
        for waiter in check.waiters {
            waiter(state.clone())
        }
        Ok(())
    }));

    let d = docs.clone();
    all.push(kit::send("docs:close-save-result", move |cx, args| {
        if let Some(token) = args.str(1) {
            super::owned_save::answer(&d, cx, token, args.get(0) == &IpcValue::Bool(true), args.str(2));
        } else {
            d.close.answer_save(cx.sender.view, args.get(0) == &IpcValue::Bool(true));
        }
        Ok(())
    }));

    let d = docs.clone();
    all.push(kit::send("docs:view-menu-state", move |cx, args| {
        let view = cx.sender.view;
        let s = args.get(0);
        let flag = |k: &str| kit::field(s, k) == &IpcValue::Bool(true);
        let next = [flag("aiSidebar"), flag("darkCanvas"), flag("odt")];
        d.view_menu.insert(view, next);
        // only the active tab patches the live menu; another tab's state waits for its focus
        if d.shell.active_docs_view() != Some(view) {
            return Ok(());
        }
        d.shell
            .set_menu_item_checked("docs-menu-ai-sidebar", next[0]);
        d.shell
            .set_menu_item_checked("docs-menu-dark-mode", next[1]);
        for id in ODT_OFF_MENU_IDS {
            d.shell.set_menu_item_enabled(id, !next[2]);
        }
        Ok(())
    }));

    all
}
