use std::collections::HashSet;

use main_core::packages::electron_utils::atomic_write::owned::Admission;
use main_core::packages::electron_utils::recovery_contract::{
    RecoveryOpenDone, RecoveryOpenFileRequest, RecoveryOpenUntitledRequest,
};
use main_core::packages::electron_utils::recovery_store::{RecoveryIdentity, recovery_copy_path};
use main_core::packages::electron_utils::save_conflict::SaveDiskState;
use main_core::shell::renderer_disposal::Failure;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{
    CallContext, NativeRequestOwner, RecoveryOpenTarget, RecoverySeedReceipt,
};
use shell_contract::runtime::{CancelSubscription, RendererGeneration};
use shell_contract::{IpcError, IpcValue, Surface, ViewId};

use super::formats::is_compressed_file;
use super::state::{Import, Unopened};
use super::{DiagramFile, DiagramViews, lookup};
use crate::value::{Args, obj, string, thrown};

pub(super) struct Preparing {
    target: RecoveryOpenTarget,
    authority: shell_contract::ipc::RecoveryAuthoritySnapshot,
    done: RecoveryOpenDone,
    ticket: String,
    _losses: Vec<CancelSubscription>,
}

pub(super) struct Seed {
    target: RecoveryOpenTarget,
    receipt: Option<RecoverySeedReceipt>,
    prepared: Prepared,
    losses: Vec<CancelSubscription>,
}

pub(super) struct Applying {
    receipt: RecoverySeedReceipt,
    token: String,
    document: RendererGeneration,
    can_succeed: bool,
    _losses: Vec<CancelSubscription>,
}

pub(super) enum Prepared {
    File {
        path: String,
        response: IpcValue,
        save_path: bool,
        clean: bool,
        disk: Option<SaveDiskState>,
        owned: Option<(String, RecoveryIdentity)>,
        restored_untitled: bool,
        imported: Option<String>,
        blocked: bool,
    },
    Untitled {
        xml: String,
        request: RecoveryOpenUntitledRequest,
        copy_path: String,
    },
}

fn effects(views: &DiagramViews) -> Result<OwnedEffectsHost, String> {
    views
        .0
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or_else(|| "Native recovery effects are unavailable".into())
}

fn owner(cx: &CallContext) -> Result<NativeRequestOwner, IpcError> {
    Ok(NativeRequestOwner {
        view: cx.sender.view,
        context: cx
            .native
            .clone()
            .ok_or_else(|| thrown("The recovery renderer has no native owner"))?,
    })
}

