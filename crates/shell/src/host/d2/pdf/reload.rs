use crate::native::{DocumentContext, ReloadReservation, WebviewExt};
use crate::{
    host::HostContext,
    ipc::native_owner::{NativeOwnerAuthority, capture_owner},
    views::ViewTable,
};
use shell_contract::{
    Hosts, Provided,
    hosts::{AcceptedPdfReload, OwnedEffectsHost, PdfReload, PdfReloadAccepted, PdfReloadHost},
    ipc::NativeRequestOwner,
    runtime::{CancelToken, RendererGeneration, Services},
};
use shell_ipc_d2::pdf::editor::{editors::PdfEditors, reload::back_on_core};
use std::sync::{Arc, Mutex};

struct NativeReload {
    cx: HostContext,
}
struct Reservation {
    native: ReloadReservation,
    views: Arc<ViewTable>,
    view: shell_contract::ViewId,
    renderer: RendererGeneration,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.views.cancel_native_reload(self.view, &self.renderer);
        self.native.cancel();
    }
}
struct Prepared {
    reservation: Reservation,
    webview: tauri::Webview,
}

type PreparedReply = Arc<Mutex<Option<Box<dyn FnOnce(Result<Prepared, String>) + Send>>>>;
fn prepared_reply(reply: &PreparedReply, result: Result<Prepared, String>) {
    let callback = reply
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some(callback) = callback {
        callback(result);
    }
}
type Loaded = Result<(NativeRequestOwner, AcceptedPdfReload), String>;
type LoadedReply = Arc<Mutex<Option<Box<dyn FnOnce(Loaded) + Send>>>>;
fn loaded_reply(reply: &LoadedReply, result: Loaded) {
    let callback = reply
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some(callback) = callback {
        callback(result);
    }
}

#[derive(Clone)]
struct ReloadCompletion(
    std::rc::Rc<std::cell::RefCell<Option<shell_contract::hosts::NativeSaveReloadDone>>>,
);
impl ReloadCompletion {
    fn finish(&self, result: Result<Arc<shell_contract::hosts::NativeSaveReloadReceipt>, String>) {
        let done = self.0.borrow_mut().take();
        if let Some(done) = done {
            done(result);
        }
    }
}
impl Drop for ReloadCompletion {
    fn drop(&mut self) {
        if std::rc::Rc::strong_count(&self.0) == 1 {
            self.finish(Err("The PDF reload continuation was discarded.".into()));
        }
    }
}

pub(crate) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let host: PdfReloadHost = Arc::new(NativeReload { cx: cx.clone() });
    hosts.insert(host);
}

