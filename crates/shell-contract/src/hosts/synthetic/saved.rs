use super::*;
use crate::ipc::{NativeSavedDocument, NativeSavedPath, NativeSavedTarget};
use crate::runtime::AdmittedDocumentRetirement;

/// Prepare all ownership gates before taking a Drive state lock. The Drive
/// receipt supplies state-only admission and never re-enters the owner gates.
pub(super) fn admit(
    model: &SyntheticOwnedEffects,
    target: &NativeSavedTarget,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: &mut dyn FnMut(&mut AdmittedDocumentRetirement<'_>) -> Result<(), String>,
) -> Result<(), String> {
    prepared(
        model,
        target,
        participants,
        cancel,
        pending,
        additional,
        |enter| enter(effect),
    )
}

pub(super) fn prepared(
    model: &SyntheticOwnedEffects,
    target: &NativeSavedTarget,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    use_plan: impl FnOnce(
        &mut dyn FnMut(
            &mut dyn FnMut(&mut AdmittedDocumentRetirement<'_>) -> Result<(), String>,
        ) -> Result<(), String>,
    ) -> Result<(), String>,
) -> Result<(), String> {
    let (drive, moved) = match target.path() {
        NativeSavedPath::Selected(_) => (None, false),
        NativeSavedPath::DriveMoved(receipt) => (Some(receipt.preflight_reopen()?), true),
        NativeSavedPath::DriveFallback(receipt) => (Some(receipt.preflight_reopen()?), false),
    };
    let publication = target.output();
    let mut owners: Vec<&NativeRequestOwner> = vec![publication.source_owner()];
    let mut generations = vec![target.created()];
    let mut publication_controls: Vec<&CancelToken> = Vec::new();
    match publication {
        crate::ipc::SavedPublication::Native(output) => {
            let operation = output.selection().binding().operation();
            generations.push(operation.authority_generation());
            publication_controls.extend([operation.cancel(), operation.shutdown()]);
            owners.extend(output.participants());
            publication_controls.extend([output.cancel(), output.pending()]);
            publication_controls.extend(output.additional());
        }
        crate::ipc::SavedPublication::Renderer(output) => {
            owners.push(output.destination());
            owners.extend(output.participants());
            publication_controls.extend([output.cancel(), output.pending()]);
            publication_controls.extend(output.additional());
        }
    }
    owners.extend(target.participants());
    owners.extend(participants);
    if let Some(snapshot) = &drive {
        owners.extend(snapshot.participants());
    }
    let mut documents = Vec::new();
    let mut pdf = Vec::new();
    let mut losses = Vec::new();
    for owner in &owners {
        model.check_owner(owner)?;
        generations.push(owner.context.renderer().ok_or(LOST)?);
        if let Some(document) = owner.context.document_lease() {
            generations.push(document);
            documents.push(document);
        }
        if let Some(document) = owner.pdf_document() {
            pdf.push(document);
        }
        if let Some(revision) = owner.pdf_revision() {
            pdf.push(revision);
        }
        losses.push(owner.loss_token().ok_or(LOST)?);
    }
    let mut controls = vec![cancel, pending, target.cancel(), target.pending()];
    controls.extend(publication_controls);
    controls.extend(target.additional());
    controls.extend_from_slice(additional);
    controls.extend(losses.iter());
    if let Some(snapshot) = &drive {
        controls.extend(snapshot.controls());
    }
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut retirement = generations.prepare_retirement(&documents)?;
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut controls = crate::runtime::CancelTokenGatePlan::from_tokens(controls);
    let mut panic_payload = None;
    let mut admit =
        |effect: &mut dyn FnMut(&mut AdmittedDocumentRetirement<'_>) -> Result<(), String>| {
            let mut enter = || {
                generations
                    .with_active_retiring(&mut retirement, |retirement| {
                        pdf.with_live(|| {
                            controls.with_active(|| {
                                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    effect(retirement)
                                })) {
                                    Ok(result) => result,
                                    Err(payload) => {
                                        panic_payload = Some(payload);
                                        Err("The synthetic saved effect panicked.".into())
                                    }
                                }
                            })
                        })
                    })
                    .flatten()
                    .flatten()
                    .ok_or_else(|| LOST.to_owned())?
            };
            match &drive {
                Some(snapshot) => {
                    snapshot.with_committed_state(moved, target.input().source_commit(), &mut enter)
                }
                None => enter(),
            }
        };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| use_plan(&mut admit)));
    drop(admit);
    drop(retirement);
    if let Some(payload) = panic_payload {
        std::panic::resume_unwind(payload);
    }
    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

pub(super) fn document(
    model: &SyntheticOwnedEffects,
    document: &NativeSavedDocument,
    actual: Option<(&NativeRequestOwner, &CancelToken)>,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[&CancelToken],
    effect: &mut dyn FnMut(&mut AdmittedDocumentRetirement<'_>) -> Result<(), String>,
) -> Result<(), String> {
    let target = document.target();
    let registered = document.owner();
    let mut owners = vec![registered.clone()];
    let mut controls = additional.to_vec();
    if let Some((actual, actual_cancel)) = actual {
        if !same_native_origin(registered, actual)
            || !actual
                .context
                .renderer()
                .zip(registered.context.renderer())
                .is_some_and(|(a, b)| a.same_generation(b))
        {
            return Err(LOST.into());
        }
        owners.push(actual.clone());
        controls.push(actual_cancel);
    }
    owners.extend_from_slice(participants);
    admit(
        model,
        target,
        &owners,
        cancel,
        pending,
        &controls,
        &mut |retirement| {
            target.reservation().with_owner(target.browser(), |_| {
                target
                    .with_registered_state(registered, || effect(retirement))
                    .ok_or_else(|| LOST.to_owned())?
            })
        },
    )
}
