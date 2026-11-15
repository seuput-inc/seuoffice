//! Captured recovery preparation. Namespace effects return to the Docs actor.

use std::io::Read;
use std::rc::Rc;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use main_core::docs::doc_format::recovery_ext_for;
use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::atomic_write::owned::{self, Admission};
use main_core::packages::electron_utils::recovery_contract::{RecoveryOpenDone, RecoveryOpenFileRequest, RecoveryOpenUntitledRequest};
use main_core::packages::electron_utils::recovery_store::{
    RecoveryAdoptOptions, RecoveryAdoptPlan, RecoveryCopyState, RecoveryDeleteReason,
    RecoveryIdentity, RecoveryPinnedRead, RecoveryStore, RecoveryStoreOptions, recovery_copy_path, create_recovery_store,
};
use main_core::shell::renderer_disposal::Failure;
use shell_contract::ipc::{NativeInvocationSelection, NativeRecoveryCopy, RecoveryOpenContext, RecoveryOpenTarget};
use shell_contract::ViewId;

use super::Docs;
use crate::kit;

pub(super) struct PendingSeed {
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    token: String,
    blank: bool,
    prepared_copy: Arc<std::sync::Mutex<Option<super::open::PreparedRecoveryCopy>>>,
    source: Option<(String, Arc<std::fs::File>)>,
    permit: Option<main_core::docs::byte_handoff::MediaPermit>,
    permit_loss: Option<main_core::docs::byte_handoff::MediaLossSubscription>,
    password_path: Option<String>,
    work_settlements: Vec<shell_contract::runtime::CancelSubscription>,
    _loss: shell_contract::runtime::CancelSubscription,
}

pub(super) struct DetachedRecoverySeed {
    seed: PendingSeed,
    _window_open: Option<String>,
    _decision: Option<(String, main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision)>,
    _untitled: Option<Option<super::UntitledOpen>>,
}

impl DetachedRecoverySeed {
    pub(super) fn fail(self, message: &str) {
        if let Ok(notification) = self.seed.receipt.fail_after_loss(message.to_owned()) {
            notification.notify();
        }
    }
}

impl Docs {
    fn retain_recovery_seed(&self, view: ViewId, receipt: shell_contract::ipc::RecoverySeedReceipt, blank: bool,
        prepared_copy: Option<super::open::PreparedRecoveryCopy>, source: Option<(String, Arc<std::fs::File>)>) {
        let loss_receipt = receipt.clone();
        let loss = receipt.target().created().loss_token().on_cancel(move || {
            if let Ok(notification) = loss_receipt.fail_after_loss("the Docs recovery renderer was lost".into()) {
                notification.notify();
            }
        });
        let previous = self.recovery_seed.borrow_mut().insert(view, PendingSeed {
            receipt, token: self.shell.random_uuid(), blank, prepared_copy: Arc::new(std::sync::Mutex::new(prepared_copy)), source,
            permit: None, permit_loss: None, password_path: None, work_settlements: Vec::new(), _loss: loss,
        });
        if let Some(previous) = previous {
            if let Ok(notification) = previous.receipt.fail_after_loss("the Docs recovery seed was replaced".into()) {
                notification.notify();
            }
        }
    }

    pub(super) fn recovery_ingress<T>(&self, cx: &shell_contract::ipc::CallContext, action: impl FnOnce() -> T) -> Result<T, String> {
        let receipt = self.recovery_seed.borrow().get(&cx.sender.view).map(|seed| seed.receipt.clone());
        let Some(receipt) = receipt else { return Ok(action()); };
        let context = cx.native.clone().ok_or("the recovery renderer has no native owner")?;
        let owner = self.checked_document_owner(cx.sender.view, shell_contract::ipc::NativeRequestOwner { view: cx.sender.view, context })
            .map_err(|error| format!("{error:?}"))?;
        let effects = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().ok_or("native recovery effects are unavailable")?;
        let mut action = Some(action);
        let mut output = None;
        effects.with_recovery_open_effect(&receipt, &owner, &cx.cancel, &mut || {
            output = Some(action.take().ok_or("the recovery request was already consumed")?());
            Ok(())
        })?;
        output.ok_or_else(|| "the recovery request was not admitted".into())
    }