impl PdfReload for NativeReload {
    fn reload_saved_document(
        &self,
        provided: &Provided,
        services: &Services,
        input: shell_contract::ipc::NativeSavedInput,
        path: shell_contract::ipc::NativeSavedPath,
        existing: NativeRequestOwner,
        mut participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        accepted: PdfReloadAccepted,
        done: shell_contract::hosts::NativeSaveReloadDone,
    ) {
        if !path.matches_publication(input.output()) {
            return done(Err(
                "The PDF reload input does not match its saved publication.".into(),
            ));
        }
        if matches!(&path, shell_contract::ipc::NativeSavedPath::Selected(_))
            && !input.source_commit().same_commit(input.output().commit())
        {
            return done(Err(
                "The PDF reload input does not belong to its selected output.".into(),
            ));
        }
        if !services.core.is_current() {
            return done(Err("PDF reload requires the core thread.".into()));
        }
        let Some(editor) = provided.all::<PdfEditors>().pop() else {
            return done(Err("The PDF editor is not installed.".into()));
        };
        let Some(effects) = services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return done(Err("Native owned effects are not installed.".into()));
        };
        let original = match existing.context.authority_ref::<NativeOwnerAuthority>() {
            Some(authority) => authority.origin.clone(),
            None => return done(Err("The PDF reload target is not authenticated.".into())),
        };
        let renderer = match existing.context.renderer() {
            Some(renderer) => renderer.clone(),
            None => return done(Err("The PDF reload renderer is unavailable.".into())),
        };
        let webview = match self.cx.windows.webview(existing.view) {
            Some(webview) => webview,
            None => return done(Err("The captured PDF browser is closed.".into())),
        };
        participants.push(existing.clone());
        let view = existing.view;
        let prepare_original = original.clone();
        let core_services = services.clone();
        let done = ReloadCompletion(std::rc::Rc::new(std::cell::RefCell::new(Some(done))));
        let on_prepared = back_on_core(services, move |result: Result<Prepared, String>| {
            let Prepared {
                reservation,
                webview,
            } = match result {
                Ok(prepared) => prepared,
                Err(error) => return done.finish(Err(error)),
            };
            let staged = match editor.prepare_saved_reload(
                &existing,
                reservation.renderer.clone(),
                input.clone(),
                path.clone(),
            ) {
                Ok(staged) => staged,
                Err(error) => return done.finish(Err(error)),
            };
            if !staged.matches_actor() {
                return done.finish(Err(
                    "The captured PDF file slot changed before reload.".into()
                ));
            }
            let browser_loss = CancelToken::default();
            let on_close = browser_loss.clone();
            let browser_subscription = match reservation
                .native
                .browser_close()
                .ok_or_else(|| "The PDF browser is already gone.".to_owned())
                .and_then(|close| close.when_closed(move |_| on_close.cancel()))
            {
                Ok(subscription) => subscription,
                Err(error) => return done.finish(Err(error)),
            };
            let reservation_loss = CancelToken::default();
            let on_failure = reservation_loss.clone();
            let failure_subscription =
                match reservation.native.on_failure(move |_| on_failure.cancel()) {
                    Ok(subscription) => subscription,
                    Err(error) => return done.finish(Err(error)),
                };
            let lineage: Arc<Mutex<Option<AcceptedPdfReload>>> = Arc::default();
            let handoff: Arc<Mutex<Option<Arc<shell_contract::hosts::NativeSaveReloadHandoff>>>> =
                Arc::default();
            let subscriptions: Arc<Mutex<Vec<shell_contract::runtime::CancelSubscription>>> =
                Arc::default();
            let retained_subscriptions = subscriptions.clone();
            let next = reservation.renderer.clone();
            let views = reservation.views.clone();
            let dispatch_reservation = reservation.native.clone();
            let native_for_handoff = reservation.native.clone();
            let completed_handoff = handoff.clone();
            let completion_effects = effects.clone();
            let predecessor_native = original.clone();
            let same_native_context: Arc<dyn Fn(&NativeRequestOwner) -> bool + Send + Sync> =
                Arc::new(move |owner| {
                    owner
                        .context
                        .authority_ref::<NativeOwnerAuthority>()
                        .is_some_and(|authority| authority.origin == predecessor_native)
                });
            let mut acceptance = match shell_contract::hosts::PreparedAcceptedPdfReload::new(
                input.clone(),
                path.clone(),
                existing.clone(),
                next.clone(),
                browser_loss.clone(),
                reservation_loss.clone(),
                same_native_context,
                participants.clone(),
                cancel.clone(),
                pending.clone(),
                additional.clone(),
            ) {
                Ok(prepared) => prepared,
                Err(error) => return done.finish(Err(error)),
            };
            let on_loaded = back_on_core(&core_services, move |result: Loaded| {
                let result = result.and_then(|(owner, lineage)| {
                    let handoff = completed_handoff
                        .lock()
                        .map_err(|_| "The PDF reload handoff is unavailable.")?
                        .take()
                        .ok_or_else(|| {
                            "The PDF reload has no admitted native handoff.".to_owned()
                        })?;
                    let mut staged = staged;
                    let owner = staged.prepare_loaded_owner(owner)?;
                    let committed = std::cell::RefCell::new(
                        None::<shell_ipc_d2::pdf::editor::reload::AdmittedPdfReloadCompletion>,
                    );
                    let mut before_notify = || {
                        if let Some(completed) = committed.borrow_mut().as_mut() {
                            completed.before_notify();
                        }
                    };
                    completion_effects.with_native_save_reload_completion_effect(
                        &handoff,
                        &owner,
                        &mut before_notify,
                        &mut |retirement| {
                            *committed.borrow_mut() = Some(staged.complete_admitted(
                                owner.clone(),
                                lineage.clone(),
                                retirement,
                            )?);
                            Ok(())
                        },
                    )?;
                    let completed = committed
                        .into_inner()
                        .ok_or("The PDF reload completion was not admitted.")?;
                    Ok((completed, handoff))
                });
                let (completed, handoff) = match result {
                    Ok(value) => value,
                    Err(error) => return done.finish(Err(error)),
                };
                let completion = done.clone();
                let callback = Box::new(move |result| {
                    let _retained = (
                        reservation,
                        browser_subscription,
                        failure_subscription,
                        retained_subscriptions,
                    );
                    completion.finish(result);
                });
                if let Err(error) = completed.await_saved_ack(handoff, callback) {
                    done.finish(Err(error));
                }
            });
            let completion: LoadedReply = Arc::new(Mutex::new(Some(on_loaded)));
            let actual_completed = completion.clone();
            let accepted_lineage = lineage.clone();
            let accepted_handoff = handoff.clone();
            let admission = Box::new(move || {
                let mut admitted = None;
                let extra: Vec<_> = additional.iter().collect();
                effects.with_saved_reload_acceptance(
                    &input,
                    &path,
                    &participants,
                    &cancel,
                    &pending,
                    &extra,
                    &mut |permit| {
                        admitted = Some(acceptance.commit(permit)?);
                        Ok(())
                    },
                )?;
                let (marker, handoff) =
                    admitted.ok_or("The native reload admission did not run.")?;
                let mut controls = vec![
                    cancel.clone(),
                    pending.clone(),
                    browser_loss.clone(),
                    reservation_loss.clone(),
                ];
                controls.extend(additional.iter().cloned());
                let mut owners = participants.clone();
                match input.output() {
                    shell_contract::ipc::SavedPublication::Native(publication) => {
                        let operation = publication.selection().binding().operation();
                        controls.extend([
                            operation.cancel().clone(),
                            operation.shutdown().clone(),
                            operation.authority_generation().loss_token(),
                            publication.cancel().clone(),
                            publication.pending().clone(),
                        ]);
                        controls.extend(publication.additional().iter().cloned());
                        owners.push(publication.selection().binding().owner().clone());
                        owners.extend(publication.participants().iter().cloned());
                    }
                    shell_contract::ipc::SavedPublication::Renderer(publication) => {
                        controls
                            .extend([publication.cancel().clone(), publication.pending().clone()]);
                        controls.extend(publication.additional().iter().cloned());
                        owners.push(publication.origin().clone());
                        owners.push(publication.destination().clone());
                        owners.extend(publication.participants().iter().cloned());
                    }
                }
                for owner in &owners {
                    if !marker.retires(owner) {
                        controls.push(
                            owner
                                .loss_token()
                                .ok_or("A PDF reload participant has no lifetime.")?,
                        );
                    }
                }
                {
                    let mut stored = subscriptions
                        .lock()
                        .map_err(|_| "The PDF reload subscriptions are unavailable.")?;
                    for control in controls {
                        let native = native_for_handoff.clone();
                        stored.push(control.on_cancel(move || native.cancel()));
                    }
                }
                *accepted_handoff
                    .lock()
                    .map_err(|_| "The PDF reload handoff is unavailable.")? = Some(handoff);
                *accepted_lineage
                    .lock()
                    .map_err(|_| "The PDF reload lineage is unavailable.")? = Some(marker.clone());
                accepted(marker);
                Ok(())
            });
            let queued = webview.reload_reserved(
                &dispatch_reservation,
                admission,
                move |result: Result<DocumentContext, String>| {
                    let result = result.and_then(|token| {
                        let owner = capture_owner(&views, view, &next)?;
                        let authority = owner
                            .context
                            .authority_ref::<NativeOwnerAuthority>()
                            .ok_or("The reloaded PDF native owner is unavailable.")?;
                        if authority.origin != token {
                            return Err(
                                "The PDF loaded a different native document than its reservation."
                                    .into(),
                            );
                        }
                        let marker = lineage
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .take()
                            .ok_or("The PDF load has no admitted predecessor lineage.")?;
                        Ok((owner, marker))
                    });
                    loaded_reply(&actual_completed, result);
                },
            );
            if let Err(error) = queued {
                loaded_reply(&completion, Err(error));
            }
        });
        let reply: PreparedReply = Arc::new(Mutex::new(Some(on_prepared)));
        let prepared = reply.clone();
        let captured_webview = webview.clone();
        let views = self.cx.views.clone();
        let queued = webview.with_native_async(move |native| {
            let result = native
                .and_then(|_| ReloadReservation::prepare(&prepare_original))
                .and_then(|reservation| {
                    let next = views.reserve_native_reload(
                        view,
                        &renderer,
                        &prepare_original,
                        &reservation,
                    )?;
                    Ok(Prepared {
                        reservation: Reservation {
                            native: reservation,
                            views,
                            view,
                            renderer: next,
                        },
                        webview: captured_webview,
                    })
                });
            prepared_reply(&prepared, result);
        });
        if let Err(error) = queued {
            prepared_reply(&reply, Err(error));
        }
    }
}
