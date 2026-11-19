use super::app::{MindmapApp, Seed, Unopened};
use super::hosts::OpenStatus;
use crate::value::{buffer, obj, string, thrown};
use main_core::packages::electron_utils::atomic_write::owned::{Admission, admitted};
use main_core::packages::electron_utils::recovery_contract::{
    RecoveryOpenDecision, RecoveryOpenDone, RecoveryOpenFileRequest, RecoveryOpenUntitledRequest,
};
use main_core::packages::electron_utils::recovery_store::{RecoveryIdentity, recovery_copy_path};
use main_core::packages::electron_utils::save_conflict::{SaveDiskState, record_disk_state};
use main_core::shell::renderer_disposal::Failure;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{CallContext, RecoveryOpenTarget, RecoverySeedReceipt};
use shell_contract::runtime::CancelSubscription;
use shell_contract::{IpcError, IpcValue, Surface, ViewId};
use std::collections::HashSet;

pub(super) struct PendingRecovery {
    ticket: u64,
    target: RecoveryOpenTarget,
    authority: shell_contract::ipc::RecoveryAuthoritySnapshot,
    done: RecoveryOpenDone,
    subscriptions: Vec<CancelSubscription>,
}

pub(super) enum PreparedRecovery {
    File {
        path: String,
        response: IpcValue,
        save_path: bool,
        disk: Option<SaveDiskState>,
        owned: Option<(String, RecoveryIdentity)>,
        restored_untitled: bool,
        imported: Option<String>,
    },
    Untitled {
        bytes: Vec<u8>,
        request: RecoveryOpenUntitledRequest,
        copy_path: String,
    },
}

pub(super) struct RecoverySeed {
    target: RecoveryOpenTarget,
    receipt: Option<RecoverySeedReceipt>,
    prepared: PreparedRecovery,
}

pub(super) struct RecoveryApplying {
    token: String,
    receipt: RecoverySeedReceipt,
    document: shell_contract::runtime::RendererGeneration,
    revision: u64,
    _loss: CancelSubscription,
}

pub(super) fn initialized(
    app: &MindmapApp,
    cx: &CallContext,
    args: crate::value::Args,
) -> Result<IpcValue, IpcError> {
    let token = args
        .str(0)
        .ok_or_else(|| thrown("The recovery acknowledgement token is required"))?;
    let ok = matches!(args.get(1), IpcValue::Bool(true));
    let error = args
        .str(2)
        .unwrap_or("The map could not be initialized")
        .to_owned();
    if super::saved_open::initialized(app, cx, token, ok, &error)? {
        return Ok(IpcValue::Null);
    }
    let mut owner = super::owned_save::native_owner(cx)?;
    let effects = effects(app).map_err(thrown)?;
    let receipt = app
        .0
        .state
        .borrow()
        .recovery_applying
        .get(&cx.sender.view)
        .map(|pending| pending.receipt.clone())
        .ok_or_else(|| thrown("No map recovery is awaiting initialization"))?;
    let mut document = None;
    effects
        .with_recovery_open_effect(&receipt, &owner, &cx.cancel, &mut || {
            let state = app.0.state.borrow();
            let pending = state
                .recovery_applying
                .get(&cx.sender.view)
                .ok_or("No map recovery is awaiting initialization")?;
            if pending.token != token
                || !state
                    .documents
                    .get(&cx.sender.view)
                    .is_some_and(|document| document.same_generation(&pending.document))
                || state
                    .revisions
                    .get(&cx.sender.view)
                    .copied()
                    .unwrap_or_default()
                    != pending.revision
            {
                return Err("The recovery acknowledgement belongs to another map".into());
            }
            document = Some(pending.document.clone());
            Ok(())
        })
        .map_err(thrown)?;
    let document = document.ok_or_else(|| thrown("The map document was released"))?;
    if owner
        .context
        .document_lease()
        .is_some_and(|captured| !captured.same_generation(&document))
    {
        return Err(thrown(
            "The recovery acknowledgement belongs to another document",
        ));
    }
    owner.context = owner.context.with_document_lease(document);
    effects
        .settle_recovery_seed(
            &receipt,
            &owner,
            &cx.cancel,
            if ok { Ok(()) } else { Err(error) },
        )
        .map_err(thrown)?;
    let mut state = app.0.state.borrow_mut();
    if state
        .recovery_applying
        .get(&cx.sender.view)
        .is_some_and(|pending| pending.token == token)
    {
        state.recovery_applying.remove(&cx.sender.view);
    }
    Ok(IpcValue::Null)
}