    pub(super) fn capture_recovery_load_scope(&self, cx: &shell_contract::ipc::CallContext, token: &str) -> Result<super::open::RecoveryLoadScope, String> {
        let (receipt, prepared_copy, source) = self.recovery_seed.borrow().get(&cx.sender.view)
            .filter(|seed| seed.token == token).map(|seed| (seed.receipt.clone(), seed.prepared_copy.clone(), seed.source.clone()))
            .ok_or("the recovery receipt is no longer pending")?;
        let context = cx.native.clone().ok_or("the recovery renderer has no native owner")?;
        let owner = self.checked_document_owner(cx.sender.view, shell_contract::ipc::NativeRequestOwner { view: cx.sender.view, context })
            .map_err(|error| format!("{error:?}"))?;
        let effects = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned().ok_or("native recovery effects are unavailable")?;
        let snapshot = receipt.target().authority_snapshot()?;
        let original_permit = self.media_permit(cx.sender.view).ok_or("the Docs recovery document is unavailable")?;
        let identity = original_permit.owner();
        effects.with_recovery_open_snapshot(&receipt, &snapshot, &owner, &[], &cx.cancel, &cx.cancel, &mut || {
            if !original_permit.is_active() || effects.docs_document_identity(&owner) != Some((cx.sender.view, identity.generation)) {
                return Err("the Docs recovery document was replaced".into());
            }
            Ok(())
        })?;
        let (source_path, source_file) = source.map_or((None, None), |(path, file)| (Some(path), Some(file)));
        Ok(super::open::RecoveryLoadScope { receipt, snapshot, owner, cancel: cx.cancel.clone(), effects,
            original_permit, permit: Arc::new(std::sync::Mutex::new(None)), prepared_copy, source_path, source_file, work: None })
    }

    pub(super) fn recovery_token(&self, view: ViewId) -> Option<String> {
        self.recovery_seed.borrow().get(&view).map(|seed| seed.token.clone())
    }

    pub(super) fn recovery_blank_token(&self, view: ViewId) -> Option<String> {
        self.recovery_seed.borrow().get(&view).filter(|seed| seed.blank).map(|seed| seed.token.clone())
    }

    pub(super) fn begin_recovery_work(&self, view: ViewId, token: &str) -> Result<shell_contract::ipc::RecoveryWorkTicket, String> {
        let receipt = self.recovery_seed.borrow().get(&view)
            .filter(|seed| seed.token == token).map(|seed| seed.receipt.clone())
            .ok_or("the recovery receipt is no longer pending")?;
        let ticket = receipt.begin_work()?;
        let until_settlement = ticket.clone();
        let subscription = receipt.on_settled(move |_| drop(until_settlement));
        let mut seeds = self.recovery_seed.borrow_mut();
        let seed = seeds.get_mut(&view).filter(|seed| seed.receipt.matches(receipt.target()))
            .ok_or("the recovery seed was replaced")?;
        seed.work_settlements.push(subscription);
        Ok(ticket)
    }

    pub(super) fn recovery_response(&self, view: ViewId, token: &str, mut result: shell_contract::IpcValue,
        permit: Option<main_core::docs::byte_handoff::MediaPermit>) -> shell_contract::IpcValue {
        let shell_contract::IpcValue::Object(fields) = &mut result else { return result; };
        let needs_password = fields.iter().any(|(key, value)| key == "needsPassword" && *value == shell_contract::IpcValue::Bool(true));
        let receipt = self.recovery_seed.borrow().get(&view).filter(|seed| seed.token == token).map(|seed| seed.receipt.clone());
        let Some(receipt) = receipt else { return result; };
        let permit_loss = if needs_password { None } else { permit.as_ref().map(|permit| {
            permit.on_revoke(move || {
                if let Ok(notification) = receipt.fail_after_loss("the recovered Docs document was replaced".into()) {
                    notification.notify();
                }
            })
        }) };
        let mut seeds = self.recovery_seed.borrow_mut();
        let Some(seed) = seeds.get_mut(&view).filter(|seed| seed.token == token) else { return result; };
        if needs_password {
            seed.password_path = fields.iter().find_map(|(key, value)| match (key.as_str(), value) {
                ("path", shell_contract::IpcValue::String(path)) => Some(path.clone()),
                _ => None,
            });
        }
        seed.permit = permit;
        seed.permit_loss = permit_loss;
        fields.push(("recoveryReceipt".into(), shell_contract::IpcValue::String(seed.token.clone())));
        result
    }

