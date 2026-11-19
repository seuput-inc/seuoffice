use super::owned_save::{SaveRevision, SaveScope};
use super::state::App;
use crate::value::thrown;
use main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason;
use shell_contract::editors::{SaveMode, Then};
use shell_contract::hosts::MessageBox;
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelSubscription, CancelToken, PathName};
use shell_contract::{IpcError, ViewId, WindowId};

pub(super) struct PendingClose {
    scope: SaveScope,
    done: Then<bool>,
    _losses: Vec<CancelSubscription>,
}

impl App {
    pub(super) fn request_close_owned(
        &self,
        view: ViewId,
        parent: Option<WindowId>,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        done: Then<bool>,
    ) {
        if !self.state.borrow().has_session(view) {
            return done(Ok(true));
        }
        let scope = match SaveScope::capture(self, view, origin, cancel) {
            Ok(scope) => scope,
            Err(error) => return done(Err(error)),
        };
        let id = self.shell.random_uuid();
        let mut losses = Vec::new();
        for loss in [
            scope.request.authority.loss_token(),
            scope.request.destination.loss_token(),
            Some(scope.request.cancel.clone()),
            Some(scope.request.document.loss_token()),
        ]
        .into_iter()
        .flatten()
        {
            let pending = scope.request.pending.clone();
            let services = self.services.clone();
            let index = self.registry_index;
            let id = id.clone();
            losses.push(loss.on_cancel(move || {
                pending.cancel();
                super::post_to(&services, index, move |app| {
                    app.finish_owned_close(&id, Err(thrown("The close owner was released")))
                });
            }));
        }
        self.state.borrow_mut().owned_closes.insert(
            id.clone(),
            PendingClose {
                scope: scope.clone(),
                done,
                _losses: losses,
            },
        );
        let dirty = self.is_view_dirty(view);
        match dirty {
            Err(error) => return self.finish_owned_close(&id, Err(error)),
            Ok(false) => return self.finish_owned_close(&id, Ok(true)),
            Ok(true) => {}
        }
        let (shared, auto) = {
            let state = self.state.borrow();
            (
                state.editor_attached_ids(scope.slot).len() > 1,
                state.auto_save_pref.get(&view).copied().unwrap_or(false)
                    && state.slot(scope.slot).is_some_and(|s| !s.path().is_empty()),
            )
        };
        if shared {
            return self.finish_owned_close(&id, Ok(true));
        }
        if auto {
            let app = self.clone();
            let origin = match scope.request.authority.renderer() {
                Ok(origin) => origin.clone(),
                Err(error) => return self.finish_owned_close(&id, Err(error)),
            };
            self.request_save_owned(
                view,
                SaveMode::Save,
                origin,
                scope.request.cancel,
                None,
                Box::new(move |result| match result {
                    Ok(true) => app.finish_owned_close(&id, Ok(true)),
                    Ok(false) | Err(_) => app.close_owned_dialog(&id, parent),
                }),
            );
        } else {
            self.close_owned_dialog(&id, parent);
        }
    }

    fn close_owned_dialog(&self, id: &str, parent: Option<WindowId>) {
        let scope = self
            .state
            .borrow()
            .owned_closes
            .get(id)
            .map(|pending| pending.scope.clone());
        let Some(scope) = scope else { return };
        let message = MessageBox {
            kind: Some("warning".into()),
            message: self.tm("closeUnsavedMsg", &[]),
            detail: Some(self.tm("closeUnsavedDetail", &[])),
            buttons: vec![
                self.tm("menuSave", &[]),
                self.tm("btnDontSave", &[]),
                self.tm("btnCancel", &[]),
            ],
            default_id: Some(0),
            cancel_id: Some(2),
            ..Default::default()
        };
        let request = scope.request;
        let services = self.services.clone();
        let index = self.registry_index;
        let id = id.to_owned();
        request.message_dialog(
            &self.dialogs,
            parent,
            message,
            Box::new(move |answer| {
                super::post_to(&services, index, move |app| match answer {
                    Err(error) => app.finish_owned_close(&id, Err(thrown(error))),
                    Ok(answer) if answer.response == 2 => app.finish_owned_close(&id, Ok(false)),
                    Ok(answer) if answer.response == 1 => app.discard_owned_close(&id),
                    Ok(_) => app.save_owned_close(&id),
                })
            }),
        );
    }

    fn save_owned_close(&self, id: &str) {
        let scope = self
            .state
            .borrow()
            .owned_closes
            .get(id)
            .map(|pending| pending.scope.clone());
        let Some(scope) = scope else { return };
        let app = self.clone();
        let id = id.to_owned();
        let origin = match scope.request.authority.renderer() {
            Ok(origin) => origin.clone(),
            Err(error) => return self.finish_owned_close(&id, Err(error)),
        };
        self.request_save_owned(
            scope.request.destination.view,
            SaveMode::Save,
            origin,
            scope.request.cancel,
            None,
            Box::new(move |result| app.finish_owned_close(&id, result)),
        );
    }

    fn discard_owned_close(&self, id: &str) {
        let scope = self
            .state
            .borrow()
            .owned_closes
            .get(id)
            .map(|pending| pending.scope.clone());
        let Some(scope) = scope else { return };
        let revision = match scope.revision(self) {
            Ok(revision) => revision,
            Err(error) => return self.finish_owned_close(id, Err(error)),
        };
        let identities = {
            let state = self.state.borrow();
            let Some(session) = state.slot(scope.slot) else {
                return self.finish_owned_close(id, Err(thrown("The presentation was released")));
            };
            let mut identities = Vec::new();
            if !session.path().is_empty() {
                identities.push(super::recovery::file_recovery_id(session.path()));
            }
            if let Some(identity) = &session.recovery_id {
                identities.push(identity.clone());
            }
            identities
        };
        let recovery = self.recovery_state.clone();
        let directory = crate::host::path_string(self.services.paths.path(PathName::UserData));
        let admission = scope.revision_admission(self, revision.clone());
        let services = self.services.clone();
        let index = self.registry_index;
        let id = id.to_owned();
        self.services.workers.spawn(Box::new(move || {
            let result = crate::owned_recovery::delete_copies(
                &recovery,
                &directory,
                &identities,
                RecoveryDeleteReason::DontSave,
                &admission,
            )
            .map_err(|error| thrown(error.message));
            super::post_to(&services, index, move |app| {
                app.close_discarded(&id, scope, revision, result)
            });
        }));
    }

    fn close_discarded(
        &self,
        id: &str,
        scope: SaveScope,
        revision: SaveRevision,
        result: Result<Vec<crate::owned_recovery::DeletedCopy>, IpcError>,
    ) {
        let result = result.and_then(|outcomes| {
            scope.effect_at_revision(self, &revision, || {
                for outcome in &outcomes {
                    self.backoff.borrow_mut().remove(&outcome.path);
                }
                if !outcomes.iter().any(|outcome| outcome.refused)
                    && let Some(session) = self.state.borrow().slot(scope.slot)
                {
                    session.copy_mark.set(None);
                }
                true
            })
        });
        self.finish_owned_close(id, result);
    }

    fn finish_owned_close(&self, id: &str, result: Result<bool, IpcError>) {
        let close = self.state.borrow_mut().owned_closes.remove(id);
        let Some(close) = close else { return };
        let result = result.and_then(|ok| close.scope.effect(self, || Ok(ok)));
        close.scope.request.pending.cancel();
        (close.done)(result);
    }
}
