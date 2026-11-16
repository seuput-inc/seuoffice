use super::app::{MAP_EXTENSION, MindmapApp, SAVE_WAIT, TimerJob, Wait, mode_name};
use super::hosts::OpenStatus;
use super::util::{file_name_base, has_key};
use crate::owned_save::OwnedSave;
use crate::value::{Args, as_bytes, as_str, field, obj, string, thrown};
use jscompat::node_path::host::{basename, dirname, join};
use jscompat::{JsObject, JsonValue};
use main_core::mindmap::save_as_formats::{
    MapSaveAsKind, save_as_default_path, save_as_filters, save_as_target,
};
use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::atomic_write::owned::Admission;
use main_core::packages::electron_utils::dialog_memory::{
    prepare_dialog_options, remember_save_path,
};
use main_core::packages::electron_utils::recovery_store::{
    RecoveryDeleteReason, RecoveryStoreOptions, create_recovery_store,
};
use main_core::packages::electron_utils::save_conflict::{
    SaveDiskState, is_modified_since, record_disk_state,
};
use shell_contract::editors::{SaveMode, Then};
use shell_contract::hosts::{MessageBox, OwnedEffectsHost};
use shell_contract::ipc::{CallContext, NativeRequestOwner, Responder};
use shell_contract::runtime::{CancelSubscription, CancelToken, RendererGeneration};
use shell_contract::{IpcError, IpcValue, ViewId};
use std::sync::{Arc, mpsc};

type SavedDone =
    Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>;

#[path = "owned_rename.rs"]
mod rename;
#[path = "save_revision.rs"]
mod revision;
use revision::MapRevision;
#[path = "owned_close.rs"]
mod close;
pub(super) use close::PendingClose;
#[path = "owned_project_save.rs"]
mod project;
pub(super) use project::save_into_project;

pub(super) struct PendingSave {
    pub request: OwnedSave,
    mode: SaveMode,
    override_path: Option<String>,
    native_destination: Option<shell_contract::ipc::NativeSaveDestination>,
    revision: Option<u64>,
    baseline: Option<String>,
    export: Option<(MapSaveAsKind, String)>,
    export_taken: bool,
    saved: bool,
    done: Option<Then<bool>>,
    _subscriptions: Vec<CancelSubscription>,
}

impl MindmapApp {
    pub(super) fn replace_save_document(
        &self,
        cx: &CallContext,
    ) -> Result<NativeRequestOwner, IpcError> {
        let source = self.capture_save_owner(cx.sender.view, native_owner(cx)?)?;
        let old_document = source
            .context
            .document_lease()
            .cloned()
            .ok_or_else(|| thrown("The map document was released"))?;
        let effects = self
            .0
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or_else(|| thrown("The native document transition host is unavailable"))?;
        let mut prepared = Some(source.prepare_document_successor().map_err(thrown)?);
        let mut retired = None;
        let mut completed = None;
        effects
            .with_owned_transition(
                &source,
                &source,
                &cx.cancel,
                &cx.cancel,
                &mut |retirement| {
                    let mut state = self.0.state.borrow_mut();
                    if !state
                        .documents
                        .get(&cx.sender.view)
                        .is_some_and(|document| document.same_generation(&old_document))
                        || prepared.is_none()
                    {
                        return Err("The map document changed before replacement".into());
                    }
                    retirement.retire(&old_document)?;
                    let owner = retirement
                        .renew_owner(prepared.take().ok_or("The map successor was released")?)?;
                    let lifetime = owner
                        .context
                        .document_lease()
                        .ok_or("The map successor has no document lease")?
                        .clone();
                    retired = state.documents.insert(cx.sender.view, lifetime);
                    state.revisions.insert(cx.sender.view, 0);
                    completed = Some(owner);
                    Ok(())
                },
            )
            .map_err(thrown)?;
        drop(retired);
        completed.ok_or_else(|| thrown("The map document transition did not complete"))
    }

    fn capture_save(
        &self,
        view: ViewId,
        origin: NativeRequestOwner,
        cancel: CancelToken,
    ) -> Result<OwnedSave, IpcError> {
        if !self.is_alive(view) {
            return Err(thrown("The map is no longer open"));
        }
        let effects = self
            .0
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or_else(|| thrown("The native save effect host is unavailable"))?;
        let expected = self
            .0
            .services
            .views
            .renderer_generation(view)
            .ok_or_else(|| thrown("The destination map renderer was released"))?;
        let destination = effects.capture_owner(view, &expected).map_err(thrown)?;
        let document = self
            .0
            .state
            .borrow()
            .documents
            .get(&view)
            .cloned()
            .ok_or_else(|| thrown("The destination map document was released"))?;
        OwnedSave::new(origin, destination, cancel, document, effects)
    }

    fn save_loss_subscriptions(&self, id: u64, request: &OwnedSave) -> Vec<CancelSubscription> {
        let owns_pending = matches!(
            &request.authority,
            crate::owned_save::SaveAuthority::Renderer(_)
        );
        let mut losses = vec![request.cancel.clone(), request.document.loss_token()];
        if let Some(loss) = request.authority.loss_token() {
            losses.push(loss);
        }
        for owner in request.participants(&[]) {
            if let Some(loss) = owner.loss_token() {
                losses.push(loss);
            }
        }
        losses
            .into_iter()
            .map(|loss| {
                let pending = request.pending.clone();
                let core = self.0.services.core.clone();
                loss.on_cancel(move || {
                    if owns_pending {
                        pending.cancel();
                    }
                    core.post(Box::new(move || {
                        super::with_app(|app| {
                            app.finish_owned_save(id, Err(thrown("The save owner was released")))
                        })
                    }));
                })
            })
            .collect()
    }

