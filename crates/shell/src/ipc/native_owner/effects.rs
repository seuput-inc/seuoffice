use std::sync::Arc;

use shell_contract::{
    IpcValue, ViewId,
    hosts::{OwnedEffects, OwnedEffectsHost},
    ipc::{NativeRequestOwner, PdfDocumentLease},
    runtime::{CancelToken, RendererGeneration, Thread},
};
use tauri::Manager;

use super::NativeOwnerAuthority;
use crate::{
    host::HostContext,
    views::{NativeAssetAuthorization, SlidesViewLease},
};
use main_core::docs::byte_handoff::MediaPermit;

fn proof(owner: &NativeRequestOwner) -> Option<&NativeOwnerAuthority> {
    owner
        .context
        .authority_ref::<NativeOwnerAuthority>()
        .filter(|proof| proof.view == owner.view)
}
fn saved_docs_origin<'a>(
    document: &shell_contract::ipc::NativeSavedDocument,
    actual: &NativeRequestOwner,
    original: &'a NativeRequestOwner,
) -> Result<&'a NativeOwnerAuthority, String> {
    let registered = document.owner();
    let expected = proof(registered).ok_or("The registered saved document is unavailable.")?;
    let caller = proof(actual).ok_or("The calling renderer is unavailable.")?;
    let media = proof(original).ok_or("The captured Docs owner is unavailable.")?;
    let renderer = registered
        .context
        .renderer()
        .ok_or("The saved renderer is unavailable.")?;
    if actual.view != registered.view
        || original.view != registered.view
        || caller.origin != expected.origin
        || media.origin != expected.origin
        || !actual
            .context
            .renderer()
            .is_some_and(|value| value.same_generation(renderer))
        || !original
            .context
            .renderer()
            .is_some_and(|value| value.same_generation(renderer))
    {
        return Err("The saved Docs renderer does not match its registered origin.".into());
    }
    Ok(media)
}

pub(crate) fn with_paired_effect<R>(
    origin: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_optional_service(origin, destination, None, None, cancel, pending, effect)
}

pub(crate) fn with_service_pair_effect<R>(
    origin: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    service: &crate::native::DocumentContext,
    service_generation: &RendererGeneration,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_optional_service(
        origin,
        destination,
        Some(service),
        Some(service_generation),
        cancel,
        pending,
        effect,
    )
}

fn with_optional_service<R>(
    origin: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    service: Option<&crate::native::DocumentContext>,
    service_generation: Option<&RendererGeneration>,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_optional_service_transition(
        origin,
        destination,
        service,
        service_generation,
        cancel,
        pending,
        |_| effect(),
    )
}

struct DeferredDocumentRetirements([Option<CancelToken>; 2]);

struct DeferredSettlement(Option<shell_contract::ipc::DeferredRecoverySettlement>);

impl Drop for DeferredSettlement {
    fn drop(&mut self) {
        if let Some(notification) = self.0.take() {
            notification.notify();
        }
    }
}

struct DeferredPreparation(Option<CancelToken>);

impl Drop for DeferredPreparation {
    fn drop(&mut self) {
        if let Some(loss) = self.0.take() {
            loss.cancel();
        }
    }
}

impl Drop for DeferredDocumentRetirements {
    fn drop(&mut self) {
        for notification in &mut self.0 {
            if let Some(notification) = notification.take() {
                notification.cancel();
            }
        }
    }
}

