//! The IPC boundary. A renderer keeps calling `ipcRenderer.invoke/send/on`
//! (its preload is unchanged, bundled over the shell's bridge); the shell
//! turns each call into a [`Message`] for the handler registered under the
//! same channel name, and every message to the renderer (a reply or an event)
//! goes back over one ordered stream per view.

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::rc::Rc;

use crate::channels::{Batch, Surface};
use crate::error::IpcError;
use crate::ids::{RequestId, ViewId, WindowId};
use crate::provided::Provided;
use crate::runtime::{CancelSubscription, CancelToken, RendererGeneration, Services};
use crate::value::IpcValue;

mod native_invocation;
mod native_closure;
mod native_placement;
mod native_drive_reopen;
mod native_saved_path;
mod native_saved_target;
mod direct_write;
pub use direct_write::{AdmittedDirectWrite, UnadmittedDirectWrite, CompletedDirectWrite, PartialAdmittedDirectWrite, DirectWriteCompletion, DirectWriteResumeError, DirectWriteStartError, StagedWriteError, RendererCommittedOutput, SavedPublication};
pub(crate) use direct_write::{publish_staged, publish_staged_validated, DirectWriteOrigin, DirectWriteScope};
pub use native_saved_target::{NativeCommittedOutput, NativeSavedInput, NativeSavedInputKind, NativeSavedTarget, NativeSavedDocument, NativeSavedDocumentSuccessor, NativeSavedDocumentSuccessorGuard};
pub use native_saved_path::NativeSavedPath;
pub use native_drive_reopen::{NativeDriveMoveReceipt, NativeDrivePendingOutcome, NativeDrivePendingReceipt, NativeDriveReopen, NativeDriveReopenSnapshot};
pub use native_placement::{PlacementMoveIdentity, PlacementMoveIssuer, PlacementSuperseded};
pub use native_closure::{NativeClosureIssuer, NativeClosureOutcome, NativeClosureReceipt, NativeClosureReservation,
    NativeBrowserOwner, NativeBrowserTransition, NativeClosedBrowserTransition, NativeTerminalBrowserTransition, NativeBrowserCompensationTransition, NativeBrowserSuperseded, NativeBrowserClosePermit, NativeBrowserCloseClaim, NativeBrowserMovePermit, NativeBrowserReplacement, NativeBrowserRetirement};
pub use native_invocation::{
    NativeInvocationIssuer, NativeInvocationOperation, NativeInvocationPurpose,
    NativeEditorCommand, NativeSaveSelection, NativeSaveFormat, NativeExistingDocumentBinding, NativeSaveDestination, NativeSaveNumberedDestination,
    NativeSaveRevisionAuthority, NativeSaveRevisionSnapshot, NativeSaveRevisionReceipt, PreparedNativeSaveRevision, PreparedNativeSaveSelection, AdmittedNativeSaveSelection,
    NativeInvocationSelection, NativeInvocationTarget, NativeInvocationDocument, NativeHeadlessCleanup, NativeRecoveryCopy,
    RecoveryWorkTicket,
    StartupRecoveryConstraint,
    CommandRecoveryConstraint,
    RecoveryAuthority, RecoveryAuthoritySnapshot, PreparedRecoveryAuthorityRenewal,
    NativeRecoveryDecision, RecoveryOpenContext, RecoveryOpenTarget, RecoveryRequestConstraint, RecoverySeedReceipt, DeferredRecoverySettlement,
};
mod recovery_prepare;
pub use recovery_prepare::{RecoveryCommittedDestination, RecoveryPreparedDestination};

/// How a renderer uses a channel
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    /// `ipcRenderer.invoke`: one reply, a value or an [`IpcError`]
    Invoke,
    /// `ipcRenderer.send`: no reply; handled in arrival order per view
    Send,
    /// `ipcRenderer.sendSync`: the renderer blocks until the reply
    SendSync,
    /// `ipcRenderer.on/once`: the main side emits, the renderer listens
    Event,
    /// `ipcRenderer.postMessage` with ports (no renderer uses it today)
    Port,
}

/// Who sent a message: the view, its window, and the preload it loaded
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sender {
    pub view: ViewId,
    pub window: WindowId,
    pub surface: Surface,
}

/// A single native PDF file-slot lifetime. Replacement creates a new lease.
#[derive(Clone, Debug)]
pub struct PdfDocumentLease(RendererGeneration);

impl PdfDocumentLease {
    pub fn new() -> Self { Self(RendererGeneration::new()) }
    pub fn is_live(&self) -> bool { self.0.is_active() }
    pub fn same_document(&self, other: &Self) -> bool { self.0.same_generation(&other.0) }
    pub fn with_live<T>(&self, commit: impl FnOnce() -> T) -> Option<T> { self.0.with_active(commit) }
    pub fn with_live_all<T, const N: usize>(leases: [Option<&Self>; N], commit: impl FnOnce() -> T) -> Option<T> {
        RendererGeneration::with_active_all(leases.map(|lease| lease.map(|lease| &lease.0)), commit)
    }
    /// Cancel the returned notification only after releasing any outer gates.
    pub fn revoke_deferred(&self) -> CancelToken { self.0.revoke_deferred() }
    pub fn loss_token(&self) -> CancelToken { self.0.loss_token() }
}

/// Preallocated canonical file-slot/revision gates for a multi-owner admission.
pub struct PdfDocumentGatePlan<'a>(crate::runtime::RendererGenerationGatePlan<'a>);

pub struct PdfRevisionRetirementPlan<'a> {
    inner: crate::runtime::DocumentRetirementPlan<'a>,
    old: &'a PdfDocumentLease,
    sources: Vec<&'a NativeRequestOwner>,
    successor: &'a PdfDocumentLease,
}

pub struct AdmittedPdfRevisionRetirement<'a, 'b> {
    inner: &'a mut crate::runtime::AdmittedDocumentRetirement<'b>,
    old: &'a PdfDocumentLease,
    sources: &'a [&'b NativeRequestOwner],
    successor: &'a PdfDocumentLease,
    committed: bool,
}

