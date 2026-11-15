use std::cell::RefMut;
use std::collections::HashMap;
use std::rc::Rc;

use shell_contract::editors::{PreparedSavedDocument, SavedDocumentLoadOutcome, Then};
use shell_contract::ipc::{CallContext, NativeBrowserOwner, NativeBrowserTransition, NativeSavedDocument, NativeSavedTarget, Responder};
use shell_contract::runtime::CancelSubscription;
use shell_contract::{IpcError, IpcValue, Surface, ViewId};

use super::Docs;
use crate::kit;


pub(super) struct DecodedSaved {
    pub format: main_core::docs::doc_format::DocFormat,
    pub bytes: std::sync::Arc<Vec<u8>>,
    pub hash: String,
    pub password: Option<String>,
    pub encrypted: bool,
}

pub(super) fn decode_saved(
    document: &NativeSavedDocument, engine: &dyn super::hosts::DocsEngine,
    shell: &dyn super::hosts::DocsShell, permit: main_core::docs::byte_handoff::MediaPermit,
    cancel: shell_contract::runtime::CancelToken,
) -> Result<DecodedSaved, String> {
    use main_core::docs::doc_format::{DocFormat, doc_format_of_path};
    use super::passwords::{docx_open_kind, OpenKind};
    let target = document.target();
    let path = target.path().path().to_str().ok_or("The saved document path is not valid Unicode")?;
    let format = doc_format_of_path(path).ok_or("The saved document format is unavailable")?;
    let input = target.input();
    let bytes = input.bytes();
    if bytes.len() as u64 > super::MAX_OPEN_BYTES { return Err("The saved document exceeds the open size limit".into()); }
    if cancel.is_cancelled() { return Err("The saved document operation was cancelled".into()); }
    let kind = if format == DocFormat::Docx { docx_open_kind(bytes) } else { OpenKind::Plain };
    if kind == OpenKind::Damaged { return Err("The saved document is damaged".into()); }
    let (bytes, hash, password, encrypted) = match kind {
        OpenKind::Encrypted => {
            let hash = super::sha256_hex(bytes);
            let title = target.path().path().file_name().and_then(|name| name.to_str()).ok_or("The saved document name is not valid Unicode")?.to_owned();
            let (plain, password) = shell.unlock_saved_document(document.clone(), title)?.ok_or("The saved document password request was cancelled")?;
            (plain, hash, Some(password), true)
        }
        OpenKind::Plain if format == DocFormat::Docx => {
            match engine.open_lazy_saved(path, input.clone(), permit, cancel.clone())? {
                Some(opened) => (opened.bytes, opened.hash, None, false),
                None => (bytes.to_vec(), super::sha256_hex(bytes), None, false),
            }
        }
        OpenKind::Plain => (bytes.to_vec(), super::sha256_hex(bytes), None, false),
        OpenKind::Damaged => return Err("The saved document is damaged".into()),
    };
    if cancel.is_cancelled() { return Err("The saved document operation was cancelled".into()); }
    Ok(DecodedSaved { format, bytes: std::sync::Arc::new(bytes), hash, password, encrypted })
}

pub(super) struct SavedContent {
    pub bytes: Vec<u8>,
    pub recovered: bool,
    pub recovery_failed: bool,
    pub recovery: Option<(
        std::sync::Arc<main_core::packages::electron_utils::recovery_store::RecoveryPinnedSnapshot>,
        Box<main_core::packages::electron_utils::recovery_store::RecoveryCopyInfo>,
    )>,
}

