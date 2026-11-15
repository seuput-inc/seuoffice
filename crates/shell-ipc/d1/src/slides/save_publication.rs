use super::*;
use shell_contract::hosts::{SavedFile, SavedSessionReceipt};

impl App {
    pub(super) fn publish_next(&self, id: &str) {
        let (scope, recipient, path) = {
            let state = self.state.borrow();
            let Some(write) = state.owned_writes.get(id) else {
                return;
            };
            let index = write.scope.recipients.len() - write.remaining;
            let Some(recipient) = write.scope.recipients.get(index).cloned() else {
                return;
            };
            (write.scope.clone(), recipient, write.path.clone())
        };
        let target = std::rc::Rc::new(std::cell::RefCell::new(recipient.clone()));
        let done_target = target.clone();
        let app = self.clone();
        let write_id = id.to_owned();
        let done = Box::new(move |answer| {
            app.saved_hook_done(&write_id, done_target.borrow().clone(), answer)
        });
        let request = &scope.request;
        match &scope.native_destination {
            Some(_) => {
                let Some(revision) = request.revision.clone() else {
                    return done(Err("The native save revision is unavailable".into()));
                };
                let app = self.clone();
                let write_id = id.to_owned();
                let on_revision = std::rc::Rc::new(
                    move |receipt: &shell_contract::ipc::NativeSaveRevisionReceipt| {
                        app.apply_write_revision(&write_id, receipt)
                            .map_err(|error| error.message)?;
                        let successor = {
                            let current = target.borrow();
                            receipt.renewed_owner(&current).cloned()
                        };
                        if let Some(successor) = successor {
                            *target.borrow_mut() = successor;
                        }
                        Ok(())
                    },
                );
                self.shell.file_saved_existing_document(
                    revision,
                    path.into(),
                    request.participants(std::slice::from_ref(&recipient)),
                    request.cancel.clone(),
                    request.pending.clone(),
                    request.additional.clone(),
                    request.effects.clone(),
                    on_revision,
                    done,
                )
            }
            None => match request.authority.renderer() {
                Ok(origin) => self.shell.file_saved_owned(
                    origin,
                    &recipient,
                    &request.cancel,
                    &request.pending,
                    &request.effects,
                    path.into(),
                    &request.participants(&[]),
                    &request.additional,
                    done,
                ),
                Err(error) => done(Err(error.message)),
            },
        }
    }

    fn apply_write_revision(
        &self,
        id: &str,
        receipt: &shell_contract::ipc::NativeSaveRevisionReceipt,
    ) -> Result<(), IpcError> {
        let (updated, pending_updates) = {
            let state = self.state.borrow();
            let write = state
                .owned_writes
                .get(id)
                .ok_or_else(|| thrown("The save request was released"))?;
            let mut updated = write.scope.clone();
            updated.apply_revision_receipt(receipt)?;
            let mut pending_updates = Vec::new();
            for (save_id, pending) in &state.owned_saves {
                if !pending
                    .scope
                    .request
                    .pending
                    .same_token(&updated.request.pending)
                {
                    continue;
                }
                let mut scope = pending.scope.clone();
                scope.apply_revision_receipt(receipt)?;
                let watch = scope
                    .reload_losses
                    .as_ref()
                    .ok_or_else(|| thrown("The save lifetime watch is unavailable"))?;
                let epoch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                let subscriptions = self.save_subscriptions(save_id, &scope, watch, &epoch);
                pending_updates.push((save_id.clone(), scope, subscriptions, epoch));
            }
            (updated, pending_updates)
        };
        let mut state = self.state.borrow_mut();
        let write = state
            .owned_writes
            .get_mut(id)
            .ok_or_else(|| thrown("The save request was released"))?;
        if !write
            .scope
            .request
            .pending
            .same_token(&updated.request.pending)
        {
            return Err(thrown("The save request was replaced"));
        }
        write.scope = updated;
        for (save_id, scope, subscriptions, epoch) in pending_updates {
            let pending = state
                .owned_saves
                .get_mut(&save_id)
                .ok_or_else(|| thrown("The save request was released"))?;
            pending
                .subscription_epoch
                .store(false, std::sync::atomic::Ordering::Release);
            pending.subscription_epoch = epoch;
            let old = std::mem::replace(&mut pending._subscriptions, subscriptions);
            pending.retired_subscriptions.push(old);
            pending.scope = scope;
        }
        Ok(())
    }