pub(super) fn file_response(file: DiagramFile, restored: Option<&str>) -> IpcValue {
    let compressed = is_compressed_file(&file.root, file.any_page_compressed, &file.xml);
    let mut fields = vec![
        ("status", string("ok")),
        ("xml", string(file.xml)),
        ("format", string(file.format.as_str())),
        ("compressed", IpcValue::Bool(compressed)),
    ];
    if let Some(restored) = restored {
        fields.push(("restored", string(restored)));
    }
    IpcValue::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

impl DiagramViews {
    fn begin_recovery(
        &self,
        view: ViewId,
        target: &RecoveryOpenTarget,
        authority: shell_contract::ipc::RecoveryAuthoritySnapshot,
        done: RecoveryOpenDone,
    ) -> Option<(String, Admission)> {
        if target.view() != view {
            done(Err(Failure(
                "The recovery target belongs to another view".into(),
            )));
            return None;
        }
        let effects = match effects(self) {
            Ok(value) => value,
            Err(error) => {
                done(Err(Failure(error)));
                return None;
            }
        };
        let losses = crate::owned_recovery::recovery_losses(target, &authority);
        let ticket = jscompat::random_uuid();
        let mut done = Some(done);
        let result = effects.with_recovery_seed_snapshot(target, &authority, &mut || {
            // the host committed the browser's owner when it made the view: the recovery
            // is staged under that owner, not by a second first adoption
            target
                .closure_reservation()
                .with_owner(target.browser_owner(), |_| {
                    self.with_state(|state| {
                        let entry = state.view(view);
                        if entry.recovery_preparing.is_some()
                            || entry.recovery_seed.is_some()
                            || entry.recovery_applying.is_some()
                        {
                            return Err("The diagram view already has a recovery operation".into());
                        }
                        entry.recovery_preparing = Some(Preparing {
                            target: target.clone(),
                            authority: authority.clone(),
                            ticket: ticket.clone(),
                            done: done.take().ok_or("The recovery completion was consumed")?,
                            _losses: Vec::new(),
                        });
                        Ok(())
                    })
                })
        });
        if let Err(error) = result {
            if let Some(done) = done {
                done(Err(Failure(error)));
            }
            return None;
        }
        let mut subscriptions = Vec::new();
        for loss in losses {
            let core = self.0.services.core.clone();
            let instance = self.0.id;
            let ticket = ticket.clone();
            subscriptions.push(loss.on_cancel(move || {
                core.post(Box::new(move || {
                    if let Some(views) = lookup(instance) {
                        views.finish_recovery(
                            view,
                            &ticket,
                            Err("The recovery operation was released".into()),
                        );
                    }
                }))
            }));
        }
        let retained = self.with_state(|state| {
            state
                .views
                .get_mut(&view)
                .and_then(|entry| entry.recovery_preparing.as_mut())
                .filter(|pending| pending.ticket == ticket)
                .map(|pending| pending._losses = subscriptions)
                .is_some()
        });
        if !retained {
            return None;
        }
        Some((
            ticket,
            crate::owned_recovery::seed_admission(effects, target.clone(), authority),
        ))
    }

    fn finish_recovery(&self, view: ViewId, ticket: &str, result: Result<Prepared, String>) {
        let captured = self.with_state(|state| {
            state
                .views
                .get(&view)
                .and_then(|entry| entry.recovery_preparing.as_ref())
                .filter(|pending| pending.ticket == ticket)
                .map(|pending| (pending.target.clone(), pending.authority.clone()))
        });
        let Some((target, authority)) = captured else {
            return;
        };
        let mut pending = None;
        let result = result.and_then(|prepared| {
            let losses = crate::owned_recovery::recovery_losses(&target, &authority);
            let mut prepared = Some(prepared);
            let receipt =
                effects(self)?.commit_recovery_seed_snapshot(&target, &authority, &mut || {
                    target
                        .closure_reservation()
                        .with_owner(target.browser_owner(), |_| {
                            self.with_state(|state| {
                                let entry = state
                                    .views
                                    .get_mut(&view)
                                    .ok_or("The diagram view was released")?;
                                if !entry.recovery_preparing.as_ref().is_some_and(|pending| {
                                    pending.ticket == ticket && pending.target.same_binding(&target)
                                }) {
                                    return Err(
                                        "The diagram recovery operation was replaced".into()
                                    );
                                }
                                pending = entry.recovery_preparing.take();
                                let prepared = prepared
                                    .take()
                                    .ok_or("The diagram recovery input was consumed")?;
                                entry.tracked = true;
                                match &prepared {
                                    Prepared::File { path, .. } => {
                                        entry.open_path = Some(path.clone());
                                        entry.allowed = Some(HashSet::from([path.clone()]));
                                        entry.unopened = Some(Unopened::Pending);
                                    }
                                    Prepared::Untitled { .. } => {
                                        entry.open_path = None;
                                        entry.allowed = None;
                                        entry.save_path = None;
                                    }
                                }
                                entry.recovery_seed = Some(Seed {
                                    target: target.clone(),
                                    receipt: None,
                                    prepared,
                                    losses: Vec::new(),
                                });
                                Ok(())
                            })
                        })
                })?;
            let losses = losses
                .into_iter()
                .map(|loss| {
                    let receipt = receipt.clone();
                    loss.on_cancel(move || {
                        if let Ok(notification) = receipt
                            .fail_after_loss("The diagram recovery target was released".into())
                        {
                            notification.notify();
                        }
                    })
                })
                .collect();
            self.with_state(|state| {
                let seed = state
                    .views
                    .get_mut(&view)
                    .and_then(|entry| entry.recovery_seed.as_mut())
                    .filter(|seed| seed.target.same_binding(&target))
                    .ok_or("The diagram recovery seed was released")?;
                seed.receipt = Some(receipt);
                seed.losses = losses;
                Ok(())
            })
        });
        if result.is_err() {
            let failed = self.with_state(|state| {
                let entry = state.views.get_mut(&view)?;
                if entry
                    .recovery_preparing
                    .as_ref()
                    .is_some_and(|pending| pending.ticket == ticket)
                {
                    entry.recovery_preparing.take()
                } else {
                    None
                }
            });
            if pending.is_none() {
                pending = failed;
            }
        }
        if let Some(pending) = pending {
            (pending.done)(result.map_err(Failure));
        }
    }

    pub(super) fn prepare_recovery_file(
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
            crate::owned_recovery::check_selected_file(&authority, Surface::Diagram, request)
        {
            return done(Err(Failure(error)));
        }
        let Some((ticket, admission)) = self.begin_recovery(view, target, authority, done) else {
            return;
        };
        let directory = self.user_data_dir();
        let identity = super::recovery::file_identity(&request.file_path);
        let copy = match recovery_copy_path(&directory, &identity) {
            Ok(copy) => copy,
            Err(error) => return self.finish_recovery(view, &ticket, Err(error.message)),
        };
        let held = self.with_state(|state| {
            state.recovery.iter().any(|(other, recovery)| {
                *other != view && !recovery.crashed && recovery.owned.contains_key(&copy)
            })
        });
        let request = request.clone();
        let engine = self.0.engine.clone();
        let shared = self.0.recovery_state.clone();
        let headless = self.0.shell.is_headless();
        let core = self.0.services.core.clone();
        let instance = self.0.id;
        self.0.services.workers.spawn(Box::new(move || {
            let result = super::recovery_prepare::file(
                engine, shared, directory, request, headless, held, admission,
            );
            core.post(Box::new(move || {
                if let Some(views) = lookup(instance) {
                    views.finish_recovery(view, &ticket, result);
                }
            }));
        }));
    }

    pub(super) fn prepare_recovery_copy(
        &self,
        view: ViewId,
        request: &RecoveryOpenUntitledRequest,
        target: &RecoveryOpenTarget,
        done: RecoveryOpenDone,
    ) {
        if request.copy.kind() != "diagram" {
            return done(Err(Failure("The recovery copy is not a diagram".into())));
        }
        let authority = match target.authority_snapshot() {
            Ok(value) => value,
            Err(error) => return done(Err(Failure(error))),
        };
        if let Err(error) =
            crate::owned_recovery::check_selected_copy(&authority, Surface::Diagram, request)
        {
            return done(Err(Failure(error)));
        }
        let Some((ticket, admission)) = self.begin_recovery(view, target, authority, done) else {
            return;
        };
        let directory = self.user_data_dir();
        let request = request.clone();
        let engine = self.0.engine.clone();
        let shared = self.0.recovery_state.clone();
        let core = self.0.services.core.clone();
        let instance = self.0.id;
        self.0.services.workers.spawn(Box::new(move || {
            let result =
                super::recovery_prepare::untitled(engine, shared, directory, request, admission);
            core.post(Box::new(move || {
                if let Some(views) = lookup(instance) {
                    views.finish_recovery(view, &ticket, result);
                }
            }));
        }));
    }

    fn recovery_receipt(&self, view: ViewId) -> Result<Option<RecoverySeedReceipt>, IpcError> {
        self.with_state(|state| {
            let Some(entry) = state.views.get(&view) else {
                return Ok(None);
            };
            if entry.recovery_preparing.is_some() {
                return Err(thrown("The diagram recovery input is still preparing"));
            }
            if entry.recovery_applying.is_some() {
                return Err(thrown(
                    "The diagram recovery input is already being applied",
                ));
            }
            entry
                .recovery_seed
                .as_ref()
                .map(|seed| {
                    seed.receipt
                        .clone()
                        .ok_or_else(|| thrown("The diagram recovery seed is not committed"))
                })
                .transpose()
        })
    }

    pub(super) fn recovery_pending_path(
        &self,
        cx: &CallContext,
    ) -> Result<Option<IpcValue>, IpcError> {
        let Some(receipt) = self.recovery_receipt(cx.sender.view)? else {
            return Ok(None);
        };
        let receiver = owner(cx)?;
        let mut answer = None;
        effects(self)
            .map_err(thrown)?
            .with_recovery_open_effect(&receipt, &receiver, &cx.cancel, &mut || {
                self.with_state(|state| {
                    let seed = state
                        .views
                        .get(&cx.sender.view)
                        .and_then(|entry| entry.recovery_seed.as_ref())
                        .filter(|seed| seed.target.same_binding(receipt.target()))
                        .ok_or("The diagram recovery seed was released")?;
                    answer = Some(match &seed.prepared {
                        Prepared::File { path, .. } => string(path.clone()),
                        Prepared::Untitled { .. } => IpcValue::Null,
                    });
                    Ok(())
                })
            })
            .map_err(thrown)?;
        Ok(answer)
    }

    pub(super) fn take_recovery_input(
        &self,
        cx: &CallContext,
        path: Option<&str>,
    ) -> Result<Option<IpcValue>, IpcError> {
        let view = cx.sender.view;
        let Some(receipt) = self.recovery_receipt(view)? else {
            return Ok(None);
        };
        let receiver = owner(cx)?;
        let document = RendererGeneration::new();
        let token = jscompat::random_uuid();
        let loss_receipt = receipt.clone();
        let mut loss = Some(document.loss_token().on_cancel(move || {
            if let Ok(notification) =
                loss_receipt.fail_after_loss("The recovered diagram was replaced".into())
            {
                notification.notify();
            }
        }));
        let mut previous = None;
        let mut cancelled = None;
        let mut answer = None;
        let effect_host = effects(self).map_err(thrown)?;
        let mut admitted = false;
        let consumed = effect_host.with_recovery_open_effect(&receipt, &receiver, &cx.cancel, &mut || {
            admitted = true;
            self.with_state(|state| {
            let seed = state.views.get(&view).and_then(|entry| entry.recovery_seed.as_ref())
                .filter(|seed| seed.target.same_binding(receipt.target())).ok_or("The diagram recovery seed was released")?;
            match (&seed.prepared, path) {
                (Prepared::File { path: selected, .. }, Some(path)) if selected == path => {}
                (Prepared::Untitled { .. }, None) => {}
                _ => return Err("The diagram recovery input does not match".into()),
            }
            let copy_path = match &seed.prepared {
                Prepared::File { owned: Some((copy, _)), .. } => Some(copy),
                Prepared::Untitled { copy_path, .. } => Some(copy_path),
                _ => None,
            };
            if copy_path.is_some_and(|copy| state.recovery.iter().any(|(other, recovery)|
                *other != view && !recovery.crashed && recovery.owned.contains_key(copy))) {
                return Err("The diagram recovery copy is already owned by another view".into());
            }
            let entry = state.views.get_mut(&view).ok_or("The diagram view was released")?;
            let seed = entry.recovery_seed.take().ok_or("The diagram recovery seed was released")?;
            let mut losses = seed.losses;
            losses.push(loss.take().ok_or("The diagram document lifetime was consumed")?);
            previous = entry.document.replace(document.clone());
            entry.first_save_running = false;
            cancelled = Some(std::mem::take(&mut entry.first_save_queue));
            let can_succeed = match &seed.prepared {
                Prepared::File { response, .. } => matches!(crate::value::field(response, "status"), IpcValue::String(status) if status == "ok" || status == "import"),
                Prepared::Untitled { .. } => true,
            };
            entry.recovery_applying = Some(Applying { receipt: receipt.clone(), token: token.clone(), document: document.clone(), can_succeed, _losses: losses });
            let prepared = seed.prepared;
            let result = match prepared {
                Prepared::File { path, mut response, save_path, clean, disk, owned, restored_untitled, imported, blocked } => {
                    let success = matches!(crate::value::field(&response, "status"), IpcValue::String(status) if status == "ok" || status == "import");
                    entry.unopened = if success { None } else { Some(Unopened::Failed) };
                    entry.save_path = save_path.then(|| path.clone());
                    entry.clean_path = clean.then(|| path.clone());
                    entry.advance_save_revision();
                    entry.import = imported.map(|title| Import { path: path.clone(), title });
                    if let Some(disk) = disk { entry.disk_states.insert(path.clone(), disk); }
                    let recovery = state.recovery_of(view);
                    recovery.blocked = blocked;
                    if let Some((copy, identity)) = owned {
                        recovery.owned.insert(copy, identity.clone());
                        if restored_untitled { recovery.untitled_copy = Some(identity); recovery.save_as_default = Some(path); }
                    }
                    if let IpcValue::Object(fields) = &mut response { fields.push(("recoveryToken".into(), string(token.clone()))); }
                    response
                }
                Prepared::Untitled { xml, request, copy_path } => {
                    let recovery = state.recovery_of(view);
                    recovery.untitled_copy = Some(request.copy.clone());
                    recovery.owned.insert(copy_path, request.copy);
                    recovery.title = Some(request.title);
                    recovery.save_as_default = request.save_as_default_path.filter(|path| !path.is_empty());
                    obj([("xml", string(xml)), ("recoveryToken", string(token.clone()))])
                }
            };
            answer = Some(result);
            Ok(())
            })
        });
        if let Err(error) = consumed {
            if admitted
                && effect_host
                    .settle_recovery_seed(&receipt, &receiver, &cx.cancel, Err(error.clone()))
                    .is_err()
                && let Ok(notification) = receipt.fail_after_loss(error.clone())
            {
                notification.notify();
            }
            return Err(thrown(error));
        }
        if let Some(previous) = previous {
            previous.revoke();
        }
        if let Some(cancelled) = cancelled {
            for (_, _, reply) in cancelled {
                reply.reply(Err(thrown("The diagram document was replaced")));
            }
        }
        Ok(answer)
    }

    pub(super) fn recovery_initialized(
        &self,
        cx: &CallContext,
        args: Args,
    ) -> Result<IpcValue, IpcError> {
        let token = args
            .str(0)
            .ok_or_else(|| thrown("The diagram recovery token is required"))?;
        let ok = matches!(args.get(1), IpcValue::Bool(true));
        let message = args
            .str(2)
            .unwrap_or("The diagram could not be initialized")
            .to_owned();
        if super::saved_load::acknowledge(self, cx, token, ok, &message)? {
            return Ok(IpcValue::Null);
        }
        let mut receiver = owner(cx)?;
        let (receipt, document) = self
            .with_state(|state| {
                let entry = state.views.get(&cx.sender.view)?;
                let pending = entry
                    .recovery_applying
                    .as_ref()
                    .filter(|pending| pending.token == token)?;
                if (ok && !pending.can_succeed)
                    || !entry
                        .document
                        .as_ref()
                        .is_some_and(|current| current.same_generation(&pending.document))
                {
                    return None;
                }
                Some((pending.receipt.clone(), pending.document.clone()))
            })
            .ok_or_else(|| thrown("The diagram recovery acknowledgement does not match"))?;
        if receiver
            .context
            .document_lease()
            .is_some_and(|lease| !lease.same_generation(&document))
        {
            return Err(thrown("The diagram document was replaced"));
        }
        receiver.context = receiver.context.with_document_lease(document);
        effects(self)
            .map_err(thrown)?
            .settle_recovery_seed(
                &receipt,
                &receiver,
                &cx.cancel,
                if ok { Ok(()) } else { Err(message) },
            )
            .map_err(thrown)?;
        self.with_state(|state| {
            if let Some(entry) = state.views.get_mut(&cx.sender.view)
                && entry
                    .recovery_applying
                    .as_ref()
                    .is_some_and(|pending| pending.token == token)
            {
                entry.recovery_applying = None;
            }
        });
        Ok(IpcValue::Null)
    }

    /// Finish the recovery work detached from an exact retired actor.
    pub(super) fn cancel_detached_recovery(&self, entry: &mut super::state::ViewState) {
        let pending = entry.recovery_preparing.take();
        let receipt = entry
            .recovery_applying
            .take()
            .map(|applying| applying.receipt)
            .or_else(|| entry.recovery_seed.take().and_then(|seed| seed.receipt));
        if let Some(receipt) = receipt
            && let Ok(notification) =
                receipt.fail_after_loss("The diagram recovery target was released".into())
        {
            notification.notify();
        }
        if let Some(pending) = pending {
            (pending.done)(Err(Failure("The diagram view was closed".into())));
        }
    }

    pub(super) fn cancel_recovery_open(&self, view: ViewId) {
        let (pending, receipt) = self.with_state(|state| {
            let Some(entry) = state.views.get_mut(&view) else {
                return (None, None);
            };
            let pending = entry.recovery_preparing.take();
            let receipt = entry
                .recovery_applying
                .take()
                .map(|applying| applying.receipt)
                .or_else(|| entry.recovery_seed.take().and_then(|seed| seed.receipt));
            (pending, receipt)
        });
        if let Some(receipt) = receipt
            && let Ok(notification) =
                receipt.fail_after_loss("The diagram recovery target was released".into())
        {
            notification.notify();
        }
        if let Some(pending) = pending {
            (pending.done)(Err(Failure("The diagram view was closed".into())));
        }
    }
}