    fn insert_save(
        &self,
        request: OwnedSave,
        mode: SaveMode,
        path: Option<String>,
        done: Option<Then<bool>>,
    ) -> u64 {
        let view = request.destination.view;
        let id = self.new_gen();
        let subscriptions = self.save_loss_subscriptions(id, &request);
        let baseline = self.file_path(view);
        self.0.state.borrow_mut().owned_saves.insert(
            id,
            PendingSave {
                request: request.clone(),
                mode,
                override_path: path,
                native_destination: None,
                revision: None,
                baseline,
                export: None,
                export_taken: false,
                saved: false,
                done,
                _subscriptions: subscriptions,
            },
        );
        id
    }

    pub(super) fn request_save_owned(
        &self,
        view: ViewId,
        mode: SaveMode,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        path: Option<String>,
        done: Then<bool>,
    ) {
        if !self.may_write(view) {
            return done(Err(thrown(
                "The file did not open, so there is no map to save",
            )));
        }
        let request = match self.capture_save(view, origin, cancel) {
            Ok(value) => value,
            Err(error) => return done(Err(error)),
        };
        self.request_save_with_scope(view, mode, path, request, done);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn request_save_native(
        &self,
        view: ViewId,
        mode: SaveMode,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        path: Option<String>,
        done: Then<bool>,
    ) {
        use shell_contract::ipc::{
            NativeEditorCommand, NativeInvocationSelection, NativeSaveSelection,
        };
        let valid = match (binding.operation().selection(), &path) {
            (
                NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
                    surface: shell_contract::Surface::Mindmap,
                    view: selected,
                    selection: NativeSaveSelection::Mode(selected_mode),
                }),
                None,
            ) => *selected == view && *selected_mode == mode,
            (
                NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
                    surface: shell_contract::Surface::Mindmap,
                    view: selected,
                    selection: NativeSaveSelection::ExactPath(selected_path),
                }),
                Some(path),
            ) => *selected == view && selected_path == std::path::Path::new(path),
            _ => false,
        };
        if !valid || binding.owner().view != view {
            return done(Err(thrown(
                "The native save selection does not match the map",
            )));
        }
        if !self.may_write(view) {
            return done(Err(thrown(
                "The file did not open, so there is no map to save",
            )));
        }
        let document = match self.0.state.borrow().documents.get(&view).cloned() {
            Some(value) => value,
            None => return done(Err(thrown("The map document was released"))),
        };
        let effects = match self.0.services.hosts.get::<OwnedEffectsHost>().cloned() {
            Some(value) => value,
            None => return done(Err(thrown("The native save effect host is unavailable"))),
        };
        let request = match OwnedSave::new_native(binding, cancel, pending, document, effects) {
            Ok(value) => value,
            Err(error) => return done(Err(error)),
        };
        self.request_save_with_scope(view, mode, path, request, done);
    }

    fn request_save_with_scope(
        &self,
        view: ViewId,
        mode: SaveMode,
        path: Option<String>,
        request: OwnedSave,
        done: Then<bool>,
    ) {
        if mode == SaveMode::Save
            && path.is_none()
            && !self.is_dirty(view)
            && self.file_path(view).is_some()
        {
            return done(request.effect(|| Ok(true)));
        }
        let id = self.insert_save(request.clone(), mode, path, Some(done));
        let emitted = request.emit(
            &request.destination,
            "mindmap:save-request",
            vec![string(mode_name(mode)), string(id.to_string())],
        );
        if let Err(error) = emitted {
            return self.finish_owned_save(id, Err(error));
        }
        self.schedule(SAVE_WAIT, TimerJob::OwnedSaveWait(id));
    }

    pub(super) fn finish_owned_save(&self, id: u64, result: Result<bool, IpcError>) {
        let pending = self.0.state.borrow_mut().owned_saves.remove(&id);
        if let Some(pending) = pending {
            let result = result.and_then(|ok| pending.request.effect(|| Ok(ok && pending.saved)));
            if matches!(
                &pending.request.authority,
                crate::owned_save::SaveAuthority::Renderer(_)
            ) {
                pending.request.pending.cancel();
            }
            if let Some(done) = pending.done {
                done(result);
            }
        }
    }

    fn pending_for(&self, cx: &CallContext, id: u64) -> Result<OwnedSave, IpcError> {
        let state = self.0.state.borrow();
        let pending = state
            .owned_saves
            .get(&id)
            .ok_or_else(|| thrown("The map save request is no longer pending"))?;
        pending.request.validate_sender(cx)?;
        let view = pending.request.destination.view;
        if !state
            .documents
            .get(&view)
            .is_some_and(|document| document.same_generation(&pending.request.document))
        {
            return Err(thrown("The map document was replaced"));
        }
        Ok(pending.request.clone())
    }

    fn release_direct_save(&self, id: u64) {
        let pending = {
            let mut state = self.0.state.borrow_mut();
            if !state
                .owned_saves
                .get(&id)
                .is_some_and(|pending| pending.done.is_none())
            {
                return;
            }
            state.owned_saves.remove(&id)
        };
        if let Some(pending) = pending
            && matches!(
                &pending.request.authority,
                crate::owned_save::SaveAuthority::Renderer(_)
            )
        {
            pending.request.pending.cancel();
        }
    }

    pub(super) fn save_ack_owned(
        &self,
        cx: &CallContext,
        raw: &str,
        ok: bool,
        error: Option<&str>,
    ) -> Result<(), IpcError> {
        let id = request_id(raw)?;
        let request = self.pending_for(cx, id)?.capture_callback(cx)?;
        request.effect(|| {
            let mut state = self.0.state.borrow_mut();
            let pending = state
                .owned_saves
                .get_mut(&id)
                .ok_or_else(|| thrown("The map save request was released"))?;
            pending.request = request.clone();
            Ok(())
        })?;
        self.finish_owned_save(
            id,
            match error {
                Some(message) => Err(thrown(message)),
                None => Ok(ok),
            },
        );
        Ok(())
    }

    fn admission(&self, id: u64) -> Admission {
        self.admission_for(id, None)
    }

    fn admission_for(&self, id: u64, destination_path: Option<String>) -> Admission {
        let core = self.0.services.core.clone();
        Arc::new(move |effect| {
            let destination_path = destination_path.clone();
            let (send, receive) = mpsc::sync_channel(1);
            core.post(Box::new(move || {
                let result = super::handle()
                    .ok_or_else(|| thrown("The map application was released"))
                    .and_then(|app| {
                        let (request, revision, baseline, native_destination) = {
                            let state = app.0.state.borrow();
                            let pending = state
                                .owned_saves
                                .get(&id)
                                .ok_or_else(|| thrown("The map save request was released"))?;
                            (
                                pending.request.clone(),
                                pending.revision,
                                pending.baseline.clone(),
                                pending.native_destination.clone(),
                            )
                        };
                        let run = || {
                            let state = app.0.state.borrow();
                            let view = request.destination.view;
                            if !state
                                .documents
                                .get(&view)
                                .is_some_and(|doc| doc.same_generation(&request.document))
                                || state.revisions.get(&view).copied() != revision
                                || state.save_path.get(&view).cloned() != baseline
                            {
                                return Err(thrown("The map changed while it was being saved"));
                            }
                            Ok(effect())
                        };
                        match (&request.authority, destination_path.as_deref()) {
                            (crate::owned_save::SaveAuthority::Native(_), Some(path)) => {
                                let destination = native_destination.as_ref().ok_or_else(|| {
                                    thrown("The native map save destination was released")
                                })?;
                                request.effect_destination(
                                    destination,
                                    std::path::Path::new(path),
                                    run,
                                )
                            }
                            _ => request.effect(run),
                        }
                    });
                let _ = send.send(result.map_err(|error| NodeError {
                    code: None,
                    message: error.message,
                }));
            }));
            receive.recv().map_err(|_| NodeError {
                code: None,
                message: "The map save dispatcher was released".into(),
            })?
        })
    }

    fn capture_existing_destination(
        &self,
        id: u64,
        request: &OwnedSave,
        path: &str,
        explicit: bool,
    ) -> Result<(), IpcError> {
        let crate::owned_save::SaveAuthority::Native(binding) = &request.authority else {
            return Ok(());
        };
        let destination = request.effect(|| {
            let state = self.0.state.borrow();
            let destination = if explicit {
                shell_contract::ipc::NativeSaveDestination::from_exact_selection(binding.clone())
                    .map_err(thrown)?
            } else {
                if state
                    .save_path
                    .get(&request.destination.view)
                    .is_none_or(|current| current != path)
                {
                    return Err(thrown("The map save path was replaced"));
                }
                shell_contract::ipc::NativeSaveDestination::from_current_document(
                    binding.clone(),
                    std::path::PathBuf::from(path),
                )
                .map_err(thrown)?
            };
            if destination.path() != std::path::Path::new(path) {
                return Err(thrown("The selected map save path changed"));
            }
            Ok(destination)
        })?;
        let mut retained = request.clone();
        retained.retain_native_selection(&destination)?;
        request.effect(|| {
            let mut state = self.0.state.borrow_mut();
            if !explicit
                && state
                    .save_path
                    .get(&request.destination.view)
                    .is_none_or(|current| current != path)
            {
                return Err(thrown("The map save path was replaced"));
            }
            let pending = state
                .owned_saves
                .get_mut(&id)
                .ok_or_else(|| thrown("The map save request was released"))?;
            pending.request = retained;
            pending.native_destination = Some(destination);
            Ok(())
        })
    }

    fn resolve_owned_target(
        &self,
        cx: CallContext,
        id: u64,
        request_value: IpcValue,
        bytes: Vec<u8>,
        responder: Responder,
    ) {
        let request = match self
            .pending_for(&cx, id)
            .and_then(|request| request.capture_callback(&cx))
        {
            Ok(value) => value,
            Err(error) => return fail(self, id, responder, error),
        };
        if let Err(error) = request.effect(|| {
            let mut state = self.0.state.borrow_mut();
            let pending = state
                .owned_saves
                .get_mut(&id)
                .ok_or_else(|| thrown("The map save request was released"))?;
            pending.request = request.clone();
            Ok(())
        }) {
            return fail(self, id, responder, error);
        }
        let view = cx.sender.view;
        let (mode, override_path) = {
            let state = self.0.state.borrow();
            let Some(pending) = state.owned_saves.get(&id) else {
                return fail(
                    self,
                    id,
                    responder,
                    thrown("The map save request was released"),
                );
            };
            (pending.mode, pending.override_path.clone())
        };
        if let Some(path) = override_path {
            if let Err(error) = self.capture_existing_destination(id, &request, &path, true) {
                return fail(self, id, responder, error);
            }
            return self.write_map_owned(cx, id, path, bytes, responder);
        }
        if mode == SaveMode::Save
            && let Some(path) = self.file_path(view)
        {
            if let Err(error) = self.capture_existing_destination(id, &request, &path, false) {
                return fail(self, id, responder, error);
            }
            return self.conflict_owned(
                cx,
                id,
                path,
                bytes,
                matches!(field(&request_value, "auto"), IpcValue::Bool(true)),
                responder,
            );
        }
        let opts = self.save_options(view, mode, as_str(field(&request_value, "defaultName")));
        let opts = match opts {
            Ok(opts) => opts,
            Err(error) => return fail(self, id, responder, error),
        };
        let opts = {
            let memory = match self.0.shared.dialog_memory.lock() {
                Ok(memory) => memory,
                Err(_) => {
                    return fail(
                        self,
                        id,
                        responder,
                        thrown("The dialog memory is unavailable"),
                    );
                }
            };
            crate::host::file_dialog(&prepare_dialog_options(&memory, &opts, None))
        };
        let core = self.0.services.core.clone();
        request.clone().save_dialog(
            &self.0.dialogs,
            self.window_of(view),
            opts,
            Box::new(move |answer| {
                core.post(Box::new(move || {
                    super::with_app(|app| {
                        let outcome =
                            answer.map_err(thrown).and_then(|selected| {
                                let request = app.pending_for(&cx, id)?;
                                let Some(selected) = selected else {
                                    return Ok(None);
                                };
                                let path = selected
                                    .path()
                                    .to_str()
                                    .ok_or_else(|| thrown("The save path is not valid Unicode"))?
                                    .to_owned();
                                request.effect(|| {
                                    let mut memory =
                                        app.0.shared.dialog_memory.lock().map_err(|_| {
                                            thrown("The dialog memory is unavailable")
                                        })?;
                                    remember_save_path(&mut memory, &path);
                                    Ok(())
                                })?;
                                Ok(Some((path, selected)))
                            });
                        match outcome {
                            Err(error) => fail(app, id, responder, error),
                            Ok(None) => {
                                let result = request.effect(|| {
                                    Ok(obj([
                                        ("ok", IpcValue::Bool(true)),
                                        ("canceled", IpcValue::Bool(true)),
                                    ]))
                                });
                                app.release_direct_save(id);
                                responder.reply(result);
                            }
                            Ok(Some((raw_path, selected))) => {
                                let (path, kind) =
                                    save_as_target(&raw_path, mode == SaveMode::SaveAs);
                                if let crate::owned_save::SaveDestination::Native(destination) =
                                    selected
                                {
                                    let destination = match destination.normalize_dialog_format(
                                        shell_contract::ipc::NativeSaveFormat::Mindmap,
                                    ) {
                                        Ok(destination)
                                            if destination.path()
                                                == std::path::Path::new(&path) =>
                                        {
                                            destination
                                        }
                                        Ok(_) => {
                                            return fail(
                                                app,
                                                id,
                                                responder,
                                                thrown(
                                                    "The normalized map destination does not match",
                                                ),
                                            );
                                        }
                                        Err(error) => {
                                            return fail(app, id, responder, thrown(error));
                                        }
                                    };
                                    let mut retained = request.clone();
                                    if let Err(error) =
                                        retained.retain_native_selection(&destination)
                                    {
                                        return fail(app, id, responder, error);
                                    }
                                    if let Err(error) = request.effect(|| {
                                        let mut state = app.0.state.borrow_mut();
                                        let pending =
                                            state.owned_saves.get_mut(&id).ok_or_else(|| {
                                                thrown("The map save request was released")
                                            })?;
                                        pending.request = retained;
                                        pending.native_destination = Some(destination);
                                        Ok(())
                                    }) {
                                        return fail(app, id, responder, error);
                                    }
                                }
                                if kind != MapSaveAsKind::Xmind {
                                    let result = request.effect(|| {
                                        let mut state = app.0.state.borrow_mut();
                                        let pending =
                                            state.owned_saves.get_mut(&id).ok_or_else(|| {
                                                thrown("The map save request was released")
                                            })?;
                                        pending.export = Some((kind, path.clone()));
                                        Ok(obj([
                                            ("ok", IpcValue::Bool(true)),
                                            (
                                                "export",
                                                obj([
                                                    ("format", string(kind.as_str())),
                                                    ("path", string(path)),
                                                    ("ownedSaveRequestId", string(id.to_string())),
                                                ]),
                                            ),
                                        ]))
                                    });
                                    responder.reply(result);
                                } else {
                                    app.write_map_owned(cx, id, path, bytes, responder);
                                }
                            }
                        }
                    })
                }))
            }),
        );
    }

    fn save_options(
        &self,
        view: ViewId,
        mode: SaveMode,
        name: Option<&str>,
    ) -> Result<JsObject<JsonValue>, IpcError> {
        let offer = mode == SaveMode::SaveAs;
        let current = self.file_path(view);
        let imported = self.0.state.borrow().imported.get(&view).cloned();
        let default = match current {
            Some(path) => join(&[&dirname(&path), &basename(&path, None)]),
            None => match self.view_save_as_default(view).or_else(|| {
                imported.map(|(path, title)| {
                    let base = file_name_base(Some(&title));
                    let base = if base.is_empty() {
                        self.tm("untitledFile")
                    } else {
                        base
                    };
                    join(&[&dirname(&path), &format!("{base}{MAP_EXTENSION}")])
                })
            }) {
                Some(path) => path,
                None => {
                    let dir = self
                        .0
                        .shell
                        .new_document_dir(view)
                        .map(Ok)
                        .unwrap_or_else(|| self.default_save_dir())
                        .map_err(thrown)?;
                    let base = file_name_base(name);
                    let base = if base.is_empty() {
                        self.tm("untitledFile")
                    } else {
                        base
                    };
                    join(&[&dir, &format!("{base}{MAP_EXTENSION}")])
                }
            },
        };
        let mut opts = JsObject::new();
        opts.set("title", JsonValue::String(self.tm("dlgSaveTitle")));
        opts.set(
            "defaultPath",
            JsonValue::String(save_as_default_path(&default, offer)),
        );
        opts.set(
            "filters",
            JsonValue::Array(
                save_as_filters(offer, &mut |key| self.tm(key))
                    .into_iter()
                    .map(|filter| {
                        let mut item = JsObject::new();
                        item.set("name", JsonValue::String(filter.name));
                        item.set(
                            "extensions",
                            JsonValue::Array(
                                filter
                                    .extensions
                                    .into_iter()
                                    .map(|ext| JsonValue::String(ext.to_owned()))
                                    .collect(),
                            ),
                        );
                        JsonValue::Object(item)
                    })
                    .collect(),
            ),
        );
        Ok(opts)
    }

    fn conflict_owned(
        &self,
        cx: CallContext,
        id: u64,
        path: String,
        bytes: Vec<u8>,
        automatic: bool,
        responder: Responder,
    ) {
        let request = match self.pending_for(&cx, id) {
            Ok(value) => value,
            Err(error) => return fail(self, id, responder, error),
        };
        let recorded = self
            .0
            .state
            .borrow()
            .disk_states
            .get(&cx.sender.view)
            .and_then(|states| states.get(&path))
            .cloned();
        let shell = self.0.shell.clone();
        let check_path = path.clone();
        self.offload(
            move || {
                shell.is_path_open_elsewhere(&check_path)
                    && is_modified_since(recorded.as_ref(), &check_path)
            },
            move |app, conflict| {
                if !conflict {
                    return app.write_map_owned(cx, id, path, bytes, responder);
                }
                if automatic {
                    let result = request.effect(|| {
                        Ok(obj([
                            ("ok", IpcValue::Bool(false)),
                            ("error", string("external-modified")),
                        ]))
                    });
                    app.release_direct_save(id);
                    return responder.reply(result);
                }
                let labels =
                    main_core::packages::electron_utils::save_conflict_labels::save_conflict_labels(
                        app.lang().as_str(),
                    );
                let options = MessageBox {
                    kind: Some("warning".into()),
                    message: labels.message,
                    detail: Some(labels.detail),
                    buttons: vec![labels.overwrite, labels.cancel],
                    default_id: Some(0),
                    cancel_id: Some(1),
                    ..Default::default()
                };
                let core = app.0.services.core.clone();
                request.clone().message_dialog(
                    &app.0.dialogs,
                    app.window_of(cx.sender.view),
                    options,
                    Box::new(move |answer| {
                        core.post(Box::new(move || {
                            super::with_app(|app| match answer {
                                Ok(answer) if answer.response == 0 => {
                                    app.write_map_owned(cx, id, path, bytes, responder)
                                }
                                Ok(_) => {
                                    let result = request.effect(|| {
                                        Ok(obj([
                                            ("ok", IpcValue::Bool(false)),
                                            ("error", string("external-modified")),
                                        ]))
                                    });
                                    app.release_direct_save(id);
                                    responder.reply(result);
                                }
                                Err(error) => fail(app, id, responder, thrown(error)),
                            })
                        }))
                    }),
                );
            },
        );
    }

    fn write_map_owned(
        &self,
        cx: CallContext,
        id: u64,
        path: String,
        bytes: Vec<u8>,
        responder: Responder,
    ) {
        if let Err(error) = self.pending_for(&cx, id) {
            return fail(self, id, responder, error);
        }
        let admit = self.admission_for(id, Some(path.clone()));
        let target = path.clone();
        self.offload(
            move || {
                main_core::packages::electron_utils::atomic_write::owned::write_file(
                    &target, &bytes, admit,
                )
                .map(|()| record_disk_state(&target))
                .map_err(|error| thrown(error.message))
            },
            move |app, outcome| {
                let request = match app.pending_for(&cx, id) {
                    Ok(value) => value,
                    Err(error) => return fail(app, id, responder, error),
                };
                let disk = match outcome {
                    Ok(disk) => disk,
                    Err(error) => return fail(app, id, responder, error),
                };
                app.adopt_owned_map(cx, id, path, request, disk, responder);
            },
        );
    }

    fn adopt_owned_map(
        &self,
        cx: CallContext,
        id: u64,
        path: String,
        request: OwnedSave,
        disk: Option<SaveDiskState>,
        responder: Responder,
    ) {
        let view = cx.sender.view;
        let previous = self.file_path(view);
        let accepted = request.effect(|| {
            let mut state = self.0.state.borrow_mut();
            let pending = state
                .owned_saves
                .get(&id)
                .ok_or_else(|| thrown("The map save request was released"))?;
            if state.revisions.get(&view).copied() != pending.revision
                || state.save_path.get(&view).cloned() != pending.baseline
            {
                return Err(thrown("The map changed while it was being saved"));
            }
            state.save_path.insert(view, path.clone());
            state.open_path.insert(view, path.clone());
            state.allowed.entry(view).or_default().insert(path.clone());
            state.dirty.remove(&view);
            if let Some(save) = state.owned_saves.get_mut(&id) {
                save.baseline = Some(path.clone());
            }
            if let Some(disk) = disk {
                state
                    .disk_states
                    .entry(view)
                    .or_default()
                    .insert(path.clone(), disk);
            }
            if let Some(previous) = previous.as_ref().filter(|old| *old != &path)
                && let Some(states) = state.disk_states.get_mut(&view)
            {
                states.remove(previous);
            }
            Ok(state
                .recovery
                .get(&view)
                .map(|recovery| recovery.owned.clone())
                .unwrap_or_default())
        });
        let copies = match accepted {
            Ok(copies) => copies,
            Err(error) => return fail(self, id, responder, error),
        };
        let revision = match MapRevision::capture(self, &request) {
            Ok(revision) => revision,
            Err(error) => return fail(self, id, responder, error),
        };
        let admission = revision.admission(self, &request);
        let recovery = self.0.recovery_state.clone();
        let dir = self.user_data_dir();
        let identities = copies
            .into_iter()
            .map(|(_, identity)| identity)
            .collect::<Vec<_>>();
        self.offload(
            move || {
                crate::owned_recovery::delete_copies(
                    &recovery,
                    &dir,
                    &identities,
                    RecoveryDeleteReason::Saved,
                    &admission,
                )
                .map_err(|error| thrown(error.message))
            },
            move |app, outcome| {
                let deleted = match outcome {
                    Ok(value) => value,
                    Err(error) => return fail(app, id, responder, error),
                };
                let cleared = revision
                    .effect(app, &request, || {
                        let mut state = app.0.state.borrow_mut();
                        if let Some(recovery) = state.recovery.get_mut(&view) {
                            recovery.owned.retain(|(key, _)| {
                                !deleted
                                    .iter()
                                    .any(|copy| !copy.refused && copy.path == *key)
                            });
                            recovery.untitled_copy = None;
                            recovery.save_as_default = None;
                            recovery.title = None;
                        }
                        Ok::<_, IpcError>(())
                    })
                    .and_then(|result| result);
                if let Err(error) = cleared {
                    return fail(app, id, responder, error);
                }
                if previous.as_deref() == Some(&path) {
                    return saved_reply(app, id, &request, path, responder);
                }
                let callback_app = app.clone();
                let callback_request = std::rc::Rc::new(std::cell::RefCell::new(request.clone()));
                let on_request = callback_request.clone();
                let on_app = app.clone();
                let on_revision = std::rc::Rc::new(
                    move |receipt: &shell_contract::ipc::NativeSaveRevisionReceipt| {
                        let mut updated = on_request.borrow().clone();
                        updated
                            .apply_revision_receipt(receipt)
                            .map_err(|error| error.message)?;
                        let subscriptions = on_app.save_loss_subscriptions(id, &updated);
                        {
                            let mut state = on_app.0.state.borrow_mut();
                            let pending = state
                                .owned_saves
                                .get_mut(&id)
                                .filter(|pending| {
                                    pending.request.pending.same_token(&updated.pending)
                                })
                                .ok_or("The map save request was released")?;
                            let mut replacement = pending.request.clone();
                            replacement
                                .apply_revision_receipt(receipt)
                                .map_err(|error| error.message)?;
                            pending.native_destination =
                                Some(receipt.snapshot().selection().clone());
                            pending.request = replacement;
                            pending._subscriptions = subscriptions;
                        }
                        *on_request.borrow_mut() = updated;
                        Ok(())
                    },
                );
                let done: SavedDone = Box::new(move |answer| match answer {
                    Ok(saved) => acknowledge_saved_hook(
                        &callback_app,
                        id,
                        callback_request.borrow().clone(),
                        saved,
                        responder,
                    ),
                    Err(error) => fail(&callback_app, id, responder, thrown(error)),
                });
                match &request.authority {
                    crate::owned_save::SaveAuthority::Renderer(origin) => {
                        app.0.shell.file_saved_owned(
                            origin,
                            &request.destination,
                            &request.participants(&[]),
                            &request.cancel,
                            &request.pending,
                            &request.additional,
                            &request.effects,
                            path.into(),
                            done,
                        )
                    }
                    crate::owned_save::SaveAuthority::Native(_) => {
                        let destination = app
                            .0
                            .state
                            .borrow()
                            .owned_saves
                            .get(&id)
                            .and_then(|pending| pending.native_destination.clone());
                        match (destination, request.revision.clone()) {
                            (Some(_), Some(revision)) => app.0.shell.file_saved_existing_document(
                                revision,
                                path.into(),
                                request.participants(&[]),
                                request.cancel.clone(),
                                request.pending.clone(),
                                request.additional.clone(),
                                request.effects.clone(),
                                on_revision,
                                done,
                            ),
                            _ => done(Err(
                                "The native map save destination or revision was released".into(),
                            )),
                        }
                    }
                }
            },
        );
    }

    pub(super) fn owned_export(
        &self,
        cx: &CallContext,
        id: u64,
        kind: MapSaveAsKind,
        path: &str,
    ) -> Result<(OwnedSave, Admission), IpcError> {
        let request = self.pending_for(cx, id)?.capture_callback(cx)?;
        request.effect(|| {
            let mut state = self.0.state.borrow_mut();
            let pending = state
                .owned_saves
                .get_mut(&id)
                .ok_or_else(|| thrown("The map save request was released"))?;
            if pending.export_taken || pending.export.as_ref() != Some(&(kind, path.to_owned())) {
                return Err(thrown("This export was not selected by this save request"));
            }
            pending.export_taken = true;
            pending.request = request.clone();
            Ok(())
        })?;
        Ok((request, self.admission_for(id, Some(path.to_owned()))))
    }
}