    pub(super) fn recovery_password_token(&self, view: ViewId, path: &str) -> Option<String> {
        self.recovery_seed.borrow().get(&view)
            .filter(|seed| seed.password_path.as_deref() == Some(path))
            .map(|seed| seed.token.clone())
    }

    pub(super) fn recovery_password_retry(&self, view: ViewId, token: &str) {
        let previous = {
            let mut seeds = self.recovery_seed.borrow_mut();
            seeds.get_mut(&view).filter(|seed| seed.token == token)
                .and_then(|seed| seed.permit_loss.take())
        };
        drop(previous);
    }

    pub(super) fn settle_recovery(&self, cx: &shell_contract::ipc::CallContext, token: &str, result: Result<(), String>) -> Result<(), String> {
        let view = cx.sender.view;
        let (receipt, permit) = {
            let seeds = self.recovery_seed.borrow();
            let seed = seeds.get(&view).filter(|seed| seed.token == token).ok_or("the recovery receipt is no longer pending")?;
            (seed.receipt.clone(), seed.permit.clone().ok_or("the recovery document was not delivered")?)
        };
        if !self.media_permit(view).is_some_and(|current| current.same_session(&permit)) {
            return Err("the recovered Docs document was replaced".into());
        }
        let context = cx.native.clone().ok_or("the recovery renderer has no native owner")?;
        let owner = self.checked_document_owner(view, shell_contract::ipc::NativeRequestOwner { view, context })
            .map_err(|error| format!("{error:?}"))?;
        let effects = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().ok_or("native recovery effects are unavailable")?;
        effects.settle_recovery_seed(&receipt, &owner, &cx.cancel, result)?;
        let mut seeds = self.recovery_seed.borrow_mut();
        if seeds.get(&view).is_some_and(|seed| seed.receipt.matches(receipt.target())) { seeds.remove(&view); }
        Ok(())
    }

    pub(super) fn take_recovery_seed(&self, view: ViewId) -> Option<DetachedRecoverySeed> {
        let seed = self.recovery_seed.borrow_mut().remove(&view)?;
        let window_open = self.window_opens.remove(view);
        let decision = self.open_decisions.remove(view);
        let untitled = self.pending_untitled.remove(view);
        self.new_blank.borrow_mut().remove(&view);
        Some(DetachedRecoverySeed { seed, _window_open: window_open, _decision: decision, _untitled: untitled })
    }

    pub(super) fn fail_recovery_seed(&self, view: ViewId, message: &str) {
        if let Some(seed) = self.take_recovery_seed(view) { seed.fail(message); }
    }
}

pub(super) fn blank(docs: &Rc<Docs>, view: ViewId, target: &RecoveryOpenTarget, title: String, done: RecoveryOpenDone) {
    if target.view() != view { return failed(done, "the Docs recovery target does not match"); }
    let snapshot = match target.authority_snapshot() { Ok(snapshot) => snapshot, Err(error) => return failed(done, error) };
    if let RecoveryOpenContext::NativeStartup(operation) = &snapshot.context {
        if !matches!(operation.selection(), NativeInvocationSelection::RecoveryBlank {
            surface: shell_contract::Surface::Docs, title: selected,
        } if selected == &title) {
            return failed(done, "the Docs blank recovery does not match the selected native operation");
        }
    }
    let Some(effects) = docs.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>() else {
        return failed(done, "native recovery effects are unavailable");
    };
    let result = effects.commit_recovery_seed_snapshot(target, &snapshot, &mut || {
        if docs.is_torn_down(view) || docs.booted.borrow().contains(&view) {
            return Err("the Docs recovery target is no longer awaiting its first document".into());
        }
        docs.new_blank.borrow_mut().insert(view);
        Ok(())
    });
    match result {
        Ok(receipt) => { docs.retain_recovery_seed(view, receipt, true, None, None); done(Ok(())); }
        Err(error) => failed(done, error),
    }
}

