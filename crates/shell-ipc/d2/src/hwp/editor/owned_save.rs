use super::*;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelSubscription, CancelToken, RendererGeneration};
use std::sync::atomic::{AtomicBool, Ordering};

struct ReloadWatch {
    origin_transferred: AtomicBool,
    cancel_transferred: AtomicBool,
    losses: Arc<Mutex<Option<shell_contract::ipc::AcceptedOwnerLosses>>>,
    subscriptions: Mutex<Vec<CancelSubscription>>,
}

impl ReloadWatch {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            origin_transferred: AtomicBool::new(false),
            cancel_transferred: AtomicBool::new(false),
            losses: Arc::new(Mutex::new(None)),
            subscriptions: Mutex::new(Vec::new()),
        })
    }

    fn accept(&self, scope: &Scope, marker: shell_contract::hosts::AcceptedPdfReload) {
        if !marker.retires(&scope.origin) { return; }
        let losses = match scope.origin.accepted_reload_losses(&marker, &scope.cancel, &scope.pending) {
            Ok(losses) => losses,
            Err(_) => { scope.pending.cancel(); return; }
        };
        let subscriptions = losses.tokens().map(|loss| {
            let pending = scope.pending.clone();
            loss.on_cancel(move || pending.cancel())
        }).collect::<Vec<_>>();
        if let Ok(mut stored) = self.losses.lock() {
            if stored.is_some() { scope.pending.cancel(); return; }
            self.cancel_transferred.store(!losses.cancel().same_token(&scope.cancel), Ordering::Release);
            self.origin_transferred.store(true, Ordering::Release);
            *stored = Some(losses);
        } else { scope.pending.cancel(); return; }
        match self.subscriptions.lock() {
            Ok(mut stored) => *stored = subscriptions,
            Err(_) => scope.pending.cancel(),
        }
    }
}


mod recovery;
mod close;

#[derive(Clone)]
pub(super) struct Scope {
    pub origin: NativeRequestOwner,
    pub destination: NativeRequestOwner,
    pub participants: Vec<NativeRequestOwner>,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    pub additional: Vec<CancelToken>,
    pub effects: OwnedEffectsHost,
    pub recovery: Option<(shell_contract::ipc::RecoverySeedReceipt, shell_contract::ipc::RecoveryAuthoritySnapshot)>,
}
impl Scope {
    pub fn effect(&self, action: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        match &self.recovery {
            Some((receipt, snapshot)) => self.effects.with_recovery_open_snapshot(receipt, snapshot, &self.origin, &self.participants, &self.cancel, &self.pending, action),
            None => self.effects.with_owned_effect_many_extra(&self.origin, &self.participants, &self.cancel, &self.pending, &self.additional.iter().collect::<Vec<_>>(), action),
        }
    }
    pub fn admit(&self, action: &mut dyn FnMut()) -> Result<(), main_core::node_fs::NodeError> {
        self.effect(&mut || { action(); Ok(()) }).map_err(|message| main_core::node_fs::NodeError { code: None, message })
    }
}

