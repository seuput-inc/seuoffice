use super::*;
use shell_contract::hosts::{AcceptedPdfReload, OwnedEffectsHost, PdfReload, PdfReloadAccepted};
use shell_contract::ipc::NativeRequestOwner;

pub(super) struct SyntheticPdfReload {
    pub(super) views: Arc<FakeViews>,
    pub(super) shared: Shared,
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
            self.finish(Err(
                "The synthetic PDF reload continuation was discarded.".into()
            ));
        }
    }
}

impl PdfReload for SyntheticPdfReload {
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
            return done(Err("Synthetic PDF reload requires the core thread.".into()));
        }
        let Some(editor) = provided
            .all::<crate::pdf::editor::editors::PdfEditors>()
            .pop()
        else {
            return done(Err("The synthetic PDF editor is not installed.".into()));
        };
        let Some(effects) = services.hosts.get::<OwnedEffectsHost>().cloned() else {
            return done(Err(
                "The synthetic owned effects service is not installed.".into()
            ));
        };
        let scripted = self.shared.0.lock().unwrap().pdf_reload.pop_front();
        let Some(scripted) = scripted else {
            return done(Err(
                "The synthetic PDF native load has no scripted outcome.".into(),
            ));
        };
        let next = RendererGeneration::new();
        let prepared =
            match editor.prepare_saved_reload(&existing, next.clone(), input.clone(), path.clone())
            {
                Ok(prepared) => prepared,
                Err(error) => return done(Err(error)),
            };
        if !prepared.matches_actor() {
            return done(Err(
                "The synthetic PDF file slot changed before reload.".into()
            ));
        }
        let view = existing.view;
        participants.push(existing.clone());
        let starting = kit::back_on_core(services, move |(): ()| editor.start_loading(view));
        let browser_loss = match self
            .views
            .browser_lifetimes
            .lock()
            .unwrap()
            .get(&view)
            .cloned()
        {
            Some(loss) => loss,
            None => return done(Err("The synthetic PDF browser was destroyed.".into())),
        };
        let synthetic_context = existing.context.clone();
        let same_context: Arc<dyn Fn(&NativeRequestOwner) -> bool + Send + Sync> =
            Arc::new(move |owner| {
                owner
                    .context
                    .renderer()
                    .zip(synthetic_context.renderer())
                    .is_some_and(|(a, b)| a.same_generation(b))
                    && owner
                        .context
                        .document_lease()
                        .zip(synthetic_context.document_lease())
                        .is_some_and(|(a, b)| a.same_generation(b))
            });
        let mut acceptance = match shell_contract::hosts::PreparedAcceptedPdfReload::new(
            input.clone(),
            path.clone(),
            existing.clone(),
            next.clone(),
            browser_loss.clone(),
            next.loss_token(),
            same_context,
            participants.clone(),
            cancel.clone(),
            pending.clone(),
            additional.clone(),
        ) {
            Ok(prepared) => prepared,
            Err(error) => return done(Err(error)),
        };
        let completion_effects = effects.clone();
        let done = ReloadCompletion(std::rc::Rc::new(std::cell::RefCell::new(Some(done))));
        let back = kit::back_on_core(
            services,
            move |result: Result<
                (
                    NativeRequestOwner,
                    AcceptedPdfReload,
                    Arc<shell_contract::hosts::NativeSaveReloadHandoff>,
                ),
                String,
            >| {
                let result = result.and_then(|(owner, lineage, handoff)| {
                    let mut prepared = prepared;
                    let owner = prepared.prepare_loaded_owner(owner)?;
                    let admitted = std::cell::RefCell::new(
                        None::<crate::pdf::editor::reload::AdmittedPdfReloadCompletion>,
                    );
                    let mut before_notify = || {
                        if let Some(completed) = admitted.borrow_mut().as_mut() {
                            completed.before_notify();
                        }
                    };
                    completion_effects.with_native_save_reload_completion_effect(
                        &handoff,
                        &owner,
                        &mut before_notify,
                        &mut |retirement| {
                            *admitted.borrow_mut() = Some(prepared.complete_admitted(
                                owner.clone(),
                                lineage.clone(),
                                retirement,
                            )?);
                            Ok(())
                        },
                    )?;
                    let completed = admitted
                        .into_inner()
                        .ok_or("The synthetic PDF reload completion was not admitted.")?;
                    Ok((completed, handoff))
                });
                let (completed, handoff) = match result {
                    Ok(value) => value,
                    Err(error) => return done.finish(Err(error)),
                };
                let callback = done.clone();
                if let Err(error) = completed.await_saved_ack(
                    handoff,
                    Box::new(move |result| {
                        callback.finish(result);
                    }),
                ) {
                    done.finish(Err(error));
                }
            },
        );
        let views = self.views.clone();
        let shared = self.shared.clone();
        services.ui.post(Box::new(move || {
            let mut admitted = None;
            let extra: Vec<_> = additional.iter().collect();
            let result = effects.with_saved_reload_acceptance(
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
            );
            if let Err(error) = result {
                return back(Err(error));
            }
            let Some((marker, handoff)) = admitted else {
                return back(Err("The synthetic native reload was not admitted.".into()));
            };
            accepted(marker.clone());
            if cancel.is_cancelled()
                || pending.is_cancelled()
                || additional.iter().any(CancelToken::is_cancelled)
                || browser_loss.is_cancelled()
                || !next.is_active()
                || std::iter::once(input.output().source_owner())
                    .chain(participants.iter())
                    .any(|owner| {
                        !marker.retires(owner)
                            && owner.loss_token().is_none_or(|loss| loss.is_cancelled())
                    })
            {
                next.revoke();
                return back(Err("The synthetic PDF reload was cancelled.".into()));
            }
            shared.log(HostCall::ReloadPdf(view));
            starting(());
            let old = views.generations.lock().unwrap().insert(view, next.clone());
            if let Some(old) = old {
                old.revoke();
                let listeners = views.replacements.lock().unwrap().clone();
                for listener in listeners {
                    listener(view, old.clone());
                }
            }
            if let Err(error) = scripted {
                next.revoke();
                return back(Err(error));
            }
            let result = effects
                .capture_owner(view, &next)
                .map(|owner| NativeRequestOwner {
                    view,
                    context: owner.context.with_document_lease(RendererGeneration::new()),
                })
                .map(|owner| {
                    shared.0.lock().unwrap().pdf_reload_owner = Some(owner.clone());
                    (owner, marker, handoff)
                });
            back(result);
        }));
    }
}

#[cfg(test)]
mod tests;