/// Runs on a worker after the saved-document admission. The recovery snapshot
/// remains alive with the result; lazy media always refers to the original
/// committed archive, including its decrypted bytes for an encrypted file.
pub(super) fn prepare_content(
    document: &NativeSavedDocument, decoded: &DecodedSaved,
    recovery: main_core::packages::electron_utils::recovery_store::SavedRecoveryOpenResult,
    engine: &dyn super::hosts::DocsEngine, permit: main_core::docs::byte_handoff::MediaPermit,
    cancel: shell_contract::runtime::CancelToken,
) -> Result<SavedContent, String> {
    use main_core::docs::doc_format::{DocFormat, is_odt_package};
    use main_core::packages::electron_utils::recovery_store::SavedRecoveryOpenResult;
    use super::passwords::{docx_open_kind, OpenKind};
    if cancel.is_cancelled() { return Err("The saved document operation was cancelled".into()); }
    let (mut bytes, recovery) = match recovery {
        SavedRecoveryOpenResult::Original => (decoded.bytes.as_ref().clone(), None),
        SavedRecoveryOpenResult::Restore { snapshot, copy } => {
            (snapshot.bytes.clone(), Some((snapshot, copy)))
        }
    };
    let mut recovered = recovery.is_some();
    let mut recovery_failed = false;
    if recovered && decoded.encrypted && docx_open_kind(&bytes) != OpenKind::Plain {
        match decoded.password.as_deref().and_then(|password| engine.decrypt_docx(&bytes, password).ok()) {
            Some(plain) => bytes = plain,
            None => { recovered = false; recovery_failed = true; bytes = decoded.bytes.as_ref().clone(); }
        }
    }
    if recovered && decoded.format == DocFormat::Odt && !is_odt_package(&bytes) {
        recovered = false;
        recovery_failed = true;
        bytes = decoded.bytes.as_ref().clone();
    }
    if recovered && decoded.format == DocFormat::Docx {
        let path = document.target().path().path().to_str().ok_or("The saved document path is not valid Unicode")?;
        if decoded.encrypted {
            engine.adopt_lazy_decrypted(&bytes, path, decoded.bytes.clone(), permit, cancel.clone())?;
        } else {
            engine.adopt_lazy_saved(&bytes, path, document.target().input().clone(), permit, cancel.clone())?;
        }
    }
    if cancel.is_cancelled() { return Err("The saved document operation was cancelled".into()); }
    Ok(SavedContent { bytes, recovered, recovery_failed, recovery })
}
pub(super) struct SavedLoad {
    pub(super) target: NativeSavedTarget,
    pub(super) document: Option<NativeSavedDocument>,
    pub(super) waiter: Option<(CallContext, Responder)>,
    pub(super) done: Option<Then<SavedDocumentLoadOutcome>>,
    pub(super) token: String,
    pub(super) response: Option<IpcValue>,
    pub(super) started: bool,
    pub(super) applying: bool,
    pub(super) recovered: bool,
    pub(super) recovery: Option<std::sync::Arc<main_core::packages::electron_utils::recovery_store::RecoveryPinnedSnapshot>>,
    pub(super) staged: std::sync::Arc<std::sync::Mutex<Option<main_core::docs::byte_handoff::MediaPermit>>>,
    pub(super) failure: Option<String>,
    pub(super) owner_loss: Option<CancelSubscription>,
    pub(super) callbacks: HashMap<u64, std::sync::Arc<dyn SavedWake>>,
    pub(super) _losses: Vec<CancelSubscription>,
}
impl Drop for SavedLoad {
    fn drop(&mut self) {
        for callback in self.callbacks.values() { callback.cancel(); }
    }
}

pub(super) trait SavedWake: Send + Sync {
    fn wake(&self);
    fn cancel(&self);
}
struct SavedCallback<T> {
    value: std::sync::Mutex<Option<T>>,
    replay: std::sync::Mutex<Option<kit::CoreReplay>>,
}
impl<T: Send> SavedWake for SavedCallback<T> {
    fn wake(&self) {
        if self.value.lock().unwrap_or_else(|error| error.into_inner()).is_none() { return; }
        if let Some(replay) = self.replay.lock().unwrap_or_else(|error| error.into_inner()).clone() {
            replay.post(Box::new(|_| {}));
        }
    }
    fn cancel(&self) {
        if let Some(replay) = self.replay.lock().unwrap_or_else(|error| error.into_inner()).as_ref() { replay.cancel(); }
    }
}

