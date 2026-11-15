//! Native saved-output publication and its exact new browser lineage.
//! These capabilities are Rust-only; no renderer payload can issue them.

use std::sync::{Arc, Mutex};
use crate::{Surface, ViewId, runtime::{CancelToken, RendererGeneration}};
use super::{NativeBrowserOwner, NativeBrowserTransition, NativeClosureReservation, NativeRequestOwner, NativeSaveDestination, NativeSavedPath, SavedPublication};

/// The real OS publication, bound to its exact selected native save inside the
/// original document's admission. A selection alone cannot construct this.
pub(crate) struct NativeOutputScope {
    pub participants: Vec<NativeRequestOwner>,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    pub additional: Vec<CancelToken>,
}

#[derive(Clone)]
pub struct NativeCommittedOutput {
    selection: NativeSaveDestination,
    commit: file_commit::FileCommitReceipt,
    scope: Arc<NativeOutputScope>,
}
impl NativeCommittedOutput {
    /// Only the canonical output-publishing host calls this, while the actual
    /// rename and original selection's authority are held in one transition.
    pub(crate) fn from_publication_admitted(selection: NativeSaveDestination, commit: file_commit::FileCommitReceipt, scope: Arc<NativeOutputScope>) -> Result<Self, String> {
        if commit.path() != selection.path() {
            return Err("The committed output does not match the selected destination.".into());
        }
        Ok(Self { selection, commit, scope })
    }
    pub fn selection(&self) -> &NativeSaveDestination { &self.selection }
    pub fn commit(&self) -> &file_commit::FileCommitReceipt { &self.commit }
    pub fn participants(&self) -> &[NativeRequestOwner] { &self.scope.participants }
    pub fn cancel(&self) -> &CancelToken { &self.scope.cancel }
    pub fn pending(&self) -> &CancelToken { &self.scope.pending }
    pub fn additional(&self) -> &[CancelToken] { &self.scope.additional }
    pub(crate) fn same_scope(&self, other: &Self) -> bool { Arc::ptr_eq(&self.scope, &other.scope) }

}

impl SavedPublication {
    /// Read the exact retained object on a worker; other writers may still modify it.
    pub fn read_input(
        &self, max_bytes: usize, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[CancelToken],
    ) -> std::io::Result<NativeSavedInput> {
        let mut losses = self.loss_tokens().map_err(|error| std::io::Error::new(std::io::ErrorKind::Interrupted, error))?;
        for owner in participants {
            losses.push(owner.loss_token().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Interrupted, "saved-output owner is unavailable"))?);
            losses.push(owner.context.renderer().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Interrupted, "saved-output renderer is unavailable"))?.loss_token());
        }
        let read = self.commit().read_bounded(max_bytes, || {
            cancel.is_cancelled() || pending.is_cancelled() || additional.iter().any(CancelToken::is_cancelled)
                || losses.iter().any(CancelToken::is_cancelled)
        })?;
        Ok(NativeSavedInput { output: self.clone(), bytes: Arc::new(SavedBytes::Read(read)), drive: false })
    }
}

/// Bytes read from the retained committed OS object, or an explicit snapshot
/// of bytes the writer actually wrote. Neither guarantees immutable contents.
#[derive(Clone)]
pub struct NativeSavedInput {
    output: SavedPublication,
    bytes: Arc<SavedBytes>,
    drive: bool,
}

