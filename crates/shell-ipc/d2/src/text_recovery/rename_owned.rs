use super::*;
use crate::html::text_doc::{NativeRenameScope, OwnedSave, RecoveryRenameScope, SaveAuthority};
use shell_contract::runtime::{CancelSubscription, CancelToken};
use std::sync::Mutex;

const PREFIX: &str = "owned-rename:";

#[derive(Clone)]
enum RenameAuthorization {
    Owned(OwnedSave),
    Recovery(RecoveryRenameScope),
    Native(NativeRenameScope),
}

impl RenameAuthorization {
    fn destination(&self) -> &shell_contract::ipc::NativeRequestOwner {
        match self {
            Self::Owned(request) => request.destination(),
            Self::Recovery(scope) => &scope.destination,
            Self::Native(scope) => &scope.destination,
        }
    }
    fn participants(&self) -> Vec<shell_contract::ipc::NativeRequestOwner> {
        match self {
            Self::Owned(request) => request
                .origin()
                .into_iter()
                .cloned()
                .chain(request.participants().iter().cloned())
                .collect(),
            Self::Recovery(scope) => scope.participants.clone(),
            Self::Native(scope) => scope.participants.clone(),
        }
    }
    fn cancel(&self) -> CancelToken {
        match self {
            Self::Owned(request) => request.cancel(),
            Self::Recovery(scope) => scope.cancel.clone(),
            Self::Native(scope) => scope.cancel.clone(),
        }
    }
    fn pending(&self) -> CancelToken {
        match self {
            Self::Owned(request) => request.pending(),
            Self::Recovery(scope) => scope.pending.clone(),
            Self::Native(scope) => scope.pending.clone(),
        }
    }
    fn loss_owners(&self) -> Vec<shell_contract::ipc::NativeRequestOwner> {
        match self {
            Self::Owned(request) => std::iter::once(request.destination().clone())
                .chain(self.participants())
                .collect(),
            Self::Recovery(scope) => std::iter::once(scope.receiver.clone())
                .chain(scope.participants.iter().cloned())
                .collect(),
            Self::Native(scope) => {
                std::iter::once(scope.snapshot.selection().binding().owner().clone())
                    .chain(scope.snapshot.aliases().iter().cloned())
                    .chain(scope.participants.iter().cloned())
                    .collect()
            }
        }
    }
    fn effect(&self, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        match self {
            Self::Owned(request) => request.admit(effect),
            Self::Recovery(scope) => scope.effects.with_recovery_open_effect_many(
                &scope.receipt,
                &scope.receiver,
                &scope.participants,
                &scope.cancel,
                &scope.pending,
                effect,
            ),
            Self::Native(scope) => scope.admit(effect),
        }
    }
    fn emit(&self, channel: &str, args: Vec<IpcValue>) -> Result<(), String> {
        match self {
            Self::Owned(request) => {
                let extra = request.callback_cancels();
                let extra = extra.iter().collect::<Vec<_>>();
                match request.authority() {
                    SaveAuthority::Renderer(origin) => request.effects().emit_owned_many_extra(
                        origin,
                        request.participants(),
                        0,
                        &request.cancel(),
                        &request.pending(),
                        &extra,
                        channel,
                        args,
                    ),
                    SaveAuthority::Native(binding) => {
                        request.effects().emit_existing_document_save_many_extra(
                            binding,
                            request.participants(),
                            0,
                            &request.cancel(),
                            &request.pending(),
                            &extra,
                            channel,
                            args,
                        )
                    }
                }
            }
            Self::Recovery(scope) => scope.effects.emit_recovery_open_many(
                &scope.receipt,
                &scope.receiver,
                &scope.participants,
                0,
                &scope.cancel,
                &scope.pending,
                channel,
                args,
            ),
            Self::Native(scope) => scope.emit(channel, args),
        }
    }
}