pub(super) fn worker_callback<T: Send + 'static>(
    docs: &Rc<Docs>, target: &NativeSavedTarget, then: impl FnOnce(T) + 'static,
) -> Result<Box<dyn FnOnce(T) + Send>, String> {
    let id = kit::unique();
    let callback = std::sync::Arc::new(SavedCallback { value: std::sync::Mutex::new(None), replay: std::sync::Mutex::new(None) });
    let captured = callback.clone();
    let weak = Rc::downgrade(docs);
    let target_copy = target.clone();
    let replay = kit::replay_on_core(&docs.services, move || {
        let value = captured.value.lock().unwrap_or_else(|error| error.into_inner()).take();
        let Some(docs) = weak.upgrade() else { return; };
        let active = {
            let mut loads = docs.saved_loads.borrow_mut();
            match loads.get_mut(&target_copy.view()).filter(|slot| slot.target.same_target(&target_copy)) {
                Some(slot) => { slot.callbacks.remove(&id); slot.failure.is_none() }
                None => false,
            }
        };
        if active { if let Some(value) = value { then(value); } }
    });
    *callback.replay.lock().unwrap_or_else(|error| error.into_inner()) = Some(replay);
    let inserted = {
        let mut loads = docs.saved_loads.borrow_mut();
        match loads.get_mut(&target.view()).filter(|slot| slot.target.same_target(target) && slot.failure.is_none()) {
            Some(slot) => { slot.callbacks.insert(id, callback.clone()); true }
            None => false,
        }
    };
    if !inserted { callback.cancel(); return Err("The saved document barrier was released".into()); }
    Ok(Box::new(move |value| {
        *callback.value.lock().unwrap_or_else(|error| error.into_inner()) = Some(value);
        callback.wake();
    }))
}

pub(super) fn wake_target(docs: &Docs, target: &NativeSavedTarget) {
    let callbacks: Vec<_> = docs.saved_loads.borrow().get(&target.view())
        .filter(|slot| slot.target.same_target(target)).map(|slot| slot.callbacks.values().cloned().collect())
        .unwrap_or_default();
    for callback in callbacks { callback.wake(); }
}

struct Prepared {
    docs: Rc<Docs>,
    slot: Option<SavedLoad>,
    registration: Option<super::prepared_registration::DocsPreparedRegistration>,
    receipt: Rc<dyn shell_contract::editors::SavedActorRegistration>,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if self.slot.is_some() { self.docs.saved_reservations.set(self.docs.saved_reservations.get() - 1); }
    }
}
impl PreparedSavedDocument for Prepared {
    fn with_validated(
        &mut self, transition: &NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), IpcError>,
    ) -> Result<(), IpcError> {
        let slot = self.slot.take().ok_or_else(|| kit::error("The saved document barrier was already consumed"))?;
        let target = &slot.target;
        if !transition.matches(target.reservation()) || !transition.matches_owner(target.browser()) {
            self.slot = Some(slot);
            return Err(kit::error("The saved document belongs to another browser"));
        }
        let mut loads = self.docs.saved_loads.borrow_mut();
        if loads.contains_key(&target.view()) || self.docs.saved_reservations.get() == 0 {
            self.slot = Some(slot);
            return Err(kit::error("A saved document is already pending for this view"));
        }
        let registration = match self.registration.as_mut() {
            Some(registration) => registration,
            None => { self.slot = Some(slot); return Err(kit::error("The saved document registration was released")); }
        };
        if let Err(error) = registration.with_saved_validated(transition, next) {
            self.slot = Some(slot);
            return Err(error);
        }
        let view = slot.target.view();
        self.docs.saved_reservations.set(self.docs.saved_reservations.get() - 1);
        loads.insert(view, slot);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Rc<dyn shell_contract::editors::SavedActorRegistration> {
        self.receipt.clone()
    }
}