enum SavedBytes {
    Read(file_commit::CommittedRead),
    Written(file_commit::CommittedWriteSnapshot),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSavedInputKind { RetainedObjectRead, WrittenSnapshot }
impl NativeSavedInput {
    /// Construct only from `FileCommitReceipt::read_bounded`, run on a worker.
    /// An arbitrary byte buffer cannot manufacture a committed read.
    pub fn from_committed_read(output: SavedPublication, read: file_commit::CommittedRead) -> Result<Self, String> {
        if !output.commit().same_commit(read.receipt()) {
            return Err("The saved input does not belong to the committed output.".into());
        }
        Ok(Self { output, bytes: Arc::new(SavedBytes::Read(read)), drive: false })
    }
    /// Prepare a Drive service's actual committed-object read. This value is
    /// not an admission: the native gate must verify this exact source receipt
    /// against the private moved/fallback receipt before any actor effect.
    pub fn from_drive_committed_read(output: SavedPublication, read: file_commit::CommittedRead) -> Self {
        Self { output, bytes: Arc::new(SavedBytes::Read(read)), drive: true }
    }
    pub fn output(&self) -> &SavedPublication { &self.output }
    /// Compare the exact retained snapshot, not merely its mutable file identity.
    pub fn same_input(&self, other: &Self) -> bool {
        self.output.same_publication(&other.output) && Arc::ptr_eq(&self.bytes, &other.bytes) && self.drive == other.drive
    }
    /// This is explicitly the successful writer's own serialized byte buffer,
    /// not a later filesystem read. It requires the exact publication receipt.
    pub fn from_written_snapshot(output: SavedPublication, snapshot: file_commit::CommittedWriteSnapshot) -> Result<Self, String> {
        Self::try_from_written_snapshot(output, snapshot).map_err(|(error, _)| error)
    }
    /// On mismatch, preserve the actual written bytes and receipt for the caller.
    pub fn try_from_written_snapshot(output: SavedPublication, snapshot: file_commit::CommittedWriteSnapshot) -> Result<Self, (String, file_commit::CommittedWriteSnapshot)> {
        if !output.commit().same_commit(snapshot.receipt()) {
            return Err(("The written snapshot does not belong to the committed output.".into(), snapshot));
        }
        Ok(Self { output, bytes: Arc::new(SavedBytes::Written(snapshot)), drive: false })
    }
    pub fn kind(&self) -> NativeSavedInputKind {
        match self.bytes.as_ref() {
            SavedBytes::Read(_) => NativeSavedInputKind::RetainedObjectRead,
            SavedBytes::Written(_) => NativeSavedInputKind::WrittenSnapshot,
        }
    }
    pub fn bytes(&self) -> &[u8] {
        match self.bytes.as_ref() { SavedBytes::Read(read) => read.bytes(), SavedBytes::Written(snapshot) => snapshot.bytes() }
    }
    pub fn source_commit(&self) -> &file_commit::FileCommitReceipt {
        match self.bytes.as_ref() { SavedBytes::Read(read) => read.receipt(), SavedBytes::Written(snapshot) => snapshot.receipt() }
    }
}

struct Target {
    input: NativeSavedInput,
    path: NativeSavedPath,
    surface: Surface,
    view: ViewId,
    created: RendererGeneration,
    reservation: NativeClosureReservation,
    browser: NativeBrowserOwner,
    participants: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    state: Mutex<TargetState>,
}

struct TargetState {
    adopted: bool,
    registered: Option<NativeRequestOwner>,
}

/// A validated adoption remains inside the borrowed browser transition.
pub struct NativeSavedAdoption<'a> {
    state: std::sync::MutexGuard<'a, TargetState>,
    _transition: std::marker::PhantomData<&'a ()>,
}
impl NativeSavedAdoption<'_> {
    pub fn commit(mut self) { self.state.adopted = true; }
}

/// Exact creator reservation; not a renderer owner or a path grant.
#[derive(Clone)]
pub struct NativeSavedTarget(Arc<Target>);

/// First native-registered owner of that exact reservation.
#[derive(Clone)]
pub struct NativeSavedDocument {
    target: NativeSavedTarget,
    owner: NativeRequestOwner,
}

/// Preallocated replacement of the registered actor owner within the same browser.
pub struct NativeSavedDocumentSuccessor {
    previous: NativeSavedDocument,
    next: NativeSavedDocument,
}

pub struct NativeSavedDocumentSuccessorGuard<'a> {
    state: std::sync::MutexGuard<'a, TargetState>,
    successor: &'a NativeSavedDocumentSuccessor,
    _transition: std::marker::PhantomData<&'a ()>,
}

impl NativeSavedDocumentSuccessorGuard<'_> {
    pub fn commit(mut self) -> NativeSavedDocument {
        self.state.registered = Some(self.successor.next.owner.clone());
        self.successor.next.clone()
    }
}

impl NativeSavedDocumentSuccessor {
    /// Caller must additionally admit both exact native origins and the old and
    /// new media permits in the same canonical transition.
    pub fn validate_in_transition<'a>(&'a self, transition: &'a NativeBrowserTransition<'_>)
        -> Result<NativeSavedDocumentSuccessorGuard<'a>, String> {
        let target = &self.previous.target.0;
        if !transition.matches(&target.reservation) || !transition.matches_owner(&target.browser) {
            return Err("The saved document browser transition is unavailable.".into());
        }
        let state = target.state.lock().map_err(|_| "The saved target state is unavailable.")?;
        let old = &self.previous.owner;
        if !state.adopted || !state.registered.as_ref().is_some_and(|actual|
            actual.view == old.view && Arc::ptr_eq(&actual.context.as_arc(), &old.context.as_arc())
                && actual.loss_token().zip(old.loss_token()).is_some_and(|(a,b)| a.same_token(&b))) {
            return Err("The saved document owner was already replaced.".into());
        }
        Ok(NativeSavedDocumentSuccessorGuard { state, successor: self, _transition: std::marker::PhantomData })
    }
}

