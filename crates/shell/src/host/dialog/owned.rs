//! Native dialog sessions retain cancellation until the native close completes.
use shell_contract::{
    hosts::FileDialog,
    ipc::NativeRequestOwner,
    runtime::{CancelSubscription, CancelToken, Thread},
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
use windows as platform;

static NEXT: AtomicU64 = AtomicU64::new(1);
const EXPIRED: &str = "Renderer document is no longer available.";

/// UI-thread native identities. Retiring an ID never leaves a reusable address
/// for an already queued cancellation to find.
pub(super) struct NativeSessions<T>(HashMap<u64, T>);
impl<T> NativeSessions<T> {
    pub(super) fn new() -> Self {
        Self(HashMap::new())
    }
    pub(super) fn insert(&mut self, id: u64, target: T) -> Result<(), String> {
        match self.0.entry(id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(target);
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                Err("Native dialog session is already registered.".into())
            }
        }
    }
    pub(super) fn get(&self, id: &u64) -> Option<&T> {
        self.0.get(id)
    }
    pub(super) fn remove(&mut self, id: &u64) -> Option<T> {
        self.0.remove(id)
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

pub(crate) struct Session {
    pub(crate) id: u64,
    signalled: AtomicBool,
    tokens: Vec<CancelToken>,
    subscriptions: Mutex<Vec<CancelSubscription>>,
    #[cfg(windows)]
    pub(crate) dispatcher: std::sync::atomic::AtomicIsize,
}

impl Session {
    fn new_many(
        origin: &NativeRequestOwner,
        destinations: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
    ) -> Result<Arc<Self>, String> {
        let mut tokens = Vec::with_capacity(destinations.len() + 3);
        tokens.extend([
            cancel.clone(),
            pending.clone(),
            origin.loss_token().ok_or(EXPIRED)?,
        ]);
        for destination in destinations {
            tokens.push(destination.loss_token().ok_or(EXPIRED)?);
        }
        Self::with_tokens(tokens)
    }

    fn new_many_extra(
        origin: &NativeRequestOwner,
        destinations: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
    ) -> Result<Arc<Self>, String> {
        let mut tokens = vec![cancel.clone(), pending.clone()];
        tokens.extend_from_slice(additional);
        Self::append_recovery_owners(&mut tokens, origin, destinations)?;
        Self::with_tokens(tokens)
    }

    fn new_native_command(
        operation: &shell_contract::ipc::NativeInvocationOperation,
        participants: &[NativeRequestOwner],
    ) -> Result<Arc<Self>, String> {
        let mut tokens = vec![
            operation.loss_token(),
            operation.cancel().clone(),
            operation.shutdown().clone(),
        ];
        for participant in participants {
            Self::append_recovery_owners(&mut tokens, participant, &[])?;
        }
        Self::with_tokens(tokens)
    }

    fn new_existing_document(
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
    ) -> Result<Arc<Self>, String> {
        let operation = binding.operation();
        let mut tokens = vec![
            cancel.clone(),
            pending.clone(),
            operation.loss_token(),
            operation.cancel().clone(),
            operation.shutdown().clone(),
        ];
        tokens.extend_from_slice(additional);
        Self::append_recovery_owners(&mut tokens, binding.owner(), participants)?;
        Self::with_tokens(tokens)
    }

    fn new_save_revision(
        snapshot: &shell_contract::ipc::NativeSaveRevisionSnapshot,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
    ) -> Result<Arc<Self>, String> {
        let binding = snapshot.selection().binding();
        let operation = binding.operation();
        let mut tokens = vec![
            cancel.clone(),
            pending.clone(),
            operation.loss_token(),
            operation.cancel().clone(),
            operation.shutdown().clone(),
            snapshot.loss_token(),
        ];
        tokens.extend_from_slice(additional);
        Self::append_recovery_owners(&mut tokens, binding.owner(), participants)?;
        for alias in snapshot.aliases() {
            Self::append_recovery_owners(&mut tokens, alias, &[])?;
        }
        Self::with_tokens(tokens)
    }

    fn new_saved(document: &shell_contract::ipc::NativeSavedDocument) -> Result<Arc<Self>, String> {
        let target = document.target();
        let operation = target.output().selection().binding().operation();
        let mut tokens = vec![
            target.cancel().clone(),
            target.pending().clone(),
            target.created().loss_token(),
            operation.loss_token(),
            operation.cancel().clone(),
            operation.shutdown().clone(),
        ];
        tokens.extend_from_slice(target.additional());
        Self::append_recovery_owners(
            &mut tokens,
            target.output().selection().binding().owner(),
            target.participants(),
        )?;
        Self::append_recovery_owners(&mut tokens, document.owner(), &[])?;
        Self::with_tokens(tokens)
    }

    fn new_recovery(
        target: &shell_contract::ipc::RecoveryOpenTarget,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    ) -> Result<Arc<Self>, String> {
        use shell_contract::ipc::RecoveryOpenContext;
        let mut tokens = vec![
            target.created().loss_token(),
            target.preparation_loss_token(),
            snapshot.revision_loss_token(),
        ];
        match &snapshot.context {
            RecoveryOpenContext::NativeStartup(operation) => {
                tokens.extend([
                    operation.loss_token(),
                    operation.cancel().clone(),
                    operation.shutdown().clone(),
                ]);
            }
            RecoveryOpenContext::Renderer {
                source,
                participants,
                cancel,
                pending,
            } => {
                tokens.extend([cancel.clone(), pending.clone()]);
                Self::append_recovery_owners(&mut tokens, source, participants)?;
            }
        }
        if let Some(request) = &snapshot.request {
            tokens.extend([request.cancel.clone(), request.pending.clone()]);
            Self::append_recovery_owners(&mut tokens, &request.source, &request.participants)?;
        }
        Self::append_recovery_roots(&mut tokens, snapshot)?;
        Self::with_tokens(tokens)
    }

    fn new_committed(
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        receiver: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
    ) -> Result<Arc<Self>, String> {
        let mut tokens = vec![
            cancel.clone(),
            pending.clone(),
            receipt.target().created().loss_token(),
            snapshot.revision_loss_token(),
        ];
        Self::append_recovery_owners(&mut tokens, receiver, &[])?;
        if let shell_contract::ipc::RecoveryOpenContext::NativeStartup(operation) =
            &snapshot.context
        {
            tokens.extend([
                operation.loss_token(),
                operation.cancel().clone(),
                operation.shutdown().clone(),
            ]);
        }
        Self::append_recovery_roots(&mut tokens, snapshot)?;
        Self::with_tokens(tokens)
    }

    fn append_recovery_roots(
        tokens: &mut Vec<CancelToken>,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    ) -> Result<(), String> {
        if let Some(startup) = &snapshot.startup {
            tokens.extend([
                startup.startup.loss_token(),
                startup.startup.cancel().clone(),
                startup.startup.shutdown().clone(),
                startup.cancel.clone(),
                startup.pending.clone(),
            ]);
            Self::append_recovery_owners(tokens, &startup.shell, &startup.participants)?;
        }
        if let Some(command) = &snapshot.command {
            tokens.extend([
                command.command.loss_token(),
                command.command.cancel().clone(),
                command.command.shutdown().clone(),
                command.cancel.clone(),
                command.pending.clone(),
            ]);
            for participant in &command.participants {
                Self::append_recovery_owners(tokens, participant, &[])?;
            }
        }
        Ok(())
    }

    fn append_recovery_owners(
        tokens: &mut Vec<CancelToken>,
        source: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
    ) -> Result<(), String> {
        for owner in std::iter::once(source).chain(participants) {
            tokens.push(owner.loss_token().ok_or(EXPIRED)?);
            tokens.push(owner.context.renderer().ok_or(EXPIRED)?.loss_token());
            if let Some(lease) = owner.context.document_lease() {
                tokens.push(lease.loss_token());
            }
            if let Some(document) = owner.pdf_document() {
                tokens.push(document.loss_token());
            }
            if let Some(revision) = owner.pdf_revision() {
                tokens.push(revision.loss_token());
            }
        }
        Ok(())
    }

    fn with_tokens(tokens: impl Into<Vec<CancelToken>>) -> Result<Arc<Self>, String> {
        let tokens = tokens.into();
        let id = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| "Native dialog identifiers exhausted.")?;
        let session = Arc::new(Self {
            id,
            signalled: AtomicBool::new(false),
            tokens,
            subscriptions: Mutex::new(Vec::new()),
            #[cfg(windows)]
            dispatcher: std::sync::atomic::AtomicIsize::new(0),
        });
        let subscriptions = session
            .tokens
            .iter()
            .map(|token| {
                let weak = Arc::downgrade(&session);
                token.on_cancel(move || {
                    if let Some(session) = weak.upgrade() {
                        session.signalled.store(true, Ordering::Release);
                        platform::wake(&session);
                    }
                })
            })
            .collect();
        *session
            .subscriptions
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = subscriptions;
        Ok(session)
    }

    pub(super) fn standalone() -> Result<Arc<Self>, String> {
        let session = Self::with_tokens(Vec::new())?;
        platform::prepare(&session)?;
        Ok(session)
    }

    pub(super) fn observe_parent(self: &Arc<Self>, token: &CancelToken) {
        let weak = Arc::downgrade(self);
        let subscription = token.on_cancel(move || {
            if let Some(session) = weak.upgrade() {
                session.signalled.store(true, Ordering::Release);
                platform::wake(&session);
            }
        });
        self.subscriptions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(subscription);
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.signalled.load(Ordering::Acquire) || self.tokens.iter().any(CancelToken::is_cancelled)
    }

    pub(crate) fn check(&self) -> Result<(), String> {
        if self.cancelled() {
            Err(EXPIRED.into())
        } else {
            Ok(())
        }
    }
}

pub(crate) fn message_saved(
    ui: Arc<dyn Thread>,
    lifecycle: Arc<crate::views::NativeWindowLifecycle>,
    document: shell_contract::ipc::NativeSavedDocument,
    options: shell_contract::hosts::MessageBox,
    done: Box<dyn FnOnce(Result<shell_contract::hosts::MessageBoxAnswer, String>) + Send>,
) {
    let parent = match super::capture_owned_lifecycle(lifecycle) {
        Ok(parent) => Some(parent),
        Err(error) => return done(Err(error)),
    };
    let session = match Session::new_saved(&document) {
        Ok(session) => session,
        Err(error) => return done(Err(error)),
    };
    ui.post(Box::new(move || {
        let target = document.target();
        let mut accepted = false;
        let result = crate::ipc::native_owner::with_saved_document_dialog_effect(
            &document,
            None,
            None,
            target.cancel(),
            target.pending(),
            &mut || {
                accepted = true;
                Ok(())
            },
        )
        .and_then(|()| {
            if !accepted {
                return Err("The saved dialog was not admitted.".into());
            }
            session.check()?;
            platform::prepare(&session)?;
            message(
                &options,
                super::checked_owned_parent(&parent, &session)?,
                &session,
            )
        })
        .and_then(|answer| {
            session.check()?;
            let mut answer = Some(answer);
            let mut admitted = None;
            crate::ipc::native_owner::with_saved_document_dialog_effect(
                &document,
                None,
                None,
                target.cancel(),
                target.pending(),
                &mut || {
                    admitted = answer.take();
                    Ok(())
                },
            )?;
            admitted.ok_or_else(|| "The saved dialog result was not admitted.".into())
        });
        drop(session);
        done(result);
    }));
}

pub(crate) fn run_saved_publication<T: Send + 'static>(
    ui: Arc<dyn Thread>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    output: shell_contract::ipc::SavedPublication,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effect: impl FnOnce(Arc<Session>) -> Result<T, String> + Send + 'static,
    done: Box<dyn FnOnce(Result<T, String>) + Send>,
) {
    let mut tokens = match output.loss_tokens() {
        Ok(tokens) => tokens,
        Err(error) => return done(Err(error)),
    };
    tokens.extend([cancel.clone(), pending.clone()]);
    tokens.extend_from_slice(&additional);
    if let Err(error) =
        Session::append_recovery_owners(&mut tokens, output.source_owner(), &participants)
    {
        return done(Err(error));
    }
    let session = match Session::with_tokens(tokens) {
        Ok(session) => session,
        Err(error) => return done(Err(error)),
    };
    ui.post(Box::new(move || {
        let extra: Vec<_> = additional.iter().collect();
        let mut accepted = Some(effect);
        let mut action = None;
        let result = output
            .with_effect(
                &*effects,
                &participants,
                &cancel,
                &pending,
                &extra,
                &mut || {
                    action = accepted.take();
                    Ok(())
                },
            )
            .and_then(|()| {
                session.check()?;
                platform::prepare(&session)?;
                action.ok_or("The saved publication dialog was not admitted.")?(session.clone())
            })
            .and_then(|value| {
                session.check()?;
                let mut value = Some(value);
                let mut result = None;
                output.with_effect(
                    &*effects,
                    &participants,
                    &cancel,
                    &pending,
                    &extra,
                    &mut || {
                        result = value.take();
                        Ok(())
                    },
                )?;
                result.ok_or_else(|| "The saved publication dialog result was not admitted.".into())
            });
        drop(session);
        done(result);
    }));
}