pub(super) fn prepare(
    docs: &Rc<Docs>, target: NativeSavedTarget,
    registration: super::prepared_registration::DocsPreparedRegistration,
) -> Result<Box<dyn PreparedSavedDocument>, IpcError> {
    if target.surface() != Surface::Docs { return Err(kit::error("The saved target is not a Word document")); }
    target.path().path().to_str().ok_or_else(|| kit::error("The saved document path is not valid Unicode"))?;
    let receipt = registration.saved_receipt().map_err(kit::error)?;
    let mut controls = vec![target.cancel().clone(), target.pending().clone(), target.created().loss_token()];
    controls.extend(target.output().loss_tokens().map_err(kit::error)?);
    controls.extend(target.additional().iter().cloned());
    for owner in target.participants() {
        controls.push(owner.loss_token().ok_or_else(|| kit::error("A saved document owner is unavailable"))?);
    }
    let staged: std::sync::Arc<std::sync::Mutex<Option<main_core::docs::byte_handoff::MediaPermit>>> = Default::default();
    let mut losses = Vec::with_capacity(controls.len() + 2);
    for control in controls {
        let weak = Rc::downgrade(docs); let captured = target.clone();
        let revoke = staged.clone();
        let notify = kit::back_on_core(&docs.services, move |()| {
            if let Some(docs) = weak.upgrade() { fail(&docs, &captured, "The saved document operation was released"); }
        });
        losses.push(control.on_cancel(move || {
            let permit = revoke.lock().unwrap_or_else(|error| error.into_inner()).as_ref().cloned();
            if let Some(permit) = permit { permit.revoke(); }
            notify(());
        }));
    }
    let slot = SavedLoad { target, document: None, waiter: None, done: None, token: docs.shell.random_uuid(), response: None, started: false, applying: false, recovered: false, recovery: None, staged, failure: None, owner_loss: None, callbacks: HashMap::new(), _losses: losses };
    let count = docs.saved_reservations.get().checked_add(1).ok_or_else(|| kit::error("The saved document barrier capacity is exhausted"))?;
    docs.saved_loads.borrow_mut().try_reserve(count).map_err(|_| kit::error("The saved document barrier could not be allocated"))?;
    docs.saved_reservations.set(count);
    Ok(Box::new(Prepared { docs: docs.clone(), slot: Some(slot), registration: Some(registration), receipt }))
}

pub(super) struct SavedLoadDisposal(SavedLoad);
impl SavedLoadDisposal {
    pub(super) fn fail(mut self, reason: &str) {
        if let Some((_, waiter)) = self.0.waiter.take() { waiter.reply(Err(kit::error(reason))); }
        if let Some(done) = self.0.done.take() { done(Err(kit::error(reason))); }
    }
}
pub(super) struct PreparedSavedDisposal<'a> {
    loads: RefMut<'a, HashMap<ViewId, SavedLoad>>,
    view: ViewId,
}
impl PreparedSavedDisposal<'_> {
    pub(super) fn commit(mut self) -> Option<SavedLoadDisposal> {
        self.loads.remove(&self.view).map(SavedLoadDisposal)
    }
}
pub(super) fn prepare_disposal<'a>(docs: &'a Docs, view: ViewId, owner: &NativeBrowserOwner) -> Result<PreparedSavedDisposal<'a>, String> {
    let loads = docs.saved_loads.try_borrow_mut().map_err(|_| "The saved document barrier is busy")?;
    if loads.get(&view).is_some_and(|slot| !slot.target.browser().same_owner(owner)) {
        return Err("The saved document barrier belongs to another browser".into());
    }
    Ok(PreparedSavedDisposal { loads, view })
}
pub(super) fn queue_document(docs: &Rc<Docs>, document: NativeSavedDocument, done: Then<SavedDocumentLoadOutcome>) -> Result<bool, IpcError> {
    let target = document.target().clone();
    let effects = match docs.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() {
        Some(effects) => effects,
        None => { let error = kit::error("Native saved document effects are unavailable"); done(Err(error.clone())); return Err(error); }
    };
    let loss = match document.owner().loss_token() {
        Some(loss) => loss,
        None => { let error = kit::error("The saved document owner lifetime is unavailable"); done(Err(error.clone())); return Err(error); }
    };
    let staged = match docs.saved_loads.borrow().get(&target.view()).filter(|slot| slot.target.same_target(&target)) {
        Some(slot) => slot.staged.clone(),
        None => { let error = kit::error("The saved document barrier was released"); done(Err(error.clone())); return Err(error); }
    };
    let weak = Rc::downgrade(docs); let captured = target.clone();
    let notify = kit::back_on_core(&docs.services, move |()| {
        if let Some(docs) = weak.upgrade() { fail(&docs, &captured, "The saved document owner was released"); }
    });
    let mut loss = Some(loss.on_cancel(move || {
        let permit = staged.lock().unwrap_or_else(|error| error.into_inner()).as_ref().cloned();
        if let Some(permit) = permit { permit.revoke(); }
        notify(());
    }));
    let mut done = Some(done);
    let mut ready = false;
    let extra = target.additional().iter().collect::<Vec<_>>();
    let admitted = effects.with_native_saved_document_input(&document, &[], target.cancel(), target.pending(), &extra, &mut |_| {
        let mut loads = docs.saved_loads.borrow_mut();
        let slot = loads.get_mut(&target.view()).filter(|slot| slot.target.same_target(&target)).ok_or("The saved document barrier was released")?;
        if slot.failure.is_some() || slot.document.is_some() || slot.done.is_some() { return Err("The saved document barrier cannot register another load".into()); }
        let subscription = loss.take().ok_or("The saved document owner subscription was consumed")?;
        let completion = done.take().ok_or("The saved document completion was consumed")?;
        slot.owner_loss = Some(subscription);
        slot.document = Some(document.clone());
        slot.done = Some(completion);
        ready = slot.waiter.is_some();
        Ok(())
    });
    if let Err(error) = admitted {
        if let Some(done) = done { done(Err(kit::error(error.clone()))); }
        return Err(kit::error(error));
    }
    Ok(ready)
}

