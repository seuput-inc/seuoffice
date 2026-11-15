use jscompat::{JsObject, JsonValue};
use jscompat::node_path::host::{basename, join};
use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::dialog_memory::{FallbackDir, prepare_dialog_options, remember_save_path, save_as_suggestion};
use main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason;
use main_core::packages::electron_utils::save_conflict::{SaveDiskState, is_modified_since, record_disk_state};
use main_core::slides::save_as_formats::{resolve_slides_save_as_pick, slides_save_as_filters, slides_save_as_suggestion};
use shell_contract::editors::{SaveMode, Then};
use shell_contract::hosts::MessageBox;
use shell_contract::ipc::{CallContext, NativeRequestOwner, Responder};
use shell_contract::runtime::{CancelToken, PathName};
use shell_contract::{IpcError, IpcValue, ViewId};
use std::sync::{Arc, mpsc};

use crate::owned_save::OwnedSave;
use main_core::packages::electron_utils::atomic_write::owned::{self as owned_write, Admission};
use crate::value::{Args, as_bool, obj, string, thrown};
use crate::slides::dialog::{filter, filter_of, jstr, options};
use crate::slides::state::App;
use super::{SaveRevision, SaveScope};

#[path = "save_publication.rs"]
mod publication;
#[path = "owned_project_save.rs"]
mod project;
pub(in crate::slides) use project::save_into_project;

pub(in crate::slides) struct PendingWrite {
    scope: SaveScope,
    previous: String,
    path: String,
    meta_rev: Option<u64>,
    op_seq: Option<u64>,
    remaining: usize,
    failure: Option<IpcError>,
    done: Box<dyn FnOnce(&App, Result<(), IpcError>)>,
}

impl App {
    pub(in crate::slides) fn capture_save_owner(&self, view: ViewId, mut native: NativeRequestOwner) -> Result<NativeRequestOwner, IpcError> {
        let state = self.state.borrow();
        if state.save_lifetime(view).is_none() {
            let slot = state.saved_loads.get(&view).ok_or_else(|| thrown("The presentation document was released"))?;
            let lease = &slot.initial_document;
            if native.view != view || !slot.target.reservation().matches(view, slot.target.created())
                || !slot.target.browser().reservation().same_reservation(slot.target.reservation())
                || !native.context.renderer().is_some_and(|renderer| renderer.same_generation(slot.target.created()) && renderer.is_active())
                || !lease.is_active() || native.context.document_lease().is_some_and(|captured| !captured.same_generation(lease)) {
                return Err(thrown("The presentation owner belongs to another document"));
            }
            if native.context.document_lease().is_none() { native.context = native.context.with_document_lease(lease.clone()); }
            return Ok(native);
        }
        let lease = state.save_lifetime(view).ok_or_else(|| thrown("The presentation document was released"))?;
        let owner = state.owner_of(view).ok_or_else(|| thrown("The presentation owner was released"))?;
        if native.view != view || !native.context.renderer().zip(owner.context.renderer()).is_some_and(|(actual, expected)| actual.same_generation(expected) && actual.is_active())
            || native.context.document_lease().is_some_and(|captured| !captured.same_generation(lease)) {
            return Err(thrown("The presentation owner belongs to another document"));
        }
        if native.context.document_lease().is_none() { native.context = native.context.with_document_lease(lease.clone()); }
        Ok(native)
    }

    pub(in crate::slides) fn rename_path_owned(&self, view: ViewId, old: &str, new: &str, origin: &NativeRequestOwner, destination: &NativeRequestOwner, cancel: &CancelToken, pending: &CancelToken, then: Then<()>) {
        self.rename_path_many_owned(view, old, new, origin, destination, &[], cancel, pending, then);
    }

    pub(in crate::slides) fn rename_path_many_owned(&self, view: ViewId, old: &str, new: &str, origin: &NativeRequestOwner, destination: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, then: Then<()>) {
        self.rename_path_many_owned_extra(view, old, new, origin, destination, participants, cancel, pending, &[], then);
    }

