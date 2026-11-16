//! Explicit fake/probe ownership model. These identities are not native authentication.

mod pdf_transition;
mod saved;
#[cfg(any(test, feature = "test-support"))]
mod saved_actor;
#[cfg(any(test, feature = "test-support"))]
pub use saved_actor::SyntheticSavedActors;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use super::{OwnedEffects, OwnedEffectsHost};
use crate::ipc::{EventSink, NativeContext, NativeRequestOwner, PdfDocumentLease};
use crate::runtime::{CancelToken, RendererGeneration, Services, Thread, Views};
use crate::{IpcValue, ViewId};

const LOST: &str = "Synthetic document ownership is no longer available.";

/// Fake-only admission of an action against every captured lifetime.
/// The action starts only after all admission gates are released.
pub fn admit_many<R>(
    origin: &NativeRequestOwner, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, action: impl FnOnce() -> R,
) -> Result<R, String> {
    let count = participants.len() + 1;
    let mut generations = Vec::with_capacity(count * 2);
    let mut pdf = Vec::with_capacity(count * 2);
    let mut losses = Vec::with_capacity(count);
    for owner in std::iter::once(origin).chain(participants) {
        generations.push(owner.context.renderer().ok_or(LOST)?);
        if let Some(document) = owner.context.document_lease() { generations.push(document); }
        if let Some(document) = owner.pdf_document() { pdf.push(document); }
        if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
        losses.push(owner.loss_token().ok_or(LOST)?);
    }
    let mut tokens = Vec::with_capacity(losses.len() + 2);
    tokens.push(cancel);
    tokens.push(pending);
    tokens.extend(losses.iter());
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut tokens = crate::runtime::CancelTokenGatePlan::from_tokens(tokens);
    let accepted = generations.with_active(|| pdf.with_live(|| tokens.with_active(|| action)))
        .flatten().flatten().ok_or_else(|| LOST.to_owned())?;
    Ok(accepted())
}

/// Register a fixture's first owner under its modeled lifetimes. This is not
/// native attestation and must only be used by synthetic hosts and probes.
pub fn register_native_invocation_fixture(
    target: &crate::ipc::NativeInvocationTarget, owner: NativeRequestOwner,
) -> Result<crate::ipc::NativeInvocationDocument, String> {
    let operation = target.operation();
    let mut generations = vec![operation.authority_generation(), target.created(), owner.context.renderer().ok_or(LOST)?];
    if let Some(document) = owner.context.document_lease() { generations.push(document); }
    let mut pdf = Vec::new();
    if let Some(document) = owner.pdf_document() { pdf.push(document); }
    if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
    let loss = owner.loss_token().ok_or(LOST)?;
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut tokens = crate::runtime::CancelTokenGatePlan::new(&[
        operation.cancel(), operation.shutdown(), &loss,
    ]);
    generations.with_active(|| pdf.with_live(|| tokens.with_active(|| {
        target.register_from_native_admitted(owner.clone())
    }))).flatten().flatten().ok_or_else(|| LOST.to_owned())??;
    target.registered_document()
}

fn validate_recovery_lineage(
    snapshot: &crate::ipc::RecoveryAuthoritySnapshot, child: &crate::ipc::NativeInvocationOperation,
) -> Result<(), String> {
    let valid = match (&snapshot.startup, &snapshot.command) {
        (Some(startup), Some(command)) => command.command.is_native_command()
            && command.command.is_child_of(&startup.startup) && child.is_child_of(&command.command),
        (Some(startup), None) => child.is_child_of(&startup.startup),
        (None, Some(command)) => command.command.is_root_native_command() && child.is_child_of(&command.command),
        (None, None) => true,
    };
    if valid { Ok(()) } else { Err(LOST.into()) }
}
/// Synthetic first-registration under the retained recovery lineage.
pub fn register_recovery_fixture(
    target: &crate::ipc::RecoveryOpenTarget, owner: NativeRequestOwner,
) -> Result<(), String> {
    let snapshot = target.authority_snapshot()?;
    let invocation = target.invocation_target().ok_or(LOST)?;
    let operation = invocation.operation();
    validate_recovery_lineage(&snapshot, operation)?;
    let mut generations = vec![operation.authority_generation(), target.created()];
    let mut owners = vec![&owner];
    let mut controls = vec![operation.cancel(), operation.shutdown()];
    if let Some(startup) = &snapshot.startup {
        owners.push(&startup.shell);
        owners.push(startup_fixture_root(&startup.startup, &startup.shell)?);
        owners.extend(startup.participants.iter());
        generations.push(startup.startup.authority_generation());
        controls.extend([startup.startup.cancel(), startup.startup.shutdown(), &startup.cancel, &startup.pending]);
    }
    if let Some(command) = &snapshot.command {
        owners.extend(command.participants.iter());
        generations.push(command.command.authority_generation());
        controls.extend([command.command.cancel(), command.command.shutdown(), &command.cancel, &command.pending]);
    }
    let mut pdf = Vec::new();
    let mut losses = Vec::new();
    for owner in owners {
        generations.push(owner.context.renderer().ok_or(LOST)?);
        if let Some(document) = owner.context.document_lease() { generations.push(document); }
        if let Some(document) = owner.pdf_document() { pdf.push(document); }
        if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
        losses.push(owner.loss_token().ok_or(LOST)?);
    }
    controls.extend(losses.iter());
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut controls = crate::runtime::CancelTokenGatePlan::from_tokens(controls);
    generations.with_active(|| pdf.with_live(|| controls.with_active(|| {
        snapshot.register_native_from_admitted(owner.clone())
    }))).flatten().flatten().ok_or_else(|| LOST.to_owned())?
}

/// Fake-only bounded admission of the exact invocation's registered target.
pub fn with_native_invocation(
    document: &crate::ipc::NativeInvocationDocument,
    origin: &NativeRequestOwner, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    with_native_invocation_extra(document, origin, participants, cancel, pending, &[], effect)
}

pub fn with_native_invocation_extra(
    document: &crate::ipc::NativeInvocationDocument,
    origin: &NativeRequestOwner, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let registered = document.owner();
    if !same_native_origin(registered, origin)
        || !origin.context.renderer().is_some_and(|renderer| renderer.same_generation(document.created())) {
        return Err(LOST.into());
    }
    let operation = document.operation();
    let count = participants.len() + 1;
    let mut generations = Vec::with_capacity(count * 2 + 2);
    let mut pdf = Vec::with_capacity(count * 2);
    let mut losses = Vec::with_capacity(count);
    generations.push(operation.authority_generation());
    generations.push(document.created());
    for owner in std::iter::once(origin).chain(participants) {
        generations.push(owner.context.renderer().ok_or(LOST)?);
        if let Some(lease) = owner.context.document_lease() { generations.push(lease); }
        if let Some(lease) = owner.pdf_document() { pdf.push(lease); }
        if let Some(lease) = owner.pdf_revision() { pdf.push(lease); }
        losses.push(owner.loss_token().ok_or(LOST)?);
    }
    let mut tokens = Vec::with_capacity(losses.len() + 4);
    tokens.extend([cancel, pending, operation.cancel(), operation.shutdown()]);
    tokens.extend_from_slice(additional);
    tokens.extend(losses.iter());
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut tokens = crate::runtime::CancelTokenGatePlan::from_tokens(tokens);
    generations.with_active(|| pdf.with_live(|| tokens.with_active(|| {
        document.target().with_registered_state(registered, effect)
    }))).flatten().flatten().flatten().ok_or_else(|| LOST.to_owned())?
}

/// Explicit fixture-only native lineage. It is not a CEF origin proof.
#[derive(Clone)]
pub struct SyntheticNativeOrigin(Arc<()>);

impl SyntheticNativeOrigin {
    pub fn new() -> Self { Self(Arc::new(())) }
}

impl Default for SyntheticNativeOrigin {
    fn default() -> Self { Self::new() }
}

fn same_native_origin(a: &NativeRequestOwner, b: &NativeRequestOwner) -> bool {
    if a.view != b.view { return false; }
    match (a.context.authority_ref::<SyntheticNativeOrigin>(), b.context.authority_ref::<SyntheticNativeOrigin>()) {
        (Some(a), Some(b)) => Arc::ptr_eq(&a.0, &b.0),
        (None, None) => {
            let a_parent = a.context.downcast_ref::<SyntheticIdentity>().and_then(|identity| identity.native_parent.clone());
            let b_parent = b.context.downcast_ref::<SyntheticIdentity>().and_then(|identity| identity.native_parent.clone());
            let inherited = a_parent.is_some() || b_parent.is_some();
            let a_origin = a_parent.unwrap_or_else(|| a.context.as_arc());
            let b_origin = b_parent.unwrap_or_else(|| b.context.as_arc());
            Arc::ptr_eq(&a_origin, &b_origin) && (inherited ||
                a.loss_token().zip(b.loss_token()).is_some_and(|(a, b)| a.same_token(&b)))
        }
        _ => false,
    }
}
/// Fixture-only reply authority for one invocation. This is not native
/// attestation; fake dispatchers must create a fresh instance for each call.
pub struct SyntheticReplyAuthority {
    state: Mutex<SyntheticReplyState>,
}

struct SyntheticReplyState {
    owner: NativeRequestOwner,
    transferred: bool,
    delivered: bool,
    cancel: Option<CancelToken>,
}

fn same_reply_owner(a: &NativeRequestOwner, b: &NativeRequestOwner) -> bool {
    a.view == b.view && Arc::ptr_eq(&a.context.as_arc(), &b.context.as_arc())
        && a.loss_token().zip(b.loss_token()).is_some_and(|(a, b)| a.same_token(&b))
}

impl SyntheticReplyAuthority {
    pub fn attach(mut owner: NativeRequestOwner, cancel: &CancelToken) -> (NativeRequestOwner, Arc<Self>) {
        owner.context = owner.context.bind_dispatcher_cancellation(owner.view, cancel.clone(), CancelToken::default());
        let authority = Arc::new(Self { state: Mutex::new(SyntheticReplyState {
            owner: owner.clone(), transferred: false, delivered: false, cancel: None,
        }) });
        owner.context = owner.context.with_request_authority(authority.clone());
        (owner, authority)
    }

    fn with_media_transfer<T>(&self, actual: &NativeRequestOwner, cancel: &CancelToken,
        successor: NativeRequestOwner, commit: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let marker = actual.context.request_authority::<Self>().ok_or(LOST)?;
        if !std::ptr::eq(Arc::as_ptr(&marker), self) || cancel.is_cancelled()
            || actual.dispatcher_cancellation_for(cancel).is_none() { return Err(LOST.into()); }
        let mut state = self.state.lock().map_err(|_| LOST)?;
        if state.delivered || state.transferred || !same_reply_owner(&state.owner, actual)
            || !same_native_origin(actual, &successor)
            || !actual.context.renderer().zip(successor.context.renderer())
                .is_some_and(|(a, b)| a.same_generation(b)) { return Err(LOST.into()); }
        let value = commit()?;
        state.owner = NativeRequestOwner { view: successor.view, context: successor.context.without_request_authority() };
        state.cancel = Some(cancel.clone());
        state.transferred = true;
        Ok(value)
    }

    /// Admit exactly one fixture reply under its retained request owner.
    pub fn deliver(&self, effects: &SyntheticOwnedEffects, deliver: impl FnOnce()) -> Result<(), String> {
        let (owner, transferred, cancel) = {
            let state = self.state.lock().map_err(|_| LOST)?;
            if state.delivered { return Err(LOST.into()); }
            (state.owner.clone(), state.transferred, state.cancel.clone())
        };
        let active = cancel.unwrap_or_default();
        effects.owned(&owner, &owner, &active, &active, || {
            let mut state = self.state.lock().map_err(|_| LOST)?;
            if state.delivered || state.transferred != transferred || !same_reply_owner(&state.owner, &owner) {
                return Err(LOST.to_owned());
            }
            state.delivered = true;
            Ok(())
        })??;
        deliver();
        Ok(())
    }
}


fn with_created_many_additional(
    source: &NativeRequestOwner, participants: &[NativeRequestOwner], created: &RendererGeneration,
    cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let mut generations = vec![created];
    let mut pdf = Vec::new();
    let mut losses = Vec::new();
    for owner in std::iter::once(source).chain(participants) {
        generations.push(owner.context.renderer().ok_or(LOST)?);
        if let Some(document) = owner.context.document_lease() { generations.push(document); }
        if let Some(document) = owner.pdf_document() { pdf.push(document); }
        if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
        losses.push(owner.loss_token().ok_or(LOST)?);
    }
    let mut tokens = vec![cancel, pending];
    tokens.extend_from_slice(additional);
    tokens.extend(losses.iter());
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut tokens = crate::runtime::CancelTokenGatePlan::from_tokens(tokens);
    generations.with_active(|| pdf.with_live(|| tokens.with_active(effect)))
        .flatten().flatten().ok_or_else(|| LOST.to_owned())?
}

pub fn with_created_many(
    source: &NativeRequestOwner, participants: &[NativeRequestOwner], created: &RendererGeneration,
    cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    with_created_many_additional(source, participants, created, cancel, pending, &[], effect)
}

pub fn with_owned_effect_many_extra(
    origin: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken,
    pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let created = origin.context.renderer().ok_or(LOST)?;
    with_created_many_additional(origin, participants, created, cancel, pending, additional, effect)
}

/// Synthetic admission of the exact selected revision and all captured owners.
pub fn with_native_save_revision_snapshot(
    snapshot: &crate::ipc::NativeSaveRevisionSnapshot, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken],
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let mut owners = snapshot.aliases().to_vec();
    owners.extend_from_slice(participants);
    with_existing_document_save_extra(snapshot.selection().binding(), &owners, cancel, pending, additional,
        &mut || snapshot.with_current(&mut *effect).ok_or_else(|| LOST.to_owned())?)
}