pub(super) fn queue_consumer(docs: &Rc<Docs>, cx: &CallContext, responder: Responder) -> Result<bool, Responder> {
    let (target, staged) = match docs.saved_loads.borrow().get(&cx.sender.view).map(|slot| (slot.target.clone(), slot.staged.clone())) {
        Some(value) => value, None => return Err(responder),
    };
    if cx.sender.surface != Surface::Docs || !cx.native.as_ref().and_then(|native| native.renderer()).is_some_and(|renderer| renderer.same_generation(target.created())) {
        responder.reply(Err(kit::error("The saved document request belongs to another renderer")));
        return Ok(false);
    }
    let effects = match docs.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() {
        Some(effects) => effects, None => { responder.reply(Err(kit::error("Native saved document effects are unavailable"))); return Ok(false); }
    };
    let weak = Rc::downgrade(docs); let captured = target.clone();
    let notify = kit::back_on_core(&docs.services, move |()| {
        if let Some(docs) = weak.upgrade() { fail(&docs, &captured, "The saved document request was released"); }
    });
    let mut loss = Some(cx.cancel.on_cancel(move || {
        let permit = staged.lock().unwrap_or_else(|error| error.into_inner()).as_ref().cloned();
        if let Some(permit) = permit { permit.revoke(); }
        notify(());
    }));
    let mut reply = Some(responder);
    let mut ready = false;
    let mut extra = target.additional().iter().collect::<Vec<_>>();
    extra.push(&cx.cancel);
    let admitted = effects.with_native_saved_target_seed(&target, &[], target.cancel(), target.pending(), &extra, &mut |_| {
        let mut loads = docs.saved_loads.borrow_mut();
        let slot = loads.get_mut(&target.view()).filter(|slot| slot.target.same_target(&target)).ok_or("The saved document barrier was released")?;
        if slot.failure.is_some() || slot.waiter.is_some() || slot.applying { return Err("The saved document request cannot be consumed".into()); }
        let subscription = loss.take().ok_or("The saved document request subscription was consumed")?;
        let responder = reply.take().ok_or("The saved document response was consumed")?;
        slot._losses.push(subscription);
        slot.waiter = Some((cx.clone(), responder));
        ready = slot.document.is_some();
        Ok(())
    });
    if let Err(error) = admitted { if let Some(reply) = reply { reply.reply(Err(kit::error(error))); } return Ok(false); }
    Ok(ready)
}

