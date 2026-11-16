use crate::{ipc::NativeRequestOwner, runtime::CancelToken};
use std::sync::{Arc, Mutex};

/// Returned only after the captured PDF was replaced and its reserved native
/// document both bound and completed loading. It is not a reload admission.
pub struct CommittedPdfReload {
    successor: NativeRequestOwner,
    lineage: AcceptedPdfReload,
}

impl CommittedPdfReload {
    /// Native implementations call this only with their actual completed-load
    /// receipt's owner after acknowledging the staged PDF slot and revision.
    pub fn from_completed_native_load(
        successor: NativeRequestOwner,
        lineage: AcceptedPdfReload,
    ) -> Result<Self, String> {
        if !successor.context.renderer().is_some_and(|renderer| {
            renderer.is_active() && renderer.same_generation(lineage.reserved_renderer())
        }) || successor.pdf_document().is_none_or(|slot| !slot.is_live())
            || successor
                .pdf_revision()
                .is_none_or(|revision| !revision.is_live())
            || successor
                .loss_token()
                .is_none_or(|loss| loss.is_cancelled())
        {
            return Err("The reloaded PDF document is no longer available.".into());
        }
        Ok(Self { successor, lineage })
    }

    pub fn owner(&self) -> &NativeRequestOwner {
        &self.successor
    }
    pub fn activation_origin(&self, original: &NativeRequestOwner) -> NativeRequestOwner {
        if self.lineage.retires(original) {
            self.successor.clone()
        } else {
            original.clone()
        }
    }
    pub fn activation_destination(&self, original: &NativeRequestOwner) -> NativeRequestOwner {
        self.activation_origin(original)
    }
    pub fn accepted_losses(
        &self,
        original: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
    ) -> Result<Option<crate::ipc::AcceptedOwnerLosses>, String> {
        if self.lineage.retires(original) {
            original
                .accepted_reload_losses(&self.lineage, cancel, pending)
                .map(Some)
        } else {
            Ok(None)
        }
    }
}

/// A native-save reload is accepted only while the original destination and
/// every captured participant are jointly admitted. This is not a completion.
pub struct NativeSaveReloadHandoff {
    input: crate::ipc::NativeSavedInput,
    path: crate::ipc::NativeSavedPath,
    lineage: AcceptedPdfReload,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    completion: Mutex<ReloadCompletion>,
}

struct ReloadCompletion {
    owner: Option<NativeRequestOwner>,
    issued: bool,
}
impl ReloadCompletion {
    fn new() -> Self {
        Self {
            owner: None,
            issued: false,
        }
    }
}

/// Evidence of the actual completed successor corresponding to that handoff.
pub struct NativeSaveReloadReceipt {
    input: crate::ipc::NativeSavedInput,
    path: crate::ipc::NativeSavedPath,
    successor: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    lineage: AcceptedPdfReload,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
}

/// One-use evidence minted only by the saved-reload acceptance wrapper while
/// its exact input, path and owners are jointly admitted.
pub struct AdmittedSavedReload<'a> {
    input: &'a crate::ipc::NativeSavedInput,
    path: &'a crate::ipc::NativeSavedPath,
    used: bool,
}
impl<'a> AdmittedSavedReload<'a> {
    pub(crate) fn new(
        input: &'a crate::ipc::NativeSavedInput,
        path: &'a crate::ipc::NativeSavedPath,
    ) -> Self {
        Self {
            input,
            path,
            used: false,
        }
    }
    pub fn consume(
        &mut self,
        input: &crate::ipc::NativeSavedInput,
        path: &crate::ipc::NativeSavedPath,
    ) -> Result<(), String> {
        if self.used || !self.input.same_input(input) || !self.path.same_witness(path) {
            return Err("The PDF reload acceptance does not match its prepared input.".into());
        }
        self.used = true;
        Ok(())
    }
}