pub(super) struct OwnedRename {
    token: String,
    request: RenameAuthorization,
    old: RecoveryIdentity,
    new: RecoveryIdentity,
    claimed: bool,
    failure: Option<String>,
    done: Option<Box<dyn FnOnce(Result<(), String>)>>,
    subscriptions: Vec<CancelSubscription>,
}

#[derive(Clone)]
struct ReplyScope {
    request: RenameAuthorization,
    actual: shell_contract::ipc::NativeRequestOwner,
    cancel: CancelToken,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
}

impl ReplyScope {
    fn effect(&self, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        match &self.request {
            RenameAuthorization::Owned(request) => {
                let mut tokens = request.callback_cancels();
                tokens.push(self.cancel.clone());
                let extra = tokens.iter().collect::<Vec<_>>();
                match request.authority() {
                    SaveAuthority::Renderer(_) => request.effects().with_same_native_origin_effect(
                        request.destination(),
                        &self.actual,
                        &self.participants,
                        &request.cancel(),
                        &request.pending(),
                        &extra,
                        effect,
                    ),
                    SaveAuthority::Native(binding) => {
                        request.effects().with_existing_document_save_effect_extra(
                            binding,
                            &self.participants,
                            &request.cancel(),
                            &request.pending(),
                            &extra,
                            effect,
                        )
                    }
                }
            }
            RenameAuthorization::Recovery(scope) => {
                scope.effects.with_recovery_open_effect_many_extra(
                    &scope.receipt,
                    &scope.receiver,
                    &self.participants,
                    &scope.cancel,
                    &scope.pending,
                    &[&self.cancel],
                    effect,
                )
            }
            RenameAuthorization::Native(scope) => {
                let mut tokens = scope.additional.clone();
                tokens.push(self.cancel.clone());
                let extra = tokens.iter().collect::<Vec<_>>();
                scope.effects.with_native_save_revision_snapshot_effect(
                    &scope.snapshot,
                    &self.participants,
                    &scope.cancel,
                    &scope.pending,
                    &extra,
                    &mut || {
                        if scope.snapshot.selection().path() != scope.expected_path.as_path() {
                            return Err("the native save selection path changed".into());
                        }
                        effect()
                    },
                )
            }
        }
    }
}

