//! Native saves retain origin, renderer, document and operation authority across
//! serialization, dialogs, worker writes, recovery cleanup and path publication.

use std::path::PathBuf;
use std::time::Duration;

use main_core::packages::electron_utils::dialog_memory::{
    prepare_dialog_options, remember_save_path,
};
use shell_contract::editors::{SaveMode, Then};
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{CallContext, NativeRequestOwner, Responder};
use shell_contract::runtime::{CancelSubscription, CancelToken, RendererGeneration};
use shell_contract::{Batch, IpcError, IpcValue, ViewId};

use super::DiagramViews;

pub(super) mod close;
#[cfg(test)]
mod publication_tests;
use super::formats::{DiagramFormat, format_for_save_path, format_of_path, with_format_extension};
use crate::owned_save::{OwnedSave, SaveAuthority};
use crate::value::{Args, as_bytes, field, obj, string, thrown};

fn with_recovery<R>(
    shared: &crate::host::RecoveryStateHost,
    dir: &str,
    action: impl FnOnce(&main_core::packages::electron_utils::recovery_store::RecoveryStore<'_>) -> R,
) -> R {
    let shared = shared.lock().unwrap_or_else(|error| error.into_inner());
    let user_data_dir = || dir.to_owned();
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |duration| duration.as_millis() as f64)
    };
    let store = main_core::packages::electron_utils::recovery_store::create_recovery_store(
        main_core::packages::electron_utils::recovery_store::RecoveryStoreOptions {
            user_data_dir: &user_data_dir,
            now_ms: &now_ms,
            random_uuid: &jscompat::random_uuid,
        },
        &shared,
    );
    action(&store)
}

#[derive(Clone)]
enum RenameAuthority {
    Owned(NativeRequestOwner),
    Native(
        shell_contract::ipc::NativeSaveRevisionSnapshot,
        std::path::PathBuf,
    ),
    Recovery {
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: NativeRequestOwner,
    },
}

#[derive(Clone)]
struct RenameScope {
    authority: RenameAuthority,
    destinations: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
}

impl RenameScope {
    fn effect<T>(&self, action: impl FnOnce() -> Result<T, IpcError>) -> Result<T, IpcError> {
        let mut action = Some(action);
        let mut output = None;
        let mut failure = None;
        let mut effect = || {
            if let RenameAuthority::Native(snapshot, expected) = &self.authority {
                if snapshot.selection().path() != expected {
                    return Err("The native save destination changed".into());
                }
            }
            let run = action
                .take()
                .ok_or_else(|| "The rename effect was already consumed".to_owned())?;
            match run() {
                Ok(value) => {
                    output = Some(value);
                    Ok(())
                }
                Err(error) => {
                    let message = error.message.clone();
                    failure = Some(error);
                    Err(message)
                }
            }
        };
        let admitted = match &self.authority {
            RenameAuthority::Owned(origin) => self.effects.with_owned_effect_many_extra(
                origin,
                &self.destinations,
                &self.cancel,
                &self.pending,
                &self.additional.iter().collect::<Vec<_>>(),
                &mut effect,
            ),
            RenameAuthority::Native(snapshot, _) => {
                self.effects.with_native_save_revision_snapshot_effect(
                    snapshot,
                    &self.destinations,
                    &self.cancel,
                    &self.pending,
                    &self.additional.iter().collect::<Vec<_>>(),
                    &mut effect,
                )
            }
            RenameAuthority::Recovery { receipt, receiver } => {
                self.effects.with_recovery_open_effect_many(
                    receipt,
                    receiver,
                    &self.destinations,
                    &self.cancel,
                    &self.pending,
                    &mut effect,
                )
            }
        };
        if let Some(error) = failure {
            return Err(error);
        }
        admitted.map_err(thrown)?;
        output.ok_or_else(|| thrown("The native rename effect did not run"))
    }

    fn check(&self) -> Result<(), IpcError> {
        self.effect(|| Ok(()))
    }

    fn emit(&self, channel: &str, args: Vec<IpcValue>) -> Result<(), IpcError> {
        match &self.authority {
            RenameAuthority::Owned(origin) => self.effects.emit_owned_many_extra(
                origin,
                &self.destinations,
                0,
                &self.cancel,
                &self.pending,
                &self.additional.iter().collect::<Vec<_>>(),
                channel,
                args,
            ),
            RenameAuthority::Native(snapshot, expected)
                if snapshot.selection().path() == expected =>
            {
                self.effects.emit_native_save_revision_snapshot(
                    snapshot,
                    &self.destinations,
                    0,
                    &self.cancel,
                    &self.pending,
                    &self.additional.iter().collect::<Vec<_>>(),
                    channel,
                    args,
                )
            }
            RenameAuthority::Native(_, _) => Err("The native save destination changed".into()),
            RenameAuthority::Recovery { receipt, receiver } => {
                self.effects.emit_recovery_open_many(
                    receipt,
                    receiver,
                    &self.destinations,
                    0,
                    &self.cancel,
                    &self.pending,
                    channel,
                    args,
                )
            }
        }
        .map_err(thrown)
    }
}

pub(super) struct PendingSave {
    scope: OwnedSave,
    target: Option<(String, DiagramFormat)>,
    native_destination: Option<shell_contract::ipc::NativeSaveDestination>,
    target_token: String,
    saving: bool,
    then: Then<bool>,
    _subscriptions: Vec<CancelSubscription>,
    owns_pending: bool,
}

impl PendingSave {
    pub(super) fn cancel_and_finish(self) {
        if self.owns_pending {
            self.scope.pending.cancel();
        }
        (self.then)(Err(thrown("The diagram view was closed")));
    }
}

pub(super) struct PendingPublication {
    scope: OwnedSave,
    session: shell_contract::hosts::SavedSessionReceipt,
    request: Option<String>,
    reply: Responder,
    _cancel: CancelSubscription,
}

impl PendingPublication {
    pub(super) fn cancel_and_finish(self) {
        if self.request.is_none() {
            self.scope.pending.cancel();
        }
        self.reply
            .reply(Err(thrown("The diagram publication was cancelled")));
    }
}

impl DiagramViews {
    pub(super) fn replace_save_document(&self, view: ViewId) {
        self.cancel_recovery_open(view);
        let (previous, queued) = self.with_state(|state| {
            let entry = state.view(view);
            entry.first_save_running = false;
            let queued = std::mem::take(&mut entry.first_save_queue);
            (entry.document.replace(RendererGeneration::new()), queued)
        });
        if let Some(previous) = previous {
            previous.revoke();
        }
        for (_, _, reply) in queued {
            reply.reply(Err(thrown("The diagram document was replaced")));
        }
    }