/// Deferred retirement of the exact old PDF slot and revision during reload.
pub struct PdfReloadRetirementPlan<'a> {
    inner: crate::runtime::DocumentRetirementPlan<'a>,
    slot: &'a PdfDocumentLease,
    revision: &'a PdfDocumentLease,
}
pub struct AdmittedPdfReloadRetirement<'a, 'b> {
    inner: &'a mut crate::runtime::AdmittedDocumentRetirement<'b>,
    slot: &'a PdfDocumentLease,
    revision: &'a PdfDocumentLease,
    committed: bool,
}
impl AdmittedPdfReloadRetirement<'_, '_> {
    pub fn retire(&mut self) -> Result<(), String> {
        if self.committed { return Err("The PDF reload retirement was already consumed.".into()); }
        self.inner.retire(&self.slot.0)?;
        if !self.slot.same_document(self.revision) { self.inner.retire(&self.revision.0)?; }
        self.committed = true;
        Ok(())
    }
    pub fn is_retired(&self) -> bool { self.committed }
    pub fn matches_predecessor(&self, owner: &NativeRequestOwner) -> bool {
        owner.pdf_document().is_some_and(|slot| slot.same_document(self.slot))
            && owner.pdf_revision().is_some_and(|revision| revision.same_document(self.revision))
    }
}

/// A prepared multi-view revision advance; old/new pairs are immutable and
/// held in the same canonical PDF gate.
pub struct PdfRevisionSetPlan<'a> {
    inner: crate::runtime::DocumentRetirementPlan<'a>,
    pairs: Vec<(&'a PdfDocumentLease, &'a PdfDocumentLease)>,
}
pub struct AdmittedPdfRevisionSet<'a, 'b> {
    inner: &'a mut crate::runtime::AdmittedDocumentRetirement<'b>,
    pairs: &'a [(&'b PdfDocumentLease, &'b PdfDocumentLease)],
    committed: bool,
}
impl AdmittedPdfRevisionSet<'_, '_> {
    pub fn contains(&self, old: &PdfDocumentLease, new: &PdfDocumentLease) -> bool {
        self.pairs.iter().any(|(a, b)| a.same_document(old) && b.same_document(new))
    }
    pub fn matches_exact(&self, pairs: &[(PdfDocumentLease, PdfDocumentLease)]) -> bool {
        self.pairs.len() == pairs.len() && self.pairs.iter().zip(pairs).all(|((old, new), (a, b))|
            old.same_document(a) && new.same_document(b))
    }
    pub fn retire_all(&mut self) -> Result<(), String> {
        if self.committed { return Err("The PDF revision set was already consumed.".into()); }
        for (i, (old, _)) in self.pairs.iter().enumerate() {
            if !self.pairs[..i].iter().any(|(previous, _)| previous.same_document(old)) {
                self.inner.retire(&old.0)?;
            }
        }
        self.committed = true;
        Ok(())
    }
    pub fn is_retired(&self) -> bool { self.committed }
}

impl AdmittedPdfRevisionRetirement<'_, '_> {
    pub fn commit(&mut self, prepared: &mut Option<PreparedPdfRevisionOwner>) -> Result<NativeRequestOwner, String> {
        let candidate = prepared.as_ref().ok_or("The PDF successor was already consumed.")?;
        if self.committed || !self.matches(candidate) {
            return Err("The PDF successor does not match the admitted revision.".into());
        }
        self.inner.retire(&self.old.0)?;
        self.committed = true;
        Ok(prepared.take().expect("validated PDF successor").owner)
    }

    fn matches(&self, candidate: &PreparedPdfRevisionOwner) -> bool {
        candidate.source.same_document(self.old)
            && candidate.owner.pdf_revision().is_some_and(|revision| revision.same_document(self.successor))
            && self.sources.iter().any(|source| candidate.source_owner.view == source.view
                && Arc::ptr_eq(&candidate.source_owner.context.as_arc(), &source.context.as_arc())
                && candidate.source_owner.loss_token().zip(source.loss_token()).is_some_and(|(a, b)| a.same_token(&b)))
    }

    /// Every input/output slot is prepared outside admission. Validate all
    /// captured owners before retiring the shared old revision once.
    pub fn commit_many(&mut self, prepared: &mut [Option<PreparedPdfRevisionOwner>],
        output: &mut [Option<NativeRequestOwner>]) -> Result<(), String> {
        if self.committed || prepared.is_empty() || prepared.len() != output.len()
            || output.iter().any(Option::is_some)
            || prepared.iter().any(|candidate| candidate.as_ref().is_none_or(|candidate| !self.matches(candidate))) {
            return Err("The PDF successors do not match the admitted revisions.".into());
        }
        self.inner.retire(&self.old.0)?;
        self.committed = true;
        for (candidate, output) in prepared.iter_mut().zip(output.iter_mut()) {
            *output = Some(candidate.take().expect("validated PDF successor").owner);
        }
        Ok(())
    }
}

impl<'a> PdfDocumentGatePlan<'a> {
    pub fn new(leases: &[&'a PdfDocumentLease]) -> Self {
        Self(crate::runtime::RendererGenerationGatePlan::from_generations(
            leases.iter().map(|lease| &lease.0).collect(),
        ))
    }

    pub fn with_live<T>(&mut self, effect: impl FnOnce() -> T) -> Option<T> {
        self.0.with_active(effect)
    }
    pub fn prepare_revision_set(&self, pairs: &[(&'a PdfDocumentLease, &'a PdfDocumentLease)])
        -> Result<PdfRevisionSetPlan<'a>, String> {
        if pairs.is_empty() || pairs.iter().any(|(old, new)| old.same_document(new))
            || pairs.iter().any(|(old, new)| pairs.iter().any(|(other_old, other_new)|
                (old.same_document(other_old) && !new.same_document(other_new))
                    || (!old.same_document(other_old) && new.same_document(other_new))
                    || new.same_document(other_old))) {
            return Err("The PDF revision successor set is inconsistent.".into());
        }
        let mut leases = Vec::with_capacity(pairs.len() * 2);
        for (old, new) in pairs { leases.extend([&old.0, &new.0]); }
        let inner = self.0.prepare_retirement(&leases)?;
        Ok(PdfRevisionSetPlan { inner, pairs: pairs.to_vec() })
    }