    fn saved_hook_done(
        &self,
        id: &str,
        recipient: NativeRequestOwner,
        answer: Result<SavedFile<std::path::PathBuf>, String>,
    ) {
        let saved = match answer {
            Ok(saved) => saved,
            Err(error) => return self.publication_done(id, Err(thrown(error)), false),
        };
        let path = crate::host::path_string(saved.path);
        let (scope, revision) = {
            let state = self.state.borrow();
            let Some(write) = state.owned_writes.get(id) else {
                return;
            };
            (
                write.scope.clone(),
                SaveRevision {
                    path: write.path.clone(),
                    meta_rev: write.meta_rev,
                    op_seq: write.op_seq,
                },
            )
        };
        let previous = match scope.effect_at_revision(self, &revision, || {
            self.state
                .borrow()
                .slot(scope.slot)
                .map(|session| session.path().to_owned())
        }) {
            Ok(Some(previous)) => previous,
            Ok(None) => {
                return self.publication_done(
                    id,
                    Err(thrown("The presentation was released")),
                    false,
                );
            }
            Err(error) => return self.publication_done(id, Err(error), false),
        };
        if previous != path {
            let app = self.clone();
            let id = id.to_owned();
            let target_path = path.clone();
            let request = scope.request;
            let done = Box::new(move |answer| match answer {
                Ok(()) => app.finalize_saved_hook(&id, recipient, target_path, saved.session),
                Err(error) => app.publication_done(&id, Err(error), false),
            });
            match &request.authority {
                crate::owned_save::SaveAuthority::Renderer(origin) => self
                    .rename_path_many_owned_extra(
                        request.destination.view,
                        &previous,
                        &path,
                        origin,
                        &request.destination,
                        &request.participants(&[]),
                        &request.cancel,
                        &request.pending,
                        &request.additional,
                        done,
                    ),
                crate::owned_save::SaveAuthority::Native(binding) => self.rename_path_existing(
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
        } else {
            self.finalize_saved_hook(id, recipient, path, saved.session);
        }
    }

    fn finalize_saved_hook(
        &self,
        id: &str,
        recipient: NativeRequestOwner,
        path: String,
        receipt: SavedSessionReceipt,
    ) {
        let (scope, revision) = {
            let state = self.state.borrow();
            let Some(write) = state.owned_writes.get(id) else {
                return;
            };
            (
                write.scope.clone(),
                SaveRevision {
                    path: write.path.clone(),
                    meta_rev: write.meta_rev,
                    op_seq: write.op_seq,
                },
            )
        };
        let accepted = scope
            .effect_at_revision(self, &revision, || {
                if !self
                    .state
                    .borrow()
                    .slot(scope.slot)
                    .is_some_and(|session| session.path() == path)
                {
                    return Err(thrown(
                        "The final saved presentation path was not acknowledged",
                    ));
                }
                Ok(())
            })
            .and_then(|answer| answer);
        if let Err(error) = accepted {
            return self.publication_done(id, Err(error), false);
        }
        let app = self.clone();
        let id = id.to_owned();
        receipt.finalize(Box::new(move |answer| match answer {
            Err(error) => app.publication_done(&id, Err(thrown(error)), false),
            Ok(()) => app.saved_hook_published(&id, recipient, path),
        }));
    }

    fn saved_hook_published(&self, id: &str, recipient: NativeRequestOwner, path: String) {
        let (scope, revision) = {
            let state = self.state.borrow();
            let Some(write) = state.owned_writes.get(id) else {
                return;
            };
            (
                write.scope.clone(),
                SaveRevision {
                    path: write.path.clone(),
                    meta_rev: write.meta_rev,
                    op_seq: write.op_seq,
                },
            )
        };
        if let Err(error) = scope.effect_at_revision(self, &revision, || ()) {
            return self.publication_done(id, Err(error), false);
        }
        let request = &scope.request;
        if let Err(error) = request.emit(&recipient, "slides:renamed", vec![string(&path)]) {
            return self.publication_done(id, Err(error), false);
        }
        let services = self.services.clone();
        let index = self.registry_index;
        let id = id.to_owned();
        let done = Box::new(move |answer| {
            crate::slides::post_to(&services, index, move |app| {
                app.publication_done(&id, answer.map_err(thrown), false)
            })
        });
        match &scope.native_destination {
            Some(destination) => self.shell.set_window_title_existing_document(
                destination.clone(),
                request.participants(std::slice::from_ref(&recipient)),
                request.cancel.clone(),
                request.pending.clone(),
                request.additional.clone(),
                request.effects.clone(),
                basename(&path, None),
                done,
            ),
            None => match request.authority.renderer() {
                Ok(origin) => self.shell.set_window_title_owned(
                    origin,
                    &recipient,
                    &request.cancel,
                    &request.pending,
                    basename(&path, None),
                    &request.effects,
                    &request.participants(&[]),
                    &request.additional,
                    done,
                ),
                Err(error) => done(Err(error.message)),
            },
        }
    }
}