    fn finish_owned_save(&self, view: ViewId, id: &str, result: Result<bool, IpcError>) {
        let pending = self.with_state(|state| state.views.get_mut(&view)?.owned_saves.remove(id));
        if let Some(pending) = pending {
            if pending.owns_pending {
                pending.scope.pending.cancel();
            }
            (pending.then)(result);
        }
    }

    pub(super) fn request_owned_save(
        &self,
        view: ViewId,
        mode: SaveMode,
        path: Option<PathBuf>,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<bool>,
    ) {
        self.request_owned_save_in(view, mode, path, Some(origin), cancel, None, then);
    }

    fn save_loss_subscriptions(
        &self,
        view: ViewId,
        id: &str,
        scope: &OwnedSave,
        owns_pending: bool,
    ) -> Vec<CancelSubscription> {
        let mut losses = vec![scope.cancel.clone(), scope.document.loss_token()];
        if let Some(loss) = scope.authority.loss_token() {
            losses.push(loss);
        }
        for owner in scope.participants(&[]) {
            if let Some(loss) = owner.loss_token() {
                losses.push(loss);
            }
        }
        let instance = self.0.id;
        losses
            .into_iter()
            .map(|loss| {
                let pending = scope.pending.clone();
                let core = self.0.services.core.clone();
                let id = id.to_owned();
                loss.on_cancel(move || {
                    if owns_pending {
                        pending.cancel();
                    }
                    core.post(Box::new(move || {
                        if let Some(views) = super::lookup(instance) {
                            views.finish_owned_save(
                                view,
                                &id,
                                Err(thrown("The save owner was released")),
                            );
                        }
                    }));
                })
            })
            .collect()
    }