fn acknowledge_saved_hook(
    app: &MindmapApp,
    id: u64,
    request: OwnedSave,
    saved: shell_contract::hosts::SavedFile<std::path::PathBuf>,
    responder: Responder,
) {
    let path = crate::host::path_string(saved.path);
    let revision = {
        let state = app.0.state.borrow();
        match state.owned_saves.get(&id).and_then(|pending| {
            pending.revision.map(|revision| MapRevision {
                revision,
                path: pending.baseline.clone(),
            })
        }) {
            Some(revision) => revision,
            None => {
                return fail(
                    app,
                    id,
                    responder,
                    thrown("The map save revision was released"),
                );
            }
        }
    };
    let previous = match revision.effect(app, &request, || app.file_path(request.destination.view))
    {
        Ok(previous) => previous,
        Err(error) => return fail(app, id, responder, error),
    };
    if previous.as_deref() == Some(&path) {
        finalize_saved_hook(app, id, request, path, saved.session, responder);
    } else {
        let Some(previous) = previous else {
            return fail(
                app,
                id,
                responder,
                thrown("The saved map path was released"),
            );
        };
        let callback_app = app.clone();
        let callback_request = request.clone();
        let final_path = path.clone();
        let done = Box::new(move |answer| match answer {
            Ok(()) => finalize_saved_hook(
                &callback_app,
                id,
                callback_request,
                final_path,
                saved.session,
                responder,
            ),
            Err(error) => fail(&callback_app, id, responder, error),
        });
        match &request.authority {
            crate::owned_save::SaveAuthority::Renderer(origin) => app.rename_path_save_owned(
                request.destination.view,
                &previous,
                &path,
                &request,
                origin,
                done,
            ),
            crate::owned_save::SaveAuthority::Native(binding) => app.rename_path_native(
                request.destination.view,
                &previous,
                &path,
                binding,
                &request.destination,
                &request.participants(&[]),
                &request.cancel,
                &request.pending,
                &request.additional,
                done,
            ),
        }
    }
}