    pub(in crate::slides) fn rename_path_many_owned_extra(&self, view: ViewId, old: &str, new: &str, origin: &NativeRequestOwner, destination: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[CancelToken], then: Then<()>) {
        let destination = match self.capture_save_owner(view, destination.clone()) { Ok(owner) => owner, Err(error) => return then(Err(error)) };
        let effects = match self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() { Some(host) => host, None => return then(Err(thrown("The native effect host is unavailable"))) };
        let document = match destination.context.document_lease().cloned() { Some(lease) => lease, None => return then(Err(thrown("The presentation document lease is unavailable"))) };
        let request = OwnedSave { authority: crate::owned_save::SaveAuthority::Renderer(origin.clone()), document, destination, cancel: cancel.clone(), pending: pending.clone(), additional: additional.to_vec(), callback_owner: None, prior_callback_owners: Vec::new(), revision: None, revision_snapshot: None, effects };
        let result = request.effect_many(participants, || {
            self.apply_renamed_path(view, old, new, pending)
        });
        let result = result.and_then(|()| {
            let targets = participants.iter().cloned().chain(std::iter::once(request.destination.clone())).collect::<Vec<_>>();
            let index = targets.len() - 1;
            request.emit_many(&targets, index, "slides:renamed", vec![string(new)])
        });
        then(result);
    }

    pub(in crate::slides) fn rename_path_recovery(&self, view: ViewId, old: &str, new: &str,
        receipt: &shell_contract::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner,
        destination: &NativeRequestOwner, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, then: Then<()>) {
        if destination.view != view { return then(Err(thrown("The presentation destination does not match the view"))); }
        let had_session = self.state.borrow().has_session(view);
        let destination = if had_session {
            match self.capture_save_owner(view, destination.clone()) { Ok(owner) => owner, Err(error) => return then(Err(error)) }
        } else {
            destination.clone()
        };
        let snapshot = match receipt.target().authority_snapshot() { Ok(snapshot) => snapshot, Err(error) => return then(Err(thrown(error))) };
        let effects = match self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() { Some(host) => host, None => return then(Err(thrown("The native effect host is unavailable"))) };
        let mut targets = participants.to_vec();
        let destination_index = targets.len();
        targets.push(destination);
        let mut applied = None;
        let result = effects.with_recovery_open_snapshot(receipt, &snapshot, receiver, &targets, cancel, pending, &mut || {
            applied = Some(if had_session {
                self.apply_renamed_path(view, old, new, pending)
            } else if self.state.borrow().has_session(view) {
                Err(thrown("The presentation document changed during recovery rename"))
            } else {
                Ok(())
            });
            Ok(())
        }).map_err(thrown).and_then(|()| applied.ok_or_else(|| thrown("The presentation rename was not admitted"))?);
        let result = result.and_then(|()| effects.emit_recovery_open_snapshot(receipt, &snapshot, receiver, &targets, destination_index, cancel, pending, "slides:renamed", vec![string(new)]).map_err(thrown));
        then(result);
    }

    pub(in crate::slides) fn rename_path_existing(&self, view: ViewId, old: &str, new: &str,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding, destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[CancelToken], then: Then<()>) {
        let destination = match self.capture_save_owner(view, destination.clone()) { Ok(owner) => owner, Err(error) => return then(Err(error)) };
        let effects = match self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() { Some(host) => host, None => return then(Err(thrown("The native effect host is unavailable"))) };
        let mut targets = participants.to_vec();
        let index = targets.len();
        targets.push(destination);
        let mut applied = None;
        let result = effects.with_existing_document_save_effect_extra(binding, &targets, cancel, pending, &additional.iter().collect::<Vec<_>>(), &mut || {
            applied = Some(self.apply_renamed_path(view, old, new, pending));
            Ok(())
        }).map_err(thrown).and_then(|()| applied.ok_or_else(|| thrown("The presentation rename was not admitted"))?);
        let result = result.and_then(|()| effects.emit_existing_document_save_many_extra(binding, &targets, index, cancel, pending, &additional.iter().collect::<Vec<_>>(), "slides:renamed", vec![string(new)]).map_err(thrown));
        then(result);
    }