/// Preallocated data for an acceptance. It grants no reload authority until
/// consumed by the canonical saved-reload admission.
pub struct PreparedAcceptedPdfReload {
    input: crate::ipc::NativeSavedInput,
    path: crate::ipc::NativeSavedPath,
    marker: Option<AcceptedPdfReload>,
    handoff: Option<Arc<NativeSaveReloadHandoff>>,
}
impl PreparedAcceptedPdfReload {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        input: crate::ipc::NativeSavedInput,
        path: crate::ipc::NativeSavedPath,
        predecessor: NativeRequestOwner,
        reserved: crate::runtime::RendererGeneration,
        browser_loss: CancelToken,
        reservation_loss: CancelToken,
        same_native_context: Arc<dyn Fn(&NativeRequestOwner) -> bool + Send + Sync>,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
    ) -> Result<Self, String> {
        if !path.matches_publication(input.output()) {
            return Err("The PDF reload input does not match its saved publication.".into());
        }
        let marker = AcceptedPdfReload {
            identity: Arc::new(()),
            predecessor,
            reserved,
            browser_loss,
            reservation_loss,
            same_native_context,
        };
        if !marker.retires(input.output().source_owner())
            && !participants.iter().any(|owner| marker.retires(owner))
        {
            return Err("The accepted reload does not retire a captured participant.".into());
        }
        let handoff = Arc::new(NativeSaveReloadHandoff {
            input: input.clone(),
            path: path.clone(),
            lineage: marker.clone(),
            participants,
            cancel,
            pending,
            additional,
            completion: Mutex::new(ReloadCompletion::new()),
        });
        Ok(Self {
            input,
            path,
            marker: Some(marker),
            handoff: Some(handoff),
        })
    }
    pub fn commit(
        &mut self,
        permit: &mut AdmittedSavedReload<'_>,
    ) -> Result<(AcceptedPdfReload, Arc<NativeSaveReloadHandoff>), String> {
        if self.marker.is_none() || self.handoff.is_none() {
            return Err("The prepared PDF reload was already consumed.".into());
        }
        permit.consume(&self.input, &self.path)?;
        Ok((
            self.marker.take().expect("checked marker"),
            self.handoff.take().expect("checked handoff"),
        ))
    }
}