/// Synthetic revision admission; native origin authentication remains a host responsibility.
pub fn with_native_save_revision_transition(
    prepared: &mut crate::ipc::PreparedNativeSaveRevision, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], before_notify: &mut dyn FnMut(),
    effect: &mut dyn FnMut(&mut crate::ipc::AdmittedPdfRevisionSet<'_, '_>,
        &mut crate::ipc::PreparedNativeSaveRevision) -> Result<(), String>,
) -> Result<(), String> {
    let snapshot = prepared.snapshot().clone();
    let pairs = prepared.pairs().to_vec();
    let pair_refs: Vec<_> = pairs.iter().map(|(old, new)| (old, new)).collect();
    let mut captured = snapshot.aliases().to_vec();
    captured.extend(prepared.request_aliases().cloned());
    captured.extend_from_slice(participants);
    let owners: Vec<_> = std::iter::once(snapshot.selection().binding().owner()).chain(captured.iter()).collect();
    let operation = snapshot.selection().binding().operation();
    let mut generations = vec![operation.authority_generation()];
    let mut pdf = Vec::with_capacity(pairs.len() * 2 + owners.len() * 2);
    for (old, new) in &pairs { pdf.extend([old, new]); }
    let mut losses = Vec::with_capacity(owners.len());
    for owner in &owners {
        generations.push(owner.context.renderer().ok_or(LOST)?);
        if let Some(document) = owner.context.document_lease() { generations.push(document); }
        if let Some(slot) = owner.pdf_document() { pdf.push(slot); }
        if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
        losses.push(owner.loss_token().ok_or(LOST)?);
    }
    let mut controls = vec![cancel, pending, operation.cancel(), operation.shutdown()];
    controls.extend(losses.iter());
    controls.extend_from_slice(additional);
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut retirement = pdf.prepare_revision_set(&pair_refs)?;
    let mut controls = crate::runtime::CancelTokenGatePlan::from_tokens(controls);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        generations.with_active(|| pdf.with_live_revision_set(&mut retirement, |admitted|
            controls.with_active(|| {
                effect(admitted, prepared)?;
                if !admitted.is_retired() || !prepared.is_committed() {
                    return Err("The native save revision transition was not committed.".into());
                }
                Ok(())
            }))).flatten().flatten().ok_or_else(|| LOST.to_owned())?
    }));
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(before_notify));
    let notification = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepared.notify_replaced()));
    drop(retirement);
    match (result, cleanup, notification) {
        (Err(panic), _, _) | (Ok(_), Err(panic), _) | (Ok(_), Ok(()), Err(panic)) => std::panic::resume_unwind(panic),
        (Ok(result), Ok(()), Ok(())) => result,
    }
}

/// Fake-only same-origin admission, including both current document lifetimes.
pub fn with_same_native_origin(
    expected: &NativeRequestOwner, actual: &NativeRequestOwner, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let renderer = expected.context.renderer().ok_or(LOST)?;
    if !same_native_origin(expected, actual)
        || !actual.context.renderer().is_some_and(|other| other.same_generation(renderer)) {
        return Err(LOST.into());
    }
    let mut owners = Vec::with_capacity(participants.len() + 1);
    owners.push(actual.clone());
    owners.extend_from_slice(participants);
    with_created_many_additional(expected, &owners, renderer, cancel, pending, additional, effect)
}

pub fn with_native_command_creation(
    operation: &crate::ipc::NativeInvocationOperation, expected: &crate::ipc::NativeEditorCommand,
    created: &RendererGeneration, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if matches!(expected, crate::ipc::NativeEditorCommand::SaveDocument { .. }) || !operation.is_native_command()
        || !matches!(operation.selection(), crate::ipc::NativeInvocationSelection::NativeCommand(command) if command == expected) {
        return Err(LOST.into());
    }
    let mut generations = vec![operation.authority_generation(), created];
    let mut pdf = Vec::new();
    let mut losses = Vec::new();
    for participant in participants {
        generations.push(participant.context.renderer().ok_or(LOST)?);
        if let Some(document) = participant.context.document_lease() { generations.push(document); }
        if let Some(document) = participant.pdf_document() { pdf.push(document); }
        if let Some(revision) = participant.pdf_revision() { pdf.push(revision); }
        losses.push(participant.loss_token().ok_or(LOST)?);
    }
    let mut tokens = vec![cancel, pending, operation.cancel(), operation.shutdown()];
    tokens.extend(losses.iter());
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut tokens = crate::runtime::CancelTokenGatePlan::from_tokens(tokens);
    generations.with_active(|| pdf.with_live(|| tokens.with_active(effect)))
        .flatten().flatten().ok_or_else(|| LOST.to_owned())?
}

#[allow(clippy::too_many_arguments)]
pub fn with_native_command_target(
    operation: &crate::ipc::NativeInvocationOperation, expected: &crate::ipc::NativeEditorCommand,
    document: &crate::ipc::NativeInvocationDocument, owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if matches!(expected, crate::ipc::NativeEditorCommand::SaveDocument { .. }) || !operation.is_native_command()
        || !matches!(operation.selection(), crate::ipc::NativeInvocationSelection::NativeCommand(command) if command == expected)
        || !operation.authority_generation().same_generation(document.operation().authority_generation()) {
        return Err(LOST.into());
    }
    with_native_invocation(document, owner, participants, cancel, pending, effect)
}

fn startup_fixture_root<'a>(
    operation: &'a crate::ipc::NativeInvocationOperation, origin: &NativeRequestOwner,
) -> Result<&'a NativeRequestOwner, String> {
    if operation.purpose() != crate::ipc::NativeInvocationPurpose::StartupLaunch { return Err(LOST.into()); }
    let root = operation.startup_owner().ok_or(LOST)?;
    let same_generation = |a: Option<&RendererGeneration>, b: Option<&RendererGeneration>| match (a, b) {
        (Some(a), Some(b)) => a.same_generation(b), (None, None) => true, _ => false,
    };
    let same_pdf = |a: Option<&PdfDocumentLease>, b: Option<&PdfDocumentLease>| match (a, b) {
        (Some(a), Some(b)) => a.same_document(b), (None, None) => true, _ => false,
    };
    if origin.context.authority_ref::<SyntheticNativeOrigin>().is_none()
        || root.context.authority_ref::<SyntheticNativeOrigin>().is_none()
        || !same_native_origin(root, origin)
        || !same_generation(root.context.renderer(), origin.context.renderer())
        || !same_generation(root.context.document_lease(), origin.context.document_lease())
        || !same_pdf(root.pdf_document(), origin.pdf_document())
        || !same_pdf(root.pdf_revision(), origin.pdf_revision()) {
        return Err(LOST.into());
    }
    Ok(root)
}

/// Admit an explicitly synthetic renderer and every participant against the
/// actual invocation operation. This never supplies native authentication.
pub fn with_native_operation(
    operation: &crate::ipc::NativeInvocationOperation, origin: &NativeRequestOwner,
    participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let root = startup_fixture_root(operation, origin)?;
    let mut owners = Vec::with_capacity(participants.len() + 1);
    owners.push(root.clone());
    owners.extend_from_slice(participants);
    with_created_many_additional(origin, &owners, operation.authority_generation(), cancel, pending,
        &[operation.cancel(), operation.shutdown()], effect)
}

pub fn with_existing_document_save(
    binding: &crate::ipc::NativeExistingDocumentBinding, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    with_existing_document_save_extra(binding, participants, cancel, pending, &[], effect)
}

pub fn with_existing_document_save_extra(
    binding: &crate::ipc::NativeExistingDocumentBinding, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let operation = binding.operation();
    let owner = binding.owner();
    if !operation.is_root_native_command()
        || !matches!(operation.selection(), crate::ipc::NativeInvocationSelection::NativeCommand(
            crate::ipc::NativeEditorCommand::SaveDocument { view, .. }) if *view == owner.view) {
        return Err(LOST.into());
    }
    let mut controls = Vec::with_capacity(additional.len() + 2);
    controls.extend_from_slice(additional);
    controls.extend([operation.cancel(), operation.shutdown()]);
    with_created_many_additional(owner, participants, operation.authority_generation(), cancel, pending, &controls, effect)
}

#[allow(clippy::too_many_arguments)]
pub fn with_startup_recovery_creation(
    startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
    child: &crate::ipc::NativeInvocationOperation, created: &RendererGeneration,
    participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if !child.is_child_of(startup) || child.purpose() != crate::ipc::NativeInvocationPurpose::Recovery {
        return Err(LOST.into());
    }
    with_startup_child_creation(startup, shell, child, created, participants, cancel, pending, effect)
}

#[allow(clippy::too_many_arguments)]
fn with_startup_child_creation(
    startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
    child: &crate::ipc::NativeInvocationOperation, created: &RendererGeneration,
    participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let root = startup_fixture_root(startup, shell)?;
    let mut generations = vec![startup.authority_generation(), child.authority_generation(), created];
    let mut pdf = Vec::new();
    let mut losses = Vec::new();
    for owner in [shell, root].into_iter().chain(participants) {
        generations.push(owner.context.renderer().ok_or(LOST)?);
        if let Some(document) = owner.context.document_lease() { generations.push(document); }
        if let Some(document) = owner.pdf_document() { pdf.push(document); }
        if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
        losses.push(owner.loss_token().ok_or(LOST)?);
    }
    let mut tokens = vec![cancel, pending, startup.cancel(), startup.shutdown(), child.cancel(), child.shutdown()];
    tokens.extend(losses.iter());
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut tokens = crate::runtime::CancelTokenGatePlan::from_tokens(tokens);
    generations.with_active(|| pdf.with_live(|| tokens.with_active(effect)))
        .flatten().flatten().ok_or_else(|| LOST.to_owned())?
}


#[allow(clippy::too_many_arguments)]
pub fn with_startup_command_preparation(
    startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
    child: &crate::ipc::NativeInvocationOperation, expected: &crate::ipc::NativeEditorCommand,
    participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if matches!(expected, crate::ipc::NativeEditorCommand::SaveDocument { .. }) || !child.is_child_of(startup) || !child.is_native_command()
        || !matches!(child.selection(), crate::ipc::NativeInvocationSelection::NativeCommand(command) if command == expected) {
        return Err(LOST.into());
    }
    with_startup_child_creation(startup, shell, child, child.authority_generation(), participants, cancel, pending, effect)
}
pub fn with_startup_recovery_seed(
    startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
    target: &crate::ipc::NativeInvocationTarget, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    with_startup_recovery_creation(startup, shell, target.operation(), target.created(), participants, cancel, pending,
        &mut || target.with_reserved_state(&mut *effect).ok_or_else(|| LOST.to_owned())?)
}

/// Fake-only joint admission of the bound startup and exact registered child.
#[allow(clippy::too_many_arguments)]
pub fn with_startup_recovery(
    startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
    child: &crate::ipc::NativeInvocationDocument, owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    with_startup_recovery_additional(startup, shell, child, owner, participants, cancel, pending, &[], effect)
}

#[allow(clippy::too_many_arguments)]
fn with_startup_recovery_additional(
    startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
    child: &crate::ipc::NativeInvocationDocument, owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
    additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if child.operation().purpose() != crate::ipc::NativeInvocationPurpose::Recovery { return Err(LOST.into()); }
    with_startup_document(startup, shell, child, owner, participants, cancel, pending, additional, effect)
}

#[allow(clippy::too_many_arguments)]
fn with_startup_document(
    startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
    child: &crate::ipc::NativeInvocationDocument, owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
    additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let root = startup_fixture_root(startup, shell)?;
    let registered = child.owner();
    if !same_native_origin(registered, owner)
        || !owner.context.renderer().is_some_and(|renderer| renderer.same_generation(child.created())) {
        return Err(LOST.into());
    }
    let operation = child.operation();
    if !operation.is_child_of(startup) {
        return Err(LOST.into());
    }
    let mut generations = vec![startup.authority_generation(), operation.authority_generation(), child.created()];
    let mut pdf = Vec::new();
    let mut losses = Vec::new();
    for captured in [owner, shell, root].into_iter().chain(participants) {
        generations.push(captured.context.renderer().ok_or(LOST)?);
        if let Some(document) = captured.context.document_lease() { generations.push(document); }
        if let Some(document) = captured.pdf_document() { pdf.push(document); }
        if let Some(revision) = captured.pdf_revision() { pdf.push(revision); }
        losses.push(captured.loss_token().ok_or(LOST)?);
    }
    let mut tokens = vec![cancel, pending, startup.cancel(), startup.shutdown(), operation.cancel(), operation.shutdown()];
    tokens.extend_from_slice(additional);
    tokens.extend(losses.iter());
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut tokens = crate::runtime::CancelTokenGatePlan::from_tokens(tokens);
    generations.with_active(|| pdf.with_live(|| tokens.with_active(|| {
        child.target().with_registered_state(registered, effect)
    }))).flatten().flatten().flatten().ok_or_else(|| LOST.to_owned())?
}

fn recovery_pre_navigation<R>(
    target: &crate::ipc::RecoveryOpenTarget,
    snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
    dialog: Option<(&NativeRequestOwner, &RendererGeneration, &CancelToken, &CancelToken)>,
    effect: impl FnOnce() -> R,
) -> Result<R, String> {
    if !snapshot.belongs_to(target) { return Err(LOST.into()); }
    let mut generations = vec![target.created()];
    let mut owners = Vec::new();
    let mut tokens = Vec::new();
    if let Some((owner, generation, cancel, pending)) = dialog {
        if !owner.context.renderer().is_some_and(|renderer| renderer.same_generation(generation)) {
            return Err(LOST.into());
        }
        owners.push(owner);
        generations.push(generation);
        tokens.extend([cancel, pending]);
    }
    if let Some(request) = &snapshot.request {
        owners.push(&request.source);
        owners.extend(request.participants.iter());
        tokens.extend([&request.cancel, &request.pending]);
    }
    if let Some(startup) = &snapshot.startup {
        let crate::ipc::RecoveryOpenContext::NativeStartup(child) = &snapshot.context else { return Err(LOST.into()); };
        validate_recovery_lineage(snapshot, child)?;
        let root = startup_fixture_root(&startup.startup, &startup.shell)?;
        owners.push(&startup.shell);
        owners.push(root);
        owners.extend(startup.participants.iter());
        generations.push(startup.startup.authority_generation());
        tokens.extend([startup.startup.cancel(), startup.startup.shutdown(), &startup.cancel, &startup.pending]);
    }
    if let Some(command) = &snapshot.command {
        let crate::ipc::RecoveryOpenContext::NativeStartup(child) = &snapshot.context else { return Err(LOST.into()); };
        validate_recovery_lineage(snapshot, child)?;
        owners.extend(command.participants.iter());
        generations.push(command.command.authority_generation());
        tokens.extend([command.command.cancel(), command.command.shutdown(), &command.cancel, &command.pending]);
    }
    match &snapshot.context {
        crate::ipc::RecoveryOpenContext::NativeStartup(operation) => {
            generations.push(operation.authority_generation());
            tokens.extend([operation.cancel(), operation.shutdown()]);
        }
        crate::ipc::RecoveryOpenContext::Renderer { source, participants, cancel, pending } => {
            owners.push(source);
            owners.extend(participants.iter());
            tokens.extend([cancel, pending]);
        }
    }
    let mut pdf = Vec::new();
    let mut losses = Vec::new();
    for owner in owners {
        generations.push(owner.context.renderer().ok_or(LOST)?);
        if let Some(document) = owner.context.document_lease() { generations.push(document); }
        if let Some(document) = owner.pdf_document() { pdf.push(document); }
        if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
        losses.push(owner.loss_token().ok_or(LOST)?);
    }
    tokens.extend(losses.iter());
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut tokens = crate::runtime::CancelTokenGatePlan::from_tokens(tokens);
    generations.with_active(|| pdf.with_live(|| tokens.with_active(|| snapshot.with_current(|| {
        if snapshot.registered.is_some() || target.is_seed_committed() { return None; }
        match &snapshot.context {
            crate::ipc::RecoveryOpenContext::NativeStartup(_) =>
                target.invocation_target()?.with_reserved_state(effect),
            crate::ipc::RecoveryOpenContext::Renderer { .. } => Some(effect()),
        }
    })))).flatten().flatten().flatten().flatten().ok_or_else(|| LOST.to_owned())
}