fn error(message: impl Into<String>) -> NodeError { NodeError { code: None, message: message.into() } }
fn failed(done: RecoveryOpenDone, message: impl Into<String>) { done(Err(Failure(message.into()))); }

struct Completion(Option<RecoveryOpenDone>);

impl Completion {
    fn finish(mut self, result: Result<(), String>) {
        if let Some(done) = self.0.take() { done(result.map_err(Failure)); }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            done(Err(Failure("the Docs recovery worker was discarded".into())));
        }
    }
}

fn gate(docs: &Docs, target: &RecoveryOpenTarget, snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
    let effects = docs.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().ok_or("native recovery effects are unavailable")?;
    effects.with_recovery_seed_snapshot(target, snapshot, effect)
}

fn validate_copy(target: &RecoveryOpenTarget, snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot, view: ViewId, request: &RecoveryOpenUntitledRequest) -> Result<(), String> {
    if target.view() != view || request.copy.kind() != "docs" { return Err("the Docs recovery target does not match".into()); }
    if let RecoveryOpenContext::NativeStartup(operation) = &snapshot.context {
        let matches = match operation.selection() {
            NativeInvocationSelection::RecoveryCopy { surface: shell_contract::Surface::Docs, copy, title, save_as_default } => {
                let identity = match (copy, &request.copy) {
                    (NativeRecoveryCopy::File { path }, RecoveryIdentity::File(file)) => path == std::path::Path::new(&file.file_path),
                    (NativeRecoveryCopy::Untitled { id, ext }, RecoveryIdentity::Untitled(file)) => id == &file.untitled_id && ext == &file.ext,
                    _ => false,
                };
                identity && title == &request.title && save_as_default.as_deref() == request.save_as_default_path.as_deref().map(std::path::Path::new)
            }
            _ => false,
        };
        if !matches { return Err("the Docs recovery does not match the selected native operation".into()); }
    }
    Ok(())
}

fn admission(docs: &Rc<Docs>, target: RecoveryOpenTarget, snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot, expected: Option<main_core::packages::electron_utils::recovery_store::RecoveryUntitledIdentity>) -> Result<Admission, String> {
    let lease = docs.media_views.borrow().get(&target.view()).map(|slot| slot.lease.clone()).ok_or("the Docs recovery actor is unavailable")?;
    let permit = lease.current().ok_or("the Docs recovery media session is unavailable")?;
    let weak = Rc::downgrade(docs);
    Ok(super::owned_write::core_admission(docs.services.clone(), Rc::new(move |effect| {
        let docs = weak.upgrade().ok_or_else(|| error("the Docs recovery actor was released"))?;
        if !docs.media_views.borrow().get(&target.view()).is_some_and(|current| current.same_view(&lease))
            || !lease.current().is_some_and(|current| current.same_session(&permit)) {
            return Err(error("the Docs recovery actor was replaced"));
        }
        let mut operation = Some(effect);
        let mut output = None;
        gate(&docs, &target, &snapshot, &mut || {
            if docs.is_torn_down(target.view()) || !docs.book.borrow().matches_untitled(target.view(), expected.as_ref()) {
                return Err("the Docs recovery target changed".into());
            }
            output = Some(operation.take().ok_or("the recovery effect was admitted twice")?());
            Ok(())
        }).map_err(error)?;
        output.ok_or_else(|| error("the recovery effect was not admitted"))
    })))
}

