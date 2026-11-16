//! Request-scoped retention of verified native origin and loss subscriptions.

mod effects;
mod saved_target;
pub(crate) use effects::register_startup_command_target;
pub(crate) use effects::with_command_preparation_service_effect;
pub(crate) use effects::with_command_recovery_preparation_effect;
pub(crate) use effects::with_command_recovery_target_effect;
pub(crate) use effects::with_existing_document_print_effect;
pub(crate) use effects::with_existing_document_save_many_effect;
pub(crate) use effects::with_existing_document_service_effect;
pub(crate) use effects::with_many_owner_effect_extra;
pub(crate) use effects::with_native_invocation_operation_effect;
pub(crate) use effects::with_native_save_revision_snapshot;
pub(crate) use effects::with_service_many_effect;
pub(crate) use effects::with_service_recovery_open_effect;
pub(crate) use effects::with_startup_command_recovery_preparation_effect;
pub(crate) use effects::with_startup_operation_effect;
pub(crate) use effects::with_startup_recovery_preparation_effect;
pub(crate) use effects::{
    capture_owner, host as effects_host, run_accepted_created_ui_many, run_accepted_ui_many,
    run_accepted_ui_pair, with_created_many_effect, with_many_owner_transition,
    with_native_invocation_effect, with_native_invocation_many_effect,
    with_native_invocation_print_many_effect, with_paired_effect, with_print_pair_effect,
    with_recovery_open_effect, with_recovery_seed_effect,
    with_service_native_invocation_effect, with_service_pair_effect,
};
pub(crate) use effects::{
    with_native_command_creation_effect, with_native_command_preparation_effect,
    with_native_command_target_effect,
};
pub(crate) use effects::{
    with_native_invocation_print_many_extra_effect, with_owned_print_many_extra_effect,
};
pub(crate) use effects::{
    with_recovery_open_service_snapshot, with_recovery_pre_navigation_snapshot,
    with_recovery_seed_snapshot,
};
pub(crate) use effects::{
    with_startup_command_creation_effect, with_startup_command_preparation_effect,
    with_startup_command_target_effect,
};
pub(crate) use saved_target::with_native_saved_document_same_origin;
pub(crate) use saved_target::{
    prepare_saved_spare_replacement, read_saved_input, with_saved_document_dialog_effect,
    with_saved_document_effect, with_saved_document_input_effect,
    with_saved_spare_replacement_effect, with_saved_target_creation_effect,
    with_saved_target_registration_effect, with_saved_target_seed_effect,
};

use crate::native::{DocumentContext, DocumentRevocation};
use shell_contract::{
    ViewId,
    ipc::{NativeContext, NativeRequestOwner},
    runtime::{CancelSubscription, CancelToken, RendererGeneration, Thread},
};

use crate::views::{DocsMediaAuthorization, NativeAssetAuthorization, SlidesViewLease};
use main_core::docs::byte_handoff::{MediaLossSubscription, MediaPermit, MediaViewLease};

struct SuccessorMediaPermit(MediaPermit);

/// The captured native origin and subscriptions live as long as the owner.
pub(crate) struct NativeOwnerAuthority {
    pub(crate) origin: DocumentContext,
    view: ViewId,
    browser_owner: Option<shell_contract::ipc::NativeBrowserOwner>,
    _origin_loss: DocumentRevocation,
    _renderer_loss: CancelSubscription,
    _asset_loss: Option<CancelSubscription>,
    docs_bound: bool,
    docs_permit: Option<MediaPermit>,
    _docs_loss: Option<MediaLossSubscription>,
    _slides_loss: Option<CancelSubscription>,
}

pub(crate) fn run_native_invocation_ui<R>(
    ui: &dyn Thread,
    operation: &shell_contract::ipc::NativeInvocationOperation,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    execute_accepted(operation.with_active(|| action))
}

/// Native-startup constructor admission, without inventing a renderer owner.
pub(crate) fn run_native_invocation_created_ui<R>(
    ui: &dyn Thread,
    target: &shell_contract::ipc::NativeInvocationTarget,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    execute_accepted(target.with_active(|| action))
}