/// Fake-only preparation against all original captured lifetimes.
pub fn with_recovery_seed(
    target: &crate::ipc::RecoveryOpenTarget, effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let snapshot = target.authority_snapshot()?;
    recovery_pre_navigation(target, &snapshot, None, effect)?
}

struct DeferredPreparation(Option<CancelToken>);
impl Drop for DeferredPreparation {
    fn drop(&mut self) { if let Some(token) = self.0.take() { token.cancel(); } }
}

struct DeferredSettlement(Option<crate::ipc::DeferredRecoverySettlement>);
impl Drop for DeferredSettlement {
    fn drop(&mut self) { if let Some(settlement) = self.0.take() { settlement.notify(); } }
}

/// Fake-only one-shot immutable enqueue, returning its exact bound receipt.
pub fn commit_recovery_seed(
    target: &crate::ipc::RecoveryOpenTarget, enqueue: &mut dyn FnMut() -> Result<(), String>,
) -> Result<crate::ipc::RecoverySeedReceipt, String> {
    let snapshot = target.authority_snapshot()?;
    commit_recovery_seed_snapshot(target, &snapshot, enqueue)
}

fn commit_recovery_seed_snapshot(
    target: &crate::ipc::RecoveryOpenTarget, snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
    enqueue: &mut dyn FnMut() -> Result<(), String>,
) -> Result<crate::ipc::RecoverySeedReceipt, String> {
    let mut notification = DeferredPreparation(None);
    recovery_pre_navigation(target, snapshot, None, || {
        let receipt = target.commit_seed_from_admitted(enqueue)?;
        notification.0 = Some(target.preparation_loss_token());
        Ok(receipt)
    })?
}

/// Synthetic dialog answer under the exact recovery seed lifetime.
pub fn recovery_dialog<T>(
    target: &crate::ipc::RecoveryOpenTarget,
    snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
    effect: impl FnOnce() -> T,
) -> Result<T, String> {
    let mut effect = Some(effect);
    let mut answer = None;
    recovery_pre_navigation(target, snapshot, None, || {
        answer = Some(effect.take().ok_or("Synthetic recovery dialog already consumed")?());
        Ok::<(), String>(())
    })??;
    answer.ok_or_else(|| "Synthetic recovery dialog was not admitted".into())
}

/// Scripted OS selection for explicit synthetic dialog fixtures only.
pub fn recovery_save(
    target: &crate::ipc::RecoveryOpenTarget,
    snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
    effect: impl FnOnce() -> Option<std::path::PathBuf>,
) -> Result<Option<crate::ipc::RecoveryPreparedDestination>, String> {
    recovery_dialog(target, snapshot, || effect().map(|path|
        crate::ipc::RecoveryPreparedDestination::from_native_save_dialog(target.clone(), snapshot.clone(), path)
    ).transpose())?
}

#[allow(clippy::too_many_arguments)]
fn with_command_recovery(
    startup: Option<&crate::ipc::StartupRecoveryConstraint>,
    command: &crate::ipc::CommandRecoveryConstraint, document: &crate::ipc::NativeInvocationDocument,
    receiver: &NativeRequestOwner, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let root = &command.command;
    let child = document.operation();
    if !child.is_child_of(root)
        || child.purpose() != crate::ipc::NativeInvocationPurpose::Recovery
        || !same_native_origin(document.owner(), receiver) {
        return Err(LOST.into());
    }
    match startup {
        Some(startup) if root.is_child_of(&startup.startup) => {}
        None if root.is_root_native_command() => {}
        _ => return Err(LOST.into()),
    }
    let mut generations = vec![root.authority_generation(), child.authority_generation(), document.created()];
    let mut pdf = Vec::new();
    let mut losses = Vec::new();
    let mut owners: Vec<&NativeRequestOwner> = std::iter::once(receiver).chain(participants).chain(command.participants.iter()).collect();
    let mut extra_controls = Vec::new();
    if let Some(startup) = startup {
        owners.push(&startup.shell);
        owners.push(startup_fixture_root(&startup.startup, &startup.shell)?);
        owners.extend(startup.participants.iter());
        generations.push(startup.startup.authority_generation());
        extra_controls.extend([startup.startup.cancel(), startup.startup.shutdown(), &startup.cancel, &startup.pending]);
    }
    for owner in owners {
        generations.push(owner.context.renderer().ok_or(LOST)?);
        if let Some(document) = owner.context.document_lease() { generations.push(document); }
        if let Some(document) = owner.pdf_document() { pdf.push(document); }
        if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
        losses.push(owner.loss_token().ok_or(LOST)?);
    }
    let mut tokens = vec![cancel, pending, &command.cancel, &command.pending,
        root.cancel(), root.shutdown(), child.cancel(), child.shutdown()];
    tokens.extend(extra_controls);
    tokens.extend_from_slice(additional);
    tokens.extend(losses.iter());
    let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
    let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
    let mut tokens = crate::runtime::CancelTokenGatePlan::from_tokens(tokens);
    generations.with_active(|| pdf.with_live(|| tokens.with_active(|| {
        document.target().with_registered_state(document.owner(), effect)
    }))).flatten().flatten().flatten().ok_or_else(|| LOST.to_owned())?
}

/// Fake-only final recovery admission against the retained first registration.
pub fn with_recovery_open(
    receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner, cancel: &CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    with_recovery_open_pending(receipt, receiver, cancel, cancel, effect)
}

pub fn with_recovery_open_pending(
    receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner,
    cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    with_recovery_open_many(receipt, receiver, &[], cancel, pending, effect)
}

pub fn with_recovery_open_many(
    receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    with_recovery_open_many_extra(receipt, receiver, participants, cancel, pending, &[], effect)
}

pub fn with_recovery_open_many_extra(
    receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner, participants: &[NativeRequestOwner],
    cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let snapshot = receipt.target().authority_snapshot()?;
    with_recovery_open_snapshot_impl(receipt, &snapshot, receiver, participants, cancel, pending, additional, effect)
}

pub fn with_recovery_open_snapshot(
    receipt: &crate::ipc::RecoverySeedReceipt, snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
    receiver: &NativeRequestOwner, cancel: &CancelToken, pending: &CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    with_recovery_open_snapshot_impl(receipt, snapshot, receiver, &[], cancel, pending, &[], effect)
}

#[allow(clippy::too_many_arguments)]
fn with_recovery_open_snapshot_impl(
    receipt: &crate::ipc::RecoverySeedReceipt, snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
    receiver: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken,
    pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let target = receipt.target();
    if !receipt.matches(target) || !snapshot.belongs_to(target) { return Err(LOST.into()); }
    let registered = snapshot.registered.as_ref().ok_or(LOST)?;
    if !same_native_origin(registered, receiver) || receiver.view != target.view()
        || !receiver.context.renderer().is_some_and(|generation| generation.same_generation(target.created())) {
        return Err(LOST.into());
    }
    let mut guarded = || snapshot.with_current(&mut *effect).ok_or_else(|| LOST.to_owned())?;
    match &snapshot.context {
        crate::ipc::RecoveryOpenContext::NativeStartup(_) => {
            let invocation = target.invocation_target().ok_or(LOST)?;
            let document = invocation.registered_document()?;
            if let Some(command) = &snapshot.command {
                with_command_recovery(snapshot.startup.as_ref(), command, &document, receiver, participants, cancel, pending, additional, &mut guarded)
            } else if let Some(startup) = &snapshot.startup {
                let mut owners = Vec::with_capacity(participants.len() + startup.participants.len());
                owners.extend_from_slice(participants);
                owners.extend_from_slice(&startup.participants);
                let mut extra = Vec::with_capacity(additional.len() + 2);
                extra.extend_from_slice(additional);
                extra.extend([&startup.cancel, &startup.pending]);
                with_startup_recovery_additional(&startup.startup, &startup.shell, &document, receiver,
                    &owners, cancel, pending, &extra, &mut guarded)
            } else {
                with_native_invocation_extra(&document, receiver, participants, cancel, pending, additional, &mut guarded)
            }
        }
        crate::ipc::RecoveryOpenContext::Renderer { .. } => {
            with_created_many_additional(receiver, participants, target.created(), cancel, pending, additional, &mut guarded)
        }
    }
}

/// Settle only an admitted receiver, then notify after all model gates drop.
pub fn settle_recovery_seed(
    receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner, cancel: &CancelToken,
    result: Result<(), String>,
) -> Result<(), String> {
    let mut result = Some(result);
    let mut deferred = DeferredSettlement(None);
    with_recovery_open(receipt, receiver, cancel, &mut || {
        deferred.0 = Some(receipt.settle_from_admitted(result.take().ok_or(LOST)?)?);
        Ok(())
    })?;
    Ok(())
}

/// Fake-only password-dialog admission before the recovery target registers.
pub fn with_recovery_dialog(
    target: &crate::ipc::RecoveryOpenTarget, dialog: &NativeRequestOwner,
    dialog_generation: &RendererGeneration, cancel: &CancelToken, pending: &CancelToken,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let snapshot = target.authority_snapshot()?;
    recovery_pre_navigation(target, &snapshot, Some((dialog, dialog_generation, cancel, pending)), effect)?
}

#[derive(Clone)]
struct SyntheticIdentity {
    view: ViewId,
    docs_document: Option<u64>,
    native_parent: Option<Arc<dyn std::any::Any + Send + Sync>>,
    _binding: Option<Arc<SyntheticDocsBinding>>,
    _permit_loss: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

struct DocsPublication {
    view: ViewId,
    renderer: RendererGeneration,
    document_id: u64,
    document: RendererGeneration,
}

type DocsPublications = Mutex<HashMap<ViewId, Weak<DocsPublication>>>;
type BrowserBindings = HashMap<ViewId, (Arc<dyn std::any::Any + Send + Sync>, crate::ipc::NativeBrowserOwner)>;

/// Retain alongside the actual model permit's revocation subscription.
/// Dropping publication removes lookup eligibility, not document authority.
pub struct SyntheticDocsBinding {
    publication: Arc<DocsPublication>,
    registry: Weak<DocsPublications>,
}

impl Drop for SyntheticDocsBinding {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else { return };
        let mut registry = registry.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(current) = registry.get_mut(&self.publication.view)
            && current.ptr_eq(&Arc::downgrade(&self.publication))
        {
            *current = Weak::new();
        }
    }
}

/// Fixture-only preallocated publication. The empty reserved slot is not
/// visible to capture_owner; validation holds it through the actor commit.
pub struct PreparedSyntheticDocsPublication {
    registry: Arc<DocsPublications>,
    binding: Option<SyntheticDocsBinding>,
    previous: Option<Arc<DocsPublication>>,
    expected: Weak<DocsPublication>,
}

pub struct SyntheticDocsPublicationGuard<'a> {
    binding: &'a mut Option<SyntheticDocsBinding>,
    previous: &'a mut Option<Arc<DocsPublication>>,
    registry: std::sync::MutexGuard<'a, HashMap<ViewId, Weak<DocsPublication>>>,
}

/// Retain and drop this outside the transition, before any loss notifications.
pub struct SyntheticDocsPrior { _retained: Option<Arc<DocsPublication>> }

impl PreparedSyntheticDocsPublication {
    pub fn validate<'a>(&'a mut self, transition: &crate::ipc::NativeBrowserTransition<'_>,
        reservation: &crate::ipc::NativeClosureReservation) -> Result<SyntheticDocsPublicationGuard<'a>, String> {
        let binding = self.binding.as_ref().ok_or("Synthetic Docs publication was already consumed")?;
        if !reservation.matches(binding.publication.view, &binding.publication.renderer) || !transition.matches(reservation) {
            return Err("Synthetic Docs publication has another browser generation".into());
        }
        let registry = self.registry.lock().map_err(|_| LOST)?;
        if !registry.get(&binding.publication.view).is_some_and(|current| current.ptr_eq(&self.expected)) {
            return Err("Synthetic Docs publication changed before adoption".into());
        }
        Ok(SyntheticDocsPublicationGuard { binding: &mut self.binding, previous: &mut self.previous, registry })
    }
}

impl SyntheticDocsPublicationGuard<'_> {
    pub fn commit(mut self) -> (SyntheticDocsBinding, SyntheticDocsPrior) {
        let binding = self.binding.take().expect("validated synthetic Docs publication");
        *self.registry.get_mut(&binding.publication.view).expect("validated synthetic Docs slot") = Arc::downgrade(&binding.publication);
        let previous = self.previous.take();
        (binding, SyntheticDocsPrior { _retained: previous })
    }
}

struct DeferredRetirements([Option<CancelToken>; 2]);

impl Drop for DeferredRetirements {
    fn drop(&mut self) {
        for notification in self.0.iter_mut().filter_map(Option::take) { notification.cancel(); }
    }
}

struct SyntheticEventNode {
    owner: NativeRequestOwner,
    channel: &'static str,
    args: Vec<IpcValue>,
    next: Option<Box<SyntheticEventNode>>,
}

#[derive(Default)]
struct SyntheticEventState {
    head: Option<Box<SyntheticEventNode>>,
    draining: bool,
}

struct SyntheticEventStream {
    view: ViewId,
    generation: RendererGeneration,
    state: Mutex<SyntheticEventState>,
}

#[derive(Default)]
struct SyntheticEventStreams(Mutex<Vec<Arc<SyntheticEventStream>>>);