struct Loading {
    document: NativeSavedDocument,
    actual: shell_contract::ipc::NativeRequestOwner,
    original: shell_contract::ipc::NativeRequestOwner,
    request_cancel: shell_contract::runtime::CancelToken,
    lease: main_core::docs::byte_handoff::MediaViewLease,
    old: main_core::docs::byte_handoff::MediaPermit,
    permit: main_core::docs::byte_handoff::MediaPermit,
}

impl Loading {
    fn admit(&self, docs: &Docs) -> Result<(), String> {
        let target = self.document.target();
        let effects = docs.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
            .ok_or("Native saved document effects are unavailable")?;
        let extra = target.additional().iter().collect::<Vec<_>>();
        effects.with_native_saved_document_same_origin(
            &self.document, &self.actual, &self.request_cancel, &[], target.cancel(), target.pending(), &extra,
            &mut |_| Ok(()),
        )
    }
    fn abort(&self, docs: &Docs, error: &str) {
        docs.abort_media_document(self.document.target().view(), &self.permit);
        fail(docs, self.document.target(), error);
    }
}

/// Worker results retain their exact target and can be redelivered after a
/// discarded core post. No saved path is reopened for decoding.
fn decode_on_worker(
    docs: &Rc<Docs>, loading: Loading,
    then: impl FnOnce(Loading, DecodedSaved) + 'static,
) -> Result<(), String> {
    let document = loading.document.clone();
    let engine = docs.engine.clone();
    let shell = docs.shell.clone();
    let permit = loading.permit.clone();
    let cancel = loading.request_cancel.clone();
    let target = document.target().clone();
    let completed = docs.clone();
    let deliver = worker_callback(docs, &target, move |result: Result<DecodedSaved, String>| {
        let decoded = match result {
            Ok(decoded) => decoded,
            Err(error) => return loading.abort(&completed, &error),
        };
        if let Err(error) = loading.admit(&completed) { return loading.abort(&completed, &error); }
        then(loading, decoded);
    })?;
    docs.services.workers.spawn(Box::new(move || {
        deliver(decode_saved(&document, engine.as_ref(), shell.as_ref(), permit, cancel));
    }));
    Ok(())
}

/// Admit the exact initial request and stage the successor before worker IO.
fn stage_load(docs: &Rc<Docs>, view: ViewId) -> Result<Loading, String> {
    let (document, cx) = {
        let loads = docs.saved_loads.borrow();
        let slot = loads.get(&view).ok_or("The saved document barrier was released")?;
        let document = slot.document.clone().ok_or("The saved document is unavailable")?;
        let cx = slot.waiter.as_ref().map(|(cx, _)| cx.clone()).ok_or("The saved document request is unavailable")?;
        (document, cx)
    };
    let actual = shell_contract::ipc::NativeRequestOwner {
        view, context: cx.native.clone().ok_or("The saved document request has no native owner")?,
    };
    let original = docs.checked_document_owner(view, actual.clone()).map_err(|error| error.message)?;
    let effects = docs.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
        .ok_or("Native saved document effects are unavailable")?;
    let target = document.target();
    let extra = target.additional().iter().collect::<Vec<_>>();
    effects.with_native_saved_document_same_origin(&document, &actual, &cx.cancel, &[], target.cancel(), target.pending(), &extra, &mut |_| {
        let mut loads = docs.saved_loads.borrow_mut();
        let slot = loads.get_mut(&view).filter(|slot| slot.target.same_target(target)).ok_or("The saved document barrier was released")?;
        if slot.started || slot.failure.is_some() || slot.waiter.is_none() { return Err("The saved document load already started".into()); }
        slot.started = true;
        Ok(())
    })?;
    let lease = docs.media_views.borrow().get(&view).map(|slot| slot.lease.clone()).ok_or("The document media view is not registered")?;
    let old = lease.current().ok_or("The document media session is closed")?;
    let candidate = lease.prepare_document()?;
    let prior = lease.pending();
    let stage = super::media_transition::DocsMediaStageAdapter {
        lease: lease.clone(), old: old.clone(), prior: prior.clone(), candidate: candidate.clone(),
    };
    if let Err(error) = effects.stage_saved_docs_media(
        &document, &actual, &cx.cancel, &original, &[], target.cancel(), target.pending(), &extra, &stage,
    ) {
        candidate.permit().revoke();
        return Err(error);
    }
    let permit = candidate.permit().clone();
    let stored = effects.with_native_saved_document_same_origin(
        &document, &actual, &cx.cancel, &[], target.cancel(), target.pending(), &extra,
        &mut |_| {
            let mut loads = docs.saved_loads.borrow_mut();
            let slot = loads.get_mut(&view).filter(|slot| slot.target.same_target(target) && slot.failure.is_none())
                .ok_or("The saved document barrier was released")?;
            *slot.staged.lock().map_err(|_| "The saved document stage is unavailable")? = Some(permit.clone());
            Ok(())
        },
    );
    if let Err(error) = stored {
        lease.abort_document(&permit);
        permit.revoke();
        return Err(error);
    }
    if let Some(prior) = prior { let _ = docs.engine.forget_lazy_media_owner(prior); }
    Ok(Loading { document, actual, original, request_cancel: cx.cancel, lease, old, permit: candidate.permit().clone() })
}