    pub fn with_live_revision_set<T>(&mut self, retirement: &mut PdfRevisionSetPlan<'a>,
        effect: impl FnOnce(&mut AdmittedPdfRevisionSet<'_, '_>) -> T) -> Option<T> {
        self.0.with_active_retiring(&mut retirement.inner, |inner| effect(&mut AdmittedPdfRevisionSet {
            inner, pairs: &retirement.pairs, committed: false,
        }))
    }
    pub fn prepare_reload_retirement(&self, predecessor: &'a NativeRequestOwner,
        successor: &'a NativeRequestOwner) -> Result<PdfReloadRetirementPlan<'a>, String> {
        let slot = predecessor.pdf_document().ok_or("The previous PDF slot is unavailable.")?;
        let revision = predecessor.pdf_revision().ok_or("The previous PDF revision is unavailable.")?;
        let next_slot = successor.pdf_document().ok_or("The successor PDF slot is unavailable.")?;
        let next_revision = successor.pdf_revision().ok_or("The successor PDF revision is unavailable.")?;
        if slot.same_document(next_slot) || slot.same_document(next_revision)
            || revision.same_document(next_slot) || revision.same_document(next_revision) {
            return Err("The PDF reload successor aliases its predecessor.".into());
        }
        let inner = self.0.prepare_retirement(&[&slot.0, &revision.0])?;
        Ok(PdfReloadRetirementPlan { inner, slot, revision })
    }

    pub fn with_live_reloading<T>(&mut self, retirement: &mut PdfReloadRetirementPlan<'a>,
        effect: impl FnOnce(&mut AdmittedPdfReloadRetirement<'_, '_>) -> T) -> Option<T> {
        self.0.with_active_retiring(&mut retirement.inner, |admitted| effect(&mut AdmittedPdfReloadRetirement {
            inner: admitted, slot: retirement.slot, revision: retirement.revision, committed: false,
        }))
    }


    pub fn prepare_retirement(&self, source: &'a NativeRequestOwner, revision: &'a PdfDocumentLease,
        successor: &'a PdfDocumentLease) -> Result<PdfRevisionRetirementPlan<'a>, String> {
        self.prepare_retirement_many(source, &[], revision, successor)
    }

    pub fn prepare_retirement_many(&self, source: &'a NativeRequestOwner, additional: &[&'a NativeRequestOwner],
        revision: &'a PdfDocumentLease, successor: &'a PdfDocumentLease) -> Result<PdfRevisionRetirementPlan<'a>, String> {
        if !source.pdf_revision().is_some_and(|actual| actual.same_document(revision)) || revision.same_document(successor) {
            return Err("The PDF retirement does not match its captured owner.".into());
        }
        let mut sources = Vec::with_capacity(additional.len() + 1);
        sources.push(source);
        sources.extend_from_slice(additional);
        self.0.prepare_retirement(&[&revision.0, &successor.0]).map(|inner| PdfRevisionRetirementPlan { inner, old: revision, sources, successor })
    }

    pub fn with_live_retiring<T>(&mut self, retirement: &mut PdfRevisionRetirementPlan<'a>,
        effect: impl FnOnce(&mut AdmittedPdfRevisionRetirement<'_, '_>) -> T) -> Option<T> {
        self.0.with_active_retiring(&mut retirement.inner, |admitted| effect(&mut AdmittedPdfRevisionRetirement {
            inner: admitted, old: retirement.old, sources: &retirement.sources, successor: retirement.successor, committed: false,
        }))
    }
}

impl Default for PdfDocumentLease {
    fn default() -> Self { Self::new() }
}

struct PdfOwnerLoss {
    _previous: Option<Arc<PdfOwnerLoss>>,
    _ingress: CancelSubscription,
    _document: CancelSubscription,
}

/// Actual dispatcher-selected cancellation identities. This is observation
/// provenance, not authority to retire a request or authorize another document.
#[derive(Clone)]
pub struct DispatcherCancellation {
    view: ViewId,
    renderer: RendererGeneration,
    renderer_cancel: CancelToken,
    external_loss: CancelToken,
    view_loss: CancelToken,
}

impl DispatcherCancellation {
    pub fn renderer(&self) -> &RendererGeneration { &self.renderer }
    pub fn external_loss(&self) -> CancelToken { self.external_loss.clone() }
    pub fn view_loss(&self) -> CancelToken { self.view_loss.clone() }
}

/// Observation-only loss set for an exact accepted PDF predecessor retirement.
/// It never grants successor authority or suppresses explicit cancellation.
#[derive(Clone)]
pub struct AcceptedOwnerLosses {
    signals: [CancelToken; 5],
    extra_cancellation: [Option<CancelToken>; 2],
    cancel: CancelToken,
    pending: CancelToken,
}

impl AcceptedOwnerLosses {
    pub fn tokens(&self) -> impl Iterator<Item = &CancelToken> {
        self.signals.iter().chain(self.extra_cancellation.iter().flatten())
    }
    pub fn cancel(&self) -> &CancelToken { &self.cancel }
    pub fn pending(&self) -> &CancelToken { &self.pending }
}

/// A native-only, request-scoped value captured at authenticated ingress.
#[derive(Clone)]
pub struct NativeContext {
    value: Arc<dyn Any + Send + Sync>,
    renderer: Option<RendererGeneration>,
    owner_loss: Option<CancelToken>,
    ingress_loss: Option<CancelToken>,
    dispatcher_cancellation: Option<DispatcherCancellation>,
    independent_native_loss: Option<CancelToken>,
    authority: Option<Arc<dyn Any + Send + Sync>>,
    request_authority: Option<Arc<dyn Any + Send + Sync>>,
    pdf_document: Option<PdfDocumentLease>,
    pdf_revision: Option<PdfDocumentLease>,
    document_lease: Option<RendererGeneration>,
    pdf_owner_loss: Option<Arc<PdfOwnerLoss>>,
}