pub(super) fn run<T: Send + 'static>(
    ui: Arc<dyn Thread>,
    origin: NativeRequestOwner,
    destination: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    effect: impl FnOnce(Arc<Session>) -> Result<T, String> + Send + 'static,
    done: Box<dyn FnOnce(Result<T, String>) + Send>,
) {
    run_many(ui, origin, vec![destination], cancel, pending, effect, done)
}

pub(crate) fn run_many<T: Send + 'static>(
    ui: Arc<dyn Thread>,
    origin: NativeRequestOwner,
    destinations: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    effect: impl FnOnce(Arc<Session>) -> Result<T, String> + Send + 'static,
    done: Box<dyn FnOnce(Result<T, String>) + Send>,
) {
    let session = match Session::new_many(&origin, &destinations, &cancel, &pending) {
        Ok(session) => session,
        Err(error) => {
            done(Err(error));
            return;
        }
    };
    let current_ui = ui.clone();
    ui.post(Box::new(move || {
        let result = crate::ipc::native_owner::run_accepted_ui_many(
            current_ui.as_ref(),
            &origin,
            &destinations,
            &cancel,
            &pending,
            || {
                session.check()?;
                platform::prepare(&session)?;
                effect(session.clone())
            },
        )
        .and_then(|result| result)
        .and_then(|value| {
            session.check()?;
            crate::ipc::native_owner::with_many_owner_transition(
                &origin,
                &destinations,
                &cancel,
                &pending,
                |_| value,
            )
            .ok_or_else(|| EXPIRED.to_owned())
        });
        // Native platform guards unregister their exact session before returning.
        drop(session);
        done(result);
    }));
}