    pub(in crate::slides) fn rename_path_revision(&self, view: ViewId, old: &str, new: &str,
        authority: &shell_contract::ipc::NativeSaveRevisionAuthority, expected_path: &std::path::Path, destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, additional: &[CancelToken], then: Then<()>) {
        let snapshot = match authority.snapshot() { Ok(snapshot) => snapshot, Err(error) => return then(Err(thrown(error))) };
        let destination = match self.capture_save_owner(view, destination.clone()) { Ok(owner) => owner, Err(error) => return then(Err(error)) };
        let effects = match self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() { Some(host) => host, None => return then(Err(thrown("The native effect host is unavailable"))) };
        let mut targets = participants.to_vec();
        let index = targets.len();
        targets.push(destination);
        let extra = additional.iter().collect::<Vec<_>>();
        let mut applied = None;
        let result = effects.with_native_save_revision_snapshot_effect(&snapshot, &targets, cancel, pending, &extra, &mut || {
            if snapshot.selection().path() != expected_path { return Err("The native save destination changed".into()); }
            applied = Some(self.apply_renamed_path(view, old, new, pending));
            Ok(())
        }).map_err(thrown).and_then(|()| applied.ok_or_else(|| thrown("The presentation rename was not admitted"))?);
        let result = result.and_then(|()| {
            if snapshot.selection().path() != expected_path { return Err(thrown("The native save destination changed")); }
            effects.emit_native_save_revision_snapshot(&snapshot, &targets, index, cancel, pending, &extra, "slides:renamed", vec![string(new)]).map_err(thrown)
        });
        then(result);
    }

    fn apply_renamed_path(&self, view: ViewId, old: &str, new: &str, pending: &CancelToken) -> Result<(), IpcError> {
        let mut state = self.state.borrow_mut();
        let slot = state.slot_of(view).ok_or_else(|| thrown("The presentation document was released"))?;
        let s = state.slot_mut(slot).ok_or_else(|| thrown("The presentation document was released"))?;
        if s.path() == old { s.core.path = new.to_owned(); }
        let session = s.id; let seq = s.core.op_seq;
        for write in state.owned_writes.values_mut().filter(|write| write.scope.session == session && write.scope.request.pending.same_token(pending) && write.op_seq == seq) {
            if write.previous == old { write.previous = new.to_owned(); }
            if write.path == old { write.path = new.to_owned(); }
        }
        Ok(())
    }
    fn pick_save_owned(&self, scope: &SaveScope, opts: JsObject<JsonValue>, done: Box<dyn FnOnce(&App, Result<Option<crate::owned_save::SaveDestination>, IpcError>) + Send>) {
        let prepared = (|| {
            scope.check(self)?;
            let fallback = self.drafts_dir()?;
            let memory = self.shared.dialog_memory.lock().map_err(|_| thrown("The dialog memory is unavailable"))?;
            Ok(crate::host::file_dialog(&prepare_dialog_options(&memory, &opts, Some(&FallbackDir::Dir(&fallback)))))
        })();
        let opts = match prepared { Ok(opts) => opts, Err(error) => return done(self, Err(error)) };
        let services = self.services.clone(); let index = self.registry_index;
        let scope = scope.clone(); let request = scope.request.clone();
        request.save_dialog(&self.dialogs, self.shell.dialog_parent(), opts,
            Box::new(move |answer| crate::slides::post_to(&services, index, move |app| {
                let answer = answer.map_err(thrown).and_then(|answer| {
                    scope.check(app)?;
                    let answer = answer.filter(|destination| !destination.path().as_os_str().is_empty());
                    let path = answer.as_ref().map(|destination| crate::host::path_string(destination.path().to_path_buf()));
                    if let Some(path) = &path { scope.effect(app, || {
                        let mut memory = app.shared.dialog_memory.lock().map_err(|_| thrown("The dialog memory is unavailable"))?;
                        remember_save_path(&mut memory, path); Ok(())
                    })?; }
                    Ok(answer)
                });
                done(app, answer);
            })));
    }

    pub(in crate::slides) fn save_to_path_owned(&self, view: ViewId, path: &str, origin: NativeRequestOwner, cancel: CancelToken, done: Then<()>) {
        let scope = match SaveScope::capture(self, view, origin, cancel) { Ok(scope) => scope, Err(error) => return done(Err(error)) };
        self.write_owned_deck(scope, path.to_owned(), true, Box::new(move |_, result| done(result)));
    }

