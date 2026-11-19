use shell_contract::editors::{SaveMode, Then};
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{CallContext, NativeRequestOwner};
use shell_contract::runtime::{CancelSubscription, CancelToken};
use shell_contract::{IpcError, ViewId};

use super::engine::{DeckId, RenderOwner};
use super::state::App;
use crate::owned_save::OwnedSave;
use crate::value::{obj, string, thrown};

#[path = "owned_save_pipeline.rs"]
mod pipeline;
pub(super) use pipeline::{PendingWrite, save, save_as, save_into_project};
#[path = "save_revision.rs"]
mod revision;
pub(super) use revision::SaveRevision;

#[derive(Default)]
pub(super) struct ReloadLossWatch {
    pub marker: Option<shell_contract::hosts::AcceptedPdfReload>,
    pub bundle: Option<shell_contract::ipc::AcceptedOwnerLosses>,
    pub failure: Option<String>,
    pub subscriptions: Vec<CancelSubscription>,
}

#[derive(Clone)]
pub(super) struct SaveScope {
    pub request: OwnedSave,
    pub slot: usize,
    pub session: u64,
    pub deck: DeckId,
    render_owner: RenderOwner,
    recipients: Vec<NativeRequestOwner>,
    pub reload_losses: Option<std::sync::Arc<std::sync::Mutex<ReloadLossWatch>>>,
    pub invocation: Option<shell_contract::ipc::NativeInvocationDocument>,
    pub native_destination: Option<shell_contract::ipc::NativeSaveDestination>,
}

pub(super) struct PendingSave {
    pub scope: SaveScope,
    pub mode: SaveMode,
    pub path: Option<String>,
    pub saved: bool,
    pub copy: Option<(String, String)>,
    pub copy_taken: bool,
    pub copy_completion: Option<(
        SaveRevision,
        std::sync::Arc<shell_contract::hosts::CommittedGeneratedPath>,
    )>,
    done: Option<Then<bool>>,
    _subscriptions: Vec<CancelSubscription>,
    subscription_epoch: std::sync::Arc<std::sync::atomic::AtomicBool>,
    retired_subscriptions: Vec<Vec<CancelSubscription>>,
}

impl SaveScope {
    pub fn capture(
        app: &App,
        view: ViewId,
        origin: NativeRequestOwner,
        cancel: CancelToken,
    ) -> Result<Self, IpcError> {
        let effects = app
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or_else(|| thrown("The native save effect host is unavailable"))?;
        let state = app.state.borrow();
        let slot = state.slot_of(view).ok_or_else(|| thrown("no file open"))?;
        let session = state.slot(slot).ok_or_else(|| thrown("no file open"))?;
        let target = state
            .owner_of(view)
            .ok_or_else(|| thrown("The destination document has no captured owner"))?;
        let expected = target
            .context
            .renderer()
            .ok_or_else(|| thrown("The destination renderer is unavailable"))?;
        let destination = effects.capture_owner(view, expected).map_err(thrown)?;
        let request = OwnedSave::new(
            origin,
            destination,
            cancel,
            state
                .save_lifetime(view)
                .ok_or_else(|| thrown("The destination document was released"))?
                .clone(),
            effects.clone(),
        )?;
        let recipients = state
            .attached_owners(slot)
            .into_iter()
            .map(|owner| {
                let mut recipient = effects
                    .capture_owner(
                        owner.view,
                        owner
                            .context
                            .renderer()
                            .ok_or_else(|| thrown("An attached renderer was released"))?,
                    )
                    .map_err(thrown)?;
                let document = state
                    .save_lifetime(owner.view)
                    .ok_or_else(|| thrown("An attached presentation was released"))?
                    .clone();
                if recipient
                    .context
                    .document_lease()
                    .is_some_and(|captured| !captured.same_generation(&document))
                {
                    return Err(thrown("An attached presentation was replaced"));
                }
                recipient.context = recipient.context.with_document_lease(document);
                Ok(recipient)
            })
            .collect::<Result<Vec<_>, IpcError>>()?;
        let invocation = state.headless_targets.get(&view).cloned();
        if invocation.as_ref().is_some_and(|document| {
            !target
                .context
                .renderer()
                .is_some_and(|renderer| renderer.same_generation(document.created()))
        }) {
            return Err(thrown(
                "The headless export belongs to another native document",
            ));
        }
        Ok(Self {
            request,
            slot,
            session: session.id,
            deck: session.deck(),
            render_owner: target.clone(),
            recipients,
            reload_losses: None,
            invocation,
            native_destination: None,
        })
    }