fn with_optional_service_transition<R>(
    origin: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    service: Option<&crate::native::DocumentContext>,
    service_generation: Option<&RendererGeneration>,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce(&mut shell_contract::runtime::AdmittedDocumentRetirement<'_>) -> R,
) -> Option<R> {
    let source = proof(origin)?;
    let target = proof(destination)?;
    let source_renderer = origin.context.renderer()?;
    let target_renderer = destination.context.renderer()?;
    let source_loss = origin.loss_token()?;
    let target_loss = destination.loss_token()?;
    if (source.docs_bound && source.docs_permit.is_none())
        || (target.docs_bound && target.docs_permit.is_none())
    {
        return None;
    }
    let source_asset = origin.context.downcast_ref::<NativeAssetAuthorization>();
    let target_asset = destination
        .context
        .downcast_ref::<NativeAssetAuthorization>();
    if source_asset.is_some_and(|asset| {
        asset.view != origin.view || !asset.matches_native_context(&source.origin)
    }) || target_asset.is_some_and(|asset| {
        asset.view != destination.view || !asset.matches_native_context(&target.origin)
    }) {
        return None;
    }
    // Declared outside every gate: even unwinding releases all admission guards
    // before loss observers can reenter native/document state.
    let mut retired = DeferredDocumentRetirements([None, None]);
    let commit = || {
        RendererGeneration::with_active_all_retiring(
            [
                Some(source_renderer),
                Some(target_renderer),
                origin.context.document_lease(),
                destination.context.document_lease(),
                service_generation,
            ],
            [
                origin.context.document_lease(),
                destination.context.document_lease(),
            ],
            &mut retired.0,
            |retirement| {
                SlidesViewLease::with_lease_active_pair(
                    [
                        origin.context.downcast_ref::<SlidesViewLease>(),
                        destination.context.downcast_ref::<SlidesViewLease>(),
                    ],
                    || {
                        MediaPermit::with_active_all(
                            [source.docs_permit.as_ref(), target.docs_permit.as_ref()],
                            || {
                                PdfDocumentLease::with_live_all(
                                    [
                                        origin.pdf_document(),
                                        destination.pdf_document(),
                                        origin.pdf_revision(),
                                        destination.pdf_revision(),
                                    ],
                                    || {
                                        CancelToken::with_active_all(
                                            [
                                                Some(cancel),
                                                Some(pending),
                                                Some(&source_loss),
                                                Some(&target_loss),
                                            ],
                                            || effect(retirement),
                                        )
                                    },
                                )
                                .flatten()
                            },
                        )
                        .flatten()
                    },
                )
                .flatten()
            },
        )
        .flatten()
    };
    crate::native::DocumentContext::with_live_all(
        [Some(&source.origin), Some(&target.origin), service],
        || match (source_asset, target_asset) {
            (Some(source), Some(target)) => source.with_lease_live_pair_fenced(target, commit),
            (Some(asset), None) | (None, Some(asset)) => asset.with_lease_live_fenced(commit),
            (None, None) => Some(commit()),
        },
    )
    .flatten()
    .flatten()
}

/// Prepare the entire captured participant set before entering any authority gate.
/// All categories retain their existing global ordering; each plan deduplicates aliases.
pub(crate) fn with_many_owner_transition<R>(
    origin: &NativeRequestOwner,
    destinations: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce(&mut shell_contract::runtime::AdmittedDocumentRetirement<'_>) -> R,
) -> Option<R> {
    with_many_owner_generations_transition(
        origin,
        destinations,
        None,
        &[],
        &[],
        cancel,
        pending,
        effect,
    )
}

pub(crate) fn with_many_owner_effect_extra<R>(
    origin: &NativeRequestOwner,
    destinations: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_many_owner_generations_transition(
        origin,
        destinations,
        None,
        &[],
        additional,
        cancel,
        pending,
        |_| effect(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn with_many_owner_generations_transition<R>(
    origin: &NativeRequestOwner,
    destinations: &[NativeRequestOwner],
    service: Option<&crate::native::DocumentContext>,
    additional_generations: &[&RendererGeneration],
    additional_cancellation: &[&CancelToken],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce(&mut shell_contract::runtime::AdmittedDocumentRetirement<'_>) -> R,
) -> Option<R> {
    with_many_owner_generations_media_transition(
        origin,
        destinations,
        service,
        additional_generations,
        additional_cancellation,
        cancel,
        pending,
        None,
        |retirement, _| effect(retirement),
    )
}

pub(crate) fn with_service_many_effect<R>(
    origin: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    service: &crate::native::DocumentContext,
    generation: &RendererGeneration,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_many_owner_generations_transition(
        origin,
        participants,
        Some(service),
        &[generation],
        &[],
        cancel,
        pending,
        |_| effect(),
    )
}

pub(super) struct PreparedOwnerTransition<'a> {
    native: Option<crate::native::DocumentGatePlan<'a>>,
    assets: Option<crate::views::AssetLeaseGatePlan<'a>>,
    generations: shell_contract::runtime::RendererGenerationGatePlan<'a>,
    retirement: shell_contract::runtime::DocumentRetirementPlan<'a>,
    slides: crate::views::SlidesViewLeaseGatePlan<'a>,
    media: main_core::docs::byte_handoff::MediaPermitGatePlan<'a>,
    pdf: shell_contract::ipc::PdfDocumentGatePlan<'a>,
    cancellation: shell_contract::runtime::CancelTokenGatePlan<'a>,
}

pub(super) fn prepare_many_owner_generations_transition<'a>(
    origin: &'a NativeRequestOwner,
    destinations: &'a [NativeRequestOwner],
    service: Option<&'a crate::native::DocumentContext>,
    additional_generations: &[&'a RendererGeneration],
    additional_cancellation: &[&'a CancelToken],
    cancel: &'a CancelToken,
    pending: &'a CancelToken,
) -> Option<PreparedOwnerTransition<'a>> {
    use crate::views::{AssetLeaseGatePlan, SlidesViewLeaseGatePlan};
    use main_core::docs::byte_handoff::MediaPermitGatePlan;
    use shell_contract::{
        ipc::PdfDocumentGatePlan,
        runtime::{CancelTokenGatePlan, RendererGenerationGatePlan},
    };

    let owners: Vec<_> = std::iter::once(origin).chain(destinations.iter()).collect();
    let proofs: Vec<_> = owners.iter().copied().map(proof).collect::<Option<_>>()?;
    let losses: Vec<_> = owners
        .iter()
        .copied()
        .map(|owner| owner.loss_token_ref())
        .collect::<Option<_>>()?;
    let mut native_tokens = Vec::with_capacity(proofs.len() + usize::from(service.is_some()));
    native_tokens.extend(proofs.iter().copied().map(|proof| &proof.origin));
    if let Some(service) = service {
        native_tokens.push(service);
    }
    let mut assets = Vec::with_capacity(owners.len());
    let mut generations = Vec::with_capacity(owners.len() * 2 + additional_generations.len());
    let mut documents = Vec::with_capacity(owners.len());
    let mut slides = Vec::with_capacity(owners.len());
    let mut media = Vec::with_capacity(owners.len());
    let mut pdf = Vec::with_capacity(owners.len() * 2);
    for (owner, proof) in owners.iter().copied().zip(proofs.iter().copied()) {
        generations.push(owner.context.renderer()?);
        if let Some(document) = owner.context.document_lease() {
            generations.push(document);
            documents.push(document);
        }
        if proof.docs_bound && proof.docs_permit.is_none() {
            return None;
        }
        if let Some(permit) = proof.docs_permit.as_ref() {
            media.push(permit);
        }
        if let Some(asset) = owner.context.downcast_ref::<NativeAssetAuthorization>() {
            if asset.view != owner.view || !asset.matches_native_context(&proof.origin) {
                return None;
            }
            assets.push(asset);
        }
        if let Some(lease) = owner.context.downcast_ref::<SlidesViewLease>() {
            slides.push(lease);
        }
        if let Some(document) = owner.pdf_document() {
            pdf.push(document);
        }
        if let Some(revision) = owner.pdf_revision() {
            pdf.push(revision);
        }
    }
    generations.extend_from_slice(additional_generations);
    let generations = RendererGenerationGatePlan::from_generations(generations);
    let retirement = generations.prepare_retirement(&documents).ok()?;
    let native = crate::native::DocumentGatePlan::new(&native_tokens)?;
    let assets = AssetLeaseGatePlan::new(&assets);
    let slides = SlidesViewLeaseGatePlan::new(&slides);
    let media = MediaPermitGatePlan::new(&media);
    let pdf = PdfDocumentGatePlan::new(&pdf);
    let mut cancellation = Vec::with_capacity(losses.len() + additional_cancellation.len() + 2);
    cancellation.extend([cancel, pending]);
    cancellation.extend(losses);
    cancellation.extend_from_slice(additional_cancellation);
    let cancellation = CancelTokenGatePlan::from_tokens(cancellation);
    Some(PreparedOwnerTransition {
        native: Some(native),
        assets: Some(assets),
        generations,
        retirement,
        slides,
        media,
        pdf,
        cancellation,
    })
}

impl PreparedOwnerTransition<'_> {
    pub(super) fn with_live<R>(
        &mut self,
        effect: impl FnOnce(&mut shell_contract::runtime::AdmittedDocumentRetirement<'_>) -> R,
    ) -> Option<R> {
        self.with_media(None, |retirement, _| effect(retirement))
    }

    pub(super) fn with_media<R>(
        &mut self,
        media_retirement: Option<(
            &MediaPermit,
            &MediaPermit,
            &mut main_core::docs::byte_handoff::DeferredMediaLoss,
        )>,
        effect: impl FnOnce(
            &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
            Option<&mut main_core::docs::byte_handoff::AdmittedMediaRetirement<'_>>,
        ) -> R,
    ) -> Option<R> {
        let native = self.native.take()?;
        let assets = self.assets.take()?;
        let Self {
            generations,
            retirement,
            slides,
            media,
            pdf,
            cancellation,
            ..
        } = self;
        let mut effect = Some(effect);
        native
            .with_live(|| {
                assets
                    .with_lease_live_fenced(|| {
                        generations
                            .with_active_retiring(retirement, |capability| {
                                slides
                                    .with_active(|| match media_retirement {
                                        Some((old, successor, notifications)) => media
                                            .with_active_retiring(
                                                old,
                                                successor,
                                                notifications,
                                                |media_retirement| {
                                                    pdf.with_live(|| {
                                                        cancellation.with_active(|| {
                                                                effect
                                                                    .take()
                                                                    .expect("single admission")(
                                                                    capability,
                                                                    Some(media_retirement),
                                                                )
                                                            })
                                                    })
                                                    .flatten()
                                                },
                                            )
                                            .flatten(),
                                        None => media
                                            .with_active(|| {
                                                pdf.with_live(|| {
                                                    cancellation.with_active(|| {
                                                        effect.take().expect("single admission")(
                                                            capability, None,
                                                        )
                                                    })
                                                })
                                                .flatten()
                                            })
                                            .flatten(),
                                    })
                                    .flatten()
                            })
                            .flatten()
                    })
                    .flatten()
            })
            .flatten()
    }
}

#[allow(clippy::too_many_arguments)]
fn with_many_owner_generations_media_transition<R>(
    origin: &NativeRequestOwner,
    destinations: &[NativeRequestOwner],
    service: Option<&crate::native::DocumentContext>,
    additional_generations: &[&RendererGeneration],
    additional_cancellation: &[&CancelToken],
    cancel: &CancelToken,
    pending: &CancelToken,
    media_retirement: Option<(
        &MediaPermit,
        &MediaPermit,
        &mut main_core::docs::byte_handoff::DeferredMediaLoss,
    )>,
    effect: impl FnOnce(
        &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
        Option<&mut main_core::docs::byte_handoff::AdmittedMediaRetirement<'_>>,
    ) -> R,
) -> Option<R> {
    prepare_many_owner_generations_transition(
        origin,
        destinations,
        service,
        additional_generations,
        additional_cancellation,
        cancel,
        pending,
    )?
    .with_media(media_retirement, effect)
}

fn same_registered_origin(
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &NativeRequestOwner,
) -> Option<shell_contract::ipc::NativeInvocationDocument> {
    let registered = target.registered_document().ok()?;
    let actual = proof(owner)?;
    let original = proof(registered.owner())?;
    if owner.view != target.view()
        || registered.owner().view != owner.view
        || actual.origin != original.origin
        || !owner.context.renderer()?.same_generation(target.created())
        || !registered
            .owner()
            .context
            .renderer()?
            .same_generation(target.created())
    {
        return None;
    }
    Some(registered)
}

#[allow(clippy::too_many_arguments)]
fn with_native_invocation_fences<R>(
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    service: Option<&crate::native::DocumentContext>,
    service_generation: Option<&RendererGeneration>,
    print_generation: Option<&RendererGeneration>,
    actor: Option<&CancelToken>,
    stop: Option<&CancelToken>,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let registered = same_registered_origin(target, owner)?;
    let operation = target.operation();
    let generations = [
        operation.authority_generation(),
        target.created(),
        service_generation.unwrap_or(target.created()),
        print_generation.unwrap_or(target.created()),
    ];
    let cancellation = [
        operation.cancel(),
        operation.shutdown(),
        actor.unwrap_or(cancel),
        stop.unwrap_or(cancel),
    ];
    with_many_owner_generations_transition(
        owner,
        participants,
        service,
        &generations,
        &cancellation,
        cancel,
        pending,
        |_| target.with_registered_state(registered.owner(), effect),
    )
    .flatten()
}

pub(crate) fn with_native_invocation_effect<R>(
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_native_invocation_many_effect(target, owner, &[], cancel, pending, effect)
}

pub(crate) fn with_native_invocation_many_effect<R>(
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_native_invocation_fences(
        target,
        owner,
        participants,
        None,
        None,
        None,
        None,
        None,
        cancel,
        pending,
        effect,
    )
}

pub(crate) fn with_service_native_invocation_effect<R>(
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &NativeRequestOwner,
    service: &crate::native::DocumentContext,
    service_generation: &RendererGeneration,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_native_invocation_fences(
        target,
        owner,
        &[],
        Some(service),
        Some(service_generation),
        None,
        None,
        None,
        cancel,
        pending,
        effect,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn with_native_invocation_print_many_effect<R>(
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    actor: &CancelToken,
    stop: &CancelToken,
    print_generation: &RendererGeneration,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_native_invocation_fences(
        target,
        owner,
        participants,
        None,
        None,
        Some(print_generation),
        Some(actor),
        Some(stop),
        cancel,
        pending,
        effect,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn with_owned_print_many_extra_effect<R>(
    origin: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    stop: &CancelToken,
    print_generation: &RendererGeneration,
    additional: &[&CancelToken],
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let mut controls = Vec::with_capacity(additional.len() + 1);
    controls.push(stop);
    controls.extend_from_slice(additional);
    with_many_owner_generations_transition(
        origin,
        participants,
        None,
        &[print_generation],
        &controls,
        cancel,
        pending,
        |_| effect(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn with_native_invocation_print_many_extra_effect<R>(
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    stop: &CancelToken,
    print_generation: &RendererGeneration,
    additional: &[&CancelToken],
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let registered = same_registered_origin(target, owner)?;
    let operation = target.operation();
    let mut controls = vec![operation.cancel(), operation.shutdown(), stop];
    controls.extend_from_slice(additional);
    with_many_owner_generations_transition(
        owner,
        participants,
        None,
        &[
            operation.authority_generation(),
            target.created(),
            print_generation,
        ],
        &controls,
        cancel,
        pending,
        |_| target.with_registered_state(registered.owner(), effect),
    )
    .flatten()
}

pub(super) fn register_native_invocation_target(
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: NativeRequestOwner,
) -> Result<(), String> {
    register_native_invocation_target_with(target, &owner, None)
}

pub(super) fn register_native_recovery_target(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    owner: NativeRequestOwner,
) -> Result<(), String> {
    register_native_invocation_target_with(
        target
            .invocation_target()
            .ok_or("The recovery reservation is unavailable.")?,
        &owner,
        Some(target),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn register_startup_command_target(
    startup: &shell_contract::ipc::NativeInvocationOperation,
    shell: &NativeRequestOwner,
    target: &shell_contract::ipc::NativeInvocationTarget,
    expected: &shell_contract::ipc::NativeEditorCommand,
    owner: NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<(), String> {
    let original =
        same_startup_origin(startup, shell).ok_or("The startup Shell owner is unavailable.")?;
    let child = target.operation();
    if !child.is_child_of(startup)
        || !matches_native_command(child, expected)
        || owner.view != target.view()
        || !owner
            .context
            .renderer()
            .is_some_and(|renderer| renderer.same_generation(target.created()))
    {
        return Err("The native command target does not match its captured reservation.".into());
    }
    let mut captured = Vec::with_capacity(participants.len() + 2);
    captured.push(shell.clone());
    captured.push(original.clone());
    captured.extend_from_slice(participants);
    with_many_owner_generations_transition(
        &owner,
        &captured,
        None,
        &[
            startup.authority_generation(),
            child.authority_generation(),
            target.created(),
        ],
        &[
            startup.cancel(),
            startup.shutdown(),
            child.cancel(),
            child.shutdown(),
        ],
        cancel,
        pending,
        |_| target.register_from_native_admitted(owner.clone()),
    )
    .ok_or_else(|| "The startup command target is no longer available.".to_owned())?
}

fn register_native_invocation_target_with(
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &NativeRequestOwner,
    recovery: Option<&shell_contract::ipc::RecoveryOpenTarget>,
) -> Result<(), String> {
    if owner.view != target.view()
        || !owner
            .context
            .renderer()
            .is_some_and(|renderer| renderer.same_generation(target.created()))
    {
        return Err("The registered native target does not match its reservation.".into());
    }
    let operation = target.operation();
    let snapshot = recovery
        .map(|recovery| recovery.authority_snapshot())
        .transpose()?;
    let mut participants = Vec::new();
    let mut generations = vec![operation.authority_generation(), target.created()];
    let mut controls = vec![operation.cancel(), operation.shutdown()];
    if recovery.is_some() {
        if let Some(command) = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.command.as_ref())
        {
            let lineage = match snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.startup.as_ref())
            {
                Some(startup) => command.command.is_child_of(&startup.startup),
                None => command.command.is_root_native_command(),
            };
            if !lineage || !operation.is_child_of(&command.command) {
                return Err("The recovery child does not match its native command.".into());
            }
            participants.extend_from_slice(&command.participants);
            generations.push(command.command.authority_generation());
            controls.extend([
                command.command.cancel(),
                command.command.shutdown(),
                &command.cancel,
                &command.pending,
            ]);
        }
        if let Some(startup) = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.startup.as_ref())
        {
            let original = same_startup_origin(&startup.startup, &startup.shell)
                .ok_or("The startup Shell owner is no longer available.")?;
            let parent_matches = match snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.command.as_ref())
            {
                Some(command) => {
                    command.command.is_child_of(&startup.startup)
                        && operation.is_child_of(&command.command)
                }
                None => operation.is_child_of(&startup.startup),
            };
            if !parent_matches {
                return Err("The recovery child does not match its startup.".into());
            }
            participants.push(startup.shell.clone());
            participants.push(original.clone());
            participants.extend_from_slice(&startup.participants);
            generations.push(startup.startup.authority_generation());
            controls.extend([
                startup.startup.cancel(),
                startup.startup.shutdown(),
                &startup.cancel,
                &startup.pending,
            ]);
        }
    }
    with_many_owner_generations_transition(
        owner,
        &participants,
        None,
        &generations,
        &controls,
        operation.cancel(),
        operation.shutdown(),
        |_| match &snapshot {
            Some(snapshot) => snapshot.register_native_from_admitted(owner.clone()),
            None => target.register_from_native_admitted(owner.clone()),
        },
    )
    .ok_or_else(|| "The registered native target is no longer available.".to_owned())?
}

pub(crate) fn with_recovery_seed_effect<R>(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_recovery_pre_navigation_effect(target, None, None, None, effect)
}

pub(crate) fn with_recovery_seed_snapshot<R>(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_recovery_pre_navigation_snapshot(target, snapshot, None, None, None, effect)
}

pub(crate) fn commit_recovery_seed(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    enqueue: impl FnOnce() -> Result<(), String>,
) -> Result<shell_contract::ipc::RecoverySeedReceipt, String> {
    let mut notification = DeferredPreparation(None);
    with_recovery_pre_navigation_effect(target, None, None, None, || {
        let receipt = target.commit_seed_from_admitted(enqueue)?;
        notification.0 = Some(target.preparation_loss_token());
        Ok(receipt)
    })
    .ok_or_else(|| "The recovery preparation is no longer available.".to_owned())?
}

pub(crate) fn commit_recovery_seed_snapshot(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    enqueue: impl FnOnce() -> Result<(), String>,
) -> Result<shell_contract::ipc::RecoverySeedReceipt, String> {
    let mut notification = DeferredPreparation(None);
    with_recovery_pre_navigation_snapshot(target, snapshot, None, None, None, || {
        let receipt = target.commit_seed_from_admitted(enqueue)?;
        notification.0 = Some(target.preparation_loss_token());
        Ok(receipt)
    })
    .ok_or_else(|| "The recovery preparation is no longer available.".to_owned())?
}

pub(super) fn with_recovery_seeded_effect<R>(
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    use shell_contract::ipc::RecoveryOpenContext;
    let target = receipt.target();
    let snapshot = target.authority_snapshot().ok()?;
    if let (Some(startup), Some(command)) = (&snapshot.startup, &snapshot.command) {
        matches_startup_snapshot_origin(&startup.startup, &startup.shell)?;
        let invocation = target.invocation_target()?;
        let child = invocation.operation();
        if !command.command.is_child_of(&startup.startup) || !child.is_child_of(&command.command) {
            return None;
        }
        let mut participants = startup.participants.clone();
        participants.extend_from_slice(&command.participants);
        return with_many_owner_generations_transition(
            &startup.shell,
            &participants,
            None,
            &[
                startup.startup.authority_generation(),
                command.command.authority_generation(),
                child.authority_generation(),
                target.created(),
            ],
            &[
                startup.startup.cancel(),
                startup.startup.shutdown(),
                command.command.cancel(),
                command.command.shutdown(),
                child.cancel(),
                child.shutdown(),
                &command.cancel,
                &command.pending,
            ],
            &startup.cancel,
            &startup.pending,
            |_| {
                invocation
                    .with_reserved_state(|| {
                        snapshot
                            .with_current(|| {
                                if receipt.matches(target) {
                                    Some(effect())
                                } else {
                                    None
                                }
                            })
                            .flatten()
                    })
                    .flatten()
            },
        )
        .flatten();
    }
    if let Some(startup) = snapshot.startup.as_ref() {
        return with_startup_recovery_seeded_snapshot(
            &startup.startup,
            &startup.shell,
            receipt,
            &snapshot,
            &startup.participants,
            &startup.cancel,
            &startup.pending,
            effect,
        );
    }
    if let Some(command) = snapshot.command.as_ref() {
        let invocation = target.invocation_target()?;
        let child = invocation.operation();
        if !command.command.is_root_native_command() || !child.is_child_of(&command.command) {
            return None;
        }
        let generations = [
            command.command.authority_generation(),
            child.authority_generation(),
            target.created(),
        ];
        let tokens = [
            command.command.cancel(),
            command.command.shutdown(),
            child.cancel(),
            child.shutdown(),
            &command.cancel,
            &command.pending,
        ];
        let initialize = || {
            invocation
                .with_reserved_state(|| {
                    snapshot
                        .with_current(|| {
                            if receipt.matches(target) {
                                Some(effect())
                            } else {
                                None
                            }
                        })
                        .flatten()
                })
                .flatten()
        };
        return if let Some((origin, rest)) = command.participants.split_first() {
            with_many_owner_generations_transition(
                origin,
                rest,
                None,
                &generations,
                &tokens,
                &command.cancel,
                &command.pending,
                |_| initialize(),
            )
            .flatten()
        } else {
            RendererGeneration::with_active_all(generations.map(Some), || {
                CancelToken::with_active_all(
                    [
                        Some(tokens[0]),
                        Some(tokens[1]),
                        Some(tokens[2]),
                        Some(tokens[3]),
                        Some(tokens[4]),
                        Some(tokens[5]),
                    ],
                    initialize,
                )
            })
            .flatten()
            .flatten()
        };
    }
    let initialize = || {
        snapshot
            .with_current(|| {
                if receipt.matches(target) {
                    Some(effect())
                } else {
                    None
                }
            })
            .flatten()
    };
    match &snapshot.context {
        RecoveryOpenContext::NativeStartup(_) => target
            .invocation_target()?
            .with_active(initialize)
            .flatten(),
        RecoveryOpenContext::Renderer { .. } => target.created().with_active(initialize).flatten(),
    }
}

pub(crate) fn with_native_invocation_operation_effect<R>(
    operation: &shell_contract::ipc::NativeInvocationOperation,
    origin: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_many_owner_generations_transition(
        origin,
        participants,
        None,
        &[operation.authority_generation()],
        &[operation.cancel(), operation.shutdown()],
        cancel,
        pending,
        |_| effect(),
    )
}

pub(crate) fn with_existing_document_save_many_effect<R>(
    binding: &shell_contract::ipc::NativeExistingDocumentBinding,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let operation = binding.operation();
    let mut controls = vec![operation.cancel(), operation.shutdown()];
    controls.extend_from_slice(additional);
    with_many_owner_generations_transition(
        binding.owner(),
        participants,
        None,
        &[operation.authority_generation()],
        &controls,
        cancel,
        pending,
        |_| effect(),
    )
}

pub(crate) fn with_native_save_revision_snapshot<R>(
    snapshot: &shell_contract::ipc::NativeSaveRevisionSnapshot,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let mut owners = snapshot.aliases().to_vec();
    owners.extend_from_slice(participants);
    with_existing_document_save_many_effect(
        snapshot.selection().binding(),
        &owners,
        cancel,
        pending,
        additional,
        || snapshot.with_current(effect),
    )
    .flatten()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn with_existing_document_service_effect<R>(
    binding: &shell_contract::ipc::NativeExistingDocumentBinding,
    participants: &[NativeRequestOwner],
    service: &crate::native::DocumentContext,
    generation: &RendererGeneration,
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let operation = binding.operation();
    let mut controls = vec![operation.cancel(), operation.shutdown()];
    controls.extend_from_slice(additional);
    with_many_owner_generations_transition(
        binding.owner(),
        participants,
        Some(service),
        &[operation.authority_generation(), generation],
        &controls,
        cancel,
        pending,
        |_| effect(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn with_existing_document_print_effect<R>(
    binding: &shell_contract::ipc::NativeExistingDocumentBinding,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    stop: &CancelToken,
    print_generation: &RendererGeneration,
    additional: &[&CancelToken],
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let operation = binding.operation();
    let mut controls = vec![operation.cancel(), operation.shutdown(), stop];
    controls.extend_from_slice(additional);
    with_many_owner_generations_transition(
        binding.owner(),
        participants,
        None,
        &[operation.authority_generation(), print_generation],
        &controls,
        cancel,
        pending,
        |_| effect(),
    )
}

fn matches_native_command(
    operation: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
) -> bool {
    !matches!(
        expected,
        shell_contract::ipc::NativeEditorCommand::SaveDocument { .. }
    ) && operation.is_native_command()
        && matches!(operation.selection(), shell_contract::ipc::NativeInvocationSelection::NativeCommand(command) if command == expected)
}

pub(crate) fn with_command_recovery_preparation_effect<R>(
    command: &shell_contract::ipc::NativeInvocationOperation,
    child: &shell_contract::ipc::NativeInvocationOperation,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    if !command.is_root_native_command()
        || !child.is_child_of(command)
        || child.purpose() != shell_contract::ipc::NativeInvocationPurpose::Recovery
    {
        return None;
    }
    if let Some((origin, rest)) = participants.split_first() {
        with_many_owner_generations_transition(
            origin,
            rest,
            None,
            &[command.authority_generation(), child.authority_generation()],
            &[
                command.cancel(),
                command.shutdown(),
                child.cancel(),
                child.shutdown(),
            ],
            cancel,
            pending,
            |_| effect(),
        )
    } else {
        RendererGeneration::with_active_all(
            [
                Some(command.authority_generation()),
                Some(child.authority_generation()),
            ],
            || {
                CancelToken::with_active_all(
                    [
                        Some(command.cancel()),
                        Some(command.shutdown()),
                        Some(child.cancel()),
                        Some(child.shutdown()),
                        Some(cancel),
                        Some(pending),
                    ],
                    effect,
                )
            },
        )
        .flatten()
    }
}
pub(crate) fn with_command_recovery_target_effect<R>(
    command: &shell_contract::ipc::NativeInvocationOperation,
    child: &shell_contract::ipc::NativeInvocationDocument,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let operation = child.operation();
    if !command.is_root_native_command()
        || !operation.is_child_of(command)
        || operation.purpose() != shell_contract::ipc::NativeInvocationPurpose::Recovery
    {
        return None;
    }
    let registered = same_registered_origin(child.target(), owner)?;
    with_many_owner_generations_transition(
        owner,
        participants,
        None,
        &[
            command.authority_generation(),
            operation.authority_generation(),
            child.created(),
        ],
        &[
            command.cancel(),
            command.shutdown(),
            operation.cancel(),
            operation.shutdown(),
        ],
        cancel,
        pending,
        |_| {
            child
                .target()
                .with_registered_state(registered.owner(), effect)
        },
    )
    .flatten()
}

/// Accept a captured native preparation before its constructor generation exists.
pub(crate) fn with_native_command_preparation_effect<R>(
    operation: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    if !operation.is_root_native_command() || !matches_native_command(operation, expected) {
        return None;
    }
    if let Some((origin, rest)) = participants.split_first() {
        with_many_owner_generations_transition(
            origin,
            rest,
            None,
            &[operation.authority_generation()],
            &[operation.cancel(), operation.shutdown()],
            cancel,
            pending,
            |_| effect(),
        )
    } else {
        RendererGeneration::with_active_all([Some(operation.authority_generation())], || {
            CancelToken::with_active_all(
                [
                    Some(cancel),
                    Some(pending),
                    Some(operation.cancel()),
                    Some(operation.shutdown()),
                ],
                effect,
            )
        })
        .flatten()
    }
}
/// Admit a short immutable engine request against the exact command, service and
/// captured participants. The returned action must run after this gate releases.
#[allow(clippy::too_many_arguments)]
pub(crate) fn with_command_preparation_service_effect<R>(
    operation: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
    startup: Option<(
        &shell_contract::ipc::NativeInvocationOperation,
        &NativeRequestOwner,
    )>,
    participants: &[NativeRequestOwner],
    service: &crate::native::DocumentContext,
    service_generation: &RendererGeneration,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    if !matches_native_command(operation, expected) {
        return None;
    }
    if let Some((root, shell)) = startup {
        let original = same_startup_origin(root, shell)?;
        if !operation.is_child_of(root) {
            return None;
        }
        let mut captured = Vec::with_capacity(participants.len() + 1);
        captured.push(original.clone());
        captured.extend_from_slice(participants);
        with_many_owner_generations_transition(
            shell,
            &captured,
            Some(service),
            &[
                root.authority_generation(),
                operation.authority_generation(),
                service_generation,
            ],
            &[
                root.cancel(),
                root.shutdown(),
                operation.cancel(),
                operation.shutdown(),
            ],
            cancel,
            pending,
            |_| effect(),
        )
    } else {
        if !operation.is_root_native_command() {
            return None;
        }
        if let Some((origin, rest)) = participants.split_first() {
            with_many_owner_generations_transition(
                origin,
                rest,
                Some(service),
                &[operation.authority_generation(), service_generation],
                &[operation.cancel(), operation.shutdown()],
                cancel,
                pending,
                |_| effect(),
            )
        } else {
            crate::native::DocumentContext::with_live_all([Some(service)], || {
                RendererGeneration::with_active_all(
                    [
                        Some(operation.authority_generation()),
                        Some(service_generation),
                    ],
                    || {
                        CancelToken::with_active_all(
                            [
                                Some(cancel),
                                Some(pending),
                                Some(operation.cancel()),
                                Some(operation.shutdown()),
                            ],
                            effect,
                        )
                    },
                )
                .flatten()
            })
            .flatten()
        }
    }
}

/// Admit only an immutable native constructor action. Execute it after all gates drop.
pub(crate) fn with_native_command_creation_effect<R>(
    operation: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
    created: &RendererGeneration,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    if !operation.is_root_native_command() || !matches_native_command(operation, expected) {
        return None;
    }
    if let Some((origin, rest)) = participants.split_first() {
        with_many_owner_generations_transition(
            origin,
            rest,
            None,
            &[operation.authority_generation(), created],
            &[operation.cancel(), operation.shutdown()],
            cancel,
            pending,
            |_| effect(),
        )
    } else {
        RendererGeneration::with_active_all(
            [Some(operation.authority_generation()), Some(created)],
            || {
                CancelToken::with_active_all(
                    [
                        Some(cancel),
                        Some(pending),
                        Some(operation.cancel()),
                        Some(operation.shutdown()),
                    ],
                    effect,
                )
            },
        )
        .flatten()
    }
}

/// Admit an effect for the same command after its actual first Registered target.
#[allow(clippy::too_many_arguments)]
pub(crate) fn with_native_command_target_effect<R>(
    operation: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
    document: &shell_contract::ipc::NativeInvocationDocument,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    if !operation.is_root_native_command()
        || !matches_native_command(operation, expected)
        || !operation
            .authority_generation()
            .same_generation(document.operation().authority_generation())
    {
        return None;
    }
    let registered = same_registered_origin(document.target(), owner)?;
    with_many_owner_generations_transition(
        owner,
        participants,
        None,
        &[operation.authority_generation(), document.created()],
        &[operation.cancel(), operation.shutdown()],
        cancel,
        pending,
        |_| {
            document
                .target()
                .with_registered_state(registered.owner(), effect)
        },
    )
    .flatten()
}

pub(crate) fn with_startup_operation_effect<R>(
    operation: &shell_contract::ipc::NativeInvocationOperation,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let original = same_startup_origin(operation, owner)?;
    let mut captured = Vec::with_capacity(participants.len() + 1);
    captured.push(original.clone());
    captured.extend_from_slice(participants);
    with_many_owner_generations_transition(
        owner,
        &captured,
        None,
        &[operation.authority_generation()],
        &[operation.cancel(), operation.shutdown()],
        cancel,
        pending,
        |_| effect(),
    )
}

fn same_startup_origin<'a>(
    operation: &'a shell_contract::ipc::NativeInvocationOperation,
    owner: &NativeRequestOwner,
) -> Option<&'a NativeRequestOwner> {
    if operation.purpose() != shell_contract::ipc::NativeInvocationPurpose::StartupLaunch {
        return None;
    }
    let original = operation.startup_owner()?;
    let same_generation =
        |a: Option<&RendererGeneration>, b: Option<&RendererGeneration>| match (a, b) {
            (Some(a), Some(b)) => a.same_generation(b),
            (None, None) => true,
            _ => false,
        };
    let same_pdf = |a: Option<&PdfDocumentLease>, b: Option<&PdfDocumentLease>| match (a, b) {
        (Some(a), Some(b)) => a.same_document(b),
        (None, None) => true,
        _ => false,
    };
    if owner.view != original.view
        || proof(owner)?.origin != proof(original)?.origin
        || !same_generation(owner.context.renderer(), original.context.renderer())
        || !same_generation(
            owner.context.document_lease(),
            original.context.document_lease(),
        )
        || !same_pdf(owner.pdf_document(), original.pdf_document())
        || !same_pdf(owner.pdf_revision(), original.pdf_revision())
    {
        return None;
    }
    Some(original)
}

// Only use with a current retained recovery snapshot. Its revision successor was
// installed atomically; all other native/document identity remains immutable.
fn matches_startup_snapshot_origin(
    operation: &shell_contract::ipc::NativeInvocationOperation,
    owner: &NativeRequestOwner,
) -> Option<()> {
    if operation.purpose() != shell_contract::ipc::NativeInvocationPurpose::StartupLaunch {
        return None;
    }
    let original = operation.startup_owner()?;
    let same_generation =
        |a: Option<&RendererGeneration>, b: Option<&RendererGeneration>| match (a, b) {
            (Some(a), Some(b)) => a.same_generation(b),
            (None, None) => true,
            _ => false,
        };
    let same_pdf = |a: Option<&PdfDocumentLease>, b: Option<&PdfDocumentLease>| match (a, b) {
        (Some(a), Some(b)) => a.same_document(b),
        (None, None) => true,
        _ => false,
    };
    if owner.view != original.view
        || proof(owner)?.origin != proof(original)?.origin
        || !same_generation(owner.context.renderer(), original.context.renderer())
        || !same_generation(
            owner.context.document_lease(),
            original.context.document_lease(),
        )
        || !same_pdf(owner.pdf_document(), original.pdf_document())
    {
        return None;
    }
    Some(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn with_startup_command_preparation_effect<R>(
    startup: &shell_contract::ipc::NativeInvocationOperation,
    shell: &NativeRequestOwner,
    child: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let original = same_startup_origin(startup, shell)?;
    if !child.is_child_of(startup) || !matches_native_command(child, expected) {
        return None;
    }
    let mut captured = Vec::with_capacity(participants.len() + 1);
    captured.push(original.clone());
    captured.extend_from_slice(participants);
    with_many_owner_generations_transition(
        shell,
        &captured,
        None,
        &[startup.authority_generation(), child.authority_generation()],
        &[
            startup.cancel(),
            startup.shutdown(),
            child.cancel(),
            child.shutdown(),
        ],
        cancel,
        pending,
        |_| effect(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn with_startup_command_creation_effect<R>(
    startup: &shell_contract::ipc::NativeInvocationOperation,
    shell: &NativeRequestOwner,
    child: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
    created: &RendererGeneration,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let original = same_startup_origin(startup, shell)?;
    if !child.is_child_of(startup) || !matches_native_command(child, expected) {
        return None;
    }
    let mut captured = Vec::with_capacity(participants.len() + 1);
    captured.push(original.clone());
    captured.extend_from_slice(participants);
    with_many_owner_generations_transition(
        shell,
        &captured,
        None,
        &[
            startup.authority_generation(),
            child.authority_generation(),
            created,
        ],
        &[
            startup.cancel(),
            startup.shutdown(),
            child.cancel(),
            child.shutdown(),
        ],
        cancel,
        pending,
        |_| effect(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn with_startup_command_target_effect<R>(
    startup: &shell_contract::ipc::NativeInvocationOperation,
    shell: &NativeRequestOwner,
    child: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
    document: &shell_contract::ipc::NativeInvocationDocument,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let original = same_startup_origin(startup, shell)?;
    if !child.is_child_of(startup)
        || !matches_native_command(child, expected)
        || !child
            .authority_generation()
            .same_generation(document.operation().authority_generation())
    {
        return None;
    }
    let registered = same_registered_origin(document.target(), owner)?;
    let mut captured = Vec::with_capacity(participants.len() + 2);
    captured.push(shell.clone());
    captured.push(original.clone());
    captured.extend_from_slice(participants);
    with_many_owner_generations_transition(
        owner,
        &captured,
        None,
        &[
            startup.authority_generation(),
            child.authority_generation(),
            document.created(),
        ],
        &[
            startup.cancel(),
            startup.shutdown(),
            child.cancel(),
            child.shutdown(),
        ],
        cancel,
        pending,
        |_| {
            document
                .target()
                .with_registered_state(registered.owner(), effect)
        },
    )
    .flatten()
}

pub(crate) fn with_startup_recovery_preparation_effect<R>(
    startup: &shell_contract::ipc::NativeInvocationOperation,
    shell: &NativeRequestOwner,
    child: &shell_contract::ipc::NativeInvocationOperation,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let original = same_startup_origin(startup, shell)?;
    if !child.is_child_of(startup)
        || child.purpose() != shell_contract::ipc::NativeInvocationPurpose::Recovery
    {
        return None;
    }
    let mut captured = Vec::with_capacity(participants.len() + 1);
    captured.push(original.clone());
    captured.extend_from_slice(participants);
    with_many_owner_generations_transition(
        shell,
        &captured,
        None,
        &[startup.authority_generation(), child.authority_generation()],
        &[
            startup.cancel(),
            startup.shutdown(),
            child.cancel(),
            child.shutdown(),
        ],
        cancel,
        pending,
        |_| effect(),
    )
}

pub(crate) fn with_startup_command_recovery_preparation_effect<R>(
    startup: &shell_contract::ipc::StartupRecoveryConstraint,
    command: &shell_contract::ipc::CommandRecoveryConstraint,
    child: &shell_contract::ipc::NativeInvocationOperation,
    request: Option<&shell_contract::ipc::RecoveryRequestConstraint>,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let original = same_startup_origin(&startup.startup, &startup.shell)?;
    if !command.command.is_child_of(&startup.startup)
        || !child.is_child_of(&command.command)
        || child.purpose() != shell_contract::ipc::NativeInvocationPurpose::Recovery
        || !matches!(command.command.selection(), shell_contract::ipc::NativeInvocationSelection::NativeCommand(
            shell_contract::ipc::NativeEditorCommand::OpenFile { surface, .. }) if *surface == child.selection().surface())
    {
        return None;
    }
    let mut captured = Vec::with_capacity(
        startup.participants.len()
            + command.participants.len()
            + 1
            + request.map_or(0, |request| request.participants.len() + 1),
    );
    captured.push(original.clone());
    captured.extend_from_slice(&startup.participants);
    captured.extend_from_slice(&command.participants);
    let mut controls = vec![
        startup.startup.cancel(),
        startup.startup.shutdown(),
        command.command.cancel(),
        command.command.shutdown(),
        child.cancel(),
        child.shutdown(),
        &command.cancel,
        &command.pending,
    ];
    if let Some(request) = request {
        captured.push(request.source.clone());
        captured.extend_from_slice(&request.participants);
        controls.extend([&request.cancel, &request.pending]);
    }
    with_many_owner_generations_transition(
        &startup.shell,
        &captured,
        None,
        &[
            startup.startup.authority_generation(),
            command.command.authority_generation(),
            child.authority_generation(),
        ],
        &controls,
        &startup.cancel,
        &startup.pending,
        |_| effect(),
    )
}

/// The immutable child constructor is accepted only under its sealed startup root.
#[allow(clippy::too_many_arguments)]
pub(crate) fn with_startup_recovery_creation_effect<R>(
    startup: &shell_contract::ipc::NativeInvocationOperation,
    shell: &NativeRequestOwner,
    child: &shell_contract::ipc::NativeInvocationOperation,
    created: &RendererGeneration,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let original = same_startup_origin(startup, shell)?;
    if !child.is_child_of(startup)
        || child.purpose() != shell_contract::ipc::NativeInvocationPurpose::Recovery
    {
        return None;
    }
    let mut captured = Vec::with_capacity(participants.len() + 1);
    captured.push(original.clone());
    captured.extend_from_slice(participants);
    with_many_owner_generations_transition(
        shell,
        &captured,
        None,
        &[
            startup.authority_generation(),
            child.authority_generation(),
            created,
        ],
        &[
            startup.cancel(),
            startup.shutdown(),
            child.cancel(),
            child.shutdown(),
        ],
        cancel,
        pending,
        |_| effect(),
    )
}

pub(crate) fn with_startup_recovery_seed_effect<R>(
    startup: &shell_contract::ipc::NativeInvocationOperation,
    shell: &NativeRequestOwner,
    target: &shell_contract::ipc::NativeInvocationTarget,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_startup_recovery_creation_effect(
        startup,
        shell,
        target.operation(),
        target.created(),
        participants,
        cancel,
        pending,
        || target.with_reserved_state(effect),
    )
    .flatten()
}

#[allow(clippy::too_many_arguments)]
fn with_startup_recovery_seeded_snapshot<R>(
    startup: &shell_contract::ipc::NativeInvocationOperation,
    shell: &NativeRequestOwner,
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let target = receipt.target();
    if !receipt.matches(target) {
        return None;
    }
    let invocation = target.invocation_target()?;
    let child = invocation.operation();
    let retained = snapshot.startup.as_ref()?;
    if !startup
        .authority_generation()
        .same_generation(retained.startup.authority_generation())
        || shell.view != retained.shell.view
        || proof(shell)?.origin != proof(&retained.shell)?.origin
        || !shell
            .loss_token()?
            .same_token(&retained.shell.loss_token()?)
    {
        return None;
    }
    matches_startup_snapshot_origin(startup, shell)?;
    if !child.is_child_of(startup)
        || child.purpose() != shell_contract::ipc::NativeInvocationPurpose::Recovery
    {
        return None;
    }
    let controls = [
        startup.cancel(),
        startup.shutdown(),
        child.cancel(),
        child.shutdown(),
    ];
    with_many_owner_generations_transition(
        shell,
        participants,
        None,
        &[
            startup.authority_generation(),
            child.authority_generation(),
            target.created(),
        ],
        &controls,
        cancel,
        pending,
        |_| {
            invocation
                .with_reserved_state(|| {
                    snapshot
                        .with_current(|| {
                            if receipt.matches(target) {
                                Some(effect())
                            } else {
                                None
                            }
                        })
                        .flatten()
                })
                .flatten()
        },
    )
    .flatten()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn with_startup_recovery_effect<R>(
    startup: &shell_contract::ipc::NativeInvocationOperation,
    shell: &NativeRequestOwner,
    child: &shell_contract::ipc::NativeInvocationDocument,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let original_shell = same_startup_origin(startup, shell)?;
    let registered = same_registered_origin(child.target(), owner)?;
    let child_operation = child.operation();
    if child_operation.purpose() != shell_contract::ipc::NativeInvocationPurpose::Recovery
        || !child_operation.is_child_of(startup)
    {
        return None;
    }
    let mut captured = Vec::with_capacity(participants.len() + 2);
    captured.push(shell.clone());
    captured.push(original_shell.clone());
    captured.extend_from_slice(participants);
    with_many_owner_generations_transition(
        owner,
        &captured,
        None,
        &[
            startup.authority_generation(),
            child_operation.authority_generation(),
            child.created(),
        ],
        &[
            startup.cancel(),
            startup.shutdown(),
            child_operation.cancel(),
            child_operation.shutdown(),
        ],
        cancel,
        pending,
        |_| {
            child
                .target()
                .with_registered_state(registered.owner(), effect)
        },
    )
    .flatten()
}

pub(crate) fn with_recovery_dialog_effect<R>(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    dialog: &NativeRequestOwner,
    created: &RendererGeneration,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    if !dialog.context.renderer()?.same_generation(created) {
        return None;
    }
    with_recovery_pre_navigation_effect(
        target,
        Some(created),
        Some(dialog),
        Some((cancel, pending)),
        effect,
    )
}

fn with_recovery_pre_navigation_effect<R>(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    created: Option<&RendererGeneration>,
    dialog: Option<&NativeRequestOwner>,
    controls: Option<(&CancelToken, &CancelToken)>,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let snapshot = target.authority_snapshot().ok()?;
    with_recovery_pre_navigation_snapshot(target, &snapshot, created, dialog, controls, effect)
}

pub(crate) fn with_recovery_pre_navigation_snapshot<R>(
    target: &shell_contract::ipc::RecoveryOpenTarget,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    created: Option<&RendererGeneration>,
    dialog: Option<&NativeRequestOwner>,
    controls: Option<(&CancelToken, &CancelToken)>,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    use shell_contract::ipc::RecoveryOpenContext;
    if !snapshot.belongs_to(target) {
        return None;
    }
    let extra_created = created.unwrap_or(target.created());
    let seed = || {
        snapshot
            .with_current(|| {
                if snapshot.registered.is_none() && !target.is_seed_committed() {
                    Some(effect())
                } else {
                    None
                }
            })
            .flatten()
    };
    match &snapshot.context {
        RecoveryOpenContext::NativeStartup(_) => {
            let invocation = target.invocation_target()?;
            let operation = invocation.operation();
            let cancel = controls.map_or(operation.cancel(), |controls| controls.0);
            let pending = controls.map_or(operation.shutdown(), |controls| controls.1);
            let final_effect = || invocation.with_reserved_state(seed).flatten();
            let request = snapshot.request.as_ref();
            if let (Some(startup), Some(command)) = (&snapshot.startup, &snapshot.command) {
                let original = same_startup_origin(&startup.startup, &startup.shell)?;
                if !command.command.is_child_of(&startup.startup)
                    || !operation.is_child_of(&command.command)
                {
                    return None;
                }
                let mut captured =
                    Vec::with_capacity(startup.participants.len() + command.participants.len() + 2);
                captured.push(original.clone());
                captured.extend_from_slice(&startup.participants);
                captured.extend_from_slice(&command.participants);
                captured.extend(dialog.cloned());
                let mut tokens = vec![
                    startup.startup.cancel(),
                    startup.startup.shutdown(),
                    command.command.cancel(),
                    command.command.shutdown(),
                    operation.cancel(),
                    operation.shutdown(),
                    &startup.cancel,
                    &startup.pending,
                    &command.cancel,
                    &command.pending,
                ];
                if let Some(request) = request {
                    captured.push(request.source.clone());
                    captured.extend_from_slice(&request.participants);
                    tokens.extend([&request.cancel, &request.pending]);
                }
                return with_many_owner_generations_transition(
                    &startup.shell,
                    &captured,
                    None,
                    &[
                        startup.startup.authority_generation(),
                        command.command.authority_generation(),
                        operation.authority_generation(),
                        target.created(),
                        extra_created,
                    ],
                    &tokens,
                    cancel,
                    pending,
                    |_| final_effect(),
                )
                .flatten();
            }
            if let Some(command) = snapshot.command.as_ref() {
                if !command.command.is_root_native_command()
                    || !operation.is_child_of(&command.command)
                {
                    return None;
                }
                let mut captured = command.participants.clone();
                captured.extend(dialog.cloned());
                let generations = [
                    command.command.authority_generation(),
                    operation.authority_generation(),
                    target.created(),
                    extra_created,
                ];
                let tokens = [
                    command.command.cancel(),
                    command.command.shutdown(),
                    operation.cancel(),
                    operation.shutdown(),
                    &command.cancel,
                    &command.pending,
                ];
                return if let Some((origin, rest)) = captured.split_first() {
                    with_many_owner_generations_transition(
                        origin,
                        rest,
                        None,
                        &generations,
                        &tokens,
                        cancel,
                        pending,
                        |_| final_effect(),
                    )
                    .flatten()
                } else {
                    RendererGeneration::with_active_all(generations.map(Some), || {
                        CancelToken::with_active_all(
                            [
                                Some(tokens[0]),
                                Some(tokens[1]),
                                Some(tokens[2]),
                                Some(tokens[3]),
                                Some(tokens[4]),
                                Some(tokens[5]),
                                Some(cancel),
                                Some(pending),
                            ],
                            final_effect,
                        )
                    })
                    .flatten()
                    .flatten()
                };
            }
            if let Some(startup) = snapshot.startup.as_ref() {
                let original = same_startup_origin(&startup.startup, &startup.shell)?;
                if !operation.is_child_of(&startup.startup) {
                    return None;
                }
                let mut captured = Vec::with_capacity(
                    startup.participants.len()
                        + 2
                        + request.map_or(0, |request| request.participants.len() + 1),
                );
                captured.push(original.clone());
                captured.extend_from_slice(&startup.participants);
                captured.extend(dialog.cloned());
                if let Some(request) = request {
                    captured.push(request.source.clone());
                    captured.extend_from_slice(&request.participants);
                }
                let mut tokens = vec![
                    startup.startup.cancel(),
                    startup.startup.shutdown(),
                    operation.cancel(),
                    operation.shutdown(),
                    &startup.cancel,
                    &startup.pending,
                ];
                if let Some(request) = request {
                    tokens.extend([&request.cancel, &request.pending]);
                }
                return with_many_owner_generations_transition(
                    &startup.shell,
                    &captured,
                    None,
                    &[
                        startup.startup.authority_generation(),
                        operation.authority_generation(),
                        target.created(),
                        extra_created,
                    ],
                    &tokens,
                    cancel,
                    pending,
                    |_| final_effect(),
                )
                .flatten();
            }
            let mut captured;
            let participants = match request {
                Some(request) => {
                    captured = Vec::with_capacity(
                        request.participants.len() + usize::from(dialog.is_some()),
                    );
                    captured.extend_from_slice(&request.participants);
                    captured.extend(dialog.cloned());
                    captured.as_slice()
                }
                None => &[],
            };
            let generations = [
                operation.authority_generation(),
                target.created(),
                extra_created,
            ];
            match (request, dialog) {
                (Some(request), _) => with_many_owner_generations_transition(
                    &request.source,
                    participants,
                    None,
                    &generations,
                    &[operation.cancel(), operation.shutdown(), cancel, pending],
                    &request.cancel,
                    &request.pending,
                    |_| final_effect(),
                )
                .flatten(),
                (None, Some(dialog)) => with_many_owner_generations_transition(
                    dialog,
                    &[],
                    None,
                    &generations,
                    &[operation.cancel(), operation.shutdown()],
                    cancel,
                    pending,
                    |_| final_effect(),
                )
                .flatten(),
                (None, None) => RendererGeneration::with_active_all(generations.map(Some), || {
                    CancelToken::with_active_all(
                        [
                            Some(operation.cancel()),
                            Some(operation.shutdown()),
                            Some(cancel),
                            Some(pending),
                        ],
                        final_effect,
                    )
                    .flatten()
                })
                .flatten(),
            }
        }
        RecoveryOpenContext::Renderer {
            source,
            participants,
            cancel,
            pending,
        } => {
            let extra_controls = controls.unwrap_or((cancel, pending));
            let request = snapshot.request.as_ref();
            let mut captured = Vec::with_capacity(
                participants.len()
                    + usize::from(dialog.is_some())
                    + request.map_or(0, |request| request.participants.len() + 1),
            );
            captured.extend_from_slice(participants);
            captured.extend(dialog.cloned());
            if let Some(request) = request {
                captured.push(request.source.clone());
                captured.extend_from_slice(&request.participants);
            }
            let request_controls = request.map_or((cancel, pending), |request| {
                (&request.cancel, &request.pending)
            });
            with_many_owner_generations_transition(
                source,
                &captured,
                None,
                &[target.created(), extra_created],
                &[
                    extra_controls.0,
                    extra_controls.1,
                    request_controls.0,
                    request_controls.1,
                ],
                cancel,
                pending,
                |_| seed(),
            )
            .flatten()
        }
    }
}

pub(crate) fn with_recovery_open_effect<R>(
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    receiver: &NativeRequestOwner,
    cancel: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_recovery_open_service_effect(receipt, receiver, &[], None, None, cancel, None, effect)
}

pub(crate) fn with_service_recovery_open_effect<R>(
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    receiver: &NativeRequestOwner,
    service: &crate::native::DocumentContext,
    service_generation: &RendererGeneration,
    cancel: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_recovery_open_service_effect(
        receipt,
        receiver,
        &[],
        Some(service),
        Some(service_generation),
        cancel,
        None,
        effect,
    )
}

#[allow(clippy::too_many_arguments)]
fn with_recovery_open_service_effect<R>(
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    receiver: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    service: Option<&crate::native::DocumentContext>,
    service_generation: Option<&RendererGeneration>,
    cancel: &CancelToken,
    pending: Option<&CancelToken>,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_recovery_open_service_effect_extra(
        receipt,
        receiver,
        participants,
        service,
        service_generation,
        cancel,
        pending,
        &[],
        effect,
    )
}

#[allow(clippy::too_many_arguments)]
fn with_recovery_open_service_effect_extra<R>(
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    receiver: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    service: Option<&crate::native::DocumentContext>,
    service_generation: Option<&RendererGeneration>,
    cancel: &CancelToken,
    pending: Option<&CancelToken>,
    additional: &[&CancelToken],
    effect: impl FnOnce() -> R,
) -> Option<R> {
    let snapshot = receipt.target().authority_snapshot().ok()?;
    with_recovery_open_service_snapshot(
        receipt,
        &snapshot,
        receiver,
        participants,
        service,
        service_generation,
        cancel,
        pending,
        additional,
        effect,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn with_recovery_open_service_snapshot<R>(
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
    receiver: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    service: Option<&crate::native::DocumentContext>,
    service_generation: Option<&RendererGeneration>,
    cancel: &CancelToken,
    pending: Option<&CancelToken>,
    additional: &[&CancelToken],
    effect: impl FnOnce() -> R,
) -> Option<R> {
    use shell_contract::ipc::RecoveryOpenContext;
    let target = receipt.target();
    if !receipt.matches(target) || !snapshot.belongs_to(target) {
        return None;
    }
    let registered = snapshot.registered.as_ref()?;
    if receiver.view != target.view()
        || registered.view != receiver.view
        || proof(receiver)?.origin != proof(registered)?.origin
        || !receiver
            .context
            .renderer()?
            .same_generation(target.created())
        || !registered
            .context
            .renderer()?
            .same_generation(target.created())
    {
        return None;
    }
    let apply = || snapshot.with_current(effect);
    if let (Some(startup), Some(command)) = (&snapshot.startup, &snapshot.command) {
        matches_startup_snapshot_origin(&startup.startup, &startup.shell)?;
        let invocation = target.invocation_target()?;
        let child = invocation.operation();
        if !command.command.is_child_of(&startup.startup) || !child.is_child_of(&command.command) {
            return None;
        }
        let first_registered = same_registered_origin(invocation, receiver)?;
        let mut captured = Vec::with_capacity(
            participants.len() + startup.participants.len() + command.participants.len() + 1,
        );
        captured.extend_from_slice(participants);
        captured.push(startup.shell.clone());
        captured.extend_from_slice(&startup.participants);
        captured.extend_from_slice(&command.participants);
        let mut controls = vec![
            startup.startup.cancel(),
            startup.startup.shutdown(),
            command.command.cancel(),
            command.command.shutdown(),
            child.cancel(),
            child.shutdown(),
            &startup.cancel,
            &startup.pending,
            &command.cancel,
            &command.pending,
        ];
        controls.extend_from_slice(additional);
        let service_generation = service_generation.unwrap_or(target.created());
        return with_many_owner_generations_transition(
            receiver,
            &captured,
            service,
            &[
                startup.startup.authority_generation(),
                command.command.authority_generation(),
                child.authority_generation(),
                target.created(),
                service_generation,
            ],
            &controls,
            cancel,
            pending.unwrap_or(child.shutdown()),
            |_| invocation.with_registered_state(first_registered.owner(), apply),
        )
        .flatten()
        .flatten();
    }
    if let Some(command) = snapshot.command.as_ref() {
        let invocation = target.invocation_target()?;
        let child = invocation.operation();
        if !command.command.is_root_native_command() || !child.is_child_of(&command.command) {
            return None;
        }
        let first_registered = same_registered_origin(invocation, receiver)?;
        let mut captured = Vec::with_capacity(participants.len() + command.participants.len());
        captured.extend_from_slice(participants);
        captured.extend_from_slice(&command.participants);
        let mut controls = vec![
            command.command.cancel(),
            command.command.shutdown(),
            child.cancel(),
            child.shutdown(),
            &command.cancel,
            &command.pending,
        ];
        controls.extend_from_slice(additional);
        let service_generation = service_generation.unwrap_or(target.created());
        return with_many_owner_generations_transition(
            receiver,
            &captured,
            service,
            &[
                command.command.authority_generation(),
                child.authority_generation(),
                target.created(),
                service_generation,
            ],
            &controls,
            cancel,
            pending.unwrap_or(child.shutdown()),
            |_| invocation.with_registered_state(first_registered.owner(), apply),
        )
        .flatten()
        .flatten();
    }
    if let Some(startup) = snapshot.startup.as_ref() {
        matches_startup_snapshot_origin(&startup.startup, &startup.shell)?;
        let invocation = target.invocation_target()?;
        let child = invocation.operation();
        if !child.is_child_of(&startup.startup) {
            return None;
        }
        let first_registered = same_registered_origin(invocation, receiver)?;
        let mut captured = Vec::with_capacity(participants.len() + startup.participants.len() + 1);
        captured.extend_from_slice(participants);
        captured.push(startup.shell.clone());
        captured.extend_from_slice(&startup.participants);
        let mut controls = vec![
            startup.startup.cancel(),
            startup.startup.shutdown(),
            child.cancel(),
            child.shutdown(),
            &startup.cancel,
            &startup.pending,
        ];
        controls.extend_from_slice(additional);
        let service_generation = service_generation.unwrap_or(target.created());
        return with_many_owner_generations_transition(
            receiver,
            &captured,
            service,
            &[
                startup.startup.authority_generation(),
                child.authority_generation(),
                target.created(),
                service_generation,
            ],
            &controls,
            cancel,
            pending.unwrap_or(child.shutdown()),
            |_| invocation.with_registered_state(first_registered.owner(), apply),
        )
        .flatten()
        .flatten();
    }
    match &snapshot.context {
        RecoveryOpenContext::NativeStartup(_) => {
            let invocation = target.invocation_target()?;
            let registered = same_registered_origin(invocation, receiver)?;
            let operation = invocation.operation();
            let mut controls = vec![operation.cancel(), operation.shutdown()];
            controls.extend_from_slice(additional);
            with_many_owner_generations_transition(
                receiver,
                participants,
                service,
                &[
                    operation.authority_generation(),
                    target.created(),
                    service_generation.unwrap_or(target.created()),
                ],
                &controls,
                cancel,
                pending.unwrap_or(operation.shutdown()),
                |_| invocation.with_registered_state(registered.owner(), apply),
            )
            .flatten()
            .flatten()
        }
        RecoveryOpenContext::Renderer { .. } => {
            let loss = receiver.loss_token()?;
            let service_generation = service_generation.unwrap_or(target.created());
            with_many_owner_generations_transition(
                receiver,
                participants,
                service,
                &[target.created(), service_generation],
                additional,
                cancel,
                pending.unwrap_or(&loss),
                |_| apply(),
            )
            .flatten()
        }
    }
}

/// Consume the exact UI action under the same N-owner admission, then start it
/// on this UI stack only after every authority gate has been released.
pub(crate) fn run_accepted_ui_many<R>(
    ui: &dyn Thread,
    origin: &NativeRequestOwner,
    destinations: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    super::execute_accepted(with_many_owner_transition(
        origin,
        destinations,
        cancel,
        pending,
        |_| action,
    ))
}

/// A captured print actor/target adds fences; it never replaces either owner.
/// The caller validates the captured browser/document in the final native action.
pub(crate) fn with_print_pair_effect<R>(
    origin: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    target_generation: &RendererGeneration,
    actor: &CancelToken,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_many_owner_generations_transition(
        origin,
        std::slice::from_ref(destination),
        None,
        &[target_generation],
        &[actor],
        cancel,
        pending,
        |_| effect(),
    )
}

pub(crate) fn with_created_many_effect<R>(
    origin: &NativeRequestOwner,
    destinations: &[NativeRequestOwner],
    created: &RendererGeneration,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: impl FnOnce() -> R,
) -> Option<R> {
    with_many_owner_generations_transition(
        origin,
        destinations,
        None,
        &[created],
        &[],
        cancel,
        pending,
        |_| effect(),
    )
}

pub(crate) fn run_accepted_created_ui_many<R>(
    ui: &dyn Thread,
    origin: &NativeRequestOwner,
    destinations: &[NativeRequestOwner],
    created: &RendererGeneration,
    cancel: &CancelToken,
    pending: &CancelToken,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    super::execute_accepted(with_created_many_effect(
        origin,
        destinations,
        created,
        cancel,
        pending,
        || action,
    ))
}

pub(crate) fn run_accepted_ui_pair<R>(
    ui: &dyn Thread,
    origin: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
    action: impl FnOnce() -> R,
) -> Result<R, String> {
    if !ui.is_current() {
        return Err("Native UI admission requires the UI thread.".into());
    }
    super::execute_accepted(with_paired_effect(
        origin,
        destination,
        cancel,
        pending,
        || action,
    ))
}

fn drain_event(ticket: Option<crate::ipc::outlet::DrainTicket>) {
    if let Some(ticket) = ticket {
        let _ = ticket.drain();
    }
}

pub(crate) struct NativeOwnedEffects {
    cx: HostContext,
}
pub(crate) fn capture_owner(
    views: &crate::views::ViewTable,
    view: ViewId,
    expected: &RendererGeneration,
) -> Result<NativeRequestOwner, String> {
    let captured = views
        .capture_native_owner(view, expected)
        .ok_or("Renderer document authentication is not ready.")?;
    let captured = if let Some(asset) = captured.asset {
        super::asset_owner_with_browser(
            view,
            captured.origin,
            asset,
            captured.renderer,
            captured.browser_owner,
        )
    } else if let Some(slides) = captured.slides {
        super::owner_with_browser(
            view,
            captured.origin,
            captured.renderer,
            slides,
            captured.browser_owner,
        )
    } else if let Some(docs) = captured.docs {
        super::owner_with_browser(
            view,
            captured.origin,
            captured.renderer,
            docs,
            captured.browser_owner,
        )
    } else {
        super::owner_with_browser(
            view,
            captured.origin,
            captured.renderer,
            (),
            captured.browser_owner,
        )
    };
    captured.ok_or_else(|| "Renderer document is no longer available.".into())
}

pub(crate) fn host(cx: &HostContext) -> OwnedEffectsHost {
    Arc::new(NativeOwnedEffects { cx: cx.clone() })
}

type MediaRecoveryFences<'a> = (
    Option<(
        &'a shell_contract::ipc::RecoveryOpenTarget,
        NativeRequestOwner,
    )>,
    Vec<&'a RendererGeneration>,
    Vec<&'a CancelToken>,
    Vec<NativeRequestOwner>,
);

fn media_recovery_fences<'a>(
    recovery: Option<&'a shell_contract::ipc::RecoverySeedReceipt>,
    snapshot: Option<&'a shell_contract::ipc::RecoveryAuthoritySnapshot>,
    original: &NativeRequestOwner,
) -> Result<MediaRecoveryFences<'a>, String> {
    let authority = proof(original).ok_or("The original Docs owner is unavailable")?;
    let recovery = recovery
        .map(|receipt| {
            let target = receipt.target();
            let registered = snapshot
                .and_then(|snapshot| snapshot.registered.clone())
                .ok_or("The recovery target is not registered")?;
            if !receipt.matches(target)
                || target.view() != original.view
                || registered.view != original.view
                || proof(&registered).is_none_or(|registered| registered.origin != authority.origin)
                || !original
                    .context
                    .renderer()
                    .is_some_and(|renderer| renderer.same_generation(target.created()))
                || !registered
                    .context
                    .renderer()
                    .is_some_and(|renderer| renderer.same_generation(target.created()))
            {
                return Err(
                    "The recovery receipt does not match the captured Docs owner".to_owned(),
                );
            }
            Ok((target, registered))
        })
        .transpose()?;
    let mut generations = Vec::new();
    let mut cancellation = Vec::new();
    let mut owners = Vec::new();
    if let Some((target, _)) = &recovery {
        generations.push(target.created());
        if let Some(invocation) = target.invocation_target() {
            generations.push(invocation.operation().authority_generation());
            cancellation.push(invocation.operation().cancel());
            cancellation.push(invocation.operation().shutdown());
            if let Some(command) = snapshot.and_then(|snapshot| snapshot.command.as_ref()) {
                let lineage = match snapshot.and_then(|snapshot| snapshot.startup.as_ref()) {
                    Some(startup) => command.command.is_child_of(&startup.startup),
                    None => command.command.is_root_native_command(),
                };
                if !lineage || !invocation.operation().is_child_of(&command.command) {
                    return Err("The recovery child does not match its native command".into());
                }
                owners.extend_from_slice(&command.participants);
                generations.push(command.command.authority_generation());
                cancellation.extend([
                    command.command.cancel(),
                    command.command.shutdown(),
                    &command.cancel,
                    &command.pending,
                ]);
            }
            if let Some(startup) = snapshot.and_then(|snapshot| snapshot.startup.as_ref()) {
                matches_startup_snapshot_origin(&startup.startup, &startup.shell)
                    .ok_or("The startup Shell owner is no longer available")?;
                let parent_matches = match snapshot.and_then(|snapshot| snapshot.command.as_ref()) {
                    Some(command) => {
                        command.command.is_child_of(&startup.startup)
                            && invocation.operation().is_child_of(&command.command)
                    }
                    None => invocation.operation().is_child_of(&startup.startup),
                };
                if !parent_matches {
                    return Err("The recovery child does not match its startup".into());
                }
                owners.push(startup.shell.clone());
                owners.extend_from_slice(&startup.participants);
                generations.push(startup.startup.authority_generation());
                cancellation.extend([
                    startup.startup.cancel(),
                    startup.startup.shutdown(),
                    &startup.cancel,
                    &startup.pending,
                ]);
            }
        }
    }
    Ok((recovery, generations, cancellation, owners))
}

#[allow(clippy::too_many_arguments)]
fn with_recovery_pdf_revision_transition(
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    receiver: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    old: &PdfDocumentLease,
    successor: &PdfDocumentLease,
    renewal: &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal,
    before_notify: &mut dyn FnMut(),
    effect: &mut dyn FnMut(
        &mut shell_contract::ipc::AdmittedPdfRevisionRetirement<'_, '_>,
        &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal,
    ) -> Result<(), String>,
) -> Result<(), String> {
    use crate::views::{AssetLeaseGatePlan, SlidesViewLeaseGatePlan};
    use main_core::docs::byte_handoff::MediaPermitGatePlan;
    use shell_contract::{
        ipc::PdfDocumentGatePlan,
        runtime::{CancelTokenGatePlan, RendererGenerationGatePlan},
    };
    if !renewal.matches(receipt) {
        return Err("The PDF renewal belongs to another recovery target.".into());
    }

    let destination = participants
        .first()
        .ok_or("The PDF destination is unavailable.")?;
    let snapshot = receipt.target().authority_snapshot()?;
    let (recovery, extra_generations, extra_cancellation, extra_owners) =
        media_recovery_fences(Some(receipt), Some(&snapshot), receiver)?;
    let (target, registered) = recovery.ok_or("The recovery target is unavailable.")?;
    let mut captured =
        Vec::with_capacity(participants.len() + extra_owners.len() + renewal.sources().len());
    captured.extend_from_slice(participants);
    captured.extend(extra_owners);
    captured.extend_from_slice(renewal.sources());
    let owners: Vec<_> = std::iter::once(receiver).chain(captured.iter()).collect();
    let proofs: Vec<_> = owners
        .iter()
        .map(|owner| proof(owner))
        .collect::<Option<_>>()
        .ok_or("The captured native owner is unavailable.")?;
    let losses: Vec<_> = owners
        .iter()
        .map(|owner| owner.loss_token())
        .collect::<Option<_>>()
        .ok_or("The captured owner loss is unavailable.")?;
    let mut native_tokens = Vec::with_capacity(proofs.len());
    let mut assets = Vec::new();
    let mut generations = Vec::new();
    let mut documents = Vec::new();
    let mut slides = Vec::new();
    let mut media = Vec::new();
    let mut pdf = vec![old, successor];
    for (owner, proof) in owners.iter().zip(&proofs) {
        native_tokens.push(&proof.origin);
        generations.push(
            owner
                .context
                .renderer()
                .ok_or("The captured renderer is unavailable.")?,
        );
        if let Some(document) = owner.context.document_lease() {
            generations.push(document);
            documents.push(document);
        }
        if proof.docs_bound && proof.docs_permit.is_none() {
            return Err("The Docs permit is unavailable.".into());
        }
        if let Some(permit) = proof.docs_permit.as_ref() {
            media.push(permit);
        }
        if let Some(asset) = owner.context.downcast_ref::<NativeAssetAuthorization>() {
            if asset.view != owner.view || !asset.matches_native_context(&proof.origin) {
                return Err("The asset owner changed.".into());
            }
            assets.push(asset);
        }
        if let Some(lease) = owner.context.downcast_ref::<SlidesViewLease>() {
            slides.push(lease);
        }
        if let Some(document) = owner.pdf_document() {
            pdf.push(document);
        }
        if let Some(revision) = owner.pdf_revision() {
            pdf.push(revision);
        }
    }
    generations.extend(extra_generations);
    let mut generations = RendererGenerationGatePlan::from_generations(generations);
    let mut retirement = generations.prepare_retirement(&documents)?;
    let native = crate::native::DocumentGatePlan::new(&native_tokens)
        .ok_or("The native owner is unavailable.")?;
    let assets = AssetLeaseGatePlan::new(&assets);
    let mut slides = SlidesViewLeaseGatePlan::new(&slides);
    let mut media = MediaPermitGatePlan::new(&media);
    let mut pdf = PdfDocumentGatePlan::new(&pdf);
    let plan = pdf.prepare_retirement_many(destination, &owners, old, successor)?;
    let mut plan = plan;
    let mut cancellation = vec![cancel, pending];
    cancellation.extend(losses.iter());
    cancellation.extend(extra_cancellation);
    let mut cancellation = CancelTokenGatePlan::from_tokens(cancellation);
    let mut action = Some(effect);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        native.with_live(|| assets.with_lease_live_fenced(|| generations.with_active_retiring(&mut retirement, |_| {
            slides.with_active(|| media.with_active(|| pdf.with_live_retiring(&mut plan, |admitted| {
                cancellation.with_active(|| {
                    let mut apply = || action.take().expect("single PDF admission")(admitted, renewal);
                    match target.invocation_target() {
                        Some(invocation) => invocation.with_registered_state(&registered, apply).ok_or("The recovery target was replaced.")?,
                        None => apply(),
                    }
                })
            }).flatten()).flatten()).flatten()
        }).flatten()).flatten()).flatten().ok_or_else(|| "The captured PDF recovery authority expired.".to_owned())?
    }));
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(before_notify));
    let notification =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| renewal.notify_replaced()));
    drop(plan);
    match (result, cleanup, notification) {
        (Err(panic), _, _) | (Ok(_), Err(panic), _) | (Ok(_), Ok(()), Err(panic)) => {
            std::panic::resume_unwind(panic)
        }
        (Ok(result), Ok(()), Ok(())) => result,
    }
}

fn with_native_save_revision_transition(
    prepared: &mut shell_contract::ipc::PreparedNativeSaveRevision,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    before_notify: &mut dyn FnMut(),
    effect: &mut dyn FnMut(
        &mut shell_contract::ipc::AdmittedPdfRevisionSet<'_, '_>,
        &mut shell_contract::ipc::PreparedNativeSaveRevision,
    ) -> Result<(), String>,
) -> Result<(), String> {
    use crate::views::{AssetLeaseGatePlan, SlidesViewLeaseGatePlan};
    use main_core::docs::byte_handoff::MediaPermitGatePlan;
    use shell_contract::{
        ipc::PdfDocumentGatePlan,
        runtime::{CancelTokenGatePlan, RendererGenerationGatePlan},
    };
    let snapshot = prepared.snapshot().clone();
    let pairs = prepared.pairs().to_vec();
    let pair_refs: Vec<_> = pairs.iter().map(|(old, new)| (old, new)).collect();
    let mut captured = snapshot.aliases().to_vec();
    captured.extend(prepared.request_aliases().cloned());
    captured.extend_from_slice(participants);
    let owners: Vec<_> = std::iter::once(snapshot.selection().binding().owner())
        .chain(captured.iter())
        .collect();
    let proofs: Vec<_> = owners
        .iter()
        .map(|owner| proof(owner))
        .collect::<Option<_>>()
        .ok_or("The native owner is unavailable.")?;
    let losses: Vec<_> = owners
        .iter()
        .map(|owner| owner.loss_token())
        .collect::<Option<_>>()
        .ok_or("The owner lifetime is unavailable.")?;
    let mut native_tokens = Vec::with_capacity(owners.len());
    let mut assets = Vec::new();
    let mut generations = Vec::new();
    let mut documents = Vec::new();
    let mut slides = Vec::new();
    let mut media = Vec::new();
    let mut pdf = Vec::with_capacity(pairs.len() * 2 + owners.len() * 2);
    for (old, new) in &pairs {
        pdf.extend([old, new]);
    }
    for (owner, authority) in owners.iter().zip(&proofs) {
        native_tokens.push(&authority.origin);
        generations.push(
            owner
                .context
                .renderer()
                .ok_or("The renderer is unavailable.")?,
        );
        if let Some(document) = owner.context.document_lease() {
            generations.push(document);
            documents.push(document);
        }
        if authority.docs_bound && authority.docs_permit.is_none() {
            return Err("The Docs permit is unavailable.".into());
        }
        if let Some(permit) = authority.docs_permit.as_ref() {
            media.push(permit);
        }
        if let Some(asset) = owner.context.downcast_ref::<NativeAssetAuthorization>() {
            if asset.view != owner.view || !asset.matches_native_context(&authority.origin) {
                return Err("The asset owner changed.".into());
            }
            assets.push(asset);
        }
        if let Some(lease) = owner.context.downcast_ref::<SlidesViewLease>() {
            slides.push(lease);
        }
        if let Some(slot) = owner.pdf_document() {
            pdf.push(slot);
        }
        if let Some(revision) = owner.pdf_revision() {
            pdf.push(revision);
        }
    }
    let operation = snapshot.selection().binding().operation();
    generations.push(operation.authority_generation());
    let mut generations = RendererGenerationGatePlan::from_generations(generations);
    let mut document_retirement = generations.prepare_retirement(&documents)?;
    let native = crate::native::DocumentGatePlan::new(&native_tokens)
        .ok_or("The native owner is unavailable.")?;
    let assets = AssetLeaseGatePlan::new(&assets);
    let mut slides = SlidesViewLeaseGatePlan::new(&slides);
    let mut media = MediaPermitGatePlan::new(&media);
    let mut pdf = PdfDocumentGatePlan::new(&pdf);
    let mut revision_plan = pdf.prepare_revision_set(&pair_refs)?;
    let mut controls = vec![cancel, pending, operation.cancel(), operation.shutdown()];
    controls.extend(losses.iter());
    controls.extend_from_slice(additional);
    let mut controls = CancelTokenGatePlan::from_tokens(controls);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        native
            .with_live(|| {
                assets
                    .with_lease_live_fenced(|| {
                        generations
                            .with_active_retiring(&mut document_retirement, |_| {
                                slides
                                    .with_active(|| {
                                        media
                                            .with_active(|| {
                                                pdf.with_live_revision_set(
                                                    &mut revision_plan,
                                                    |retirement| {
                                                        controls.with_active(|| {
                    effect(retirement, prepared)?;
                    if !retirement.is_retired() || !prepared.is_committed() {
                        return Err("The native save revision transition was not committed.".into());
                    }
                    Ok(())
                })
                                                    },
                                                )
                                                .flatten()
                                            })
                                            .flatten()
                                    })
                                    .flatten()
                            })
                            .flatten()
                    })
                    .flatten()
            })
            .flatten()
            .ok_or("The native save revision authority expired.")?
    }));
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(before_notify));
    let notification =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepared.notify_replaced()));
    drop(revision_plan);
    match (result, cleanup, notification) {
        (Err(panic), _, _) | (Ok(_), Err(panic), _) | (Ok(_), Ok(()), Err(panic)) => {
            std::panic::resume_unwind(panic)
        }
        (Ok(result), Ok(()), Ok(())) => result,
    }
}

impl OwnedEffects for NativeOwnedEffects {
    fn capture_owner(
        &self,
        view: ViewId,
        expected: &RendererGeneration,
    ) -> Result<NativeRequestOwner, String> {
        capture_owner(&self.cx.views, view, expected)
    }
    fn with_exact_browser_owner_effect(
        &self,
        expected: &shell_contract::ipc::NativeBrowserOwner,
        actual: &NativeRequestOwner,
        cancel: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let authority = proof(actual).ok_or("The authenticated browser owner is unavailable.")?;
        if !authority
            .browser_owner
            .as_ref()
            .is_some_and(|owner| owner.same_owner(expected))
        {
            return Err("The authenticated browser owner does not match.".into());
        }
        let renderer = actual
            .context
            .renderer()
            .ok_or("The authenticated renderer is unavailable.")?;
        let slot = self
            .cx
            .views
            .capture_browser_owner(actual.view, &authority.origin, renderer, expected)
            .ok_or("The captured browser record is unavailable.")?;
        with_many_owner_effect_extra(actual, &[], cancel, cancel, &[], || {
            expected.reservation().with_owner(expected, |transition| {
                if !slot.matches(transition) {
                    return Err("The captured browser owner was replaced.".into());
                }
                effect()
            })
        })
        .ok_or_else(|| "The authenticated browser owner expired.".to_owned())?
    }
    fn docs_document_identity(&self, owner: &NativeRequestOwner) -> Option<(ViewId, u64)> {
        let identity = proof(owner)?.docs_permit.as_ref()?.owner();
        Some((ViewId(identity.view), identity.generation))
    }
    fn with_native_saved_target_seed(
        &self,
        target: &shell_contract::ipc::NativeSavedTarget,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
    ) -> Result<(), String> {
        super::saved_target::with_saved_target_seed_effect(
            target,
            participants,
            cancel,
            pending,
            additional,
            effect,
        )
    }
    fn with_native_saved_document_input(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
    ) -> Result<(), String> {
        super::saved_target::with_saved_document_input_effect(
            document,
            participants,
            cancel,
            pending,
            additional,
            effect,
        )
    }
    fn emit_native_saved_document_input(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let mut frame = Some(shell_core::outlet::event_frame(channel, &args));
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let mut queued = None;
        self.with_native_saved_document_input(
            document,
            participants,
            cancel,
            pending,
            additional,
            &mut |_| {
                let frame = frame
                    .take()
                    .ok_or("The saved document event was already enqueued.")?;
                queued = Some(ipc.outlet.enqueue_generation_fenced(
                    target.view,
                    generation,
                    frame,
                )?);
                Ok(())
            },
        )?;
        drain_event(queued.ok_or("The saved document event was not admitted.")?);
        Ok(())
    }
    fn post_ui_native_saved_document_input(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let controls: Vec<_> = additional.iter().collect();
        let mut action = Some(action);
        let mut accepted_action = None;
        let accepted = super::saved_target::with_saved_document_input_effect(
            &document,
            &participants,
            &cancel,
            &pending,
            &controls,
            &mut |_| {
                accepted_action = Some(
                    action
                        .take()
                        .ok_or("The saved UI action was already accepted.")?,
                );
                Ok(())
            },
        );
        if let Err(error) = accepted {
            return done(Err(error));
        }
        let Some(action) = accepted_action else {
            return done(Err("The saved UI action was not accepted.".into()));
        };
        self.cx.ui.post(Box::new(move || {
            let controls: Vec<_> = additional.iter().collect();
            let mut action = Some(action);
            let mut accepted_action = None;
            let admitted = super::saved_target::with_saved_document_input_effect(
                &document,
                &participants,
                &cancel,
                &pending,
                &controls,
                &mut |_| {
                    accepted_action = Some(
                        action
                            .take()
                            .ok_or("The saved UI action was already consumed.")?,
                    );
                    Ok(())
                },
            );
            let result = admitted.and_then(|()| {
                accepted_action.ok_or_else(|| "The saved UI action was not admitted.".to_owned())?()
            });
            done(result);
        }));
    }
    fn emit_native_saved_document_same_origin(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &NativeRequestOwner,
        actual_cancel: &CancelToken,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let mut frame = Some(shell_core::outlet::event_frame(channel, &args));
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let mut queued = None;
        self.with_native_saved_document_same_origin(
            document,
            actual,
            actual_cancel,
            participants,
            cancel,
            pending,
            additional,
            &mut |_| {
                let frame = frame
                    .take()
                    .ok_or("The saved document event was already enqueued.")?;
                queued = Some(ipc.outlet.enqueue_generation_fenced(
                    target.view,
                    generation,
                    frame,
                )?);
                Ok(())
            },
        )?;
        drain_event(queued.ok_or("The saved document event was not admitted.")?);
        Ok(())
    }
    fn with_native_saved_document_same_origin(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &NativeRequestOwner,
        actual_cancel: &CancelToken,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
    ) -> Result<(), String> {
        super::saved_target::with_native_saved_document_same_origin(
            document,
            actual,
            actual_cancel,
            participants,
            cancel,
            pending,
            additional,
            effect,
        )
    }
    fn with_native_saved_document_transition(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &NativeRequestOwner,
        actual_cancel: &CancelToken,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut(
            &shell_contract::ipc::NativeSavedInput,
            &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        super::saved_target::with_native_saved_document_transition(
            document,
            actual,
            actual_cancel,
            participants,
            cancel,
            pending,
            additional,
            effect,
        )
    }
    fn with_native_command_preparation_effect(
        &self,
        operation: &shell_contract::ipc::NativeInvocationOperation,
        expected: &shell_contract::ipc::NativeEditorCommand,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_native_command_preparation_effect(
            operation,
            expected,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The captured native command is no longer available.".to_owned())?
    }
    fn with_startup_command_target_effect(
        &self,
        startup: &shell_contract::ipc::NativeInvocationOperation,
        shell: &NativeRequestOwner,
        child: &shell_contract::ipc::NativeInvocationOperation,
        expected: &shell_contract::ipc::NativeEditorCommand,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_startup_command_target_effect(
            startup,
            shell,
            child,
            expected,
            document,
            owner,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The captured startup command target is no longer available.".to_owned())?
    }
    fn with_startup_command_creation_effect(
        &self,
        startup: &shell_contract::ipc::NativeInvocationOperation,
        shell: &NativeRequestOwner,
        child: &shell_contract::ipc::NativeInvocationOperation,
        expected: &shell_contract::ipc::NativeEditorCommand,
        created: &RendererGeneration,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_startup_command_creation_effect(
            startup,
            shell,
            child,
            expected,
            created,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| {
            "The captured startup command constructor is no longer available.".to_owned()
        })?
    }
    fn with_startup_command_preparation_effect(
        &self,
        startup: &shell_contract::ipc::NativeInvocationOperation,
        shell: &NativeRequestOwner,
        child: &shell_contract::ipc::NativeInvocationOperation,
        expected: &shell_contract::ipc::NativeEditorCommand,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_startup_command_preparation_effect(
            startup,
            shell,
            child,
            expected,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The captured startup command is no longer available.".to_owned())?
    }
    fn with_native_command_creation_effect(
        &self,
        operation: &shell_contract::ipc::NativeInvocationOperation,
        expected: &shell_contract::ipc::NativeEditorCommand,
        created: &RendererGeneration,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_native_command_creation_effect(
            operation,
            expected,
            created,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The captured native command is no longer available.".to_owned())?
    }
    fn with_native_command_target_effect(
        &self,
        operation: &shell_contract::ipc::NativeInvocationOperation,
        expected: &shell_contract::ipc::NativeEditorCommand,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_native_command_target_effect(
            operation,
            expected,
            document,
            owner,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The captured native command target is no longer available.".to_owned())?
    }
    fn stage_saved_docs_media(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &NativeRequestOwner,
        actual_cancel: &CancelToken,
        original: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        stage: &dyn shell_contract::hosts::DocsMediaStage,
    ) -> Result<(), String> {
        use main_core::docs::byte_handoff::{
            DeferredMediaLoss, MediaViewLease, PreparedMediaCandidate,
        };
        let lease = stage
            .lease()
            .downcast_ref::<MediaViewLease>()
            .ok_or("Invalid Docs media lease")?;
        let old = stage
            .old()
            .downcast_ref::<MediaPermit>()
            .ok_or("Invalid original Docs permit")?;
        let prepared_candidate = stage
            .candidate()
            .downcast_ref::<PreparedMediaCandidate>()
            .ok_or("Invalid prepared Docs permit")?;
        let candidate = prepared_candidate.permit();
        let prior = stage
            .prior()
            .map(|value| {
                value
                    .downcast_ref::<MediaPermit>()
                    .ok_or("Invalid previous Docs permit")
            })
            .transpose()?;
        let authority = saved_docs_origin(document, actual, original)?;
        if lease.view() != original.view.0
            || old.owner().view != original.view.0
            || candidate.owner().view != original.view.0
            || prior.is_some_and(|permit| permit.owner().view != original.view.0)
            || stage.old_identity() != (original.view, old.owner().generation)
            || !authority
                .docs_permit
                .as_ref()
                .is_some_and(|permit| permit.same_session(old))
        {
            return Err("The captured Docs media stage does not match its owner".into());
        }
        let renderer = original
            .context
            .renderer()
            .ok_or("The original Docs renderer is unavailable")?;
        let make_owner = |permit: &MediaPermit| {
            super::owner_with_browser(
                original.view,
                authority.origin.clone(),
                renderer.clone(),
                super::SuccessorMediaPermit(permit.clone()),
                authority.browser_owner.clone(),
            )
            .ok_or_else(|| "The captured Docs staging permit is unavailable".to_owned())
        };
        let mut captured = Vec::with_capacity(participants.len() + 5);
        captured.extend_from_slice(participants);
        captured.extend([
            document.owner().clone(),
            actual.clone(),
            original.clone(),
            make_owner(candidate)?,
        ]);
        if let Some(prior) = prior.filter(|permit| permit.is_active()) {
            captured.push(make_owner(prior)?);
        }
        let mut controls = Vec::with_capacity(additional.len() + 1);
        controls.push(actual_cancel);
        controls.extend_from_slice(additional);
        let target = document.target();
        let mut notifications = DeferredMediaLoss::default();
        let result = super::saved_target::with_prepared_saved_target_media(
            target,
            &captured,
            cancel,
            pending,
            &controls,
            |admit| {
                lease.with_prepared_stage(old, prior, prepared_candidate, |prepared| {
                    let retirement_target = prepared.retirement_target().clone();
                    let mut prepared = Some(prepared);
                    admit(
                        Some((&retirement_target, candidate, &mut notifications)),
                        &mut |_, retirement| {
                            let retirement =
                                retirement.ok_or("Missing admitted Docs media retirement")?;
                            target.reservation().with_owner(target.browser(), |_| {
                                target
                                    .with_registered_state(document.owner(), || {
                                        prepared
                                            .take()
                                            .ok_or("The Docs media stage was already consumed")?
                                            .commit(retirement)
                                    })
                                    .ok_or("The registered saved document was replaced")?
                            })
                        },
                    )
                })?
            },
        );
        drop(notifications);
        result
    }
    fn transition_saved_docs_media(
        &self,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &NativeRequestOwner,
        actual_cancel: &CancelToken,
        original: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        transition: &dyn shell_contract::hosts::DocsMediaTransition,
        retain: &mut dyn FnMut(
            &NativeRequestOwner,
            &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String>,
        before_notify: &mut dyn FnMut(
            &shell_contract::ipc::NativeSavedDocument,
            &NativeRequestOwner,
        ) -> Result<(), String>,
    ) -> Result<(shell_contract::ipc::NativeSavedDocument, NativeRequestOwner), String> {
        use main_core::docs::byte_handoff::{DeferredMediaLoss, MediaViewLease};
        let lease = transition
            .lease()
            .downcast_ref::<MediaViewLease>()
            .ok_or("Invalid Docs media lease")?;
        let old = transition
            .old()
            .downcast_ref::<MediaPermit>()
            .ok_or("Invalid original Docs permit")?;
        let successor = transition
            .successor()
            .downcast_ref::<MediaPermit>()
            .ok_or("Invalid staged Docs permit")?;
        let authority = saved_docs_origin(document, actual, original)?;
        if lease.view() != original.view.0
            || old.owner().view != original.view.0
            || successor.owner().view != original.view.0
            || !authority
                .docs_permit
                .as_ref()
                .is_some_and(|permit| permit.same_session(old))
            || transition.old_identity() != (original.view, old.owner().generation)
            || transition.successor_identity() != (original.view, successor.owner().generation)
        {
            return Err("The captured Docs media transition does not match its owner".into());
        }
        let registered = document.owner();
        let registered_proof =
            proof(registered).ok_or("The registered saved document is unavailable")?;
        let renderer = original
            .context
            .renderer()
            .ok_or("The original Docs renderer is unavailable")?;
        let fresh_owner = |source: &NativeRequestOwner,
                           source_proof: &NativeOwnerAuthority|
         -> Result<NativeRequestOwner, String> {
            if source_proof._asset_loss.is_some() || source_proof._slides_loss.is_some() {
                return Err(
                    "The captured Docs owner has incompatible native lifetime evidence".into(),
                );
            }
            let base = super::owner_with_browser(
                source.view,
                source_proof.origin.clone(),
                renderer.clone(),
                super::SuccessorMediaPermit(successor.clone()),
                source_proof.browser_owner.clone(),
            )
            .ok_or("The staged Docs owner is unavailable")?;
            let context = source
                .context
                .with_media_successor_base(&base.context)
                .ok_or("The captured Docs owner cannot retain its native constraints")?;
            Ok(NativeRequestOwner {
                view: source.view,
                context,
            })
        };
        let next_owner = fresh_owner(original, authority)?;
        let next_registered = match registered_proof.docs_permit.as_ref() {
            Some(permit) if permit.same_session(old) => fresh_owner(registered, registered_proof)?,
            Some(_) => return Err("The registered saved Docs media owner was replaced".into()),
            None if registered_proof.docs_bound => {
                return Err("The registered saved Docs media owner is unavailable".into());
            }
            None => registered.clone(),
        };
        let reply = actual
            .context
            .request_authority::<crate::ipc::ReplyAuthority>()
            .ok_or("The native request reply authority is unavailable")?;
        if actual.dispatcher_cancellation_for(actual_cancel).is_none() {
            return Err("The saved Docs request cancellation does not match its ingress".into());
        }
        let actual_proof = proof(actual).ok_or("The calling renderer is unavailable")?;
        let next_actual = match actual_proof.docs_permit.as_ref() {
            Some(permit) if permit.same_session(old) => fresh_owner(actual, actual_proof)?,
            Some(_) => return Err("The calling Docs media owner was replaced".into()),
            None if actual_proof.docs_bound => {
                return Err("The calling Docs media owner is unavailable".into());
            }
            None => actual.clone(),
        };
        let prepared_successor = document.prepare_owner_successor(next_registered.clone())?;
        let mut captured = Vec::with_capacity(participants.len() + 6);
        captured.extend_from_slice(participants);
        captured.extend([
            registered.clone(),
            actual.clone(),
            original.clone(),
            next_owner.clone(),
            next_registered,
            next_actual.clone(),
        ]);
        let mut controls = Vec::with_capacity(additional.len() + 1);
        controls.push(actual_cancel);
        controls.extend_from_slice(additional);
        let target = document.target();
        let mut notifications = DeferredMediaLoss::default();
        let mut committed = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::saved_target::with_prepared_saved_target_media(
                target,
                &captured,
                cancel,
                pending,
                &controls,
                |admit| {
                    lease.with_staged_transition(old, successor, |staged| {
                let mut staged = Some(staged);
                admit(Some((old, successor, &mut notifications)), &mut |_, retirement| {
                    let retirement = retirement.ok_or("Missing admitted Docs media retirement")?;
                    target.reservation().with_owner(target.browser(), |browser| {
                        let mut guard = Some(prepared_successor.validate_in_transition(browser)?);
                        let mut commit = || {
                            reply.with_media_transfer(actual, actual_cancel, next_actual.clone(), || {
                                staged.take().ok_or("The Docs media transition was already consumed")?.commit(retirement)?;
                                committed = Some((guard.take().expect("validated saved Docs successor").commit(), next_actual.clone()));
                                Ok(())
                            })
                        };
                        let result = retain(&next_owner, &mut commit);
                        result?;
                        if committed.is_none() { return Err("The Docs media transition was not committed".into()); }
                        Ok(())
                    })
                })
            })?
                },
            )
        }));
        let notify =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match committed.as_ref() {
                Some((successor, next_actual)) => before_notify(successor, next_actual),
                None => Ok(()),
            }));
        drop(notifications);
        match (result, notify) {
            (Err(payload), _) | (Ok(_), Err(payload)) => std::panic::resume_unwind(payload),
            (Ok(result), Ok(notify)) => {
                result?;
                notify?;
                committed.ok_or_else(|| "The saved Docs successor was not committed".into())
            }
        }
    }
    fn stage_docs_media(
        &self,
        original: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        stage: &dyn shell_contract::hosts::DocsMediaStage,
        recovery: Option<&shell_contract::ipc::RecoverySeedReceipt>,
    ) -> Result<(), String> {
        use main_core::docs::byte_handoff::{
            DeferredMediaLoss, MediaViewLease, PreparedMediaCandidate,
        };
        let lease = stage
            .lease()
            .downcast_ref::<MediaViewLease>()
            .ok_or("Invalid Docs media lease")?;
        let old = stage
            .old()
            .downcast_ref::<MediaPermit>()
            .ok_or("Invalid original Docs permit")?;
        let prepared_candidate = stage
            .candidate()
            .downcast_ref::<PreparedMediaCandidate>()
            .ok_or("Invalid prepared Docs permit")?;
        let candidate = prepared_candidate.permit();
        let prior = stage
            .prior()
            .map(|value| {
                value
                    .downcast_ref::<MediaPermit>()
                    .ok_or("Invalid previous Docs permit")
            })
            .transpose()?;
        let authority = proof(original).ok_or("The original Docs owner is unavailable")?;
        if lease.view() != original.view.0
            || old.owner().view != original.view.0
            || candidate.owner().view != original.view.0
            || prior.is_some_and(|permit| permit.owner().view != original.view.0)
            || stage.old_identity() != (original.view, old.owner().generation)
            || !authority
                .docs_permit
                .as_ref()
                .is_some_and(|permit| permit.same_session(old))
        {
            return Err("The captured Docs media stage does not match its owner".into());
        }
        let snapshot = recovery
            .map(|receipt| receipt.target().authority_snapshot())
            .transpose()?;
        let (recovery, extra_generations, extra_cancellation, extra_owners) =
            media_recovery_fences(recovery, snapshot.as_ref(), original)?;
        let renderer = original
            .context
            .renderer()
            .ok_or("The original Docs renderer is unavailable")?;
        let make_owner = |permit: &MediaPermit| {
            super::owner_with_browser(
                original.view,
                authority.origin.clone(),
                renderer.clone(),
                super::SuccessorMediaPermit(permit.clone()),
                authority.browser_owner.clone(),
            )
            .ok_or_else(|| "The captured Docs staging permit is unavailable".to_owned())
        };
        let mut captured = Vec::with_capacity(participants.len() + 2);
        captured.extend_from_slice(participants);
        captured.extend(extra_owners);
        captured.push(make_owner(candidate)?);
        if let Some(prior) = prior.filter(|prior| prior.is_active()) {
            captured.push(make_owner(prior)?);
        }
        let mut notifications = DeferredMediaLoss::default();
        let result = lease.with_prepared_stage(old, prior, prepared_candidate, |prepared| {
            let retirement_target = prepared.retirement_target().clone();
            with_many_owner_generations_media_transition(
                original,
                &captured,
                None,
                &extra_generations,
                &extra_cancellation,
                cancel,
                pending,
                Some((&retirement_target, candidate, &mut notifications)),
                |_, retirement| {
                    let admitted = retirement.ok_or("Missing admitted Docs media staging gate")?;
                    let commit = || match &snapshot {
                        Some(snapshot) => snapshot
                            .with_current(|| prepared.commit(admitted))
                            .ok_or_else(|| "The recovery authority was replaced".to_owned())?,
                        None => prepared.commit(admitted),
                    };
                    match &recovery {
                        Some((target, registered)) => match target.invocation_target() {
                            Some(invocation) => invocation
                                .with_registered_state(registered, commit)
                                .ok_or_else(|| {
                                    "The registered recovery target was replaced".to_owned()
                                })?,
                            None => commit(),
                        },
                        None => commit(),
                    }
                },
            )
            .ok_or("The captured Docs staging authority expired")?
        });
        drop(notifications);
        result?
    }
    fn transition_docs_media(
        &self,
        original: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        transition: &dyn shell_contract::hosts::DocsMediaTransition,
        recovery: Option<&shell_contract::ipc::RecoverySeedReceipt>,
        retain: &mut dyn FnMut(
            &NativeRequestOwner,
            &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String>,
    ) -> Result<NativeRequestOwner, String> {
        use main_core::docs::byte_handoff::{DeferredMediaLoss, MediaViewLease};
        let lease = transition
            .lease()
            .downcast_ref::<MediaViewLease>()
            .ok_or("Invalid Docs media lease")?;
        let old = transition
            .old()
            .downcast_ref::<MediaPermit>()
            .ok_or("Invalid original Docs permit")?;
        let successor = transition
            .successor()
            .downcast_ref::<MediaPermit>()
            .ok_or("Invalid staged Docs permit")?;
        let authority = proof(original).ok_or("The original Docs owner is unavailable")?;
        if lease.view() != original.view.0
            || old.owner().view != original.view.0
            || successor.owner().view != original.view.0
            || !authority
                .docs_permit
                .as_ref()
                .is_some_and(|permit| permit.same_session(old))
            || transition.old_identity() != (original.view, old.owner().generation)
            || transition.successor_identity() != (original.view, successor.owner().generation)
        {
            return Err("The captured Docs media transition does not match its owner".into());
        }
        let snapshot = recovery
            .map(|receipt| receipt.target().authority_snapshot())
            .transpose()?;
        let (recovery, extra_generations, extra_cancellation, extra_owners) =
            media_recovery_fences(recovery, snapshot.as_ref(), original)?;
        let renderer = original
            .context
            .renderer()
            .ok_or("The original Docs renderer is unavailable")?
            .clone();
        let mut next_owner = super::owner_with_browser(
            original.view,
            authority.origin.clone(),
            renderer,
            super::SuccessorMediaPermit(successor.clone()),
            authority.browser_owner.clone(),
        )
        .ok_or("The staged Docs owner is unavailable")?;
        if let Some(document) = original.context.document_lease() {
            next_owner.context = next_owner.context.with_document_lease(document.clone());
        }
        let mut captured = Vec::with_capacity(participants.len() + 1);
        captured.extend_from_slice(participants);
        captured.extend(extra_owners);
        captured.push(next_owner.clone());
        let mut notifications = DeferredMediaLoss::default();
        let result = lease.with_staged_transition(old, successor, |staged| {
            with_many_owner_generations_media_transition(
                original,
                &captured,
                None,
                &extra_generations,
                &extra_cancellation,
                cancel,
                pending,
                Some((old, successor, &mut notifications)),
                |_, retirement| -> Result<(), String> {
                    let admitted = retirement.ok_or("Missing admitted Docs media retirement")?;
                    let mut staged = Some(staged);
                    let mut committed = false;
                    let mut commit = || {
                        let staged = staged
                            .take()
                            .ok_or("The Docs media transition was already consumed")?;
                        let apply = || match &snapshot {
                            Some(snapshot) => snapshot
                                .with_current(|| staged.commit(admitted))
                                .ok_or_else(|| "The recovery authority was replaced".to_owned())?,
                            None => staged.commit(admitted),
                        };
                        match &recovery {
                            Some((target, registered)) => match target.invocation_target() {
                                Some(invocation) => invocation
                                    .with_registered_state(registered, apply)
                                    .ok_or_else(|| {
                                        "The registered recovery target was replaced".to_owned()
                                    })?,
                                None => apply(),
                            },
                            None => apply(),
                        }?;
                        committed = true;
                        Ok(())
                    };
                    let result = retain(&next_owner, &mut commit);
                    result?;
                    if !committed {
                        return Err("The Docs media transition was not committed".into());
                    }
                    Ok(())
                },
            )
            .ok_or("The captured Docs media authority expired")?
        });
        drop(notifications);
        result??;
        Ok(next_owner)
    }
    fn with_created_effect(
        &self,
        source: &NativeRequestOwner,
        created: &RendererGeneration,
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        super::with_created_effect(source, created, cancel, pending, effect)
            .ok_or_else(|| "Renderer document is no longer available.".to_owned())?
    }
    fn with_created_many_effect(
        &self,
        source: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        created: &RendererGeneration,
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_created_many_effect(source, participants, created, cancel, pending, effect)
            .ok_or_else(|| "Renderer document is no longer available.".to_owned())?
    }

    fn with_owned_effect(
        &self,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_paired_effect(origin, destination, cancel, pending, effect)
            .ok_or_else(|| "Renderer document is no longer available.".to_owned())?
    }

    fn with_same_native_origin_effect(
        &self,
        expected: &NativeRequestOwner,
        actual: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional_cancellation: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let matches = expected.view == actual.view
            && proof(expected)
                .zip(proof(actual))
                .is_some_and(|(expected, actual)| expected.origin == actual.origin)
            && expected
                .context
                .renderer()
                .zip(actual.context.renderer())
                .is_some_and(|(expected, actual)| expected.same_generation(actual));
        if !matches {
            return Err("The native document origin does not match.".into());
        }
        let mut captured = Vec::with_capacity(participants.len() + 1);
        captured.push(actual.clone());
        captured.extend_from_slice(participants);
        with_many_owner_generations_transition(
            expected,
            &captured,
            None,
            &[],
            additional_cancellation,
            cancel,
            pending,
            |_| effect(),
        )
        .ok_or_else(|| "Renderer document is no longer available.".to_owned())?
    }

    fn with_owned_transition(
        &self,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut(
            &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        with_optional_service_transition(origin, destination, None, None, cancel, pending, effect)
            .ok_or_else(|| "Renderer document is no longer available.".to_owned())?
    }

    fn with_owned_transition_many(
        &self,
        origin: &NativeRequestOwner,
        destinations: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut(
            &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        with_many_owner_transition(origin, destinations, cancel, pending, effect)
            .ok_or_else(|| "Renderer document is no longer available.".to_owned())?
    }

    fn emit_owned(
        &self,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let generation = destination
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let queued = with_paired_effect(origin, destination, cancel, pending, || {
            outlet.enqueue_generation_fenced(destination.view, generation, frame)
        })
        .ok_or_else(|| "Renderer document is no longer available.".to_owned())??;
        drain_event(queued);
        Ok(())
    }

    fn emit_owned_many(
        &self,
        origin: &NativeRequestOwner,
        destinations: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = destinations
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let queued = with_many_owner_transition(origin, destinations, cancel, pending, |_| {
            outlet.enqueue_generation_fenced(target.view, generation, frame)
        })
        .ok_or_else(|| "Renderer document is no longer available.".to_owned())??;
        drain_event(queued);
        Ok(())
    }
    fn emit_owned_many_extra(
        &self,
        origin: &NativeRequestOwner,
        destinations: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = destinations
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let queued = with_many_owner_generations_transition(
            origin,
            destinations,
            None,
            &[],
            additional,
            cancel,
            pending,
            |_| outlet.enqueue_generation_fenced(target.view, generation, frame),
        )
        .ok_or("Renderer document is no longer available.")??;
        drain_event(queued);
        Ok(())
    }

    fn emit_native_command_creation_many(
        &self,
        operation: &shell_contract::ipc::NativeInvocationOperation,
        expected: &shell_contract::ipc::NativeEditorCommand,
        created: &RendererGeneration,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let queued = with_native_command_creation_effect(
            operation,
            expected,
            created,
            participants,
            cancel,
            pending,
            || outlet.enqueue_generation_fenced(target.view, generation, frame),
        )
        .ok_or("The native command creation is no longer available.")??;
        drain_event(queued);
        Ok(())
    }

    fn emit_native_invocation_many(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let queued = with_native_invocation_many_effect(
            document.target(),
            origin,
            participants,
            cancel,
            pending,
            || outlet.enqueue_generation_fenced(target.view, generation, frame),
        )
        .ok_or_else(|| "The native invocation document is no longer available.".to_owned())??;
        drain_event(queued);
        Ok(())
    }

    fn emit_native_operation_many(
        &self,
        operation: &shell_contract::ipc::NativeInvocationOperation,
        origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let queued =
            with_startup_operation_effect(operation, origin, participants, cancel, pending, || {
                outlet.enqueue_generation_fenced(target.view, generation, frame)
            })
            .ok_or_else(|| "The startup operation is no longer available.".to_owned())??;
        drain_event(queued);
        Ok(())
    }
    fn emit_existing_document_save_many(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let queued = with_native_invocation_operation_effect(
            binding.operation(),
            binding.owner(),
            participants,
            cancel,
            pending,
            || outlet.enqueue_generation_fenced(target.view, generation, frame),
        )
        .ok_or("The existing document save is no longer available.")??;
        drain_event(queued);
        Ok(())
    }
    fn emit_existing_document_save_many_extra(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let operation = binding.operation();
        let mut controls = vec![operation.cancel(), operation.shutdown()];
        controls.extend_from_slice(additional);
        let queued = with_many_owner_generations_transition(
            binding.owner(),
            participants,
            None,
            &[operation.authority_generation()],
            &controls,
            cancel,
            pending,
            |_| outlet.enqueue_generation_fenced(target.view, generation, frame),
        )
        .ok_or("The existing document save is no longer available.")??;
        drain_event(queued);
        Ok(())
    }

    fn emit_startup_recovery_many(
        &self,
        startup: &shell_contract::ipc::NativeInvocationOperation,
        shell: &NativeRequestOwner,
        child: &shell_contract::ipc::NativeInvocationDocument,
        owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let queued = with_startup_recovery_effect(
            startup,
            shell,
            child,
            owner,
            participants,
            cancel,
            pending,
            || outlet.enqueue_generation_fenced(target.view, generation, frame),
        )
        .ok_or_else(|| "The startup recovery document is no longer available.".to_owned())??;
        drain_event(queued);
        Ok(())
    }

    fn with_owned_effect_many(
        &self,
        origin: &NativeRequestOwner,
        destinations: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_many_owner_transition(origin, destinations, cancel, pending, |_| effect())
            .ok_or_else(|| "Renderer document is no longer available.".to_owned())?
    }
    fn with_owned_effect_many_extra(
        &self,
        origin: &NativeRequestOwner,
        destinations: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_many_owner_generations_transition(
            origin,
            destinations,
            None,
            &[],
            additional,
            cancel,
            pending,
            |_| effect(),
        )
        .ok_or_else(|| "Renderer document is no longer available.".to_owned())?
    }

    fn with_native_invocation_effect(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_native_invocation_many_effect(
            document.target(),
            origin,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The native invocation document is no longer available.".to_owned())?
    }
    fn with_native_invocation_effect_extra(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let target = document.target();
        let registered = same_registered_origin(target, origin)
            .ok_or("The native invocation document is no longer available.")?;
        let operation = target.operation();
        let mut controls = vec![operation.cancel(), operation.shutdown()];
        controls.extend_from_slice(additional);
        with_many_owner_generations_transition(
            origin,
            participants,
            None,
            &[operation.authority_generation(), target.created()],
            &controls,
            cancel,
            pending,
            |_| target.with_registered_state(registered.owner(), effect),
        )
        .flatten()
        .ok_or_else(|| "The native invocation document is no longer available.".to_owned())?
    }

    fn with_native_operation_effect(
        &self,
        operation: &shell_contract::ipc::NativeInvocationOperation,
        origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_startup_operation_effect(operation, origin, participants, cancel, pending, effect)
            .ok_or_else(|| "The startup operation is no longer available.".to_owned())?
    }
    fn with_prepared_saved_publication(
        &self,
        publication: &shell_contract::ipc::SavedPublication,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        use_plan: &mut dyn FnMut(
            &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut owners = Vec::new();
        let mut controls = Vec::new();
        let mut generations = Vec::new();
        match publication {
            shell_contract::ipc::SavedPublication::Native(value) => {
                let operation = value.selection().binding().operation();
                owners.extend_from_slice(value.participants());
                controls.extend([value.cancel(), value.pending()]);
                controls.extend(value.additional().iter());
                controls.extend([operation.cancel(), operation.shutdown()]);
                generations.push(operation.authority_generation());
            }
            shell_contract::ipc::SavedPublication::Renderer(value) => {
                owners.push(value.destination().clone());
                owners.extend_from_slice(value.participants());
                controls.extend([value.cancel(), value.pending()]);
                controls.extend(value.additional().iter());
            }
        }
        owners.extend_from_slice(participants);
        controls.extend_from_slice(additional);
        let mut plan = prepare_many_owner_generations_transition(
            publication.source_owner(),
            &owners,
            None,
            &generations,
            &controls,
            cancel,
            pending,
        )
        .ok_or("The saved publication authority is unavailable")?;
        let mut attempted = false;
        let mut admitted = false;
        let mut panic_payload = None;
        let mut admit = |effect: &mut dyn FnMut() -> Result<(), String>| {
            if attempted {
                return Err("The saved publication plan was already consumed".into());
            }
            attempted = true;
            let result = plan
                .with_live(|_| {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut *effect)) {
                        Ok(result) => result,
                        Err(payload) => {
                            panic_payload = Some(payload);
                            Err("The saved publication effect panicked".into())
                        }
                    }
                })
                .ok_or_else(|| "The saved publication authority expired".to_owned())?;
            admitted = result.is_ok();
            result
        };
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| use_plan(&mut admit)));
        drop(plan);
        if let Some(payload) = panic_payload {
            std::panic::resume_unwind(payload);
        }
        match result {
            Err(payload) => std::panic::resume_unwind(payload),
            Ok(Ok(())) if !admitted => Err("The saved publication plan was not admitted".into()),
            Ok(result) => result,
        }
    }
    fn with_existing_document_save_effect(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_native_invocation_operation_effect(
            binding.operation(),
            binding.owner(),
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The existing document save is no longer available.".to_owned())?
    }
    fn with_existing_document_save_effect_extra(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let operation = binding.operation();
        let mut controls = Vec::with_capacity(additional.len() + 2);
        controls.extend([operation.cancel(), operation.shutdown()]);
        controls.extend_from_slice(additional);
        with_many_owner_generations_transition(
            binding.owner(),
            participants,
            None,
            &[operation.authority_generation()],
            &controls,
            cancel,
            pending,
            |_| effect(),
        )
        .ok_or_else(|| "The existing document save is no longer available.".to_owned())?
    }
    fn with_existing_document_same_origin_effect(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        actual: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let expected = binding.owner();
        if expected.view != actual.view
            || !proof(expected)
                .zip(proof(actual))
                .is_some_and(|(expected, actual)| expected.origin == actual.origin)
            || !expected
                .context
                .renderer()
                .zip(actual.context.renderer())
                .is_some_and(|(a, b)| a.same_generation(b))
        {
            return Err("The native document origin does not match.".into());
        }
        let mut captured = Vec::with_capacity(participants.len() + 1);
        captured.push(actual.clone());
        captured.extend_from_slice(participants);
        with_existing_document_save_many_effect(
            binding, &captured, cancel, pending, additional, effect,
        )
        .ok_or_else(|| "The existing document save is no longer available.".to_owned())?
    }
    fn with_saved_reload_acceptance_effect(
        &self,
        input: &shell_contract::ipc::NativeSavedInput,
        path: &shell_contract::ipc::NativeSavedPath,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if !path.matches_publication(input.output()) {
            return Err("The PDF reload publication changed".into());
        }
        let (snapshot, moved) = match path {
            shell_contract::ipc::NativeSavedPath::Selected(_) => {
                if !input.output().commit().same_commit(input.source_commit()) {
                    return Err(
                        "The PDF reload input does not match its selected publication".into(),
                    );
                }
                (None, false)
            }
            shell_contract::ipc::NativeSavedPath::DriveMoved(receipt) => (
                Some(shell_ipc_a::drive::preflight_committed_reopen(receipt)?),
                true,
            ),
            shell_contract::ipc::NativeSavedPath::DriveFallback(receipt) => (
                Some(shell_ipc_a::drive::preflight_committed_reopen(receipt)?),
                false,
            ),
        };
        let mut owners = participants.to_vec();
        let mut controls = additional.to_vec();
        if let Some(snapshot) = &snapshot {
            owners.extend_from_slice(snapshot.participants());
            controls.extend(snapshot.controls().iter());
        }
        self.with_prepared_saved_publication(
            input.output(),
            &owners,
            cancel,
            pending,
            &controls,
            &mut |admit| match &snapshot {
                Some(snapshot) => shell_ipc_a::drive::with_committed_reopen_snapshot(
                    snapshot,
                    moved,
                    input.source_commit(),
                    &mut || admit(effect),
                ),
                None => admit(effect),
            },
        )
    }
    fn with_native_save_reload_completion_effect(
        &self,
        handoff: &shell_contract::hosts::NativeSaveReloadHandoff,
        loaded: &NativeRequestOwner,
        before_notify: &mut dyn FnMut(),
        effect: &mut dyn FnMut(
            &mut shell_contract::ipc::AdmittedPdfReloadRetirement<'_, '_>,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        use crate::views::{AssetLeaseGatePlan, SlidesViewLeaseGatePlan};
        use main_core::docs::byte_handoff::MediaPermitGatePlan;
        use shell_contract::{
            ipc::PdfDocumentGatePlan,
            runtime::{CancelTokenGatePlan, RendererGenerationGatePlan},
        };
        let mut participants = handoff.completion_participants(loaded)?;
        let mut publication_controls = Vec::new();
        let mut publication_generations = Vec::new();
        match handoff.publication() {
            shell_contract::ipc::SavedPublication::Native(value) => {
                let operation = value.selection().binding().operation();
                publication_controls.extend([
                    operation.cancel(),
                    operation.shutdown(),
                    value.cancel(),
                    value.pending(),
                ]);
                publication_controls.extend(value.additional().iter());
                publication_generations.push(operation.authority_generation());
                participants.extend(value.participants().iter().map(|owner| {
                    if handoff.retires(owner) {
                        loaded.clone()
                    } else {
                        owner.clone()
                    }
                }));
            }
            shell_contract::ipc::SavedPublication::Renderer(value) => {
                publication_controls.extend([value.cancel(), value.pending()]);
                publication_controls.extend(value.additional().iter());
                participants.push(if handoff.retires(value.destination()) {
                    loaded.clone()
                } else {
                    value.destination().clone()
                });
                participants.extend(value.participants().iter().map(|owner| {
                    if handoff.retires(owner) {
                        loaded.clone()
                    } else {
                        owner.clone()
                    }
                }));
            }
        }
        let owners: Vec<_> = std::iter::once(loaded).chain(participants.iter()).collect();
        let proofs: Vec<_> = owners
            .iter()
            .map(|owner| proof(owner))
            .collect::<Option<_>>()
            .ok_or("The native owner is unavailable.")?;
        let losses: Vec<_> = owners
            .iter()
            .map(|owner| owner.loss_token())
            .collect::<Option<_>>()
            .ok_or("The owner lifetime is unavailable.")?;
        let mut native_tokens = Vec::with_capacity(owners.len());
        let mut assets = Vec::new();
        let mut generations = Vec::new();
        let mut documents = Vec::new();
        let mut slides = Vec::new();
        let mut media = Vec::new();
        let mut pdf = vec![
            handoff
                .predecessor()
                .pdf_document()
                .ok_or("The previous PDF slot is unavailable.")?,
            handoff
                .predecessor()
                .pdf_revision()
                .ok_or("The previous PDF revision is unavailable.")?,
        ];
        for (owner, authority) in owners.iter().zip(&proofs) {
            native_tokens.push(&authority.origin);
            generations.push(
                owner
                    .context
                    .renderer()
                    .ok_or("The renderer is unavailable.")?,
            );
            if let Some(document) = owner.context.document_lease() {
                generations.push(document);
                documents.push(document);
            }
            if authority.docs_bound && authority.docs_permit.is_none() {
                return Err("The Docs permit is unavailable.".into());
            }
            if let Some(permit) = authority.docs_permit.as_ref() {
                media.push(permit);
            }
            if let Some(asset) = owner.context.downcast_ref::<NativeAssetAuthorization>() {
                if asset.view != owner.view || !asset.matches_native_context(&authority.origin) {
                    return Err("The asset owner changed.".into());
                }
                assets.push(asset);
            }
            if let Some(lease) = owner.context.downcast_ref::<SlidesViewLease>() {
                slides.push(lease);
            }
            if let Some(slot) = owner.pdf_document() {
                pdf.push(slot);
            }
            if let Some(revision) = owner.pdf_revision() {
                pdf.push(revision);
            }
        }
        generations.extend(publication_generations);
        let mut generations = RendererGenerationGatePlan::from_generations(generations);
        let mut document_retirement = generations.prepare_retirement(&documents)?;
        let native = crate::native::DocumentGatePlan::new(&native_tokens)
            .ok_or("The native owner is unavailable.")?;
        let assets = AssetLeaseGatePlan::new(&assets);
        let mut slides = SlidesViewLeaseGatePlan::new(&slides);
        let mut media = MediaPermitGatePlan::new(&media);
        let mut pdf = PdfDocumentGatePlan::new(&pdf);
        let mut pdf_retirement = pdf.prepare_reload_retirement(handoff.predecessor(), loaded)?;
        let mut controls = vec![
            handoff.cancel(),
            handoff.pending(),
            handoff.browser_loss(),
            handoff.reservation_loss(),
        ];
        controls.extend(publication_controls);
        controls.extend(losses.iter());
        controls.extend(handoff.additional().iter());
        let mut controls = CancelTokenGatePlan::from_tokens(controls);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            native.with_live(|| assets.with_lease_live_fenced(|| generations.with_active_retiring(
            &mut document_retirement, |_| slides.with_active(|| media.with_active(|| pdf.with_live_reloading(
                &mut pdf_retirement, |retirement| controls.with_active(|| {
                    effect(retirement)?;
                    if !retirement.is_retired() { return Err("The PDF predecessor was not retired.".into()); }
                    handoff.record_completion_admitted(loaded, retirement)
                })).flatten()).flatten()).flatten()
        ).flatten()).flatten()).flatten().ok_or("The native save reload completion is no longer available.")?
        }));
        let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(before_notify));
        drop(pdf_retirement);
        match (result, cleanup) {
            (Err(panic), _) | (Ok(_), Err(panic)) => std::panic::resume_unwind(panic),
            (Ok(result), Ok(())) => result,
        }
    }
    fn with_native_save_reload_parser_effect(
        &self,
        handoff: &shell_contract::hosts::NativeSaveReloadHandoff,
        actual: &NativeRequestOwner,
        actual_cancel: &CancelToken,
        effect: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
    ) -> Result<(), String> {
        let registered = handoff
            .completed_owner()
            .ok_or("The PDF reload successor is not registered")?;
        let expected =
            proof(&registered).ok_or("The registered PDF native origin is unavailable")?;
        let sender = proof(actual).ok_or("The PDF parser native origin is unavailable")?;
        if actual.view != registered.view
            || sender.origin != expected.origin
            || !actual
                .context
                .renderer()
                .zip(registered.context.renderer())
                .is_some_and(|(actual, expected)| actual.same_generation(expected))
        {
            return Err("The PDF parser does not match the registered successor".into());
        }
        let mut owners = handoff.completion_participants(&registered)?;
        owners.push(actual.clone());
        let mut controls = vec![
            handoff.browser_loss(),
            handoff.reservation_loss(),
            handoff.pending(),
        ];
        controls.extend(handoff.additional().iter());
        if matches!(
            handoff.path(),
            shell_contract::ipc::NativeSavedPath::Selected(_)
        ) && !handoff
            .publication()
            .commit()
            .same_commit(handoff.input().source_commit())
        {
            return Err("The PDF reload input does not match its selected publication".into());
        }
        let (drive, moved) = match handoff.path() {
            shell_contract::ipc::NativeSavedPath::Selected(_) => (None, false),
            shell_contract::ipc::NativeSavedPath::DriveMoved(receipt) => (
                Some(shell_ipc_a::drive::preflight_committed_reopen(receipt)?),
                true,
            ),
            shell_contract::ipc::NativeSavedPath::DriveFallback(receipt) => (
                Some(shell_ipc_a::drive::preflight_committed_reopen(receipt)?),
                false,
            ),
        };
        if let Some(snapshot) = &drive {
            owners.extend(snapshot.participants().iter().map(|owner| {
                if handoff.retires(owner) {
                    registered.clone()
                } else {
                    owner.clone()
                }
            }));
            controls.extend(snapshot.controls().iter());
        }
        let mut generations = Vec::new();
        match handoff.publication() {
            shell_contract::ipc::SavedPublication::Native(value) => {
                let operation = value.selection().binding().operation();
                controls.extend([
                    operation.cancel(),
                    operation.shutdown(),
                    value.cancel(),
                    value.pending(),
                ]);
                controls.extend(value.additional().iter());
                generations.push(operation.authority_generation());
                owners.extend(value.participants().iter().map(|owner| {
                    if handoff.retires(owner) {
                        registered.clone()
                    } else {
                        owner.clone()
                    }
                }));
            }
            shell_contract::ipc::SavedPublication::Renderer(value) => {
                controls.extend([value.cancel(), value.pending()]);
                controls.extend(value.additional().iter());
                owners.push(if handoff.retires(value.destination()) {
                    registered.clone()
                } else {
                    value.destination().clone()
                });
                owners.extend(value.participants().iter().map(|owner| {
                    if handoff.retires(owner) {
                        registered.clone()
                    } else {
                        owner.clone()
                    }
                }));
            }
        }
        let mut plan = prepare_many_owner_generations_transition(
            &registered,
            &owners,
            None,
            &generations,
            &controls,
            actual_cancel,
            handoff.cancel(),
        )
        .ok_or("The PDF reload parser authority is unavailable")?;
        let mut panic_payload = None;
        let mut enter = || {
            plan.with_live(|_| {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handoff.with_completed_owner(&registered, &mut || effect(handoff.input()))
                })) {
                    Ok(result) => result,
                    Err(payload) => {
                        panic_payload = Some(payload);
                        Err("The PDF reload parser effect panicked".into())
                    }
                }
            })
            .ok_or_else(|| "The PDF reload parser authority is unavailable".to_owned())?
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &drive {
            Some(snapshot) => shell_ipc_a::drive::with_committed_reopen_snapshot(
                snapshot,
                moved,
                handoff.input().source_commit(),
                &mut enter,
            ),
            None => enter(),
        }));
        drop(plan);
        if let Some(payload) = panic_payload {
            std::panic::resume_unwind(payload);
        }
        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
    fn with_native_save_reload_reader_effect(
        &self,
        receipt: &shell_contract::hosts::NativeSaveReloadReceipt,
        actual: &NativeRequestOwner,
        actual_cancel: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let registered = receipt.successor();
        let expected =
            proof(registered).ok_or("The registered PDF native origin is unavailable")?;
        let sender = proof(actual).ok_or("The PDF reader native origin is unavailable")?;
        if actual.view != registered.view
            || sender.origin != expected.origin
            || !actual
                .context
                .renderer()
                .zip(registered.context.renderer())
                .is_some_and(|(actual, expected)| actual.same_generation(expected))
        {
            return Err("The PDF reader does not match the registered successor".into());
        }
        with_many_owner_generations_transition(
            registered,
            std::slice::from_ref(actual),
            None,
            &[],
            &[],
            actual_cancel,
            actual_cancel,
            |_| effect(),
        )
        .ok_or_else(|| "The PDF successor document is unavailable".to_owned())?
    }
    fn with_native_save_reload_effect(
        &self,
        receipt: &shell_contract::hosts::NativeSaveReloadReceipt,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.with_native_save_reload_many_effect(
            receipt,
            receipt.successor(),
            &[],
            receipt.cancel(),
            receipt.pending(),
            effect,
        )
    }
    fn with_native_save_reload_many_effect_extra(
        &self,
        receipt: &shell_contract::hosts::NativeSaveReloadReceipt,
        owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let publication = receipt.publication();
        let mut owners = Vec::with_capacity(receipt.participants().len() + participants.len() + 3);
        owners.extend_from_slice(receipt.participants());
        owners.push(
            receipt
                .owner_after_reload(publication.source_owner())
                .clone(),
        );
        owners.push(receipt.owner_after_reload(owner).clone());
        owners.extend(
            participants
                .iter()
                .map(|owner| receipt.owner_after_reload(owner).clone()),
        );
        let mut controls = vec![
            receipt.browser_loss(),
            receipt.reservation_loss(),
            receipt.cancel(),
            receipt.pending(),
        ];
        controls.extend(receipt.additional().iter());
        controls.extend_from_slice(additional);
        let mut generations = Vec::new();
        match publication {
            shell_contract::ipc::SavedPublication::Native(value) => {
                let operation = value.selection().binding().operation();
                controls.extend([
                    operation.cancel(),
                    operation.shutdown(),
                    value.cancel(),
                    value.pending(),
                ]);
                controls.extend(value.additional().iter());
                generations.push(operation.authority_generation());
                owners.extend(
                    value
                        .participants()
                        .iter()
                        .map(|owner| receipt.owner_after_reload(owner).clone()),
                );
            }
            shell_contract::ipc::SavedPublication::Renderer(value) => {
                controls.extend([value.cancel(), value.pending()]);
                controls.extend(value.additional().iter());
                owners.push(receipt.owner_after_reload(value.destination()).clone());
                owners.extend(
                    value
                        .participants()
                        .iter()
                        .map(|owner| receipt.owner_after_reload(owner).clone()),
                );
            }
        }
        with_many_owner_generations_transition(
            receipt.successor(),
            &owners,
            None,
            &generations,
            &controls,
            cancel,
            pending,
            |_| effect(),
        )
        .ok_or_else(|| "The completed native save reload is no longer available.".to_owned())?
    }

    fn with_startup_recovery_creation_effect(
        &self,
        startup: &shell_contract::ipc::NativeInvocationOperation,
        shell: &NativeRequestOwner,
        child: &shell_contract::ipc::NativeInvocationOperation,
        created: &RendererGeneration,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_startup_recovery_creation_effect(
            startup,
            shell,
            child,
            created,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The startup recovery constructor is no longer available.".to_owned())?
    }
    fn with_startup_recovery_seed_effect(
        &self,
        startup: &shell_contract::ipc::NativeInvocationOperation,
        shell: &NativeRequestOwner,
        target: &shell_contract::ipc::NativeInvocationTarget,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_startup_recovery_seed_effect(
            startup,
            shell,
            target,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The startup recovery seed is no longer available.".to_owned())?
    }
    fn with_startup_recovery_effect(
        &self,
        startup: &shell_contract::ipc::NativeInvocationOperation,
        shell: &NativeRequestOwner,
        child: &shell_contract::ipc::NativeInvocationDocument,
        owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_startup_recovery_effect(
            startup,
            shell,
            child,
            owner,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The startup recovery document is no longer available.".to_owned())?
    }

    fn with_command_recovery_target_effect(
        &self,
        command: &shell_contract::ipc::NativeInvocationOperation,
        child: &shell_contract::ipc::NativeInvocationDocument,
        owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_command_recovery_target_effect(
            command,
            child,
            owner,
            participants,
            cancel,
            pending,
            effect,
        )
        .ok_or_else(|| "The command recovery document is no longer available.".to_owned())?
    }

    fn with_recovery_seed_effect(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_recovery_seed_effect(target, effect)
            .ok_or_else(|| "The recovery constructor is no longer available.".to_owned())?
    }
    fn with_recovery_seed_snapshot(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_recovery_seed_snapshot(target, snapshot, effect)
            .ok_or_else(|| "The recovery constructor is no longer available.".to_owned())?
    }

    fn commit_recovery_seed(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        enqueue: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<shell_contract::ipc::RecoverySeedReceipt, String> {
        commit_recovery_seed(target, enqueue)
    }
    fn commit_recovery_seed_snapshot(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        enqueue: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<shell_contract::ipc::RecoverySeedReceipt, String> {
        commit_recovery_seed_snapshot(target, snapshot, enqueue)
    }

    fn with_recovery_open_effect(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        cancel: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_recovery_open_effect(receipt, receiver, cancel, effect)
            .ok_or_else(|| "The recovery document is no longer available.".to_owned())?
    }
    fn with_recovery_open_snapshot(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        receiver: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_recovery_open_service_snapshot(
            receipt,
            snapshot,
            receiver,
            participants,
            None,
            None,
            cancel,
            Some(pending),
            &[],
            effect,
        )
        .ok_or_else(|| "The recovery document is no longer available.".to_owned())?
    }

    fn with_recovery_open_effect_pending(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_recovery_open_service_effect(
            receipt,
            receiver,
            &[],
            None,
            None,
            cancel,
            Some(pending),
            effect,
        )
        .ok_or_else(|| "The recovery document is no longer available.".to_owned())?
    }

    fn with_recovery_open_effect_many(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_recovery_open_service_effect(
            receipt,
            receiver,
            participants,
            None,
            None,
            cancel,
            Some(pending),
            effect,
        )
        .ok_or_else(|| "The recovery document is no longer available.".to_owned())?
    }

    fn with_recovery_open_effect_many_extra(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_recovery_open_service_effect_extra(
            receipt,
            receiver,
            participants,
            None,
            None,
            cancel,
            Some(pending),
            additional,
            effect,
        )
        .ok_or_else(|| "The recovery document is no longer available.".to_owned())?
    }

    fn with_recovery_pdf_revision_transition(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        old: &PdfDocumentLease,
        successor: &PdfDocumentLease,
        renewal: &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal,
        before_notify: &mut dyn FnMut(),
        effect: &mut dyn FnMut(
            &mut shell_contract::ipc::AdmittedPdfRevisionRetirement<'_, '_>,
            &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        with_recovery_pdf_revision_transition(
            receipt,
            receiver,
            participants,
            cancel,
            pending,
            old,
            successor,
            renewal,
            before_notify,
            effect,
        )
    }
    fn with_native_save_revision_transition(
        &self,
        prepared: &mut shell_contract::ipc::PreparedNativeSaveRevision,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        before_notify: &mut dyn FnMut(),
        effect: &mut dyn FnMut(
            &mut shell_contract::ipc::AdmittedPdfRevisionSet<'_, '_>,
            &mut shell_contract::ipc::PreparedNativeSaveRevision,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        with_native_save_revision_transition(
            prepared,
            participants,
            cancel,
            pending,
            additional,
            before_notify,
            effect,
        )
    }
    fn emit_native_save_revision_snapshot(
        &self,
        snapshot: &shell_contract::ipc::NativeSaveRevisionSnapshot,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?;
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let mut owners = snapshot.aliases().to_vec();
        owners.extend_from_slice(participants);
        let queued = with_existing_document_save_many_effect(
            snapshot.selection().binding(),
            &owners,
            cancel,
            pending,
            additional,
            || {
                snapshot.with_current(|| {
                    outlet.enqueue_generation_fenced(target.view, generation, frame)
                })
            },
        )
        .flatten()
        .ok_or("The native save snapshot was replaced.")??;
        drain_event(queued);
        Ok(())
    }

    fn post_ui_native_save_revision_snapshot(
        &self,
        snapshot: shell_contract::ipc::NativeSaveRevisionSnapshot,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        self.cx.ui.post(Box::new(move || {
            let mut owners = snapshot.aliases().to_vec();
            owners.extend_from_slice(&participants);
            let controls: Vec<_> = additional.iter().collect();
            let admitted = with_existing_document_save_many_effect(
                snapshot.selection().binding(),
                &owners,
                &cancel,
                &pending,
                &controls,
                || snapshot.with_current(|| action),
            )
            .flatten()
            .ok_or_else(|| "The native save snapshot was replaced.".to_owned());
            let result = admitted.and_then(|action| action());
            done(result);
        }));
    }

    fn emit_recovery_open_many(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?
            .clone();
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let view = target.view;
        let queued = with_recovery_open_service_effect(
            receipt,
            receiver,
            participants,
            None,
            None,
            cancel,
            Some(pending),
            || outlet.enqueue_generation_fenced(view, &generation, frame),
        )
        .ok_or("The recovery document is no longer available.")??;
        drain_event(queued);
        Ok(())
    }
    fn emit_recovery_open_snapshot(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        receiver: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        target: usize,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = participants
            .get(target)
            .ok_or("The captured event target is unavailable.")?;
        let generation = target
            .context
            .renderer()
            .ok_or("Renderer document is no longer available.")?
            .clone();
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let queued = with_recovery_open_service_snapshot(
            receipt,
            snapshot,
            receiver,
            participants,
            None,
            None,
            cancel,
            Some(pending),
            &[],
            || outlet.enqueue_generation_fenced(target.view, &generation, frame),
        )
        .ok_or("The recovery document is no longer available.")??;
        drain_event(queued);
        Ok(())
    }

    fn settle_recovery_seed(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        cancel: &CancelToken,
        result: Result<(), String>,
    ) -> Result<(), String> {
        let mut deferred = DeferredSettlement(None);
        with_recovery_open_effect(receipt, receiver, cancel, || {
            deferred.0 = Some(receipt.settle_from_admitted(result)?);
            Ok(())
        })
        .ok_or_else(|| "The recovery document is no longer available.".to_owned())?
    }
    fn settle_recovery_seed_snapshot(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        receiver: &NativeRequestOwner,
        cancel: &CancelToken,
        result: Result<(), String>,
    ) -> Result<(), String> {
        let mut deferred = DeferredSettlement(None);
        with_recovery_open_service_snapshot(
            receipt,
            snapshot,
            receiver,
            &[],
            None,
            None,
            cancel,
            None,
            &[],
            || {
                deferred.0 = Some(receipt.settle_from_admitted(result)?);
                Ok(())
            },
        )
        .ok_or_else(|| "The recovery document is no longer available.".to_owned())?
    }

    fn with_recovery_dialog_effect(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        dialog: &NativeRequestOwner,
        dialog_generation: &RendererGeneration,
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_recovery_dialog_effect(target, dialog, dialog_generation, cancel, pending, effect)
            .ok_or_else(|| "The protected recovery dialog is no longer available.".to_owned())?
    }
    fn with_recovery_dialog_snapshot(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        dialog: &NativeRequestOwner,
        dialog_generation: &RendererGeneration,
        cancel: &CancelToken,
        pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if !dialog
            .context
            .renderer()
            .is_some_and(|generation| generation.same_generation(dialog_generation))
        {
            return Err("The protected recovery dialog is no longer available.".into());
        }
        with_recovery_pre_navigation_snapshot(
            target,
            snapshot,
            Some(dialog_generation),
            Some(dialog),
            Some((cancel, pending)),
            effect,
        )
        .ok_or_else(|| "The protected recovery dialog is no longer available.".to_owned())?
    }

    fn emit_recovery_dialog(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        dialog: &NativeRequestOwner,
        dialog_generation: &RendererGeneration,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let queued =
            with_recovery_dialog_effect(target, dialog, dialog_generation, cancel, pending, || {
                outlet.enqueue_generation_fenced(dialog.view, dialog_generation, frame)
            })
            .ok_or_else(|| "The protected recovery dialog is no longer available.".to_owned())??;
        drain_event(queued);
        Ok(())
    }
    fn emit_recovery_dialog_snapshot(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        dialog: &NativeRequestOwner,
        dialog_generation: &RendererGeneration,
        cancel: &CancelToken,
        pending: &CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        if !dialog
            .context
            .renderer()
            .is_some_and(|generation| generation.same_generation(dialog_generation))
        {
            return Err("The protected recovery dialog is no longer available.".into());
        }
        let frame = shell_core::outlet::event_frame(channel, &args);
        let ipc = self
            .cx
            .app
            .try_state::<crate::ipc::Ipc>()
            .ok_or("IPC output is not ready.")?;
        let outlet = ipc.outlet.clone();
        let queued = with_recovery_pre_navigation_snapshot(
            target,
            snapshot,
            Some(dialog_generation),
            Some(dialog),
            Some((cancel, pending)),
            || outlet.enqueue_generation_fenced(dialog.view, dialog_generation, frame),
        )
        .ok_or_else(|| "The protected recovery dialog is no longer available.".to_owned())??;
        drain_event(queued);
        Ok(())
    }

    fn post_ui_recovery_open_many(
        &self,
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        self.cx.ui.post(Box::new(move || {
            let mut action = Some(action);
            let mut accepted = None;
            let admitted = with_recovery_open_service_effect(
                &receipt,
                &receiver,
                &participants,
                None,
                None,
                &cancel,
                Some(&pending),
                || {
                    accepted = action.take();
                    Ok::<(), String>(())
                },
            )
            .ok_or_else(|| "The recovery document is no longer available.".to_owned())
            .and_then(|result| result);
            let result = admitted.and_then(|()| {
                (accepted.ok_or_else(|| "The recovery UI action was not admitted.".to_owned())?)()
            });
            done(result);
        }));
    }
    fn post_ui_existing_document_save(
        &self,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        self.cx.ui.post(Box::new(move || {
            let mut action = Some(action);
            let admitted = with_native_invocation_operation_effect(
                binding.operation(),
                binding.owner(),
                &participants,
                &cancel,
                &pending,
                || action.take(),
            )
            .flatten()
            .ok_or_else(|| "The existing document save is no longer available.".to_owned());
            let result = admitted.and_then(|action| action());
            done(result);
        }));
    }
    fn post_ui_existing_document_save_extra(
        &self,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        self.cx.ui.post(Box::new(move || {
            let mut action = Some(action);
            let operation = binding.operation();
            let mut controls = vec![operation.cancel(), operation.shutdown()];
            controls.extend(additional.iter());
            let admitted = with_many_owner_generations_transition(
                binding.owner(),
                &participants,
                None,
                &[operation.authority_generation()],
                &controls,
                &cancel,
                &pending,
                |_| action.take(),
            )
            .flatten()
            .ok_or_else(|| "The existing document save is no longer available.".to_owned());
            let result = admitted.and_then(|action| action());
            done(result);
        }));
    }

    fn post_ui_owned_many(
        &self,
        origin: NativeRequestOwner,
        destinations: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let ui = self.cx.ui.clone();
        let current_ui = ui.clone();
        ui.post(Box::new(move || {
            let result = run_accepted_ui_many(
                current_ui.as_ref(),
                &origin,
                &destinations,
                &cancel,
                &pending,
                action,
            )
            .and_then(|result| result);
            done(result);
        }));
    }
    fn post_ui_owned_many_extra(
        &self,
        origin: NativeRequestOwner,
        destinations: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        self.cx.ui.post(Box::new(move || {
            let mut action = Some(action);
            let controls: Vec<_> = additional.iter().collect();
            let admitted = with_many_owner_generations_transition(
                &origin,
                &destinations,
                None,
                &[],
                &controls,
                &cancel,
                &pending,
                |_| action.take(),
            )
            .flatten()
            .ok_or_else(|| "Renderer document is no longer available.".to_owned());
            let result = admitted.and_then(|action| action());
            done(result);
        }));
    }

    fn post_ui_owned(
        &self,
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let ui = self.cx.ui.clone();
        let current_ui = ui.clone();
        ui.post(Box::new(move || {
            let result = run_accepted_ui_pair(
                current_ui.as_ref(),
                &origin,
                &destination,
                &cancel,
                &pending,
                action,
            )
            .and_then(|result| result);
            done(result);
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::mpsc,
    };

    #[test]
    fn retirement_notification_unwinds_after_outer_and_inner_admission_guards() {
        let outer = RendererGeneration::new();
        let renderer = RendererGeneration::new();
        let document = RendererGeneration::new();
        let cancel = CancelToken::default();
        let observed_outer = outer.clone();
        let observed_renderer = renderer.clone();
        let observed_document = document.clone();
        let observed_cancel = cancel.clone();
        let (send, receive) = mpsc::channel();
        let _subscription = document.loss_token().on_cancel(move || {
            // Reenter both outer and inner admission categories. An observer
            // invoked from inside any held gate would deadlock here.
            let admitted = observed_outer
                .with_active(|| {
                    observed_renderer.with_active(|| observed_cancel.with_active(|| ()))
                })
                .flatten()
                .flatten()
                .is_some();
            let retired = observed_document.with_active(|| ()).is_none();
            send.send((admitted, retired)).unwrap();
        });
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut notifications = DeferredDocumentRetirements([None, None]);
            outer.with_active(|| {
                RendererGeneration::with_active_all_retiring(
                    [Some(&renderer), Some(&document)],
                    [Some(&document), None],
                    &mut notifications.0,
                    |retirement| {
                        cancel.with_active::<()>(|| {
                            retirement.retire(&document).unwrap();
                            assert!(receive.try_recv().is_err());
                            panic!("terminal transition unwound after logical retirement");
                        })
                    },
                )
            });
        }));
        assert!(result.is_err());
        assert_eq!(receive.recv().unwrap(), (true, true));
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn committed_preparation_notification_survives_unwind_outside_gate() {
        let generation = RendererGeneration::new();
        let loss = CancelToken::default();
        let observed = generation.clone();
        let (send, receive) = mpsc::channel();
        let _subscription =
            loss.on_cancel(move || send.send(observed.with_active(|| ()).is_some()).unwrap());
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut deferred = DeferredPreparation(None);
            generation.with_active(|| {
                deferred.0 = Some(loss);
                panic!("unwind after the preparation committed");
            });
        }));
        assert!(result.is_err());
        assert!(receive.recv().unwrap());
    }

    #[test]
    fn native_command_root_requires_issuer_selection_and_live_controls() {
        use shell_contract::ipc::{
            NativeEditorCommand, NativeInvocationIssuer, NativeInvocationSelection,
        };
        let shutdown = CancelToken::default();
        let issuer = NativeInvocationIssuer::new(shutdown.clone());
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let created = RendererGeneration::new();
        let command = NativeEditorCommand::OpenHome;
        let operation = issuer.issue_command(command.clone(), cancel.clone());
        let generic = issuer.issue(
            NativeInvocationSelection::NativeCommand(command.clone()),
            CancelToken::default(),
        );
        assert!(
            with_native_command_creation_effect(
                &generic,
                &command,
                &created,
                &[],
                &cancel,
                &pending,
                || ()
            )
            .is_none()
        );
        assert!(
            with_native_command_creation_effect(
                &operation,
                &NativeEditorCommand::OpenHelp { topic: None },
                &created,
                &[],
                &cancel,
                &pending,
                || ()
            )
            .is_none()
        );
        assert!(
            with_native_command_creation_effect(
                &operation,
                &command,
                &created,
                &[],
                &cancel,
                &pending,
                || ()
            )
            .is_some()
        );
        let help = NativeEditorCommand::OpenHelp {
            topic: Some("guide".into()),
        };
        let help_operation = issuer.issue_command(help.clone(), CancelToken::default());
        assert!(
            with_native_command_creation_effect(
                &help_operation,
                &help,
                &created,
                &[],
                &cancel,
                &pending,
                || ()
            )
            .is_some()
        );
        assert!(
            with_native_command_creation_effect(
                &help_operation,
                &NativeEditorCommand::OpenHelp { topic: None },
                &created,
                &[],
                &cancel,
                &pending,
                || ()
            )
            .is_none()
        );
        let shell_generation = RendererGeneration::new();
        let shell = NativeRequestOwner {
            view: ViewId(91),
            context: shell_contract::ipc::NativeContext::with_owner_loss(
                (),
                shell_generation,
                CancelToken::default(),
            ),
        };
        let startup = issuer.issue_startup(Vec::new(), false, shell, CancelToken::default());
        let child = issuer
            .issue_command_child(&startup, command.clone(), CancelToken::default())
            .unwrap();
        assert!(
            with_native_command_preparation_effect(&child, &command, &[], &cancel, &pending, || ())
                .is_none()
        );
        assert!(
            with_native_command_creation_effect(
                &child,
                &command,
                &created,
                &[],
                &cancel,
                &pending,
                || ()
            )
            .is_none()
        );
        let target = child.reserve_target(ViewId(92), created.clone()).unwrap();
        let target_owner = NativeRequestOwner {
            view: ViewId(92),
            context: shell_contract::ipc::NativeContext::with_owner_loss(
                (),
                created.clone(),
                CancelToken::default(),
            ),
        };
        target
            .register_from_native_admitted(target_owner.clone())
            .unwrap();
        let document = target.registered_document().unwrap();
        assert!(
            with_native_command_target_effect(
                &child,
                &command,
                &document,
                &target_owner,
                &[],
                &cancel,
                &pending,
                || ()
            )
            .is_none()
        );
        pending.cancel();
        assert!(
            with_native_command_creation_effect(
                &operation,
                &command,
                &created,
                &[],
                &cancel,
                &pending,
                || ()
            )
            .is_none()
        );
        shutdown.cancel();
        assert!(
            with_native_command_preparation_effect(
                &operation,
                &command,
                &[],
                &cancel,
                &CancelToken::default(),
                || ()
            )
            .is_none()
        );
    }
}