pub(super) fn file(docs: &Rc<Docs>, view: ViewId, request: &RecoveryOpenFileRequest, target: &RecoveryOpenTarget, done: RecoveryOpenDone) {
    if target.view() != view { return failed(done, "the Docs recovery target does not match"); }
    let snapshot = match target.authority_snapshot() { Ok(snapshot) => snapshot, Err(error) => return failed(done, error) };
    if let RecoveryOpenContext::NativeStartup(operation) = &snapshot.context {
        let valid = matches!(operation.selection(), NativeInvocationSelection::RecoveryFile {
            surface: shell_contract::Surface::Docs, path, decision,
        } if path == std::path::Path::new(&request.file_path)
            && matches!((decision, request.decision),
                (shell_contract::ipc::NativeRecoveryDecision::Restore, main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision::Restore)
                | (shell_contract::ipc::NativeRecoveryDecision::Original, main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision::Original)));
        if !valid { return failed(done, "the Docs recovery does not match the selected native operation"); }
    }
    if main_core::docs::doc_format::doc_format_of_path(&request.file_path).is_none() {
        return failed(done, "the Docs recovery file format is unsupported");
    }
    let Some(captured_lease) = docs.media_views.borrow().get(&view).map(|slot| slot.lease.clone()) else {
        return failed(done, "the Docs recovery actor is unavailable");
    };
    let Some(captured_permit) = captured_lease.current() else {
        return failed(done, "the Docs recovery media session is unavailable");
    };
    let expected = docs.book.borrow().untitled_of(view);
    let admit = match admission(docs, target.clone(), snapshot.clone(), expected.clone()) {
        Ok(admit) => admit, Err(error) => return failed(done, error),
    };
    let path = request.file_path.clone();
    let decision = request.decision;
    let worker_path = path.clone();
    let identity = super::recovery::file_identity(&path);
    let root = docs.user_data(&[]);
    let completed = docs.clone();
    let target = target.clone();
    let completion = Completion(Some(done));
    kit::on_worker(&docs.services, move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let source_file = Arc::new(owned::admitted(&admit, move || std::fs::File::open(worker_path))
            .map_err(|error| error.message)?
            .map_err(|error| error.to_string())?);
        let prepared = match decision {
            main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision::Original =>
                super::open::PreparedRecoveryCopy::Original,
            main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision::Restore => {
                let pinned = owned::admitted(&admit, move || RecoveryPinnedRead::open(&root, &identity))
                    .map_err(|error| error.message)?;
                super::open::PreparedRecoveryCopy::Restore(pinned.ok().and_then(|pinned| pinned.read().ok()))
            }
        };
        Ok((prepared, source_file))
        })).unwrap_or_else(|_| Err("the Docs recovery file worker failed".into()))
    }, move |result| {
        let (prepared, source_file) = match result { Ok(prepared) => prepared, Err(error) => return completion.finish(Err(error)) };
        if !completed.media_views.borrow().get(&view).is_some_and(|current| current.same_view(&captured_lease))
            || !captured_lease.current().is_some_and(|current| current.same_session(&captured_permit)) {
            return completion.finish(Err("the Docs recovery actor was replaced".into()));
        }
        let Some(effects) = completed.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>() else {
            return completion.finish(Err("native recovery effects are unavailable".into()));
        };
        let result = effects.commit_recovery_seed_snapshot(&target, &snapshot, &mut || {
            if completed.is_torn_down(view) || completed.booted.borrow().contains(&view) { return Err("the Docs recovery target is no longer awaiting its first document".into()); }
            if !completed.book.borrow().matches_untitled(view, expected.as_ref()) {
                return Err("the Docs recovery target changed".into());
            }
            completed.open_decisions.insert(view, (path.clone(), decision));
            completed.window_opens.insert(view, path.clone());
            Ok(())
        });
        match result {
            Ok(receipt) => { completed.retain_recovery_seed(view, receipt, false, Some(prepared), Some((path, source_file))); completion.finish(Ok(())); }
            Err(error) => completion.finish(Err(error)),
        }
    });
}