impl NativeSaveReloadHandoff {
    /// The native producer calls this inside the original destination admission,
    /// at the same point as the real reserved reload is accepted.
    pub fn from_native_admission(
        input: crate::ipc::NativeSavedInput,
        path: crate::ipc::NativeSavedPath,
        lineage: AcceptedPdfReload,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
    ) -> Result<Self, String> {
        if !path.matches_publication(input.output()) {
            return Err("The PDF reload input does not match its saved publication.".into());
        }
        if !lineage.retires(input.output().source_owner())
            && !participants.iter().any(|owner| lineage.retires(owner))
        {
            return Err("The accepted reload does not retire a captured participant.".into());
        }
        Ok(Self {
            input,
            path,
            lineage,
            participants,
            cancel,
            pending,
            additional,
            completion: Mutex::new(ReloadCompletion::new()),
        })
    }
    /// Prepare the exact loaded native owner and retained non-retired participants
    /// for the actor's short completion transition. No actor state changes here.
    pub fn completion_participants(
        &self,
        successor: &NativeRequestOwner,
    ) -> Result<Vec<NativeRequestOwner>, String> {
        if successor.view != self.lineage.predecessor.view
            || !successor
                .context
                .renderer()
                .is_some_and(|renderer| renderer.same_generation(self.lineage.reserved_renderer()))
        {
            return Err("The loaded native owner does not match the accepted reservation.".into());
        }
        let mut participants: Vec<_> = self
            .participants
            .iter()
            .map(|owner| {
                if self.lineage.retires(owner) {
                    successor.clone()
                } else {
                    owner.clone()
                }
            })
            .collect();
        if !self.lineage.retires(self.input.output().source_owner()) {
            participants.push(self.input.output().source_owner().clone());
        }
        Ok(participants)
    }
    pub fn input(&self) -> &crate::ipc::NativeSavedInput {
        &self.input
    }
    pub fn path(&self) -> &crate::ipc::NativeSavedPath {
        &self.path
    }
    pub fn publication(&self) -> &crate::ipc::SavedPublication {
        self.input.output()
    }
    pub fn predecessor(&self) -> &NativeRequestOwner {
        &self.lineage.predecessor
    }
    pub fn accepted_lineage(&self) -> &AcceptedPdfReload {
        &self.lineage
    }
    /// True only for an owner of the exact admitted predecessor lineage.
    pub fn retires(&self, owner: &NativeRequestOwner) -> bool {
        self.lineage.retires(owner)
    }
    pub fn cancel(&self) -> &CancelToken {
        &self.cancel
    }
    pub fn pending(&self) -> &CancelToken {
        &self.pending
    }
    pub fn additional(&self) -> &[CancelToken] {
        &self.additional
    }
    pub fn browser_loss(&self) -> &CancelToken {
        self.lineage.browser_loss()
    }
    pub fn reservation_loss(&self) -> &CancelToken {
        self.lineage.reservation_loss()
    }
    /// Snapshot the owner admitted by the exact actor transition, if any.
    pub fn completed_owner(&self) -> Option<NativeRequestOwner> {
        let state = self.completion.lock().ok()?;
        if state.issued {
            None
        } else {
            state.owner.clone()
        }
    }
    /// Called inside the combined parser admission; the effect must be short and state-only.
    pub fn with_completed_owner(
        &self,
        expected: &NativeRequestOwner,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let state = self
            .completion
            .lock()
            .map_err(|_| "The reload completion state is unavailable.")?;
        if state.issued {
            return Err("The PDF reload was already completed.".into());
        }
        let actual = state
            .owner
            .as_ref()
            .ok_or("The PDF reload successor is not admitted.")?;
        if actual.view != expected.view
            || !Arc::ptr_eq(&actual.context.as_arc(), &expected.context.as_arc())
            || !actual
                .loss_token()
                .zip(expected.loss_token())
                .is_some_and(|(a, b)| a.same_token(&b))
        {
            return Err("The PDF reload successor changed.".into());
        }
        effect()
    }
    /// Called by the canonical completion gate after the actor swap and exact
    /// predecessor retirement, never by an acceptance or a load callback alone.
    pub fn record_completion_admitted(
        &self,
        loaded: &NativeRequestOwner,
        retirement: &crate::ipc::AdmittedPdfReloadRetirement<'_, '_>,
    ) -> Result<(), String> {
        if !retirement.is_retired()
            || !retirement.matches_predecessor(self.predecessor())
            || loaded.view != self.lineage.predecessor.view
            || !loaded
                .context
                .renderer()
                .is_some_and(|renderer| renderer.same_generation(self.lineage.reserved_renderer()))
        {
            return Err("The admitted reload completion does not match its predecessor.".into());
        }
        let mut state = self
            .completion
            .lock()
            .map_err(|_| "The reload completion state is unavailable.")?;
        if state.issued || state.owner.is_some() {
            return Err("The native reload completion was already recorded.".into());
        }
        state.owner = Some(loaded.clone());
        Ok(())
    }

    /// Requires the actual completed-load receipt for this exact acceptance.
    pub fn complete(
        &self,
        completed: CommittedPdfReload,
    ) -> Result<NativeSaveReloadReceipt, String> {
        if !self.lineage.same_lineage(&completed.lineage) {
            return Err("The completed reload belongs to a different acceptance.".into());
        }
        let participants = self.completion_participants(&completed.successor)?;
        let mut admitted = self
            .completion
            .lock()
            .map_err(|_| "The reload completion state is unavailable.")?;
        if admitted.issued {
            return Err("The PDF reload was already completed.".into());
        }
        let actual = admitted
            .owner
            .as_ref()
            .ok_or("The native reload actor transition was not admitted.")?;
        if actual.view != completed.successor.view
            || !Arc::ptr_eq(
                &actual.context.as_arc(),
                &completed.successor.context.as_arc(),
            )
            || !actual
                .loss_token()
                .zip(completed.successor.loss_token())
                .is_some_and(|(a, b)| a.same_token(&b))
        {
            return Err("The completed reload is not the admitted native successor.".into());
        }
        admitted.issued = true;
        admitted.owner = None;
        Ok(NativeSaveReloadReceipt {
            input: self.input.clone(),
            path: self.path.clone(),
            successor: completed.successor,
            participants,
            lineage: self.lineage.clone(),
            cancel: self.cancel.clone(),
            pending: self.pending.clone(),
            additional: self.additional.clone(),
        })
    }
}