    pub(in crate::slides) fn save_to_path_native(&self, view: ViewId, path: &std::path::Path, binding: shell_contract::ipc::NativeExistingDocumentBinding, cancel: CancelToken, pending: CancelToken, done: Then<()>) {
        let destination = match shell_contract::ipc::NativeSaveDestination::from_exact_selection(binding.clone()) {
            Ok(destination) if destination.path() == path => destination,
            Ok(_) => return done(Err(thrown("The native save path does not match its selection"))),
            Err(error) => return done(Err(thrown(error))),
        };
        let mut scope = match SaveScope::capture_native(self, view, binding, cancel, CancelToken::default()) { Ok(scope) => scope, Err(error) => return done(Err(error)) };
        scope.request.additional.push(pending);
        if let Err(error) = scope.check(self) { return done(Err(error)); }
        if let Err(error) = scope.request.retain_native_selection(&destination) { return done(Err(error)); }
        scope.native_destination = Some(destination);
        self.write_owned_deck(scope, crate::host::path_string(path.to_path_buf()), true, Box::new(move |_, result| done(result)));
    }

    fn write_owned_deck(&self, scope: SaveScope, path: String, make_parent: bool, done: Box<dyn FnOnce(&App, Result<(), IpcError>)>) {
        let prepare = (|| {
            if matches!(&scope.request.authority, crate::owned_save::SaveAuthority::Native(_)) && scope.native_destination.is_none() {
                return Err(thrown("The native save destination was not retained"));
            }
            scope.check(self)?;
            let state = self.state.borrow(); let s = state.slot(scope.slot).ok_or_else(|| thrown("no file open"))?;
            if s.core.ai_run { return Err(thrown("An AI request is editing this deck; saving waits for your review")); }
            Ok((s.path().to_owned(), s.core.meta_rev, s.core.op_seq, s.password.clone()))
        })();
        let (previous, meta_rev, op_seq, password) = match prepare { Ok(value) => value, Err(error) => return done(self, Err(error)) };
        let bytes = self.engine.save_bytes(scope.deck).map_err(|error| thrown(error.0)).and_then(|bytes| match password {
            Some(password) => self.engine.encrypt_ooxml(&bytes, &password).map_err(|error| thrown(error.0)), None => Ok(bytes),
        });
        let bytes = match bytes { Ok(bytes) => bytes, Err(error) => return done(self, Err(error)) };
        if let Err(error) = scope.check(self) { return done(self, Err(error)); }
        let id = self.shell.random_uuid();
        self.state.borrow_mut().owned_writes.insert(id.clone(), PendingWrite { scope, previous, path: path.clone(), meta_rev, op_seq, remaining: 0, failure: None, done });
        let services = self.services.clone(); let index = self.registry_index;
        let admit = self.write_admission(&id);
        self.services.workers.spawn(Box::new(move || {
            let result = (|| {
                if make_parent { owned_write::create_parent(&path, &admit).map_err(|error| thrown(error.message))?; }
                owned_write::write_file(&path, &bytes, admit).map_err(|error| thrown(error.message))?;
                Ok(record_disk_state(&path))
            })();
            crate::slides::post_to(&services, index, move |app| app.deck_written(&id, result));
        }));
    }

    fn write_admission(&self, id: &str) -> Admission {
        let core = self.services.core.clone(); let index = self.registry_index; let id = id.to_owned();
        Arc::new(move |effect| {
            let (send, receive) = mpsc::sync_channel(1); let id = id.clone();
            core.post(Box::new(move || {
                let result = crate::slides::with_app(index, |app| {
                    let (scope, previous, path, meta_rev, op_seq) = {
                        let state = app.state.borrow(); let write = state.owned_writes.get(&id).ok_or_else(|| thrown("The save request was released"))?;
                        (write.scope.clone(), write.previous.clone(), write.path.clone(), write.meta_rev, write.op_seq)
                    };
                    let action = || {
                        scope.check_document(app)?;
                        let state = app.state.borrow(); let s = state.slot(scope.slot).ok_or_else(|| thrown("no file open"))?;
                        if s.core.ai_run || s.path() != previous || s.core.meta_rev != meta_rev || s.core.op_seq != op_seq { return Err(thrown("The presentation changed while it was being saved")); }
                        Ok(effect())
                    };
                    match &scope.native_destination {
                        Some(destination) => scope.request.effect_destination(destination, std::path::Path::new(&path), action),
                        None => scope.effect(app, action),
                    }
                }).unwrap_or_else(|| Err(thrown("The Slides application was released")));
                let _ = send.send(result.map_err(|error| NodeError { code: None, message: error.message }));
            }));
            receive.recv().map_err(|_| NodeError { code: None, message: "The save effect dispatcher was released".into() })?
        })
    }