impl NativeContext {
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        Self { value: Arc::new(value), renderer: None, owner_loss: None, ingress_loss: None, dispatcher_cancellation: None, independent_native_loss: None, authority: None, request_authority: None, pdf_document: None, pdf_revision: None, document_lease: None, pdf_owner_loss: None }
    }

    pub fn downcast_ref<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.value.downcast_ref()
    }

    pub fn as_arc(&self) -> Arc<dyn Any + Send + Sync> {
        self.value.clone()
    }

    pub fn with_renderer<T: Any + Send + Sync>(value: T, renderer: RendererGeneration) -> Self {
        Self { value: Arc::new(value), renderer: Some(renderer), owner_loss: None, ingress_loss: None, dispatcher_cancellation: None, independent_native_loss: None, authority: None, request_authority: None, pdf_document: None, pdf_revision: None, document_lease: None, pdf_owner_loss: None }
    }

    pub fn renderer(&self) -> Option<&RendererGeneration> {
        self.renderer.as_ref()
    }

    /// Attach the producer's event-driven combined owner-loss token.
    pub fn with_owner_loss<T: Any + Send + Sync>(
        value: T,
        renderer: RendererGeneration,
        owner_loss: CancelToken,
    ) -> Self {
        Self { value: Arc::new(value), renderer: Some(renderer), ingress_loss: Some(owner_loss.clone()), owner_loss: Some(owner_loss), dispatcher_cancellation: None, independent_native_loss: None, authority: None, request_authority: None, pdf_document: None, pdf_revision: None, document_lease: None, pdf_owner_loss: None }
    }


    /// Called by Dispatcher after it has selected the actual per-generation
    /// token and this view's independent destruction token. Legacy contexts
    /// without a renderer keep no provenance and cannot transfer cancellation.
    pub fn bind_dispatcher_cancellation(mut self, view: ViewId, renderer_cancel: CancelToken, view_loss: CancelToken) -> Self {
        self.dispatcher_cancellation = self.renderer.as_ref().map(|renderer| DispatcherCancellation {
            view, renderer: renderer.clone(), external_loss: renderer_cancel.external_loss_token(),
            renderer_cancel, view_loss,
        });
        self
    }

    /// The native producer supplies the actual independent view/asset-policy
    /// loss, not the context/renderer/document combined cancellation token.
    pub fn with_independent_native_loss(mut self, loss: CancelToken) -> Self {
        self.independent_native_loss = Some(loss);
        self
    }

    pub fn owner_loss(&self) -> Option<CancelToken> {
        self.owner_loss.clone()
    }

    /// Retain the native origin proof and its subscriptions for the owner's lifetime.
    pub fn with_authority<T: Any + Send + Sync>(mut self, authority: T) -> Self {
        self.authority = Some(Arc::new(authority));
        self
    }

    pub fn authority_ref<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.authority.as_ref()?.downcast_ref()
    }
    /// Attach an opaque capability created by authenticated native ingress.
    pub fn with_request_authority<T: Any + Send + Sync>(mut self, authority: Arc<T>) -> Self {
        self.request_authority = Some(authority);
        self
    }

    pub fn request_authority<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.request_authority.as_ref()?.clone().downcast::<T>().ok()
    }
    pub fn without_request_authority(mut self) -> Self {
        self.request_authority = None;
        self
    }

    /// Preserve request payload and non-media constraints when the native host
    /// has prepared a replacement for the exact same authenticated origin.
    pub fn with_media_successor_base(&self, successor: &NativeContext) -> Option<Self> {
        if !self.renderer.as_ref()?.same_generation(successor.renderer.as_ref()?) {
            return None;
        }
        let ingress = successor.owner_loss.clone()?;
        let mut context = self.clone();
        context.authority = successor.authority.clone();
        context.owner_loss = Some(ingress.clone());
        context.ingress_loss = Some(ingress);
        context.pdf_owner_loss = None;
        if let Some(loss) = context.independent_native_loss.clone() { context.attach_owner_loss(&loss); }
        if let Some(document) = context.pdf_document.clone() { context.attach_owner_loss(&document.loss_token()); }
        if let Some(revision) = context.pdf_revision.clone() { context.attach_owner_loss(&revision.loss_token()); }
        if let Some(document) = context.document_lease.clone() { context.attach_owner_loss(&document.loss_token()); }
        Some(context)
    }
    /// Synthetic Docs fixtures model the media permit as their document lease.
    pub(crate) fn with_synthetic_media_successor(&self, successor: &NativeContext) -> Option<Self> {
        if !self.renderer.as_ref()?.same_generation(successor.renderer.as_ref()?) {
            return None;
        }
        let ingress = successor.owner_loss.clone()?;
        let mut context = self.clone();
        context.value = successor.value.clone();
        context.authority = successor.authority.clone();
        context.document_lease = successor.document_lease.clone();
        context.owner_loss = Some(ingress.clone());
        context.ingress_loss = Some(ingress);
        context.pdf_owner_loss = None;
        if let Some(loss) = context.independent_native_loss.clone() { context.attach_owner_loss(&loss); }
        if let Some(document) = context.pdf_document.clone() { context.attach_owner_loss(&document.loss_token()); }
        if let Some(revision) = context.pdf_revision.clone() { context.attach_owner_loss(&revision.loss_token()); }
        if let Some(document) = context.document_lease.clone() { context.attach_owner_loss(&document.loss_token()); }
        Some(context)
    }

    /// Attach the lease captured by the PDF document actor, not a view lookup.
    pub fn with_pdf_document(mut self, lease: PdfDocumentLease) -> Self {
        self.attach_owner_loss(&lease.loss_token());
        self.pdf_document = Some(lease);
        self
    }

    /// Attach the revision captured for this operation, not a current lookup.
    pub fn with_pdf_revision(mut self, lease: PdfDocumentLease) -> Self {
        self.attach_owner_loss(&lease.loss_token());
        self.pdf_revision = Some(lease);
        self
    }

    fn attach_owner_loss(&mut self, lease_loss: &CancelToken) {
        if let Some(ingress_loss) = self.owner_loss.take() {
            let combined = CancelToken::default();
            let notify = combined.clone();
            let ingress = ingress_loss.on_cancel(move || notify.cancel());
            let notify = combined.clone();
            let document = lease_loss.on_cancel(move || notify.cancel());
            self.pdf_owner_loss = Some(Arc::new(PdfOwnerLoss {
                _previous: self.pdf_owner_loss.take(),
                _ingress: ingress,
                _document: document,
            }));
            self.owner_loss = Some(combined);
        }
    }

    pub fn pdf_document(&self) -> Option<&PdfDocumentLease> {
        self.pdf_document.as_ref()
    }

    pub fn pdf_revision(&self) -> Option<&PdfDocumentLease> {
        self.pdf_revision.as_ref()
    }

    pub fn with_document_lease(mut self, lease: RendererGeneration) -> Self {
        self.attach_owner_loss(&lease.loss_token());
        self.document_lease = Some(lease);
        self
    }

    pub fn document_lease(&self) -> Option<&RendererGeneration> {
        self.document_lease.as_ref()
    }
}

impl fmt::Debug for NativeContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeContext([redacted])")
    }
}

/// A successor is staged without holding admission gates. Only a terminal
/// retirement capability can expose its native owner after retiring its source.
pub struct PreparedDocumentOwner {
    pub(crate) source_document: RendererGeneration,
    pub(crate) owner: NativeRequestOwner,
}

pub struct PreparedPdfRevisionOwner {
    source: PdfDocumentLease,
    source_owner: NativeRequestOwner,
    owner: NativeRequestOwner,
}