pub(crate) fn run_many_extra<T: Send + 'static>(
    ui: Arc<dyn Thread>,
    origin: NativeRequestOwner,
    destinations: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effect: impl FnOnce(Arc<Session>) -> Result<T, String> + Send + 'static,
    done: Box<dyn FnOnce(Result<T, String>) + Send>,
) {
    let session =
        match Session::new_many_extra(&origin, &destinations, &cancel, &pending, &additional) {
            Ok(session) => session,
            Err(error) => return done(Err(error)),
        };
    let current_ui = ui.clone();
    ui.post(Box::new(move || {
        let extra: Vec<_> = additional.iter().collect();
        let result = if !current_ui.is_current() {
            Err("Native dialog requires the UI thread.".to_owned())
        } else {
            crate::ipc::native_owner::with_many_owner_effect_extra(
                &origin,
                &destinations,
                &cancel,
                &pending,
                &extra,
                || effect,
            )
            .ok_or_else(|| EXPIRED.to_owned())
            .and_then(|effect| {
                session.check()?;
                platform::prepare(&session)?;
                effect(session.clone())
            })
            .and_then(|value| {
                session.check()?;
                crate::ipc::native_owner::with_many_owner_effect_extra(
                    &origin,
                    &destinations,
                    &cancel,
                    &pending,
                    &extra,
                    || value,
                )
                .ok_or_else(|| EXPIRED.to_owned())
            })
        };
        drop(session);
        done(result);
    }));
}

