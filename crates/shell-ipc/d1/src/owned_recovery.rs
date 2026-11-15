use crate::host::RecoveryStateHost;
use main_core::node_fs::NodeError;
use main_core::packages::electron_utils::atomic_write::owned::{Admission, admitted};
use main_core::packages::electron_utils::recovery_store::{
    RecoveryDeletePlan, RecoveryDeleteReason, RecoveryIdentity, RecoveryStoreOptions,
    create_recovery_store,
};
use std::sync::TryLockError;

pub(crate) struct DeletedCopy {
    pub path: String,
    pub refused: bool,
}

enum Preparation {
    Busy,
    Ready(String, Option<RecoveryDeletePlan>),
}

pub(crate) fn delete_copies(
    shared: &RecoveryStateHost,
    directory: &str,
    identities: &[RecoveryIdentity],
    reason: RecoveryDeleteReason,
    admission: &Admission,
) -> Result<Vec<DeletedCopy>, NodeError> {
    let mut outcomes = Vec::with_capacity(identities.len());
    for identity in identities {
        let (path, plan) = loop {
            let shared = shared.clone();
            let directory = directory.to_owned();
            let identity = identity.clone();
            let prepared = admitted(admission, move || {
                let state = match shared.try_lock() {
                    Ok(state) => state,
                    Err(TryLockError::WouldBlock) => return Ok(Preparation::Busy),
                    Err(TryLockError::Poisoned(_)) => {
                        return Err(NodeError {
                            code: None,
                            message: "The recovery state is unavailable".into(),
                        });
                    }
                };
                let root = || directory.clone();
                let store = create_recovery_store(
                    RecoveryStoreOptions {
                        user_data_dir: &root,
                        now_ms: &now_ms,
                        random_uuid: &jscompat::random_uuid,
                    },
                    &state,
                );
                let path = store.copy_path(&identity)?;
                let plan = store.prepare_delete(&identity, reason)?;
                Ok::<_, NodeError>(Preparation::Ready(path, plan))
            })??;
            match prepared {
                Preparation::Ready(path, plan) => break (path, plan),
                Preparation::Busy => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        };
        let refused = plan.is_none();
        if let Some(plan) = plan {
            plan.remove_owned(&mut |effect| admitted(admission, effect))?;
        }
        outcomes.push(DeletedCopy { path, refused });
    }
    Ok(outcomes)
}

pub(crate) fn seed_admission(
    effects: shell_contract::hosts::OwnedEffectsHost,
    target: shell_contract::ipc::RecoveryOpenTarget,
    snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
) -> Admission {
    std::sync::Arc::new(move |effect| {
        let mut effect = Some(effect);
        let mut result = None;
        effects
            .with_recovery_seed_snapshot(&target, &snapshot, &mut || {
                result = Some(effect
                    .take()
                    .ok_or("The recovery effect was already consumed")?(
                ));
                Ok(())
            })
            .map_err(|message| NodeError {
                code: None,
                message,
            })?;
        result.ok_or_else(|| NodeError {
            code: None,
            message: "The recovery effect was not admitted".into(),
        })
    })
}

pub(crate) fn recovery_losses(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
) -> Vec<shell_contract::runtime::CancelToken> {
    use shell_contract::ipc::RecoveryOpenContext;
    let mut losses = vec![
        target.created().loss_token(),
        snapshot.revision_loss_token(),
    ];
    match &snapshot.context {
        RecoveryOpenContext::Renderer {
            source,
            participants,
            cancel,
            pending,
        } => {
            losses.extend([cancel.clone(), pending.clone()]);
            for owner in std::iter::once(source).chain(participants) {
                losses.extend(owner.loss_token());
                losses.extend(
                    owner
                        .context
                        .renderer()
                        .map(|renderer| renderer.loss_token()),
                );
                losses.extend(
                    owner
                        .context
                        .document_lease()
                        .map(|document| document.loss_token()),
                );
            }
        }
        RecoveryOpenContext::NativeStartup(operation) => {
            losses.extend([
                operation.cancel().clone(),
                operation.shutdown().clone(),
                operation.loss_token(),
            ]);
        }
    }
    if let Some(request) = &snapshot.request {
        losses.extend([request.cancel.clone(), request.pending.clone()]);
        for owner in std::iter::once(&request.source).chain(&request.participants) {
            losses.extend(owner.loss_token());
            losses.extend(
                owner
                    .context
                    .renderer()
                    .map(|renderer| renderer.loss_token()),
            );
            losses.extend(
                owner
                    .context
                    .document_lease()
                    .map(|document| document.loss_token()),
            );
        }
    }
    if let Some(startup) = &snapshot.startup {
        losses.extend([
            startup.startup.loss_token(),
            startup.startup.cancel().clone(),
            startup.startup.shutdown().clone(),
            startup.cancel.clone(),
            startup.pending.clone(),
        ]);
        for owner in std::iter::once(&startup.shell).chain(&startup.participants) {
            losses.extend(owner.loss_token());
            losses.extend(
                owner
                    .context
                    .renderer()
                    .map(|renderer| renderer.loss_token()),
            );
            losses.extend(
                owner
                    .context
                    .document_lease()
                    .map(|document| document.loss_token()),
            );
        }
    }
    if let Some(command) = &snapshot.command {
        losses.extend([
            command.command.loss_token(),
            command.command.cancel().clone(),
            command.command.shutdown().clone(),
            command.cancel.clone(),
            command.pending.clone(),
        ]);
        for owner in &command.participants {
            losses.extend(owner.loss_token());
            losses.extend(
                owner
                    .context
                    .renderer()
                    .map(|renderer| renderer.loss_token()),
            );
            losses.extend(
                owner
                    .context
                    .document_lease()
                    .map(|document| document.loss_token()),
            );
        }
    }
    losses
}

pub(crate) fn mark_loaded(
    shared: &RecoveryStateHost,
    directory: &str,
    identity: &RecoveryIdentity,
    admission: &Admission,
) -> Result<(), NodeError> {
    let copy = main_core::packages::electron_utils::recovery_store::recovery_copy_path(
        directory, identity,
    )?;
    let metadata = main_core::packages::electron_utils::recovery_store::RecoveryStore::prepare_copy_state_from_path(
        identity, &copy, main_core::packages::electron_utils::recovery_store::RecoveryCopyState::Undecided,
    );
    loop {
        let shared = shared.clone();
        let directory = directory.to_owned();
        let identity = identity.clone();
        let cleared = admitted(admission, move || {
            let state = match shared.try_lock() {
                Ok(state) => state,
                Err(TryLockError::WouldBlock) => return Ok(false),
                Err(TryLockError::Poisoned(_)) => {
                    return Err(NodeError {
                        code: None,
                        message: "The recovery state is unavailable".into(),
                    });
                }
            };
            let root = || directory.clone();
            let store = create_recovery_store(
                RecoveryStoreOptions {
                    user_data_dir: &root,
                    now_ms: &now_ms,
                    random_uuid: &jscompat::random_uuid,
                },
                &state,
            );
            store.clear_discarded(&identity)?;
            Ok(true)
        })??;
        if cleared {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if let Some(metadata) = metadata {
        metadata.write_via(&mut |path, json| {
            main_core::packages::electron_utils::atomic_write::owned::write_json(
                path,
                json,
                admission.clone(),
            )
            .map(|()| true)
        })?;
    }
    Ok(())
}

pub(crate) fn check_selected_file(
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    surface: shell_contract::Surface,
    request: &main_core::packages::electron_utils::recovery_contract::RecoveryOpenFileRequest,
) -> Result<(), String> {
    use main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision;
    use shell_contract::ipc::{
        NativeInvocationSelection, NativeRecoveryDecision, RecoveryOpenContext,
    };
    if let RecoveryOpenContext::NativeStartup(operation) = &snapshot.context {
        let decision = match request.decision {
            RecoveryOpenDecision::Restore => NativeRecoveryDecision::Restore,
            RecoveryOpenDecision::Original => NativeRecoveryDecision::Original,
        };
        if !matches!(operation.selection(), NativeInvocationSelection::RecoveryFile { surface: selected, path, decision: selected_decision }
            if *selected == surface && path == std::path::Path::new(&request.file_path) && *selected_decision == decision)
        {
            return Err("The recovery file does not match the native startup selection".into());
        }
    }
    Ok(())
}

pub(crate) fn check_selected_copy(
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    surface: shell_contract::Surface,
    request: &main_core::packages::electron_utils::recovery_contract::RecoveryOpenUntitledRequest,
) -> Result<(), String> {
    use shell_contract::ipc::{NativeInvocationSelection, NativeRecoveryCopy, RecoveryOpenContext};
    if let RecoveryOpenContext::NativeStartup(operation) = &snapshot.context {
        let selected = match &request.copy {
            RecoveryIdentity::File(file) => NativeRecoveryCopy::File {
                path: file.file_path.clone().into(),
            },
            RecoveryIdentity::Untitled(copy) => NativeRecoveryCopy::Untitled {
                id: copy.untitled_id.clone(),
                ext: copy.ext.clone(),
            },
        };
        if request.copy.kind()
            != match surface {
                shell_contract::Surface::Slides => "slides",
                shell_contract::Surface::Mindmap => "mindmap",
                shell_contract::Surface::Diagram => "diagram",
                _ => return Err("The recovery surface is unsupported".into()),
            }
            || !matches!(operation.selection(), NativeInvocationSelection::RecoveryCopy { surface: actual, copy, title, save_as_default }
                if *actual == surface && *copy == selected && *title == request.title
                    && save_as_default.as_deref() == request.save_as_default_path.as_deref().map(std::path::Path::new))
        {
            return Err("The recovery copy does not match the native startup selection".into());
        }
    }
    Ok(())
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |value| value.as_millis() as f64)
}
