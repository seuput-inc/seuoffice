use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};

use crate::native::{DocumentContext, NativeEvent, NativeEventKind};
use main_core::shell::tab_manager::TabError;
use shell_contract::{
    ViewId,
    ipc::{
        NativeEditorCommand, NativeInvocationDocument, NativeInvocationOperation,
        NativeInvocationTarget, NativeRequestOwner,
    },
    runtime::{CancelSubscription, CancelToken, RendererGeneration, Thread},
};
use shell_ipc_w::tabs::host::{
    ViewCreationCleanupState, ViewCreationFailure, ViewCreationReady, ViewCreationResult,
};

use super::{HostContext, accepted};
use crate::ipc::native_owner::{self, NativeOwnerAuthority};

pub(super) type Done = Box<dyn FnOnce(ViewCreationResult<NativeInvocationDocument>)>;
pub(super) type FrameCallback = Arc<dyn Fn(ViewId, &NativeEvent) + Send + Sync>;
pub(super) type Create = Box<
    dyn FnOnce(FrameCallback) -> Result<crate::host::window::CreatedChild, accepted::InstallError>
        + Send,
>;
struct Pending {
    operation: NativeInvocationOperation,
    startup: Option<(NativeInvocationOperation, NativeRequestOwner)>,
    command: NativeEditorCommand,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    core: Arc<dyn Thread>,
    ready: Option<ViewCreationReady<ViewId>>,
    document: Option<NativeInvocationDocument>,
    failure: Option<String>,
    cleaning: bool,
    done: Done,
    _losses: Vec<CancelSubscription>,
}

thread_local! { static PENDING: RefCell<HashMap<u64, Pending>> = RefCell::new(HashMap::new()); }
static NEXT: AtomicU64 = AtomicU64::new(1);

pub(super) struct Unstarted(pub Option<(HostContext, ViewId, RendererGeneration)>);
impl Drop for Unstarted {
    fn drop(&mut self) {
        if let Some((cx, view, created)) = self.0.take() {
            cx.views.remove_view_if_generation(view, &created);
        }
    }
}