pub(crate) fn run_accepted_native_invocation_ui_many<R>(
    ui: &dyn Thread,
    document: &shell_contract::ipc::NativeInvocationDocument,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    execute_accepted(with_native_invocation_many_effect(
        document.target(),
        owner,
        participants,
        cancel,
        pending,
        || action,
    ))
}

pub(crate) fn run_accepted_native_operation_ui_many<R>(
    ui: &dyn Thread,
    operation: &shell_contract::ipc::NativeInvocationOperation,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    execute_accepted(effects::with_startup_operation_effect(
        operation,
        owner,
        participants,
        cancel,
        pending,
        || action,
    ))
}

pub(crate) fn run_accepted_startup_recovery_ui_many<R>(
    ui: &dyn Thread,
    startup: &shell_contract::ipc::NativeInvocationOperation,
    shell: &NativeRequestOwner,
    child: &shell_contract::ipc::NativeInvocationDocument,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    execute_accepted(effects::with_startup_recovery_effect(
        startup,
        shell,
        child,
        owner,
        participants,
        cancel,
        pending,
        || action,
    ))
}

pub(crate) fn run_recovery_snapshot_ui<R>(
    ui: &dyn Thread,
    target: &shell_contract::ipc::RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    created: Option<&RendererGeneration>,
    dialog: Option<&NativeRequestOwner>,
    controls: Option<(&CancelToken, &CancelToken)>,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    if let (Some(dialog), Some(created)) = (dialog, created) {
        if !dialog
            .context
            .renderer()
            .is_some_and(|generation| generation.same_generation(created))
        {
            return Err("The recovery dialog generation changed.".into());
        }
    }
    execute_accepted(with_recovery_pre_navigation_snapshot(
        target,
        snapshot,
        created,
        dialog,
        controls,
        || action,
    ))
}

pub(crate) fn run_recovery_prepare_action_snapshot<R>(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    execute_accepted(with_recovery_seed_snapshot(target, snapshot, || action))
}

pub(crate) fn run_recovery_seeded_ui<R>(
    ui: &dyn Thread,
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    execute_accepted(effects::with_recovery_seeded_effect(receipt, || action))
}

/// Actual first-Registered producer only. The owner must have this shell's
/// private attested native proof and match the constructor's captured epoch.
pub(crate) fn register_native_invocation_target(
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: NativeRequestOwner,
) -> Result<(), String> {
    effects::register_native_invocation_target(target, owner)
}

pub(crate) fn register_recovery_open_target(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    owner: NativeRequestOwner,
) -> Result<(), String> {
    use shell_contract::ipc::RecoveryOpenContext;
    if !target.is_seed_committed() {
        return Err("The recovery seed has not been committed.".into());
    }
    if owner.view != target.view()
        || !owner
            .context
            .renderer()
            .is_some_and(|renderer| renderer.same_generation(target.created()))
    {
        return Err("The recovery target does not match its constructor reservation.".into());
    }
    let snapshot = target.authority_snapshot()?;
    match &snapshot.context {
        RecoveryOpenContext::NativeStartup(_) => {
            effects::register_native_recovery_target(target, owner)
        }
        RecoveryOpenContext::Renderer { .. } => {
            let loss = owner
                .loss_token()
                .ok_or("The recovery target has no native loss signal.")?;
            with_created_many_effect(&owner, &[], target.created(), &loss, &loss, || {
                snapshot.register_native_from_admitted(owner.clone())
            })
            .ok_or_else(|| "The recovery constructor is no longer available.".to_owned())?
        }
    }
}

pub(crate) fn authenticated_owner(context: &NativeContext) -> Option<NativeRequestOwner> {
    let authority = context.authority_ref::<NativeOwnerAuthority>()?;
    Some(NativeRequestOwner {
        view: authority.view,
        context: context.clone(),
    })
}

/// The exact browser owner captured alongside authenticated ingress, if adopted.
pub(crate) fn authenticated_browser_owner(
    context: &NativeContext,
) -> Option<&shell_contract::ipc::NativeBrowserOwner> {
    context
        .authority_ref::<NativeOwnerAuthority>()?
        .browser_owner
        .as_ref()
}

