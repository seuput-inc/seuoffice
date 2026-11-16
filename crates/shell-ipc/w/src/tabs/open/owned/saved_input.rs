use super::generated_cleanup::GeneratedCleanupCoordinator;
use super::*;
use crate::tabs::activation::TabActivationAuthority;
use crate::tabs::host::{
    CapturedSavedShell, EditorSeedRollbackStore, GeneratedPrecloseBarrier,
    GeneratedPrecloseInstaller, PreparedSavedView, RetainedSpareBinding, SavedViewLoaded,
    ShellWindow, ViewCreationCleanup, ViewCreationFailure,
};
use main_core::shell::tab_manager::ViewCreationCleanupState;
use shell_contract::hosts::{
    CommittedGeneratedPath, GeneratedPathOutcome, OwnedEffectsHost, PdfReloadAccepted,
    PdfReloadHost,
};
use shell_contract::ipc::{NativeSavedInput, NativeSavedPath};
use std::rc::Rc;
use std::sync::Arc;

pub(super) type SavedDone = Box<dyn FnOnce(Result<GeneratedPathOutcome, String>)>;
type SavedOpenDone = Box<dyn FnOnce(Result<GeneratedPathOutcome, String>) + Send>;

/// The native close of a saved view runs the coordinator's preclose first, so
/// the manager record and the captured survivors are prepared before the
/// browser is claimed and closed.
fn gated_cleanup(
    barrier: GeneratedPrecloseBarrier,
    cleanup: ViewCreationCleanup,
) -> ViewCreationCleanup {
    Arc::new(
        move |done: Box<
            dyn FnOnce(Result<shell_contract::ipc::NativeBrowserRetirement, String>) + Send,
        >| {
            let cleanup = cleanup.clone();
            barrier(Box::new(move |result| match result {
                Ok(()) => cleanup(done),
                Err(error) => done(Err(error)),
            }));
        },
    )
}

