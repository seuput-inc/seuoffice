use super::*;
use crate::tabs::activation::TabActivationAuthority;
use crate::tabs::host::{
    CapturedSavedShell, EditorSeedRollbackStore, PreparedSavedView, RetainedSpareBinding,
    SavedViewLoaded, ShellWindow, ViewCreationCleanup, ViewCreationFailure,
};
use main_core::shell::tab_manager::ViewCreationCleanupState;
use shell_contract::hosts::{
    CommittedGeneratedPath, GeneratedPathOutcome, OwnedEffectsHost, PdfReloadAccepted,
    PdfReloadHost,
};
use shell_contract::ipc::{NativeSavedInput, NativeSavedPath};
use std::rc::Rc;
use std::sync::Arc;

type SavedOpenDone = Box<dyn FnOnce(Result<GeneratedPathOutcome, String>) + Send>;

/// What the fresh-editor continuation keeps from the original request.
struct SavedOpen {
    id: u64,
    epoch: u64,
    rollback: EditorSeedRollbackStore,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
}

impl TabsState {
    /// Open a committed saved input as an editor. A PDF already open at the
    /// path is reloaded in place; any other open tab, an unsupported kind or a
    /// kind that cannot adopt a fresh saved view reports `NotOpened`, so the
    /// caller reveals the file instead of granting new editor authority.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_saved_input_owned(
        &self,
        input: NativeSavedInput,
        path: NativeSavedPath,
        shell: ShellWindow,
        shell_owner: NativeRequestOwner,
        captured_shell: Rc<dyn CapturedSavedShell>,
        epoch: u64,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
        accepted: PdfReloadAccepted,
        finish: SavedOpenDone,
    ) {
        if epoch != self.manager_epoch.get()
            || cancel.is_cancelled()
            || pending.is_cancelled()
            || self.shell_window() != Some(shell)
            || shell_owner.view != shell.view
        {
            return finish(Err(
                "The captured saved-open operation is no longer active".into()
            ));
        }
        let Some(path_text) = path.path().to_str().map(str::to_owned) else {
            return finish(Ok(GeneratedPathOutcome::NotOpened));
        };
        let request = match document_kind(&path_text) {
            Some(kind) if kind != TabKind::Diagram => {
                editor_request_for_path(kind, &path_text).map(|request| (kind, request))
            }
            _ => None,
        };
        let Some((kind, request)) = request else {
            return finish(Ok(GeneratedPathOutcome::NotOpened));
        };
        let existing = self
            .with_manager(|manager, _| {
                manager.find_tab_of_kind_by_path(kind, Some(&path_text)).map(|id| {
                    let view = manager.web_contents_for_tab(&id);
                    (id, view)
                })
            })
            .flatten();
        if let Some((id, view)) = existing {
            return match view {
                Some(view) if kind == TabKind::Pdf => self.reload_saved_pdf(
                    id,
                    view,
                    input,
                    path,
                    participants,
                    cancel,
                    pending,
                    additional,
                    effects,
                    accepted,
                    finish,
                ),
                _ => finish(Ok(GeneratedPathOutcome::NotOpened)),
            };
        }
        // Acceptance exists only for an in-place PDF reload; a fresh editor has none.
        drop(accepted);
        let id = match self.owned_id() {
            Ok(id) => id,
            Err(error) => return finish(Err(error)),
        };
        self.open_saved_fresh(
            request,
            input,
            path,
            captured_shell,
            SavedOpen {
                id,
                epoch,
                rollback: Rc::default(),
                participants,
                cancel,
                pending,
                additional,
                effects,
            },
            finish,
        );
    }

    fn reload_saved_pdf(
        &self,
        id: String,
        view: ViewId,
        input: NativeSavedInput,
        path: NativeSavedPath,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
        accepted: PdfReloadAccepted,
        finish: SavedOpenDone,
    ) {
        let Some(reload) = self.services.hosts.get::<PdfReloadHost>().cloned() else {
            return finish(Ok(GeneratedPathOutcome::NotOpened));
        };
        let existing = match self
            .services
            .views
            .renderer_generation(view)
            .ok_or_else(|| "The open PDF renderer is unavailable".to_owned())
            .and_then(|generation| effects.capture_owner(view, &generation))
        {
            Ok(existing) => existing,
            Err(error) => return finish(Err(error)),
        };
        let state = self.this.clone();
        reload.reload_saved_document(
            &self.provided,
            &self.services,
            input,
            path,
            existing,
            participants,
            cancel,
            pending,
            additional,
            accepted,
            Box::new(move |result| {
                let receipt = match result {
                    Ok(receipt) => receipt,
                    Err(error) => return finish(Err(error)),
                };
                let Some(state) = state.upgrade() else {
                    return finish(Err("The tab manager is unavailable".into()));
                };
                let owner = receipt.successor().clone();
                let participants = receipt.participants().to_vec();
                let cancel = receipt.cancel().clone();
                let pending = receipt.pending().clone();
                let authority = TabActivationAuthority::NativeSaveReload {
                    receipt: receipt.clone(),
                    owner: owner.clone(),
                };
                state.activate_loaded_authorized(
                    &id,
                    authority,
                    owner,
                    participants,
                    cancel,
                    pending,
                    Box::new(move |result| {
                        finish(
                            result
                                .and_then(|()| {
                                    CommittedGeneratedPath::from_native_save_reload(
                                        receipt, effects,
                                    )
                                })
                                .map(|receipt| GeneratedPathOutcome::Opened(Arc::new(receipt))),
                        )
                    }),
                    None,
                    None,
                );
            }),
        );
    }

    fn open_saved_fresh(
        &self,
        request: EditorOpen,
        input: NativeSavedInput,
        path: NativeSavedPath,
        captured_shell: Rc<dyn CapturedSavedShell>,
        open: SavedOpen,
        finish: SavedOpenDone,
    ) {
        let id = open.id;
        let Some(plan) = self
            .inner
            .borrow_mut()
            .manager_call(|manager, host| manager.prepare_editor_open(host, request))
        else {
            return finish(Err("The shell window is unavailable".into()));
        };
        if plan.reserved_view().is_some() {
            self.cancel_owned_step(Step::New(plan));
            return finish(Err(
                "A saved editor cannot adopt the reserved spare view".into()
            ));
        }
        if let Err(error) = self.begin_creation(id) {
            self.cancel_owned_step(Step::New(plan));
            return finish(Err(error));
        }
        let prepared = match plan.creation_spec() {
            Some(spec) => captured_shell.prepare_fresh(
                &spec,
                input,
                path,
                open.participants.clone(),
                open.cancel.clone(),
                open.pending.clone(),
                open.additional.clone(),
                open.rollback.clone(),
            ),
            None => Err(main_core::shell::tab_manager::TabError(
                "The saved editor has no creation request".into(),
            )),
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let state = self.rc();
                let reply = Rc::new(RefCell::new(Some(finish)));
                let failed = reply.clone();
                let message = error.0.clone();
                self.retain_editor_cleanup(
                    None,
                    None,
                    open.rollback,
                    false,
                    Box::new(move |failure| {
                        if let Some(done) = failed.borrow_mut().take() {
                            done(Err(format!("{message}; cleanup failed: {failure}")));
                        }
                    }),
                    Box::new(move || {
                        state.cancel_owned_step(Step::New(plan));
                        state.finish_creation(id);
                        if let Some(done) = reply.borrow_mut().take() {
                            done(Err(error.0));
                        }
                    }),
                );
                return;
            }
        };
        let PreparedSavedView {
            target,
            binding,
            start,
        } = prepared;
        let retained = RetainedSpareBinding::new(binding, open.rollback.clone(), None);
        let adoption = match self.prepare_saved_manager_adoption(plan, &target, retained, open.epoch)
        {
            Ok(adoption) => adoption,
            Err((plan, error)) => {
                drop(start);
                let state = self.rc();
                let reply = Rc::new(RefCell::new(Some(finish)));
                let failed = reply.clone();
                let message = error.clone();
                self.retain_editor_cleanup(
                    None,
                    None,
                    open.rollback,
                    false,
                    Box::new(move |failure| {
                        if let Some(done) = failed.borrow_mut().take() {
                            done(Err(format!("{message}; cleanup failed: {failure}")));
                        }
                    }),
                    Box::new(move || {
                        state.cancel_owned_step(Step::New(plan));
                        state.finish_creation(id);
                        if let Some(done) = reply.borrow_mut().take() {
                            done(Err(error));
                        }
                    }),
                );
                return;
            }
        };
        let PreparedSavedManagerAdoption {
            adopt,
            register,
            tab_id,
            recover,
        } = adoption;
        let state = self.rc();
        start(
            adopt,
            register,
            Box::new(move |result| {
                if let Some(plan) = recover() {
                    state.cancel_owned_step(Step::New(plan));
                }
                match result {
                    Ok(ready) => state.finish_saved_open(open, ready, tab_id, finish),
                    Err(failure) => state.fail_saved_creation(open, failure, finish),
                }
            }),
        );
    }

    fn finish_saved_open(
        &self,
        open: SavedOpen,
        ready: crate::tabs::host::ViewCreationReady<SavedViewLoaded>,
        tab_id: Rc<RefCell<Option<String>>>,
        finish: SavedOpenDone,
    ) {
        let SavedViewLoaded { document, .. } = ready.view;
        let cleanup = ready.cleanup;
        let view = document.target().view();
        let generation = document.target().created().clone();
        let active = open.epoch == self.manager_epoch.get()
            && !self.creation_shutdown.get()
            && !open.cancel.is_cancelled()
            && !open.pending.is_cancelled();
        let tab = tab_id.borrow().clone();
        let tab = match tab {
            Some(tab) if active => tab,
            tab => {
                let error = if tab.is_none() {
                    "The saved editor was not adopted"
                } else {
                    "The saved-open operation is no longer active"
                };
                return self.discard_saved_open(open, view, tab, cleanup, error.into(), finish);
            }
        };
        let lifetime = self.retain_adopted_editor(
            view,
            generation,
            ready.reservation.reservation.clone(),
            ready.reservation.owner.clone(),
            ready.closure,
            open.rollback.clone(),
        );
        if let Err(failure) = lifetime {
            let error = failure.message().to_owned();
            return self.discard_saved_open(open, view, Some(tab), cleanup, error, finish);
        }
        let SavedOpen {
            id,
            participants,
            cancel,
            pending,
            additional,
            effects,
            ..
        } = open;
        let owner = document.owner().clone();
        let authority = TabActivationAuthority::SavedDocument {
            document: document.clone(),
            owner: owner.clone(),
            additional: additional.clone(),
        };
        let state = self.rc();
        let activated = tab.clone();
        self.activate_loaded_authorized(
            &tab,
            authority,
            owner,
            participants.clone(),
            cancel.clone(),
            pending.clone(),
            Box::new(move |result| {
                let outcome = result
                    .and_then(|()| {
                        CommittedGeneratedPath::from_saved_document(
                            document,
                            participants,
                            cancel,
                            pending,
                            additional,
                            effects,
                        )
                    })
                    .map(|receipt| GeneratedPathOutcome::Opened(Arc::new(receipt)));
                if outcome.is_err() {
                    state.inner.borrow_mut().manager_call(|manager, host| {
                        if manager.web_contents_for_tab(&activated) == Some(view) {
                            manager.close_tab_without_prompt(host, &activated);
                        }
                    });
                }
                state.finish_creation(id);
                finish(outcome);
            }),
            None,
            None,
        );
    }

    fn fail_saved_creation(
        &self,
        open: SavedOpen,
        failure: ViewCreationFailure,
        finish: SavedOpenDone,
    ) {
        let ViewCreationFailure { error, cleanup } = failure;
        let (cleanup, proof) = match cleanup {
            ViewCreationCleanupState::Complete => (None, None),
            ViewCreationCleanupState::Acknowledged(proof) => (None, Some(proof)),
            ViewCreationCleanupState::Pending(cleanup) => (Some(cleanup), None),
        };
        let id = open.id;
        let state = self.rc();
        let reply = Rc::new(RefCell::new(Some(finish)));
        let failed = reply.clone();
        let original = error.0.clone();
        self.retain_editor_cleanup(
            cleanup,
            proof,
            open.rollback,
            false,
            Box::new(move |failure| {
                if let Some(done) = failed.borrow_mut().take() {
                    done(Err(format!("{original}; cleanup failed: {failure}")));
                }
            }),
            Box::new(move || {
                state.finish_creation(id);
                if let Some(done) = reply.borrow_mut().take() {
                    done(Err(error.0));
                }
            }),
        );
    }

    fn discard_saved_open(
        &self,
        open: SavedOpen,
        view: ViewId,
        tab: Option<String>,
        cleanup: ViewCreationCleanup,
        error: String,
        finish: SavedOpenDone,
    ) {
        let SavedOpen { id, rollback, .. } = open;
        if let Some(tab) = tab {
            self.inner.borrow_mut().manager_call(|manager, host| {
                if manager.web_contents_for_tab(&tab) == Some(view) {
                    manager.close_tab_without_prompt(host, &tab);
                }
            });
        }
        let state = self.rc();
        let reply = Rc::new(RefCell::new(Some(finish)));
        let failed = reply.clone();
        let message = error.clone();
        self.retain_editor_cleanup(
            Some(cleanup),
            None,
            rollback,
            false,
            Box::new(move |failure| {
                if let Some(done) = failed.borrow_mut().take() {
                    done(Err(format!("{message}; cleanup failed: {failure}")));
                }
            }),
            Box::new(move || {
                state.finish_creation(id);
                if let Some(done) = reply.borrow_mut().take() {
                    done(Err(error));
                }
            }),
        );
    }
}
