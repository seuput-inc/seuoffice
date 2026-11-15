use super::*;

pub(super) fn transition(
    model: &SyntheticOwnedEffects,
    receipt: &crate::ipc::RecoverySeedReceipt,
    receiver: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    old: &PdfDocumentLease,
    successor: &PdfDocumentLease,
    renewal: &mut crate::ipc::PreparedRecoveryAuthorityRenewal,
    before_notify: &mut dyn FnMut(),
    effect: &mut dyn FnMut(
        &mut crate::ipc::AdmittedPdfRevisionRetirement<'_, '_>,
        &mut crate::ipc::PreparedRecoveryAuthorityRenewal,
    ) -> Result<(), String>,
) -> Result<(), String> {
    let source = participants.first().ok_or(LOST)?;
    if !source
        .pdf_revision()
        .is_some_and(|revision| revision.same_document(old))
    {
        return Err(LOST.into());
    }
    let target = receipt.target();
    if !receipt.matches(target) || !renewal.matches(receipt) {
        return Err(LOST.into());
    }
    let snapshot = target.authority_snapshot()?;
    let registered = snapshot.registered.as_ref().ok_or(LOST)?;
    if !same_native_origin(registered, receiver)
        || receiver.view != target.view()
        || !receiver
            .context
            .renderer()
            .is_some_and(|generation| generation.same_generation(target.created()))
    {
        return Err(LOST.into());
    }
    let mut generations = vec![target.created()];
    let renewal_sources = renewal.sources().to_vec();
    let mut owners: Vec<&NativeRequestOwner> =
        std::iter::once(receiver).chain(participants).collect();
    owners.extend(renewal_sources.iter());
    let mut tokens = vec![cancel, pending];
    let mut invocation = None;
    if let crate::ipc::RecoveryOpenContext::NativeStartup(child) = &snapshot.context {
        validate_recovery_lineage(&snapshot, child)?;
        generations.push(child.authority_generation());
        tokens.extend([child.cancel(), child.shutdown()]);
        invocation = Some(
            target
                .invocation_target()
                .ok_or(LOST)?
                .registered_document()?,
        );
        if let Some(startup) = &snapshot.startup {
            let root = startup_fixture_root(&startup.startup, &startup.shell)?;
            owners.push(&startup.shell);
            owners.push(root);
            owners.extend(startup.participants.iter());
            generations.push(startup.startup.authority_generation());
            tokens.extend([
                startup.startup.cancel(),
                startup.startup.shutdown(),
                &startup.cancel,
                &startup.pending,
            ]);
        }
        if let Some(command) = &snapshot.command {
            owners.extend(command.participants.iter());
            generations.push(command.command.authority_generation());
            tokens.extend([
                command.command.cancel(),
                command.command.shutdown(),
                &command.cancel,
                &command.pending,
            ]);
        }
    }
    let mut pdf = vec![old, successor];
    let mut losses = Vec::with_capacity(owners.len());
    for &owner in &owners {
        model.check_owner(owner)?;
        generations.push(owner.context.renderer().ok_or(LOST)?);
        if let Some(document) = owner.context.document_lease() {
            generations.push(document);
        }
        if let Some(document) = owner.pdf_document() {
            pdf.push(document);
        }
        if let Some(revision) = owner.pdf_revision() {
            pdf.push(revision);
        }
        losses.push(owner.loss_token().ok_or(LOST)?);
    }
    tokens.extend(losses.iter());
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut retirement = pdf.prepare_retirement_many(source, &owners, old, successor)?;
    let mut tokens = crate::runtime::CancelTokenGatePlan::from_tokens(tokens);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        generations
            .with_active(|| {
                pdf.with_live_retiring(&mut retirement, |admitted| {
                    tokens.with_active(|| match &invocation {
                        Some(document) => document
                            .target()
                            .with_registered_state(document.owner(), || effect(admitted, renewal)),
                        None => Some(effect(admitted, renewal)),
                    })
                })
            })
            .flatten()
            .flatten()
            .flatten()
            .ok_or_else(|| LOST.to_owned())?
    }));
    let notify = std::panic::catch_unwind(std::panic::AssertUnwindSafe(before_notify));
    let replaced =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| renewal.notify_replaced()));
    drop(retirement);
    match (result, notify, replaced) {
        (Err(panic), _, _) => std::panic::resume_unwind(panic),
        (Ok(_), Err(panic), _) => std::panic::resume_unwind(panic),
        (Ok(_), Ok(()), Err(panic)) => std::panic::resume_unwind(panic),
        (Ok(result), Ok(()), Ok(())) => result,
    }
}