/// Publish only the staged permit and the exact retained document. Recovery
/// state changes, if needed, have already completed before this continuation.
fn publish_content(
    docs: &Rc<Docs>, loading: Loading, decoded: DecodedSaved, content: SavedContent,
    data_url: String, disk: Option<(f64, f64)>,
) -> Result<(), String> {
    let target = loading.document.target().clone();
    let view = target.view();
    let path = target.path().path().to_str().ok_or("The saved document path is not valid Unicode")?.to_owned();
    let effects = docs.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
        .ok_or("Native saved document effects are unavailable")?;
    let extra = target.additional().iter().collect::<Vec<_>>();
    let transition = super::media_transition::DocsMediaTransitionAdapter {
        lease: loading.lease.clone(), old: loading.old.clone(), successor: loading.permit.clone(),
    };
    let candidate = super::disposal::candidate(docs, view)?;
    let mut retain = |owner: &shell_contract::ipc::NativeRequestOwner, commit: &mut dyn FnMut() -> Result<(), String>| {
        super::disposal::retain_successor(candidate.as_ref(), &loading.lease, &loading.old, &loading.permit, owner, commit)
    };
    let mut before_notify = |document: &NativeSavedDocument, _actual: &shell_contract::ipc::NativeRequestOwner| {
        let loss = document.owner().loss_token().ok_or("The successor document owner lifetime is unavailable")?;
        let weak = Rc::downgrade(docs);
        let captured = target.clone();
        let notify = kit::back_on_core(&docs.services, move |()| {
            if let Some(docs) = weak.upgrade() { fail(&docs, &captured, "The saved document owner was released"); }
        });
        let subscription = loss.on_cancel(move || notify(()));
        let mut loads = docs.saved_loads.borrow_mut();
        let slot = loads.get_mut(&view).filter(|slot| slot.target.same_target(&target) && slot.failure.is_none())
            .ok_or("The saved document barrier was released")?;
        slot.document = Some(document.clone());
        slot.owner_loss = Some(subscription);
        slot.staged.lock().map_err(|_| "The saved document stage is unavailable")?.take();
        Ok(())
    };
    let (successor, actual) = effects.transition_saved_docs_media(
        &loading.document, &loading.actual, &loading.request_cancel, &loading.original,
        &[], target.cancel(), target.pending(), &extra, &transition, &mut retain, &mut before_notify,
    )?;
    #[cfg(any(test, feature = "probe"))]
    docs.retain_synthetic_successor(view, &actual);
    let _ = docs.engine.forget_lazy_media_owner(loading.old);
    let token = docs.saved_loads.borrow().get(&view).filter(|slot| slot.target.same_target(&target))
        .map(|slot| slot.token.clone()).ok_or("The saved document barrier was released")?;
    let response = super::Obj::new()
        .set("path", path.clone())
        .set("name", super::hosts::basename(&path))
        .set("dataUrl", data_url)
        .set("hash", decoded.hash.clone())
        .set("encrypted", decoded.encrypted)
        .set("recovered", if content.recovered { IpcValue::Bool(true) } else { IpcValue::Undefined })
        .set("recoveryReceipt", token)
        .done();
    let mut waiter = None;
    let mut recovery = content.recovery.map(|(snapshot, _)| snapshot);
    effects.with_native_saved_document_same_origin(
        &successor, &actual, &loading.request_cancel, &[], target.cancel(), target.pending(), &extra,
        &mut |_| {
            let mut loads = docs.saved_loads.borrow_mut();
            let slot = loads.get_mut(&view).filter(|slot| slot.target.same_target(&target) && slot.failure.is_none())
                .ok_or("The saved document barrier was released")?;
            if !slot.started || slot.applying || slot.waiter.is_none() { return Err("The saved document result cannot be published".into()); }
            docs.allow_doc_write(view, &path);
            docs.remember_open_doc(view, &path);
            docs.remember_disk(view, &path, decoded.hash.clone(), disk);
            let mut passwords = docs.passwords.borrow_mut();
            passwords.remember(view, &path, decoded.password.as_deref());
            passwords.mark_encrypted(view, &path, decoded.encrypted);
            slot.recovered = content.recovered;
            slot.recovery = if content.recovered { recovery.take() } else { None };
            slot.applying = true;
            waiter = slot.waiter.take();
            Ok(())
        },
    )?;
    let (_, responder) = waiter.ok_or("The saved document response was not admitted")?;
    responder.reply(Ok(response));
    Ok(())
}