pub(super) fn untitled(docs: &Rc<Docs>, view: ViewId, request: &RecoveryOpenUntitledRequest, target: &RecoveryOpenTarget, done: RecoveryOpenDone) {
    let snapshot = match target.authority_snapshot() { Ok(snapshot) => snapshot, Err(error) => return failed(done, error) };
    if let Err(error) = validate_copy(target, &snapshot, view, request) { return failed(done, error); }
    if docs.booted.borrow().contains(&view) || docs.is_torn_down(view) { return failed(done, "the Docs recovery target is no longer awaiting its first document"); }
    let Some(captured_lease) = docs.media_views.borrow().get(&view).map(|slot| slot.lease.clone()) else {
        return failed(done, "the Docs recovery actor is unavailable");
    };
    let Some(captured_permit) = captured_lease.current() else {
        return failed(done, "the Docs recovery media session is unavailable");
    };
    let expected = docs.book.borrow().untitled_of(view);
    let admit = match admission(docs, target.clone(), snapshot.clone(), expected.clone()) { Ok(admit) => admit, Err(error) => return failed(done, error) };
    let copy = request.copy.clone();
    let title = request.title.clone();
    let suggested = request.save_as_default_path.clone();
    let root = docs.user_data(&[]);
    let shell = docs.shell.clone();
    let shared = docs.recovery_shared.clone();
    let ext = match &copy { RecoveryIdentity::File(file) => recovery_ext_for(&file.file_path), RecoveryIdentity::Untitled(file) if file.ext == ".odt" => ".odt", _ => ".docx" };
    let done_docs = docs.clone();
    let done_target = target.clone();
    let worker_expected = expected.clone();
    let worker_target = target.clone();
    let worker_snapshot = snapshot.clone();
    let completion = Completion(Some(done));
    kit::on_worker(&docs.services, move || std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<_, String> {
        let source = recovery_copy_path(&root, &copy).map_err(|error| error.message)?;
        let file_path = source.clone();
        let mut file = owned::admitted(&admit, move || std::fs::File::open(file_path))
            .map_err(|error| error.message)?.map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|error| error.to_string())?;
        let password = match super::passwords::docx_open_kind(&bytes) {
            super::passwords::OpenKind::Plain => None,
            super::passwords::OpenKind::Damaged => return Err("the recovered Word document is damaged".into()),
            super::passwords::OpenKind::Encrypted => {
                let (plain, password) = shell.unlock_recovery_prepared(worker_target, worker_snapshot, title.clone(), &bytes)?
                    .ok_or("the recovered Word document password was cancelled")?;
                bytes = plain;
                Some(password)
            }
        };
        let identity = match &copy {
            RecoveryIdentity::Untitled(identity) => identity.clone(),
            RecoveryIdentity::File(_) => {
                let plan = RecoveryAdoptPlan::prepare(&root, "docs", &source, &RecoveryAdoptOptions {
                    title: title.clone(), ext: ext.into(), state: Some(RecoveryCopyState::Undecided), original_path: None,
                }, &|| shell.random_uuid(), &|| shell.now_ms()).map_err(|error| error.message)?
                    .ok_or("the recovery copy cannot be adopted")?;
                let operations = admit.clone();
                let metadata_admit = admit.clone();
                plan.publish_via_owned(&mut |effect| owned::admitted(&operations, effect), &mut |path, value| {
                    let rejected = Arc::new(AtomicBool::new(false));
                    let marker = rejected.clone();
                    let inner = metadata_admit.clone();
                    let checked: Admission = Arc::new(move |effect| inner(effect).map_err(|error| { marker.store(true, Ordering::Release); error }));
                    let result = owned::write_json(path, value, checked);
                    if rejected.load(Ordering::Acquire) { result.map(|()| true) } else { Ok(result.is_ok()) }
                }).map_err(|error| error.message)?.ok_or("the recovery copy could not be adopted")?
            }
        };
        let identity_value = RecoveryIdentity::Untitled(identity.clone());
        if let Some(previous) = worker_expected.as_ref().filter(|previous| previous.untitled_id != identity.untitled_id) {
            let previous = RecoveryIdentity::Untitled(previous.clone());
            let shared = shared.clone(); let shell = shell.clone(); let root = root.clone();
            let plan = owned::admitted(&admit, move || {
                let guard = shared.try_lock().map_err(|_| error("the recovery store is busy"))?;
                let dir = || root.clone(); let now = || shell.now_ms(); let uuid = || shell.random_uuid();
                let store = create_recovery_store(RecoveryStoreOptions { user_data_dir: &dir, now_ms: &now, random_uuid: &uuid }, &guard);
                store.prepare_delete(&previous, RecoveryDeleteReason::ClosedClean)
            }).map_err(|error| error.message)?.map_err(|error| error.message)?;
            if let Some(plan) = plan {
                plan.remove_owned(&mut |effect| owned::admitted(&admit, effect)).map_err(|error| error.message)?;
            }
        }
        let identity_for_clear = identity_value.clone();
        let clear_shared = shared.clone(); let clear_shell = shell.clone(); let clear_root = root.clone();
        owned::admitted(&admit, move || {
            let guard = clear_shared.try_lock().map_err(|_| error("the recovery store is busy"))?;
            let dir = || clear_root.clone(); let now = || clear_shell.now_ms(); let uuid = || clear_shell.random_uuid();
            let store = create_recovery_store(RecoveryStoreOptions { user_data_dir: &dir, now_ms: &now, random_uuid: &uuid }, &guard);
            store.clear_discarded(&identity_for_clear)
        }).map_err(|error| error.message)?.map_err(|error| error.message)?;
        let actual_path = recovery_copy_path(&root, &identity_value).map_err(|error| error.message)?;
        if let Some(plan) = RecoveryStore::prepare_copy_state_from_path(&identity_value, &actual_path, RecoveryCopyState::Undecided) {
            plan.write_via(&mut |path, value| {
                let rejected = Arc::new(AtomicBool::new(false)); let marker = rejected.clone(); let inner = admit.clone();
                let checked: Admission = Arc::new(move |effect| inner(effect).map_err(|error| { marker.store(true, Ordering::Release); error }));
                let result = owned::write_json(path, value, checked);
                if rejected.load(Ordering::Acquire) { result.map(|()| true) } else { Ok(result.is_ok()) }
            }).map_err(|error| error.message)?;
        }
        let info = super::recovery::with_store(&shared, &root, &shell, |store| store.copy_info(&identity_value).ok().flatten());
        let fields = super::open::untitled_recovery_fields(&identity, &title, ext, suggested, info);
        Ok((fields, bytes, identity, password))
    })).unwrap_or_else(|_| Err("the Docs recovery copy worker failed".into())), move |result| {
        if !done_docs.media_views.borrow().get(&view).is_some_and(|current| current.same_view(&captured_lease))
            || !captured_lease.current().is_some_and(|current| current.same_session(&captured_permit)) {
            return completion.finish(Err("the Docs recovery actor was replaced".into()));
        }
        let (fields, bytes, identity, password) = match result { Ok(result) => result, Err(error) => return completion.finish(Err(error)) };
        let mut output = Some((fields, bytes, identity, password));
        let Some(effects) = done_docs.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>() else {
            return completion.finish(Err("native recovery effects are unavailable".into()));
        };
        let result = effects.commit_recovery_seed_snapshot(&done_target, &snapshot, &mut || {
            if done_docs.is_torn_down(view) || done_docs.booted.borrow().contains(&view) { return Err("the Docs recovery target is no longer awaiting its first document".into()); }
            let (fields, bytes, identity, password) = output.take().ok_or("the recovery seed was admitted twice")?;
            if !done_docs.book.borrow_mut().replace_untitled_if_current(view, expected.as_ref(), identity) { return Err("the Docs recovery identity changed".into()); }
            if let Some(password) = password { done_docs.passwords.borrow_mut().set(view, None, Some(&password)); }
            done_docs.pending_untitled.insert(view, Some((fields, bytes)));
            Ok(())
        });
        match result {
            Ok(receipt) => { done_docs.retain_recovery_seed(view, receipt, false, None, None); completion.finish(Ok(())); }
            Err(error) => completion.finish(Err(error)),
        }
    });
}