/// The native lifetime belonging to the authenticated sender of a request.
#[derive(Clone, Debug)]
pub struct NativeRequestOwner {
    pub view: ViewId,
    pub context: NativeContext,
}

impl NativeRequestOwner {
    /// Exact-token matching is mandatory before an accepted predecessor marker
    /// can distinguish dispatcher renderer retirement from external cancellation.
    pub fn dispatcher_cancellation_for(&self, cancel: &CancelToken) -> Option<&DispatcherCancellation> {
        self.dispatcher_cancellation_provenance()
            .filter(|provenance| provenance.renderer_cancel.same_token(cancel))
    }

    pub fn dispatcher_cancellation_provenance(&self) -> Option<&DispatcherCancellation> {
        let provenance = self.context.dispatcher_cancellation.as_ref()?;
        (provenance.view == self.view
            && self.context.renderer().is_some_and(|renderer| renderer.same_generation(&provenance.renderer)))
            .then_some(provenance)
    }

    /// Call only at the actual accepted-marker handoff, before intentional old
    /// context retirement. Consumers replace only that predecessor's observers;
    /// their external pending token and every other owner's loss stay unchanged.
    pub fn accepted_reload_losses(&self, accepted: &crate::hosts::AcceptedPdfReload, cancel: &CancelToken, pending: &CancelToken) -> Result<AcceptedOwnerLosses, String> {
        if !accepted.retires(self) { return Err("The accepted reload does not retire this captured owner.".into()); }
        let provenance = self.dispatcher_cancellation_provenance()
            .ok_or("Dispatcher cancellation provenance is unavailable.")?;
        let independent = self.context.independent_native_loss.clone()
            .ok_or("Independent native owner loss is unavailable.")?;
        let external = provenance.external_loss();
        let cancel = if provenance.renderer_cancel.same_token(cancel) { external.clone() } else { cancel.clone() };
        let pending = if provenance.renderer_cancel.same_token(pending) { external.clone() } else { pending.clone() };
        let extra_cancellation = [
            (!cancel.same_token(&external)).then(|| cancel.clone()),
            (!pending.same_token(&external) && !pending.same_token(&cancel)).then(|| pending.clone()),
        ];
        Ok(AcceptedOwnerLosses {
            signals: [independent, provenance.view_loss(), external,
                accepted.browser_loss().clone(), accepted.reservation_loss().clone()],
            extra_cancellation, cancel, pending,
        })
    }

    /// Allocate and subscribe before entering the terminal transition. The
    /// original native/asset/media/PDF authority is retained, never looked up.
    pub fn prepare_document_successor(&self) -> Result<PreparedDocumentOwner, String> {
        let source_document = self.context.document_lease.clone()
            .ok_or("The captured document lifetime is unavailable.")?;
        let ingress = self.context.ingress_loss.clone()
            .ok_or("The captured native lifetime is unavailable.")?;
        let mut context = self.context.clone();
        context.owner_loss = Some(ingress);
        context.pdf_owner_loss = None;
        context.document_lease = None;
        if let Some(document) = context.pdf_document.clone() {
            context.attach_owner_loss(&document.loss_token());
        }
        if let Some(revision) = context.pdf_revision.clone() {
            context.attach_owner_loss(&revision.loss_token());
        }
        context = context.with_document_lease(RendererGeneration::new());
        Ok(PreparedDocumentOwner {
            source_document, owner: Self { view: self.view, context },
        })
    }

    /// Stage the same captured owner with a new PDF revision, retaining the
    /// original ingress and every non-revision lifetime.
    pub fn prepare_pdf_revision_successor(&self, successor: PdfDocumentLease) -> Result<PreparedPdfRevisionOwner, String> {
        let source = self.context.pdf_revision.clone().ok_or("The captured PDF revision is unavailable.")?;
        if source.same_document(&successor) { return Err("The PDF successor must be a new revision.".into()); }
        let ingress = self.context.ingress_loss.clone().ok_or("The captured native lifetime is unavailable.")?;
        let mut context = self.context.clone();
        context.owner_loss = Some(ingress);
        context.pdf_owner_loss = None;
        context.pdf_revision = None;
        if let Some(document) = context.pdf_document.clone() { context.attach_owner_loss(&document.loss_token()); }
        if let Some(document) = context.document_lease.clone() { context.attach_owner_loss(&document.loss_token()); }
        context = context.with_pdf_revision(successor);
        Ok(PreparedPdfRevisionOwner { source, source_owner: self.clone(), owner: Self { view: self.view, context } })
    }

    /// None means the originating native lifetime is not observable.
    pub fn loss_token(&self) -> Option<CancelToken> {
        self.context.owner_loss()
    }
    /// Borrow the same captured lifetime for a preallocated admission plan.
    pub fn loss_token_ref(&self) -> Option<&CancelToken> {
        self.context.owner_loss.as_ref()
    }

    pub fn pdf_document(&self) -> Option<&PdfDocumentLease> {
        self.context.pdf_document()
    }

    pub fn pdf_revision(&self) -> Option<&PdfDocumentLease> {
        self.context.pdf_revision()
    }
}

/// What a handler gets besides its arguments
#[derive(Clone, Debug)]
pub struct CallContext {
    pub sender: Sender,
    /// set for `invoke`, none for `send` and `sendSync`
    pub request: Option<RequestId>,
    /// fires when the sender's view is destroyed or navigates away, as
    /// `abort-on-destroyed` did; the renderer cannot cancel an invoke
    pub cancel: CancelToken,
    /// Captured by the native ingress; absent for non-native dispatchers.
    pub native: Option<NativeContext>,
}

/// A message from a renderer, after the bridge decoded it
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub kind: ChannelKind,
    pub channel: String,
    pub args: Vec<IpcValue>,
}

/// A handler's answer; for `send` an `Err` is logged as Electron logged an
/// uncaught exception in an `ipcMain.on` listener
pub type Reply = Result<IpcValue, IpcError>;

/// Answers one message, once, from any thread. A handler that awaited in
/// TypeScript (a dialog, the network, a child process) keeps the responder,
/// returns, and answers when its work ends. Dropping it unanswered is a bug;
/// the shell then rejects the invoke so the renderer does not hang.
pub struct Responder(Box<dyn FnOnce(Reply) + Send>);

impl Responder {
    pub fn new(answer: impl FnOnce(Reply) + Send + 'static) -> Self {
        Responder(Box::new(answer))
    }

    pub fn reply(self, reply: Reply) {
        (self.0)(reply)
    }
}