pub(super) fn settle(
    docs: &Docs, cx: &CallContext, token: &str, result: Result<(), String>,
) -> Result<bool, String> {
    let view = cx.sender.view;
    let document = match docs.saved_loads.borrow().get(&view) {
        None => return Ok(false),
        Some(slot) => {
            if slot.token != token || !slot.applying || slot.failure.is_some() {
                return Err("The saved document receipt is invalid".into());
            }
            slot.document.clone().ok_or("The saved document owner is unavailable")?
        }
    };
    if cx.sender.surface != Surface::Docs { return Err("The saved document receipt belongs to another surface".into()); }
    let actual = shell_contract::ipc::NativeRequestOwner {
        view, context: cx.native.clone().ok_or("The saved document receipt has no native owner")?,
    };
    let target = document.target();
    let effects = docs.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
        .ok_or("Native saved document effects are unavailable")?;
    let extra = target.additional().iter().collect::<Vec<_>>();
    let mut completed = None;
    effects.with_native_saved_document_same_origin(
        &document, &actual, &cx.cancel, &[], target.cancel(), target.pending(), &extra,
        &mut |_| {
            let mut loads = docs.saved_loads.borrow_mut();
            let slot = loads.get(&view).ok_or("The saved document barrier was released")?;
            if !slot.target.same_target(target) || slot.token != token || !slot.applying || slot.failure.is_some() {
                return Err("The saved document receipt was replaced".into());
            }
            completed = loads.remove(&view);
            Ok(())
        },
    )?;
    let mut slot = completed.ok_or("The saved document receipt was not committed")?;
    if let Some(done) = slot.done.take() {
        done(result.map(|()| if slot.recovered {
            SavedDocumentLoadOutcome::RecoveryApplied
        } else {
            SavedDocumentLoadOutcome::Applied
        }).map_err(kit::error));
    }
    Ok(true)
}


fn fail(docs: &Docs, target: &NativeSavedTarget, reason: &str) {
    let (waiter, done, callbacks, staged) = {
        let mut loads = docs.saved_loads.borrow_mut();
        let Some(slot) = loads.get_mut(&target.view()).filter(|slot| slot.target.same_target(target)) else { return; };
        if slot.failure.is_some() { return; }
        slot.failure = Some(reason.to_owned());
        (slot.waiter.take(), slot.done.take(), std::mem::take(&mut slot.callbacks),
            slot.staged.lock().unwrap_or_else(|error| error.into_inner()).take())
    };
    for callback in callbacks.values() { callback.cancel(); }
    if let Some(permit) = staged { docs.abort_media_document(target.view(), &permit); }
    if let Some((_, waiter)) = waiter { waiter.reply(Err(kit::error(reason))); }
    if let Some(done) = done { done(Err(kit::error(reason))); }
}