pub(crate) fn run_many_extra_finish<T: Send + 'static, U: Send + 'static>(
    ui: Arc<dyn Thread>,
    origin: NativeRequestOwner,
    destinations: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effect: impl FnOnce(Arc<Session>) -> Result<T, String> + Send + 'static,
    finish: impl FnOnce(T) -> Result<U, String> + Send + 'static,
    done: Box<dyn FnOnce(Result<U, String>) + Send>,
) {
    let session =
        match Session::new_many_extra(&origin, &destinations, &cancel, &pending, &additional) {
            Ok(session) => session,
            Err(error) => return done(Err(error)),
        };
    let current_ui = ui.clone();
    ui.post(Box::new(move || {
        let extra: Vec<_> = additional.iter().collect();
        let result = if !current_ui.is_current() {
            Err("Native dialog requires the UI thread.".to_owned())
        } else {
            crate::ipc::native_owner::with_many_owner_effect_extra(
                &origin,
                &destinations,
                &cancel,
                &pending,
                &extra,
                || effect,
            )
            .ok_or_else(|| EXPIRED.to_owned())
            .and_then(|effect| {
                session.check()?;
                platform::prepare(&session)?;
                effect(session.clone())
            })
            .and_then(|value| {
                session.check()?;
                crate::ipc::native_owner::with_many_owner_effect_extra(
                    &origin,
                    &destinations,
                    &cancel,
                    &pending,
                    &extra,
                    || finish(value),
                )
                .ok_or_else(|| EXPIRED.to_owned())?
            })
        };
        drop(session);
        done(result);
    }));
}