    fn deck_written(&self, id: &str, result: Result<Option<SaveDiskState>, IpcError>) {
        let record = self.state.borrow_mut().owned_writes.remove(id); let Some(mut write) = record else { return };
        let result = result.and_then(|disk| {
            write.scope.check(self)?;
            {
                let state = self.state.borrow(); let s = state.slot(write.scope.slot).ok_or_else(|| thrown("no file open"))?;
                if s.core.meta_rev != write.meta_rev || s.core.op_seq != write.op_seq || s.path() != write.previous { return Err(thrown("The presentation changed while it was being saved")); }
            }
            let request = &write.scope.request;
            match &write.scope.native_destination {
                Some(destination) => self.engine.commit_saved_existing_document(write.scope.deck, &write.scope.render_owner, &request.document, destination, &request.participants(&[]), &request.cancel, &request.pending, &request.additional, &request.effects),
                None => self.engine.commit_saved_owned(write.scope.deck, request.authority.renderer()?, &request.destination, &request.participants(&[]), &request.cancel, &request.pending, &request.additional, &request.effects),
            }.map_err(|error| thrown(error.0))?;
            write.scope.effect(self, || {
                let mut state = self.state.borrow_mut(); let s = state.slot_mut(write.scope.slot).ok_or_else(|| thrown("no file open"))?;
                self.follow_template(s, &write.previous, &write.path);
                s.core.path = write.path.clone(); s.disk_state = disk; s.ask_path_on_first_save = false;
                if s.core.meta_rev == write.meta_rev { s.core.meta_dirty = false; }
                Ok(())
            })
        });
        if let Err(error) = result { return (write.done)(self, Err(error)); }
        let identities = {
            let state = self.state.borrow(); let s = state.slot(write.scope.slot);
            let mut ids = vec![crate::slides::recovery::file_recovery_id(&write.path)];
            if !write.previous.is_empty() && write.previous != write.path { ids.insert(0, crate::slides::recovery::file_recovery_id(&write.previous)); }
            if let Some(identity) = s.and_then(|s| s.recovery_id.clone()) { ids.push(identity); } ids
        };
        let revision = match write.scope.revision(self) { Ok(revision) => revision, Err(error) => return (write.done)(self, Err(error)) };
        let admission = write.scope.revision_admission(self, revision.clone());
        let request = write.scope.request.clone(); let recovery = self.recovery_state.clone();
        let user_data = crate::host::path_string(self.services.paths.path(PathName::UserData));
        let saved_path = write.path.clone();
        let shell = self.shell.clone(); let services = self.services.clone(); let index = self.registry_index; let id = id.to_owned();
        write.remaining = write.scope.recipients.len();
        self.state.borrow_mut().owned_writes.insert(id.clone(), write);
        self.services.workers.spawn(Box::new(move || {
            let result = crate::owned_recovery::delete_copies(&recovery, &user_data, &identities, RecoveryDeleteReason::Saved, &admission).map_err(|error| thrown(error.message)).and_then(|outcomes| {
                if !shell.is_headless() { update_recent(&request, &admission, &user_data, &saved_path)?; }
                Ok(outcomes)
            });
            crate::slides::post_to(&services, index, move |app| app.publish_written(&id, revision, result));
        }));
    }

    fn publish_written(&self, id: &str, revision: SaveRevision, result: Result<Vec<crate::owned_recovery::DeletedCopy>, IpcError>) {
        let outcomes = match result { Ok(outcomes) => outcomes, Err(error) => return self.publication_done(id, Err(error), true) };
        let scope = {
            let state = self.state.borrow(); let Some(write) = state.owned_writes.get(id) else { return };
            write.scope.clone()
        };
        let clear = scope.effect_at_revision(self, &revision, || {
            let refused = outcomes.iter().any(|outcome| outcome.refused);
            for outcome in &outcomes { self.backoff.borrow_mut().remove(&outcome.path); }
            let mut state = self.state.borrow_mut(); let s = state.slot_mut(scope.slot).ok_or_else(|| thrown("no file open"))?;
            if !refused { s.copy_mark.set(None); } s.recovery_id = None; Ok::<_, IpcError>(())
        }).and_then(|result| result);
        if let Err(error) = clear { return self.publication_done(id, Err(error), true); }
        if scope.recipients.is_empty() { return self.publication_done(id, Err(thrown("The presentation has no attached view")), true); }
        self.publish_next(id);
    }