fn retain_applying(
    app: &MindmapApp,
    view: ViewId,
    receipt: RecoverySeedReceipt,
    result: &mut IpcValue,
) -> Result<(), IpcError> {
    let token = jscompat::random_uuid();
    let (document, revision) = {
        let state = app.0.state.borrow();
        (
            state
                .documents
                .get(&view)
                .cloned()
                .ok_or_else(|| thrown("The map document was released"))?,
            state.revisions.get(&view).copied().unwrap_or_default(),
        )
    };
    let IpcValue::Object(fields) = result else {
        return Err(thrown("The map recovery result is invalid"));
    };
    fields.push(("recoveryToken".into(), string(token.clone())));
    let loss_receipt = receipt.clone();
    let loss = document.loss_token().on_cancel(move || {
        if let Ok(notification) =
            loss_receipt.fail_after_loss("The map document was replaced".into())
        {
            notification.notify();
        }
    });
    app.0.state.borrow_mut().recovery_applying.insert(
        view,
        RecoveryApplying {
            token,
            receipt,
            document,
            revision,
            _loss: loss,
        },
    );
    Ok(())
}

fn effects(app: &MindmapApp) -> Result<OwnedEffectsHost, String> {
    app.0
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or_else(|| "The native recovery effect host is unavailable".into())
}

impl MindmapApp {
    fn begin_recovery(
        &self,
        view: ViewId,
        target: &RecoveryOpenTarget,
        authority: shell_contract::ipc::RecoveryAuthoritySnapshot,
        done: RecoveryOpenDone,
    ) -> Option<(u64, Admission)> {
        if view != target.view() {
            done(Err(Failure(
                "The recovery target does not match the map".into(),
            )));
            return None;
        }
        let effects = match effects(self) {
            Ok(effects) => effects,
            Err(error) => {
                done(Err(Failure(error)));
                return None;
            }
        };
        let losses = crate::owned_recovery::recovery_losses(target, &authority);
        let mut done = Some(done);
        let mut previous = None;
        let mut ticket = None;
        let admitted = effects.with_recovery_seed_snapshot(target, &authority, &mut || {
            // the host committed the browser's owner when it made the view: the recovery
            // is staged under that owner, not by a second first adoption
            target
                .closure_reservation()
                .with_owner(target.browser_owner(), |_| {
                    let mut state = self.0.state.borrow_mut();
                    state.next_gen = state
                        .next_gen
                        .checked_add(1)
                        .ok_or("The recovery request sequence is exhausted")?;
                    let id = state.next_gen;
                    previous = state.recovery_preparing.insert(
                        view,
                        PendingRecovery {
                            ticket: id,
                            target: target.clone(),
                            authority: authority.clone(),
                            done: done.take().ok_or("The recovery completion was consumed")?,
                            subscriptions: Vec::new(),
                        },
                    );
                    state.recovery_seeds.remove(&view);
                    ticket = Some(id);
                    Ok(())
                })
        });
        if let Some(previous) = previous {
            (previous.done)(Err(Failure("The recovery request was replaced".into())));
        }
        if let Err(error) = admitted {
            if let Some(done) = done {
                done(Err(Failure(error)));
            }
            return None;
        }
        let ticket = ticket?;
        let mut subscriptions = Vec::new();
        for loss in losses {
            let core = self.0.services.core.clone();
            subscriptions.push(loss.on_cancel(move || {
                core.post(Box::new(move || {
                    super::with_app(|app| {
                        app.finish_recovery(
                            view,
                            ticket,
                            Err("The recovery operation was released".into()),
                        );
                    })
                }))
            }));
        }
        {
            let mut state = self.0.state.borrow_mut();
            let pending = state
                .recovery_preparing
                .get_mut(&view)
                .filter(|pending| pending.ticket == ticket)?;
            pending.subscriptions = subscriptions;
        }
        Some((
            ticket,
            crate::owned_recovery::seed_admission(effects, target.clone(), authority),
        ))
    }