    pub fn capture_native(
        app: &App,
        view: ViewId,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
    ) -> Result<Self, IpcError> {
        let effects = app
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or_else(|| thrown("The native save effect host is unavailable"))?;
        let state = app.state.borrow();
        let slot = state.slot_of(view).ok_or_else(|| thrown("no file open"))?;
        let session = state.slot(slot).ok_or_else(|| thrown("no file open"))?;
        let target = state
            .owner_of(view)
            .ok_or_else(|| thrown("The destination document has no captured owner"))?;
        let document = state
            .save_lifetime(view)
            .ok_or_else(|| thrown("The destination document was released"))?
            .clone();
        if binding.owner().view != view
            || !binding
                .owner()
                .context
                .renderer()
                .zip(target.context.renderer())
                .is_some_and(|(actual, expected)| actual.same_generation(expected))
        {
            return Err(thrown("The native save belongs to another presentation"));
        }
        let request = OwnedSave::new_native(binding, cancel, pending, document, effects.clone())?;
        let recipients = state
            .attached_owners(slot)
            .into_iter()
            .map(|owner| {
                let mut recipient = effects
                    .capture_owner(
                        owner.view,
                        owner
                            .context
                            .renderer()
                            .ok_or_else(|| thrown("An attached renderer was released"))?,
                    )
                    .map_err(thrown)?;
                let document = state
                    .save_lifetime(owner.view)
                    .ok_or_else(|| thrown("An attached presentation was released"))?
                    .clone();
                if recipient
                    .context
                    .document_lease()
                    .is_some_and(|captured| !captured.same_generation(&document))
                {
                    return Err(thrown("An attached presentation was replaced"));
                }
                recipient.context = recipient.context.with_document_lease(document);
                Ok(recipient)
            })
            .collect::<Result<Vec<_>, IpcError>>()?;
        let scope = Self {
            request,
            slot,
            session: session.id,
            deck: session.deck(),
            render_owner: target.clone(),
            recipients,
            reload_losses: None,
            invocation: None,
            native_destination: None,
        };
        drop(state);
        scope.effect(app, || Ok(()))?;
        Ok(scope)
    }

    fn apply_revision_receipt(
        &mut self,
        receipt: &shell_contract::ipc::NativeSaveRevisionReceipt,
    ) -> Result<(), IpcError> {
        self.request.apply_revision_receipt(receipt)?;
        if let Some(owner) = receipt.renewed_owner(&self.render_owner) {
            self.render_owner = owner.clone();
        }
        for owner in &mut self.recipients {
            if let Some(successor) = receipt.renewed_owner(owner) {
                *owner = successor.clone();
            }
        }
        self.native_destination = Some(receipt.snapshot().selection().clone());
        Ok(())
    }

    pub fn check(&self, app: &App) -> Result<(), IpcError> {
        self.request.check()?;
        if self.invocation.as_ref().is_some_and(|document| {
            !document.created().is_active()
                || document.operation().loss_token().is_cancelled()
                || document.operation().cancel().is_cancelled()
                || document.operation().shutdown().is_cancelled()
        }) {
            return Err(thrown("The native export operation was released"));
        }
        self.check_document(app)
    }

    pub(super) fn check_document(&self, app: &App) -> Result<(), IpcError> {
        let state = app.state.borrow();
        let view = self.request.destination.view;
        state
            .slot(self.slot)
            .filter(|session| session.id == self.session && session.deck() == self.deck)
            .ok_or_else(|| thrown("The destination presentation was replaced"))?;
        if state.slot_of(view) != Some(self.slot)
            || !state
                .save_lifetime(view)
                .is_some_and(|lifetime| lifetime.same_generation(&self.request.document))
        {
            return Err(thrown("The destination presentation was replaced"));
        }
        Ok(())
    }

    pub fn effect<T>(
        &self,
        app: &App,
        action: impl FnOnce() -> Result<T, IpcError>,
    ) -> Result<T, IpcError> {
        match &self.invocation {
            Some(document) => self.request.effect_invocation(document, &[], || {
                self.check(app)?;
                action()
            }),
            None => self.request.effect(|| {
                self.check(app)?;
                action()
            }),
        }
    }
}