impl NativeSaveReloadReceipt {
    pub fn input(&self) -> &crate::ipc::NativeSavedInput {
        &self.input
    }
    pub fn path(&self) -> &crate::ipc::NativeSavedPath {
        &self.path
    }
    pub fn publication(&self) -> &crate::ipc::SavedPublication {
        self.input.output()
    }
    pub fn successor(&self) -> &NativeRequestOwner {
        &self.successor
    }
    pub fn accepted_lineage(&self) -> &AcceptedPdfReload {
        &self.lineage
    }
    /// Substitute only an owner proven to be the retired predecessor.
    pub fn owner_after_reload<'a>(
        &'a self,
        owner: &'a NativeRequestOwner,
    ) -> &'a NativeRequestOwner {
        if self.lineage.retires(owner) {
            &self.successor
        } else {
            owner
        }
    }
    pub fn retires(&self, owner: &NativeRequestOwner) -> bool {
        self.lineage.retires(owner)
    }
    pub fn participants(&self) -> &[NativeRequestOwner] {
        &self.participants
    }
    pub fn cancel(&self) -> &CancelToken {
        &self.cancel
    }
    pub fn pending(&self) -> &CancelToken {
        &self.pending
    }
    pub fn additional(&self) -> &[CancelToken] {
        &self.additional
    }
    pub fn browser_loss(&self) -> &CancelToken {
        self.lineage.browser_loss()
    }
    pub fn reservation_loss(&self) -> &CancelToken {
        self.lineage.reservation_loss()
    }
}

pub type PdfReloadDone = Box<dyn FnOnce(Result<CommittedPdfReload, String>)>;
pub trait GeneratedTerminalAck: Send + Sync {
    /// Called only after the original actor settles its original responder.
    fn complete(&self) -> Result<(), String>;
    /// Restore the exact unacknowledged result when its delivery is dropped.
    fn restore(&self, result: Result<GeneratedPathOutcome, String>);
}

pub struct GeneratedTerminalDelivery {
    result: Option<Result<GeneratedPathOutcome, String>>,
    ack: Arc<dyn GeneratedTerminalAck>,
}
impl GeneratedTerminalDelivery {
    pub fn new(
        result: Result<GeneratedPathOutcome, String>,
        ack: Arc<dyn GeneratedTerminalAck>,
    ) -> Self {
        Self {
            result: Some(result),
            ack,
        }
    }
    pub fn result(&self) -> &Result<GeneratedPathOutcome, String> {
        self.result
            .as_ref()
            .expect("a terminal delivery remains readable until acknowledged")
    }
    /// Only the actor that settled its original responder may acknowledge.
    pub fn acknowledge(&mut self) -> Result<(), String> {
        self.ack.complete()?;
        self.result.take();
        Ok(())
    }
}
impl Drop for GeneratedTerminalDelivery {
    fn drop(&mut self) {
        if let Some(result) = self.result.take() {
            self.ack.restore(result);
        }
    }
}

/// Initial dispatch either delivers a real terminal result or hands off the
/// still-owned operation. Pending is never an acknowledgement of completion.
pub enum GeneratedPathDispatchOutcome {
    Terminal(GeneratedTerminalDelivery),
    Pending(Arc<dyn GeneratedPathPending>),
}

pub type GeneratedPathObserver = Box<dyn FnOnce(GeneratedTerminalDelivery) + Send>;
pub type GeneratedPathWake = Arc<dyn Fn() + Send + Sync>;

/// Registration failed without installing or invoking the original observer.
pub struct GeneratedObserveFailure {
    pub error: String,
    pub observer: GeneratedPathObserver,
    pub wake: GeneratedPathWake,
}