    pub(super) fn request_native_save(
        &self,
        view: ViewId,
        mode: SaveMode,
        path: Option<PathBuf>,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<bool>,
    ) {
        let prepared = (|| {
            use shell_contract::ipc::{
                NativeEditorCommand, NativeInvocationSelection, NativeSaveSelection,
            };
            match (binding.operation().selection(), &path) {
                (
                    NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
                        surface: shell_contract::Surface::Diagram,
                        view: selected_view,
                        selection: NativeSaveSelection::Mode(selected_mode),
                    }),
                    None,
                ) if *selected_view == view && *selected_mode == mode => {}
                (
                    NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
                        surface: shell_contract::Surface::Diagram,
                        view: selected_view,
                        selection: NativeSaveSelection::ExactPath(selected),
                    }),
                    Some(path),
                ) if *selected_view == view && selected == path => {}
                _ => {
                    return Err(thrown(
                        "The native save selection does not match the diagram",
                    ));
                }
            }
            if binding.owner().view != view {
                return Err(thrown("The native save belongs to another view"));
            }
            let document = self
                .with_state(|state| state.views.get(&view)?.document.clone())
                .ok_or_else(|| thrown("The diagram document is unavailable"))?;
            let effects = self
                .0
                .services
                .hosts
                .need::<OwnedEffectsHost>(Batch::D1)
                .map_err(|_| thrown("Native save effects are unavailable"))?;
            OwnedSave::new_native(binding, cancel.clone(), pending, document, effects)
        })();
        match prepared {
            Ok(scope) => {
                self.request_owned_save_in(view, mode, path, None, cancel, Some(scope), then)
            }
            Err(error) => then(Err(error)),
        }
    }

    fn request_owned_save_in(
        &self,
        view: ViewId,
        mode: SaveMode,
        path: Option<PathBuf>,
        origin: Option<NativeRequestOwner>,
        cancel: CancelToken,
        captured: Option<OwnedSave>,
        then: Then<bool>,
    ) {
        let owns_pending = captured.is_none();
        let begin = (|| {
            let (document, current, dirty, writable) = self
                .with_state(|state| {
                    let entry = state.views.get(&view)?;
                    Some((
                        entry.document.clone()?,
                        entry.save_path.clone(),
                        entry.dirty,
                        entry.may_write(),
                    ))
                })
                .ok_or_else(|| thrown("The diagram is no longer open"))?;
            if !writable {
                return Err(thrown(
                    "The file did not open, so there is no diagram to save",
                ));
            }
            let scope = match captured {
                Some(scope) => {
                    if !scope.document.same_generation(&document) {
                        return Err(thrown("The diagram document was replaced"));
                    }
                    scope.check()?;
                    scope
                }
                None => {
                    let effects = self
                        .0
                        .services
                        .hosts
                        .need::<OwnedEffectsHost>(Batch::D1)
                        .map_err(|_| thrown("Native save effects are unavailable"))?;
                    let generation = self
                        .0
                        .services
                        .views
                        .renderer_generation(view)
                        .ok_or_else(|| thrown("The diagram renderer is unavailable"))?;
                    let destination = effects.capture_owner(view, &generation).map_err(thrown)?;
                    OwnedSave::new(
                        origin.ok_or_else(|| thrown("The renderer save owner is unavailable"))?,
                        destination,
                        cancel,
                        document,
                        effects,
                    )?
                }
            };
            let explicit = match path {
                Some(path) if path.is_absolute() => Some(
                    path.to_str()
                        .ok_or_else(|| thrown("The save path is not valid Unicode"))?
                        .to_owned(),
                ),
                Some(_) => return Err(thrown("The save path must be absolute")),
                None => None,
            };
            let target = explicit
                .clone()
                .or_else(|| (mode == SaveMode::Save).then(|| current.clone()).flatten());
            let clean = !dirty
                && target
                    .as_ref()
                    .is_some_and(|target| current.as_ref() == Some(target));
            Ok((scope, target, current, clean, explicit.is_some()))
        })();
        let (mut scope, target, current, clean, explicit) = match begin {
            Ok(value) => value,
            Err(error) => return then(Err(error)),
        };
        if clean && mode == SaveMode::Save {
            return then(scope.effect(|| Ok(true)));
        }
        let id = self.0.shell.save_token();
        let native_destination = match (&scope.authority, target.as_ref()) {
            (SaveAuthority::Native(binding), Some(path)) => {
                let cap = scope.effect(|| {
                    use shell_contract::ipc::{
                        NativeEditorCommand, NativeInvocationSelection, NativeSaveDestination,
                        NativeSaveSelection,
                    };
                    match binding.operation().selection() {
                        NativeInvocationSelection::NativeCommand(
                            NativeEditorCommand::SaveDocument {
                                selection: NativeSaveSelection::ExactPath(_),
                                ..
                            },
                        ) => NativeSaveDestination::from_exact_selection(binding.clone())
                            .map_err(thrown),
                        NativeInvocationSelection::NativeCommand(
                            NativeEditorCommand::SaveDocument {
                                selection: NativeSaveSelection::Mode(SaveMode::Save),
                                ..
                            },
                        ) => {
                            if !self.with_state(|state| {
                                state
                                    .views
                                    .get(&view)
                                    .and_then(|entry| entry.save_path.as_deref())
                                    .is_some_and(|current| current == path)
                            }) {
                                return Err(thrown("The diagram save path was replaced"));
                            }
                            NativeSaveDestination::from_current_document(
                                binding.clone(),
                                PathBuf::from(path),
                            )
                            .map_err(thrown)
                        }
                        _ => Err(thrown(
                            "The native diagram save has no selected destination",
                        )),
                    }
                });
                match cap {
                    Ok(cap) => Some(cap),
                    Err(error) => return then(Err(error)),
                }
            }
            _ => None,
        };
        if let Some(destination) = &native_destination {
            if let Err(error) = scope.retain_native_selection(destination) {
                return then(Err(error));
            }
        }
        if self.with_state(|state| {
            state
                .views
                .get(&view)
                .is_some_and(|entry| entry.owned_saves.contains_key(&id))
        }) {
            return then(Err(thrown("Duplicate diagram save request")));
        }
        let token = self.0.shell.save_token();
        let subscriptions = self.save_loss_subscriptions(view, &id, &scope, owns_pending);
        let selected = target.map(|path| {
            let format = format_of_path(&path);
            (path, format)
        });
        self.with_state(|state| {
            state.view(view).owned_saves.insert(
                id.clone(),
                PendingSave {
                    scope: scope.clone(),
                    target: selected,
                    native_destination,
                    target_token: token,
                    saving: false,
                    then,
                    _subscriptions: subscriptions,
                    owns_pending,
                },
            );
        });
        let timeout_id = id.clone();
        self.after(Duration::from_secs(120), move |views| {
            views.finish_owned_save(
                view,
                &timeout_id,
                Err(thrown("Timed out saving the diagram")),
            )
        });
        if scope.check().is_err() {
            return self.finish_owned_save(view, &id, Err(thrown("The save owner was released")));
        }
        if self.with_state(|state| {
            state
                .view(view)
                .owned_saves
                .get(&id)
                .is_some_and(|pending| pending.target.is_some())
        }) {
            self.emit_owned_save(view, &id, if explicit { "retarget" } else { "save" });
            return;
        }
        let (options, current_format) = match self.save_options(view, current.as_deref(), None) {
            Ok(options) => options,
            Err(error) => return self.finish_owned_save(view, &id, Err(thrown(error))),
        };
        let options = {
            let memory = self
                .0
                .shared
                .dialog_memory
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            prepare_dialog_options(&memory, &options, None)
        };
        let options = crate::host::file_dialog(&options);
        let core = self.0.services.core.clone();
        let dialog_scope = scope.clone();
        let deliver = Box::new(move |result| {
            core.post(Box::new(move || {
                let Some(views) = super::lookup(instance) else {
                    return;
                };
                let selected = match result {
                    Ok(Some(selected)) => selected,
                    Ok(None) => return views.finish_owned_save(view, &id, Ok(false)),
                    Err(error) => return views.finish_owned_save(view, &id, Err(thrown(error))),
                };
                let path = match selected.path().to_str() {
                    Some(path) => path.to_owned(),
                    None => {
                        return views.finish_owned_save(
                            view,
                            &id,
                            Err(thrown("The save path is not valid Unicode")),
                        );
                    }
                };
                if let Err(error) = scope.check() {
                    return views.finish_owned_save(view, &id, Err(error));
                }
                let format = format_for_save_path(&path, current_format);
                let target = with_format_extension(&path, format);
                let native_destination = match selected {
                    crate::owned_save::SaveDestination::Renderer(_) => None,
                    crate::owned_save::SaveDestination::Native(destination) => {
                        use shell_contract::ipc::NativeSaveFormat;
                        let format = match format {
                            DiagramFormat::Drawio => NativeSaveFormat::DiagramDrawio,
                            DiagramFormat::Dio => NativeSaveFormat::DiagramDio,
                            DiagramFormat::DrawioSvg => NativeSaveFormat::DiagramSvg,
                            DiagramFormat::DrawioPng => NativeSaveFormat::DiagramPng,
                        };
                        match destination.normalize_dialog_format(format) {
                            Ok(destination)
                                if destination.path() == std::path::Path::new(&target) =>
                            {
                                Some(destination)
                            }
                            Ok(_) => {
                                return views.finish_owned_save(
                                    view,
                                    &id,
                                    Err(thrown(
                                        "The normalized diagram destination does not match",
                                    )),
                                );
                            }
                            Err(error) => {
                                return views.finish_owned_save(view, &id, Err(thrown(error)));
                            }
                        }
                    }
                };
                let mut retained_scope = scope.clone();
                if let Some(destination) = &native_destination {
                    if let Err(error) = retained_scope.retain_native_selection(destination) {
                        return views.finish_owned_save(view, &id, Err(error));
                    }
                }
                let result = scope.effect(|| {
                    remember_save_path(
                        &mut views
                            .0
                            .shared
                            .dialog_memory
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()),
                        &path,
                    );
                    views.with_state(|state| {
                        if let Some(pending) = state.view(view).owned_saves.get_mut(&id) {
                            pending.target = Some((target, format));
                            pending.scope = retained_scope;
                            pending.native_destination = native_destination;
                        }
                    });
                    Ok(())
                });
                if let Err(error) = result {
                    return views.finish_owned_save(view, &id, Err(error));
                }
                views.emit_owned_save(view, &id, "saveAs");
            }));
        });
        dialog_scope.save_dialog(&self.0.dialogs, self.window_of(view), options, deliver);
    }

    fn emit_owned_save(&self, view: ViewId, id: &str, mode: &str) {
        let scope = self.with_state(|state| {
            state
                .views
                .get(&view)?
                .owned_saves
                .get(id)
                .map(|pending| pending.scope.clone())
        });
        let Some(scope) = scope else {
            return;
        };
        let result = scope.check().and_then(|_| {
            scope.emit(
                &scope.destination,
                "diagram:save-request",
                vec![string(mode), string(id)],
            )
        });
        if let Err(error) = result {
            self.finish_owned_save(view, id, Err(error));
        }
    }

    pub(super) fn owned_save_target(
        &self,
        cx: &CallContext,
        args: &Args,
    ) -> Result<IpcValue, IpcError> {
        let IpcValue::String(id) = field(args.get(0), "requestId") else {
            return Err(thrown("Missing save request identity"));
        };
        let scope = self
            .with_state(|state| {
                state
                    .views
                    .get(&cx.sender.view)?
                    .owned_saves
                    .get(id)
                    .map(|pending| pending.scope.clone())
            })
            .ok_or_else(|| thrown("Unknown owned save request"))?
            .capture_callback(cx)?;
        scope.effect(|| {
            self.with_state(|state| {
                let pending = state
                    .views
                    .get(&cx.sender.view)
                    .and_then(|entry| entry.owned_saves.get(id))
                    .ok_or_else(|| thrown("Unknown owned save request"))?;
                let (path, format) = pending
                    .target
                    .as_ref()
                    .ok_or_else(|| thrown("The save target is not ready"))?;
                Ok(obj([
                    ("ok", IpcValue::Bool(true)),
                    ("token", string(&pending.target_token)),
                    ("path", string(path)),
                    ("format", string(format.as_str())),
                ]))
            })
        })
    }

    pub(super) fn owned_save_ack(
        &self,
        cx: &CallContext,
        id: &str,
        ok: bool,
    ) -> Result<(), IpcError> {
        let scope = self
            .with_state(|state| {
                state
                    .views
                    .get(&cx.sender.view)?
                    .owned_saves
                    .get(id)
                    .map(|pending| pending.scope.clone())
            })
            .ok_or_else(|| thrown("Unknown owned save request"))?;
        let scope = scope.capture_callback(cx)?;
        scope.effect(|| {
            self.with_state(|state| {
                let pending = state
                    .views
                    .get_mut(&cx.sender.view)
                    .and_then(|entry| entry.owned_saves.get_mut(id))
                    .ok_or_else(|| thrown("Unknown owned save request"))?;
                pending.scope = scope.clone();
                Ok(())
            })
        })?;
        if !ok {
            self.finish_owned_save(cx.sender.view, id, Ok(false));
        }
        Ok(())
    }

    pub(super) fn save_dispatch(&self, cx: &CallContext, args: Args, reply: Responder) {
        let IpcValue::String(id) = field(args.get(0), "requestId") else {
            if field(args.get(0), "requestId") != &IpcValue::Undefined {
                return reply.reply(Err(thrown("Invalid diagram save request identity")));
            }
            return reply.reply(self.save(cx, args));
        };
        let id = id.clone();
        let view = cx.sender.view;
        let request = args.get(0);
        let prepared = self.with_state(|state| {
            let pending = state.views.get(&view)?.owned_saves.get(&id)?;
            Some((
                pending.scope.clone(),
                pending.target.clone()?,
                pending.target_token.clone(),
            ))
        });
        let Some((scope, (path, _), token)) = prepared else {
            return reply.reply(Err(thrown("Unknown owned save request")));
        };
        let scope = match scope.capture_callback(cx) {
            Ok(scope) => scope,
            Err(error) => {
                self.finish_owned_save(view, &id, Err(error.clone()));
                return reply.reply(Err(error));
            }
        };
        if field(request, "token") != &IpcValue::String(token) {
            return reply.reply(Err(thrown("The save target belongs to another request")));
        }
        let Some(bytes) = as_bytes(field(request, "bytes")) else {
            return reply.reply(Err(thrown("diagram: bad save request")));
        };
        let admitted = scope.effect(|| {
            Ok(self.with_state(|state| {
                let Some(pending) = state.view(view).owned_saves.get_mut(&id) else {
                    return false;
                };
                if pending.saving {
                    return false;
                }
                pending.saving = true;
                pending.scope = scope.clone();
                true
            }))
        });
        match admitted {
            Ok(true) => {}
            Ok(false) => return reply.reply(Err(thrown("The owned save was already consumed"))),
            Err(error) => return reply.reply(Err(error)),
        }
        let bytes = bytes.to_vec();
        let current = self.file_path(view);
        let recorded = self.with_state(|state| state.view(view).disk_states.get(&path).cloned());
        let elsewhere =
            current.as_deref() == Some(path.as_str()) && self.0.shell.is_path_open_elsewhere(&path);
        let auto = field(request, "auto") == &IpcValue::Bool(true);
        let core = self.0.services.core.clone();
        let instance = self.0.id;
        let engine = self.0.engine.clone();
        self.0.services.workers.spawn(Box::new(move || {
            let validation =
                engine.open_diagram(&jscompat::node_path::host::basename(&path, None), &bytes);
            let conflict = elsewhere
                && main_core::packages::electron_utils::save_conflict::is_modified_since(
                    recorded.as_ref(),
                    &path,
                );
            core.post(Box::new(move || {
                let Some(views) = super::lookup(instance) else {
                    return reply.reply(Err(thrown("The diagram editor was released")));
                };
                match validation {
                    Err(error) => {
                        views.finish_owned_save(view, &id, Err(thrown(error.clone())));
                        return reply.reply(Err(thrown(error)));
                    }
                    Ok(Err(check)) => {
                        views.finish_owned_save(view, &id, Ok(false));
                        return reply.reply(Ok(obj([
                            ("ok", IpcValue::Bool(false)),
                            (
                                "error",
                                string(format!(
                                    "diagram: refusing to write a file that would not open ({})",
                                    check.reason
                                )),
                            ),
                        ])));
                    }
                    Ok(Ok(_)) => {}
                }
                if conflict && auto {
                    views.finish_owned_save(view, &id, Ok(false));
                    return reply.reply(Ok(obj([
                        ("ok", IpcValue::Bool(false)),
                        ("error", string("external-modified")),
                    ])));
                }
                if !conflict {
                    return views.write_owned_diagram(view, id, scope, path, bytes, current, reply);
                }
                let labels =
                    main_core::packages::electron_utils::save_conflict_labels::save_conflict_labels(
                        views.lang().as_str(),
                    );
                let options = shell_contract::hosts::MessageBox {
                    kind: Some("warning".into()),
                    message: labels.message.into(),
                    detail: Some(labels.detail.into()),
                    buttons: vec![labels.overwrite.into(), labels.cancel.into()],
                    default_id: Some(0),
                    cancel_id: Some(1),
                    ..Default::default()
                };
                let dialogs = views.0.dialogs.clone();
                let parent = views.window_of(view);
                let core = views.0.services.core.clone();
                let next_scope = scope.clone();
                let deliver = Box::new(
                    move |result: Result<shell_contract::hosts::MessageBoxAnswer, String>| {
                        core.post(Box::new(move || {
                            let Some(views) = super::lookup(instance) else {
                                return reply.reply(Err(thrown("The diagram editor was released")));
                            };
                            let answer = match result {
                                Ok(answer) => answer,
                                Err(error) => {
                                    views.finish_owned_save(view, &id, Err(thrown(error.clone())));
                                    return reply.reply(Err(thrown(error)));
                                }
                            };
                            if answer.response != 0 {
                                views.finish_owned_save(view, &id, Ok(false));
                                return reply.reply(Ok(obj([
                                    ("ok", IpcValue::Bool(false)),
                                    ("error", string("external-modified")),
                                ])));
                            }
                            views.write_owned_diagram(
                                view, id, next_scope, path, bytes, current, reply,
                            );
                        }));
                    },
                );
                scope.message_dialog(&dialogs, parent, options, deliver);
            }));
        }));
    }

    fn write_owned_diagram(
        &self,
        view: ViewId,
        id: String,
        scope: OwnedSave,
        path: String,
        bytes: Vec<u8>,
        current: Option<String>,
        reply: Responder,
    ) {
        let native_destination = match &scope.authority {
            SaveAuthority::Renderer(_) => None,
            SaveAuthority::Native(_) => {
                let destination = self.with_state(|state| {
                    state
                        .views
                        .get(&view)
                        .and_then(|entry| entry.owned_saves.get(&id))
                        .and_then(|pending| pending.native_destination.clone())
                });
                match destination {
                    Some(destination) => Some(destination),
                    None => {
                        let error = thrown("The native diagram save destination was released");
                        self.finish_owned_save(view, &id, Err(error.clone()));
                        return reply.reply(Err(error));
                    }
                }
            }
        };
        let shell = self.0.shell.clone();
        let core = self.0.services.core.clone();
        let instance = self.0.id;
        let pending_scope = scope.clone();
        self.0.services.workers.spawn(Box::new(move || {
            let result = (|| -> Result<(), IpcError> {
                scope.check()?;
                main_core::packages::electron_utils::atomic_write::atomic_write_file_authorized(
                    &path,
                    &bytes,
                    &mut |effect| {
                        match &native_destination {
                            Some(destination) => scope.effect_destination(
                                destination,
                                std::path::Path::new(&path),
                                || {
                                    effect();
                                    Ok(())
                                },
                            ),
                            None => scope.effect(|| {
                                effect();
                                Ok(())
                            }),
                        }
                        .map_err(|error| main_core::node_fs::NodeError {
                            code: None,
                            message: error.message,
                        })
                    },
                )
                .map_err(|error| thrown(error.message))
            })();
            core.post(Box::new(move || {
                let Some(views) = super::lookup(instance) else {
                    return reply.reply(Err(thrown("The diagram editor was released")));
                };
                if let Err(error) = result {
                    views.finish_owned_save(view, &id, Ok(false));
                    return reply.reply(Err(error));
                }
                if current.as_deref() != Some(path.as_str()) {
                    let views = views.clone();
                    let next_scope =
                        std::rc::Rc::new(std::cell::RefCell::new(pending_scope.clone()));
                    let on_scope = next_scope.clone();
                    let on_views = views.clone();
                    let on_id = id.clone();
                    let on_revision = std::rc::Rc::new(
                        move |receipt: &shell_contract::ipc::NativeSaveRevisionReceipt| {
                            let mut updated = on_scope.borrow().clone();
                            updated
                                .apply_revision_receipt(receipt)
                                .map_err(|error| error.message)?;
                            let owns_pending = on_views
                                .with_state(|state| {
                                    state
                                        .views
                                        .get(&view)
                                        .and_then(|entry| entry.owned_saves.get(&on_id))
                                        .filter(|pending| {
                                            pending.scope.pending.same_token(&updated.pending)
                                        })
                                        .map(|pending| pending.owns_pending)
                                })
                                .ok_or("The diagram save request was released")?;
                            let subscriptions = on_views.save_loss_subscriptions(
                                view,
                                &on_id,
                                &updated,
                                owns_pending,
                            );
                            on_views.with_state(|state| {
                                let pending = state
                                    .views
                                    .get_mut(&view)
                                    .and_then(|entry| entry.owned_saves.get_mut(&on_id))
                                    .filter(|pending| {
                                        pending.scope.pending.same_token(&updated.pending)
                                    })
                                    .ok_or("The diagram save request was released")?;
                                let mut replacement = pending.scope.clone();
                                replacement
                                    .apply_revision_receipt(receipt)
                                    .map_err(|error| error.message)?;
                                pending.native_destination =
                                    Some(receipt.snapshot().selection().clone());
                                pending.scope = replacement;
                                pending._subscriptions = subscriptions;
                                Ok::<(), String>(())
                            })?;
                            *on_scope.borrow_mut() = updated;
                            Ok(())
                        },
                    );
                    let done = Box::new(move |result| match result {
                        Ok(saved) => views.complete_owned_diagram(
                            view,
                            Some(id),
                            next_scope.borrow().clone(),
                            saved,
                            current,
                            reply,
                        ),
                        Err(error) => {
                            views.finish_owned_save(view, &id, Ok(false));
                            reply.reply(Err(thrown(error)));
                        }
                    });
                    match (&pending_scope.authority, native_destination) {
                        (SaveAuthority::Renderer(origin), None) => shell.file_saved_owned(
                            origin,
                            &pending_scope.destination,
                            &pending_scope.participants(&[]),
                            &pending_scope.cancel,
                            &pending_scope.pending,
                            &pending_scope.additional,
                            &pending_scope.effects,
                            &path,
                            done,
                        ),
                        (SaveAuthority::Native(_), Some(_)) => match pending_scope.revision.clone()
                        {
                            Some(revision) => shell.file_saved_existing_document(
                                revision,
                                PathBuf::from(&path),
                                pending_scope.participants(&[]),
                                pending_scope.cancel.clone(),
                                pending_scope.pending.clone(),
                                pending_scope.additional.clone(),
                                pending_scope.effects.clone(),
                                on_revision,
                                done,
                            ),
                            None => done(Err("The diagram save revision was released".into())),
                        },
                        _ => done(Err(
                            "The diagram save destination does not match its authority".into(),
                        )),
                    }
                } else {
                    views.complete_owned_diagram(
                        view,
                        Some(id),
                        pending_scope,
                        shell_contract::hosts::SavedFile {
                            path,
                            session: shell_contract::hosts::SavedSessionReceipt::NoPublication,
                        },
                        current,
                        reply,
                    );
                }
            }));
        }));
    }

    fn complete_owned_diagram(
        &self,
        view: ViewId,
        request: Option<String>,
        scope: OwnedSave,
        saved: shell_contract::hosts::SavedFile<String>,
        previous: Option<String>,
        reply: Responder,
    ) {
        let publication_id = self.0.shell.random_uuid();
        if let Err(error) = scope.check() {
            return reply.reply(Err(error));
        }
        if self.with_state(|state| {
            state
                .view(view)
                .owned_publications
                .contains_key(&publication_id)
        }) {
            return reply.reply(Err(thrown("Duplicate diagram publication")));
        }
        let path = saved.path;
        let instance = self.0.id;
        let core = self.0.services.core.clone();
        let cancel_id = publication_id.clone();
        let expected = scope.pending.clone();
        let wait_for_publication = request.is_none();
        let cancel = scope.pending.on_cancel(move || {
            if wait_for_publication {
                return;
            }
            core.post(Box::new(move || {
                let Some(views) = super::lookup(instance) else {
                    return;
                };
                let operation = views.with_state(|state| {
                    let entry = state.views.get_mut(&view)?;
                    if !entry
                        .owned_publications
                        .get(&cancel_id)?
                        .scope
                        .pending
                        .same_token(&expected)
                    {
                        return None;
                    }
                    entry.owned_publications.remove(&cancel_id)
                });
                if let Some(operation) = operation {
                    let error = thrown("The diagram publication was cancelled");
                    operation.reply.reply(Err(error.clone()));
                    if let Some(request) = operation.request {
                        views.finish_owned_save(view, &request, Err(error));
                    }
                }
            }));
        });
        self.with_state(|state| {
            state.view(view).owned_publications.insert(
                publication_id.clone(),
                PendingPublication {
                    scope: scope.clone(),
                    session: saved.session,
                    request,
                    reply,
                    _cancel: cancel,
                },
            );
        });
        let owned = self.with_state(|state| {
            state
                .recovery
                .get(&view)
                .map(|entry| {
                    entry
                        .owned
                        .iter()
                        .map(|(key, identity)| (key.clone(), identity.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        });
        let recovery = self.0.recovery_state.clone();
        let dir = self.user_data_dir();
        let core = self.0.services.core.clone();
        self.0.services.workers.spawn(Box::new(move || {
            let disk = main_core::packages::electron_utils::save_conflict::record_disk_state(&path);
            let deleted = with_recovery(&recovery, &dir, |store| -> Result<Vec<String>, IpcError> {
                let mut removed = Vec::new();
                for (key, identity) in owned {
                    let result = store.delete_copy_authorized(&identity,
                        main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason::Saved,
                        &mut |effect| scope.effect(|| { effect(); Ok(()) }).map_err(|error| main_core::node_fs::NodeError { code: None, message: error.message }));
                    scope.check()?;
                    if matches!(result, Ok(main_core::packages::electron_utils::recovery_store::RecoveryDeleteResult::Deleted)) { removed.push(key); }
                }
                Ok(removed)
            });
            core.post(Box::new(move || {
                let Some(views) = super::lookup(instance) else { return; };
                let operation = views.with_state(|state| {
                    let entry = state.views.get_mut(&view)?;
                    if !entry.owned_publications.get(&publication_id)?.scope.pending.same_token(&scope.pending) { return None; }
                    entry.owned_publications.remove(&publication_id)
                });
                let Some(operation) = operation else { return; };
                let result = deleted.and_then(|deleted| views.adopt_owned_diagram(view, &scope, &path, previous.as_deref(), disk, deleted));
                if let Err(error) = result {
                    operation.reply.reply(Err(error.clone()));
                    if let Some(request) = operation.request { views.finish_owned_save(view, &request, Err(error)); }
                    else { scope.pending.cancel(); }
                    return;
                }
                operation.session.finalize(Box::new(move |result| {
                    let result = result.map_err(thrown).and_then(|()| scope.effect(|| Ok(obj([("ok", IpcValue::Bool(true)), ("path", string(&path))]))));
                    let saved = result.is_ok();
                    operation.reply.reply(result);
                    if let Some(request) = operation.request { views.finish_owned_save(view, &request, Ok(saved)); }
                    else { scope.pending.cancel(); }
                }));
            }));
        }));
    }

    fn adopt_owned_diagram(
        &self,
        view: ViewId,
        scope: &OwnedSave,
        path: &str,
        previous: Option<&str>,
        disk: Option<main_core::packages::electron_utils::save_conflict::SaveDiskState>,
        deleted: Vec<String>,
    ) -> Result<(), IpcError> {
        scope.effect(|| {
            self.with_state(|state| {
                let entry = state
                    .views
                    .get_mut(&view)
                    .ok_or_else(|| thrown("The diagram view was closed"))?;
                entry.save_path = Some(path.to_owned());
                entry.advance_save_revision();
                entry.open_path = Some(path.to_owned());
                entry
                    .allowed
                    .get_or_insert_with(Default::default)
                    .insert(path.to_owned());
                entry.dirty = false;
                entry.clean_path = Some(path.to_owned());
                entry.import = None;
                Ok(())
            })
        })?;
        scope.effect(|| {
            self.with_state(|state| {
                if let Some(entry) = state.recovery.get_mut(&view) {
                    for key in deleted {
                        entry.owned.remove(&key);
                    }
                    entry.untitled_copy = None;
                    entry.save_as_default = None;
                    entry.title = None;
                }
                if let Some(entry) = state.views.get_mut(&view) {
                    if let Some(disk) = disk {
                        entry.disk_states.insert(path.to_owned(), disk);
                    }
                    if let Some(previous) = previous.filter(|previous| *previous != path) {
                        entry.disk_states.remove(previous);
                    }
                }
            });
            Ok(())
        })
    }

    pub(super) fn save_into_project_owned(&self, cx: &CallContext, args: Args, reply: Responder) {
        let view = cx.sender.view;
        let document = self.with_state(|state| {
            state
                .views
                .get(&view)
                .and_then(|entry| entry.document.clone())
        });
        let mut request = Some((cx.clone(), args, reply));
        let queued = self.with_state(|state| {
            let Some(entry) = state.views.get_mut(&view) else {
                return false;
            };
            if entry.first_save_running {
                if let Some(request) = request.take() {
                    entry.first_save_queue.push_back(request);
                }
                true
            } else {
                entry.first_save_running = true;
                false
            }
        });
        if queued {
            return;
        }
        let Some((cx, args, reply)) = request else {
            return;
        };
        let core = self.0.services.core.clone();
        let instance = self.0.id;
        let reply = Responder::new(move |result| {
            core.post(Box::new(move || {
                if let Some(views) = super::lookup(instance) {
                    let next = views.with_state(|state| {
                        state.views.get_mut(&view).and_then(|entry| {
                            if !entry.document.as_ref().zip(document.as_ref()).is_some_and(
                                |(current, expected)| current.same_generation(expected),
                            ) {
                                return None;
                            }
                            entry.first_save_running = false;
                            entry.first_save_queue.pop_front()
                        })
                    });
                    reply.reply(result);
                    if let Some((cx, args, reply)) = next {
                        views.save_into_project_owned(&cx, args, reply);
                    }
                } else {
                    reply.reply(result);
                }
            }));
        });
        self.start_project_save_owned(&cx, args, reply);
    }

    fn start_project_save_owned(&self, cx: &CallContext, args: Args, reply: Responder) {
        let view = cx.sender.view;
        let skipped = || {
            obj([
                ("ok", IpcValue::Bool(false)),
                ("skipped", IpcValue::Bool(true)),
            ])
        };
        let writable =
            self.with_state(|state| state.views.get(&view).is_none_or(|entry| entry.may_write()));
        if !self.0.shell.has_new_document_writer() || !writable || self.file_path(view).is_some() {
            return reply.reply(Ok(skipped()));
        }
        let request = args.get(0);
        let Some(bytes) = as_bytes(field(request, "bytes")) else {
            return reply.reply(Ok(obj([
                ("ok", IpcValue::Bool(false)),
                ("error", string("diagram: bad save request")),
            ])));
        };
        let begin = (|| {
            let native = cx
                .native
                .clone()
                .ok_or_else(|| thrown("The diagram save has no authenticated native owner"))?;
            let effects = self
                .0
                .services
                .hosts
                .need::<OwnedEffectsHost>(Batch::D1)
                .map_err(|_| thrown("Native save effects are unavailable"))?;
            let generation = native
                .renderer()
                .ok_or_else(|| thrown("The diagram renderer is unavailable"))?;
            let destination = effects.capture_owner(view, generation).map_err(thrown)?;
            let document = self
                .with_state(|state| state.views.get(&view)?.document.clone())
                .ok_or_else(|| thrown("The diagram document is unavailable"))?;
            OwnedSave::new(
                NativeRequestOwner {
                    view,
                    context: native,
                },
                destination,
                cx.cancel.clone(),
                document,
                effects,
            )
        })();
        let scope = match begin {
            Ok(scope) => scope,
            Err(error) => return reply.reply(Err(error)),
        };
        let revision = match scope.effect(|| {
            self.with_state(|state| {
                let entry = state
                    .views
                    .get_mut(&view)
                    .ok_or_else(|| thrown("The diagram view was closed"))?;
                if entry.save_path.is_some() {
                    return Err(thrown("The diagram already has a save path"));
                }
                entry.advance_save_revision();
                entry
                    .save_revision
                    .ok_or_else(|| thrown("The diagram save revision was exhausted"))
            })
        }) {
            Ok(revision) => revision,
            Err(error) => return reply.reply(Err(error)),
        };
        let admission_scope = scope.clone();
        let admission_core = self.0.services.core.clone();
        let admission_instance = self.0.id;
        let admission: main_core::packages::electron_utils::atomic_write::owned::Admission =
            std::sync::Arc::new(move |effect| {
                let (send, receive) = std::sync::mpsc::sync_channel(1);
                let scope = admission_scope.clone();
                admission_core.post(Box::new(move || {
                    let result = super::lookup(admission_instance)
                        .ok_or_else(|| thrown("The diagram editor was released"))
                        .and_then(|views| {
                            scope.effect(|| {
                                let valid = views.with_state(|state| {
                                    state.views.get(&view).is_some_and(|entry| {
                                        entry.save_revision == Some(revision)
                                            && entry.save_path.is_none()
                                            && entry.document.as_ref().is_some_and(|document| {
                                                document.same_generation(&scope.document)
                                            })
                                    })
                                });
                                if !valid {
                                    return Err(thrown(
                                        "The diagram changed while the save was pending",
                                    ));
                                }
                                Ok(effect())
                            })
                        });
                    let _ = send.send(result.map_err(|error| main_core::node_fs::NodeError {
                        code: None,
                        message: error.message,
                    }));
                }));
                receive.recv().map_err(|_| main_core::node_fs::NodeError {
                    code: None,
                    message: "The diagram operation dispatcher was released".into(),
                })?
            });
        let name = match field(request, "defaultName") {
            IpcValue::String(name) => Some(name.as_str()),
            _ => None,
        };
        let mut base = super::formats::safe_file_name_base(name);
        if base.is_empty() {
            base = self.tm("untitled");
        }
        let file_name = format!("{base}{}", DiagramFormat::Drawio.extension());
        let bytes = bytes.to_vec();
        let engine = self.0.engine.clone();
        let shell = self.0.shell.clone();
        let core = self.0.services.core.clone();
        let instance = self.0.id;
        let timeout = scope.pending.clone();
        self.after(Duration::from_secs(120), move |_| timeout.cancel());
        self.0.services.workers.spawn(Box::new(move || {
            let result = (|| -> Result<Result<Option<String>, String>, IpcError> {
                scope.check()?;
                if engine
                    .open_diagram(&file_name, &bytes)
                    .map_err(thrown)?
                    .is_err()
                {
                    return Ok(Err(
                        "diagram: refusing to write a diagram that would not open".to_owned(),
                    ));
                }
                let path = shell
                    .write_new_document_owned(
                        scope.authority.renderer()?,
                        &scope.destination,
                        &scope.cancel,
                        &scope.pending,
                        &scope.effects,
                        admission,
                        &file_name,
                        &bytes,
                    )
                    .map_err(thrown)?;
                Ok(Ok(path))
            })();
            core.post(Box::new(move || {
                let Some(views) = super::lookup(instance) else {
                    return reply.reply(Err(thrown("The diagram editor was released")));
                };
                if let Err(error) = scope.check() {
                    return reply.reply(Err(error));
                }
                match result {
                    Ok(Ok(Some(path))) if views.file_path(view).is_none() => {
                        let publication = scope.clone();
                        let target = views.clone();
                        let origin = match scope.authority.renderer() {
                            Ok(owner) => owner.clone(),
                            Err(error) => return reply.reply(Err(error)),
                        };
                        views.0.shell.file_saved_owned(
                            &origin,
                            &scope.destination,
                            &scope.participants(&[]),
                            &scope.cancel,
                            &scope.pending,
                            &scope.additional,
                            &scope.effects,
                            &path,
                            Box::new(move |result| match result {
                                Ok(saved) => target.complete_owned_diagram(
                                    view,
                                    None,
                                    publication,
                                    saved,
                                    None,
                                    reply,
                                ),
                                Err(error) => {
                                    publication.pending.cancel();
                                    reply.reply(Err(thrown(error)));
                                }
                            }),
                        );
                    }
                    Ok(Err(error)) => reply.reply(Ok(obj([
                        ("ok", IpcValue::Bool(false)),
                        ("error", string(error)),
                    ]))),
                    Err(error) => reply.reply(Err(error)),
                    _ => reply.reply(Ok(skipped())),
                }
            }));
        }));
    }

    pub(super) fn rename_owned_diagram(
        &self,
        view: ViewId,
        old_path: &std::path::Path,
        new_path: &std::path::Path,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        self.rename_diagram(
            view,
            old_path,
            new_path,
            RenameAuthority::Owned(origin.clone()),
            destination,
            participants,
            cancel,
            pending,
            additional,
            then,
        );
    }

    pub(super) fn rename_native_diagram(
        &self,
        view: ViewId,
        old_path: &std::path::Path,
        new_path: &std::path::Path,
        snapshot: &shell_contract::ipc::NativeSaveRevisionSnapshot,
        expected_path: &std::path::Path,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        self.rename_diagram(
            view,
            old_path,
            new_path,
            RenameAuthority::Native(snapshot.clone(), expected_path.to_path_buf()),
            destination,
            participants,
            cancel,
            pending,
            additional,
            then,
        );
    }

    pub(super) fn rename_recovery_diagram(
        &self,
        view: ViewId,
        old_path: &std::path::Path,
        new_path: &std::path::Path,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        then: Then<()>,
    ) {
        self.rename_diagram(
            view,
            old_path,
            new_path,
            RenameAuthority::Recovery {
                receipt: receipt.clone(),
                receiver: receiver.clone(),
            },
            destination,
            participants,
            cancel,
            pending,
            &[],
            then,
        );
    }

    fn rename_diagram(
        &self,
        view: ViewId,
        old_path: &std::path::Path,
        new_path: &std::path::Path,
        authority: RenameAuthority,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        let prepared = (|| -> Result<_, IpcError> {
            let old = old_path
                .to_str()
                .ok_or_else(|| thrown("The old path is not valid Unicode"))?;
            let new = new_path
                .to_str()
                .ok_or_else(|| thrown("The new path is not valid Unicode"))?;
            if destination.view != view {
                return Err(thrown("The rename belongs to another view"));
            }
            let document = self
                .with_state(|state| state.views.get(&view)?.document.clone())
                .ok_or_else(|| thrown("The diagram document is unavailable"))?;
            if !destination
                .context
                .document_lease()
                .is_some_and(|expected| expected.same_generation(&document))
            {
                return Err(thrown("The diagram document was replaced"));
            }
            let effects = self
                .0
                .services
                .hosts
                .need::<OwnedEffectsHost>(Batch::D1)
                .map_err(|_| thrown("Native save effects are unavailable"))?;
            let mut destinations = Vec::with_capacity(participants.len() + 1);
            destinations.push(destination.clone());
            destinations.extend_from_slice(participants);
            let scope = RenameScope {
                authority,
                destinations,
                cancel: cancel.clone(),
                pending: pending.clone(),
                additional: additional.to_vec(),
                effects,
            };
            scope.effect(|| {
                self.with_state(|state| {
                    let entry = state
                        .views
                        .get_mut(&view)
                        .ok_or_else(|| thrown("The diagram view was closed"))?;
                    if entry.save_path.as_deref() == Some(old) {
                        entry.save_path = Some(new.to_owned());
                        entry.advance_save_revision();
                    }
                    if entry.open_path.as_deref() == Some(old) {
                        entry.open_path = Some(new.to_owned());
                        entry.advance_save_revision();
                    }
                    if let Some(allowed) = &mut entry.allowed
                        && allowed.contains(old)
                    {
                        allowed.insert(new.to_owned());
                    }
                    if let Some(disk) = entry.disk_states.remove(old) {
                        entry.disk_states.insert(new.to_owned(), disk);
                    }
                    Ok(())
                })
            })?;
            Ok((scope, old.to_owned(), new.to_owned()))
        })();
        let (scope, old, new) = match prepared {
            Ok(value) => value,
            Err(error) => return then(Err(error)),
        };
        let id = self.0.shell.random_uuid();
        let mut then = Some(then);
        if let Err(error) = scope.effect(|| {
            self.with_state(|state| {
                let entry = state
                    .views
                    .get_mut(&view)
                    .ok_or_else(|| thrown("The diagram view was closed"))?;
                if entry.owned_renames.contains_key(&id) {
                    return Err(thrown("Duplicate diagram rename request"));
                }
                entry.owned_renames.insert(
                    id.clone(),
                    then.take()
                        .ok_or_else(|| thrown("The rename callback was already consumed"))?,
                );
                Ok(())
            })
        }) {
            if let Some(then) = then {
                then(Err(error));
            }
            return;
        }
        let owned = self.with_state(|state| {
            state
                .recovery
                .get(&view)
                .map(|entry| entry.owned.clone())
                .unwrap_or_default()
        });
        let recovery = self.0.recovery_state.clone();
        let dir = self.user_data_dir();
        let core = self.0.services.core.clone();
        let instance = self.0.id;
        self.0.services.workers.spawn(Box::new(move || {
            let result = with_recovery(&recovery, &dir, |store| {
                use main_core::packages::electron_utils::recovery_store::{
                    RecoveryDeleteReason, RecoveryDeleteResult, RecoveryWriteOptions,
                };
                scope.check()?;
                let from = super::recovery::file_identity(&old);
                let to = super::recovery::file_identity(&new);
                let from_key = store
                    .copy_path(&from)
                    .map_err(|error| thrown(error.message))?;
                if !owned.contains_key(&from_key) {
                    return Ok((None, None));
                }
                let mut admit = |effect: &mut dyn FnMut()| {
                    scope
                        .effect(|| {
                            effect();
                            Ok(())
                        })
                        .map_err(|error| main_core::node_fs::NodeError {
                            code: None,
                            message: error.message,
                        })
                };
                let mut tracked = None;
                let moved = (|| {
                    if let Some(bytes) = store.read_copy(&from) {
                        tracked = Some((store.copy_path(&to)?, to.clone()));
                        store.write_copy_authorized(
                            &to,
                            &bytes,
                            &RecoveryWriteOptions::default(),
                            &mut admit,
                        )?;
                    }
                    Ok::<_, main_core::node_fs::NodeError>(())
                })();
                scope.check()?;
                if moved.is_err() {
                    return Ok((tracked, None));
                }
                let deleted = store.delete_copy_authorized(
                    &from,
                    RecoveryDeleteReason::Discarded,
                    &mut admit,
                );
                scope.check()?;
                Ok::<_, IpcError>((
                    tracked,
                    matches!(deleted, Ok(RecoveryDeleteResult::Deleted)).then_some(from_key),
                ))
            });
            core.post(Box::new(move || {
                let Some(views) = super::lookup(instance) else {
                    return;
                };
                let then =
                    views.with_state(|state| state.views.get_mut(&view)?.owned_renames.remove(&id));
                let Some(then) = then else {
                    return;
                };
                let result = result
                    .and_then(|(tracked, deleted)| {
                        scope.effect(|| {
                            views.with_state(|state| {
                                let entry = state.recovery_of(view);
                                if let Some((key, identity)) = tracked {
                                    entry.owned.insert(key, identity);
                                }
                                if let Some(key) = deleted {
                                    entry.owned.remove(&key);
                                }
                            });
                            Ok(())
                        })
                    })
                    .and_then(|()| scope.emit("diagram:file-renamed", vec![string(new)]));
                then(result);
            }));
        }));
    }
}