/// Assemble only after ingress has authenticated and bound all three identities.
pub(crate) fn asset_owner_with_browser(
    view: ViewId,
    origin: DocumentContext,
    asset: NativeAssetAuthorization,
    renderer: RendererGeneration,
    browser_owner: Option<shell_contract::ipc::NativeBrowserOwner>,
) -> Option<NativeRequestOwner> {
    if asset.view != view || !asset.matches_native_context(&origin) {
        return None;
    }
    let loss = CancelToken::default();
    let cancelled = loss.clone();
    let origin_loss = origin.on_revoke(move || cancelled.cancel())?;
    let cancelled = loss.clone();
    let renderer_loss = renderer.loss_token().on_cancel(move || cancelled.cancel());
    let cancelled = loss.clone();
    let independent_loss = asset.loss_token();
    let asset_loss = independent_loss.on_cancel(move || cancelled.cancel());
    if loss.is_cancelled() || !origin.is_live() || !renderer.is_active() || !asset.is_current() {
        return None;
    }
    let authority = NativeOwnerAuthority {
        view,
        origin,
        browser_owner,
        _origin_loss: origin_loss,
        _renderer_loss: renderer_loss,
        _asset_loss: Some(asset_loss),
        docs_bound: false,
        docs_permit: None,
        _docs_loss: None,
        _slides_loss: None,
    };
    let context = NativeContext::with_owner_loss(asset, renderer, loss)
        .with_independent_native_loss(independent_loss)
        .with_authority(authority);
    Some(NativeRequestOwner { view, context })
}

/// The caller must bind the exact origin, view and renderer before construction.
pub(crate) fn owner<T: std::any::Any + Send + Sync>(
    view: ViewId,
    origin: DocumentContext,
    renderer: RendererGeneration,
    payload: T,
) -> Option<NativeRequestOwner> {
    owner_with_browser(view, origin, renderer, payload, None)
}

pub(crate) fn owner_with_browser<T: std::any::Any + Send + Sync>(
    view: ViewId,
    origin: DocumentContext,
    renderer: RendererGeneration,
    payload: T,
    browser_owner: Option<shell_contract::ipc::NativeBrowserOwner>,
) -> Option<NativeRequestOwner> {
    let loss = CancelToken::default();
    let cancelled = loss.clone();
    let origin_loss = origin.on_revoke(move || cancelled.cancel())?;
    let cancelled = loss.clone();
    let renderer_loss = renderer.loss_token().on_cancel(move || cancelled.cancel());
    let value = &payload as &dyn std::any::Any;
    let docs = value.downcast_ref::<MediaViewLease>();
    let docs_authorization = value.downcast_ref::<DocsMediaAuthorization>();
    let explicit_permit = value
        .downcast_ref::<SuccessorMediaPermit>()
        .map(|permit| &permit.0);
    let docs_permit = match docs_authorization {
        Some(authorization) if authorization.view == view => Some(authorization.permit.clone()),
        Some(_) => return None,
        None => match explicit_permit {
            Some(permit) if permit.owner().view == view.0 => Some(permit.clone()),
            Some(_) => return None,
            None => docs.and_then(MediaViewLease::current),
        },
    };
    let docs_loss = docs_permit.as_ref().map(|permit| {
        let cancelled = loss.clone();
        permit.on_revoke(move || cancelled.cancel())
    });
    let slides_loss = value.downcast_ref::<SlidesViewLease>().map(|lease| {
        let cancelled = loss.clone();
        lease.loss_token().on_cancel(move || cancelled.cancel())
    });
    if loss.is_cancelled() || !origin.is_live() || !renderer.is_active() {
        return None;
    }
    let authority = NativeOwnerAuthority {
        view,
        origin,
        browser_owner,
        _origin_loss: origin_loss,
        _renderer_loss: renderer_loss,
        _asset_loss: None,
        docs_bound: docs.is_some() || docs_authorization.is_some() || explicit_permit.is_some(),
        docs_permit,
        _docs_loss: docs_loss,
        _slides_loss: slides_loss,
    };
    let context = NativeContext::with_owner_loss(payload, renderer, loss).with_authority(authority);
    Some(NativeRequestOwner { view, context })
}