impl App {
    fn save_subscriptions(
        &self,
        id: &str,
        scope: &SaveScope,
        reload_losses: &std::sync::Arc<std::sync::Mutex<ReloadLossWatch>>,
        epoch: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Vec<CancelSubscription> {
        let index = self.registry_index;
        let mut subscriptions = Vec::new();
        let mut losses = vec![
            (scope.request.authority.loss_token(), 0),
            (Some(scope.request.cancel.clone()), 2),
            (Some(scope.request.document.loss_token()), 1),
        ];
        if let Some(snapshot) = &scope.request.revision_snapshot {
            losses.push((Some(snapshot.loss_token()), 1));
        }
        losses.extend(
            scope
                .request
                .participants(&scope.recipients)
                .iter()
                .map(|owner| (owner.loss_token(), 1)),
        );
        for (loss, role) in losses
            .into_iter()
            .filter_map(|(loss, role)| loss.map(|loss| (loss, role)))
            .chain(
                scope
                    .request
                    .additional
                    .iter()
                    .cloned()
                    .map(|loss| (loss, 3)),
            )
        {
            let core = self.services.core.clone();
            let pending = scope.request.pending.clone();
            let id = id.to_owned();
            let reload_losses = reload_losses.clone();
            let original_cancel = scope.request.cancel.clone();
            let epoch = epoch.clone();
            subscriptions.push(loss.on_cancel(move || {
                if !epoch.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                if reload_losses.lock().ok().is_some_and(|watch| {
                    watch.bundle.as_ref().is_some_and(|bundle| {
                        role == 0 || role == 2 && !bundle.cancel().same_token(&original_cancel)
                    })
                }) {
                    return;
                }
                pending.cancel();
                let id = id.clone();
                core.post(Box::new(move || {
                    super::with_app(index, |app| {
                        app.finish_owned_save(&id, Err(thrown("The save owner was released")))
                    });
                }));
            }));
        }
        subscriptions
    }

    fn insert_save(
        &self,
        mut scope: SaveScope,
        mode: SaveMode,
        path: Option<String>,
        done: Option<Then<bool>>,
    ) -> String {
        let reload_losses = std::sync::Arc::new(std::sync::Mutex::new(ReloadLossWatch::default()));
        scope.reload_losses = Some(reload_losses.clone());
        let id = self.shell.random_uuid();
        let epoch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let subscriptions = self.save_subscriptions(&id, &scope, &reload_losses, &epoch);
        let request = PendingSave {
            scope: scope.clone(),
            mode,
            path,
            saved: false,
            copy: None,
            copy_taken: false,
            copy_completion: None,
            done,
            _subscriptions: subscriptions,
            subscription_epoch: epoch,
            retired_subscriptions: Vec::new(),
        };
        self.state
            .borrow_mut()
            .owned_saves
            .insert(id.clone(), request);
        id
    }

    pub(super) fn begin_renderer_save(
        &self,
        cx: &CallContext,
        mode: SaveMode,
    ) -> Result<String, IpcError> {
        let scope = SaveScope::capture(
            self,
            cx.sender.view,
            super::session::render_owner(cx)?,
            cx.cancel.clone(),
        )?;
        Ok(self.insert_save(scope, mode, None, None))
    }

    pub(super) fn release_direct_save(&self, id: &str) {
        let pending = {
            let mut state = self.state.borrow_mut();
            if !state
                .owned_saves
                .get(id)
                .is_some_and(|save| save.done.is_none())
            {
                return;
            }
            state.owned_saves.remove(id)
        };
        if let Some(pending) = pending {
            pending.scope.request.pending.cancel();
        }
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
        let scope = match SaveScope::capture(self, view, origin, cancel) {
            Ok(scope) => scope,
            Err(error) => return done(Err(error)),
        };
        let id = self.insert_save(scope.clone(), mode, path, Some(done));
        let emitted = scope.request.emit(
            &scope.request.destination,
            "slides:close-save-request",
            vec![obj([
                ("id", string(&id)),
                (
                    "mode",
                    string(if mode == SaveMode::SaveAs {
                        "saveAs"
                    } else {
                        "save"
                    }),
                ),
            ])],
        );
        if let Err(error) = emitted {
            self.finish_owned_save(&id, Err(error));
            return;
        }
        self.after(super::lifecycle::CLOSE_SAVE_TIMEOUT, move |app| {
            app.finish_owned_save(&id, Err(thrown("Timed out saving the presentation")))
        });
    }

    pub(super) fn request_save_native(
        &self,
        view: ViewId,
        mode: SaveMode,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        done: Then<bool>,
    ) {
        if !matches!(binding.operation().selection(), shell_contract::ipc::NativeInvocationSelection::NativeCommand(
            shell_contract::ipc::NativeEditorCommand::SaveDocument { surface: shell_contract::Surface::Slides, view: selected_view, selection: shell_contract::ipc::NativeSaveSelection::Mode(selected_mode) }
        ) if *selected_view == view && *selected_mode == mode)
        {
            return done(Err(thrown(
                "The native save selection does not match the presentation request",
            )));
        }
        let mut scope =
            match SaveScope::capture_native(self, view, binding, cancel, CancelToken::default()) {
                Ok(scope) => scope,
                Err(error) => return done(Err(error)),
            };
        scope.request.additional.push(pending);
        if let Err(error) = scope.check(self) {
            return done(Err(error));
        }
        let id = self.insert_save(scope.clone(), mode, None, Some(done));
        let emitted = scope.request.emit(
            &scope.request.destination,
            "slides:close-save-request",
            vec![obj([
                ("id", string(&id)),
                (
                    "mode",
                    string(if mode == SaveMode::SaveAs {
                        "saveAs"
                    } else {
                        "save"
                    }),
                ),
            ])],
        );
        if let Err(error) = emitted {
            self.finish_owned_save(&id, Err(error));
            return;
        }
        self.after(super::lifecycle::CLOSE_SAVE_TIMEOUT, move |app| {
            app.finish_owned_save(&id, Err(thrown("Timed out saving the presentation")))
        });
    }

    pub(super) fn export_scope(
        &self,
        cx: &CallContext,
        id: Option<&str>,
        kind: &str,
        path: &str,
    ) -> Result<SaveScope, IpcError> {
        if let Some(id) = id {
            let (scope, _) = self.owned_save_for(cx, id, None)?;
            scope.effect(self, || {
                let mut state = self.state.borrow_mut();
                let pending = state
                    .owned_saves
                    .get_mut(id)
                    .ok_or_else(|| thrown("The save request was released"))?;
                if pending.copy_taken
                    || pending
                        .copy
                        .as_ref()
                        .is_none_or(|(selected_kind, selected_path)| {
                            selected_kind != kind || selected_path != path
                        })
                {
                    return Err(thrown(
                        "This copy destination was not selected by the save request",
                    ));
                }
                pending.copy_taken = true;
                Ok(())
            })?;
            Ok(scope)
        } else {
            SaveScope::capture(
                self,
                cx.sender.view,
                super::session::render_owner(cx)?,
                cx.cancel.clone(),
            )
        }
    }

    pub(super) fn owned_save_for(
        &self,
        cx: &CallContext,
        id: &str,
        mode: Option<SaveMode>,
    ) -> Result<(SaveScope, Option<String>), IpcError> {
        let state = self.state.borrow();
        let pending = state
            .owned_saves
            .get(id)
            .ok_or_else(|| thrown("The save request is no longer pending"))?;
        pending.scope.request.validate_sender(cx)?;
        if mode.is_some_and(|mode| mode != pending.mode) {
            return Err(thrown("The save response has the wrong mode"));
        }
        let mut scope = pending.scope.clone();
        let path = pending.path.clone();
        drop(state);
        scope.request = scope.request.capture_callback(cx)?;
        scope.check(self)?;
        let core = self.services.core.clone();
        let index = self.registry_index;
        let operation = id.to_owned();
        let operation_pending = scope.request.pending.clone();
        let subscription = cx.cancel.on_cancel(move || {
            operation_pending.cancel();
            core.post(Box::new(move || {
                super::with_app(index, |app| {
                    app.finish_owned_save(&operation, Err(thrown("The save callback was released")))
                });
            }));
        });
        let mut state = self.state.borrow_mut();
        let pending = state
            .owned_saves
            .get_mut(id)
            .ok_or_else(|| thrown("The save request is no longer pending"))?;
        if !pending
            .scope
            .request
            .pending
            .same_token(&scope.request.pending)
        {
            return Err(thrown("The save request was replaced"));
        }
        pending._subscriptions.push(subscription);
        pending.scope = scope.clone();
        Ok((scope, path))
    }

    pub(super) fn finish_owned_save(&self, id: &str, result: Result<bool, IpcError>) {
        let pending = self.state.borrow_mut().owned_saves.remove(id);
        if let Some(pending) = pending {
            let result = result.and_then(|ok| match &pending.copy_completion {
                Some((revision, receipt)) => receipt
                    .with_terminal_commit(&mut || {
                        pending
                            .scope
                            .check_revision_state(self, revision)
                            .map_err(|error| error.message)
                    })
                    .map(|()| ok && pending.saved)
                    .map_err(thrown),
                None => pending.scope.effect(self, || Ok(ok && pending.saved)),
            });
            pending.scope.request.pending.cancel();
            if let Some(done) = pending.done {
                done(result);
            }
        }
    }

    pub(super) fn owned_save_answered(
        &self,
        cx: &CallContext,
        id: &str,
        ok: bool,
    ) -> Result<(), IpcError> {
        let reloaded = {
            let state = self.state.borrow();
            let pending = state
                .owned_saves
                .get(id)
                .ok_or_else(|| thrown("The save request is no longer pending"))?;
            let actual = cx.native.as_ref().and_then(|native| native.renderer());
            if cx.sender.view != pending.scope.request.destination.view
                || !actual
                    .zip(pending.scope.request.destination.context.renderer())
                    .is_some_and(|(actual, expected)| actual.same_generation(expected))
            {
                return Err(thrown("The save response belongs to another renderer"));
            }
            pending.copy_completion.is_some()
        };
        if !reloaded {
            self.owned_save_for(cx, id, None)?;
        }
        self.finish_owned_save(id, Ok(ok));
        Ok(())
    }
}
