use super::{
    Completion, DIAGRAM_CHECKING, DIAGRAM_WAITERS, DiagramCheckGuard, Done, PendingOwnedOpen, Step,
    TabsState, document_kind, editor_request_for_path, native_open_surface,
};
use crate::tabs::{
    activation::TabActivationAuthority,
    host::{EditorSeedRollbackStore, ViewClosureBinding},
};
use main_core::shell::tab_manager::{EditorOpen, ViewCreationCleanupState};
use shell_contract::ipc::{
    NativeEditorCommand, NativeInvocationOperation, NativeInvocationSelection,
};
use std::rc::Rc;

impl TabsState {
    pub fn route_document_path_native(
        &self,
        path: String,
        operation: NativeInvocationOperation,
        done: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        let expected = NativeEditorCommand::OpenFile {
            surface: native_open_surface(&path),
            path: std::path::PathBuf::from(&path),
        };
        if !operation.is_root_native_command()
            || operation.selection() != &NativeInvocationSelection::NativeCommand(expected.clone())
        {
            return done(Err(
                "The native open command does not match its selected path".into(),
            ));
        }
        self.route_native_selected(path, operation, expected, None, done);
    }

    fn route_native_selected(
        &self,
        path: String,
        operation: NativeInvocationOperation,
        expected: NativeEditorCommand,
        diagram: Option<DiagramCheckGuard>,
        done: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        let cancel = operation.cancel().clone();
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return done(Err("Native command effects are unavailable".into()));
        };
        if let Err(error) = effects.with_native_command_preparation_effect(
            &operation,
            &expected,
            &[],
            &cancel,
            &cancel,
            &mut || Ok(()),
        ) {
            return done(Err(error));
        }
        if !std::path::Path::new(&path).exists() {
            return done(Ok(false));
        }
        let detached = {
            let mut state = self.inner.borrow_mut();
            let crate::tabs::state::Inner {
                detached, bridge, ..
            } = &mut *state;
            detached.find_detached_tab_by_path(bridge, &path)
        };
        if let Some(detached) = detached {
            let captured = self
                .services
                .views
                .renderer_generation(detached.view)
                .ok_or("The detached editor renderer is unavailable")
                .and_then(|generation| {
                    effects
                        .capture_owner(detached.view, &generation)
                        .map_err(|_| "The detached editor owner is unavailable")
                });
            let target = match captured {
                Ok(target) => target,
                Err(error) => return done(Err(error.into())),
            };
            let authority = TabActivationAuthority::NativeCommand {
                operation,
                expected,
                owner: target.clone(),
            };
            return self.hooks.reveal_captured_many(
                authority,
                target,
                Vec::new(),
                cancel.clone(),
                cancel,
                Box::new(move |result| {
                    drop(diagram);
                    done(result.map(|()| true));
                }),
            );
        }
        if self.inner.borrow().manager.is_none() {
            return done(Ok(false));
        }
        let Some(kind) = document_kind(&path) else {
            return self.hooks.notify_unsupported_file_native(
                path,
                operation,
                expected,
                Box::new(move |result| done(result.map(|()| false))),
            );
        };
        let existing = {
            let mut state = self.inner.borrow_mut();
            let crate::tabs::state::Inner {
                manager, bridge, ..
            } = &mut *state;
            manager.as_ref().and_then(|manager| {
                if kind == main_core::shell::tab_manager::TabKind::Diagram {
                    manager.find_diagram_tab_by_path(bridge, Some(&path))
                } else {
                    manager.find_tab_of_kind_by_path(kind, Some(&path))
                }
            })
        };
        if let Some(id) = existing {
            let state = self.rc();
            return self.hooks.record_recent_file_native(
                path,
                operation.clone(),
                expected.clone(),
                Box::new(move |result| match result {
                    Ok(()) => {
                        state.activate_native_existing(id, operation, expected, diagram, done)
                    }
                    Err(error) => done(Err(error)),
                }),
            );
        }
        if kind == main_core::shell::tab_manager::TabKind::Diagram && diagram.is_none() {
            let key = jscompat::node_path::host::resolve(&[&path]);
            let key = if cfg!(target_os = "linux") {
                key
            } else {
                key.to_lowercase()
            };
            if !DIAGRAM_CHECKING.with(|checking| checking.borrow_mut().insert(key.clone())) {
                let state = self.this.clone();
                DIAGRAM_WAITERS.with(|waiting| {
                    waiting
                        .borrow_mut()
                        .entry(key)
                        .or_default()
                        .push(Box::new(move || {
                            if let Some(state) = state.upgrade() {
                                state.route_native_selected(path, operation, expected, None, done);
                            } else {
                                done(Err("The tab manager is unavailable".into()));
                            }
                        }))
                });
                return;
            }
            let guard = DiagramCheckGuard::new(key);
            let state = self.this.clone();
            self.hooks.diagram_open_refusal_native(
                path.clone(),
                operation.clone(),
                expected.clone(),
                Box::new(move |result| {
                    let Some(state) = state.upgrade() else {
                        return done(Err("The tab manager is unavailable".into()));
                    };
                    match result {
                        Err(error) => done(Err(error)),
                        Ok(Some(reason)) => {
                            let detail = Some(jscompat::node_path::host::basename(&path, None));
                            state.hooks.show_app_warning_native(
                                reason,
                                detail,
                                operation,
                                expected,
                                Box::new(move |result| {
                                    drop(guard);
                                    done(result.map(|()| false));
                                }),
                            );
                        }
                        Ok(None) => state.route_native_selected(
                            path,
                            operation,
                            expected,
                            Some(guard),
                            done,
                        ),
                    }
                }),
            );
            return;
        }
        let Some(request) = editor_request_for_path(kind, &path) else {
            return done(Ok(false));
        };
        let state = self.rc();
        self.hooks.record_recent_file_native(
            path,
            operation.clone(),
            expected.clone(),
            Box::new(move |result| {
                if let Err(error) = result {
                    return done(Err(error));
                }
                state.open_native_editor(
                    request,
                    operation,
                    expected,
                    Box::new(move |result| {
                        drop(diagram);
                        done(result.map(|_| true));
                    }),
                );
            }),
        );
    }

    pub(super) fn activate_native_existing(
        &self,
        id: String,
        operation: NativeInvocationOperation,
        expected: NativeEditorCommand,
        diagram: Option<DiagramCheckGuard>,
        done: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return done(Err("Native command effects are unavailable".into()));
        };
        let cancel = operation.cancel().clone();
        let Some(shell) = self.shell_window() else {
            return done(Err("The shell window is unavailable".into()));
        };
        let captured = self
            .services
            .views
            .renderer_generation(shell.view)
            .ok_or("The shell renderer is unavailable")
            .and_then(|generation| {
                effects
                    .capture_owner(shell.view, &generation)
                    .map_err(|_| "The shell owner is unavailable")
            });
        let shell_owner = match captured {
            Ok(owner) => owner,
            Err(error) => return done(Err(error.into())),
        };
        let mut prepared = None;
        let admitted = effects.with_native_command_preparation_effect(
            &operation,
            &expected,
            std::slice::from_ref(&shell_owner),
            &cancel,
            &cancel,
            &mut || {
                let mut inner = self.inner.borrow_mut();
                let manager = inner
                    .manager
                    .as_mut()
                    .ok_or("The shell window is unavailable")?;
                let view = manager.web_contents_for_tab(&id);
                prepared = Some((view, manager.prepare_tab_load(&id)?));
                Ok(())
            },
        );
        if let Err(error) = admitted {
            return done(Err(error));
        }
        let Some((view, plan)) = prepared else {
            return done(Err("The native tab activation was not admitted".into()));
        };
        if let Some(plan) = plan {
            let operation_id = match self.owned_id() {
                Ok(value) => value,
                Err(error) => {
                    self.inner
                        .borrow_mut()
                        .manager_call(|manager, _| manager.cancel_tab_load(plan));
                    return done(Err(error));
                }
            };
            let opener = *plan.opener();
            let context = plan.context().clone();
            let generation = match shell_owner.context.renderer().cloned() {
                Some(generation) => generation,
                None => {
                    self.inner
                        .borrow_mut()
                        .manager_call(|manager, _| manager.cancel_tab_load(plan));
                    return done(Err("The shell owner has no renderer".into()));
                }
            };
            if let Err(error) = self.begin_creation(operation_id) {
                self.inner
                    .borrow_mut()
                    .manager_call(|manager, _| manager.cancel_tab_load(plan));
                return done(Err(error));
            }
            let completed = self.rc();
            let completion: Done = Box::new(move |result| {
                completed.finish_creation(operation_id);
                done(result.map(|_| true));
            });
            self.pending_owned.borrow_mut().insert(
                operation_id,
                PendingOwnedOpen {
                    epoch: self.manager_epoch.get(),
                    cancel: cancel.clone(),
                    generation,
                    step: Step::NativeHydrate {
                        plan,
                        command: operation.clone(),
                        shell: shell_owner,
                        prepared: None,
                    },
                    done: Completion::Plain(completion),
                    generated_cleanup: None,
                    diagram,
                },
            );
            let state = self.rc();
            self.hooks.open_opener_native_command(
                &opener,
                &context,
                operation,
                cancel.clone(),
                cancel,
                Box::new(move |result| {
                    let pending = state.pending_owned.borrow().contains_key(&operation_id);
                    if pending {
                        state.finish_owned_open(
                            operation_id,
                            result.map(super::NativeOpened::Hydrated),
                        );
                    } else if let Ok(loaded) = result {
                        state.discard_hydration(
                            loaded.attempt,
                            loaded.discard,
                            "The native tab activation was cancelled".into(),
                            Box::new(|result| {
                                if let Err(error) = result {
                                    eprintln!("[recovery] {error}");
                                }
                            }),
                            None,
                        );
                    }
                }),
            );
            return;
        }
        let Some(view) = view else {
            return done(Err("The target tab has no editor view".into()));
        };
        let target = match self
            .services
            .views
            .renderer_generation(view)
            .ok_or("The target editor renderer is unavailable")
            .and_then(|generation| {
                effects
                    .capture_owner(view, &generation)
                    .map_err(|_| "The target editor owner is unavailable")
            }) {
            Ok(target) => target,
            Err(error) => return done(Err(error.into())),
        };
        let authority = TabActivationAuthority::NativeCommand {
            operation,
            expected,
            owner: target.clone(),
        };
        self.activate_loaded_authorized(
            &id,
            authority,
            target,
            vec![shell_owner],
            cancel.clone(),
            cancel,
            Box::new(move |result| {
                drop(diagram);
                done(result.map(|()| true));
            }),
            None,
            None,
        );
    }
    pub fn open_mcp_blank_docs_native(
        &self,
        operation: NativeInvocationOperation,
        done: Box<dyn FnOnce(Result<shell_contract::ViewId, String>)>,
    ) {
        let expected = match operation.selection() {
            NativeInvocationSelection::NativeCommand(
                command @ NativeEditorCommand::NewDocument {
                    surface: shell_contract::Surface::Docs,
                    ..
                },
            ) => command.clone(),
            _ => {
                return done(Err(
                    "The selected native command is not a new Docs document".into(),
                ));
            }
        };
        let request = EditorOpen::Docs {
            path: None,
            options: Some(main_core::shell::tab_manager::OpenDocsOptions {
                new_blank: true,
                ai_content: None,
            }),
        };
        self.open_native_editor(request, operation, expected, {
            let state = self.rc();
            Box::new(move |result| {
                done(result.map(|(_, view, _)| {
                    state
                        .inner
                        .borrow_mut()
                        .mcp_blank
                        .insert(view, crate::tabs::state::McpBlank::Docs);
                    view
                }))
            })
        });
    }

    pub fn open_mcp_blank_sheets_native(
        &self,
        operation: NativeInvocationOperation,
        done: Box<dyn FnOnce(Result<shell_contract::ViewId, String>)>,
    ) {
        let expected = match operation.selection() {
            NativeInvocationSelection::NativeCommand(
                command @ NativeEditorCommand::NewDocument {
                    surface: shell_contract::Surface::Sheets,
                    ..
                },
            ) => command.clone(),
            _ => {
                return done(Err(
                    "The selected native command is not a new Sheets document".into(),
                ));
            }
        };
        let request = EditorOpen::Sheets {
            path: None,
            new_blank: true,
        };
        self.open_native_editor(request, operation, expected, {
            let state = self.rc();
            Box::new(move |result| {
                done(result.map(|(_, view, _)| {
                    state
                        .inner
                        .borrow_mut()
                        .mcp_blank
                        .insert(view, crate::tabs::state::McpBlank::Sheets);
                    view
                }))
            })
        });
    }

    /// A blank document of `kind` through the native `NewDocument` command: the tab opens
    /// through the owned creation path and the folder the click remembered binds to it. A
    /// failure is shown, as a failed create-tab click must not look like a no-op.
    pub(in crate::tabs) fn open_new_native(&self, kind: crate::tabs::open::NewKind) {
        use crate::tabs::open::NewKind;
        use shell_contract::Surface;
        let (surface, request) = match kind {
            NewKind::Doc => (
                Surface::Docs,
                EditorOpen::Docs {
                    path: None,
                    options: Some(main_core::shell::tab_manager::OpenDocsOptions {
                        new_blank: true,
                        ai_content: None,
                    }),
                },
            ),
            NewKind::Sheet => (
                Surface::Sheets,
                EditorOpen::Sheets {
                    path: None,
                    new_blank: true,
                },
            ),
            NewKind::Slide => (Surface::Slides, EditorOpen::Slides { path: None }),
            NewKind::Markdown => (Surface::Markdown, EditorOpen::Markdown { path: None }),
            NewKind::Html => (Surface::Html, EditorOpen::Html { path: None }),
            NewKind::Mindmap => (Surface::Mindmap, EditorOpen::Mindmap { path: None }),
            NewKind::Diagram => (Surface::Diagram, EditorOpen::Diagram { path: None }),
            NewKind::Hwp => (
                Surface::Hwp,
                EditorOpen::Hwp {
                    path: None,
                    copy: None,
                },
            ),
            NewKind::Pdf => {
                return self.surface_new_tab_error("A blank PDF is a file, not a native document");
            }
        };
        let cancel = shell_contract::runtime::CancelToken::default();
        let expected = NativeEditorCommand::NewDocument {
            surface,
            directory: None,
        };
        let operation = match self
            .hooks
            .issue_native_command(expected.clone(), cancel.clone())
        {
            Ok(operation) => operation,
            Err(error) => return self.surface_new_tab_error(&error),
        };
        let state = self.rc();
        self.open_native_editor(
            request,
            operation,
            expected,
            Box::new(move |result| {
                drop(cancel);
                match result {
                    Ok((id, _, _)) => state.bind_pending_dir(kind.key(), Some(&id)),
                    Err(error) => state.surface_new_tab_error(&error),
                }
            }),
        );
    }

    /// The imported-PDF Hangul copy (`NativeEditorCommand::HwpCopy`) opened as a new untitled tab.
    pub fn open_hwp_copy_native(
        &self,
        path: String,
        title: String,
        operation: NativeInvocationOperation,
        done: Box<dyn FnOnce(Result<shell_contract::ViewId, String>)>,
    ) {
        let expected = NativeEditorCommand::HwpCopy {
            path: std::path::PathBuf::from(&path),
            title: title.clone(),
        };
        let request = EditorOpen::Hwp {
            path: Some(path),
            copy: Some(main_core::shell::tab_manager::HwpCopy {
                title,
                imported_pdf: true,
            }),
        };
        self.open_native_editor(
            request,
            operation,
            expected,
            Box::new(move |result| done(result.map(|(_, view, _)| view))),
        );
    }

    pub(super) fn open_native_editor(
        &self,
        request: EditorOpen,
        operation: NativeInvocationOperation,
        expected: NativeEditorCommand,
        done: Done,
    ) {
        if !operation.is_root_native_command()
            || operation.selection() != &NativeInvocationSelection::NativeCommand(expected.clone())
        {
            return done(Err(
                "The native editor command does not match its selection".into(),
            ));
        }
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return done(Err("Native command effects are unavailable".into()));
        };
        let cancel = operation.cancel().clone();
        let mut request = Some(request);
        let mut plan = None;
        let admitted = effects.with_native_command_preparation_effect(
            &operation,
            &expected,
            &[],
            &cancel,
            &cancel,
            &mut || {
                let mut inner = self.inner.borrow_mut();
                let manager = inner
                    .manager
                    .as_mut()
                    .ok_or("The shell window is unavailable")?;
                plan = Some(
                    manager.prepare_editor_open_pure(
                        request
                            .take()
                            .ok_or("The native request was already consumed")?,
                    ),
                );
                Ok(())
            },
        );
        if let Err(error) = admitted {
            return done(Err(error));
        }
        let Some(mut plan) = plan else {
            return done(Err("The native editor reservation was not admitted".into()));
        };
        let id = match self.owned_id() {
            Ok(id) => id,
            Err(error) => {
                self.cancel_owned_step(Step::New(plan));
                return done(Err(error));
            }
        };
        let epoch = self.manager_epoch.get();
        let new_blank = plan.requires_sheets_new_blank();
        let (seed, docs_path, workbook, _) = plan.initialization_inputs();
        let rollback: EditorSeedRollbackStore = Rc::default();
        if let Some(view) = plan.reserved_view().copied() {
            let Some(retained) = plan.reserved_binding().cloned() else {
                self.cancel_owned_step(Step::New(plan));
                return done(Err("The spare binding is unavailable".into()));
            };
            let target = match &retained.browser.registration {
                crate::tabs::host::NativeBrowserRegistration::Registered(owner)
                    if owner.view == view =>
                {
                    owner.clone()
                }
                _ => {
                    self.cancel_owned_step(Step::New(plan));
                    return done(Err("The spare is not registered".into()));
                }
            };
            let Some(generation) = target.context.renderer().cloned() else {
                self.cancel_owned_step(Step::New(plan));
                return done(Err("The spare renderer is unavailable".into()));
            };
            let staged_seed = match self.hooks.prepare_spare_editor_seed(
                view, seed, docs_path, workbook, new_blank, &target, &retained, &rollback,
            ) {
                Ok(staged) => staged,
                Err(error) => {
                    self.cancel_owned_step(Step::New(plan));
                    return done(Err(error.0));
                }
            };
            let binding = ViewClosureBinding {
                reservation: retained.browser.reservation.clone(),
                owner: retained.browser.owner.clone(),
                producer: retained.browser.producer.clone(),
                closure: retained.browser.closure.clone(),
                registration: crate::tabs::host::NativeBrowserRegistration::Registered(
                    target.clone(),
                ),
            };
            self.pending_owned.borrow_mut().insert(
                id,
                PendingOwnedOpen {
                    epoch,
                    cancel: cancel.clone(),
                    generation,
                    step: Step::New(plan),
                    done: Completion::Plain(done),
                    generated_cleanup: None,
                    diagram: None,
                },
            );
            let authority = TabActivationAuthority::NativeCommand {
                operation,
                expected,
                owner: target.clone(),
            };
            return self.finish_new_editor(
                id,
                authority,
                target,
                None,
                binding,
                rollback,
                false,
                Some(staged_seed),
            );
        }
        let Some(spec) = plan.creation_spec() else {
            self.cancel_owned_step(Step::New(plan));
            return done(Err(
                "The native editor creation specification is unavailable".into(),
            ));
        };
        let hooks = self.hooks.clone();
        let selected = operation.clone();
        let command = expected.clone();
        let receipts = rollback.clone();
        let cancellation = cancel.clone();
        let initialize = Box::new(
            move |view,
                  created: &shell_contract::runtime::RendererGeneration,
                  reservation: &shell_contract::ipc::NativeClosureReservation,
                  browser_owner: &shell_contract::ipc::NativeBrowserOwner| {
                hooks.commit_native_editor_seed(
                    &selected,
                    &command,
                    view,
                    seed,
                    docs_path,
                    workbook,
                    None,
                    &receipts,
                    created,
                    reservation,
                    browser_owner,
                    &cancellation,
                    &cancellation,
                )
            },
        );
        let (generation, construct) = match self.hooks.prepare_view_native_command(
            &spec,
            operation.clone(),
            rollback.clone(),
            initialize,
            Vec::new(),
            cancel.clone(),
            cancel.clone(),
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.cancel_owned_step(Step::New(plan));
                return done(Err(error.0));
            }
        };
        if let Err(error) = self.begin_creation(id) {
            self.cancel_owned_step(Step::New(plan));
            return done(Err(error));
        }
        self.pending_owned.borrow_mut().insert(
            id,
            PendingOwnedOpen {
                epoch,
                cancel,
                generation,
                step: Step::New(plan),
                done: Completion::Plain(done),
                generated_cleanup: None,
                diagram: None,
            },
        );
        let state = self.rc();
        construct(Box::new(move |result| match result {
            Ok(ready) => {
                let target = ready.view.owner().clone();
                let authority = TabActivationAuthority::NativeCommandTarget {
                    document: ready.view,
                    expected,
                    owner: target.clone(),
                };
                let binding = ViewClosureBinding {
                    reservation: ready.reservation.reservation,
                    owner: ready.reservation.owner,
                    producer: ready.reservation.producer,
                    closure: ready.closure,
                    registration: crate::tabs::host::NativeBrowserRegistration::Registered(
                        target.clone(),
                    ),
                };
                state.finish_new_editor(
                    id,
                    authority,
                    target,
                    Some(ready.cleanup),
                    binding,
                    rollback,
                    true,
                    None,
                );
            }
            Err(failure) => {
                let (cleanup, proof) = match failure.cleanup {
                    ViewCreationCleanupState::Complete => (None, None),
                    ViewCreationCleanupState::Acknowledged(proof) => (None, Some(proof)),
                    ViewCreationCleanupState::Pending(cleanup) => (Some(cleanup), None),
                };
                let completed = state.clone();
                let failed = state.clone();
                let original = failure.error.0.clone();
                state.retain_editor_cleanup(
                    cleanup,
                    proof,
                    rollback,
                    true,
                    Box::new(move |error| {
                        failed.notify_pending_creation_error(
                            id,
                            format!("{original}; cleanup failed: {error}"),
                        );
                    }),
                    Box::new(move || {
                        completed.finish_new_editor_failure(id, failure.error.0);
                    }),
                );
                state.finish_creation(id);
            }
        }));
    }
}