    fn finish_recovery(&self, view: ViewId, ticket: u64, result: Result<PreparedRecovery, String>) {
        let captured = self
            .0
            .state
            .borrow()
            .recovery_preparing
            .get(&view)
            .filter(|pending| pending.ticket == ticket)
            .map(|pending| (pending.target.clone(), pending.authority.clone()));
        let Some((target, authority)) = captured else {
            return;
        };
        let mut pending = None;
        let result = result.and_then(|prepared| {
            let mut prepared = Some(prepared);
            let receipt =
                effects(self)?.commit_recovery_seed_snapshot(&target, &authority, &mut || {
                    target
                        .closure_reservation()
                        .with_owner(target.browser_owner(), |_| {
                            let mut state = self.0.state.borrow_mut();
                            if !state.recovery_preparing.get(&view).is_some_and(|pending| {
                                pending.ticket == ticket && pending.target.same_binding(&target)
                            }) {
                                return Err("The recovery request was replaced".into());
                            }
                            pending = state.recovery_preparing.remove(&view);
                            let prepared =
                                prepared.take().ok_or("The recovery seed was consumed")?;
                            if let PreparedRecovery::File { path, .. } = &prepared {
                                state.open_path.insert(view, path.clone());
                                state.allowed.insert(view, HashSet::from([path.clone()]));
                                state.unopened.insert(view, Unopened::Pending);
                            } else {
                                state.open_path.remove(&view);
                                state.save_path.remove(&view);
                            }
                            state.recovery_seeds.insert(
                                view,
                                RecoverySeed {
                                    target: target.clone(),
                                    receipt: None,
                                    prepared,
                                },
                            );
                            Ok(())
                        })
                })?;
            let mut state = self.0.state.borrow_mut();
            let seed = state
                .recovery_seeds
                .get_mut(&view)
                .filter(|seed| seed.target.same_binding(&target))
                .ok_or("The recovery seed was released")?;
            seed.receipt = Some(receipt);
            Ok(())
        });
        if result.is_err() {
            let mut state = self.0.state.borrow_mut();
            if state
                .recovery_preparing
                .get(&view)
                .is_some_and(|pending| pending.ticket == ticket)
            {
                pending = state.recovery_preparing.remove(&view);
            }
            if state
                .recovery_seeds
                .get(&view)
                .is_some_and(|seed| seed.target.same_binding(&target))
            {
                state.recovery_seeds.remove(&view);
            }
        }
        if let Some(pending) = pending {
            drop(pending.subscriptions);
            (pending.done)(result.map_err(Failure));
        }
    }

    pub(super) fn cancel_recovery_open(&self, view: ViewId) {
        let pending = self.0.state.borrow_mut().recovery_preparing.remove(&view);
        let receipt = {
            let mut state = self.0.state.borrow_mut();
            state
                .recovery_applying
                .remove(&view)
                .map(|pending| pending.receipt)
                .or_else(|| {
                    state
                        .recovery_seeds
                        .remove(&view)
                        .and_then(|seed| seed.receipt)
                })
        };
        if let Some(receipt) = receipt
            && let Ok(notification) =
                receipt.fail_after_loss("The map recovery target was released".into())
        {
            notification.notify();
        }
        if let Some(pending) = pending {
            (pending.done)(Err(Failure("The mindmap view was closed".into())));
        }
    }