    fn publication_done(&self, id: &str, result: Result<(), IpcError>, finish: bool) {
        let finish = finish || result.is_err();
        let completed = {
            let mut state = self.state.borrow_mut(); let Some(write) = state.owned_writes.get_mut(id) else { return };
            if let Err(error) = result { write.failure.get_or_insert(error); }
            if !finish { write.remaining = write.remaining.saturating_sub(1); }
            if finish || write.remaining == 0 { state.owned_writes.remove(id) } else { None }
        };
        if let Some(write) = completed {
            let revision = SaveRevision { path: write.path.clone(), meta_rev: write.meta_rev, op_seq: write.op_seq };
            let result = write.failure.map_or_else(|| write.scope.effect_at_revision(self, &revision, || ()), Err);
            (write.done)(self, result);
        } else { self.publish_next(id); }
    }
}


fn update_recent(request: &OwnedSave, admission: &Admission, user_data: &str, path: &str) -> Result<(), IpcError> {
    request.check()?;
    let target = std::path::Path::new(user_data).join("slides-recent.json");
    super::super::recent::push_owned(target.to_string_lossy().into_owned(), path.to_owned(), admission.clone()).map_err(thrown)
}

pub(in crate::slides) fn save(app: &App, cx: &CallContext, args: Args, responder: Responder) {
    if !app.state.borrow().has_session(cx.sender.view) { return responder.reply(Ok(obj([("ok", IpcValue::Bool(false)), ("error", string("no file open"))]))); }
    let id = match args.str(1).map(str::to_owned).map(Ok).unwrap_or_else(|| app.begin_renderer_save(cx, SaveMode::Save)) { Ok(id) => id, Err(error) => return responder.reply(Err(error)) };
    start(app, cx, args, responder, id, SaveMode::Save);
}

pub(in crate::slides) fn save_as(app: &App, cx: &CallContext, args: Args, responder: Responder) {
    if !app.state.borrow().has_session(cx.sender.view) { return responder.reply(Ok(obj([("ok", IpcValue::Bool(false)), ("error", string("no file open"))]))); }
    let id = match args.str(1).map(str::to_owned).map(Ok).unwrap_or_else(|| app.begin_renderer_save(cx, SaveMode::SaveAs)) { Ok(id) => id, Err(error) => return responder.reply(Err(error)) };
    start(app, cx, args, responder, id, SaveMode::SaveAs);
}