fn finalize_saved_hook(
    app: &MindmapApp,
    id: u64,
    request: OwnedSave,
    path: String,
    receipt: shell_contract::hosts::SavedSessionReceipt,
    responder: Responder,
) {
    let expected = {
        let state = app.0.state.borrow();
        state.owned_saves.get(&id).and_then(|pending| {
            pending.revision.map(|revision| MapRevision {
                revision,
                path: pending.baseline.clone(),
            })
        })
    };
    let Some(revision) = expected else {
        return fail(
            app,
            id,
            responder,
            thrown("The map save revision was released"),
        );
    };
    let accepted = revision
        .effect(app, &request, || {
            if app.file_path(request.destination.view).as_deref() != Some(&path) {
                return Err(thrown("The final saved map path was not acknowledged"));
            }
            Ok(())
        })
        .and_then(|answer| answer);
    if let Err(error) = accepted {
        return fail(app, id, responder, error);
    }
    let app = app.clone();
    receipt.finalize(Box::new(move |answer| match answer {
        Ok(()) => saved_reply(&app, id, &request, path, responder),
        Err(error) => fail(&app, id, responder, thrown(error)),
    }));
}

fn saved_reply(app: &MindmapApp, id: u64, request: &OwnedSave, path: String, responder: Responder) {
    let revision = {
        let state = app.0.state.borrow();
        match state.owned_saves.get(&id).and_then(|pending| {
            pending.revision.map(|revision| MapRevision {
                revision,
                path: pending.baseline.clone(),
            })
        }) {
            Some(revision) => revision,
            None => {
                return fail(
                    app,
                    id,
                    responder,
                    thrown("The map save revision was released"),
                );
            }
        }
    };
    let result = revision
        .effect(app, request, || {
            app.0
                .state
                .borrow_mut()
                .owned_saves
                .get_mut(&id)
                .ok_or_else(|| thrown("The map save request was released"))?
                .saved = true;
            Ok(obj([("ok", IpcValue::Bool(true)), ("path", string(path))]))
        })
        .and_then(|result| result);
    app.release_direct_save(id);
    responder.reply(result);
}