impl NativeSavedTarget {
    /// Prepare the real creator's immutable reservation before its one-time
    /// initial browser adoption. This does not itself admit an effect.
    pub fn prepare(
        input: NativeSavedInput, path: NativeSavedPath, surface: Surface,
        view: ViewId, created: RendererGeneration, reservation: NativeClosureReservation,
        browser: NativeBrowserOwner,
        participants: Vec<NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
        additional: Vec<CancelToken>,
    ) -> Result<Self, String> {
        let output = input.output();
        if !path.matches_publication(output)
            || input.drive != !matches!(&path, NativeSavedPath::Selected(_))
            || !reservation.matches(view, &created)
            || !browser.reservation().same_reservation(&reservation) {
            return Err("The saved output does not match its native browser reservation.".into());
        }
        Ok(Self(Arc::new(Target { input, path, surface, view, created, reservation, browser,
            participants, cancel, pending, additional,
            state: Mutex::new(TargetState { adopted: false, registered: None }) })))
    }
    pub fn same_target(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) }
    pub fn input(&self) -> &NativeSavedInput { &self.0.input }
    pub fn output(&self) -> &SavedPublication { self.0.input.output() }
    pub fn path(&self) -> &NativeSavedPath { &self.0.path }
    pub fn surface(&self) -> Surface { self.0.surface }
    pub fn view(&self) -> ViewId { self.0.view }
    pub fn created(&self) -> &RendererGeneration { &self.0.created }
    pub fn reservation(&self) -> &NativeClosureReservation { &self.0.reservation }
    pub fn browser(&self) -> &NativeBrowserOwner { &self.0.browser }
    pub fn participants(&self) -> &[NativeRequestOwner] { &self.0.participants }
    pub fn cancel(&self) -> &CancelToken { &self.0.cancel }
    pub fn pending(&self) -> &CancelToken { &self.0.pending }
    pub fn additional(&self) -> &[CancelToken] { &self.0.additional }

    /// Prepare the proposed successor before entering authority gates. It is
    /// not adopted until the exact replacement transition commits.
    pub fn prepare_successor(
        &self, replacement: &super::NativeBrowserReplacement,
    ) -> Result<Self, String> {
        if !replacement.original().same_owner(&self.0.browser)
            || !replacement.successor().reservation().same_reservation(&self.0.reservation) {
            return Err("The saved target successor does not match the browser replacement.".into());
        }
        Self::prepare(self.0.input.clone(), self.0.path.clone(), self.0.surface,
            self.0.view, self.0.created.clone(), self.0.reservation.clone(), replacement.successor().clone(),
            self.0.participants.clone(), self.0.cancel.clone(), self.0.pending.clone(),
            self.0.additional.clone())
    }

    /// Validate before mutating the actor or manager. Retain this guard only
    /// within the same browser transition, then commit without failure.
    pub fn validate_in_transition<'a>(&'a self, transition: &'a NativeBrowserTransition<'_>) -> Result<NativeSavedAdoption<'a>, String> {
        if !transition.matches(&self.0.reservation)
            || !(transition.matches_owner(&self.0.browser) || transition.proposes_owner(&self.0.browser)) {
            return Err("The browser transition does not match the saved target.".into());
        }
        let state = self.0.state.lock().map_err(|_| "The saved target state is unavailable.")?;
        if state.adopted { return Err("The saved target was already adopted.".into()); }
        Ok(NativeSavedAdoption { state, _transition: std::marker::PhantomData })
    }

    /// State check only, inside the exact browser and authority gate.
    pub fn with_adopted_state<T>(&self, effect: impl FnOnce() -> T) -> Option<T> {
        let adopted = self.0.state.lock().ok().is_some_and(|state| state.adopted);
        adopted.then(effect)
    }

    /// State-only transition: caller holds the combined native, saved-output,
    /// browser, renderer and cancellation admission. It cannot register twice.
    pub fn register_from_native_admitted(&self, owner: NativeRequestOwner) -> Result<NativeSavedDocument, String> {
        if owner.view != self.0.view || owner.loss_token().is_none()
            || !owner.context.renderer().is_some_and(|generation| generation.same_generation(&self.0.created)) {
            return Err("The registered saved document does not match its reservation.".into());
        }
        let mut state = self.0.state.lock().map_err(|_| "The saved target state is unavailable.")?;
        if !state.adopted || state.registered.is_some() { return Err("The saved target is not awaiting registration.".into()); }
        state.registered = Some(owner.clone());
        Ok(NativeSavedDocument { target: self.clone(), owner })
    }

    /// State check only; caller already holds all canonical gates. The state
    /// mutex is released before the short effect is executed.
    pub fn with_registered_state<T>(&self, owner: &NativeRequestOwner, effect: impl FnOnce() -> T) -> Option<T> {
        let matches = self.0.state.lock().ok().is_some_and(|state| state.registered.as_ref().is_some_and(|actual|
            actual.view == owner.view && Arc::ptr_eq(&actual.context.as_arc(), &owner.context.as_arc())
                && actual.loss_token().zip(owner.loss_token()).is_some_and(|(a,b)| a.same_token(&b))));
        matches.then(effect)
    }
}
impl NativeSavedDocument {
    pub fn target(&self) -> &NativeSavedTarget { &self.target }
    pub fn owner(&self) -> &NativeRequestOwner { &self.owner }
    /// Prepare outside the admission. Exact native-origin and media lineage
    /// are checked by the host while committing this candidate.
    pub fn prepare_owner_successor(&self, owner: NativeRequestOwner) -> Result<NativeSavedDocumentSuccessor, String> {
        if owner.view != self.target.view() || owner.loss_token().is_none()
            || !owner.context.renderer().is_some_and(|generation| generation.same_generation(self.target.created())) {
            return Err("The saved document successor does not match its browser.".into());
        }
        Ok(NativeSavedDocumentSuccessor { previous: self.clone(), next: NativeSavedDocument {
            target: self.target.clone(), owner,
        } })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{ffi::CString, fs::{self, File}, time::{SystemTime, UNIX_EPOCH}};
    use crate::ipc::{NativeClosureIssuer, NativeContext, NativeInvocationIssuer, NativeSaveSelection};

    #[test]
    fn committed_output_requires_actual_publication_and_exact_creator_lineage() {
        let dir = std::env::temp_dir().join(format!("saved-target-{}-{}", std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("stage"), b"synthetic").unwrap();
        let parent = File::open(&dir).unwrap();
        let source = File::open(dir.join("stage")).unwrap();
        let path = dir.join("output.pdf");
        let commit = file_commit::rename_exclusive(&parent, &source, &CString::new("stage").unwrap(),
            &CString::new("output.pdf").unwrap(), &path).unwrap();
        let source_view = ViewId(1);
        let source_generation = RendererGeneration::new();
        let source_owner = NativeRequestOwner { view: source_view,
            context: NativeContext::with_owner_loss((), source_generation, CancelToken::new())
                .with_document_lease(RendererGeneration::new()) };
        let issuer = NativeInvocationIssuer::new(CancelToken::new());
        let binding = issuer.issue_existing_document_save(Surface::Docs, source_view,
            NativeSaveSelection::ExactPath(path.clone()), source_owner, CancelToken::new()).unwrap();
        let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
        let other_owner = selection.binding().owner().clone();
        let other_binding = issuer.issue_existing_document_save(Surface::Docs, source_view,
            NativeSaveSelection::ExactPath(dir.join("other.pdf")), other_owner, CancelToken::new()).unwrap();
        let other_selection = NativeSaveDestination::from_exact_selection(other_binding).unwrap();
        let scope = Arc::new(NativeOutputScope { participants: vec![], cancel: CancelToken::new(), pending: CancelToken::new(), additional: vec![] });
        assert!(NativeCommittedOutput::from_publication_admitted(other_selection, commit.clone(), scope.clone()).is_err());
        fs::rename(&path, dir.join("moved.pdf")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let read = commit.read_bounded(1024, || false).unwrap();
        assert_eq!(read.bytes(), b"synthetic");
        let output = SavedPublication::Native(NativeCommittedOutput::from_publication_admitted(selection.clone(), commit, scope.clone()).unwrap());
        let cancelled = CancelToken::new();
        cancelled.cancel();
        assert!(matches!(output.read_input(1024, &[], &cancelled, &CancelToken::new(), &[]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted));
        assert_eq!(output.read_input(1024, &[], &CancelToken::new(), &CancelToken::new(), &[]).unwrap().bytes(), b"synthetic");
        let direct_path = dir.join("direct.pdf");
        let direct = file_commit::prepare_direct(&parent, &direct_path).unwrap().open().unwrap();
        let snapshot = match direct.write_owned(b"actually written".to_vec()).unwrap() {
            file_commit::DirectOwnedWriteOutcome::Committed(snapshot) => snapshot,
            file_commit::DirectOwnedWriteOutcome::WrittenWithoutReceipt { .. } => panic!("synthetic direct write lacked a receipt"),
        };
        assert!(NativeSavedInput::from_written_snapshot(output.clone(), snapshot).is_err());
        let direct = file_commit::prepare_direct(&parent, &direct_path).unwrap().open().unwrap();
        let snapshot = match direct.write_owned(b"written snapshot".to_vec()).unwrap() {
            file_commit::DirectOwnedWriteOutcome::Committed(snapshot) => snapshot,
            file_commit::DirectOwnedWriteOutcome::WrittenWithoutReceipt { .. } => panic!("synthetic direct write lacked a receipt"),
        };
        let direct_binding = issuer.issue_existing_document_save(Surface::Docs, source_view,
            NativeSaveSelection::ExactPath(direct_path), selection.binding().owner().clone(), CancelToken::new()).unwrap();
        let direct_selection = NativeSaveDestination::from_exact_selection(direct_binding).unwrap();
        let direct_output = SavedPublication::Native(NativeCommittedOutput::from_publication_admitted(direct_selection, snapshot.receipt().clone(), scope).unwrap());
        let written = NativeSavedInput::from_written_snapshot(direct_output, snapshot).unwrap();
        assert_eq!(written.kind(), NativeSavedInputKind::WrittenSnapshot);
        assert_eq!(written.bytes(), b"written snapshot");
        let input = NativeSavedInput::from_committed_read(output.clone(), read).unwrap();
        assert_eq!(input.kind(), NativeSavedInputKind::RetainedObjectRead);
        assert!(input.same_input(&input.clone()));
        let second_read = output.commit().read_bounded(1024, || false).unwrap();
        let second_input = NativeSavedInput::from_committed_read(output.clone(), second_read).unwrap();
        assert!(!input.same_input(&second_input));
        let created = RendererGeneration::new();
        let view = ViewId(2);
        let (_issuer, reservation, browser) = NativeClosureIssuer::new_with_owner(view, created.clone());
        let target = NativeSavedTarget::prepare(input, NativeSavedPath::Selected(output),
            Surface::Pdf, view, created.clone(), reservation.clone(), browser.clone(),
            vec![], CancelToken::new(), CancelToken::new(), vec![]).unwrap();
        let owner = NativeRequestOwner { view, context: NativeContext::with_owner_loss((), created.clone(), CancelToken::new()) };
        assert!(target.register_from_native_admitted(owner.clone()).is_err());
        reservation.with_initial_owner(&browser, |transition| {
            target.validate_in_transition(transition)?.commit();
            Ok(())
        }).unwrap();
        let foreign = NativeRequestOwner { view, context: NativeContext::with_owner_loss((), RendererGeneration::new(), CancelToken::new()) };
        assert!(target.register_from_native_admitted(foreign).is_err());
        let document = target.register_from_native_admitted(owner.clone()).unwrap();
        assert!(target.register_from_native_admitted(owner.clone()).is_err());
        assert_eq!(document.target().with_registered_state(&owner, || 7), Some(7));
        let alias = NativeRequestOwner { view, context: NativeContext::with_owner_loss((), created.clone(), CancelToken::new()) };
        assert_eq!(document.target().with_registered_state(&alias, || 7), None);
        let renewal = document.prepare_owner_successor(alias.clone()).unwrap();
        let renewed = reservation.with_owner(&browser, |transition| {
            Ok(renewal.validate_in_transition(transition)?.commit())
        }).unwrap();
        assert_eq!(target.with_registered_state(&owner, || 7), None);
        assert_eq!(target.with_registered_state(renewed.owner(), || 7), Some(7));
        assert!(reservation.with_owner(&browser, |transition| {
            renewal.validate_in_transition(transition).map(|_| ())
        }).is_err());
        let rejected = reservation.prepare_replacement(&browser).unwrap();
        let rejected_target = target.prepare_successor(&rejected).unwrap();
        assert!(rejected.commit(|transition, _| {
            let _adoption = rejected_target.validate_in_transition(transition)?;
            Err::<(), String>("actor rejected".into())
        }).is_err());
        assert!(reservation.with_owner(&browser, |_| Ok(())).is_ok());
        let plan = reservation.prepare_replacement(&browser).unwrap();
        let next = target.prepare_successor(&plan).unwrap();
        let (successor, superseded, ()) = plan.commit(|transition, _| {
            let adoption = next.validate_in_transition(transition)?;
            adoption.commit();
            Ok(())
        }).unwrap();
        assert!(superseded.matches(&reservation, &browser));
        assert!(successor.same_owner(next.browser()));
        fs::remove_dir_all(dir).unwrap();
    }
}
