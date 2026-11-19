use super::super::engine::{PreparedOpen, RenderOwner};
use super::super::state::{App, DeckHandle, PendingRecoverySeed, SlideSession, StagedBinding};
use crate::value::{obj, thrown};
use main_core::packages::electron_utils::atomic_write::owned::{Admission, admitted, write_json};
use main_core::packages::electron_utils::recovery_contract::{
    RecoveryOpenDecision, RecoveryOpenDone, RecoveryOpenFileRequest, RecoveryOpenUntitledRequest,
};
use main_core::packages::electron_utils::recovery_store::{
    RecoveryAdoptOptions, RecoveryAdoptPlan, RecoveryCopyState, RecoveryIdentity,
    RecoveryPinnedRead, RecoveryPinnedSnapshot, RecoveryStore, recovery_copy_path,
};
use main_core::packages::electron_utils::save_conflict::{SaveDiskState, record_disk_state};
use main_core::shell::renderer_disposal::Failure;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{CallContext, RecoveryOpenTarget};
use shell_contract::{IpcError, IpcValue, Surface, ViewId};

pub(crate) enum PreparedRecovery {
    File {
        path: String,
        bytes: Vec<u8>,
        password: Option<String>,
        disk: Option<SaveDiskState>,
        recovered: bool,
        snapshot: Option<Box<RecoveryPinnedSnapshot>>,
    },
    Untitled {
        bytes: Vec<u8>,
        identity: RecoveryIdentity,
        password: Option<String>,
        suggested: String,
    },
}

pub(crate) struct RecoveryApplying {
    token: String,
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    session: u64,
    document: shell_contract::runtime::RendererGeneration,
    _loss: shell_contract::runtime::CancelSubscription,
}

pub(crate) fn initialized(
    app: &App,
    cx: &CallContext,
    args: crate::value::Args,
) -> Result<IpcValue, IpcError> {
    if let Some(result) = super::super::saved_load::initialized(app, cx, &args) {
        return result;
    }
    let token = args
        .str(0)
        .ok_or_else(|| thrown("The recovery acknowledgement token is required"))?;
    let ok = matches!(args.get(1), IpcValue::Bool(true));
    let error = args
        .str(2)
        .unwrap_or("The presentation could not be initialized")
        .to_owned();
    let mut owner = super::super::session::render_owner(cx)?;
    let effects = effects(app).map_err(thrown)?;
    let receipt = app
        .state
        .borrow()
        .recovery_applying
        .get(&cx.sender.view)
        .map(|pending| pending.receipt.clone())
        .ok_or_else(|| thrown("No presentation recovery is awaiting initialization"))?;
    let authority_snapshot = receipt.target().authority_snapshot().map_err(thrown)?;
    let mut document = None;
    effects
        .with_recovery_open_snapshot(
            &receipt,
            &authority_snapshot,
            &owner,
            &[],
            &cx.cancel,
            &cx.cancel,
            &mut || {
                let state = app.state.borrow();
                let current = state
                    .recovery_applying
                    .get(&cx.sender.view)
                    .ok_or("No presentation recovery is awaiting initialization")?;
                if current.token != token
                    || !state
                        .session(cx.sender.view)
                        .is_some_and(|session| session.id == current.session)
                    || !state
                        .save_lifetime(cx.sender.view)
                        .is_some_and(|document| document.same_generation(&current.document))
                {
                    return Err(
                        "The recovery acknowledgement belongs to another presentation".into(),
                    );
                }
                document = Some(current.document.clone());
                Ok(())
            },
        )
        .map_err(thrown)?;
    let document = document.ok_or_else(|| thrown("The presentation document was released"))?;
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
        .settle_recovery_seed_snapshot(
            &receipt,
            &authority_snapshot,
            &owner,
            &cx.cancel,
            if ok { Ok(()) } else { Err(error) },
        )
        .map_err(thrown)?;
    let mut state = app.state.borrow_mut();
    if state
        .recovery_applying
        .get(&cx.sender.view)
        .is_some_and(|pending| pending.token == token)
    {
        state.recovery_applying.remove(&cx.sender.view);
    }
    Ok(IpcValue::Null)
}