/// Called at the beginning of the actual UI task with an immutable captured action.
/// Once accepted, construction completes normally even if the old owner is revoked.
/// Unlike a final file commit, the constructor runs after every authority gate drops.
pub(crate) fn run_accepted_ui<R, F: FnOnce() -> R>(
    ui: &dyn Thread,
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
    action: F,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    execute_accepted(with_owner_effect(owner, cancel, || action))
}

/// A short non-reentrant final effect. Never call UI/core constructors or wait here.
pub(crate) fn with_owner_effect<R>(
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
    action: impl FnOnce() -> R,
) -> Option<R> {
    with_source_effect(owner, None, cancel, cancel, action)
}

pub(crate) fn with_created_effect<R>(
    source: &NativeRequestOwner,
    created: &RendererGeneration,
    cancel: &CancelToken,
    pending: &CancelToken,
    action: impl FnOnce() -> R,
) -> Option<R> {
    with_source_effect(source, Some(created), cancel, pending, action)
}

pub(crate) fn run_accepted_created_ui<R>(
    ui: &dyn Thread,
    source: &NativeRequestOwner,
    created: &RendererGeneration,
    cancel: &CancelToken,
    pending: &CancelToken,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    execute_accepted(with_created_effect(
        source,
        created,
        cancel,
        pending,
        || action,
    ))
}

fn with_source_effect<R>(
    owner: &NativeRequestOwner,
    created: Option<&RendererGeneration>,
    cancel: &CancelToken,
    pending: &CancelToken,
    action: impl FnOnce() -> R,
) -> Option<R> {
    let authority = owner
        .context
        .authority_ref::<NativeOwnerAuthority>()
        .filter(|authority| authority.view == owner.view)?;
    let renderer = owner.context.renderer()?;
    let loss = owner.loss_token()?;
    if authority.docs_bound && authority.docs_permit.is_none() {
        return None;
    }
    let effect = || {
        RendererGeneration::with_active_all(
            [Some(renderer), owner.context.document_lease(), created],
            || {
                SlidesViewLease::with_lease_active_pair(
                    [owner.context.downcast_ref::<SlidesViewLease>(), None],
                    || {
                        MediaPermit::with_active_all([authority.docs_permit.as_ref()], || {
                            with_pdf_lifetimes(owner, || {
                                CancelToken::with_active_all(
                                    [Some(cancel), Some(pending), Some(&loss)],
                                    action,
                                )
                            })
                            .flatten()
                        })
                        .flatten()
                    },
                )
                .flatten()
            },
        )
        .flatten()
    };
    let asset = owner.context.downcast_ref::<NativeAssetAuthorization>();
    if asset.is_some_and(|asset| {
        asset.view != owner.view || !asset.matches_native_context(&authority.origin)
    }) {
        return None;
    }
    let apply = || match asset {
        Some(asset) => asset.with_lease_live_fenced(effect),
        None => Some(effect()),
    };
    authority.origin.with_live(apply).flatten().flatten()
}

fn with_pdf_lifetimes<T>(owner: &NativeRequestOwner, action: impl FnOnce() -> T) -> Option<T> {
    shell_contract::ipc::PdfDocumentLease::with_live_all(
        [owner.pdf_document(), owner.pdf_revision()],
        action,
    )
}