pub(crate) fn run_native_command<T: Send + 'static>(
    ui: Arc<dyn Thread>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    operation: shell_contract::ipc::NativeInvocationOperation,
    expected: shell_contract::ipc::NativeEditorCommand,
    participants: Vec<NativeRequestOwner>,
    effect: impl FnOnce(Arc<Session>) -> Result<T, String> + Send + 'static,
    done: Box<dyn FnOnce(Result<T, String>) + Send>,
) {
    let session = match Session::new_native_command(&operation, &participants) {
        Ok(session) => session,
        Err(error) => return done(Err(error)),
    };
    ui.post(Box::new(move || {
        let cancel = operation.cancel().clone();
        let pending = operation.shutdown().clone();
        let mut effect = Some(effect);
        let mut accepted = None;
        let result = effects
            .with_native_command_preparation_effect(
                &operation,
                &expected,
                &participants,
                &cancel,
                &pending,
                &mut || {
                    accepted = effect.take();
                    Ok(())
                },
            )
            .and_then(|()| {
                session.check()?;
                platform::prepare(&session)?;
                (accepted.ok_or("The native command dialog was not admitted")?)(session.clone())
            })
            .and_then(|value| {
                session.check()?;
                let mut value = Some(value);
                let mut accepted = None;
                effects.with_native_command_preparation_effect(
                    &operation,
                    &expected,
                    &participants,
                    &cancel,
                    &pending,
                    &mut || {
                        accepted = value.take();
                        Ok(())
                    },
                )?;
                accepted
                    .ok_or_else(|| "The native command dialog result was not admitted".to_owned())
            });
        drop(session);
        done(result);
    }));
}
pub(crate) fn run_existing_document<T: Send + 'static, U: Send + 'static>(
    ui: Arc<dyn Thread>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    binding: shell_contract::ipc::NativeExistingDocumentBinding,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effect: impl FnOnce(Arc<Session>) -> Result<T, String> + Send + 'static,
    finish: impl FnOnce(T, &shell_contract::ipc::NativeExistingDocumentBinding) -> Result<U, String>
    + Send
    + 'static,
    done: Box<dyn FnOnce(Result<U, String>) + Send>,
) {
    let session = match Session::new_existing_document(
        &binding,
        &participants,
        &cancel,
        &pending,
        &additional,
    ) {
        Ok(session) => session,
        Err(error) => return done(Err(error)),
    };
    ui.post(Box::new(move || {
        let extra: Vec<_> = additional.iter().collect();
        let mut effect = Some(effect);
        let mut accepted = None;
        let result = effects
            .with_existing_document_save_effect_extra(
                &binding,
                &participants,
                &cancel,
                &pending,
                &extra,
                &mut || {
                    accepted = effect.take();
                    Ok(())
                },
            )
            .and_then(|()| {
                session.check()?;
                platform::prepare(&session)?;
                accepted.ok_or("The native save dialog was not admitted.")?(session.clone())
            })
            .and_then(|value| {
                session.check()?;
                let mut value = Some(value);
                let mut finish = Some(finish);
                let mut output = None;
                effects.with_existing_document_save_effect_extra(
                    &binding,
                    &participants,
                    &cancel,
                    &pending,
                    &extra,
                    &mut || {
                        output = Some(finish
                            .take()
                            .ok_or("The native save dialog was already completed.")?(
                            value
                                .take()
                                .ok_or("The native save dialog result was already consumed.")?,
                            &binding,
                        )?);
                        Ok(())
                    },
                )?;
                output.ok_or_else(|| "The native save dialog result was not admitted.".into())
            });
        drop(session);
        done(result);
    }));
}