pub(crate) fn abandon(app: &App, view: ViewId) {
    let pending = app.state.borrow_mut().recovery_applying.remove(&view);
    let receipt = pending.map(|pending| pending.receipt).or_else(|| {
        app.state
            .borrow_mut()
            .recovery_seeds
            .remove(&view)
            .and_then(|(_, receipt, _)| receipt)
    });
    if let Some(receipt) = receipt
        && let Ok(notification) =
            receipt.fail_after_loss("The presentation recovery target was released".into())
    {
        notification.notify();
    }
}

fn effects(app: &App) -> Result<OwnedEffectsHost, String> {
    app.services
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or_else(|| "The native recovery effect host is unavailable".into())
}

fn start(
    app: &App,
    view: ViewId,
    target: &RecoveryOpenTarget,
    snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
    done: RecoveryOpenDone,
) -> Option<(u64, Admission)> {
    if view != target.view() {
        done(Err(Failure(
            "The recovery target does not match the presentation".into(),
        )));
        return None;
    }
    let effects = match effects(app) {
        Ok(effects) => effects,
        Err(error) => {
            done(Err(Failure(error)));
            return None;
        }
    };
    let mut done = Some(done);
    let mut previous = None;
    let mut ticket = None;
    let admitted = effects.with_recovery_seed_snapshot(target, &snapshot, &mut || {
        let mut state = app.state.borrow_mut();
        state.next_ticket = state
            .next_ticket
            .checked_add(1)
            .ok_or("The recovery request sequence is exhausted")?;
        let id = state.next_ticket;
        previous = state.pending_recovery_seeds.insert(
            view,
            PendingRecoverySeed {
                ticket: id,
                target: target.clone(),
                snapshot: snapshot.clone(),
                done: done.take().ok_or("The recovery completion was consumed")?,
                subscriptions: Vec::new(),
            },
        );
        state.recovery_seeds.remove(&view);
        ticket = Some(id);
        Ok(())
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
    for loss in crate::owned_recovery::recovery_losses(target, &snapshot) {
        let services = app.services.clone();
        let index = app.registry_index;
        subscriptions.push(loss.on_cancel(move || {
            super::super::post_to(&services, index, move |app| {
                finish(
                    app,
                    view,
                    ticket,
                    Err("The recovery operation was released".into()),
                );
            })
        }));
    }
    {
        let mut state = app.state.borrow_mut();
        let pending = state
            .pending_recovery_seeds
            .get_mut(&view)
            .filter(|pending| pending.ticket == ticket)?;
        pending.subscriptions = subscriptions;
    }
    Some((
        ticket,
        crate::owned_recovery::seed_admission(effects, target.clone(), snapshot),
    ))
}

fn current(app: &App, view: ViewId, ticket: u64) -> Option<RecoveryOpenTarget> {
    app.state
        .borrow()
        .pending_recovery_seeds
        .get(&view)
        .filter(|pending| pending.ticket == ticket)
        .map(|pending| pending.target.clone())
}

fn finish(app: &App, view: ViewId, ticket: u64, result: Result<PreparedRecovery, String>) {
    let (target, snapshot) = {
        let state = app.state.borrow();
        let Some(pending) = state
            .pending_recovery_seeds
            .get(&view)
            .filter(|pending| pending.ticket == ticket)
        else {
            return;
        };
        (pending.target.clone(), pending.snapshot.clone())
    };
    let mut pending = None;
    let result = result.and_then(|prepared| {
        let effects = effects(app)?;
        let mut prepared = Some(prepared);
        let receipt = effects.commit_recovery_seed_snapshot(&target, &snapshot, &mut || {
            let mut state = app.state.borrow_mut();
            if !state
                .pending_recovery_seeds
                .get(&view)
                .is_some_and(|pending| {
                    pending.ticket == ticket && pending.target.same_binding(&target)
                })
            {
                return Err("The recovery request was replaced".into());
            }
            pending = state.pending_recovery_seeds.remove(&view);
            state.recovery_seeds.insert(
                view,
                (
                    target.clone(),
                    None,
                    prepared.take().ok_or("The recovery seed was consumed")?,
                ),
            );
            Ok(())
        })?;
        let mut state = app.state.borrow_mut();
        let entry = state
            .recovery_seeds
            .get_mut(&view)
            .filter(|(queued, _, _)| queued.same_binding(&target))
            .ok_or("The recovery seed was released")?;
        entry.1 = Some(receipt);
        Ok(())
    });
    if result.is_err() {
        let mut state = app.state.borrow_mut();
        if state
            .pending_recovery_seeds
            .get(&view)
            .is_some_and(|pending| pending.ticket == ticket)
        {
            pending = state.pending_recovery_seeds.remove(&view);
        }
        if state
            .recovery_seeds
            .get(&view)
            .is_some_and(|(queued, _, _)| queued.same_binding(&target))
        {
            state.recovery_seeds.remove(&view);
        }
    }
    if let Some(pending) = pending {
        drop(pending.subscriptions);
        (pending.done)(result.map_err(Failure));
    }
}

fn copy_state(
    directory: &str,
    identity: &RecoveryIdentity,
    state: RecoveryCopyState,
    admission: &Admission,
) -> Result<(), String> {
    let copy = recovery_copy_path(directory, identity).map_err(|error| error.message)?;
    if let Some(metadata) = RecoveryStore::prepare_copy_state_from_path(identity, &copy, state) {
        metadata
            .write_via(&mut |path, json| write_json(path, json, admission.clone()).map(|()| true))
            .map_err(|error| error.message)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn repair_failed_copy(
    app: &App,
    effects: OwnedEffectsHost,
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    authority_snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
    owner: RenderOwner,
    cancel: shell_contract::runtime::CancelToken,
    snapshot: RecoveryPinnedSnapshot,
    error: IpcError,
    responder: shell_contract::ipc::Responder,
) {
    let admission_effects = effects.clone();
    let admission_receipt = receipt.clone();
    let admission_authority = authority_snapshot.clone();
    let admission_owner = owner.clone();
    let admission_cancel = cancel.clone();
    let admission: Admission = std::sync::Arc::new(move |effect| {
        let mut effect = Some(effect);
        let mut value = None;
        admission_effects
            .with_recovery_open_snapshot(
                &admission_receipt,
                &admission_authority,
                &admission_owner,
                &[],
                &admission_cancel,
                &admission_cancel,
                &mut || {
                    value = Some(effect
                        .take()
                        .ok_or("The recovery effect was already consumed")?(
                    ));
                    Ok(())
                },
            )
            .map_err(|message| main_core::node_fs::NodeError {
                code: None,
                message,
            })?;
        value.ok_or_else(|| main_core::node_fs::NodeError {
            code: None,
            message: "The recovery effect was not admitted".into(),
        })
    });
    let services = app.services.clone();
    let index = app.registry_index;
    app.services.workers.spawn(Box::new(move || {
        let outcome = match snapshot.prepare_copy_state(RecoveryCopyState::Later) {
            Some(plan) => plan.write_via(&mut |path, metadata| {
                write_json(path, metadata, admission.clone()).map(|()| true)
            }),
            None => Ok(false),
        };
        super::super::post_to(&services, index, move |app| {
            match outcome {
                Ok(true) => {}
                Ok(false) => eprintln!(
                    "[slides] could not mark the recovery copy as later: metadata is unavailable"
                ),
                Err(failure) => eprintln!(
                    "[slides] could not mark the recovery copy as later: {}",
                    failure.message
                ),
            }
            if effects
                .settle_recovery_seed_snapshot(
                    &receipt,
                    &authority_snapshot,
                    &owner,
                    &cancel,
                    Err(error.message.clone()),
                )
                .is_err()
                && let Ok(notification) = receipt.fail_after_loss(error.message.clone())
            {
                notification.notify();
            }
            responder.reply(Err(error));
            app.pump();
        });
    }));
}

fn prepare_file(
    app: &App,
    view: ViewId,
    ticket: u64,
    request: RecoveryOpenFileRequest,
    prepared: Result<PreparedOpen, String>,
    admission: Admission,
) {
    if current(app, view, ticket).is_none() {
        return;
    }
    let prepared = match prepared {
        Ok(PreparedOpen::Rejected) => {
            return finish(app, view, ticket, Err("Recovery was cancelled.".into()));
        }
        Ok(prepared) => prepared,
        Err(error) => return finish(app, view, ticket, Err(error)),
    };
    let engine = app.engine.clone();
    let shared = app.recovery_state.clone();
    let directory = crate::host::path_string(
        app.services
            .paths
            .path(shell_contract::runtime::PathName::UserData),
    );
    let services = app.services.clone();
    let index = app.registry_index;
    app.services.workers.spawn(Box::new(move || {
        let result = (|| {
            admitted(&admission, || ()).map_err(|error| error.message)?;
            let (path, raw, password) = match prepared {
                PreparedOpen::Plain(path) => {
                    let bytes =
                        super::super::lifecycle::read_file(&path).map_err(|error| error.message)?;
                    (path, bytes, None)
                }
                PreparedOpen::Unlocked {
                    path,
                    bytes,
                    password,
                } => (path, bytes, Some(password)),
                PreparedOpen::Rejected => return Err("Recovery was cancelled.".into()),
            };
            let disk = record_disk_state(&path);
            let original = if raw.is_empty() {
                engine.create_blank_pptx().map_err(|error| error.0)?
            } else {
                raw
            };
            let identity = super::file_recovery_id(&path);
            let mut recovered = false;
            let mut retained_snapshot = None;
            let bytes = if request.decision == RecoveryOpenDecision::Restore {
                let pinned_directory = directory.clone();
                let pinned_identity = identity.clone();
                let pinned = admitted(&admission, move || {
                    RecoveryPinnedRead::open(&pinned_directory, &pinned_identity)
                })
                .map_err(|error| error.message)?;
                match pinned {
                    Ok(pinned) => {
                        let mut snapshot = pinned.read().map_err(|error| error.message)?;
                        let copy = std::mem::take(&mut snapshot.bytes);
                        crate::owned_recovery::mark_loaded(
                            &shared, &directory, &identity, &admission,
                        )
                        .map_err(|error| error.message)?;
                        retained_snapshot = Some(Box::new(snapshot));
                        if engine.is_encrypted_ooxml(&copy).map_err(|error| error.0)? {
                            match password.as_deref().and_then(|password| {
                                engine.decrypt_office_file(&copy, password).ok()
                            }) {
                                Some(bytes) => {
                                    recovered = true;
                                    bytes
                                }
                                None => {
                                    copy_state(
                                        &directory,
                                        &identity,
                                        RecoveryCopyState::Later,
                                        &admission,
                                    )?;
                                    original
                                }
                            }
                        } else {
                            recovered = true;
                            copy
                        }
                    }
                    Err(error) if error.code == Some("ENOENT") => original,
                    Err(error) => return Err(error.message),
                }
            } else {
                original
            };
            admitted(&admission, || ()).map_err(|error| error.message)?;
            Ok(PreparedRecovery::File {
                path,
                bytes,
                password,
                disk,
                recovered,
                snapshot: if recovered { retained_snapshot } else { None },
            })
        })();
        super::super::post_to(&services, index, move |app| {
            finish(app, view, ticket, result)
        });
    }));
}

#[allow(clippy::type_complexity)]
fn restore_unlocked(
    app: &App,
    view: ViewId,
    ticket: u64,
    request: RecoveryOpenUntitledRequest,
    admission: Admission,
    unlocked: Result<Option<(Vec<u8>, Option<String>)>, String>,
) {
    if current(app, view, ticket).is_none() {
        return;
    }
    let (bytes, password) = match unlocked {
        Ok(Some(value)) => value,
        Ok(None) => return finish(app, view, ticket, Err("Recovery was cancelled.".into())),
        Err(error) => return finish(app, view, ticket, Err(error)),
    };
    let title = request
        .title
        .strip_suffix(".pptx")
        .or_else(|| request.title.strip_suffix(".PPTX"))
        .unwrap_or(&request.title)
        .to_owned();
    let title = if title.is_empty() {
        app.tm("untitledDeck", &[])
    } else {
        title
    };
    let suggested = request
        .save_as_default_path
        .clone()
        .unwrap_or_else(|| format!("{title}.pptx"));
    let directory = crate::host::path_string(
        app.services
            .paths
            .path(shell_contract::runtime::PathName::UserData),
    );
    let shell = app.shell.clone();
    let services = app.services.clone();
    let index = app.registry_index;
    app.services.workers.spawn(Box::new(move || {
        let result = (|| {
            let identity = match request.copy {
                RecoveryIdentity::Untitled(identity) => RecoveryIdentity::Untitled(identity),
                RecoveryIdentity::File(file) => {
                    let source =
                        recovery_copy_path(&directory, &RecoveryIdentity::File(file.clone()))
                            .map_err(|error| error.message)?;
                    let plan = RecoveryAdoptPlan::prepare(
                        &directory,
                        "slides",
                        &source,
                        &RecoveryAdoptOptions {
                            title,
                            ext: ".pptx".into(),
                            state: Some(RecoveryCopyState::Undecided),
                            original_path: Some(
                                request.save_as_default_path.unwrap_or(file.file_path),
                            ),
                        },
                        &|| shell.random_uuid(),
                        &super::now_ms,
                    )
                    .map_err(|error| error.message)?
                    .ok_or("Recovery copy not found.")?;
                    let identity = plan
                        .publish_via_owned(
                            &mut |effect| admitted(&admission, effect),
                            &mut |path, json| {
                                write_json(path, json, admission.clone()).map(|()| true)
                            },
                        )
                        .map_err(|error| error.message)?
                        .ok_or("The recovery copy could not be adopted")?;
                    RecoveryIdentity::Untitled(identity)
                }
            };
            admitted(&admission, || ()).map_err(|error| error.message)?;
            Ok(PreparedRecovery::Untitled {
                bytes,
                identity,
                password,
                suggested,
            })
        })();
        super::super::post_to(&services, index, move |app| {
            finish(app, view, ticket, result)
        });
    }));
}

impl
    main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract<
        ViewId,
        RecoveryOpenTarget,
    > for super::super::SlidesViews
{
    fn kind(&self) -> &str {
        "slides"
    }
    fn write_copies_now(&self, done: Box<dyn FnOnce(Result<(), Failure>)>) {
        done(self.0.run_recovery_pass(true).map_err(Failure));
    }
    fn open_file_with_decision(
        &self,
        view: &ViewId,
        request: &RecoveryOpenFileRequest,
        target: &RecoveryOpenTarget,
        done: RecoveryOpenDone,
    ) {
        let snapshot = match target.authority_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return done(Err(Failure(error))),
        };
        if let Err(error) =
            crate::owned_recovery::check_selected_file(&snapshot, Surface::Slides, request)
        {
            return done(Err(Failure(error)));
        }
        let Some((ticket, admission)) = start(&self.0, *view, target, snapshot.clone(), done)
        else {
            return;
        };
        let view = *view;
        let request = request.clone();
        let services = self.0.services.clone();
        let index = self.0.registry_index;
        self.0.shell.prepare_for_recovery_open(
            target.clone(),
            snapshot,
            request.file_path.clone(),
            Box::new(move |result| {
                super::super::post_to(&services, index, move |app| {
                    prepare_file(app, view, ticket, request, result, admission)
                });
            }),
        );
    }
    fn open_untitled_copy(
        &self,
        view: &ViewId,
        request: &RecoveryOpenUntitledRequest,
        target: &RecoveryOpenTarget,
        done: RecoveryOpenDone,
    ) {
        let snapshot = match target.authority_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return done(Err(Failure(error))),
        };
        if let Err(error) =
            crate::owned_recovery::check_selected_copy(&snapshot, Surface::Slides, request)
        {
            return done(Err(Failure(error)));
        }
        let Some((ticket, admission)) = start(&self.0, *view, target, snapshot.clone(), done)
        else {
            return;
        };
        let view = *view;
        let request = request.clone();
        let target = target.clone();
        let directory = crate::host::path_string(
            self.0
                .services
                .paths
                .path(shell_contract::runtime::PathName::UserData),
        );
        let engine = self.0.engine.clone();
        let services = self.0.services.clone();
        let index = self.0.registry_index;
        self.0.services.workers.spawn(Box::new(move || {
            let result = (|| {
                admitted(&admission, || ()).map_err(|error| error.message)?;
                let path =
                    recovery_copy_path(&directory, &request.copy).map_err(|error| error.message)?;
                let bytes = std::fs::read(&path).map_err(|error| {
                    main_core::node_fs::node_error(&error, "read", &path).message
                })?;
                let encrypted = engine.is_encrypted_ooxml(&bytes).map_err(|error| error.0)?;
                admitted(&admission, || ()).map_err(|error| error.message)?;
                Ok((bytes, encrypted))
            })();
            super::super::post_to(&services, index, move |app| {
                if current(app, view, ticket).is_none() {
                    return;
                }
                match result {
                    Err(error) => finish(app, view, ticket, Err(error)),
                    Ok((bytes, false)) => restore_unlocked(
                        app,
                        view,
                        ticket,
                        request,
                        admission,
                        Ok(Some((bytes, None))),
                    ),
                    Ok((bytes, true)) => {
                        let services = app.services.clone();
                        let index = app.registry_index;
                        app.shell.unlock_recovery_copy_prepared(
                            target,
                            snapshot,
                            request.title.clone(),
                            bytes,
                            Box::new(move |result| {
                                super::super::post_to(&services, index, move |app| {
                                    restore_unlocked(
                                        app,
                                        view,
                                        ticket,
                                        request,
                                        admission,
                                        result.map(|value| {
                                            value.map(|(bytes, password)| (bytes, Some(password)))
                                        }),
                                    )
                                });
                            }),
                        );
                    }
                }
            });
        }));
    }
    fn is_unprotectable(&self, _view: &ViewId) -> bool {
        false
    }
}

pub(crate) fn consume(
    app: &App,
    cx: &CallContext,
    fit: f64,
    responder: &mut Option<shell_contract::ipc::Responder>,
) -> Result<Option<IpcValue>, IpcError> {
    let view = cx.sender.view;
    let owner = super::super::session::render_owner(cx)?;
    let queued = app
        .state
        .borrow()
        .recovery_seeds
        .get(&view)
        .map(|(target, receipt, _)| (target.clone(), receipt.clone()));
    let Some((target, receipt)) = queued else {
        if app
            .state
            .borrow()
            .pending_recovery_seeds
            .contains_key(&view)
        {
            return Err(thrown("The recovery input has not finished preparing"));
        }
        return Ok(None);
    };
    let receipt = receipt.ok_or_else(|| thrown("The recovery queue has not committed"))?;
    let authority_snapshot = target.authority_snapshot().map_err(thrown)?;
    let effects = effects(app).map_err(thrown)?;
    let mut prepared = None;
    effects
        .with_recovery_open_snapshot(
            &receipt,
            &authority_snapshot,
            &owner,
            &[],
            &cx.cancel,
            &cx.cancel,
            &mut || {
                if !owner
                    .context
                    .renderer()
                    .is_some_and(|renderer| renderer.same_generation(target.created()))
                {
                    return Err("The recovery renderer was replaced".into());
                }
                let mut state = app.state.borrow_mut();
                if !state
                    .recovery_seeds
                    .get(&view)
                    .is_some_and(|(queued, _, _)| queued.same_binding(&target))
                {
                    return Err("The recovery seed was already consumed".into());
                }
                prepared = state
                    .recovery_seeds
                    .remove(&view)
                    .map(|(_, _, prepared)| prepared);
                Ok(())
            },
        )
        .map_err(thrown)?;
    let prepared = prepared.ok_or_else(|| thrown("The recovery seed was already consumed"))?;
    let mut failed_snapshot = None;
    let opened = (|| -> Result<IpcValue, IpcError> {
        let authority =
            super::super::open_authority::OpenAuthority::capture(app, &owner, cx.cancel.clone())?;
        let (path, bytes, password, disk, identity, suggested, dirty, snapshot) = match prepared {
            PreparedRecovery::File {
                path,
                bytes,
                password,
                disk,
                recovered,
                snapshot,
            } => (path, bytes, password, disk, None, None, recovered, snapshot),
            PreparedRecovery::Untitled {
                bytes,
                identity,
                password,
                suggested,
            } => (
                String::new(),
                bytes,
                password,
                None,
                Some(identity),
                Some(suggested),
                true,
                None,
            ),
        };
        let deck = match app.engine.open(&bytes) {
            Ok(deck) => deck,
            Err(error) => {
                failed_snapshot = snapshot;
                return Err(thrown(error.0));
            }
        };
        let mut session = SlideSession::new(&path, DeckHandle::new(deck, app.engine.clone()), fit);
        let session_id = session.id;
        let staged = StagedBinding::new(app.engine.clone(), deck, &owner)
            .map_err(|error| thrown(error.0))?;
        app.engine
            .adopt_embedded_fonts(deck, &owner)
            .map_err(|error| thrown(error.0))?;
        session.password = password;
        session.disk_state = disk;
        session.recovery_id = identity;
        session.ask_path_on_first_save = suggested.is_some();
        session.first_save_name = suggested;
        if dirty {
            session.core.mark_meta_dirty();
        }
        session.core.schedule_history_notify();
        let slides = app.render_all(&session, &owner)?;
        let payload = obj([
            ("slides", slides.clone()),
            (
                "size",
                app.size_value(app.engine.size(deck).map_err(|error| thrown(error.0))?),
            ),
        ]);
        let mut result = app.open_result(&session, slides)?;
        let owner: RenderOwner = authority.commit(app, &owner, None, Some(session), staged)?;
        if !path.is_empty() {
            app.push_recent(&path);
            app.shell.opened(view, &path);
        }
        if dirty {
            app.services
                .events
                .emit_to_owner_checked(&owner, "slides:deck-changed", vec![payload])
                .map_err(thrown)?;
        }
        let document = app
            .state
            .borrow()
            .save_lifetime(view)
            .cloned()
            .ok_or_else(|| thrown("The presentation document was released"))?;
        let token = app.shell.random_uuid();
        let IpcValue::Object(fields) = &mut result else {
            return Err(thrown("The presentation open result is invalid"));
        };
        fields.push(("recoveryToken".into(), IpcValue::String(token.clone())));
        let loss_receipt = receipt.clone();
        let loss = document.loss_token().on_cancel(move || {
            if let Ok(notification) =
                loss_receipt.fail_after_loss("The presentation document was replaced".into())
            {
                notification.notify();
            }
        });
        app.state.borrow_mut().recovery_applying.insert(
            view,
            RecoveryApplying {
                token,
                receipt: receipt.clone(),
                session: session_id,
                document,
                _loss: loss,
            },
        );
        Ok(result)
    })();
    match opened {
        Ok(result) => Ok(Some(result)),
        Err(error) => {
            if let Some(snapshot) = failed_snapshot {
                let response = responder
                    .take()
                    .ok_or_else(|| thrown("The recovery response was released"))?;
                repair_failed_copy(
                    app,
                    effects,
                    receipt,
                    authority_snapshot,
                    owner,
                    cx.cancel.clone(),
                    *snapshot,
                    error,
                    response,
                );
                return Ok(None);
            }
            if effects
                .settle_recovery_seed_snapshot(
                    &receipt,
                    &authority_snapshot,
                    &owner,
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