fn execute_accepted<R>(action: Option<impl FnOnce() -> R>) -> Result<R, String> {
    match action {
        Some(action) => Ok(action()),
        None => Err("Renderer document is no longer available.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::effects::commit_recovery_seed;
    use super::*;
    use std::cell::Cell;

    #[test]
    fn revocation_before_admission_prevents_ui_effect() {
        let generation = RendererGeneration::new();
        generation.revoke();
        let effects = Cell::new(0);
        let action = || effects.set(effects.get() + 1);
        assert!(execute_accepted(generation.with_active(|| action)).is_err());
        assert_eq!(effects.get(), 0);
    }

    #[test]
    fn revocation_after_admission_does_not_abandon_accepted_action() {
        let generation = RendererGeneration::new();
        let effects = Cell::new(0);
        let action = || effects.set(effects.get() + 1);
        let accepted = generation.with_active(|| action);
        generation.revoke();
        assert_eq!(execute_accepted(accepted), Ok(()));
        assert_eq!(effects.get(), 1);
    }

    #[test]
    fn accepted_constructor_can_reenter_and_finish_after_revocation() {
        let generation = RendererGeneration::new();
        let effects = Cell::new(0);
        let action = || {
            generation.revoke();
            effects.set(effects.get() + 1);
            Err::<(), _>("native construction failed")
        };
        let accepted = generation.with_active(|| action);
        assert_eq!(
            execute_accepted(accepted),
            Ok(Err("native construction failed"))
        );
        assert_eq!(effects.get(), 1);
        assert!(!generation.is_active());
    }

    fn invocation() -> shell_contract::ipc::NativeInvocationOperation {
        use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
        NativeInvocationIssuer::new(CancelToken::default()).issue(
            NativeInvocationSelection::HeadlessExport {
                surface: shell_contract::Surface::Docs,
                input: "selected.docx".into(),
                output: "selected.pdf".into(),
                format: "pdf".into(),
                page: None,
                ops: None,
                batch: None,
            },
            CancelToken::default(),
        )
    }

    #[test]
    fn native_registration_rejects_a_context_without_attested_proof() {
        let operation = invocation();
        let created = RendererGeneration::new();
        let target = operation
            .reserve_target(ViewId(19), created.clone())
            .unwrap();
        let owner = NativeRequestOwner {
            view: ViewId(19),
            context: NativeContext::with_owner_loss((), created, CancelToken::default()),
        };
        assert!(register_native_invocation_target(&target, owner).is_err());
        assert!(target.registered_document().is_err());
    }

    #[test]
    fn a_registered_contract_snapshot_does_not_fabricate_native_authority() {
        let operation = invocation();
        let created = RendererGeneration::new();
        let target = operation
            .reserve_target(ViewId(20), created.clone())
            .unwrap();
        let owner = NativeRequestOwner {
            view: ViewId(20),
            context: NativeContext::with_owner_loss((), created, CancelToken::default()),
        };
        // Bypass only the Rust contract in this fixture. The real native adapter
        // must still refuse it even though a registered snapshot exists.
        target
            .with_active(|| target.register_from_native_admitted(owner.clone()))
            .unwrap()
            .unwrap();
        assert!(target.registered_document().is_ok());
        let effects = Cell::new(0);
        assert!(
            with_native_invocation_effect(
                &target,
                &owner,
                operation.cancel(),
                operation.shutdown(),
                || effects.set(effects.get() + 1)
            )
            .is_none()
        );
        assert_eq!(effects.get(), 0);
    }

    #[test]
    fn native_constructor_action_can_finish_the_operation_after_admission() {
        let operation = invocation();
        let effects = Cell::new(0);
        let accepted = operation.with_active(|| {
            || {
                operation.finish();
                effects.set(effects.get() + 1);
            }
        });
        assert_eq!(execute_accepted(accepted), Ok(()));
        assert_eq!(effects.get(), 1);
        assert!(operation.with_active(|| ()).is_none());
    }

    #[test]
    fn recovery_commit_notifies_preparation_loss_after_all_gates_release() {
        use shell_contract::ipc::{
            NativeInvocationIssuer, NativeInvocationSelection, RecoveryOpenContext,
            RecoveryOpenTarget,
        };
        let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
            NativeInvocationSelection::RecoveryBlank {
                surface: shell_contract::Surface::Docs,
                title: "Untitled".into(),
            },
            CancelToken::default(),
        );
        let created = RendererGeneration::new();
        let (_issuer, closure, owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(ViewId(21), created.clone());
        let target = RecoveryOpenTarget::bind(
            RecoveryOpenContext::NativeStartup(operation.clone()),
            ViewId(21),
            created,
            closure,
            owner,
        )
        .unwrap();
        let to_finish = operation.clone();
        let _subscription = target
            .preparation_loss_token()
            .on_cancel(move || to_finish.finish());
        let receipt = commit_recovery_seed(&target, || Ok(())).unwrap();
        assert!(receipt.matches(&target));
        assert!(operation.loss_token().is_cancelled());
    }
}