    pub fn open_file_with_decision(
        &self,
        view: ViewId,
        request: &RecoveryOpenFileRequest,
        target: &RecoveryOpenTarget,
        done: RecoveryOpenDone,
    ) {
        let authority = match target.authority_snapshot() {
            Ok(value) => value,
            Err(error) => return done(Err(Failure(error))),
        };
        if let Err(error) =
            crate::owned_recovery::check_selected_file(&authority, Surface::Mindmap, request)
        {
            return done(Err(Failure(error)));
        }
        let Some((ticket, admission)) = self.begin_recovery(view, target, authority, done) else {
            return;
        };
        let directory = self.user_data_dir();
        let identity = super::recovery::file_identity(&request.file_path);
        let copy_path = match recovery_copy_path(&directory, &identity) {
            Ok(path) => path,
            Err(error) => return self.finish_recovery(view, ticket, Err(error.message)),
        };
        let held_elsewhere = self
            .0
            .state
            .borrow()
            .recovery
            .iter()
            .any(|(other, recovery)| {
                *other != view
                    && !recovery.crashed
                    && recovery.owned.iter().any(|(key, _)| key == &copy_path)
            });
        let shared = self.0.recovery_state.clone();
        let engine = self.0.engine.clone();
        let request = request.clone();
        self.offload(
            move || {
                admitted(&admission, || ()).map_err(|error| error.message)?;
                let path = request.file_path;
                if path.to_lowercase().ends_with(".mm") {
                    let metadata = std::fs::metadata(&path).map_err(|error| {
                        main_core::node_fs::node_error(&error, "stat", &path).message
                    })?;
                    if metadata.len() > 32 * 1024 * 1024 {
                        return Err("The FreeMind file exceeds the import limit".into());
                    }
                    let bytes = std::fs::read(&path).map_err(|error| {
                        main_core::node_fs::node_error(&error, "read", &path).message
                    })?;
                    let name = jscompat::node_path::host::basename(&path, None);
                    let title = name[..name.len() - 3].to_owned();
                    admitted(&admission, || ()).map_err(|error| error.message)?;
                    return Ok(PreparedRecovery::File {
                        path,
                        response: obj([
                            ("status", string("import")),
                            ("importer", string("freemind")),
                            ("bytes", buffer(bytes)),
                            ("title", string(title.clone())),
                        ]),
                        save_path: false,
                        disk: None,
                        owned: None,
                        restored_untitled: false,
                        imported: Some(title),
                    });
                }
                let original = std::fs::read(&path);
                if request.decision == RecoveryOpenDecision::Restore && !held_elsewhere {
                    match std::fs::read(&copy_path) {
                        Ok(restored) => {
                            crate::owned_recovery::mark_loaded(
                                &shared, &directory, &identity, &admission,
                            )
                            .map_err(|error| error.message)?;
                            if engine.open_xmind(&restored)?.status == OpenStatus::Ok {
                                let original_opens = match original.as_ref() {
                                    Ok(bytes) => engine.open_xmind(bytes)?.status == OpenStatus::Ok,
                                    Err(_) => false,
                                };
                                let disk =
                                    original_opens.then(|| record_disk_state(&path)).flatten();
                                admitted(&admission, || ()).map_err(|error| error.message)?;
                                return Ok(PreparedRecovery::File {
                                    path,
                                    response: obj([
                                        ("status", string("ok")),
                                        ("bytes", buffer(restored)),
                                        (
                                            "restored",
                                            string(if original_opens {
                                                "file"
                                            } else {
                                                "untitled"
                                            }),
                                        ),
                                    ]),
                                    save_path: original_opens,
                                    disk,
                                    owned: Some((copy_path, identity)),
                                    restored_untitled: !original_opens,
                                    imported: None,
                                });
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(
                                main_core::node_fs::node_error(&error, "read", &copy_path).message
                            );
                        }
                    }
                }
                let bytes = original.map_err(|error| {
                    main_core::node_fs::node_error(&error, "read", &path).message
                })?;
                let opened = engine.open_xmind(&bytes)?;
                let save_path = opened.status == OpenStatus::Ok;
                let disk = save_path.then(|| record_disk_state(&path)).flatten();
                let response = if save_path {
                    obj([("status", string("ok")), ("bytes", buffer(bytes))])
                } else {
                    obj([("status", string(opened.status.as_str()))])
                };
                admitted(&admission, || ()).map_err(|error| error.message)?;
                Ok(PreparedRecovery::File {
                    path,
                    response,
                    save_path,
                    disk,
                    owned: None,
                    restored_untitled: false,
                    imported: None,
                })
            },
            move |app, result| app.finish_recovery(view, ticket, result),
        );
    }

    pub fn open_untitled_copy(
        &self,
        view: ViewId,
        request: &RecoveryOpenUntitledRequest,
        target: &RecoveryOpenTarget,
        done: RecoveryOpenDone,
    ) {
        let authority = match target.authority_snapshot() {
            Ok(value) => value,
            Err(error) => return done(Err(Failure(error))),
        };
        if let Err(error) =
            crate::owned_recovery::check_selected_copy(&authority, Surface::Mindmap, request)
        {
            return done(Err(Failure(error)));
        }
        let Some((ticket, admission)) = self.begin_recovery(view, target, authority, done) else {
            return;
        };
        let directory = self.user_data_dir();
        let shared = self.0.recovery_state.clone();
        let request = request.clone();
        self.offload(
            move || {
                admitted(&admission, || ()).map_err(|error| error.message)?;
                let copy_path =
                    recovery_copy_path(&directory, &request.copy).map_err(|error| error.message)?;
                let bytes = std::fs::read(&copy_path).map_err(|error| {
                    main_core::node_fs::node_error(&error, "read", &copy_path).message
                })?;
                crate::owned_recovery::mark_loaded(&shared, &directory, &request.copy, &admission)
                    .map_err(|error| error.message)?;
                admitted(&admission, || ()).map_err(|error| error.message)?;
                Ok(PreparedRecovery::Untitled {
                    bytes,
                    request,
                    copy_path,
                })
            },
            move |app, result| app.finish_recovery(view, ticket, result),
        );
    }

    fn recovery_receipt(&self, cx: &CallContext) -> Result<Option<RecoverySeedReceipt>, IpcError> {
        let state = self.0.state.borrow();
        if state.recovery_preparing.contains_key(&cx.sender.view) {
            return Err(thrown("The recovery input has not finished preparing"));
        }
        state
            .recovery_seeds
            .get(&cx.sender.view)
            .map(|seed| {
                seed.receipt
                    .clone()
                    .ok_or_else(|| thrown("The recovery queue has not committed"))
            })
            .transpose()
    }

    pub(super) fn recovery_pending_path(
        &self,
        cx: &CallContext,
    ) -> Result<Option<IpcValue>, IpcError> {
        let Some(receipt) = self.recovery_receipt(cx)? else {
            return Ok(None);
        };
        let owner = super::owned_save::native_owner(cx)?;
        let mut result = None;
        effects(self)
            .map_err(thrown)?
            .with_recovery_open_effect(&receipt, &owner, &cx.cancel, &mut || {
                let state = self.0.state.borrow();
                let seed = state
                    .recovery_seeds
                    .get(&cx.sender.view)
                    .filter(|seed| receipt.target().same_binding(&seed.target))
                    .ok_or("The recovery seed was released")?;
                result = Some(match &seed.prepared {
                    PreparedRecovery::File { path, .. } => string(path.clone()),
                    PreparedRecovery::Untitled { .. } => IpcValue::Null,
                });
                Ok(())
            })
            .map_err(thrown)?;
        Ok(result)
    }

    fn take_recovery(
        &self,
        cx: &CallContext,
        path: Option<&str>,
    ) -> Result<Option<(PreparedRecovery, RecoverySeedReceipt)>, IpcError> {
        let Some(receipt) = self.recovery_receipt(cx)? else {
            return Ok(None);
        };
        let owner = super::owned_save::native_owner(cx)?;
        let mut prepared = None;
        effects(self)
            .map_err(thrown)?
            .with_recovery_open_effect(&receipt, &owner, &cx.cancel, &mut || {
                let mut state = self.0.state.borrow_mut();
                let seed = state
                    .recovery_seeds
                    .get(&cx.sender.view)
                    .filter(|seed| receipt.target().same_binding(&seed.target))
                    .ok_or("The recovery seed was released")?;
                match (&seed.prepared, path) {
                    (PreparedRecovery::File { path: selected, .. }, Some(path))
                        if selected == path => {}
                    (PreparedRecovery::Untitled { .. }, None) => {}
                    _ => {
                        return Err("The recovery request does not match the prepared input".into());
                    }
                }
                prepared = state
                    .recovery_seeds
                    .remove(&cx.sender.view)
                    .map(|seed| seed.prepared);
                Ok(())
            })
            .map_err(thrown)?;
        Ok(prepared.map(|prepared| (prepared, receipt)))
    }

    pub(super) fn open_recovery_file(
        &self,
        cx: &CallContext,
        path: &str,
    ) -> Result<Option<IpcValue>, IpcError> {
        let Some((prepared, receipt)) = self.take_recovery(cx, Some(path))? else {
            return Ok(None);
        };
        let mut error_owner = super::owned_save::native_owner(cx)?;
        let opened = (|| -> Result<IpcValue, IpcError> {
            let PreparedRecovery::File {
                path,
                response,
                save_path,
                disk,
                owned,
                restored_untitled,
                imported,
            } = prepared
            else {
                return Err(thrown("The prepared recovery input is not a file"));
            };
            let owner = self.replace_save_document(cx)?;
            error_owner = owner.clone();
            let effects = effects(self).map_err(thrown)?;
            let view = cx.sender.view;
            let mut response = Some(response);
            let mut result = None;
            effects
                .with_owned_effect(&owner, &owner, &cx.cancel, &cx.cancel, &mut || {
                    let mut state = self.0.state.borrow_mut();
                    if save_path || restored_untitled || imported.is_some() {
                        state.unopened.remove(&view);
                    } else {
                        state.unopened.insert(view, Unopened::Failed);
                    }
                    if save_path {
                        state.save_path.insert(view, path.clone());
                    } else {
                        state.save_path.remove(&view);
                    }
                    if let Some(disk) = disk.clone() {
                        state
                            .disk_states
                            .entry(view)
                            .or_default()
                            .insert(path.clone(), disk);
                    }
                    if let Some(title) = imported.clone() {
                        state.imported.insert(view, (path.clone(), title));
                    }
                    if let Some((copy_path, identity)) = owned.clone() {
                        let recovery = state.recovery.entry(view).or_default();
                        recovery.owned.push((copy_path, identity.clone()));
                        if restored_untitled {
                            recovery.untitled_copy = Some(identity);
                            recovery.save_as_default = Some(path.clone());
                        }
                    }
                    result = response.take();
                    Ok(())
                })
                .map_err(thrown)?;
            let mut result =
                result.ok_or_else(|| thrown("The recovery file response was not admitted"))?;
            retain_applying(self, view, receipt.clone(), &mut result)?;
            Ok(result)
        })();
        match opened {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                if effects(self)
                    .map_err(thrown)?
                    .settle_recovery_seed(
                        &receipt,
                        &error_owner,
                        &cx.cancel,
                        Err(error.message.clone()),
                    )
                    .is_err()
                    && let Ok(notification) = receipt.fail_after_loss(error.message.clone())
                {
                    notification.notify();
                }
                Err(error)
            }
        }
    }