/// A retained cleanup and reopen continuation for the original saved output.
/// Retrying never recreates the output, request owners, or accepted reload.
pub trait GeneratedPathPending: Send + Sync {
    /// The original terminal result is delivered once, including when it was
    /// settled before subscription. A second observer is rejected.
    fn observe(
        &self,
        done: GeneratedPathObserver,
        wake: GeneratedPathWake,
    ) -> Result<(), GeneratedObserveFailure>;
    /// Re-notify the same actor mailbox after delivery without replaying result.
    fn wake_observer(&self) -> Result<(), String>;
    /// Reacquire the exact restored delivery without cloning its result.
    fn take_redelivery(&self) -> Result<Option<GeneratedTerminalDelivery>, String>;
    /// Allows the owning actor to retire an unstarted preparation during quit.
    fn is_cancelling(&self) -> bool;
    /// Reports only this retry attempt; the observer owns the terminal result.
    fn retry(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>);
    /// On process shutdown, retry only the inverse cleanup and settle the
    /// original operation as cancelled; never resume opening a browser.
    fn cancel_and_drain(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>);
}

/// Core-thread-only prepared operation; no external effect runs before start.
pub trait PreparedGeneratedPath {
    fn task(&self) -> Arc<dyn GeneratedPathPending>;
    fn start(self: Box<Self>) -> Result<(), GeneratedStartFailure>;
}

pub struct GeneratedStartFailure {
    pub error: String,
    pub prepared: Box<dyn PreparedGeneratedPath>,
}
/// Process-owned retention, registered before the Drive transaction dispatches.
/// Dropping the registration does not declare cleanup complete.
pub trait GeneratedCleanupRegistration: Send {
    fn complete(self: Box<Self>);
}

pub enum GeneratedPathOutcome {
    NotOpened,
    Opened(Arc<CommittedGeneratedPath>),
    CleanupPending(Arc<dyn GeneratedPathPending>),
}

pub struct CommittedGeneratedPath {
    source: NativeRequestOwner,
    target: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: super::OwnedEffectsHost,
    native_destination: Option<crate::ipc::NativeSaveDestination>,
    saved_document: Option<crate::ipc::NativeSavedDocument>,
    native_reload: Option<Arc<NativeSaveReloadReceipt>>,
    reload: Option<CommittedPdfReload>,
    _accepted_losses: Option<crate::ipc::AcceptedOwnerLosses>,
}

impl CommittedGeneratedPath {
    /// The caller supplies only an actual loaded/bound target receipt's owner.
    pub fn from_loaded_target(
        source: NativeRequestOwner,
        target: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        effects: super::OwnedEffectsHost,
    ) -> Result<Self, String> {
        let participants = vec![target.clone(), destination];
        let receipt = Self {
            source,
            target,
            participants,
            cancel,
            pending,
            additional: Vec::new(),
            effects,
            native_destination: None,
            saved_document: None,
            native_reload: None,
            reload: None,
            _accepted_losses: None,
        };
        receipt.with_terminal_commit(&mut || Ok(()))?;
        Ok(receipt)
    }
    pub fn from_reloaded_target(
        source: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        effects: super::OwnedEffectsHost,
        reload: CommittedPdfReload,
    ) -> Result<Self, String> {
        let losses = reload.accepted_losses(&source, &cancel, &pending)?;
        let cancel = losses
            .as_ref()
            .map_or(cancel, |losses| losses.cancel().clone());
        let pending = losses
            .as_ref()
            .map_or(pending, |losses| losses.pending().clone());
        let participants = vec![
            reload.owner().clone(),
            reload.activation_destination(&destination),
        ];
        let receipt = Self {
            source,
            target: reload.owner().clone(),
            participants,
            cancel,
            pending,
            additional: Vec::new(),
            effects,
            native_destination: None,
            saved_document: None,
            native_reload: None,
            reload: Some(reload),
            _accepted_losses: losses,
        };
        receipt.with_terminal_commit(&mut || Ok(()))?;
        Ok(receipt)
    }

