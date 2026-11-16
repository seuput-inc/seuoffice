//! One admission for a committed native output and its exact browser lineage.

use shell_contract::ipc::{
    NativeRequestOwner, NativeSavedDocument, NativeSavedInput, NativeSavedPath, NativeSavedTarget,
    SavedPublication,
};
use shell_contract::runtime::CancelToken;

use super::effects::prepare_many_owner_generations_transition;

/// Read only the committed source object on a worker, before constructing a
/// blank actor. The final actor and document effects must still use the gates
/// below. A read error never changes the already-committed save result.
pub(crate) fn read_saved_input(
    output: &SavedPublication,
    path: &NativeSavedPath,
    max_bytes: usize,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
) -> std::io::Result<NativeSavedInput> {
    if !path.matches_publication(output) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "The saved source does not match the committed output",
        ));
    }
    match path {
        NativeSavedPath::Selected(_) => {
            output.read_input(max_bytes, participants, cancel, pending, additional)
        }
        NativeSavedPath::DriveMoved(receipt) | NativeSavedPath::DriveFallback(receipt) => {
            let mut controls = output
                .loss_tokens()
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::Interrupted, error))?;
            controls.extend([cancel.clone(), pending.clone()]);
            controls.extend_from_slice(additional);
            for owner in participants {
                controls.push(owner.loss_token().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "The saved-source participant is unavailable",
                    )
                })?);
                controls.push(
                    owner
                        .context
                        .renderer()
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::Interrupted,
                                "The saved-source renderer is unavailable",
                            )
                        })?
                        .loss_token(),
                );
            }
            let moved = matches!(path, NativeSavedPath::DriveMoved(_));
            let read =
                shell_ipc_a::drive::read_committed_reopen(receipt, moved, max_bytes, &controls)?;
            Ok(NativeSavedInput::from_drive_committed_read(
                output.clone(),
                read,
            ))
        }
    }
}