fn start(app: &App, cx: &CallContext, args: Args, responder: Responder, id: String, mode: SaveMode) {
    let (scope, path) = match app.owned_save_for(cx, &id, Some(mode)) { Ok(value) => value, Err(error) => return failed(app, &id, responder, error) };
    let automatic = mode == SaveMode::Save && as_bool(args.get(0)) == Some(true);
    if app.state.borrow().slot(scope.slot).is_some_and(|session| session.core.ai_run) {
        return unwritten(app, &scope, &id, obj([("ok", IpcValue::Bool(false)), ("reason", string(crate::slides::lifecycle::AI_REVIEW_SAVE_REASON))]), responder);
    }
    let preparation = (|| {
        scope.check(app)?;
        let state = app.state.borrow(); let s = state.slot(scope.slot).ok_or_else(|| thrown("no file open"))?;
        if let Some(path) = path { return Ok((Some(path), None)); }
        if mode == SaveMode::Save && !s.path().is_empty() { return Ok((Some(s.path().to_owned()), None)); }
        let opts = if mode == SaveMode::SaveAs {
            let name = args.str(0).ok_or_else(|| IpcError {
                name: Some(if s.path().is_empty() { "TypeError".into() } else { "TypeError [ERR_INVALID_ARG_TYPE]".into() }),
                message: if s.path().is_empty() { "Cannot read properties of undefined (reading 'replace')".into() } else { "The \"path\" argument must be of type string. Received undefined".into() },
            })?;
            let suggestion = slides_save_as_suggestion(&save_as_suggestion(Some(s.path()).filter(|p| !p.is_empty()), name));
            let lang = app.lang();
            options([("defaultPath", jstr(suggestion)), ("filters", JsonValue::Array(slides_save_as_filters(&|key| crate::slides::i18n::tm(lang, key, &[])).into_iter().map(|f| filter_of(f.name, f.extensions)).collect()))])
        } else {
            if as_bool(args.get(0)) == Some(true) { return Ok((None, None)); }
            let name = s.first_save_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("{}.pptx", app.tm("untitledDeck", &[])));
            let path = match app.shell.new_document_dir(cx.sender.view).filter(|d| !d.is_empty()) { Some(dir) if !std::path::Path::new(&name).is_absolute() => join(&[&dir, &name]), _ => name };
            options([("defaultPath", jstr(path)), ("filters", JsonValue::Array(vec![filter("PowerPoint", &["pptx"])]))])
        };
        Ok((None, Some(opts)))
    })();
    let (path, opts) = match preparation { Ok(value) => value, Err(error) => return failed(app, &id, responder, error) };
    if let Some(path) = path {
        let mut scope = scope;
        if let crate::owned_save::SaveAuthority::Native(binding) = &scope.request.authority {
            let destination = scope.effect(app, || {
                let state = app.state.borrow();
                let session = state.slot(scope.slot).ok_or_else(|| thrown("no file open"))?;
                if session.path() != path { return Err(thrown("The presentation path changed during save")); }
                shell_contract::ipc::NativeSaveDestination::from_current_document(binding.clone(), std::path::PathBuf::from(&path)).map_err(thrown)
            });
            let destination = match destination { Ok(destination) => destination, Err(error) => return failed(app, &id, responder, error) };
            if let Err(error) = scope.request.retain_native_selection(&destination) { return failed(app, &id, responder, error); }
            scope.native_destination = Some(destination);
        }
        return conflict(app, scope, id, path, mode, automatic, responder);
    }
    let Some(opts) = opts else { return unwritten(app, &scope, &id, obj([("ok", IpcValue::Bool(false)), ("reason", string("external-modified"))]), responder); };
    let next = scope.clone();
    app.pick_save_owned(&scope, opts, Box::new(move |app, answer| match answer {
        Err(error) => failed(app, &id, responder, error),
        Ok(None) => unwritten(app, &next, &id, obj([("ok", IpcValue::Bool(false))]), responder),
        Ok(Some(selected)) => {
            let mut next = next;
            let path = match selected {
                crate::owned_save::SaveDestination::Renderer(path) => crate::host::path_string(path),
                crate::owned_save::SaveDestination::Native(destination) => {
                    let destination = if mode == SaveMode::SaveAs {
                        match destination.normalize_dialog_format(shell_contract::ipc::NativeSaveFormat::Slides) {
                            Ok(destination) => destination, Err(error) => return failed(app, &id, responder, thrown(error)),
                        }
                    } else { destination };
                    let path = crate::host::path_string(destination.path().to_path_buf());
                    if let Err(error) = next.request.retain_native_selection(&destination) { return failed(app, &id, responder, error); }
                    next.native_destination = Some(destination);
                    path
                }
            };
            if mode == SaveMode::SaveAs {
                let pick = resolve_slides_save_as_pick(&path);
                if pick.kind == "png" || pick.kind == "pdf" {
                    let result = next.effect(app, || {
                        let mut state = app.state.borrow_mut();
                        let pending = state.owned_saves.get_mut(&id).ok_or_else(|| thrown("The save request was released"))?;
                        pending.path = Some(pick.path.clone()); pending.copy = Some((pick.kind.to_owned(), pick.path.clone()));
                        pending.scope.native_destination = next.native_destination.clone();
                        pending.scope.request.revision = next.request.revision.clone();
                        pending.scope.request.revision_snapshot = next.request.revision_snapshot.clone();
                        Ok(obj([("ok", IpcValue::Bool(false)), ("exportAs", string(pick.kind)), ("path", string(pick.path)), ("saveRequestId", string(&id))]))
                    });
                    return reply_save(app, &id, result, true, responder);
                }
                conflict(app, next, id, pick.path, mode, false, responder);
            } else { conflict(app, next, id, path, mode, automatic, responder); }
        }
    }));
}