    /// Bind a completed native-save continuation to its original selected path
    /// and every captured participant. This does not mint a destination grant.
    pub fn from_existing_document_save(
        selection: crate::ipc::NativeSaveDestination,
        target: NativeRequestOwner,
        mut participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: super::OwnedEffectsHost,
    ) -> Result<Self, String> {
        let source = selection.binding().owner().clone();
        participants.push(target.clone());
        let receipt = Self {
            source,
            target,
            participants,
            cancel,
            pending,
            additional,
            effects,
            native_destination: Some(selection),
            saved_document: None,
            native_reload: None,
            reload: None,
            _accepted_losses: None,
        };
        receipt.with_terminal_commit(&mut || Ok(()))?;
        Ok(receipt)
    }

    /// Preserve the exact saved target and committed source through terminal admission.
    pub fn from_saved_document(
        document: crate::ipc::NativeSavedDocument,
        mut participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: super::OwnedEffectsHost,
    ) -> Result<Self, String> {
        let source = document.target().output().source_owner().clone();
        let target = document.owner().clone();
        participants.push(target.clone());
        let receipt = Self {
            source,
            target,
            participants,
            cancel,
            pending,
            additional,
            effects,
            native_destination: None,
            saved_document: Some(document),
            native_reload: None,
            reload: None,
            _accepted_losses: None,
        };
        receipt.with_terminal_commit(&mut || Ok(()))?;
        Ok(receipt)
    }

    /// Carries the actual completed native reload, not its earlier acceptance.
    pub fn from_native_save_reload(
        reload: Arc<NativeSaveReloadReceipt>,
        effects: super::OwnedEffectsHost,
    ) -> Result<Self, String> {
        let source = reload.publication().source_owner().clone();
        let target = reload.successor.clone();
        let receipt = Self {
            source,
            target,
            participants: reload.participants.clone(),
            cancel: reload.cancel.clone(),
            pending: reload.pending.clone(),
            additional: reload.additional.clone(),
            effects,
            native_destination: None,
            saved_document: None,
            native_reload: Some(reload),
            reload: None,
            _accepted_losses: None,
        };
        receipt.with_terminal_commit(&mut || Ok(()))?;
        Ok(receipt)
    }
    pub fn view(&self) -> crate::ViewId {
        self.target.view
    }
    pub fn request_controls(&self) -> (&CancelToken, &CancelToken) {
        (&self.cancel, &self.pending)
    }
    pub fn with_captured_participants(
        mut self,
        participants: &[NativeRequestOwner],
    ) -> Result<Self, String> {
        self.participants.reserve(participants.len());
        for participant in participants {
            let participant = self.native_reload.as_ref().map_or_else(
                || participant.clone(),
                |reload| reload.owner_after_reload(participant).clone(),
            );
            self.participants.push(self.reload.as_ref().map_or_else(
                || participant.clone(),
                |reload| reload.activation_origin(&participant),
            ));
        }
        self.with_terminal_commit(&mut || Ok(()))?;
        Ok(self)
    }
    pub fn with_captured_cancellations(
        mut self,
        additional: &[CancelToken],
    ) -> Result<Self, String> {
        self.additional.extend_from_slice(additional);
        self.with_terminal_commit(&mut || Ok(()))?;
        Ok(self)
    }
    pub fn with_terminal_commit(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if let Some(reload) = &self.native_reload {
            let additional: Vec<_> = self.additional.iter().collect();
            return self.effects.with_native_save_reload_many_effect_extra(
                reload,
                reload.successor(),
                &self.participants,
                &self.cancel,
                &self.pending,
                &additional,
                effect,
            );
        }
        if let Some(document) = &self.saved_document {
            let additional: Vec<_> = self.additional.iter().collect();
            return self.effects.with_native_saved_document_input(
                document,
                &self.participants,
                &self.cancel,
                &self.pending,
                &additional,
                &mut |_| effect(),
            );
        }
        if let Some(selection) = &self.native_destination {
            if self.additional.is_empty() {
                return self.effects.with_existing_document_destination_effect(
                    selection,
                    selection.path(),
                    &self.participants,
                    &self.cancel,
                    &self.pending,
                    &[],
                    effect,
                );
            }
            let additional: Vec<_> = self.additional.iter().collect();
            return self.effects.with_existing_document_destination_effect(
                selection,
                selection.path(),
                &self.participants,
                &self.cancel,
                &self.pending,
                &additional,
                effect,
            );
        }
        let source = self
            .reload
            .as_ref()
            .filter(|reload| reload.lineage.retires(&self.source))
            .map_or(&self.source, |reload| reload.owner());
        let additional: Vec<_> = self.additional.iter().collect();
        self.effects.with_owned_effect_many_extra(
            source,
            &self.participants,
            &self.cancel,
            &self.pending,
            &additional,
            effect,
        )
    }
}