impl SyntheticEventStreams {
    fn prepare(&self, owner: &NativeRequestOwner, channel: &'static str, args: Vec<IpcValue>)
        -> Result<(Arc<SyntheticEventStream>, Box<SyntheticEventNode>), String> {
        let generation = owner.context.renderer().ok_or(LOST)?;
        let mut discarded = Vec::new();
        let mut streams = self.0.lock().map_err(|_| "Synthetic event streams are unavailable.")?;
        let mut index = 0;
        while index < streams.len() {
            if !streams[index].generation.is_active() && Arc::strong_count(&streams[index]) == 1 {
                discarded.push(streams.swap_remove(index));
            } else { index += 1; }
        }
        let stream = match streams.iter().find(|stream| stream.view == owner.view && stream.generation.same_generation(generation)) {
            Some(stream) => stream.clone(),
            None => {
                let stream = Arc::new(SyntheticEventStream { view: owner.view, generation: generation.clone(), state: Mutex::new(SyntheticEventState::default()) });
                streams.push(stream.clone());
                stream
            }
        };
        drop(streams);
        drop(discarded);
        Ok((stream, Box::new(SyntheticEventNode { owner: owner.clone(), channel, args, next: None })))
    }
}

struct SyntheticDrainGuard<'a> { stream: &'a SyntheticEventStream, armed: bool }
impl Drop for SyntheticDrainGuard<'_> {
    fn drop(&mut self) {
        if self.armed
            && let Ok(mut state) = self.stream.state.lock()
        {
            state.draining = false;
        }
    }
}

impl SyntheticEventStream {
    fn enqueue(&self, node: &mut Option<Box<SyntheticEventNode>>) -> Result<bool, String> {
        let mut state = self.state.lock().map_err(|_| "Synthetic event stream is unavailable.")?;
        let node = node.take().ok_or("The synthetic event was already enqueued.")?;
        let mut tail = &mut state.head;
        while let Some(entry) = tail { tail = &mut entry.next; }
        *tail = Some(node);
        if state.draining { Ok(false) } else { state.draining = true; Ok(true) }
    }

    fn drain(&self, events: &dyn EventSink) {
        let mut guard = SyntheticDrainGuard { stream: self, armed: true };
        loop {
            let next = {
                let Ok(mut state) = self.state.lock() else { return; };
                match state.head.take() {
                    Some(mut node) => { state.head = node.next.take(); node }
                    None => { state.draining = false; guard.armed = false; return; }
                }
            };
            if let Err(error) = events.emit_to_owner_checked(&next.owner, next.channel, next.args) {
                eprintln!("synthetic event delivery failed: {error}");
            }
        }
    }
}

pub struct PreparedSyntheticEvent {
    stream: Arc<SyntheticEventStream>,
    node: Option<Box<SyntheticEventNode>>,
    events: Arc<dyn EventSink>,
    drain: bool,
}

impl PreparedSyntheticEvent {
    /// Call only inside the existing canonical authority and revision admission.
    pub fn enqueue(&mut self) -> Result<(), String> {
        self.drain = self.stream.enqueue(&mut self.node)?;
        Ok(())
    }

    /// Call after all admission and revision gates have been released.
    pub fn drain(self) {
        if self.drain { self.stream.drain(self.events.as_ref()); }
    }
}
/// Process-retained fixture operations; snapshots expose the actual retry/drain
/// objects without treating dropped registrations as completed cleanup.
#[derive(Default)]
pub struct SyntheticGeneratedCleanupRegistry {
    state: Arc<Mutex<SyntheticGeneratedCleanupState>>,
}

#[derive(Default)]
struct SyntheticGeneratedCleanupState {
    next: u64,
    reject_next: Option<String>,
    operations: HashMap<u64, Arc<dyn super::GeneratedPathPending>>,
}

impl SyntheticGeneratedCleanupRegistry {
    pub fn reject_next(&self, reason: String) {
        self.state.lock().unwrap_or_else(|error| error.into_inner()).reject_next = Some(reason);
    }

    pub fn retained(&self) -> Vec<Arc<dyn super::GeneratedPathPending>> {
        self.state.lock().unwrap_or_else(|error| error.into_inner()).operations.values().cloned().collect()
    }
}

struct SyntheticGeneratedCleanupRegistration {
    state: Arc<Mutex<SyntheticGeneratedCleanupState>>,
    id: u64,
}

impl super::GeneratedCleanupRegistration for SyntheticGeneratedCleanupRegistration {
    fn complete(self: Box<Self>) {
        self.state.lock().unwrap_or_else(|error| error.into_inner()).operations.remove(&self.id);
    }
}

impl SyntheticGeneratedCleanupRegistry {
    pub fn register(&self, operation: Arc<dyn super::GeneratedPathPending>) -> Result<Box<dyn super::GeneratedCleanupRegistration>, String> {
        let mut state = self.state.lock().map_err(|_| "Synthetic cleanup registry is unavailable")?;
        if let Some(error) = state.reject_next.take() { return Err(error); }
        let id = state.next.checked_add(1).ok_or("Synthetic cleanup registry capacity exhausted")?;
        state.next = id;
        state.operations.insert(id, operation);
        Ok(Box::new(SyntheticGeneratedCleanupRegistration { state: self.state.clone(), id }))
    }
}


#[derive(Clone)]
pub struct SyntheticOwnedEffects {
    ui: Arc<dyn Thread>,
    views: Arc<dyn Views>,
    events: Arc<dyn EventSink>,
    docs: Arc<DocsPublications>,
    event_streams: Arc<SyntheticEventStreams>,
    browsers: Arc<Mutex<BrowserBindings>>,
}

impl SyntheticOwnedEffects {
    pub fn new(ui: Arc<dyn Thread>, views: Arc<dyn Views>, events: Arc<dyn EventSink>) -> Self {
        Self { ui, views, events, docs: Arc::default(), event_streams: Arc::default(), browsers: Arc::default() }
    }

    /// Synthetic fixture insertion under the real modeled browser transition.
    pub fn bind_browser_fixture(&self, owner: &NativeRequestOwner, browser: &crate::ipc::NativeBrowserOwner,
        transition: &crate::ipc::NativeBrowserTransition<'_>) -> Result<(), String> {
        if !(transition.matches_owner(browser) || transition.proposes_owner(browser)) {
            return Err(LOST.into());
        }
        self.check_owner(owner)?;
        let mut browsers = self.browsers.lock().map_err(|_| LOST)?;
        if browsers.get(&owner.view).is_some_and(|(_, prior)| !transition.matches_owner(prior)) {
            return Err(LOST.into());
        }
        browsers.insert(owner.view, (owner.context.as_arc(), browser.clone()));
        Ok(())
    }

