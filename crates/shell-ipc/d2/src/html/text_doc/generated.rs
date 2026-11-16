//! A text export's publication and what follows it. The output is staged on a
//! worker, published on the core turn under the save's authority, and then
//! opened or revealed through the shell's retained generated-path operation.
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use shell_contract::hosts::{
    CommittedGeneratedPath, GeneratedPathOutcome, GeneratedPathWake, OsShellHost, OwnedEffectsHost,
    PdfReloadAccepted, PreparedGeneratedPath,
};
use shell_contract::ipc::{NativeRequestOwner, NativeSavedInput, SavedPublication};
use shell_contract::runtime::{CancelSubscription, CancelToken, Services};

use super::{OwnedSave, SaveAuthority, TextDocs};
use crate::docs::hosts::GeneratedActorWake;
use crate::generated_mailbox::{self, Actor, CleanupMailbox};
use crate::staged::{Published, StagedOutput};

/// The shell's preparation of an opened or revealed generated output.
pub(crate) type Prepare = Box<
    dyn FnOnce(
        SavedPublication,
        Option<NativeSavedInput>,
        Vec<NativeRequestOwner>,
        CancelToken,
        CancelToken,
        Vec<CancelToken>,
        OwnedEffectsHost,
        PdfReloadAccepted,
        GeneratedPathWake,
    ) -> Result<Box<dyn PreparedGeneratedPath>, String>,
>;