/// An admitted predecessor-to-reserved-successor lineage, not successor authority.
/// Its loss signals come from the captured browser and native reload reservation.
#[derive(Clone)]
pub struct AcceptedPdfReload {
    identity: Arc<()>,
    predecessor: NativeRequestOwner,
    reserved: crate::runtime::RendererGeneration,
    browser_loss: CancelToken,
    reservation_loss: CancelToken,
    same_native_context: Arc<dyn Fn(&NativeRequestOwner) -> bool + Send + Sync>,
}

impl AcceptedPdfReload {
    /// Called by the native producer only inside its final multi-owner admission.
    /// Constructing this value does not retire a document or authorize a new one.
    pub fn from_native_admission(
        predecessor: NativeRequestOwner,
        reserved: crate::runtime::RendererGeneration,
        browser_loss: CancelToken,
        reservation_loss: CancelToken,
        same_native_context: Arc<dyn Fn(&NativeRequestOwner) -> bool + Send + Sync>,
    ) -> Self {
        Self {
            predecessor,
            reserved,
            browser_loss,
            reservation_loss,
            same_native_context,
            identity: Arc::new(()),
        }
    }
    fn same_lineage(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity)
    }
    pub fn retires(&self, owner: &NativeRequestOwner) -> bool {
        owner.view == self.predecessor.view
            && (self.same_native_context)(owner)
            && owner
                .context
                .renderer()
                .zip(self.predecessor.context.renderer())
                .is_some_and(|(a, b)| a.same_generation(b))
            && match (
                owner.context.document_lease(),
                self.predecessor.context.document_lease(),
            ) {
                (Some(a), Some(b)) => a.same_generation(b),
                (None, None) => true,
                _ => false,
            }
            && owner
                .pdf_document()
                .zip(self.predecessor.pdf_document())
                .is_some_and(|(a, b)| a.same_document(b))
            && owner
                .pdf_revision()
                .zip(self.predecessor.pdf_revision())
                .is_some_and(|(a, b)| a.same_document(b))
    }
    pub fn reserved_renderer(&self) -> &crate::runtime::RendererGeneration {
        &self.reserved
    }
    pub fn browser_loss(&self) -> &CancelToken {
        &self.browser_loss
    }
    pub fn reservation_loss(&self) -> &CancelToken {
        &self.reservation_loss
    }
}

pub type PdfReloadAccepted = Box<dyn FnOnce(AcceptedPdfReload) + Send>;
pub type NativeSaveReloadDone = Box<dyn FnOnce(Result<Arc<NativeSaveReloadReceipt>, String>)>;

/// Core-only service for reopening generated output in an already open PDF.
/// All three participants are captured before output work begins. The callback
/// remains on the core thread and completes after actual native load readiness.
pub trait PdfReload: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn reload_saved_document(
        &self,
        provided: &crate::Provided,
        services: &crate::runtime::Services,
        input: crate::ipc::NativeSavedInput,
        path: crate::ipc::NativeSavedPath,
        existing: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        accepted: PdfReloadAccepted,
        done: NativeSaveReloadDone,
    );
}

pub type PdfReloadHost = Arc<dyn PdfReload>;