    pub fn with_saved_target_creation(
        &self, target: &crate::ipc::NativeSavedTarget, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken],
        effect: &mut dyn FnMut(&crate::ipc::NativeBrowserTransition<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        saved::admit(self, target, participants, cancel, pending, additional, &mut |_| {
            target.reservation().with_initial_owner(target.browser(), |transition| {
                let adoption = target.validate_in_transition(transition)?;
                effect(transition)?;
                adoption.commit();
                Ok(())
            })
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_saved_target_registration(
        &self, target: &crate::ipc::NativeSavedTarget, owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        additional: &[&CancelToken], effect: &mut dyn FnMut(&crate::ipc::NativeBrowserTransition<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        if owner.view != target.view() || !owner.context.renderer().is_some_and(|generation| generation.same_generation(target.created())) {
            return Err(LOST.into());
        }
        let mut owners = Vec::with_capacity(participants.len() + 1);
        owners.push(owner.clone());
        owners.extend_from_slice(participants);
        saved::admit(self, target, &owners, cancel, pending, additional, &mut |_| {
            target.reservation().with_owner(target.browser(), |transition| effect(transition))
        })
    }

    pub fn prepare_event(&self, recipient: &NativeRequestOwner, channel: &str, args: Vec<IpcValue>) -> Result<PreparedSyntheticEvent, String> {
        self.check_owner(recipient)?;
        let channel = crate::channels::CHANNELS.iter().find(|spec| spec.name == channel)
            .map(|spec| spec.name).ok_or_else(|| format!("Unknown synthetic event channel '{channel}'."))?;
        let (stream, node) = self.event_streams.prepare(recipient, channel, args)?;
        Ok(PreparedSyntheticEvent { stream, node: Some(node), events: self.events.clone(), drain: false })
    }

    fn emit_admitted(
        &self, recipient: &NativeRequestOwner, channel: &str, args: Vec<IpcValue>,
        admit: impl FnOnce(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut event = self.prepare_event(recipient, channel, args)?;
        admit(&mut || event.enqueue())?;
        event.drain();
        Ok(())
    }

    /// Called only by fake/probe service constructors, before services are shared.
    pub fn install(mut services: Services) -> Services {
        if services.hosts.get::<OwnedEffectsHost>().is_none() {
            let synthetic = Arc::new(Self::new(
                services.ui.clone(), services.views.clone(), services.events.clone(),
            ));
            let effects: OwnedEffectsHost = synthetic.clone();
            let hosts = Arc::get_mut(&mut services.hosts)
                .expect("synthetic effects must be installed before sharing services");
            hosts.insert(effects);
            hosts.insert(synthetic);
        }
        if services.hosts.get::<Arc<SyntheticGeneratedCleanupRegistry>>().is_none() {
            let hosts = Arc::get_mut(&mut services.hosts)
                .expect("synthetic cleanup registry must be installed before sharing services");
            hosts.insert(Arc::new(SyntheticGeneratedCleanupRegistry::default()));
        }
        services
    }
    /// Capture the first registered fixture context without a Docs publication.
    pub fn capture_invocation_root(
        &self, view: ViewId, expected: &RendererGeneration, origin: &SyntheticNativeOrigin,
    ) -> Result<NativeRequestOwner, String> {
        let current = self.views.renderer_generation(view).ok_or(LOST)?;
        if self.views.sender_of(view).is_none() || !expected.is_active() || !current.same_generation(expected) {
            return Err(LOST.into());
        }
        Ok(NativeRequestOwner { view, context: NativeContext::with_owner_loss(
            SyntheticIdentity { view, docs_document: None, native_parent: None, _binding: None, _permit_loss: None }, expected.clone(), expected.loss_token(),
        ).with_authority(origin.clone()) })
    }

    /// Capture the currently bound fixture Docs permit, preserving explicit lineage.
    pub fn capture_invocation_docs(
        &self, view: ViewId, expected: &RendererGeneration, origin: &SyntheticNativeOrigin,
    ) -> Result<NativeRequestOwner, String> {
        let mut owner = self.capture_owner(view, expected)?;
        if self.docs_document_identity(&owner).is_none() || owner.context.document_lease().is_none() {
            return Err(LOST.into());
        }
        owner.context = owner.context.with_authority(origin.clone());
        Ok(owner)
    }


    /// Reserve a fixture publication without consulting or publishing a current
    /// renderer. The exact reservation is checked by validate at adoption.
    pub fn prepare_docs_document(
        &self, view: ViewId, renderer: &RendererGeneration, document_id: u64, document: RendererGeneration,
    ) -> Result<PreparedSyntheticDocsPublication, String> {
        if !renderer.is_active() || !document.is_active() { return Err(LOST.into()); }
        let publication = Arc::new(DocsPublication { view, renderer: renderer.clone(), document_id, document });
        let (expected, previous) = {
            let mut registry = self.docs.lock().map_err(|_| LOST)?;
            registry.try_reserve(1).map_err(|error| error.to_string())?;
            let slot = registry.entry(view).or_insert_with(Weak::new);
            (slot.clone(), slot.upgrade())
        };
        Ok(PreparedSyntheticDocsPublication {
            registry: self.docs.clone(),
            binding: Some(SyntheticDocsBinding { publication, registry: Arc::downgrade(&self.docs) }),
            previous,
            expected,
        })
    }

    pub fn bind_docs_document(
        &self, view: ViewId, expected: &RendererGeneration, document_id: u64,
        document: RendererGeneration,
    ) -> Result<SyntheticDocsBinding, String> {
        let current = self.views.renderer_generation(view).ok_or(LOST)?;
        if self.views.sender_of(view).is_none() || !current.same_generation(expected) ||
            !expected.is_active() || !document.is_active() { return Err(LOST.into()); }
        let publication = Arc::new(DocsPublication {
            view, renderer: expected.clone(), document_id, document,
        });
        self.docs.lock().unwrap_or_else(|error| error.into_inner())
            .insert(view, Arc::downgrade(&publication));
        Ok(SyntheticDocsBinding { publication, registry: Arc::downgrade(&self.docs) })
    }
    pub fn capture_docs_owner(
        &self, view: ViewId, expected: &RendererGeneration, document: u64,
        lease: RendererGeneration,
    ) -> Result<NativeRequestOwner, String> {
        let mut owner = self.capture_owner(view, expected)?;
        if !lease.is_active() { return Err(LOST.into()); }
        owner.context = NativeContext::with_owner_loss(
            SyntheticIdentity { view, docs_document: Some(document), native_parent: None, _binding: None, _permit_loss: None },
            expected.clone(), expected.loss_token(),
        ).with_document_lease(lease);
        Ok(owner)
    }

    fn check_owner(&self, owner: &NativeRequestOwner) -> Result<(), String> {
        let generation = owner.context.renderer().ok_or(LOST)?;
        let current = self.views.renderer_generation(owner.view).ok_or(LOST)?;
        if self.views.sender_of(owner.view).is_none() || !current.same_generation(generation) {
            return Err(LOST.into());
        }
        Ok(())
    }

    fn check_recovery_preparation(&self, snapshot: &crate::ipc::RecoveryAuthoritySnapshot) -> Result<(), String> {
        if let Some(request) = &snapshot.request {
            for owner in std::iter::once(&request.source).chain(&request.participants) { self.check_owner(owner)?; }
        }
        if let crate::ipc::RecoveryOpenContext::Renderer { source, participants, .. } = &snapshot.context {
            for owner in std::iter::once(source).chain(participants) { self.check_owner(owner)?; }
        }
        if let Some(startup) = &snapshot.startup {
            let root = startup_fixture_root(&startup.startup, &startup.shell)?;
            for owner in [&startup.shell, root].into_iter().chain(&startup.participants) { self.check_owner(owner)?; }
        }
        if let Some(command) = &snapshot.command {
            for owner in &command.participants { self.check_owner(owner)?; }
        }
        Ok(())
    }

    fn owned<T>(
        &self, origin: &NativeRequestOwner, destination: &NativeRequestOwner,
        cancel: &CancelToken, pending: &CancelToken, effect: impl FnOnce() -> T,
    ) -> Result<T, String> {
        self.check_owner(origin)?;
        self.check_owner(destination)?;
        let source = origin.context.renderer().ok_or(LOST)?;
        let target = destination.context.renderer().ok_or(LOST)?;
        let source_loss = origin.loss_token();
        let target_loss = destination.loss_token();
        RendererGeneration::with_active_all([
            Some(source), Some(target), origin.context.document_lease(), destination.context.document_lease(),
        ], || PdfDocumentLease::with_live_all([
            origin.pdf_document(), origin.pdf_revision(), destination.pdf_document(), destination.pdf_revision(),
        ], || CancelToken::with_active_all([
            Some(cancel), Some(pending), source_loss.as_ref(), target_loss.as_ref(),
        ], effect))).flatten().flatten().ok_or_else(|| LOST.into())
    }
}

impl OwnedEffects for SyntheticOwnedEffects {
    fn capture_owner(&self, view: ViewId, expected: &RendererGeneration) -> Result<NativeRequestOwner, String> {
        let current = self.views.renderer_generation(view).ok_or(LOST)?;
        if self.views.sender_of(view).is_none() || !expected.is_active() || !current.same_generation(expected) {
            return Err(LOST.into());
        }
        let publication = self.docs.lock().unwrap_or_else(|error| error.into_inner())
            .get(&view).and_then(Weak::upgrade);
        if publication.as_ref().is_some_and(|document|
            !document.renderer.same_generation(expected) || !document.document.is_active()) {
            return Err(LOST.into());
        }
        let mut context = NativeContext::with_owner_loss(
            SyntheticIdentity { view, docs_document: publication.as_ref().map(|document| document.document_id), native_parent: None, _binding: None, _permit_loss: None },
            expected.clone(), expected.loss_token(),
        );
        if let Some(publication) = publication {
            context = context.with_document_lease(publication.document.clone());
        }
        Ok(NativeRequestOwner { view, context })
    }

    fn with_exact_browser_owner_effect(
        &self, expected: &crate::ipc::NativeBrowserOwner, actual: &NativeRequestOwner,
        cancel: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(actual)?;
        let identity = actual.context.as_arc();
        self.with_owned_effect_many_extra(actual, &[], cancel, cancel, &[], &mut || {
            expected.reservation().with_owner(expected, |_| {
                let matches = self.browsers.lock().map_err(|_| LOST)?.get(&actual.view)
                    .is_some_and(|(captured, browser)| Arc::ptr_eq(captured, &identity) && browser.same_owner(expected));
                if !matches { return Err(LOST.into()); }
                effect()
            })
        })
    }

    fn with_native_saved_target_seed(&self, target: &crate::ipc::NativeSavedTarget, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken],
        effect: &mut dyn FnMut(&crate::ipc::NativeSavedInput) -> Result<(), String>) -> Result<(), String> {
        saved::admit(self, target, participants, cancel, pending, additional, &mut |_| {
            target.reservation().with_owner(target.browser(), |_| {
                target.with_adopted_state(|| effect(target.input())).ok_or_else(|| LOST.to_owned())?
            })
        })
    }

    fn with_native_saved_document_input(&self, document: &crate::ipc::NativeSavedDocument, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken],
        effect: &mut dyn FnMut(&crate::ipc::NativeSavedInput) -> Result<(), String>) -> Result<(), String> {
        saved::document(self, document, None, participants, cancel, pending, additional,
            &mut |_| effect(document.target().input()))
    }

    fn emit_native_saved_document_input(&self, document: &crate::ipc::NativeSavedDocument, participants: &[NativeRequestOwner],
        target: usize, cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], channel: &str, args: Vec<IpcValue>) -> Result<(), String> {
        let recipient = participants.get(target).ok_or(LOST)?;
        self.emit_admitted(recipient, channel, args, |enqueue|
            self.with_native_saved_document_input(document, participants, cancel, pending, additional, &mut |_| enqueue()))
    }

    fn post_ui_native_saved_document_input(
        &self, document: crate::ipc::NativeSavedDocument, participants: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, additional: Vec<CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>, done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let controls: Vec<_> = additional.iter().collect();
        if let Err(error) = self.with_native_saved_document_input(&document, &participants, &cancel, &pending, &controls, &mut |_| Ok(())) {
            return done(Err(error));
        }
        let model = self.clone();
        self.ui.post(Box::new(move || {
            let controls: Vec<_> = additional.iter().collect();
            let mut action = Some(action);
            let mut accepted = None;
            let result = model.with_native_saved_document_input(&document, &participants, &cancel, &pending, &controls, &mut |_| {
                accepted = action.take();
                Ok(())
            }).and_then(|()| accepted.ok_or_else(|| LOST.to_owned())?());
            done(result);
        }));
    }

    fn with_native_saved_document_same_origin(&self, document: &crate::ipc::NativeSavedDocument, actual: &NativeRequestOwner,
        actual_cancel: &CancelToken, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        additional: &[&CancelToken], effect: &mut dyn FnMut(&crate::ipc::NativeSavedInput) -> Result<(), String>) -> Result<(), String> {
        saved::document(self, document, Some((actual, actual_cancel)), participants, cancel, pending, additional,
            &mut |_| effect(document.target().input()))
    }

    fn emit_native_saved_document_same_origin(&self, document: &crate::ipc::NativeSavedDocument, actual: &NativeRequestOwner,
        actual_cancel: &CancelToken, participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken,
        additional: &[&CancelToken], channel: &str, args: Vec<IpcValue>) -> Result<(), String> {
        let recipient = participants.get(target).ok_or(LOST)?;
        self.emit_admitted(recipient, channel, args, |enqueue| self.with_native_saved_document_same_origin(
            document, actual, actual_cancel, participants, cancel, pending, additional, &mut |_| enqueue()))
    }

    fn with_native_saved_document_transition(&self, document: &crate::ipc::NativeSavedDocument, actual: &NativeRequestOwner,
        actual_cancel: &CancelToken, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        additional: &[&CancelToken], effect: &mut dyn FnMut(&crate::ipc::NativeSavedInput,
            &mut crate::runtime::AdmittedDocumentRetirement<'_>) -> Result<(), String>) -> Result<(), String> {
        saved::document(self, document, Some((actual, actual_cancel)), participants, cancel, pending, additional,
            &mut |retirement| effect(document.target().input(), retirement))
    }

    fn stage_saved_docs_media(&self, document: &crate::ipc::NativeSavedDocument, actual: &NativeRequestOwner,
        actual_cancel: &CancelToken, original: &NativeRequestOwner, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], stage: &dyn super::DocsMediaStage) -> Result<(), String> {
        let owner = document.owner();
        if !same_native_origin(owner, actual) || !same_native_origin(owner, original)
            || !actual.context.renderer().zip(owner.context.renderer()).is_some_and(|(a, b)| a.same_generation(b))
            || !original.context.renderer().zip(owner.context.renderer()).is_some_and(|(a, b)| a.same_generation(b))
            || self.docs_document_identity(original) != Some(stage.old_identity()) { return Err(LOST.into()); }
        let mut owners = vec![owner.clone(), actual.clone(), original.clone()];
        owners.extend_from_slice(participants);
        let mut controls = additional.to_vec();
        controls.push(actual_cancel);
        saved::prepared(self, document.target(), &owners, cancel, pending, &controls, |admit| {
            stage.synthetic_stage(&mut |commit| admit(&mut |_| {
                document.target().reservation().with_owner(document.target().browser(), |_| {
                    document.target().with_registered_state(owner, || {
                        let registry = self.docs.lock().unwrap_or_else(|error| error.into_inner());
                        let current = registry.get(&original.view).and_then(Weak::upgrade).ok_or(LOST)?;
                        if current.document_id != stage.old_identity().1
                            || !original.context.document_lease().is_some_and(|old| old.same_generation(&current.document)) {
                            return Err(LOST.into());
                        }
                        commit()
                    }).ok_or_else(|| LOST.to_owned())?
                })
            }))
        })
    }

    fn transition_saved_docs_media(&self, document: &crate::ipc::NativeSavedDocument, actual: &NativeRequestOwner,
        actual_cancel: &CancelToken, original: &NativeRequestOwner, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], transition: &dyn super::DocsMediaTransition,
        retain: &mut dyn FnMut(&NativeRequestOwner, &mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
        before_notify: &mut dyn FnMut(&crate::ipc::NativeSavedDocument, &NativeRequestOwner) -> Result<(), String>,
    ) -> Result<(crate::ipc::NativeSavedDocument, NativeRequestOwner), String> {
        let owner = document.owner();
        let old = transition.old_identity();
        let next = transition.successor_identity();
        if !same_native_origin(owner, actual) || !same_native_origin(owner, original)
            || !actual.context.renderer().zip(owner.context.renderer()).is_some_and(|(a, b)| a.same_generation(b))
            || !original.context.renderer().zip(owner.context.renderer()).is_some_and(|(a, b)| a.same_generation(b))
            || old.0 != owner.view || next.0 != owner.view || old == next
            || self.docs_document_identity(original) != Some(old) { return Err(LOST.into()); }
        let renderer = original.context.renderer().ok_or(LOST)?.clone();
        let previous = original.context.document_lease().ok_or(LOST)?.clone();
        let identity = original.context.downcast_ref::<SyntheticIdentity>().ok_or(LOST)?;
        let parent = identity.native_parent.clone().unwrap_or_else(|| original.context.as_arc());
        let (generation, loss) = transition.synthetic_successor()?;
        let publication = Arc::new(DocsPublication { view: owner.view, renderer: renderer.clone(),
            document_id: next.1, document: generation.clone() });
        let binding = Arc::new(SyntheticDocsBinding { publication: publication.clone(), registry: Arc::downgrade(&self.docs) });
        let mut context = NativeContext::with_owner_loss(
            SyntheticIdentity { view: owner.view, docs_document: Some(next.1), native_parent: Some(parent),
                _binding: Some(binding), _permit_loss: Some(loss) }, renderer.clone(), renderer.loss_token(),
        ).with_document_lease(generation);
        if let Some(origin) = original.context.authority_ref::<SyntheticNativeOrigin>() {
            context = context.with_authority(origin.clone());
        }
        let base = NativeRequestOwner { view: owner.view, context };
        let fresh = |source: &NativeRequestOwner| -> Result<NativeRequestOwner, String> {
            match self.docs_document_identity(source) {
                Some(id) if id == old => Ok(NativeRequestOwner { view: source.view,
                    context: source.context.with_synthetic_media_successor(&base.context).ok_or(LOST)? }),
                Some(_) => Err(LOST.into()),
                None => Ok(source.clone()),
            }
        };
        let successor = fresh(original)?;
        let next_registered = fresh(owner)?;
        let next_actual = fresh(actual)?;
        if !same_native_origin(owner, &successor) || actual.dispatcher_cancellation_for(actual_cancel).is_none() {
            return Err(LOST.into());
        }
        let reply = actual.context.request_authority::<SyntheticReplyAuthority>().ok_or(LOST)?;
        let prepared = document.prepare_owner_successor(next_registered)?;
        self.docs.lock().unwrap_or_else(|error| error.into_inner()).try_reserve(1).map_err(|error| error.to_string())?;
        let mut owners = vec![owner.clone(), actual.clone(), original.clone()];
        owners.extend_from_slice(participants);
        let mut controls = additional.to_vec();
        controls.push(actual_cancel);
        let result = std::cell::RefCell::new(None);
        saved::prepared(self, document.target(), &owners, cancel, pending, &controls, |admit| {
            transition.synthetic_transition(&mut |commit| admit(&mut |_| {
                document.target().reservation().with_owner(document.target().browser(), |browser| {
                    let mut guard = Some(prepared.validate_in_transition(browser)?);
                    let mut registry = self.docs.lock().unwrap_or_else(|error| error.into_inner());
                    let current = registry.get(&owner.view).and_then(Weak::upgrade).ok_or(LOST)?;
                    if current.document_id != old.1 || !current.renderer.same_generation(&renderer)
                        || !current.document.same_generation(&previous) { return Err(LOST.into()); }
                    let mut committed = false;
                    retain(&successor, &mut || {
                        if committed { return Err("The Docs media transition was already committed.".into()); }
                        let guard = guard.take().ok_or("The Docs media transition was already attempted.")?;
                        reply.with_media_transfer(actual, actual_cancel, next_actual.clone(), || {
                            commit()?;
                            committed = true;
                            *result.borrow_mut() = Some((guard.commit(), next_actual.clone()));
                            registry.insert(owner.view, Arc::downgrade(&publication));
                            Ok(())
                        })?;
                        Ok(())
                    })?;
                    if !committed { return Err("The Docs media transition was not committed.".into()); }
                    // Both state changes were committed before control returned to retain.
                    Ok(())
                })
            }), &mut || {
                if let Some((successor, next_actual)) = result.borrow().as_ref() { before_notify(successor, next_actual)?; }
                Ok(())
            })
        })?;
        result.into_inner().ok_or_else(|| LOST.to_owned())
    }

    fn docs_document_identity(&self, owner: &NativeRequestOwner) -> Option<(ViewId, u64)> {
        let identity = owner.context.downcast_ref::<SyntheticIdentity>()?;
        (identity.view == owner.view).then_some((identity.view, identity.docs_document?))
    }

    fn stage_docs_media(
        &self, owner: &NativeRequestOwner, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, stage: &dyn super::DocsMediaStage,
        recovery: Option<&crate::ipc::RecoverySeedReceipt>,
    ) -> Result<(), String> {
        self.check_owner(owner)?;
        for participant in participants { self.check_owner(participant)?; }
        let old = stage.old_identity();
        if old.0 != owner.view || self.docs_document_identity(owner) != Some(old) { return Err(LOST.into()); }
        let renderer = owner.context.renderer().ok_or(LOST)?.clone();
        let document = owner.context.document_lease().ok_or(LOST)?.clone();
        stage.synthetic_stage(&mut |commit| {
            let mut effect = || {
                let registry = self.docs.lock().unwrap_or_else(|error| error.into_inner());
                let current = registry.get(&owner.view).and_then(Weak::upgrade).ok_or(LOST)?;
                if current.document_id != old.1 || !current.renderer.same_generation(&renderer)
                    || !current.document.same_generation(&document) { return Err(LOST.into()); }
                commit()
            };
            match recovery {
                None => self.with_owned_effect_many(owner, participants, cancel, pending, &mut effect),
                Some(receipt) => with_recovery_open_many(receipt, owner, participants, cancel, pending, &mut effect),
            }
        })
    }

    fn transition_docs_media(
        &self, owner: &NativeRequestOwner, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, transition: &dyn super::DocsMediaTransition,
        recovery: Option<&crate::ipc::RecoverySeedReceipt>,
        retain: &mut dyn FnMut(&NativeRequestOwner, &mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
    ) -> Result<NativeRequestOwner, String> {
        self.check_owner(owner)?;
        for participant in participants { self.check_owner(participant)?; }
        let old = transition.old_identity();
        let next = transition.successor_identity();
        if old.0 != owner.view || next.0 != owner.view || old == next || self.docs_document_identity(owner) != Some(old) {
            return Err(LOST.into());
        }
        let renderer = owner.context.renderer().ok_or(LOST)?.clone();
        let previous_document = owner.context.document_lease().ok_or(LOST)?.clone();
        let old_identity = owner.context.downcast_ref::<SyntheticIdentity>().ok_or(LOST)?;
        let native_parent = old_identity.native_parent.clone().unwrap_or_else(|| owner.context.as_arc());
        let (document, loss) = transition.synthetic_successor()?;
        if !document.is_active() { return Err(LOST.into()); }
        let publication = Arc::new(DocsPublication {
            view: owner.view, renderer: renderer.clone(), document_id: next.1, document: document.clone(),
        });
        let binding = Arc::new(SyntheticDocsBinding { publication: publication.clone(), registry: Arc::downgrade(&self.docs) });
        let mut context = NativeContext::with_owner_loss(
            SyntheticIdentity { view: owner.view, docs_document: Some(next.1), native_parent: Some(native_parent),
                _binding: Some(binding), _permit_loss: Some(loss) },
            renderer.clone(), renderer.loss_token(),
        ).with_document_lease(document);
        if let Some(origin) = owner.context.authority_ref::<SyntheticNativeOrigin>() {
            context = context.with_authority(origin.clone());
        }
        let successor = NativeRequestOwner { view: owner.view, context };
        {
            let mut registry = self.docs.lock().unwrap_or_else(|error| error.into_inner());
            registry.try_reserve(1).map_err(|error| error.to_string())?;
        }
        transition.synthetic_transition(&mut |commit| {
            let mut effect = || {
                let mut registry = self.docs.lock().unwrap_or_else(|error| error.into_inner());
                let current = registry.get(&owner.view).and_then(Weak::upgrade).ok_or(LOST)?;
                if current.document_id != old.1 || !current.renderer.same_generation(&renderer)
                    || !current.document.same_generation(&previous_document) {
                    return Err(LOST.into());
                }
                let mut committed = false;
                retain(&successor, &mut || {
                    if committed { return Err("The Docs media transition was already committed.".into()); }
                    commit()?;
                    committed = true;
                    Ok(())
                })?;
                if !committed { return Err("The Docs media transition was not committed.".into()); }
                registry.insert(owner.view, Arc::downgrade(&publication));
                Ok(())
            };
            match recovery {
                None => self.with_owned_effect_many(owner, participants, cancel, pending, &mut effect),
                Some(receipt) => with_recovery_open_many(receipt, owner, participants, cancel, pending, &mut effect),
            }
        }, &mut || Ok(()))?;
        Ok(successor)
    }

    fn with_native_command_creation_effect(
        &self, operation: &crate::ipc::NativeInvocationOperation, expected: &crate::ipc::NativeEditorCommand,
        created: &RendererGeneration, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for participant in participants { self.check_owner(participant)?; }
        with_native_command_creation(operation, expected, created, participants, cancel, pending, effect)
    }

    fn emit_native_command_creation_many(
        &self, operation: &crate::ipc::NativeInvocationOperation, expected: &crate::ipc::NativeEditorCommand,
        created: &RendererGeneration, participants: &[NativeRequestOwner], target: usize,
        cancel: &CancelToken, pending: &CancelToken, channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let recipient = participants.get(target).ok_or("The synthetic event recipient was not admitted.")?;
        self.emit_admitted(recipient, channel, args, |enqueue|
            self.with_native_command_creation_effect(operation, expected, created, participants, cancel, pending, enqueue))
    }

    fn with_native_command_target_effect(
        &self, operation: &crate::ipc::NativeInvocationOperation, expected: &crate::ipc::NativeEditorCommand,
        document: &crate::ipc::NativeInvocationDocument, owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(owner)?;
        for participant in participants { self.check_owner(participant)?; }
        with_native_command_target(operation, expected, document, owner, participants, cancel, pending, effect)
    }

    fn with_created_effect(
        &self, source: &NativeRequestOwner, created: &RendererGeneration,
        cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(source)?;
        let renderer = source.context.renderer().ok_or(LOST)?;
        let loss = source.loss_token();
        RendererGeneration::with_active_all([Some(renderer), source.context.document_lease(), Some(created)], || {
            PdfDocumentLease::with_live_all([source.pdf_document(), source.pdf_revision()], || {
                CancelToken::with_active_all([Some(cancel), Some(pending), loss.as_ref()], effect)
            })
        }).flatten().flatten().ok_or_else(|| LOST.to_owned())?
    }

    fn with_created_many_effect(
        &self, source: &NativeRequestOwner, participants: &[NativeRequestOwner], created: &RendererGeneration,
        cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(source).chain(participants) { self.check_owner(owner)?; }
        with_created_many(source, participants, created, cancel, pending, effect)
    }

    fn with_owned_effect(
        &self, origin: &NativeRequestOwner, destination: &NativeRequestOwner,
        cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.owned(origin, destination, cancel, pending, effect)?
    }

    fn with_owned_effect_many(
        &self, origin: &NativeRequestOwner, destinations: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.with_owned_effect_many_extra(origin, destinations, cancel, pending, &[], effect)
    }

    fn with_owned_effect_many_extra(
        &self, origin: &NativeRequestOwner, destinations: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(origin).chain(destinations) { self.check_owner(owner)?; }
        let created = origin.context.renderer().ok_or(LOST)?;
        with_created_many_additional(origin, destinations, created, cancel, pending, additional, effect)
    }

    fn with_native_operation_effect(
        &self, operation: &crate::ipc::NativeInvocationOperation, origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(origin).chain(participants) { self.check_owner(owner)?; }
        with_native_operation(operation, origin, participants, cancel, pending, effect)
    }

    fn with_prepared_saved_publication(
        &self, publication: &crate::ipc::SavedPublication, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken],
        use_plan: &mut dyn FnMut(&mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut owners = vec![publication.source_owner()];
        let mut generations = Vec::new();
        let mut controls = vec![cancel, pending];
        match publication {
            crate::ipc::SavedPublication::Native(output) => {
                let operation = output.selection().binding().operation();
                generations.push(operation.authority_generation());
                controls.extend([operation.cancel(), operation.shutdown()]);
                owners.extend(output.participants());
                controls.extend([output.cancel(), output.pending()]);
                controls.extend(output.additional());
            }
            crate::ipc::SavedPublication::Renderer(output) => {
                owners.push(output.destination());
                owners.extend(output.participants());
                controls.extend([output.cancel(), output.pending()]);
                controls.extend(output.additional());
            }
        }
        owners.extend(participants);
        let mut pdf = Vec::new();
        let mut losses = Vec::new();
        for owner in owners {
            self.check_owner(owner)?;
            generations.push(owner.context.renderer().ok_or(LOST)?);
            if let Some(document) = owner.context.document_lease() { generations.push(document); }
            if let Some(document) = owner.pdf_document() { pdf.push(document); }
            if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
            losses.push(owner.loss_token().ok_or(LOST)?);
        }
        controls.extend_from_slice(additional);
        controls.extend(losses.iter());
        let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
        let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
        let mut controls = crate::runtime::CancelTokenGatePlan::from_tokens(controls);
        let mut attempted = false;
        let mut admitted = false;
        let mut panic_payload = None;
        let mut admit = |effect: &mut dyn FnMut() -> Result<(), String>| {
            if attempted { return Err("The synthetic saved publication plan was already consumed".into()); }
            attempted = true;
            let result = generations.with_active(|| pdf.with_live(|| controls.with_active(|| {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(effect)) {
                    Ok(result) => result,
                    Err(payload) => { panic_payload = Some(payload); Err("The synthetic saved publication effect panicked".into()) }
                }
            }))).flatten().flatten().ok_or_else(|| LOST.to_owned())?;
            admitted = result.is_ok();
            result
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| use_plan(&mut admit)));
        drop(admit);
        drop(controls);
        drop(pdf);
        drop(generations);
        if let Some(payload) = panic_payload { std::panic::resume_unwind(payload); }
        match result {
            Err(payload) => std::panic::resume_unwind(payload),
            Ok(Ok(())) if !admitted => Err("The synthetic saved publication plan was not admitted".into()),
            Ok(result) => result,
        }
    }

    fn with_existing_document_save_effect(
        &self, binding: &crate::ipc::NativeExistingDocumentBinding, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(binding.owner()).chain(participants) { self.check_owner(owner)?; }
        with_existing_document_save(binding, participants, cancel, pending, effect)
    }

    fn with_existing_document_save_effect_extra(
        &self, binding: &crate::ipc::NativeExistingDocumentBinding, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(binding.owner()).chain(participants) { self.check_owner(owner)?; }
        with_existing_document_save_extra(binding, participants, cancel, pending, additional, effect)
    }

    fn with_existing_document_same_origin_effect(
        &self, binding: &crate::ipc::NativeExistingDocumentBinding, actual: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let expected = binding.owner();
        for owner in std::iter::once(expected).chain(std::iter::once(actual)).chain(participants) { self.check_owner(owner)?; }
        if !same_native_origin(expected, actual)
            || !expected.context.renderer().zip(actual.context.renderer()).is_some_and(|(a, b)| a.same_generation(b)) {
            return Err(LOST.into());
        }
        let mut owners = Vec::with_capacity(participants.len() + 1);
        owners.push(actual.clone());
        owners.extend_from_slice(participants);
        with_existing_document_save_extra(binding, &owners, cancel, pending, additional, effect)
    }

    fn with_saved_reload_acceptance_effect(
        &self, input: &crate::ipc::NativeSavedInput, path: &crate::ipc::NativeSavedPath,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if !path.matches_publication(input.output()) { return Err("Synthetic reload path does not match its publication".into()); }
        if matches!(path, crate::ipc::NativeSavedPath::Selected(_))
            && !input.output().commit().same_commit(input.source_commit()) {
            return Err("Synthetic reload input does not match its publication".into());
        }
        let (drive, moved) = match path {
            crate::ipc::NativeSavedPath::Selected(_) => (None, false),
            crate::ipc::NativeSavedPath::DriveMoved(receipt) => (Some(receipt.preflight_reopen()?), true),
            crate::ipc::NativeSavedPath::DriveFallback(receipt) => (Some(receipt.preflight_reopen()?), false),
        };
        let mut owners = participants.to_vec();
        let mut controls = additional.to_vec();
        if let Some(snapshot) = &drive {
            owners.extend_from_slice(snapshot.participants());
            controls.extend(snapshot.controls());
        }
        self.with_prepared_saved_publication(input.output(), &owners, cancel, pending, &controls, &mut |admit| {
            let mut enter = || admit(effect);
            match &drive {
                Some(snapshot) => snapshot.with_committed_state(moved, input.source_commit(), &mut enter),
                None => enter(),
            }
        })
    }

    fn with_native_save_reload_completion_effect(
        &self, handoff: &crate::hosts::NativeSaveReloadHandoff, loaded: &NativeRequestOwner,
        before_notify: &mut dyn FnMut(),
        effect: &mut dyn FnMut(&mut crate::ipc::AdmittedPdfReloadRetirement<'_, '_>) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut participants = handoff.completion_participants(loaded)?;
        let mut generations = Vec::new();
        let mut publication_controls = Vec::new();
        match handoff.publication() {
            crate::ipc::SavedPublication::Native(value) => {
                let operation = value.selection().binding().operation();
                generations.push(operation.authority_generation());
                publication_controls.extend([operation.cancel(), operation.shutdown(), value.cancel(), value.pending()]);
                publication_controls.extend(value.additional().iter());
                participants.extend(value.participants().iter().map(|owner| if handoff.retires(owner) { loaded.clone() } else { owner.clone() }));
            }
            crate::ipc::SavedPublication::Renderer(value) => {
                publication_controls.extend([value.cancel(), value.pending()]);
                publication_controls.extend(value.additional().iter());
                participants.push(if handoff.retires(value.destination()) { loaded.clone() } else { value.destination().clone() });
                participants.extend(value.participants().iter().map(|owner| if handoff.retires(owner) { loaded.clone() } else { owner.clone() }));
            }
        }
        let owners: Vec<_> = std::iter::once(loaded).chain(participants.iter()).collect();
        for owner in &owners { self.check_owner(owner)?; }
        let mut pdf = vec![handoff.predecessor().pdf_document().ok_or(LOST)?, handoff.predecessor().pdf_revision().ok_or(LOST)?];
        let mut losses = Vec::new();
        for owner in &owners {
            generations.push(owner.context.renderer().ok_or(LOST)?);
            if let Some(document) = owner.context.document_lease() { generations.push(document); }
            if let Some(slot) = owner.pdf_document() { pdf.push(slot); }
            if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
            losses.push(owner.loss_token().ok_or(LOST)?);
        }
        let mut controls = vec![handoff.cancel(), handoff.pending(), handoff.browser_loss(), handoff.reservation_loss()];
        controls.extend(publication_controls);
        controls.extend(losses.iter());
        controls.extend(handoff.additional().iter());
        let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
        let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
        let mut retirement = pdf.prepare_reload_retirement(handoff.predecessor(), loaded)?;
        let mut controls = crate::runtime::CancelTokenGatePlan::from_tokens(controls);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            generations.with_active(|| pdf.with_live_reloading(&mut retirement, |admitted|
                controls.with_active(|| {
                    effect(admitted)?;
                    if !admitted.is_retired() { return Err("The PDF predecessor was not retired.".into()); }
                    handoff.record_completion_admitted(loaded, admitted)
                }))).flatten().flatten().ok_or_else(|| LOST.to_owned())?
        }));
        let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(before_notify));
        drop(retirement);
        match (result, cleanup) {
            (Err(panic), _) | (Ok(_), Err(panic)) => std::panic::resume_unwind(panic),
            (Ok(result), Ok(())) => result,
        }
    }

    fn with_native_save_reload_parser_effect(
        &self, handoff: &crate::hosts::NativeSaveReloadHandoff, actual: &NativeRequestOwner,
        actual_cancel: &CancelToken, effect: &mut dyn FnMut(&crate::ipc::NativeSavedInput) -> Result<(), String>,
    ) -> Result<(), String> {
        let registered = handoff.completed_owner().ok_or("The synthetic reload successor is not registered")?;
        if !same_native_origin(&registered, actual) || !registered.context.renderer().zip(actual.context.renderer())
            .is_some_and(|(expected, actual)| expected.same_generation(actual)) { return Err(LOST.into()); }
        let mut owners = handoff.completion_participants(&registered)?;
        owners.push(actual.clone());
        let mut controls = vec![handoff.browser_loss(), handoff.reservation_loss(), handoff.pending()];
        controls.extend(handoff.additional().iter());
        if matches!(handoff.path(), crate::ipc::NativeSavedPath::Selected(_))
            && !handoff.publication().commit().same_commit(handoff.input().source_commit()) {
            return Err("Synthetic reload input does not match its publication".into());
        }
        let (drive, moved) = match handoff.path() {
            crate::ipc::NativeSavedPath::Selected(_) => (None, false),
            crate::ipc::NativeSavedPath::DriveMoved(receipt) => (Some(receipt.preflight_reopen()?), true),
            crate::ipc::NativeSavedPath::DriveFallback(receipt) => (Some(receipt.preflight_reopen()?), false),
        };
        if let Some(snapshot) = &drive {
            owners.extend(snapshot.participants().iter().map(|owner| if handoff.retires(owner) { registered.clone() } else { owner.clone() }));
            controls.extend(snapshot.controls());
        }
        let generation = match handoff.publication() {
            crate::ipc::SavedPublication::Native(value) => {
                let operation = value.selection().binding().operation();
                controls.extend([operation.cancel(), operation.shutdown(), value.cancel(), value.pending()]);
                controls.extend(value.additional().iter());
                owners.extend(value.participants().iter().map(|owner| if handoff.retires(owner) { registered.clone() } else { owner.clone() }));
                operation.authority_generation()
            }
            crate::ipc::SavedPublication::Renderer(value) => {
                controls.extend([value.cancel(), value.pending()]);
                controls.extend(value.additional().iter());
                owners.push(if handoff.retires(value.destination()) { registered.clone() } else { value.destination().clone() });
                owners.extend(value.participants().iter().map(|owner| if handoff.retires(owner) { registered.clone() } else { owner.clone() }));
                registered.context.renderer().ok_or(LOST)?
            }
        };
        let mut generations = vec![generation];
        let mut pdf = Vec::new();
        let mut losses = Vec::new();
        for owner in std::iter::once(&registered).chain(owners.iter()) {
            self.check_owner(owner)?;
            generations.push(owner.context.renderer().ok_or(LOST)?);
            if let Some(document) = owner.context.document_lease() { generations.push(document); }
            if let Some(document) = owner.pdf_document() { pdf.push(document); }
            if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
            losses.push(owner.loss_token().ok_or(LOST)?);
        }
        controls.extend([actual_cancel, handoff.cancel()]);
        controls.extend(losses.iter());
        let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
        let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
        let mut controls = crate::runtime::CancelTokenGatePlan::from_tokens(controls);
        let mut panic_payload = None;
        let mut enter = || {
            generations.with_active(|| pdf.with_live(|| controls.with_active(|| {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
                    handoff.with_completed_owner(&registered, &mut || effect(handoff.input())))) {
                    Ok(result) => result,
                    Err(payload) => { panic_payload = Some(payload); Err("Synthetic reload parser effect panicked".into()) }
                }
            }))).flatten().flatten().ok_or_else(|| LOST.to_owned())?
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &drive {
            Some(snapshot) => snapshot.with_committed_state(moved, handoff.input().source_commit(), &mut enter),
            None => enter(),
        }));
        drop(enter);
        drop(controls);
        drop(pdf);
        drop(generations);
        if let Some(payload) = panic_payload { std::panic::resume_unwind(payload); }
        match result { Ok(result) => result, Err(payload) => std::panic::resume_unwind(payload) }
    }

    fn with_native_save_reload_reader_effect(
        &self, receipt: &crate::hosts::NativeSaveReloadReceipt, actual: &NativeRequestOwner,
        actual_cancel: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let registered = receipt.successor();
        if !same_native_origin(registered, actual) || !registered.context.renderer().zip(actual.context.renderer())
            .is_some_and(|(expected, actual)| expected.same_generation(actual)) { return Err(LOST.into()); }
        self.check_owner(registered)?;
        self.check_owner(actual)?;
        with_created_many_additional(registered, std::slice::from_ref(actual), registered.context.renderer().ok_or(LOST)?,
            actual_cancel, actual_cancel, &[], effect)
    }

    fn with_native_save_reload_effect(
        &self, receipt: &crate::hosts::NativeSaveReloadReceipt, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.with_native_save_reload_many_effect_extra(receipt, receipt.successor(), &[], receipt.cancel(), receipt.pending(), &[], effect)
    }

    fn with_native_save_reload_many_effect_extra(
        &self, receipt: &crate::hosts::NativeSaveReloadReceipt, owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        extra: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let publication = receipt.publication();
        let mut owners = receipt.participants().to_vec();
        owners.push(receipt.owner_after_reload(publication.source_owner()).clone());
        owners.push(receipt.owner_after_reload(owner).clone());
        owners.extend(participants.iter().map(|owner| receipt.owner_after_reload(owner).clone()));
        let mut additional = vec![receipt.browser_loss(), receipt.reservation_loss(), receipt.cancel(), receipt.pending()];
        additional.extend(receipt.additional().iter());
        additional.extend_from_slice(extra);
        let generation = match publication {
            crate::ipc::SavedPublication::Native(value) => {
                let operation = value.selection().binding().operation();
                additional.extend([operation.cancel(), operation.shutdown(), value.cancel(), value.pending()]);
                additional.extend(value.additional().iter());
                owners.extend(value.participants().iter().map(|owner| receipt.owner_after_reload(owner).clone()));
                operation.authority_generation()
            }
            crate::ipc::SavedPublication::Renderer(value) => {
                additional.extend([value.cancel(), value.pending()]);
                additional.extend(value.additional().iter());
                owners.push(receipt.owner_after_reload(value.destination()).clone());
                owners.extend(value.participants().iter().map(|owner| receipt.owner_after_reload(owner).clone()));
                receipt.successor().context.renderer().ok_or(LOST)?
            }
        };
        for candidate in std::iter::once(receipt.successor()).chain(owners.iter()) { self.check_owner(candidate)?; }
        with_created_many_additional(receipt.successor(), &owners, generation, cancel, pending, &additional, effect)
    }

    fn with_native_command_preparation_effect(
        &self, operation: &crate::ipc::NativeInvocationOperation, expected: &crate::ipc::NativeEditorCommand,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if !operation.is_root_native_command() { return Err(LOST.into()); }
        for participant in participants { self.check_owner(participant)?; }
        with_native_command_creation(operation, expected, operation.authority_generation(), participants, cancel, pending, effect)
    }

    fn with_startup_command_creation_effect(
        &self, startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
        child: &crate::ipc::NativeInvocationOperation, expected: &crate::ipc::NativeEditorCommand,
        created: &RendererGeneration, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(shell).chain(participants) { self.check_owner(owner)?; }
        if matches!(expected, crate::ipc::NativeEditorCommand::SaveDocument { .. }) || !child.is_child_of(startup) || !child.is_native_command()
            || !matches!(child.selection(), crate::ipc::NativeInvocationSelection::NativeCommand(command) if command == expected) {
            return Err(LOST.into());
        }
        with_startup_child_creation(startup, shell, child, created, participants, cancel, pending, effect)
    }

    fn with_startup_command_preparation_effect(
        &self, startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
        child: &crate::ipc::NativeInvocationOperation, expected: &crate::ipc::NativeEditorCommand,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(shell).chain(participants) { self.check_owner(owner)?; }
        with_startup_command_preparation(startup, shell, child, expected, participants, cancel, pending, effect)
    }

    fn with_startup_command_target_effect(
        &self, startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
        child: &crate::ipc::NativeInvocationOperation, expected: &crate::ipc::NativeEditorCommand,
        document: &crate::ipc::NativeInvocationDocument, owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for candidate in [shell, owner].into_iter().chain(participants) { self.check_owner(candidate)?; }
        if matches!(expected, crate::ipc::NativeEditorCommand::SaveDocument { .. }) || !child.is_child_of(startup) || !child.is_native_command()
            || !matches!(child.selection(), crate::ipc::NativeInvocationSelection::NativeCommand(command) if command == expected)
            || !child.authority_generation().same_generation(document.operation().authority_generation()) {
            return Err(LOST.into());
        }
        with_startup_document(startup, shell, document, owner, participants, cancel, pending, &[], effect)
    }

    fn with_startup_recovery_creation_effect(
        &self, startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
        child: &crate::ipc::NativeInvocationOperation, created: &RendererGeneration,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(shell).chain(participants) { self.check_owner(owner)?; }
        with_startup_recovery_creation(startup, shell, child, created, participants, cancel, pending, effect)
    }

    fn with_startup_recovery_seed_effect(
        &self, startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
        target: &crate::ipc::NativeInvocationTarget, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(shell).chain(participants) { self.check_owner(owner)?; }
        with_startup_recovery_seed(startup, shell, target, participants, cancel, pending, effect)
    }

    fn with_startup_recovery_effect(
        &self, startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
        child: &crate::ipc::NativeInvocationDocument, owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for candidate in [shell, owner].into_iter().chain(participants) { self.check_owner(candidate)?; }
        with_startup_recovery(startup, shell, child, owner, participants, cancel, pending, effect)
    }

    fn with_command_recovery_target_effect(
        &self, command: &crate::ipc::NativeInvocationOperation, child: &crate::ipc::NativeInvocationDocument,
        owner: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for candidate in std::iter::once(owner).chain(participants) { self.check_owner(candidate)?; }
        if owner.view != child.target().view()
            || !owner.context.renderer().is_some_and(|generation| generation.same_generation(child.created())) {
            return Err(LOST.into());
        }
        let constraint = crate::ipc::CommandRecoveryConstraint {
            command: command.clone(), participants: Vec::new(), cancel: cancel.clone(), pending: pending.clone(),
        };
        with_command_recovery(None, &constraint, child, owner, participants, cancel, pending, &[], effect)
    }

    fn emit_startup_recovery_many(
        &self, startup: &crate::ipc::NativeInvocationOperation, shell: &NativeRequestOwner,
        child: &crate::ipc::NativeInvocationDocument, owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken,
        channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        for candidate in [shell, owner].into_iter().chain(participants) { self.check_owner(candidate)?; }
        let recipient = participants.get(target).ok_or("The synthetic event recipient was not admitted.")?;
        self.emit_admitted(recipient, channel, args, |enqueue|
            with_startup_recovery(startup, shell, child, owner, participants, cancel, pending, enqueue))
    }

    fn emit_native_operation_many(
        &self, operation: &crate::ipc::NativeInvocationOperation, origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken,
        channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        for owner in std::iter::once(origin).chain(participants) { self.check_owner(owner)?; }
        let recipient = participants.get(target).ok_or("The synthetic event recipient was not admitted.")?;
        self.emit_admitted(recipient, channel, args, |enqueue|
            with_native_operation(operation, origin, participants, cancel, pending, enqueue))
    }

    fn emit_existing_document_save_many(
        &self, binding: &crate::ipc::NativeExistingDocumentBinding, participants: &[NativeRequestOwner],
        target: usize, cancel: &CancelToken, pending: &CancelToken, channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        self.emit_existing_document_save_many_extra(binding, participants, target, cancel, pending, &[], channel, args)
    }

    fn emit_existing_document_save_many_extra(
        &self, binding: &crate::ipc::NativeExistingDocumentBinding, participants: &[NativeRequestOwner],
        target: usize, cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken],
        channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        for owner in std::iter::once(binding.owner()).chain(participants) { self.check_owner(owner)?; }
        let recipient = participants.get(target).ok_or("The synthetic event recipient was not admitted.")?;
        self.emit_admitted(recipient, channel, args, |enqueue|
            self.with_existing_document_save_effect_extra(binding, participants, cancel, pending, additional, enqueue))
    }

    fn emit_native_save_revision_snapshot(
        &self, snapshot: &crate::ipc::NativeSaveRevisionSnapshot, participants: &[NativeRequestOwner],
        target: usize, cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken],
        channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let recipient = participants.get(target).ok_or("The synthetic event recipient was not admitted.")?;
        self.emit_admitted(recipient, channel, args, |enqueue|
            self.with_native_save_revision_snapshot_effect(snapshot, participants, cancel, pending, additional, enqueue))
    }

    fn with_native_invocation_effect(
        &self, document: &crate::ipc::NativeInvocationDocument, origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(origin).chain(participants) { self.check_owner(owner)?; }
        with_native_invocation(document, origin, participants, cancel, pending, effect)
    }

    fn with_native_invocation_effect_extra(
        &self, document: &crate::ipc::NativeInvocationDocument, origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(origin).chain(participants) { self.check_owner(owner)?; }
        with_native_invocation_extra(document, origin, participants, cancel, pending, additional, effect)
    }

    fn emit_native_invocation_many(
        &self, document: &crate::ipc::NativeInvocationDocument, origin: &NativeRequestOwner,
        participants: &[NativeRequestOwner], target: usize, cancel: &CancelToken, pending: &CancelToken,
        channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        for owner in std::iter::once(origin).chain(participants) { self.check_owner(owner)?; }
        let recipient = participants.get(target).ok_or("The synthetic event recipient was not admitted.")?;
        self.emit_admitted(recipient, channel, args, |enqueue|
            with_native_invocation(document, origin, participants, cancel, pending, enqueue))
    }

    fn with_same_native_origin_effect(
        &self, expected: &NativeRequestOwner, actual: &NativeRequestOwner, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        for owner in std::iter::once(expected).chain(std::iter::once(actual)).chain(participants) { self.check_owner(owner)?; }
        with_same_native_origin(expected, actual, participants, cancel, pending, additional, effect)
    }

    fn with_recovery_dialog_effect(
        &self, target: &crate::ipc::RecoveryOpenTarget, dialog: &NativeRequestOwner,
        dialog_generation: &RendererGeneration, cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(dialog)?;
        let snapshot = target.authority_snapshot()?;
        self.check_recovery_preparation(&snapshot)?;
        recovery_pre_navigation(target, &snapshot, Some((dialog, dialog_generation, cancel, pending)), effect)?
    }

    fn with_recovery_dialog_snapshot(
        &self, target: &crate::ipc::RecoveryOpenTarget, snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        dialog: &NativeRequestOwner, dialog_generation: &RendererGeneration,
        cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(dialog)?;
        self.check_recovery_preparation(snapshot)?;
        recovery_pre_navigation(target, snapshot, Some((dialog, dialog_generation, cancel, pending)), effect)?
    }

    fn emit_recovery_dialog(
        &self, target: &crate::ipc::RecoveryOpenTarget, dialog: &NativeRequestOwner,
        generation: &RendererGeneration, cancel: &CancelToken, pending: &CancelToken,
        channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        self.check_owner(dialog)?;
        let snapshot = target.authority_snapshot()?;
        self.check_recovery_preparation(&snapshot)?;
        self.emit_admitted(dialog, channel, args, |enqueue|
            recovery_pre_navigation(target, &snapshot, Some((dialog, generation, cancel, pending)), enqueue)?)
    }

    fn emit_recovery_dialog_snapshot(
        &self, target: &crate::ipc::RecoveryOpenTarget, snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        dialog: &NativeRequestOwner, generation: &RendererGeneration, cancel: &CancelToken, pending: &CancelToken,
        channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        self.check_owner(dialog)?;
        self.check_recovery_preparation(snapshot)?;
        self.emit_admitted(dialog, channel, args, |enqueue|
            recovery_pre_navigation(target, snapshot, Some((dialog, generation, cancel, pending)), enqueue)?)
    }

    fn with_recovery_seed_effect(
        &self, target: &crate::ipc::RecoveryOpenTarget, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let snapshot = target.authority_snapshot()?;
        self.check_recovery_preparation(&snapshot)?;
        recovery_pre_navigation(target, &snapshot, None, effect)?
    }

    fn with_recovery_seed_snapshot(
        &self, target: &crate::ipc::RecoveryOpenTarget, snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_recovery_preparation(snapshot)?;
        recovery_pre_navigation(target, snapshot, None, effect)?
    }

    fn commit_recovery_seed(
        &self, target: &crate::ipc::RecoveryOpenTarget, enqueue: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<crate::ipc::RecoverySeedReceipt, String> {
        let snapshot = target.authority_snapshot()?;
        self.check_recovery_preparation(&snapshot)?;
        commit_recovery_seed_snapshot(target, &snapshot, enqueue)
    }

    fn commit_recovery_seed_snapshot(
        &self, target: &crate::ipc::RecoveryOpenTarget, snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        enqueue: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<crate::ipc::RecoverySeedReceipt, String> {
        self.check_recovery_preparation(snapshot)?;
        commit_recovery_seed_snapshot(target, snapshot, enqueue)
    }

    fn with_recovery_open_snapshot(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        receiver: &NativeRequestOwner, participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(receiver)?;
        for participant in participants { self.check_owner(participant)?; }
        with_recovery_open_snapshot_impl(receipt, snapshot, receiver, participants, cancel, pending, &[], effect)
    }

    fn with_recovery_open_effect(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner,
        cancel: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(receiver)?;
        with_recovery_open(receipt, receiver, cancel, effect)
    }

    fn with_recovery_open_effect_pending(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner,
        cancel: &CancelToken, pending: &CancelToken, effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(receiver)?;
        with_recovery_open_pending(receipt, receiver, cancel, pending, effect)
    }

    fn with_recovery_open_effect_many(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(receiver)?;
        for participant in participants { self.check_owner(participant)?; }
        with_recovery_open_many(receipt, receiver, participants, cancel, pending, effect)
    }

    fn with_recovery_open_effect_many_extra(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(receiver)?;
        for participant in participants { self.check_owner(participant)?; }
        with_recovery_open_many_extra(receipt, receiver, participants, cancel, pending, additional, effect)
    }

    fn with_recovery_pdf_revision_transition(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        old: &PdfDocumentLease, successor: &PdfDocumentLease, renewal: &mut crate::ipc::PreparedRecoveryAuthorityRenewal,
        before_notify: &mut dyn FnMut(),
        effect: &mut dyn FnMut(&mut crate::ipc::AdmittedPdfRevisionRetirement<'_, '_>,
            &mut crate::ipc::PreparedRecoveryAuthorityRenewal) -> Result<(), String>,
    ) -> Result<(), String> {
        pdf_transition::transition(self, receipt, receiver, participants, cancel, pending, old, successor, renewal, before_notify, effect)
    }

    fn with_native_save_revision_transition(
        &self, prepared: &mut crate::ipc::PreparedNativeSaveRevision, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], before_notify: &mut dyn FnMut(),
        effect: &mut dyn FnMut(&mut crate::ipc::AdmittedPdfRevisionSet<'_, '_>,
            &mut crate::ipc::PreparedNativeSaveRevision) -> Result<(), String>,
    ) -> Result<(), String> {
        let snapshot = prepared.snapshot().clone();
        for owner in std::iter::once(snapshot.selection().binding().owner())
            .chain(snapshot.aliases()).chain(prepared.request_aliases()).chain(participants) {
            self.check_owner(owner)?;
        }
        with_native_save_revision_transition(prepared, participants, cancel, pending, additional, before_notify, effect)
    }

    fn emit_recovery_open_many(
        &self,receipt:&crate::ipc::RecoverySeedReceipt,receiver:&NativeRequestOwner,
        participants:&[NativeRequestOwner],target:usize,cancel:&CancelToken,pending:&CancelToken,
        channel:&str,args:Vec<IpcValue>,
    )->Result<(),String> {
        self.check_owner(receiver)?;
        for participant in participants { self.check_owner(participant)?; }
        let recipient=participants.get(target).ok_or("The synthetic event recipient was not admitted.")?;
        self.emit_admitted(recipient, channel, args, |enqueue|
            with_recovery_open_many(receipt, receiver, participants, cancel, pending, enqueue))
    }

    fn emit_recovery_open_snapshot(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        receiver: &NativeRequestOwner, participants: &[NativeRequestOwner], target: usize,
        cancel: &CancelToken, pending: &CancelToken, channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        self.check_owner(receiver)?;
        for participant in participants { self.check_owner(participant)?; }
        let recipient = participants.get(target).ok_or("The synthetic event recipient was not admitted.")?;
        self.emit_admitted(recipient, channel, args, |enqueue|
            with_recovery_open_snapshot_impl(receipt, snapshot, receiver, participants, cancel, pending, &[], enqueue))
    }

    fn settle_recovery_seed(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner,
        cancel: &CancelToken, result: Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(receiver)?;
        settle_recovery_seed(receipt, receiver, cancel, result)
    }

    fn settle_recovery_seed_snapshot(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        receiver: &NativeRequestOwner, cancel: &CancelToken, result: Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(receiver)?;
        let mut result = Some(result);
        let mut deferred = DeferredSettlement(None);
        with_recovery_open_snapshot_impl(receipt, snapshot, receiver, &[], cancel, cancel, &[], &mut || {
            deferred.0 = Some(receipt.settle_from_admitted(result.take().ok_or(LOST)?)?);
            Ok(())
        })?;
        Ok(())
    }

    fn with_owned_transition(
        &self, origin: &NativeRequestOwner, destination: &NativeRequestOwner,
        cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut(&mut crate::runtime::AdmittedDocumentRetirement<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_owner(origin)?;
        self.check_owner(destination)?;
        let source = origin.context.renderer().ok_or(LOST)?;
        let target = destination.context.renderer().ok_or(LOST)?;
        let source_loss = origin.loss_token();
        let target_loss = destination.loss_token();
        let documents = [origin.context.document_lease(), destination.context.document_lease()];
        let mut notifications = DeferredRetirements([None, None]);
        RendererGeneration::with_active_all_retiring([
            Some(source), Some(target), documents[0], documents[1],
        ], documents, &mut notifications.0, |retirement| {
            PdfDocumentLease::with_live_all([
                origin.pdf_document(), origin.pdf_revision(), destination.pdf_document(), destination.pdf_revision(),
            ], || CancelToken::with_active_all([
                Some(cancel), Some(pending), source_loss.as_ref(), target_loss.as_ref(),
            ], || effect(retirement)))
        }).flatten().flatten().ok_or_else(|| LOST.to_owned())?
    }

    fn with_owned_transition_many(
        &self, origin: &NativeRequestOwner, destinations: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken,
        effect: &mut dyn FnMut(&mut crate::runtime::AdmittedDocumentRetirement<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        let count = destinations.len() + 1;
        let mut generations = Vec::with_capacity(count * 2);
        let mut documents = Vec::with_capacity(count);
        let mut pdf = Vec::with_capacity(count * 2);
        let mut losses = Vec::with_capacity(count);
        for owner in std::iter::once(origin).chain(destinations) {
            self.check_owner(owner)?;
            generations.push(owner.context.renderer().ok_or(LOST)?);
            if let Some(document) = owner.context.document_lease() {
                generations.push(document);
                documents.push(document);
            }
            if let Some(document) = owner.pdf_document() { pdf.push(document); }
            if let Some(revision) = owner.pdf_revision() { pdf.push(revision); }
            if let Some(loss) = owner.loss_token() { losses.push(loss); }
        }
        let mut tokens = Vec::with_capacity(losses.len() + 2);
        tokens.push(cancel);
        tokens.push(pending);
        tokens.extend(losses.iter());
        let mut generations = crate::runtime::RendererGenerationGatePlan::from_generations(generations);
        let mut retirement = generations.prepare_retirement(&documents)?;
        let mut pdf = crate::ipc::PdfDocumentGatePlan::new(&pdf);
        let mut tokens = crate::runtime::CancelTokenGatePlan::from_tokens(tokens);
        generations.with_active_retiring(&mut retirement, |admitted| {
            pdf.with_live(|| tokens.with_active(|| effect(admitted)))
        }).flatten().flatten().ok_or_else(|| LOST.to_owned())?
    }

    fn emit_owned(
        &self, origin: &NativeRequestOwner, destination: &NativeRequestOwner,
        cancel: &CancelToken, pending: &CancelToken, channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        self.emit_admitted(destination, channel, args, |enqueue|
            self.with_owned_effect(origin, destination, cancel, pending, enqueue))
    }

    fn emit_owned_many(
        &self, origin: &NativeRequestOwner, destinations: &[NativeRequestOwner], target: usize,
        cancel: &CancelToken, pending: &CancelToken, channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        self.emit_owned_many_extra(origin, destinations, target, cancel, pending, &[], channel, args)
    }

    fn emit_owned_many_extra(
        &self, origin: &NativeRequestOwner, destinations: &[NativeRequestOwner], target: usize,
        cancel: &CancelToken, pending: &CancelToken, additional: &[&CancelToken], channel: &str, args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let target = destinations.get(target).ok_or("The synthetic event recipient was not admitted.")?;
        self.emit_admitted(target, channel, args, |enqueue|
            self.with_owned_effect_many_extra(origin, destinations, cancel, pending, additional, enqueue))
    }

    fn post_ui_recovery_open_many(
        &self, receipt: crate::ipc::RecoverySeedReceipt, receiver: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>, done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        self.ui.post(Box::new(move || {
            let mut action = Some(action);
            let mut accepted = None;
            let admitted = with_recovery_open_many(&receipt, &receiver, &participants, &cancel, &pending, &mut || {
                accepted = action.take();
                Ok(())
            });
            done(admitted.and_then(|()| (accepted.ok_or_else(|| "The synthetic recovery UI action was not admitted.".to_owned())?)()));
        }));
    }

    fn post_ui_existing_document_save(
        &self, binding: crate::ipc::NativeExistingDocumentBinding, participants: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        self.post_ui_existing_document_save_extra(binding, participants, cancel, pending, Vec::new(), action, done)
    }

    fn post_ui_existing_document_save_extra(
        &self, binding: crate::ipc::NativeExistingDocumentBinding, participants: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, additional: Vec<CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let model = self.clone();
        self.ui.post(Box::new(move || {
            let mut action = Some(action);
            let mut accepted = None;
            let extra = additional.iter().collect::<Vec<_>>();
            let admitted = model.with_existing_document_save_effect_extra(&binding, &participants, &cancel, &pending, &extra, &mut || {
                accepted = action.take();
                Ok(())
            });
            done(admitted.and_then(|()| (accepted.ok_or_else(|| "The synthetic save UI action was not admitted.".to_owned())?)()));
        }));
    }

    fn post_ui_native_save_revision_snapshot(
        &self, snapshot: crate::ipc::NativeSaveRevisionSnapshot, participants: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, additional: Vec<CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>, done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let model = self.clone();
        self.ui.post(Box::new(move || {
            let mut action = Some(action);
            let mut accepted = None;
            let extra = additional.iter().collect::<Vec<_>>();
            let admitted = model.with_native_save_revision_snapshot_effect(&snapshot, &participants, &cancel, &pending, &extra, &mut || {
                accepted = action.take(); Ok(())
            });
            done(admitted.and_then(|()| (accepted.ok_or_else(|| "The synthetic save UI action was not admitted.".to_owned())?)()));
        }));
    }

    fn post_ui_owned(
        &self, origin: NativeRequestOwner, destination: NativeRequestOwner,
        cancel: CancelToken, pending: CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let model = self.clone();
        self.ui.post(Box::new(move || {
            let admitted = model.owned(&origin, &destination, &cancel, &pending, || action);
            let result = admitted.and_then(|action| action());
            done(result);
        }));
    }

    fn post_ui_owned_many(
        &self, origin: NativeRequestOwner, destinations: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        self.post_ui_owned_many_extra(origin, destinations, cancel, pending, Vec::new(), action, done)
    }

    fn post_ui_owned_many_extra(
        &self, origin: NativeRequestOwner, destinations: Vec<NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, additional: Vec<CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let model = self.clone();
        self.ui.post(Box::new(move || {
            let mut action = Some(action);
            let mut accepted = None;
            let extra = additional.iter().collect::<Vec<_>>();
            let admission = model.with_owned_effect_many_extra(&origin, &destinations, &cancel, &pending, &extra, &mut || {
                accepted = action.take();
                Ok(())
            });
            let result = admission.and_then(|()| {
                let action = accepted.ok_or_else(|| "The synthetic UI action was not admitted.".to_owned())?;
                action()
            });
            done(result);
        }));
    }
}

#[cfg(test)]
pub(crate) mod tests;