/// What the fresh-editor continuation keeps from the original request.
struct SavedOpen {
    id: u64,
    epoch: u64,
    source: NativeRequestOwner,
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
    /// kind that cannot adopt a fresh saved view (Sheets while its spare view
    /// is reserved: `CapturedSavedShell` has no spare-transfer preparation)
    /// reports `NotOpened`, so the caller reveals the file instead of granting
    /// new editor authority.
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
                manager
                    .find_tab_of_kind_by_path(kind, Some(&path_text))
                    .map(|id| {
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
                source: shell_owner,
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
            return finish(Ok(GeneratedPathOutcome::NotOpened));
        }
        let (cleanup_shell, tabs, cleanup_participants) =
            match self.capture_generated_cleanup_context(&open.source, &open.participants) {
                Ok(captured) => captured,
                Err(error) => {
                    self.cancel_owned_step(Step::New(plan));
                    return finish(Err(error));
                }
            };
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
                return self.abandon_saved_open(plan, open, error.0, finish);
            }
        };
        let PreparedSavedView {
            target,
            binding,
            start,
        } = prepared;
        let ready_binding = binding.clone();
        let retained = RetainedSpareBinding::new(binding, open.rollback.clone(), None);
        let adoption =
            match self.prepare_saved_manager_adoption(plan, &target, retained, open.epoch) {
                Ok(adoption) => adoption,
                Err((plan, error)) => {
                    drop(start);
                    return self.abandon_saved_open(plan, open, error, finish);
                }
            };
        let PreparedSavedManagerAdoption {
            adopt,
            register,
            tab_id,
            recover,
            preview,
        } = adoption;
        let view = target.view();
        let created = target.created().clone();
        let authority = GeneratedCleanupAuthority {
            operation: id,
            source: open.source.clone(),
            shell: cleanup_shell.1,
            shell_window: cleanup_shell.0,
            manager_epoch: open.epoch,
            cleanup: self.hooks.cleanup_lifetime(),
            participants: cleanup_participants,
            survivors: tabs,
            preview,
            reservation: ready_binding.reservation.clone(),
            browser: ready_binding.owner.clone(),
            retirement: Rc::new(RefCell::new(None)),
            cancel: open.cancel.clone(),
            pending: open.pending.clone(),
            rollback: open.rollback.clone(),
        };
        // The host installs no preclose barrier for a saved view; its close runs
        // through the closure watch and the coordinator's rollback below.
        let barrier_slot: Rc<RefCell<Option<GeneratedPrecloseBarrier>>> = Rc::default();
        let installed = barrier_slot.clone();
        let installer: GeneratedPrecloseInstaller = Box::new(move |barrier, _failure| {
            *installed.borrow_mut() = Some(barrier);
            Ok(Arc::new(|| {}))
        });
        let (coordinator, coordinator_cleanup) = match GeneratedCleanupCoordinator::prepare(
            self.rc(),
            authority,
            id,
            view,
            ready_binding,
            open.participants.clone(),
            None,
            tab_id.clone(),
            false,
            installer,
        ) {
            Ok(prepared) => prepared,
            Err((_, error)) => {
                drop(start);
                drop(adopt);
                drop(register);
                let plan = recover();
                return match plan {
                    Some(plan) => self.abandon_saved_open(plan, open, error, finish),
                    None => {
                        self.finish_creation(id);
                        finish(Err(error))
                    }
                };
            }
        };
        let Some(barrier) = barrier_slot.borrow_mut().take() else {
            drop(start);
            drop(adopt);
            drop(register);
            let plan = recover();
            let error = "The saved close barrier was not installed".to_owned();
            return match plan {
                Some(plan) => self.abandon_saved_open(plan, open, error, finish),
                None => {
                    self.finish_creation(id);
                    finish(Err(error))
                }
            };
        };
        let cleanup = gated_cleanup(barrier, coordinator_cleanup);
        self.pending_owned.borrow_mut().insert(
            id,
            PendingOwnedOpen {
                epoch: open.epoch,
                cancel: open.cancel.clone(),
                generation: created,
                step: Step::GeneratedFailed { plan: None, view },
                done: Completion::Saved(finish),
                generated_cleanup: Some(coordinator.clone()),
                diagram: None,
            },
        );
        let state = self.rc();
        start(
            adopt,
            register,
            Box::new(move |result| {
                if let Some(plan) = recover() {
                    let plan = {
                        let mut pending = state.pending_owned.borrow_mut();
                        match pending.get_mut(&id) {
                            Some(PendingOwnedOpen {
                                step:
                                    Step::GeneratedFailed {
                                        plan: slot @ None, ..
                                    },
                                ..
                            }) => {
                                *slot = Some(plan);
                                None
                            }
                            _ => Some(plan),
                        }
                    };
                    if let Some(plan) = plan {
                        state.cancel_owned_step(Step::New(plan));
                    }
                }
                match result {
                    Ok(ready) => {
                        let pending = state.pending_owned.borrow_mut().remove(&id);
                        match pending {
                            Some(pending) => state.finish_saved_creation(
                                open,
                                pending,
                                ready,
                                coordinator,
                                tab_id,
                                cleanup,
                            ),
                            None => {
                                let completed = state.clone();
                                state.retain_creation_cleanup_with_rollback(
                                    Some(cleanup),
                                    None,
                                    Some(coordinator.rollback_action()),
                                    Box::new(|error| {
                                        eprintln!("[shell] orphaned saved cleanup failed: {error}")
                                    }),
                                    Box::new(move || {
                                        eprintln!("[shell] discarded orphaned saved editor {id}");
                                        completed.finish_creation(id);
                                    }),
                                );
                            }
                        }
                    }
                    Err(failure) => {
                        let ViewCreationFailure {
                            error,
                            cleanup: failure_cleanup,
                        } = failure;
                        let (cleanup, proof) = match failure_cleanup {
                            ViewCreationCleanupState::Complete => (None, None),
                            ViewCreationCleanupState::Acknowledged(proof) => (None, Some(proof)),
                            ViewCreationCleanupState::Pending(_) => (Some(cleanup), None),
                        };
                        let completed = state.clone();
                        let failed = state.clone();
                        let original = error.0.clone();
                        state.retain_creation_cleanup_with_rollback(
                            cleanup,
                            proof,
                            Some(coordinator.rollback_action()),
                            Box::new(move |failure| {
                                failed.notify_pending_creation_error(
                                    id,
                                    format!("{original}; cleanup failed: {failure}"),
                                );
                            }),
                            Box::new(move || {
                                completed.settle_saved_open(id, Err(error.0));
                                completed.finish_creation(id);
                            }),
                        );
                    }
                }
            }),
        );
    }

    /// Nothing was adopted: hand the plan back and release the creation slot
    /// once the retained rollback has drained.
    fn abandon_saved_open(
        &self,
        plan: PreparedTabOpen<ViewId, RetainedSpareBinding>,
        open: SavedOpen,
        error: String,
        finish: SavedOpenDone,
    ) {
        let id = open.id;
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
    }

    fn settle_saved_open(&self, id: u64, result: Result<GeneratedPathOutcome, String>) {
        let pending = self.pending_owned.borrow_mut().remove(&id);
        self.owned_abort.borrow_mut().remove(&id);
        if let Some(PendingOwnedOpen {
            done: Completion::Saved(done),
            ..
        }) = pending
        {
            done(result);
        }
    }

    /// The editor loaded. As for a generated editor, the caller hears only a
    /// settled activation; a committed-cleanup-pending activation reports its
    /// error at once while the retained retry keeps the tab and its cleanup.
    fn finish_saved_creation(
        &self,
        open: SavedOpen,
        pending: PendingOwnedOpen,
        ready: crate::tabs::host::ViewCreationReady<SavedViewLoaded>,
        coordinator: Rc<GeneratedCleanupCoordinator>,
        tab_id: Rc<RefCell<Option<String>>>,
        cleanup: ViewCreationCleanup,
    ) {
        let SavedOpen {
            id,
            epoch,
            participants,
            cancel,
            pending: request_pending,
            additional,
            effects,
            ..
        } = open;
        let PendingOwnedOpen { step, done, .. } = pending;
        self.cancel_owned_step(step);
        let (done, replied) = match done {
            Completion::Saved(done) => (Some(done), false),
            _ => (None, true),
        };
        let aborted = self
            .owned_abort
            .borrow_mut()
            .remove(&id)
            .or_else(|| replied.then(|| "The saved open already replied".to_owned()));
        let SavedViewLoaded { document, .. } = ready.view;
        let view = document.target().view();
        let generation = document.target().created().clone();
        let tab = tab_id.borrow().clone();
        let Some(tab) = tab.filter(|_| coordinator.matches_binding(&ready.reservation)) else {
            return self.fail_generated_completion_parts(
                id,
                cleanup,
                coordinator,
                None,
                "The saved editor completion no longer matches its adoption".into(),
                done,
            );
        };
        let inactive = aborted.or_else(|| {
            (epoch != self.manager_epoch.get()
                || self.creation_shutdown.get()
                || cancel.is_cancelled()
                || request_pending.is_cancelled())
            .then(|| "The saved-open operation is no longer active".to_owned())
        });
        if let Some(error) = inactive {
            return self.fail_generated_completion_parts(
                id,
                cleanup,
                coordinator,
                None,
                error,
                done,
            );
        }
        let owner = document.owner().clone();
        let authority = TabActivationAuthority::SavedDocument {
            document: document.clone(),
            owner: owner.clone(),
            additional: additional.clone(),
        };
        let reply = Rc::new(RefCell::new(done));
        let failed_reply = reply.clone();
        let on_pending = Rc::new(move |error: &str| {
            let done = failed_reply.borrow_mut().take();
            if let Some(done) = done {
                done(Err(error.to_owned()));
            }
        });
        let state = self.rc();
        let activation_participants = participants.clone();
        let activation_cancel = cancel.clone();
        let activation_pending = request_pending.clone();
        let settled = Box::new(move |result: Result<(), String>| {
            let done = reply.borrow_mut().take();
            let result = result.and_then(|()| {
                if done.is_none() {
                    return Err("The saved activation already reported a failure".into());
                }
                if epoch != state.manager_epoch.get() || !generation.is_active() {
                    return Err("The saved editor changed before activation settled".into());
                }
                CommittedGeneratedPath::from_saved_document(
                    document,
                    participants,
                    cancel,
                    request_pending,
                    additional,
                    effects,
                )
                .map(|receipt| GeneratedPathOutcome::Opened(Arc::new(receipt)))
            });
            match result {
                Err(error) => state.fail_generated_completion_parts(
                    id,
                    cleanup,
                    coordinator,
                    None,
                    error,
                    done,
                ),
                Ok(outcome) => {
                    let store = match coordinator.rollback_store() {
                        Ok(store) => store,
                        Err(error) => {
                            return state.fail_generated_completion_parts(
                                id,
                                cleanup,
                                coordinator,
                                None,
                                error,
                                done,
                            );
                        }
                    };
                    match state.retain_adopted_editor(
                        view,
                        generation,
                        ready.reservation.reservation,
                        ready.reservation.owner,
                        ready.closure,
                        store,
                    ) {
                        Ok(handle) => {
                            if handle.is_unclosed() != Ok(true) {
                                let error =
                                    "The saved editor retired during lifetime handoff".to_owned();
                                return state.fail_generated_completion_parts(
                                    id,
                                    cleanup,
                                    coordinator,
                                    Some(handle.into_failure(error.clone())),
                                    error,
                                    done,
                                );
                            }
                            coordinator.release();
                            state.finish_creation(id);
                            if let Some(done) = done {
                                done(Ok(outcome));
                            }
                        }
                        Err(failure) => {
                            let error = failure.message().to_owned();
                            state.fail_generated_completion_parts(
                                id,
                                cleanup,
                                coordinator,
                                Some(failure),
                                error,
                                done,
                            );
                        }
                    }
                }
            }
        });
        self.activate_loaded_authorized(
            &tab,
            authority,
            owner,
            activation_participants,
            activation_cancel,
            activation_pending,
            Box::new(|_| {}),
            Some(on_pending),
            Some(settled),
        );
    }
}