pub(super) fn run_save_revision<T: Send + 'static, U: Send + 'static>(
    ui: Arc<dyn Thread>,
    snapshot: shell_contract::ipc::NativeSaveRevisionSnapshot,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effect: impl FnOnce(Arc<Session>) -> Result<T, String> + Send + 'static,
    finish: impl FnOnce(T, &shell_contract::ipc::NativeSaveRevisionSnapshot) -> Result<U, String>
    + Send
    + 'static,
    done: Box<dyn FnOnce(Result<U, String>) + Send>,
) {
    let session = match Session::new_save_revision(
        &snapshot,
        &participants,
        &cancel,
        &pending,
        &additional,
    ) {
        Ok(session) => session,
        Err(error) => return done(Err(error)),
    };
    ui.post(Box::new(move || {
        let extra: Vec<_> = additional.iter().collect();
        let result = crate::ipc::native_owner::with_native_save_revision_snapshot(
            &snapshot,
            &participants,
            &cancel,
            &pending,
            &extra,
            || effect,
        )
        .ok_or_else(|| EXPIRED.to_owned())
        .and_then(|effect| {
            session.check()?;
            platform::prepare(&session)?;
            effect(session.clone())
        })
        .and_then(|value| {
            session.check()?;
            crate::ipc::native_owner::with_native_save_revision_snapshot(
                &snapshot,
                &participants,
                &cancel,
                &pending,
                &extra,
                || finish(value, &snapshot),
            )
            .ok_or_else(|| EXPIRED.to_owned())?
        });
        drop(session);
        done(result);
    }));
}

pub(super) fn run_prepared<T: Send + 'static, U: Send + 'static>(
    ui: Arc<dyn Thread>,
    target: shell_contract::ipc::RecoveryOpenTarget,
    snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
    effect: impl FnOnce(Arc<Session>) -> Result<T, String> + Send + 'static,
    finish: impl FnOnce(
        T,
        &shell_contract::ipc::RecoveryOpenTarget,
        &shell_contract::ipc::RecoveryAuthoritySnapshot,
    ) -> Result<U, String>
    + Send
    + 'static,
    done: Box<dyn FnOnce(Result<U, String>) + Send>,
) {
    let session = match Session::new_recovery(&target, &snapshot) {
        Ok(session) => session,
        Err(error) => {
            done(Err(error));
            return;
        }
    };
    let current_ui = ui.clone();
    ui.post(Box::new(move || {
        let result = crate::ipc::native_owner::run_recovery_snapshot_ui(
            current_ui.as_ref(),
            &target,
            &snapshot,
            None,
            None,
            None,
            || {
                session.check()?;
                platform::prepare(&session)?;
                effect(session.clone())
            },
        )
        .and_then(|result| result)
        .and_then(|value| {
            session.check()?;
            crate::ipc::native_owner::with_recovery_seed_snapshot(&target, &snapshot, || {
                finish(value, &target, &snapshot)
            })
            .ok_or_else(|| EXPIRED.to_owned())?
        });
        drop(session);
        done(result);
    }));
}