pub(super) fn matches_spec(
    spec: &main_core::shell::tab_manager::ViewSpec<'_>,
    command: &NativeEditorCommand,
) -> bool {
    use main_core::shell::tab_manager::{HwpUntitledCopy, ViewSpec};
    use shell_contract::Surface;
    let (surface, path) = match spec {
        ViewSpec::Docs { path } => (Surface::Docs, *path),
        ViewSpec::Sheets { opening_workbook } => {
            return match command {
                NativeEditorCommand::OpenFile {
                    surface: Surface::Sheets,
                    ..
                } => *opening_workbook == Some(true),
                NativeEditorCommand::NewDocument {
                    surface: Surface::Sheets,
                    ..
                } => *opening_workbook != Some(true),
                _ => false,
            };
        }
        ViewSpec::Slides { path } => (Surface::Slides, *path),
        ViewSpec::Pdf { path } => (Surface::Pdf, Some(*path)),
        ViewSpec::Markdown { path } => (Surface::Markdown, *path),
        ViewSpec::Html { path } => (Surface::Html, *path),
        ViewSpec::Mindmap { path } => (Surface::Mindmap, *path),
        ViewSpec::Epub { path } => (Surface::Epub, Some(*path)),
        ViewSpec::Diagram { path } => (Surface::Diagram, *path),
        ViewSpec::Hwp {
            path,
            untitled_copy: HwpUntitledCopy::Flag(false),
        } => (Surface::Hwp, *path),
        ViewSpec::Hwp {
            path,
            untitled_copy: HwpUntitledCopy::BaseName(title),
        } => {
            return matches!(command,
            NativeEditorCommand::HwpCopy { path: selected, title: expected }
                if title == expected && path.is_some_and(|path| std::path::Path::new(path) == selected));
        }
        ViewSpec::Hwp { .. } | ViewSpec::HtmlPresent { .. } => return false,
    };
    match command {
        NativeEditorCommand::OpenFile {
            surface: expected,
            path: selected,
        } => {
            *expected == surface && path.is_some_and(|path| std::path::Path::new(path) == selected)
        }
        NativeEditorCommand::NewDocument {
            surface: expected, ..
        } => *expected == surface && path.is_none_or(str::is_empty),
        NativeEditorCommand::SaveDocument { .. }
        | NativeEditorCommand::HwpCopy { .. }
        | NativeEditorCommand::OpenHome
        | NativeEditorCommand::OpenHelp { .. }
        | NativeEditorCommand::ActivateTab { .. }
        | NativeEditorCommand::HydrateTab { .. } => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn start(
    cx: HostContext,
    core: Arc<dyn Thread>,
    target: NativeInvocationTarget,
    command: NativeEditorCommand,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    startup: Option<(NativeInvocationOperation, NativeRequestOwner)>,
    create: Create,
    done: Done,
) {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let operation = target.operation().clone();
    let mut tokens = vec![
        operation.loss_token(),
        operation.cancel().clone(),
        operation.shutdown().clone(),
        target.created().loss_token(),
        cancel.clone(),
        pending.clone(),
    ];
    if let Some((root, shell)) = &startup {
        let Some(shell_loss) = shell.loss_token() else {
            cx.views
                .remove_view_if_generation(target.view(), target.created());
            return done(Err(ViewCreationFailure {
                error: TabError("The startup Shell has no lifetime".into()),
                cleanup: ViewCreationCleanupState::Complete,
            }));
        };
        tokens.extend([
            root.loss_token(),
            root.cancel().clone(),
            root.shutdown().clone(),
            shell_loss,
        ]);
    }
    for owner in &participants {
        let Some(loss) = owner.loss_token() else {
            cx.views
                .remove_view_if_generation(target.view(), target.created());
            return done(Err(ViewCreationFailure {
                error: TabError("A native command participant has no lifetime".into()),
                cleanup: ViewCreationCleanupState::Complete,
            }));
        };
        tokens.push(loss);
    }
    let losses = tokens
        .into_iter()
        .map(|token| {
            let core = core.clone();
            token.on_cancel(move || {
                core.post(Box::new(move || {
                    fail(id, "The native command was cancelled".into())
                }))
            })
        })
        .collect();
    PENDING.with(|entries| {
        entries.borrow_mut().insert(
            id,
            Pending {
                operation: operation.clone(),
                startup: startup.clone(),
                command: command.clone(),
                participants: participants.clone(),
                cancel: cancel.clone(),
                pending: pending.clone(),
                core: core.clone(),
                ready: None,
                document: None,
                failure: None,
                cleaning: false,
                done,
                _losses: losses,
            },
        );
    });
    let views = cx.views.clone();
    let frame_target = target.clone();
    let frame_core = core.clone();
    let first = Arc::new(OnceLock::<DocumentContext>::new());
    let registered_owner = Arc::new(OnceLock::<NativeRequestOwner>::new());
    let frame_startup = startup.clone();
    let frame_command = command.clone();
    let frame_participants = participants.clone();
    let frame_cancel = cancel.clone();
    let frame_pending = pending.clone();
    let frame: FrameCallback = Arc::new(move |view, event| {
        if view != frame_target.view() {
            return;
        }
        match &event.kind {
            NativeEventKind::DocumentRegistered { context: origin }
                if origin.browser().is_same_browser(&event.browser) =>
            {
                if let Some(old) = first.get() {
                    if old != origin {
                        frame_core.post(Box::new(move || {
                            fail(id, "The native command document was replaced".into())
                        }));
                    }
                    return;
                }
                let result = (|| {
                    let owner = native_owner::capture_owner(&views, view, frame_target.created())?;
                    if owner
                        .context
                        .authority_ref::<NativeOwnerAuthority>()
                        .is_none_or(|proof| &proof.origin != origin)
                    {
                        return Err(
                            "The native command registration does not match its browser".to_owned()
                        );
                    }
                    if let Some((root, shell)) = &frame_startup {
                        native_owner::register_startup_command_target(
                            root,
                            shell,
                            &frame_target,
                            &frame_command,
                            owner,
                            &frame_participants,
                            &frame_cancel,
                            &frame_pending,
                        )?;
                    } else {
                        native_owner::register_native_invocation_target(&frame_target, owner)?;
                    }
                    frame_target.registered_document()
                })();
                if let Ok(document) = &result {
                    let _ = registered_owner.set(document.owner().clone());
                    let _ = first.set(origin.clone());
                }
                frame_core.post(Box::new(move || registered(id, result)));
            }
            NativeEventKind::DocumentRevoked { document, .. }
                if first
                    .get()
                    .is_some_and(|origin| origin.identity() == document) =>
            {
                frame_core.post(Box::new(move || {
                    fail(id, "The native command document was revoked".into())
                }))
            }
            NativeEventKind::NavigationFailed { .. } => frame_core.post(Box::new(move || {
                fail(id, "The native command renderer failed".into())
            })),
            _ => {}
        }
    });
    let view = target.view();
    let created = target.created().clone();
    let proof = cx.views.closure_proof(view, &created);
    let gate_created = created.clone();
    let gate_operation = operation.clone();
    accepted::schedule_child(
        cx,
        core,
        view,
        created,
        proof,
        Box::new(move || {
            let invoke = move || create(frame);
            let action = match &startup {
                Some((root, shell)) => native_owner::with_startup_command_creation_effect(
                    root,
                    shell,
                    &gate_operation,
                    &command,
                    &gate_created,
                    &participants,
                    &cancel,
                    &pending,
                    || invoke,
                ),
                None => native_owner::with_native_command_creation_effect(
                    &gate_operation,
                    &command,
                    &gate_created,
                    &participants,
                    &cancel,
                    &pending,
                    || invoke,
                ),
            };
            match action {
                Some(action) => action(),
                None => Err(accepted::InstallError {
                    error: TabError("The native command expired before construction".into()),
                    close: None,
                    proof: None,
                }),
            }
        }),
        Box::new(move |result| created_result(id, result)),
    );
}

fn registered(id: u64, result: Result<NativeInvocationDocument, String>) {
    match result {
        Ok(document) => PENDING.with(|entries| {
            if let Some(state) = entries.borrow_mut().get_mut(&id) {
                state.document = Some(document);
            }
        }),
        Err(error) => return fail(id, error),
    }
    advance(id);
}

fn created_result(id: u64, result: ViewCreationResult<ViewId>) {
    match result {
        Ok(ready) => PENDING.with(|entries| {
            if let Some(state) = entries.borrow_mut().get_mut(&id) {
                state.ready = Some(ready);
            }
        }),
        Err(failure) => {
            let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
            if let Some(state) = state {
                (state.done)(Err(failure));
            }
            return;
        }
    }
    advance(id);
}

fn fail(id: u64, error: String) {
    PENDING.with(|entries| {
        if let Some(state) = entries.borrow_mut().get_mut(&id)
            && state.failure.is_none()
        {
            state.failure = Some(error);
        }
    });
    advance(id);
}

fn advance(id: u64) {
    let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
    let Some(mut state) = state else {
        return;
    };
    if state.cleaning {
        PENDING.with(|entries| {
            entries.borrow_mut().insert(id, state);
        });
        return;
    }
    if state.failure.is_none()
        && let (Some(document), Some(_)) = (&state.document, &state.ready)
    {
        let admitted = match &state.startup {
            Some((root, shell)) => native_owner::with_startup_command_target_effect(
                root,
                shell,
                &state.operation,
                &state.command,
                document,
                document.owner(),
                &state.participants,
                &state.cancel,
                &state.pending,
                || (),
            )
            .is_some(),
            None => native_owner::with_native_command_target_effect(
                &state.operation,
                &state.command,
                document,
                document.owner(),
                &state.participants,
                &state.cancel,
                &state.pending,
                || (),
            )
            .is_some(),
        };
        if admitted {
            let document = state.document.take().unwrap();
            let ready = state.ready.take().unwrap();
            return (state.done)(Ok(ViewCreationReady {
                view: document,
                cleanup: ready.cleanup,
                closure: ready.closure,
                reservation: ready.reservation,
            }));
        }
        state.failure = Some("The native command expired before delivery".into());
    }
    if state.failure.is_none() || state.ready.is_none() {
        PENDING.with(|entries| {
            entries.borrow_mut().insert(id, state);
        });
        return;
    }
    let cleanup = state.ready.as_ref().unwrap().cleanup.clone();
    let core = state.core.clone();
    state.cleaning = true;
    PENDING.with(|entries| {
        entries.borrow_mut().insert(id, state);
    });
    cleanup(Box::new(move |result| {
        core.post(Box::new(move || cleanup_finished(id, result)))
    }));
}

fn cleanup_finished(id: u64, result: Result<shell_contract::ipc::NativeBrowserRetirement, String>) {
    let state = PENDING.with(|entries| entries.borrow_mut().remove(&id));
    let Some(mut state) = state else {
        return;
    };
    let mut error = state
        .failure
        .take()
        .unwrap_or_else(|| "The native command failed".into());
    let cleanup = match result {
        Ok(receipt) => ViewCreationCleanupState::Acknowledged(receipt),
        Err(reason) => {
            error.push_str(&format!("; native cleanup failed: {reason}"));
            ViewCreationCleanupState::Pending(state.ready.take().unwrap().cleanup)
        }
    };
    (state.done)(Err(ViewCreationFailure {
        error: TabError(error),
        cleanup,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use main_core::shell::tab_manager::ViewSpec;
    use shell_contract::Surface;

    #[test]
    fn selected_docs_path_cannot_be_replaced_by_another_path() {
        let command = NativeEditorCommand::OpenFile {
            surface: Surface::Docs,
            path: "/selected.docx".into(),
        };
        assert!(matches_spec(
            &ViewSpec::Docs {
                path: Some("/selected.docx")
            },
            &command
        ));
        assert!(!matches_spec(
            &ViewSpec::Docs {
                path: Some("/other.docx")
            },
            &command
        ));
        assert!(!matches_spec(&ViewSpec::Docs { path: None }, &command));
    }

    #[test]
    fn workbook_command_requires_the_opening_workbook_view() {
        let command = NativeEditorCommand::OpenFile {
            surface: Surface::Sheets,
            path: "/selected.xlsx".into(),
        };
        assert!(matches_spec(
            &ViewSpec::Sheets {
                opening_workbook: Some(true)
            },
            &command
        ));
        assert!(!matches_spec(
            &ViewSpec::Sheets {
                opening_workbook: Some(false)
            },
            &command
        ));
    }
}