#[derive(Clone)]
pub(super) enum RenameScope {
    Renderer(Scope),
    Native {
        snapshot: shell_contract::ipc::NativeSaveRevisionSnapshot,
        expected_path: PathBuf,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
    },
}
impl RenameScope {
    fn effect(&self, action: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        match self {
            Self::Renderer(scope) => scope.effect(action),
            Self::Native { snapshot, expected_path, participants, cancel, pending, additional, effects } =>
                effects.with_native_save_revision_snapshot_effect(snapshot, participants, cancel, pending, &additional.iter().collect::<Vec<_>>(), &mut || {
                    if snapshot.selection().path() != expected_path { return Err("hwp: native save destination changed".into()); }
                    action()
                }),
        }
    }
    fn admit(&self, action: &mut dyn FnMut()) -> Result<(), main_core::node_fs::NodeError> {
        self.effect(&mut || { action(); Ok(()) }).map_err(|message| main_core::node_fs::NodeError { code: None, message })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase { Requested, Choosing, Exporting, Pdf, ClaimedPdfExport, Writing }
pub(super) struct OwnedSave {
    token: String,
    scope: Scope,
    generation: RendererGeneration,
    phase: Phase,
    pdf_target: Option<String>,
    timer: Option<Timer>,
    then: Then<bool>,
    reload_watch: Arc<ReloadWatch>,
    _subscriptions: Vec<CancelSubscription>,
}

pub(super) struct OwnedClose {
    id: u64,
    scope: Scope,
    then: RefCell<Option<Then<bool>>>,
    subscriptions: RefCell<Vec<CancelSubscription>>,
}

impl Hwp {
    pub(super) fn rotate_document_epoch(&self, view: ViewId) {
        self.cancel_owned(view, "hwp: document replaced");
        let previous = self.change(view, |state| state.document_epoch.replace(RendererGeneration::new()));
        if let Some(previous) = previous { previous.revoke(); }
    }

    pub(super) fn capture_hwp_owner(&self, view: ViewId, mut native: NativeRequestOwner) -> Result<NativeRequestOwner, IpcError> {
        if native.view != view || self.is_destroyed(view) { return Err(error("hwp: document owner mismatch")); }
        let current = self.services.views.renderer_generation(view).ok_or_else(|| error("hwp: renderer is unavailable"))?;
        if !native.context.renderer().is_some_and(|generation| generation.same_generation(&current)) {
            return Err(error("hwp: renderer owner mismatch"));
        }
        let epoch = self.get(view, |state| state.document_epoch.clone()).ok_or_else(|| error("hwp: document is not tracked"))?;
        if !epoch.is_active() { return Err(error("hwp: document replaced")); }
        if let Some(captured) = native.context.document_lease() {
            if !captured.same_generation(&epoch) { return Err(error("hwp: stale document owner")); }
        } else {
            native.context = native.context.with_document_lease(epoch);
        }
        Ok(native)
    }

    pub(super) fn finish_owned(&self, view: ViewId, token: &str, result: Result<bool, IpcError>) {
        let operation = {
            let mut saves = self.owned_saves.borrow_mut();
            if !saves.get(&view).is_some_and(|save| save.token == token) { return; }
            saves.remove(&view)
        };
        if let Some(operation) = operation {
            if let Some(timer) = operation.timer { kit::clear(timer); }
            operation.scope.pending.cancel();
            self.change(view, |state| state.pending_save = None);
            (operation.then)(result);
        }
    }

    pub(super) fn cancel_owned(&self, view: ViewId, message: &str) {
        self.cancel_close_owned(view, message);
        let token = self.owned_saves.borrow().get(&view).map(|save| save.token.clone());
        if let Some(token) = token { self.finish_owned(view, &token, Err(error(message))); }
    }

    pub(super) fn owned_scope(&self, cx: &CallContext, token: &str, phase: Phase) -> Result<Scope, String> {
        let saves = self.owned_saves.borrow();
        let save = saves.get(&cx.sender.view).ok_or("hwp: no owned save in progress")?;
        if save.token != token || save.phase != phase || !cx.native.as_ref().and_then(|native| native.renderer()).is_some_and(|generation| generation.same_generation(&save.generation)) {
            return Err("hwp: stale save request".into());
        }
        save.scope.effect(&mut || Ok(()))?;
        Ok(save.scope.clone())
    }

    pub(super) fn request_save_owned(
        self: &Rc<Self>, view: ViewId, mode: SaveMode,
        origin: NativeRequestOwner, cancel: CancelToken, then: Then<bool>,
    ) {
        if self.is_destroyed(view) || !self.get(view, |state| state.granted) { return then(Err(error("hwp: document is no longer open"))); }
        let Some(generation) = self.services.views.renderer_generation(view) else { return then(Err(error("hwp: renderer is unavailable"))); };
        let destination = match self.owned_effects.capture_owner(view, &generation) {
            Ok(owner) => match self.capture_hwp_owner(view, owner) {
                Ok(owner) => owner,
                Err(error) => return then(Err(error)),
            },
            Err(message) => return then(Err(error(message))),
        };
        self.request_save_captured(view, mode, origin, destination, cancel, then);
    }

    fn request_save_captured(
        self: &Rc<Self>, view: ViewId, mode: SaveMode, origin: NativeRequestOwner,
        destination: NativeRequestOwner, cancel: CancelToken, then: Then<bool>,
    ) {
        if let Err(error) = self.capture_hwp_owner(view, destination.clone()) { return then(Err(error)); }
        if self.save_waiters.borrow().contains_key(&view) || self.owned_saves.borrow().contains_key(&view) || self.get(view, |state| state.pending_save.is_some()) { return then(Ok(false)); }
        let Some(generation) = destination.context.renderer().cloned() else { return then(Err(error("hwp: renderer is unavailable"))); };
        let pending = CancelToken::default();
        let scope = Scope { origin, participants: vec![destination.clone()], destination, cancel, pending: pending.clone(), additional: Vec::new(), effects: self.owned_effects.clone(), recovery: None };
        if self.get(view, |state| state.memo_modal) {
            let result = scope.effects.emit_owned(&scope.origin, &scope.destination, &scope.cancel, &scope.pending, "hwp:memo-blocked", vec![]);
            return then(result.map(|()| false).map_err(error));
        }
        if let Err(message) = scope.effect(&mut || Ok(())) { return then(Err(error(message))); }
        let token = kit::unique().to_string();
        let hwp = self.clone();
        let timed_token = token.clone();
        let timer = kit::after(&self.services, SAVE_WAIT, move || hwp.finish_owned(view, &timed_token, Err(error("hwp: save timed out"))));
        let reload_watch = ReloadWatch::new();
        let mut subscriptions = Vec::new();
        if let Some(loss) = scope.origin.context.owner_loss() {
            let pending = pending.clone();
            let watch = reload_watch.clone();
            subscriptions.push(loss.on_cancel(move || {
                if !watch.origin_transferred.load(Ordering::Acquire) { pending.cancel(); }
            }));
        }
        {
            let pending = pending.clone();
            let watch = reload_watch.clone();
            subscriptions.push(scope.cancel.on_cancel(move || {
                if !watch.cancel_transferred.load(Ordering::Acquire) { pending.cancel(); }
            }));
        }
        for loss in [scope.destination.context.owner_loss(), Some(generation.loss_token())].into_iter().flatten() {
            let pending = pending.clone();
            subscriptions.push(loss.on_cancel(move || pending.cancel()));
        }
        let hwp = self.clone();
        let cancelled_token = token.clone();
        let back = kit::back_on_core(&self.services, move |()| hwp.finish_owned(view, &cancelled_token, Err(error("hwp: save request expired"))));
        subscriptions.push(pending.on_cancel(move || back(())));
        self.owned_saves.borrow_mut().insert(view, OwnedSave { token: token.clone(), scope: scope.clone(), generation, phase: Phase::Requested, pdf_target: None, timer: Some(timer), then, _subscriptions: subscriptions, reload_watch });
        let result = scope.effects.emit_owned(&scope.origin, &scope.destination, &scope.cancel, &scope.pending,
            "hwp:save-request", vec![match mode { SaveMode::Save => "save", SaveMode::SaveAs => "saveAs" }.into_ipc(), token.as_str().into_ipc()]);
        if let Err(message) = result { self.finish_owned(view, &token, Err(error(message))); }
    }

    pub(super) fn begin_owned(self: &Rc<Self>, cx: &CallContext, mode: &IpcValue, token: &str, reply: shell_contract::ipc::Responder) {
        let view = cx.sender.view;
        let scope = match self.owned_scope(cx, token, Phase::Requested) {
            Ok(scope) => scope,
            Err(message) => return reply.reply(Ok(failed(message))),
        };
        let memo = self.get(view, |state| state.memo_modal);
        let tracked = self.get(view, |state| state.tracked);
        if memo || tracked {
            let notice = if memo {
                scope.effects.emit_owned(&scope.origin, &scope.destination, &scope.cancel, &scope.pending, "hwp:memo-blocked", vec![])
            } else { Ok(()) };
            let answer = match notice {
                Ok(()) => obj([
                    ("ok", false.into_ipc()),
                    ("error", if memo { "hwp: a memo editor is open" } else { "hwp: the document has tracked changes" }.into_ipc()),
                    ("reason", if memo { "memo" } else { "tracked" }.into_ipc()),
                ]),
                Err(message) => failed(message),
            };
            self.finish_owned(view, token, Ok(false));
            return reply.reply(Ok(answer));
        }
        let token = token.to_owned();
        let mut next = None;
        let result = scope.effect(&mut || {
            self.change(view, |state| { state.pending_save = None; state.pdf_save_as = None; });
            let current = self.get(view, |state| state.save_path.clone());
            let format = current.as_deref().and_then(hwp_format_of_path);
            self.owned_saves.borrow_mut().get_mut(&view).ok_or("hwp: save expired")?.phase = Phase::Choosing;
            next = Some((current, format));
            Ok(())
        });
        if let Err(message) = result { self.finish_owned(view, &token, Err(error(message.clone()))); return reply.reply(Ok(failed(message))); }
        let Some((current, current_format)) = next else { return reply.reply(Ok(failed("hwp: save expired"))); };
        let save_as = as_str(mode) == Some("saveAs");
        if let Some(path) = current.as_ref().filter(|_| current_format.is_some() && !save_as) {
            return self.owned_target_ready(view, &token, &scope, path.clone(), reply);
        }
        let (parent, options, fallback) = match self.save_dialog_options(view, current.as_deref(), current_format, save_as) {
            Ok(plan) => plan,
            Err(message) => { self.finish_owned(view, &token, Err(error(message.clone()))); return reply.reply(Ok(failed(message))); }
        };
        let options = main_core::packages::electron_utils::dialog_memory::prepare_dialog_options(
            &self.dialog_memory.lock().unwrap_or_else(|poison| poison.into_inner()), &options, None);
        let options = FileDialogs::options(&options);
        let hwp = self.clone();
        let callback_scope = scope.clone();
        let done = kit::back_on_core(&self.services, move |picked: Result<Option<PathBuf>, String>| {
            let picked = match picked {
                Ok(Some(path)) => path.to_string_lossy().into_owned(),
                Ok(None) => { hwp.finish_owned(view, &token, Ok(false)); return reply.reply(Ok(obj([("ok", true.into_ipc()), ("canceled", true.into_ipc())]))); }
                Err(message) => { hwp.finish_owned(view, &token, Err(error(message.clone()))); return reply.reply(Ok(failed(message))); }
            };
            let (path, kind) = save_as_target(&picked, fallback, save_as);
            let accepted = callback_scope.effect(&mut || {
                main_core::packages::electron_utils::dialog_memory::remember_save_path(&mut hwp.dialog_memory.lock().unwrap_or_else(|poison| poison.into_inner()), &picked);
                Ok(())
            });
            if let Err(message) = accepted { hwp.finish_owned(view, &token, Err(error(message.clone()))); return reply.reply(Ok(failed(message))); }
            if kind == HwpSaveAsKind::Pdf {
                return hwp.owned_pdf_target(view, &token, &callback_scope, path, reply);
            }
            hwp.owned_target_ready(view, &token, &callback_scope, path, reply);
        });
        self.dialogs.save_owned(parent, options, scope.origin, scope.destination, scope.cancel, scope.pending, done);
    }

    fn owned_target_ready(&self, view: ViewId, token: &str, scope: &Scope, path: String, reply: shell_contract::ipc::Responder) {
        let mut answer = None;
        let accepted = scope.effect(&mut || {
            let save = self.owned_saves.borrow();
            if !save.get(&view).is_some_and(|save| save.token == token && save.phase == Phase::Choosing) { return Err("hwp: stale save request".into()); }
            drop(save);
            answer = Some(self.set_pending_target(view, path.clone())?);
            if matches!(answer, Some(Ok(_))) { self.owned_saves.borrow_mut().get_mut(&view).ok_or("hwp: save expired")?.phase = Phase::Exporting; }
            Ok(())
        });
        match (accepted, answer) {
            (Ok(()), Some(Ok(answer))) => reply.reply(Ok(answer)),
            (Ok(()), Some(Err(answer))) => { self.finish_owned(view, token, Ok(false)); reply.reply(Ok(answer)); }
            (Err(message), _) => { self.finish_owned(view, token, Err(error(message.clone()))); reply.reply(Ok(failed(message))); }
            _ => reply.reply(Ok(failed("hwp: save expired"))),
        }
    }

    fn owned_pdf_target(&self, view: ViewId, token: &str, scope: &Scope, path: String, reply: shell_contract::ipc::Responder) {
        let accepted = scope.effect(&mut || {
            let mut saves = self.owned_saves.borrow_mut();
            let save = saves.get_mut(&view).filter(|save| save.token == token && save.phase == Phase::Choosing).ok_or("hwp: stale save request")?;
            save.phase = Phase::Pdf;
            save.pdf_target = Some(path.clone());
            Ok(())
        });
        match accepted {
            Ok(()) => reply.reply(Ok(obj([("ok", true.into_ipc()), ("path", path.into_ipc()), ("export", "pdf".into_ipc())]))),
            Err(message) => {
                self.finish_owned(view, token, Err(error(message.clone())));
                reply.reply(Ok(failed(message)));
            }
        }
    }

    pub(super) fn take_owned_pdf_grant(self: &Rc<Self>, view: ViewId, token: &str, generation: &RendererGeneration,
        actual: Option<&NativeRequestOwner>, caller_cancel: &CancelToken) -> Option<shell_contract::editors::OwnedPdfGrant> {
        let (scope, reload_watch) = {
            let saves = self.owned_saves.borrow();
            let save = saves.get(&view)?;
            if save.token != token || save.phase != Phase::Pdf || !save.generation.same_generation(generation) { return None; }
            (save.scope.clone(), save.reload_watch.clone())
        };
        let actual = actual?;
        let mut participants = scope.participants.clone();
        participants.push(scope.origin.clone());
        let mut path = None;
        let mut renderer_timer = None;
        scope.effects.with_same_native_origin_effect(&scope.destination, actual, &participants, &scope.cancel, &scope.pending, &[caller_cancel], &mut || {
            let mut saves = self.owned_saves.borrow_mut();
            let save = saves.get_mut(&view).filter(|save| save.token == token && save.phase == Phase::Pdf).ok_or("hwp: stale PDF grant")?;
            path = save.pdf_target.take();
            save.phase = Phase::ClaimedPdfExport;
            renderer_timer = save.timer.take();
            Ok(())
        }).ok()?;
        if let Some(timer) = renderer_timer { kit::clear(timer); }
        let path = PathBuf::from(path?);
        let hwp = self.clone();
        let completion_token = token.to_owned();
        let complete = kit::back_on_core(&self.services, move |result: Result<(), String>| {
            hwp.finish_owned(view, &completion_token, result.map(|()| false).map_err(error));
        });
        let accepted_scope = scope.clone();
        let accepted_losses = reload_watch.losses.clone();
        let accepted = Box::new(move |marker| reload_watch.accept(&accepted_scope, marker));
        Some(shell_contract::editors::OwnedPdfGrant {
            path, request_token: token.to_owned(), authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(scope.origin), destination: scope.destination,
            cancel: scope.cancel, pending: scope.pending, effects: scope.effects, accepted, accepted_losses, complete,
        })
    }

    pub(super) fn file_renamed_owned(
        self: &Rc<Self>, view: ViewId, old_path: &Path, new_path: &Path,
        origin: &NativeRequestOwner, destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[CancelToken], then: Then<()>,
    ) {
        if destination.context.document_lease().is_none() { return then(Err(error("hwp: captured document owner is required"))); }
        if let Err(error) = self.capture_hwp_owner(view, destination.clone()) { return then(Err(error)); }
        let mut destinations = Vec::with_capacity(participants.len() + 1);
        destinations.push(destination.clone());
        destinations.extend_from_slice(participants);
        let scope = Scope { origin: origin.clone(), destination: destination.clone(), participants: destinations, cancel: cancel.clone(), pending: pending.clone(), additional: additional.to_vec(), effects: self.owned_effects.clone(), recovery: None };
        self.file_renamed_scoped(view, old_path, new_path, RenameScope::Renderer(scope), then);
    }

    pub(super) fn file_renamed_recovery(
        self: &Rc<Self>, view: ViewId, old_path: &Path, new_path: &Path,
        receipt: &shell_contract::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner,
        destination: &NativeRequestOwner, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, then: Then<()>,
    ) {
        if destination.context.document_lease().is_none() { return then(Err(error("hwp: captured document owner is required"))); }
        if let Err(error) = self.capture_hwp_owner(view, destination.clone()) { return then(Err(error)); }
        let snapshot = match receipt.target().authority_snapshot() {
            Ok(snapshot) => snapshot,
            Err(message) => return then(Err(error(message))),
        };
        let mut destinations = Vec::with_capacity(participants.len() + 1);
        destinations.push(destination.clone());
        destinations.extend_from_slice(participants);
        let scope = Scope {
            origin: receiver.clone(), destination: destination.clone(), participants: destinations,
            cancel: cancel.clone(), pending: pending.clone(), additional: Vec::new(), effects: self.owned_effects.clone(),
            recovery: Some((receipt.clone(), snapshot)),
        };
        self.file_renamed_scoped(view, old_path, new_path, RenameScope::Renderer(scope), then);
    }

    pub(super) fn file_renamed_existing_document(
        self: &Rc<Self>, view: ViewId, old_path: &Path, new_path: &Path,
        authority: &shell_contract::ipc::NativeSaveRevisionAuthority, expected_path: &Path, destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        additional: &[CancelToken], then: Then<()>,
    ) {
        if destination.context.document_lease().is_none() { return then(Err(error("hwp: captured document owner is required"))); }
        if let Err(error) = self.capture_hwp_owner(view, destination.clone()) { return then(Err(error)); }
        let snapshot = match authority.snapshot() { Ok(snapshot) => snapshot, Err(message) => return then(Err(error(message))) };
        let mut owners = Vec::with_capacity(participants.len() + 1);
        owners.push(destination.clone());
        owners.extend_from_slice(participants);
        let scope = RenameScope::Native { snapshot, expected_path: expected_path.to_owned(), participants: owners, cancel: cancel.clone(), pending: pending.clone(),
            additional: additional.to_vec(), effects: self.owned_effects.clone() };
        self.file_renamed_scoped(view, old_path, new_path, scope, then);
    }

    fn file_renamed_scoped(
        self: &Rc<Self>, view: ViewId, old_path: &Path, new_path: &Path, scope: RenameScope, then: Then<()>,
    ) {
        let old_path = old_path.to_string_lossy().into_owned();
        let new_path = new_path.to_string_lossy().into_owned();
        let changed = scope.effect(&mut || {
            self.change(view, |state| {
                if state.save_path.as_deref() == Some(old_path.as_str()) { state.save_path = Some(new_path.clone()); }
                if state.open_path.as_deref() == Some(old_path.as_str()) { state.open_path = Some(new_path.clone()); }
                if state.allowed.contains(&old_path) { state.allowed.insert(new_path.clone()); }
                if let Some(recorded) = state.disk_states.remove(&old_path) { state.disk_states.insert(new_path.clone(), recorded); }
            });
            Ok(())
        });
        if let Err(message) = changed { return then(Err(error(message))); }
        self.renamed_recovery_owned(view, old_path, new_path, scope, then);
    }
}


impl Hwp {
    pub(super) fn finish_commit(
        self: &Rc<Self>, view: ViewId, target: String, before: Option<RecoveryIdentity>,
        landed: Landed, scope: Option<Scope>, done: Box<dyn FnOnce(IpcValue)>,
    ) {
        let mut is_new = false;
        let mut update = || {
            if !self.is_destroyed(view) {
                is_new = self.change(view, |state| {
                    if let Some(disk) = landed.disk.clone() { state.disk_states.insert(target.clone(), disk); }
                    state.written.insert(target.clone());
                    let is_new = state.save_path.as_deref() != Some(target.as_str());
                    state.save_path = Some(target.clone());
                    state.untitled_copy = None;
                    state.open_path = Some(target.clone());
                    state.allowed.insert(target.clone());
                    state.dirty = false;
                    is_new
                });
            }
            Ok(())
        };
        if let Some(scope) = scope {
            if let Err(message) = scope.effect(&mut update) { return done(failed(message)); }
            let hwp = self.clone();
            let continuation_scope = scope.clone();
            self.saved_recovery_owned(view, before, scope, Box::new(move |result| {
                match result {
                    Ok(()) => hwp.notify_commit(view, target, landed, is_new, Some(continuation_scope), done),
                    Err(error) => done(failed(error.message)),
                }
            }));
            return;
        }
        let _ = update();
        if !self.is_destroyed(view) {
            self.with_recovery(|recovery, electron, shared| recovery.hwp_recovery_saved(electron, shared, wc(view), before));
        }
        self.notify_commit(view, target, landed, is_new, None, done);
    }

    fn notify_commit(self: &Rc<Self>, view: ViewId, target: String, landed: Landed, is_new: bool, scope: Option<Scope>, done: Box<dyn FnOnce(IpcValue)>) {
        if !is_new { return done(commit_reply(&target, landed)); }
        let Some(scope) = scope else {
            self.shell.file_saved(view, &target);
            return done(commit_reply(&target, landed));
        };
        let notified_target = target.clone();
        let hwp = self.clone();
        let callback_scope = scope.clone();
        let callback = Box::new(move |result: Result<shell_contract::hosts::SavedFile<String>, String>| {
            let saved = match result { Ok(saved) => saved, Err(message) => return done(failed(message)) };
            if saved.path == target { return finalize_saved_commit(saved, landed, done); }
            let final_path = saved.path.clone();
            let worker_path = final_path.clone();
            let hwp2 = hwp.clone();
            on_worker(&hwp.services, move || record_disk_state(&worker_path), move |disk| {
                let result = callback_scope.effect(&mut || {
                    hwp2.change(view, |state| {
                        state.open_path = Some(final_path.clone());
                        state.save_path = Some(final_path.clone());
                        state.allowed.insert(final_path.clone());
                        state.written.insert(final_path.clone());
                        if let Some(disk) = &disk { state.disk_states.insert(final_path.clone(), disk.clone()); }
                    });
                    Ok(())
                });
                match result { Ok(()) => finalize_saved_commit(saved, landed, done), Err(message) => done(failed(message)) }
            });
        });
        self.shell.file_saved_owned(view, notified_target, scope.origin, scope.destination, scope.cancel, scope.pending, scope.effects, callback);
    }
}

fn finalize_saved_commit(saved: shell_contract::hosts::SavedFile<String>, landed: Landed, done: Box<dyn FnOnce(IpcValue)>) {
    let shell_contract::hosts::SavedFile { path, session } = saved;
    session.finalize(Box::new(move |result| {
        done(match result { Ok(()) => commit_reply(&path, landed), Err(message) => failed(message) });
    }));
}

fn commit_reply(target: &str, landed: Landed) -> IpcValue {
    let mut answer = vec![("ok".to_owned(), true.into_ipc()), ("path".to_owned(), target.into_ipc())];
    if let Some(backup) = landed.backup_path { answer.push(("backupPath".into(), backup.into_ipc())); }
    if landed.tails_lost { answer.push(("tailsLost".into(), true.into_ipc())); }
    if landed.memos_lost > 0 { answer.push(("memosLost".into(), (landed.memos_lost as f64).into_ipc())); }
    IpcValue::Object(answer)
}

impl Hwp {
    pub(super) fn commit_owned(self: &Rc<Self>, cx: &CallContext, args: &Args, token: &str, reply: shell_contract::ipc::Responder) {
        let view = cx.sender.view;
        let scope = match self.owned_scope(cx, token, Phase::Exporting) {
            Ok(scope) => scope,
            Err(message) => return reply.reply(Ok(failed(message))),
        };
        let token = token.to_owned();
        let hwp = self.clone();
        let completion_scope = scope.clone();
        let done = Box::new(move |answer: IpcValue| {
            let ok = matches!(field(&answer, "ok"), IpcValue::Bool(true));
            let mut delivery = None;
            let mut operation = None;
            let mut reply = Some(reply);
            let mut answer = Some(answer);
            let result = completion_scope.effect(&mut || {
                let mut saves = hwp.owned_saves.borrow_mut();
                if !saves.get(&view).is_some_and(|save| save.token == token && save.phase == Phase::Writing) {
                    return Err("hwp: stale save completion".into());
                }
                operation = saves.remove(&view);
                drop(saves);
                hwp.change(view, |state| state.pending_save = None);
                delivery = Some((reply.take().ok_or("hwp: duplicate save completion")?, answer.take().ok_or("hwp: missing save completion")?));
                Ok(())
            });
            match result {
                Ok(()) => {
                    if let Some((reply, answer)) = delivery { reply.reply(Ok(answer)); }
                    if let Some(operation) = operation {
                        if let Some(timer) = operation.timer { kit::clear(timer); }
                        operation.scope.pending.cancel();
                        (operation.then)(Ok(ok));
                    }
                }
                Err(message) => {
                    if let Some(reply) = reply { reply.reply(Ok(failed(message.clone()))); }
                    hwp.finish_owned(view, &token, Err(error(message)));
                }
            }
        });
        let mut pending = None;
        let accepted = scope.effect(&mut || {
            pending = self.change(view, |state| state.pending_save.take());
            self.owned_saves.borrow_mut().get_mut(&view).ok_or("hwp: save expired")?.phase = Phase::Writing;
            Ok(())
        });
        if let Err(message) = accepted { return done(failed(message)); }
        let Some((target, format)) = pending else { return done(failed("hwp: no save in progress")); };
        let data = match uint8(args.get(0)) {
            Some(bytes) if !bytes.is_empty() && bytes.len() <= MAX_HWP_SAVE_BYTES => bytes,
            _ => return done(failed("hwp: bad save request")),
        };
        if !bytes_match_format(data, format) { return done(failed(format!("hwp: exported bytes are not a {} file", format.as_str()))); }
        let data = data.to_vec();
        let tabs = args.get(1).clone();
        let (auto, memos) = commit_options_of(args.get(2));
        let recorded = self.get(view, |state| state.disk_states.get(&target).cloned());
        let elsewhere = self.shell.is_path_open_elsewhere(&target);
        let worker_path = target.clone();
        let hwp = self.clone();
        on_worker(&self.services, move || {
            let probe = |_: &str| Ok(elsewhere);
            should_defer_auto_save(Some(&probe), &worker_path, recorded.as_ref())
        }, move |conflicted| {
            if !conflicted {
                return hwp.commit_ready(view, target, format, data, tabs, memos, Some(scope), done);
            }
            if auto { return done(failed(EXTERNAL_MODIFIED)); }
            let labels = main_core::packages::electron_utils::save_conflict_labels::save_conflict_labels(&hwp.shell.ui_lang());
            let options = MessageBox {
                kind: Some("warning".into()), message: labels.message,
                detail: Some(labels.detail), buttons: vec![labels.overwrite, labels.cancel],
                default_id: Some(0), cancel_id: Some(1),
                ..Default::default()
            };
            let callback_scope = scope.clone();
            let hwp2 = hwp.clone();
            let callback = kit::back_on_core(&hwp.services, move |answer: Result<shell_contract::hosts::MessageBoxAnswer, String>| {
                match answer {
                    Ok(answer) if answer.response == 0 => hwp2.commit_ready(view, target, format, data, tabs, memos, Some(callback_scope), done),
                    Ok(_) => done(failed(EXTERNAL_MODIFIED)),
                    Err(message) => done(failed(message)),
                }
            });
            hwp.dialogs.message_owned(hwp.window_of(view), options, scope.origin, scope.destination, scope.cancel, scope.pending, callback);
        });
    }

    pub(super) fn owned_save_pending(&self, view: ViewId) -> bool { self.owned_saves.borrow().contains_key(&view) }

    pub(super) fn abort_owned(&self, cx: &CallContext, token: &str) {
        let valid = self.owned_saves.borrow().get(&cx.sender.view).is_some_and(|save| {
            save.token == token && cx.native.as_ref().and_then(|native| native.renderer()).is_some_and(|generation| generation.same_generation(&save.generation))
        });
        if valid { self.finish_owned(cx.sender.view, token, Ok(false)); }
    }
}