fn try_reply_store<R>(
    recovery: &TextRecovery,
    scope: &ReplyScope,
    action: impl FnOnce(&RecoveryStore<'_>) -> R,
) -> Result<Option<R>, String> {
    let data = recovery
        .services
        .paths
        .path(PathName::UserData)
        .to_string_lossy()
        .into_owned();
    let now = (recovery.host.now_ms)();
    let mut action = Some(action);
    let mut output = None;
    scope.effect(&mut || {
        let shared = match recovery.shared.try_lock() {
            Ok(shared) => shared,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
        };
        let directory = || data.clone();
        let clock = || now;
        let uuid = jscompat::random_uuid;
        let store = create_recovery_store(
            RecoveryStoreOptions {
                user_data_dir: &directory,
                now_ms: &clock,
                random_uuid: &uuid,
            },
            &shared,
        );
        output = Some(action
            .take()
            .ok_or("the recovery store action was admitted twice")?(
            &store
        ));
        Ok(())
    })?;
    Ok(output)
}

fn ticket_admission(
    scope: ReplyScope,
    shared: SharedRecoveryHost,
    user_data: String,
    now: f64,
    ticket: main_core::packages::electron_utils::recovery_store::RecoveryWriteTicket,
) -> main_core::packages::electron_utils::atomic_write::owned::Admission {
    Arc::new(move |effect| {
        let mut effect = Some(effect);
        loop {
            let mut output = None;
            scope
                .effect(&mut || {
                    let state = match shared.try_lock() {
                        Ok(state) => state,
                        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                        Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
                    };
                    let directory = || user_data.clone();
                    let clock = || now;
                    let uuid = jscompat::random_uuid;
                    let store = create_recovery_store(
                        RecoveryStoreOptions {
                            user_data_dir: &directory,
                            now_ms: &clock,
                            random_uuid: &uuid,
                        },
                        &state,
                    );
                    if !store.write_ticket_is_current(&ticket) {
                        return Err("the recovery copy was superseded".into());
                    }
                    output = Some(effect
                        .take()
                        .ok_or("the recovery file effect was admitted twice")?(
                    ));
                    Ok(())
                })
                .map_err(|message| main_core::node_fs::NodeError {
                    code: None,
                    message,
                })?;
            if let Some(output) = output {
                return Ok(output);
            }
            drop(shared.lock().unwrap_or_else(|error| error.into_inner()));
        }
    })
}

impl TextRecovery {
    pub(crate) fn renamed_owned_async(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &str,
        new_path: &str,
        request: OwnedSave,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.renamed_with_authorization(
            view,
            old_path,
            new_path,
            RenameAuthorization::Owned(request),
            done,
        );
    }

    pub(crate) fn renamed_recovery_async(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &str,
        new_path: &str,
        scope: RecoveryRenameScope,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.renamed_with_authorization(
            view,
            old_path,
            new_path,
            RenameAuthorization::Recovery(scope),
            done,
        );
    }

    pub(crate) fn renamed_native_async(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &str,
        new_path: &str,
        scope: NativeRenameScope,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.renamed_with_authorization(
            view,
            old_path,
            new_path,
            RenameAuthorization::Native(scope),
            done,
        );
    }

    fn renamed_with_authorization(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &str,
        new_path: &str,
        request: RenameAuthorization,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let token = format!("{PREFIX}{}", kit::unique());
        let old = file_identity(self.host.kind, old_path);
        let new = file_identity(self.host.kind, new_path);
        let weak = Rc::downgrade(self);
        let cancellation_token = token.clone();
        let back = kit::back_on_core(&self.services, move |()| {
            if let Some(me) = weak.upgrade() {
                me.cancel_owned_rename(
                    view,
                    &cancellation_token,
                    "the recovery rename was cancelled",
                );
            }
        });
        let callback = Arc::new(Mutex::new(Some(back)));
        let mut signals = vec![request.cancel(), request.pending()];
        if let RenameAuthorization::Owned(owned) = &request {
            signals.extend(owned.callback_cancels());
        }
        if let RenameAuthorization::Native(scope) = &request {
            let operation = scope.snapshot.selection().binding().operation();
            signals.extend(scope.additional.iter().cloned());
            signals.extend([
                scope.snapshot.loss_token(),
                operation.cancel().clone(),
                operation.shutdown().clone(),
                operation.loss_token(),
            ]);
        }
        if let RenameAuthorization::Recovery(scope) = &request {
            signals.push(scope.receipt.target().created().loss_token());
            if let shell_contract::ipc::RecoveryOpenContext::NativeStartup(operation) =
                scope.receipt.target().context()
            {
                signals.extend([
                    operation.cancel().clone(),
                    operation.shutdown().clone(),
                    operation.loss_token(),
                ]);
            }
        }
        for owner in request.loss_owners() {
            let Some(signal) = owner.loss_token() else {
                return done(Err(
                    "the recovery rename native lifetime is unavailable".into()
                ));
            };
            signals.push(signal);
        }
        let subscriptions = signals
            .into_iter()
            .map(|signal| {
                let callback = callback.clone();
                signal.on_cancel(move || {
                    let callback = callback
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take();
                    if let Some(callback) = callback {
                        callback(());
                    }
                })
            })
            .collect();
        let mut done = Some(done);
        let mut subscriptions = Some(subscriptions);
        let mut follows = false;
        let result = request.effect(&mut || {
            let mut views = self.views.borrow_mut();
            let Some(state) = views.get_mut(&view) else {
                return Ok(());
            };
            if self.owned_renames.borrow().contains_key(&view) {
                return Err("a recovery rename is already pending".into());
            }
            if !state.owns {
                return Ok(());
            }
            state.last_text = None;
            state.owns = false;
            self.owned_renames.borrow_mut().insert(
                view,
                OwnedRename {
                    token: token.clone(),
                    request: request.clone(),
                    old: old.clone(),
                    new: new.clone(),
                    claimed: false,
                    failure: None,
                    done: done.take(),
                    subscriptions: subscriptions.take().unwrap_or_default(),
                },
            );
            follows = true;
            Ok(())
        });
        if let Err(error) = result {
            if let Some(done) = done.take() {
                done(Err(error));
            }
            return;
        }
        if !follows {
            if let Some(done) = done.take() {
                done(Ok(()));
            }
            return;
        }
        let emitted = request.emit(self.host.flush_channel, vec![token.clone().into_ipc()]);
        if let Err(error) = emitted {
            self.finish_owned_rename(view, &token, Err(error));
        }
    }

    fn finish_owned_rename(&self, view: ViewId, token: &str, result: Result<(), String>) {
        let entry = {
            let mut entries = self.owned_renames.borrow_mut();
            if entries.get(&view).is_some_and(|entry| entry.token == token) {
                entries.remove(&view)
            } else {
                None
            }
        };
        if let Some(mut entry) = entry {
            let result = match entry.failure.take() {
                Some(error) => Err(error),
                None => result,
            };
            let done = entry.done.take();
            drop(entry);
            if let Some(done) = done {
                done(result);
            }
        }
    }

    fn cancel_owned_rename(&self, view: ViewId, token: &str, error: &str) {
        let claimed = {
            let mut entries = self.owned_renames.borrow_mut();
            match entries.get_mut(&view).filter(|entry| entry.token == token) {
                Some(entry) if entry.claimed => {
                    if entry.failure.is_none() {
                        entry.failure = Some(error.to_owned());
                    }
                    true
                }
                _ => false,
            }
        };
        if !claimed {
            self.finish_owned_rename(view, token, Err(error.to_owned()));
        }
    }

    pub(super) fn is_owned_rename_token(token: &str) -> bool {
        token.starts_with(PREFIX)
    }
}

impl TextRecovery {
    pub(super) fn handle_owned_rename(
        self: &Rc<Self>,
        cx: &shell_contract::ipc::CallContext,
        token: String,
        text: Option<String>,
        renderer_error: Option<String>,
        responder: shell_contract::ipc::Responder,
    ) {
        let view = cx.sender.view;
        let pending = self
            .owned_renames
            .borrow()
            .get(&view)
            .filter(|entry| entry.token == token)
            .map(|entry| entry.request.clone());
        let Some(request) = pending else {
            return responder.reply(Err(kit::error(
                "the recovery rename token is no longer active",
            )));
        };
        let Some(native) = cx.native.clone() else {
            return responder.reply(Err(kit::error(
                "the recovery flush has no authenticated sender",
            )));
        };
        let actual = shell_contract::ipc::NativeRequestOwner {
            view,
            context: native,
        };
        if actual.view != request.destination().view
            || !actual
                .context
                .renderer()
                .zip(request.destination().context.renderer())
                .is_some_and(|(a, b)| a.same_generation(b))
        {
            return responder.reply(Err(kit::error(
                "the recovery flush belongs to another renderer",
            )));
        }
        let mut participants = request.participants();
        if matches!(
            &request,
            RenameAuthorization::Recovery(_) | RenameAuthorization::Native(_)
        ) || matches!(&request,RenameAuthorization::Owned(owned) if matches!(owned.authority(),SaveAuthority::Native(_)))
        {
            participants.push(actual.clone());
        }
        let scope = ReplyScope {
            request: request.clone(),
            actual,
            cancel: cx.cancel.clone(),
            participants,
        };
        let mut identity = None;
        let admitted = scope.effect(&mut || {
            if !self.views.borrow().contains_key(&view) {
                return Err("the recovery view is no longer tracked".into());
            }
            let mut entries = self.owned_renames.borrow_mut();
            let entry = entries
                .get_mut(&view)
                .filter(|entry| entry.token == token)
                .ok_or("the recovery rename token is no longer active")?;
            if entry.claimed {
                return Err("the recovery flush was already received".into());
            }
            if let RecoveryIdentity::File(file) = &entry.new {
                if (self.host.save_path)(view).as_deref() != Some(file.file_path.as_str()) {
                    return Err("the recovery destination changed".into());
                }
            }
            entry.claimed = true;
            identity = Some(entry.new.clone());
            Ok(())
        });
        if let Err(error) = admitted {
            return responder.reply(Err(kit::error(error)));
        }
        if let Some(error) = renderer_error {
            self.finish_owned_rename(view, &token, Err(error.clone()));
            return responder.reply(Err(kit::error(error)));
        }
        let Some(text) = text else {
            self.finish_owned_rename(view, &token, Ok(()));
            return responder.reply(Ok("skipped".into_ipc()));
        };
        let Some(identity) = identity else {
            return responder.reply(Err(kit::error("the recovery flush was not admitted")));
        };
        self.prepare_rename_write(view, token, text, identity, scope, responder);
    }

    fn prepare_rename_write(
        self: &Rc<Self>,
        view: ViewId,
        token: String,
        text: String,
        identity: RecoveryIdentity,
        scope: ReplyScope,
        responder: shell_contract::ipc::Responder,
    ) {
        let title = match &identity {
            RecoveryIdentity::File(file) => basename(&file.file_path, None),
            _ => String::new(),
        };
        let options = RecoveryWriteOptions {
            title: Some(title),
            ..RecoveryWriteOptions::default()
        };
        let plan = match try_reply_store(self, &scope, |store| {
            store.prepare_write(&identity, &options)
        }) {
            Err(error) => {
                self.finish_owned_rename(view, &token, Err(error.clone()));
                return responder.reply(Err(kit::error(error)));
            }
            Ok(None) => {
                let me = Rc::clone(self);
                return self.after_store_available(move || {
                    me.prepare_rename_write(view, token, text, identity, scope, responder)
                });
            }
            Ok(Some(Ok(Some(plan)))) => plan,
            Ok(Some(Ok(None))) => {
                let error = "the recovery copy was superseded".to_owned();
                self.finish_owned_rename(view, &token, Err(error.clone()));
                return responder.reply(Err(kit::error(error)));
            }
            Ok(Some(Err(error))) => {
                let message = error.message;
                self.finish_owned_rename(view, &token, Err(message.clone()));
                return responder.reply(Err(kit::error(message)));
            }
        };
        let ticket = plan.ticket().clone();
        let worker_ticket = ticket.clone();
        let data = self
            .services
            .paths
            .path(PathName::UserData)
            .to_string_lossy()
            .into_owned();
        let admission = ticket_admission(
            scope.clone(),
            self.shared.clone(),
            data,
            (self.host.now_ms)(),
            worker_ticket,
        );
        let me = Rc::clone(self);
        let written_text = text.clone();
        kit::on_worker(
            &self.services,
            move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let metadata_admission = admission.clone();
                    let copy_admission = admission.clone();
                    plan.write_via_owned(
                        text.as_bytes(),
                        &mut |path, metadata| {
                            main_core::packages::electron_utils::atomic_write::owned::write_json(
                                path,
                                metadata,
                                metadata_admission.clone(),
                            )
                        },
                        &mut |path, bytes| {
                            main_core::packages::electron_utils::atomic_write::owned::write_file(
                                path,
                                bytes,
                                copy_admission.clone(),
                            )
                        },
                    )
                    .map_err(|error| error.message)
                }))
                .unwrap_or_else(|_| Err("the recovery copy worker panicked".into()))
            },
            move |result| {
                if let Err(error) = result {
                    me.finish_owned_rename(view, &token, Err(error.clone()));
                    return responder.reply(Err(kit::error(error)));
                }
                me.prepare_rename_delete(
                    view,
                    token,
                    written_text,
                    identity,
                    scope,
                    ticket,
                    responder,
                );
            },
        );
    }

    fn prepare_rename_delete(
        self: &Rc<Self>,
        view: ViewId,
        token: String,
        text: String,
        identity: RecoveryIdentity,
        scope: ReplyScope,
        ticket: main_core::packages::electron_utils::recovery_store::RecoveryWriteTicket,
        responder: shell_contract::ipc::Responder,
    ) {
        let old = self
            .owned_renames
            .borrow()
            .get(&view)
            .filter(|entry| entry.token == token)
            .map(|entry| entry.old.clone());
        let Some(old) = old else {
            return responder.reply(Err(kit::error("the recovery rename is no longer active")));
        };
        let plan = if old == identity {
            None
        } else {
            match try_reply_store(self, &scope, |store| {
                if !store.write_ticket_is_current(&ticket) {
                    return Err(main_core::node_fs::NodeError {
                        code: None,
                        message: "the recovery copy was superseded".into(),
                    });
                }
                store.prepare_delete(&old, RecoveryDeleteReason::ClosedClean)
            }) {
                Err(error) => {
                    self.finish_owned_rename(view, &token, Err(error.clone()));
                    return responder.reply(Err(kit::error(error)));
                }
                Ok(None) => {
                    let me = Rc::clone(self);
                    return self.after_store_available(move || {
                        me.prepare_rename_delete(
                            view, token, text, identity, scope, ticket, responder,
                        )
                    });
                }
                Ok(Some(Err(error))) => {
                    let message = error.message;
                    self.finish_owned_rename(view, &token, Err(message.clone()));
                    return responder.reply(Err(kit::error(message)));
                }
                Ok(Some(Ok(plan))) => plan,
            }
        };
        let data = self
            .services
            .paths
            .path(PathName::UserData)
            .to_string_lossy()
            .into_owned();
        let admission = ticket_admission(
            scope.clone(),
            self.shared.clone(),
            data,
            (self.host.now_ms)(),
            ticket,
        );
        let me = Rc::clone(self);
        kit::on_worker(
            &self.services,
            move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Some(plan) = plan {
                        plan.remove_owned(&mut |effect| {
                            main_core::packages::electron_utils::atomic_write::owned::admitted(
                                &admission,
                                move || effect(),
                            )
                        })
                        .map_err(|error| error.message)?;
                    }
                    Ok::<(), String>(())
                }))
                .unwrap_or_else(|_| Err("the recovery cleanup worker panicked".into()))
            },
            move |result| {
                if let Err(error) = result {
                    me.finish_owned_rename(view, &token, Err(error.clone()));
                    return responder.reply(Err(kit::error(error)));
                }
                let updated = scope.effect(&mut || {
                    let mut views = me.views.borrow_mut();
                    let state = views
                        .get_mut(&view)
                        .ok_or("the recovery view is no longer tracked")?;
                    if !me
                        .owned_renames
                        .borrow()
                        .get(&view)
                        .is_some_and(|entry| entry.token == token)
                    {
                        return Err("the recovery rename is no longer active".into());
                    }
                    state.last_text = Some(text.clone());
                    state.owns = true;
                    state.written_as = Some(identity.clone());
                    Ok(())
                });
                match updated {
                    Ok(()) => {
                        me.finish_owned_rename(view, &token, Ok(()));
                        responder.reply(Ok("written".into_ipc()));
                    }
                    Err(error) => {
                        me.finish_owned_rename(view, &token, Err(error.clone()));
                        responder.reply(Err(kit::error(error)));
                    }
                }
            },
        );
    }
}

impl TextRecovery {
    pub(super) fn fail_owned_rename(&self, view: ViewId, error: &str) {
        let token = self
            .owned_renames
            .borrow()
            .get(&view)
            .map(|entry| entry.token.clone());
        if let Some(token) = token {
            self.cancel_owned_rename(view, &token, error);
        }
    }
}