impl Actor for TextDocs {
    fn services(&self) -> &Services {
        &self.services
    }
    fn mailbox(&self) -> &CleanupMailbox {
        &self.cleanup_mailbox
    }
    fn register_cleanup(
        &self,
        wake: GeneratedActorWake,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String> {
        (self.hooks.register_cleanup)(wake)
    }
}

/// What an accepted PDF replacement retires, kept until the export settles.
#[derive(Default)]
struct Reload {
    losses: Mutex<Option<shell_contract::ipc::AcceptedOwnerLosses>>,
    subscriptions: Mutex<Vec<CancelSubscription>>,
}

fn reload_accepted(request: &OwnedSave, hold: Arc<Reload>) -> PdfReloadAccepted {
    let origin = request.origin().cloned();
    let cancel = request.cancel();
    let pending = request.pending();
    Box::new(move |marker| {
        let losses = match origin.as_ref().filter(|origin| marker.retires(origin)) {
            Some(origin) => match origin.accepted_reload_losses(&marker, &cancel, &pending) {
                Ok(losses) => Some(losses),
                Err(_) => {
                    pending.cancel();
                    return;
                }
            },
            None => None,
        };
        let tokens: Vec<_> = match losses.as_ref() {
            Some(losses) => losses.tokens().cloned().collect(),
            None => vec![
                marker.browser_loss().clone(),
                marker.reservation_loss().clone(),
            ],
        };
        let mut subscriptions = hold
            .subscriptions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for loss in tokens {
            let pending = pending.clone();
            subscriptions.push(loss.on_cancel(move || pending.cancel()));
        }
        drop(subscriptions);
        *hold
            .losses
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = losses;
    })
}

impl OwnedSave {
    /// Publishes a staged output on this core turn under the save's authority.
    /// A native save publishes against the destination it selected for `path`.
    pub(crate) fn publish_staged(
        &self,
        path: &str,
        mut staged: StagedOutput,
    ) -> Result<Published, String> {
        self.check_target(Path::new(path))?;
        let participants = self.captured_participants();
        let additional = self.callback_cancels();
        let selection = match self.authority() {
            SaveAuthority::Renderer(_) => None,
            SaveAuthority::Native(_) => Some(
                self.native_destination()?
                    .filter(|selection| selection.path() == Path::new(path))
                    .ok_or("the native save destination does not match the selected path")?,
            ),
        };
        let commit = staged.take()?;
        let result = match (self.authority(), selection) {
            (SaveAuthority::Renderer(origin), _) => self.effects().publish_owned_staged_write(
                origin,
                self.destination(),
                &participants,
                &self.cancel(),
                &self.pending(),
                &additional,
                Path::new(path),
                commit,
            ),
            (SaveAuthority::Native(_), Some(selection)) => {
                self.effects().publish_native_staged_write_validated(
                    &selection,
                    &participants,
                    &self.cancel(),
                    &self.pending(),
                    &additional,
                    commit,
                    &mut || Ok(()),
                )
            }
            (SaveAuthority::Native(_), None) => {
                staged.restore(commit);
                return Err("the native save destination was not selected".into());
            }
        };
        staged.conclude(result)
    }
}

impl TextDocs {
    /// Reveals an already written file under the save's authority; nothing is
    /// published, so no generated operation is retained.
    pub(crate) fn reveal_written(
        &self,
        request: &OwnedSave,
        os: OsShellHost,
        path: std::path::PathBuf,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        match request.authority() {
            SaveAuthority::Renderer(origin) => request.effects().post_ui_owned_many_extra(
                origin.clone(),
                request.captured_participants(),
                request.cancel(),
                request.pending(),
                request.callback_cancels(),
                Box::new(move || {
                    os.show_item_in_folder(&path);
                    Ok(())
                }),
                crate::kit::back_on_core(&self.services, then),
            ),
            SaveAuthority::Native(_) => then(request.admit(&mut || {
                os.show_item_in_folder(&path);
                Ok(())
            })),
        }
    }
}

fn finish(receipt: &CommittedGeneratedPath, then: Box<dyn FnOnce(Result<(), String>)>) {
    let mut accepted = false;
    let admitted = receipt.with_terminal_commit(&mut || {
        accepted = true;
        Ok(())
    });
    then(match admitted {
        Ok(()) if accepted => Ok(()),
        Ok(()) => Err("The generated export was not admitted".into()),
        Err(error) => Err(error),
    });
}

fn revealed_receipt(request: &OwnedSave) -> Result<CommittedGeneratedPath, String> {
    match request.authority() {
        SaveAuthority::Renderer(origin) => CommittedGeneratedPath::from_loaded_target(
            origin.clone(),
            request.destination().clone(),
            request.destination().clone(),
            request.cancel(),
            request.pending(),
            request.effects().clone(),
        ),
        SaveAuthority::Native(_) => {
            let selection = request
                .native_destination()?
                .ok_or("the native save destination was not selected")?;
            CommittedGeneratedPath::from_existing_document_save(
                selection,
                request.destination().clone(),
                request.captured_participants(),
                request.cancel(),
                request.pending(),
                request.callback_cancels(),
                request.effects().clone(),
            )
        }
    }
}

impl TextDocs {
    /// Opens or reveals a published export. `then` runs on the core thread once
    /// the shell's terminal result was committed, or with the failure; a
    /// headless run has no one to show the file to, so the caller does not use
    /// this there.
    pub(crate) fn complete_export(
        self: &Rc<Self>,
        request: OwnedSave,
        published: Published,
        os: OsShellHost,
        prepare: Prepare,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let (output, input) = published;
        let path = output.path().to_owned();
        let hold = Arc::new(Reload::default());
        let retained = hold.clone();
        let completed = request.clone();
        let docs = Rc::clone(self);
        let Some(ticket) = generated_mailbox::begin(
            self,
            Box::new(move |opened, settled| {
                let _retained = retained;
                match opened {
                    Ok(GeneratedPathOutcome::Opened(receipt)) => {
                        finish(&receipt, then);
                        settled.complete();
                    }
                    Err(error) => {
                        then(Err(error));
                        settled.complete();
                    }
                    Ok(GeneratedPathOutcome::CleanupPending(_)) => {
                        then(Err(
                            "The generated export has an invalid nested cleanup".into()
                        ));
                        settled.complete();
                    }
                    Ok(GeneratedPathOutcome::NotOpened) => {
                        let after = completed.clone();
                        let owner = settled.id();
                        let revealed_at = path.clone();
                        let done = generated_mailbox::callback(
                            &docs,
                            owner,
                            move |result: Result<(), String>| {
                                match result.and_then(|()| revealed_receipt(&after)) {
                                    Ok(receipt) => finish(&receipt, then),
                                    Err(error) => then(Err(error)),
                                }
                                settled.complete();
                            },
                        );
                        let action: Box<dyn FnOnce() -> Result<(), String> + Send> =
                            Box::new(move || {
                                os.show_item_in_folder(&revealed_at);
                                Ok(())
                            });
                        match completed.authority() {
                            SaveAuthority::Renderer(origin) => {
                                completed.effects().post_ui_owned_many_extra(
                                    origin.clone(),
                                    completed.captured_participants(),
                                    completed.cancel(),
                                    completed.pending(),
                                    completed.callback_cancels(),
                                    action,
                                    done,
                                )
                            }
                            SaveAuthority::Native(binding) => {
                                completed.effects().post_ui_existing_document_save_extra(
                                    binding.clone(),
                                    completed.captured_participants(),
                                    completed.cancel(),
                                    completed.pending(),
                                    completed.callback_cancels(),
                                    action,
                                    done,
                                )
                            }
                        }
                    }
                }
            }),
        ) else {
            return;
        };
        let wake = ticket.wake();
        match prepare(
            output,
            Some(input),
            request.captured_participants(),
            request.cancel(),
            request.pending(),
            request.callback_cancels(),
            request.effects().clone(),
            reload_accepted(&request, hold),
            wake,
        ) {
            Ok(prepared) => ticket.attach(self, prepared),
            Err(error) => ticket.fail(self, error),
        }
    }
}