pub(super) fn run_committed<T: Send + 'static, U: Send + 'static>(
    ui: Arc<dyn Thread>,
    effects: shell_contract::hosts::OwnedEffectsHost,
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
    receiver: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    effect: impl FnOnce(Arc<Session>) -> Result<T, String> + Send + 'static,
    finish: impl FnOnce(
        T,
        &shell_contract::ipc::RecoverySeedReceipt,
        &shell_contract::ipc::RecoveryAuthoritySnapshot,
    ) -> Result<U, String>
    + Send
    + 'static,
    done: Box<dyn FnOnce(Result<U, String>) + Send>,
) {
    let session = match Session::new_committed(&receipt, &snapshot, &receiver, &cancel, &pending) {
        Ok(session) => session,
        Err(error) => return done(Err(error)),
    };
    ui.post(Box::new(move || {
        let mut effect = Some(effect);
        let mut accepted = None;
        let result = effects
            .with_recovery_open_snapshot(
                &receipt,
                &snapshot,
                &receiver,
                &[],
                &cancel,
                &pending,
                &mut || {
                    accepted = effect.take();
                    Ok(())
                },
            )
            .and_then(|()| {
                session.check()?;
                platform::prepare(&session)?;
                accepted.ok_or("The recovery dialog was not admitted.")?(session.clone())
            })
            .and_then(|value| {
                session.check()?;
                let mut value = Some(value);
                let mut finish = Some(finish);
                let mut output = None;
                effects.with_recovery_open_snapshot(
                    &receipt,
                    &snapshot,
                    &receiver,
                    &[],
                    &cancel,
                    &pending,
                    &mut || {
                        output = Some(finish
                            .take()
                            .ok_or("The recovery dialog was already completed.")?(
                            value
                                .take()
                                .ok_or("The recovery dialog result was already consumed.")?,
                            &receipt,
                            &snapshot,
                        )?);
                        Ok(())
                    },
                )?;
                output.ok_or_else(|| "The recovery dialog result was not admitted.".into())
            });
        drop(session);
        done(result);
    }));
}

#[derive(Debug)]
pub(super) struct FileSpec {
    pub title: Option<String>,
    pub filters: Vec<(String, Vec<String>)>,
    pub directory: Option<PathBuf>,
    pub name: Option<String>,
    pub folders: bool,
    pub multiple: bool,
    pub create_directories: Option<bool>,
}

impl FileSpec {
    pub(super) fn of(options: &FileDialog) -> Self {
        let mut directory = None;
        let mut name = None;
        if let Some(path) = &options.default_path {
            if path.is_dir() {
                directory = Some(path.clone());
            } else {
                directory = path
                    .parent()
                    .filter(|path| path.is_dir())
                    .map(PathBuf::from);
                name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
            }
        }
        Self {
            title: options.title.clone(),
            filters: options
                .filters
                .iter()
                .filter(|filter| {
                    !filter.extensions.is_empty() && !filter.extensions.iter().any(|ext| ext == "*")
                })
                .map(|filter| (filter.name.clone(), filter.extensions.clone()))
                .collect(),
            directory,
            name,
            folders: super::has(&options.properties, "openDirectory"),
            multiple: super::has(&options.properties, "multiSelections"),
            create_directories: super::has(&options.properties, "createDirectory").then_some(true),
        }
    }
}

pub(super) fn files(
    app: &tauri::AppHandle,
    options: &FileDialog,
    parent: Option<&tauri::Window>,
    save: bool,
    session: &Arc<Session>,
) -> Result<Option<Vec<PathBuf>>, String> {
    platform::files(app, &FileSpec::of(options), parent, save, session)
}

pub(crate) fn message(
    options: &shell_contract::hosts::MessageBox,
    parent: Option<&tauri::Window>,
    session: &Arc<Session>,
) -> Result<shell_contract::hosts::MessageBoxAnswer, String> {
    super::message_box::show_owned(options, parent, session)
}

#[cfg(test)]
mod tests;