fn conflict(app: &App, scope: SaveScope, id: String, path: String, mode: SaveMode, automatic: bool, responder: Responder) {
    let eligible = {
        let state = app.state.borrow();
        state.slot(scope.slot).filter(|s| mode == SaveMode::Save && path == s.path()).map(|s| s.disk_state.clone())
    };
    let Some(recorded) = eligible else { return write_reply(app, scope, id, path, responder); };
    let revision = match scope.revision(app) { Ok(revision) => revision, Err(error) => return failed(app, &id, responder, error) };
    let shell = app.shell.clone(); let checked_path = path.clone(); let services = app.services.clone(); let index = app.registry_index;
    app.services.workers.spawn(Box::new(move || {
        let answer = shell.is_path_open_elsewhere(&checked_path).map(|elsewhere| elsewhere && is_modified_since(recorded.as_ref(), &checked_path));
        crate::slides::post_to(&services, index, move |app| {
            if let Err(error) = scope.effect_at_revision(app, &revision, || ()) { return failed(app, &id, responder, error); }
            match answer {
                Ok(false) => write_reply(app, scope, id, path, responder),
                Ok(true) => confirm_conflict(app, scope, id, path, automatic, responder),
                Err(error) => failed(app, &id, responder, thrown(error.0)),
            }
        });
    }));
}

fn confirm_conflict(app: &App, scope: SaveScope, id: String, path: String, automatic: bool, responder: Responder) {
    if automatic { return unwritten(app, &scope, &id, obj([("ok", IpcValue::Bool(false)), ("reason", string("external-modified"))]), responder); }
    let labels = main_core::packages::electron_utils::save_conflict_labels::save_conflict_labels(app.lang().as_str());
    let options = MessageBox { kind: Some("warning".into()), message: labels.message, detail: Some(labels.detail), buttons: vec![labels.overwrite, labels.cancel], default_id: Some(0), cancel_id: Some(1), ..Default::default() };
    let services = app.services.clone(); let index = app.registry_index; let request = scope.request.clone();
    request.message_dialog(&app.dialogs, app.shell.dialog_parent(), options,
        Box::new(move |answer| crate::slides::post_to(&services, index, move |app| match answer {
            Ok(answer) if answer.response == 0 => write_reply(app, scope, id, path, responder),
            Ok(_) => unwritten(app, &scope, &id, obj([("ok", IpcValue::Bool(false)), ("reason", string("external-modified"))]), responder),
            Err(error) => failed(app, &id, responder, thrown(error)),
        })));
}

fn notify_save_as(app: &App, id: &str, result: &Result<IpcValue, IpcError>) {
    let view = app.state.borrow().owned_saves.get(id).filter(|save| save.mode == SaveMode::SaveAs).map(|save| save.scope.request.destination.view);
    if let Some(view) = view { app.save_as_answered(view, result); }
}

fn reply_save(app: &App, id: &str, result: Result<IpcValue, IpcError>, keep_copy: bool, responder: Responder) {
    notify_save_as(app, id, &result);
    if !keep_copy { app.release_direct_save(id); }
    responder.reply(result);
}

fn unwritten(app: &App, scope: &SaveScope, id: &str, value: IpcValue, responder: Responder) {
    reply_save(app, id, scope.effect(app, || Ok(value)), false, responder);
}

fn failed(app: &App, id: &str, responder: Responder, error: IpcError) {
    let reply = if error.name.is_some() { Err(error.clone()) } else { Ok(obj([("ok", IpcValue::Bool(false)), ("error", string(&error.message))])) };
    notify_save_as(app, id, &reply);
    app.finish_owned_save(id, Err(error)); responder.reply(reply);
}

fn write_reply(app: &App, scope: SaveScope, id: String, path: String, responder: Responder) {
    let result_scope = scope.clone();
    app.write_owned_deck(scope, path.clone(), false, Box::new(move |app, result| {
        let result = result.and_then(|()| {
            let (slides, path) = {
                let state = app.state.borrow(); let s = state.slot(result_scope.slot).ok_or_else(|| thrown("no file open"))?;
                (app.render_all(s, &result_scope.render_owner)?, s.path().to_owned())
            };
            result_scope.effect(app, || {
                app.state.borrow_mut().owned_saves.get_mut(&id).ok_or_else(|| thrown("The save request was released"))?.saved = true;
                Ok(obj([("ok", IpcValue::Bool(true)), ("path", string(path)), ("slides", slides)]))
            })
        });
        if let Err(error) = &result { return failed(app, &id, responder, error.clone()); }
        reply_save(app, &id, result, false, responder);
    }));
}