fn fail(app: &MindmapApp, id: u64, responder: Responder, error: IpcError) {
    app.finish_owned_save(id, Err(error.clone()));
    responder.reply(Err(error));
}
pub(super) fn request_id(raw: &str) -> Result<u64, IpcError> {
    raw.parse::<u64>()
        .map_err(|_| thrown("The map save request identifier is invalid"))
}

/// `MINDMAP_CHANNELS.save` takes `saveWaiters.get(wcId)` and resumes it with
/// the call's outcome: only a written map is a save, not a cancel, a Save As
/// copy or a failure.
fn resume_menu_save(app: &MindmapApp, view: ViewId, responder: Responder) -> Responder {
    let core = app.0.services.core.clone();
    Responder::new(move |reply| {
        let saved = matches!(
            &reply,
            Ok(value) if field(value, "ok") == &IpcValue::Bool(true) && has_key(value, "path")
        );
        core.post(Box::new(move || {
            super::with_app(|app| {
                let waiter = app.0.state.borrow_mut().save_waiters.take(view);
                if let Some(waiter) = waiter {
                    waiter(Wait::Answer(saved));
                }
            })
        }));
        responder.reply(reply);
    })
}

pub(super) fn save(app: &MindmapApp, cx: &CallContext, args: Args, responder: Responder) {
    let responder = resume_menu_save(app, cx.sender.view, responder);
    let value = args.get(0);
    if !app.may_write(cx.sender.view) {
        return responder.reply(Ok(obj([
            ("ok", IpcValue::Bool(false)),
            (
                "error",
                string("mindmap: the file did not open, nothing is written"),
            ),
        ])));
    }
    let mode = if as_str(field(value, "mode")) == Some("saveAs") {
        SaveMode::SaveAs
    } else {
        SaveMode::Save
    };
    let id = match as_str(field(value, "ownedSaveRequestId")) {
        Some(raw) => match request_id(raw) {
            Ok(id) => id,
            Err(error) => return responder.reply(Err(error)),
        },
        None => {
            let request = match native_owner(cx)
                .and_then(|origin| app.capture_save(cx.sender.view, origin, cx.cancel.clone()))
            {
                Ok(request) => request,
                Err(error) => return responder.reply(Err(error)),
            };
            app.insert_save(request, mode, None, None)
        }
    };
    let request = match app.pending_for(cx, id) {
        Ok(value) => value,
        Err(error) => return fail(app, id, responder, error),
    };
    let accepted = request.effect(|| {
        let mut state = app.0.state.borrow_mut();
        let revision = state
            .revisions
            .get(&cx.sender.view)
            .copied()
            .ok_or_else(|| thrown("The map document was released"))?;
        let baseline = state.save_path.get(&cx.sender.view).cloned();
        let pending = state
            .owned_saves
            .get_mut(&id)
            .ok_or_else(|| thrown("The map save request was released"))?;
        if mode != pending.mode || pending.revision.is_some() {
            return Err(thrown(
                "The map save response was duplicated or has the wrong mode",
            ));
        }
        pending.revision = Some(revision);
        pending.baseline = baseline;
        as_bytes(field(value, "bytes"))
            .map(Vec::from)
            .ok_or_else(|| thrown("mindmap: bad save request"))
    });
    let bytes = match accepted {
        Ok(bytes) => bytes,
        Err(error) => return fail(app, id, responder, error),
    };
    let engine = app.0.engine.clone();
    let cx = cx.clone();
    let value = value.clone();
    app.offload(
        move || engine.open_xmind(&bytes).map(|opened| (opened, bytes)),
        move |app, checked| match checked {
            Ok((opened, bytes)) if opened.status == OpenStatus::Ok => {
                app.resolve_owned_target(cx, id, value, bytes, responder)
            }
            Ok((opened, _)) => {
                // Electron answers `{ ok: false, error }` (only an engine failure throws)
                let result = app.pending_for(&cx, id).and_then(|request| {
                    request.effect(|| {
                        Ok(obj([
                            ("ok", IpcValue::Bool(false)),
                            (
                                "error",
                                string(format!(
                                    "mindmap: refusing to write a file that would not open ({})",
                                    opened.status.as_str()
                                )),
                            ),
                        ]))
                    })
                });
                app.finish_owned_save(id, Ok(false));
                responder.reply(result);
            }
            Err(error) => fail(app, id, responder, thrown(error)),
        },
    );
}

pub(super) fn native_owner(cx: &CallContext) -> Result<NativeRequestOwner, IpcError> {
    let context = cx
        .native
        .clone()
        .ok_or_else(|| thrown("The request has no native document authority"))?;
    if !context
        .renderer()
        .is_some_and(|renderer| renderer.is_active())
        || cx.cancel.is_cancelled()
    {
        return Err(thrown("The requesting document was released"));
    }
    Ok(NativeRequestOwner {
        view: cx.sender.view,
        context,
    })
}