    pub(super) fn take_recovery_seed(
        &self,
        cx: &CallContext,
    ) -> Result<Option<IpcValue>, IpcError> {
        let Some((prepared, receipt)) = self.take_recovery(cx, None)? else {
            return Ok(None);
        };
        let owner = super::owned_save::native_owner(cx)?;
        let opened = (|| -> Result<IpcValue, IpcError> {
            let PreparedRecovery::Untitled {
                bytes,
                request,
                copy_path,
            } = prepared
            else {
                return Err(thrown(
                    "The prepared recovery input is not an untitled copy",
                ));
            };
            let effects = effects(self).map_err(thrown)?;
            let mut seed = Some(Seed::Copy(bytes));
            let mut result = None;
            effects
                .with_owned_effect(&owner, &owner, &cx.cancel, &cx.cancel, &mut || {
                    let mut state = self.0.state.borrow_mut();
                    let recovery = state.recovery.entry(cx.sender.view).or_default();
                    recovery.untitled_copy = Some(request.copy.clone());
                    recovery
                        .owned
                        .push((copy_path.clone(), request.copy.clone()));
                    recovery.title = Some(request.title.clone());
                    recovery.save_as_default = request
                        .save_as_default_path
                        .clone()
                        .filter(|path| !path.is_empty());
                    result = seed.take();
                    Ok(())
                })
                .map_err(thrown)?;
            let seed = result.ok_or_else(|| thrown("The recovery seed was not admitted"))?;
            let mut result = seed.to_value();
            retain_applying(self, cx.sender.view, receipt.clone(), &mut result)?;
            Ok(result)
        })();
        match opened {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                if effects(self)
                    .map_err(thrown)?
                    .settle_recovery_seed(&receipt, &owner, &cx.cancel, Err(error.message.clone()))
                    .is_err()
                    && let Ok(notification) = receipt.fail_after_loss(error.message.clone())
                {
                    notification.notify();
                }
                Err(error)
            }
        }
    }
}