/// One channel's handler: `ipcMain.handle` and `ipcMain.on` in one shape.
/// Runs on the core thread, as every `ipcMain` listener ran on Electron's
/// main thread: in arrival order, one at a time, so `main-core`'s state
/// (`Rc`, `RefCell`) needs no lock. Not `Send`: it never leaves that thread.
pub trait Handler {
    fn call(&self, cx: &CallContext, args: Vec<IpcValue>, reply: Responder);
}

/// Where a main-side event goes
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// `webContents.send` and `event.sender.send`
    View(ViewId),
    /// every view of one window
    Window(WindowId),
    /// every view that loaded this preload
    Surface(Surface),
    /// every view, as the broadcast loops over `BrowserWindow.getAllWindows()`
    All,
}

/// Emits main-side events. Callable from any thread; for one view, events
/// and replies arrive in the order they were emitted, so a stream's chunks
/// are delivered before the reply of the invoke that produced them.
pub trait EventSink: Send + Sync {
    fn emit(&self, target: Target, channel: &'static str, args: Vec<IpcValue>);
    /// Deliver only to the captured renderer lifetime, or report failure.
    fn emit_to_owner_checked(&self, _owner: &NativeRequestOwner, _channel: &'static str, _args: Vec<IpcValue>) -> Result<(), String> {
        Err("generation-scoped event delivery is unavailable".into())
    }
}

/// Why a registration was refused at start; the shell does not start then
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// not in [`crate::CHANNELS`]: regenerate the table or fix the name
    Unknown(&'static str),
    /// two handlers for one channel
    Twice(&'static str),
    /// the channel belongs to another batch: file ownership is per batch
    WrongBatch {
        channel: &'static str,
        owner: Batch,
        by: Batch,
    },
    /// the registration's kind is not how renderers use the channel
    WrongKind {
        channel: &'static str,
        kind: ChannelKind,
    },
    /// the shell was built without a host the batch needs ([`crate::hosts`])
    MissingHost { batch: Batch, host: &'static str },
    /// a provided host could not establish its startup authority
    Initialization {
        batch: Batch,
        host: &'static str,
        message: String,
    },
}

/// The channel table the shell fills at start on the core thread, one batch
/// after another. After all batches, every spec of kind invoke, send or
/// sendSync must have a handler, and the shell refuses a message from a
/// surface the spec does not list (the capability check Electron did not have).
pub trait Registry {
    fn register(
        &mut self,
        by: Batch,
        channel: &'static str,
        kind: ChannelKind,
        handler: Rc<dyn Handler>,
    ) -> Result<(), RegisterError>;

    /// What the batches provide to each other ([`crate::provided`]): add to
    /// it while registering, keep the `Rc` and read it when a handler runs
    fn provided(&self) -> Rc<Provided>;
}

/// A batch's entry point: `crates/shell/src/ipc/<batch>/mod.rs` exports one
pub type RegisterBatch = fn(&mut dyn Registry, &Services) -> Result<(), RegisterError>;

#[cfg(test)]
mod native_context_tests {
    use super::*;

    #[test]
    fn native_context_clones_capability_without_debugging_its_contents() {
        let generation = RendererGeneration::new();
        let context = NativeContext::with_renderer(String::from("sensitive"), generation.clone());
        let copy = context.clone();
        assert_eq!(copy.downcast_ref::<String>().map(String::as_str), Some("sensitive"));
        assert!(copy.renderer().is_some_and(|token| token.same_generation(&generation)));
        assert!(!format!("{copy:?}").contains("sensitive"));
        generation.revoke();
        assert!(!copy.renderer().unwrap().is_active());
    }

    #[test]
    fn cloned_context_retains_authority_and_observes_owner_loss() {
        let loss = CancelToken::default();
        let authority = Arc::new(());
        let weak_authority = Arc::downgrade(&authority);
        let context = NativeContext::with_owner_loss(7_u32, RendererGeneration::new(), loss.clone())
            .with_authority(authority);
        let copy = context.clone();
        drop(context);

        assert!(weak_authority.upgrade().is_some());
        assert_eq!(copy.downcast_ref::<u32>(), Some(&7));
        assert!(!copy.owner_loss().unwrap().is_cancelled());
        loss.cancel();
        assert!(copy.owner_loss().unwrap().is_cancelled());
        drop(copy);
        assert!(weak_authority.upgrade().is_none());
    }
    #[test]
    fn media_successor_preserves_request_and_nonmedia_lifetimes() {
        let renderer = RendererGeneration::new();
        let old_media = CancelToken::new();
        let new_media = CancelToken::new();
        let independent = CancelToken::new();
        let cancel = CancelToken::new();
        let document = RendererGeneration::new();
        let request = Arc::new(47_u32);
        let original = NativeContext::with_owner_loss("request", renderer.clone(), old_media.clone())
            .with_independent_native_loss(independent.clone())
            .with_document_lease(document.clone())
            .with_request_authority(request.clone())
            .bind_dispatcher_cancellation(ViewId(7), cancel.clone(), CancelToken::new());
        let base = NativeContext::with_owner_loss((), renderer, new_media);
        let successor = original.with_media_successor_base(&base).unwrap();
        assert_eq!(successor.downcast_ref::<&str>(), Some(&"request"));
        assert!(Arc::ptr_eq(&successor.request_authority::<u32>().unwrap(), &request));
        assert!(NativeRequestOwner { view: ViewId(7), context: successor.clone() }.dispatcher_cancellation_for(&cancel).is_some());
        assert!(successor.document_lease().unwrap().same_generation(&document));
        old_media.cancel();
        assert!(original.owner_loss().unwrap().is_cancelled());
        assert!(!successor.owner_loss().unwrap().is_cancelled());
        independent.cancel();
        assert!(successor.owner_loss().unwrap().is_cancelled());
    }


    #[test]
    fn pdf_document_lease_replacement_and_revocation_fence_commit() {
        let first = PdfDocumentLease::new();
        let captured = first.clone();
        let replacement = PdfDocumentLease::new();
        assert!(first.same_document(&captured));
        assert!(!first.same_document(&replacement));
        assert_eq!(captured.with_live(|| 17), Some(17));
        let loss = captured.loss_token();
        let notify = first.revoke_deferred();
        assert!(!loss.is_cancelled());
        assert_eq!(captured.with_live(|| 18), None);
        notify.cancel();
        assert!(loss.is_cancelled());
        assert_eq!(replacement.with_live(|| 19), Some(19));
    }

    #[test]
    fn pdf_owner_loss_includes_ingress_and_document_lifetimes() {
        let ingress = CancelToken::default();
        let document = PdfDocumentLease::new();
        let owner = NativeContext::with_owner_loss((), RendererGeneration::new(), ingress.clone())
            .with_pdf_document(document.clone());
        let copy = owner.clone();
        drop(owner);
        document.revoke_deferred().cancel();
        assert!(copy.owner_loss().unwrap().is_cancelled());

        let other = NativeContext::with_owner_loss((), RendererGeneration::new(), ingress.clone())
            .with_pdf_document(PdfDocumentLease::new());
        ingress.cancel();
        assert!(other.owner_loss().unwrap().is_cancelled());

        let unverified = NativeContext::new(()).with_pdf_document(PdfDocumentLease::new());
        assert!(unverified.owner_loss().is_none());
    }

    #[test]
    fn pdf_revision_loss_cancels_only_the_captured_operation() {
        let slot = PdfDocumentLease::new();
        let revision = PdfDocumentLease::new();
        let inspection = NativeContext::with_owner_loss((), RendererGeneration::new(), CancelToken::default())
            .with_pdf_document(slot.clone());
        let operation = inspection.clone().with_pdf_revision(revision.clone());
        assert!(operation.pdf_document().unwrap().same_document(inspection.pdf_document().unwrap()));
        revision.revoke_deferred().cancel();
        assert!(operation.owner_loss().unwrap().is_cancelled());
        assert!(!inspection.owner_loss().unwrap().is_cancelled());
        assert!(slot.is_live());
    }

    #[test]
    fn pdf_revision_successor_keeps_ingress_and_slot_without_old_revision_loss() {
        let ingress = CancelToken::default();
        let slot = PdfDocumentLease::new();
        let old = PdfDocumentLease::new();
        let new = PdfDocumentLease::new();
        let owner = NativeRequestOwner { view: ViewId(7), context: NativeContext::with_owner_loss((), RendererGeneration::new(), ingress.clone())
            .with_pdf_document(slot.clone()).with_pdf_revision(old.clone()) };
        let mut prepared = Some(owner.prepare_pdf_revision_successor(new.clone()).unwrap());
        let mut gate = PdfDocumentGatePlan::new(&[&slot, &old, &new]);
        let mut retirement = gate.prepare_retirement(&owner, &old, &new).unwrap();
        let successor = gate.with_live_retiring(&mut retirement, |admitted| admitted.commit(&mut prepared)).unwrap().unwrap();
        assert!(!old.is_live());
        assert!(!owner.loss_token().unwrap().is_cancelled());
        drop(retirement);
        assert!(owner.loss_token().unwrap().is_cancelled());
        assert!(!successor.loss_token().unwrap().is_cancelled());
        assert!(successor.pdf_document().unwrap().same_document(&slot));
        assert!(successor.pdf_revision().unwrap().same_document(&new));
        ingress.cancel();
        assert!(successor.loss_token().unwrap().is_cancelled());
    }

    #[test]
    fn pdf_revision_successor_rejects_a_different_captured_owner_without_retiring() {
        let old = PdfDocumentLease::new();
        let new = PdfDocumentLease::new();
        let owner = NativeRequestOwner { view: ViewId(7), context: NativeContext::with_owner_loss((), RendererGeneration::new(), CancelToken::default())
            .with_pdf_revision(old.clone()) };
        let other = NativeRequestOwner { view: ViewId(8), context: NativeContext::with_owner_loss((), RendererGeneration::new(), CancelToken::default())
            .with_pdf_revision(old.clone()) };
        let mut prepared = Some(other.prepare_pdf_revision_successor(new.clone()).unwrap());
        let mut gate = PdfDocumentGatePlan::new(&[&old, &new]);
        let mut retirement = gate.prepare_retirement(&owner, &old, &new).unwrap();
        assert!(gate.with_live_retiring(&mut retirement, |admitted| admitted.commit(&mut prepared)).unwrap().is_err());
        assert!(old.is_live());
        assert!(prepared.is_some());
    }

    #[test]
    fn pdf_revision_renews_independently_captured_admitted_owners_atomically() {
        let old = PdfDocumentLease::new();
        let new = PdfDocumentLease::new();
        let ingress = CancelToken::default();
        let renderer = RendererGeneration::new();
        let make_owner = || NativeRequestOwner { view: ViewId(7), context:
            NativeContext::with_owner_loss((), renderer.clone(), ingress.clone()).with_pdf_revision(old.clone()) };
        let first = make_owner();
        let second = make_owner();
        let mut staged = [Some(first.prepare_pdf_revision_successor(new.clone()).unwrap()),
            Some(second.prepare_pdf_revision_successor(new.clone()).unwrap())];
        let mut outputs = [None, None];
        let mut gate = PdfDocumentGatePlan::new(&[&old, &new]);
        let mut retirement = gate.prepare_retirement_many(&first, &[&second], &old, &new).unwrap();
        gate.with_live_retiring(&mut retirement, |admitted| admitted.commit_many(&mut staged, &mut outputs)).unwrap().unwrap();
        drop(retirement);
        assert!(first.loss_token().unwrap().is_cancelled());
        assert!(second.loss_token().unwrap().is_cancelled());
        assert!(outputs.iter().all(|owner| owner.as_ref().is_some_and(|owner| !owner.loss_token().unwrap().is_cancelled()
            && owner.pdf_revision().unwrap().same_document(&new))));
        ingress.cancel();
        assert!(outputs.iter().all(|owner| owner.as_ref().unwrap().loss_token().unwrap().is_cancelled()));
    }

    #[test]
    fn admitted_successor_retains_native_proof_but_not_retired_document_loss() {
        let ingress = CancelToken::default();
        let renderer = RendererGeneration::new();
        let document = RendererGeneration::new();
        let authority = Arc::new(17_u32);
        let owner = NativeRequestOwner { view: ViewId(7), context:
            NativeContext::with_owner_loss("native payload", renderer.clone(), ingress.clone())
                .with_authority(authority.clone()).with_document_lease(document.clone()),
        };
        let prepared = owner.prepare_document_successor().unwrap();
        let mut notifications = [None, None];
        let successor = RendererGeneration::with_active_all_retiring(
            [Some(&renderer), Some(&document)], [Some(&document), None], &mut notifications,
            |retirement| {
                retirement.retire(&document).unwrap();
                retirement.renew_owner(prepared).unwrap()
            },
        ).unwrap();
        for notification in notifications.into_iter().flatten() { notification.cancel(); }
        assert!(owner.loss_token().unwrap().is_cancelled());
        assert!(!successor.loss_token().unwrap().is_cancelled());
        assert_eq!(successor.view, owner.view);
        assert!(successor.context.renderer().unwrap().same_generation(&renderer));
        assert!(Arc::ptr_eq(successor.context.authority_ref::<Arc<u32>>().unwrap(), &authority));
        assert!(!successor.context.document_lease().unwrap().same_generation(&document));
        ingress.cancel();
        assert!(successor.loss_token().unwrap().is_cancelled());
    }

    #[test]
    fn successor_is_not_exposed_without_retiring_its_captured_document() {
        let document = RendererGeneration::new();
        let owner = NativeRequestOwner { view: ViewId(7), context:
            NativeContext::with_owner_loss((), RendererGeneration::new(), CancelToken::default())
                .with_document_lease(document.clone()),
        };
        let prepared = owner.prepare_document_successor().unwrap();
        let mut notifications = [None, None];
        assert_eq!(RendererGeneration::with_active_all_retiring(
            [Some(&document)], [Some(&document), None], &mut notifications,
            |retirement| retirement.renew_owner(prepared).is_err(),
        ), Some(true));
        assert!(document.is_active());
        assert!(!owner.loss_token().unwrap().is_cancelled());
    }

    // Explicit synthetic identities exercise observation provenance, not CEF
    // authentication or permission to construct a production reload receipt.
    fn reload_loss_fixture() -> (NativeRequestOwner, crate::hosts::AcceptedPdfReload, CancelToken, CancelToken, CancelToken) {
        let renderer = RendererGeneration::new();
        let dispatch = CancelToken::default();
        let view_loss = CancelToken::default();
        let independent = CancelToken::default();
        let identity = Arc::new(37_u8);
        let owner = NativeRequestOwner { view: ViewId(17), context:
            NativeContext::with_owner_loss(identity.clone(), renderer, CancelToken::default())
                .with_independent_native_loss(independent.clone())
                .with_pdf_document(PdfDocumentLease::new())
                .with_pdf_revision(PdfDocumentLease::new())
                .bind_dispatcher_cancellation(ViewId(17), dispatch.clone(), view_loss.clone()),
        };
        let lineage = crate::hosts::AcceptedPdfReload::from_native_admission(
            owner.clone(), RendererGeneration::new(), CancelToken::default(), CancelToken::default(),
            Arc::new(move |candidate| candidate.context.downcast_ref::<Arc<u8>>()
                .is_some_and(|value| Arc::ptr_eq(value, &identity))),
        );
        (owner, lineage, dispatch, independent, view_loss)
    }

    #[test]
    fn accepted_reload_retains_independent_loss_and_later_explicit_cancel() {
        let (owner, lineage, dispatch, independent, view_loss) = reload_loss_fixture();
        let pending = CancelToken::default();
        let bundle = owner.accepted_reload_losses(&lineage, &dispatch, &pending).unwrap();
        assert!(bundle.pending().same_token(&pending));
        dispatch.cancel_renderer_retired();
        owner.pdf_revision().unwrap().revoke_deferred().cancel();
        assert!(owner.loss_token().unwrap().is_cancelled());
        assert!(!bundle.cancel().is_cancelled());
        assert!(bundle.tokens().all(|token| !token.is_cancelled()));
        dispatch.clone().cancel();
        assert!(bundle.cancel().is_cancelled());
        assert!(bundle.tokens().any(|token| token.same_token(&independent)));
        assert!(bundle.tokens().any(|token| token.same_token(&view_loss)));
        assert!(bundle.tokens().any(|token| token.same_token(lineage.browser_loss())));
        assert!(bundle.tokens().any(|token| token.same_token(lineage.reservation_loss())));
    }

    #[test]
    fn accepted_reload_does_not_rewrite_distinct_external_controls() {
        let (owner, lineage, _, _, _) = reload_loss_fixture();
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        cancel.cancel();
        let bundle = owner.accepted_reload_losses(&lineage, &cancel, &pending).unwrap();
        assert!(bundle.cancel().same_token(&cancel));
        assert!(bundle.pending().same_token(&pending));
        assert!(bundle.cancel().is_cancelled());
        pending.cancel();
        assert!(bundle.pending().is_cancelled());
    }

    #[test]
    fn accepted_reload_renderer_aliases_share_real_external_loss_not_a_fresh_token() {
        let (owner, lineage, dispatch, _, _) = reload_loss_fixture();
        let external = dispatch.external_loss_token();
        let bundle = owner.accepted_reload_losses(&lineage, &dispatch, &dispatch).unwrap();
        assert!(bundle.cancel().same_token(&external));
        assert!(bundle.pending().same_token(&external));
        dispatch.cancel_renderer_retired();
        assert!(!bundle.cancel().is_cancelled());
        dispatch.cancel();
        assert!(bundle.cancel().is_cancelled());
        assert!(bundle.pending().is_cancelled());
    }

    #[test]
    fn accepted_reload_fails_closed_without_dispatcher_or_independent_provenance() {
        let (owner, lineage, dispatch, independent, view_loss) = reload_loss_fixture();
        let unbound = NativeRequestOwner { view: owner.view, context:
            NativeContext::with_owner_loss(owner.context.downcast_ref::<Arc<u8>>().unwrap().clone(),
                owner.context.renderer().unwrap().clone(), CancelToken::default())
                .with_pdf_document(owner.pdf_document().unwrap().clone())
                .with_pdf_revision(owner.pdf_revision().unwrap().clone()),
        };
        assert!(lineage.retires(&unbound));
        let pending = CancelToken::default();
        assert_eq!(unbound.accepted_reload_losses(&lineage, &dispatch, &pending).err().as_deref(),
            Some("Dispatcher cancellation provenance is unavailable."));
        let without_asset = NativeRequestOwner { view: owner.view, context: unbound.context.clone()
            .bind_dispatcher_cancellation(owner.view, dispatch.clone(), view_loss) };
        assert_eq!(without_asset.accepted_reload_losses(&lineage, &dispatch, &pending).err().as_deref(),
            Some("Independent native owner loss is unavailable."));
        let complete = NativeRequestOwner { view: owner.view, context: without_asset.context
            .with_independent_native_loss(independent) };
        assert!(complete.accepted_reload_losses(&lineage, &dispatch, &pending).is_ok());
    }
}