fn with_saved_target(
    target: &NativeSavedTarget,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: impl FnMut(
        &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    with_saved_target_generations(
        target,
        participants,
        cancel,
        pending,
        additional,
        &[],
        effect,
    )
}

fn with_saved_target_generations(
    target: &NativeSavedTarget,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    extra_generations: &[&shell_contract::runtime::RendererGeneration],
    mut effect: impl FnMut(
        &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    with_saved_target_generations_media(
        target,
        participants,
        cancel,
        pending,
        additional,
        extra_generations,
        None,
        |retirement, _| effect(retirement),
    )
}

pub(super) fn with_prepared_saved_target_media(
    target: &NativeSavedTarget,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    use_plan: impl FnOnce(
        &mut dyn FnMut(
            Option<(
                &main_core::docs::byte_handoff::MediaPermit,
                &main_core::docs::byte_handoff::MediaPermit,
                &mut main_core::docs::byte_handoff::DeferredMediaLoss,
            )>,
            &mut dyn FnMut(
                &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
                Option<&mut main_core::docs::byte_handoff::AdmittedMediaRetirement<'_>>,
            ) -> Result<(), String>,
        ) -> Result<(), String>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    with_prepared_saved_target_generations_media(
        target,
        participants,
        cancel,
        pending,
        additional,
        &[],
        use_plan,
    )
}

fn with_saved_target_generations_media(
    target: &NativeSavedTarget,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    extra_generations: &[&shell_contract::runtime::RendererGeneration],
    media: Option<(
        &main_core::docs::byte_handoff::MediaPermit,
        &main_core::docs::byte_handoff::MediaPermit,
        &mut main_core::docs::byte_handoff::DeferredMediaLoss,
    )>,
    mut effect: impl FnMut(
        &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
        Option<&mut main_core::docs::byte_handoff::AdmittedMediaRetirement<'_>>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    with_prepared_saved_target_generations_media(
        target,
        participants,
        cancel,
        pending,
        additional,
        extra_generations,
        |admit| admit(media, &mut effect),
    )
}

fn with_prepared_saved_target_generations_media(
    target: &NativeSavedTarget,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    extra_generations: &[&shell_contract::runtime::RendererGeneration],
    use_plan: impl FnOnce(
        &mut dyn FnMut(
            Option<(
                &main_core::docs::byte_handoff::MediaPermit,
                &main_core::docs::byte_handoff::MediaPermit,
                &mut main_core::docs::byte_handoff::DeferredMediaLoss,
            )>,
            &mut dyn FnMut(
                &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
                Option<&mut main_core::docs::byte_handoff::AdmittedMediaRetirement<'_>>,
            ) -> Result<(), String>,
        ) -> Result<(), String>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    let publication = target.output();
    let (drive, moved) = match target.path() {
        NativeSavedPath::Selected(_) => (None, false),
        NativeSavedPath::DriveMoved(receipt) => (
            Some(shell_ipc_a::drive::preflight_committed_reopen(receipt)?),
            true,
        ),
        NativeSavedPath::DriveFallback(receipt) => (
            Some(shell_ipc_a::drive::preflight_committed_reopen(receipt)?),
            false,
        ),
    };
    let drive_participants = drive
        .as_ref()
        .map_or(&[][..], |snapshot| snapshot.participants());
    let drive_controls = drive
        .as_ref()
        .map_or(&[][..], |snapshot| snapshot.controls());
    let mut owners = Vec::with_capacity(
        target.participants().len() + participants.len() + drive_participants.len(),
    );
    owners.extend_from_slice(target.participants());
    owners.extend_from_slice(participants);
    owners.extend_from_slice(drive_participants);
    let mut controls =
        Vec::with_capacity(target.additional().len() + additional.len() + drive_controls.len() + 4);
    controls.extend([target.cancel(), target.pending()]);
    controls.extend(target.additional().iter());
    controls.extend_from_slice(additional);
    controls.extend(drive_controls.iter());
    let mut generations = Vec::with_capacity(extra_generations.len() + 2);
    generations.push(target.created());
    match publication {
        SavedPublication::Native(value) => {
            let operation = value.selection().binding().operation();
            owners.extend_from_slice(value.participants());
            controls.extend([
                operation.cancel(),
                operation.shutdown(),
                value.cancel(),
                value.pending(),
            ]);
            controls.extend(value.additional().iter());
            generations.push(operation.authority_generation());
        }
        SavedPublication::Renderer(value) => {
            owners.push(value.destination().clone());
            owners.extend_from_slice(value.participants());
            controls.extend([value.cancel(), value.pending()]);
            controls.extend(value.additional().iter());
        }
    }
    generations.extend_from_slice(extra_generations);
    let mut plan = prepare_many_owner_generations_transition(
        publication.source_owner(),
        &owners,
        None,
        &generations,
        &controls,
        cancel,
        pending,
    )
    .ok_or("The committed saved-output authority is no longer available.")?;
    let mut panic_payload = None;
    let mut admit = |mut media: Option<(
        &main_core::docs::byte_handoff::MediaPermit,
        &main_core::docs::byte_handoff::MediaPermit,
        &mut main_core::docs::byte_handoff::DeferredMediaLoss,
    )>,
                     effect: &mut dyn FnMut(
        &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
        Option<&mut main_core::docs::byte_handoff::AdmittedMediaRetirement<'_>>,
    ) -> Result<(), String>| {
        let mut enter = || {
            plan.with_media(media.take(), |retirement, media_retirement| {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    effect(retirement, media_retirement)
                })) {
                    Ok(result) => result,
                    Err(payload) => {
                        panic_payload = Some(payload);
                        Err("The saved-output effect panicked.".into())
                    }
                }
            })
            .ok_or_else(|| {
                "The committed saved-output authority is no longer available.".to_owned()
            })?
        };
        match &drive {
            None => enter(),
            Some(snapshot) => shell_ipc_a::drive::with_committed_reopen_snapshot(
                snapshot,
                moved,
                target.input().source_commit(),
                &mut enter,
            ),
        }
    };
    // Both the caller's lease and the Drive locks must be gone before plan
    // retirement can notify observers, including when a callback panics.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| use_plan(&mut admit)));
    drop(admit);
    drop(plan);
    if let Some(payload) = panic_payload {
        std::panic::resume_unwind(payload);
    }
    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// The browser's initial adoption, actor insertion, and saved-target adoption
/// run in one short transition. The caller must perform no asynchronous work
/// inside `effect`; its accepted constructor runs only after the gate drops.
pub(crate) fn with_saved_target_creation_effect(
    target: &NativeSavedTarget,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: &mut dyn FnMut(&shell_contract::ipc::NativeBrowserTransition<'_>) -> Result<(), String>,
) -> Result<(), String> {
    with_saved_target(target, participants, cancel, pending, additional, |_| {
        target
            .reservation()
            .with_initial_owner(target.browser(), |transition| {
                let adoption = target.validate_in_transition(transition)?;
                effect(transition)?;
                adoption.commit();
                Ok(())
            })
    })
}

/// Prepare the exact successor before actor and manager staging; no browser
/// ownership or saved-target state changes until the admission below commits.
pub(crate) struct PreparedSavedSpareReplacement {
    original: NativeSavedTarget,
    successor: NativeSavedTarget,
    replacement: shell_contract::ipc::NativeBrowserReplacement,
}

impl PreparedSavedSpareReplacement {
    pub(crate) fn successor(&self) -> &NativeSavedTarget {
        &self.successor
    }
}

pub(crate) fn prepare_saved_spare_replacement(
    target: &NativeSavedTarget,
) -> Result<PreparedSavedSpareReplacement, String> {
    let replacement = target.reservation().prepare_replacement(target.browser())?;
    let successor = target.prepare_successor(&replacement)?;
    Ok(PreparedSavedSpareReplacement {
        original: target.clone(),
        successor,
        replacement,
    })
}

/// Replace a retained registered spare's browser owner atomically with the
/// actor/manager handoff and the successor's saved-target adoption.
pub(crate) fn with_saved_spare_replacement_effect(
    target: &NativeSavedTarget,
    prepared: PreparedSavedSpareReplacement,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: &mut dyn FnMut(
        &shell_contract::ipc::NativeBrowserTransition<'_>,
        &shell_contract::ipc::NativeBrowserOwner,
        &NativeSavedTarget,
    ) -> Result<(), String>,
) -> Result<
    (
        NativeSavedTarget,
        shell_contract::ipc::NativeBrowserSuperseded,
    ),
    String,
> {
    if !prepared.original.same_target(target)
        || !prepared.replacement.original().same_owner(target.browser())
        || !prepared
            .replacement
            .successor()
            .same_owner(prepared.successor.browser())
        || !prepared
            .successor
            .reservation()
            .same_reservation(target.reservation())
        || owner.view != target.view()
        || !owner
            .context
            .renderer()
            .is_some_and(|generation| generation.same_generation(target.created()))
    {
        return Err("The registered spare does not match its native reservation.".into());
    }
    // The retained owner's exact private native context record is marked
    // ready only by its verified native lifecycle event. Never inspect the
    // current view or infer readiness from registration.
    let origin = owner
        .context
        .authority_ref::<super::NativeOwnerAuthority>()
        .filter(|authority| authority.view == owner.view)
        .ok_or("The original spare native context is unavailable.")?;
    let ready = origin
        .origin
        .document_ready()
        .ok_or("The original spare document has not reported native readiness.")?;
    let mut captured = Vec::with_capacity(participants.len() + 1);
    captured.push(owner.clone());
    captured.extend_from_slice(participants);
    let successor_target = prepared.successor;
    let mut replacement = Some(prepared.replacement);
    let mut committed = None;
    with_saved_target(target, &captured, cancel, pending, additional, |_| {
        // The canonical owner plan already holds this exact native origin's
        // liveness gate. Reacquiring ready.with_live here would deadlock.
        if ready.origin() != &origin.origin {
            return Err("The spare readiness belongs to another native document.".into());
        }
        let (_, superseded, successor_target) = replacement
            .take()
            .ok_or("The saved spare replacement was already consumed.")?
            .commit(|transition, successor| {
                if !transition.matches_owner(target.browser())
                    || !transition.proposes_owner(successor)
                {
                    return Err("The saved spare replacement identity changed.".into());
                }
                let adoption = successor_target.validate_in_transition(transition)?;
                effect(transition, successor, &successor_target)?;
                adoption.commit();
                Ok(successor_target.clone())
            })?;
        committed = Some((successor_target, superseded));
        Ok(())
    })?;
    committed.ok_or_else(|| "The saved spare replacement was not committed.".into())
}

/// Enqueue the exact retained input into an already-adopted blank actor.
/// The callback only performs a short state update; parsing or I/O must run
/// after admission is released, and its result must use the document gate.
pub(crate) fn with_saved_target_seed_effect(
    target: &NativeSavedTarget,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
) -> Result<(), String> {
    with_saved_target(target, participants, cancel, pending, additional, |_| {
        target.reservation().with_owner(target.browser(), |_| {
            target
                .with_adopted_state(|| effect(target.input()))
                .ok_or_else(|| "The saved target was not adopted.".to_owned())?
        })
    })
}

/// Bind the first actual native registration to the retained creator target.
pub(crate) fn with_saved_target_registration_effect(
    target: &NativeSavedTarget,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: &mut dyn FnMut(&shell_contract::ipc::NativeBrowserTransition<'_>) -> Result<(), String>,
) -> Result<(), String> {
    if owner.view != target.view()
        || !owner
            .context
            .renderer()
            .is_some_and(|generation| generation.same_generation(target.created()))
    {
        return Err("The registered document does not match the saved target.".into());
    }
    let mut captured = Vec::with_capacity(participants.len() + 1);
    captured.push(owner.clone());
    captured.extend_from_slice(participants);
    with_saved_target(target, &captured, cancel, pending, additional, |_| {
        target
            .reservation()
            .with_owner(target.browser(), |transition| effect(transition))
    })
}

/// Effects on the first registered saved document keep the same reservation,
/// source save, original participants, cancellation and document identity.
pub(crate) fn with_saved_document_effect(
    document: &NativeSavedDocument,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let target = document.target();
    let owner = document.owner();
    let mut captured = Vec::with_capacity(participants.len() + 1);
    captured.push(owner.clone());
    captured.extend_from_slice(participants);
    with_saved_target(target, &captured, cancel, pending, additional, |_| {
        target.reservation().with_owner(target.browser(), |_| {
            target
                .with_registered_state(owner, || effect())
                .ok_or_else(|| "The registered saved document was replaced.".to_owned())?
        })
    })
}

/// Complete parser/open state only for the first registered exact renderer.
/// Worker parsing runs outside the gate and returns here before publication.
pub(crate) fn with_saved_document_input_effect(
    document: &NativeSavedDocument,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
) -> Result<(), String> {
    with_saved_document_effect(
        document,
        participants,
        cancel,
        pending,
        additional,
        &mut || effect(document.target().input()),
    )
}

/// Renderer-origin calls must match the first registered native document; the
/// caller's current IPC cancellation and both captured owners participate in
/// the same canonical admission as the original saved-output authority.
pub(crate) fn with_native_saved_document_transition(
    document: &NativeSavedDocument,
    actual: &NativeRequestOwner,
    actual_cancel: &CancelToken,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: &mut dyn FnMut(
        &NativeSavedInput,
        &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    let target = document.target();
    let registered = document.owner();
    let mut owners = Vec::with_capacity(participants.len() + 2);
    owners.push(registered.clone());
    owners.push(actual.clone());
    owners.extend_from_slice(participants);
    let mut controls = Vec::with_capacity(additional.len() + 1);
    controls.push(actual_cancel);
    controls.extend_from_slice(additional);
    with_saved_target(target, &owners, cancel, pending, &controls, |retirement| {
        let original = registered
            .context
            .authority_ref::<super::NativeOwnerAuthority>()
            .filter(|proof| proof.view == registered.view)
            .ok_or("The registered saved document is unavailable.")?;
        let caller = actual
            .context
            .authority_ref::<super::NativeOwnerAuthority>()
            .filter(|proof| proof.view == actual.view)
            .ok_or("The calling renderer is unavailable.")?;
        if actual.view != registered.view
            || caller.origin != original.origin
            || !actual
                .context
                .renderer()
                .zip(registered.context.renderer())
                .is_some_and(|(left, right)| left.same_generation(right))
        {
            return Err("The calling renderer does not match the saved document.".into());
        }
        target.reservation().with_owner(target.browser(), |_| {
            target
                .with_registered_state(registered, || effect(target.input(), retirement))
                .ok_or_else(|| "The registered saved document was replaced.".to_owned())?
        })
    })
}

pub(crate) fn with_native_saved_document_same_origin(
    document: &NativeSavedDocument,
    actual: &NativeRequestOwner,
    actual_cancel: &CancelToken,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: &mut dyn FnMut(&NativeSavedInput) -> Result<(), String>,
) -> Result<(), String> {
    with_native_saved_document_transition(
        document,
        actual,
        actual_cancel,
        participants,
        cancel,
        pending,
        additional,
        &mut |input, _| effect(input),
    )
}

/// Admit the saved document and, when present, the exact password-dialog
/// creation generation and registered owner in one canonical transition.
/// The callback only accepts a prepared UI action; execute that action after
/// this function returns and all admission guards have dropped.
pub(crate) fn with_saved_document_dialog_effect(
    document: &NativeSavedDocument,
    created: Option<&shell_contract::runtime::RendererGeneration>,
    dialog_owner: Option<&NativeRequestOwner>,
    cancel: &CancelToken,
    pending: &CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if let Some(owner) = dialog_owner {
        let generation = created.ok_or("The dialog creation identity is unavailable.")?;
        if !owner
            .context
            .renderer()
            .is_some_and(|actual| actual.same_generation(generation))
        {
            return Err("The dialog owner does not match its created renderer.".into());
        }
    }
    let target = document.target();
    let registered = document.owner();
    let mut owners = Vec::with_capacity(1 + usize::from(dialog_owner.is_some()));
    owners.push(registered.clone());
    if let Some(owner) = dialog_owner {
        owners.push(owner.clone());
    }
    let generations: Vec<_> = created.into_iter().collect();
    with_saved_target_generations(target, &owners, cancel, pending, &[], &generations, |_| {
        target.reservation().with_owner(target.browser(), |_| {
            target
                .with_registered_state(registered, || effect())
                .ok_or_else(|| "The registered saved document was replaced.".to_owned())?
        })
    })
}
