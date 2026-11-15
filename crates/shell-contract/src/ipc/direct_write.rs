//! One admitted direct descriptor and its exact completed write.
use std::{io, path::{Path, PathBuf}, sync::{Arc, OnceLock}};
use crate::{hosts::OwnedEffects, runtime::CancelToken};
use super::{NativeCommittedOutput, NativeRequestOwner, NativeSaveDestination};

#[derive(Clone)]
pub(crate) enum DirectWriteOrigin {
    Native(NativeSaveDestination),
    Renderer { origin: NativeRequestOwner, destination: NativeRequestOwner },
}

#[derive(Clone)]
pub(crate) struct DirectWriteScope {
    pub origin: DirectWriteOrigin,
    pub path: PathBuf,
    pub participants: Vec<NativeRequestOwner>,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    pub additional: Vec<CancelToken>,
}

impl DirectWriteScope {
    pub(crate) fn admit<E: OwnedEffects + ?Sized>(&self, effects: &E, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        let additional: Vec<_> = self.additional.iter().collect();
        match &self.origin {
            DirectWriteOrigin::Native(selection) => effects.with_existing_document_destination_effect(
                selection, &self.path, &self.participants, &self.cancel, &self.pending, &additional, effect),
            DirectWriteOrigin::Renderer { origin, destination } => {
                let mut owners = Vec::with_capacity(self.participants.len() + 1);
                owners.push(destination.clone());
                owners.extend_from_slice(&self.participants);
                effects.with_owned_effect_many_extra(origin, &owners, &self.cancel, &self.pending, &additional, effect)
            }
        }
    }
}

impl DirectWriteScope {
    fn native_output_scope(&self) -> Arc<super::native_saved_target::NativeOutputScope> {
        Arc::new(super::native_saved_target::NativeOutputScope {
            participants: self.participants.clone(), cancel: self.cancel.clone(), pending: self.pending.clone(),
            additional: self.additional.clone(),
        })
    }
}
struct RendererPublication {
    scope: DirectWriteScope,
    commit: OnceLock<file_commit::FileCommitReceipt>,
}

/// Real publication under the original renderer's captured authority.
#[derive(Clone)]
pub struct RendererCommittedOutput(Arc<RendererPublication>);

impl RendererCommittedOutput {
    pub(crate) fn prepare(scope: DirectWriteScope) -> Result<Self, String> {
        if !matches!(&scope.origin, DirectWriteOrigin::Renderer { .. }) {
            return Err("The captured output is not a renderer publication.".into());
        }
        Ok(Self(Arc::new(RendererPublication { scope, commit: OnceLock::new() })))
    }
    pub(crate) fn commit_admitted(&self, commit: file_commit::FileCommitReceipt) -> Result<(), String> {
        if commit.path() != self.0.scope.path {
            return Err("The committed renderer output does not match its captured destination.".into());
        }
        self.0.commit.set(commit).map_err(|_| "The renderer publication was already committed.".into())
    }
    pub fn path(&self) -> &Path { &self.0.scope.path }
    pub fn commit(&self) -> &file_commit::FileCommitReceipt { self.0.commit.get().expect("published renderer output") }
    pub fn same_publication(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) }
    pub fn origin(&self) -> &NativeRequestOwner {
        match &self.0.scope.origin { DirectWriteOrigin::Renderer { origin, .. } => origin, _ => unreachable!() }
    }
    pub fn destination(&self) -> &NativeRequestOwner {
        match &self.0.scope.origin { DirectWriteOrigin::Renderer { destination, .. } => destination, _ => unreachable!() }
    }
    pub fn participants(&self) -> &[NativeRequestOwner] { &self.0.scope.participants }
    pub fn cancel(&self) -> &CancelToken { &self.0.scope.cancel }
    pub fn pending(&self) -> &CancelToken { &self.0.scope.pending }
    pub fn additional(&self) -> &[CancelToken] { &self.0.scope.additional }
}

/// Saved output retains its actual native or authenticated renderer provenance.
#[derive(Clone)]
pub enum SavedPublication {
    Native(NativeCommittedOutput),
    Renderer(RendererCommittedOutput),
}
impl SavedPublication {
    pub fn path(&self) -> &Path {
        match self { Self::Native(value) => value.selection().path(), Self::Renderer(value) => value.path() }
    }
    pub fn commit(&self) -> &file_commit::FileCommitReceipt {
        match self { Self::Native(value) => value.commit(), Self::Renderer(value) => value.commit() }
    }
    pub fn source_owner(&self) -> &NativeRequestOwner {
        match self { Self::Native(value) => value.selection().binding().owner(), Self::Renderer(value) => value.origin() }
    }
    /// Observable lifetimes only; effects still require a canonical admission.
    pub fn loss_tokens(&self) -> Result<Vec<CancelToken>, String> {
        let mut tokens = Vec::new();
        fn append(tokens: &mut Vec<CancelToken>, owner: &NativeRequestOwner) -> Result<(), String> {
            tokens.push(owner.loss_token().ok_or("The saved publication owner is unavailable")?);
            tokens.push(owner.context.renderer().ok_or("The saved publication renderer is unavailable")?.loss_token());
            Ok(())
        }
        match self {
            Self::Native(value) => {
                let operation = value.selection().binding().operation();
                append(&mut tokens, value.selection().binding().owner())?;
                for owner in value.participants() { append(&mut tokens, owner)?; }
                tokens.extend([operation.cancel().clone(), operation.shutdown().clone(), operation.authority_generation().loss_token(), value.cancel().clone(), value.pending().clone()]);
                tokens.extend(value.additional().iter().cloned());
            }
            Self::Renderer(value) => {
                append(&mut tokens, value.origin())?;
                append(&mut tokens, value.destination())?;
                for owner in value.participants() { append(&mut tokens, owner)?; }
                tokens.extend([value.cancel().clone(), value.pending().clone()]);
                tokens.extend(value.additional().iter().cloned());
            }
        }
        Ok(tokens)
    }
    pub fn same_publication(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Native(left), Self::Native(right)) => left.selection().matches_binding(right.selection().binding())
                && left.selection().path() == right.selection().path() && left.commit().same_commit(right.commit())
                && left.same_scope(right),
            (Self::Renderer(left), Self::Renderer(right)) => left.same_publication(right),
            _ => false,
        }
    }
    /// Admit the original publication authority together with new participants.
    pub fn with_effect<E: OwnedEffects + ?Sized>(
        &self, effects: &E, participants: &[NativeRequestOwner], cancel: &CancelToken,
        pending: &CancelToken, additional: &[&CancelToken], effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        match self {
            Self::Native(value) => {
                let mut owners = Vec::with_capacity(value.participants().len() + participants.len());
                owners.extend_from_slice(value.participants());
                owners.extend_from_slice(participants);
                let mut controls = Vec::with_capacity(value.additional().len() + additional.len() + 2);
                controls.extend([value.cancel(), value.pending()]);
                controls.extend(value.additional().iter());
                controls.extend_from_slice(additional);
                effects.with_existing_document_destination_effect(
                    value.selection(), value.selection().path(), &owners, cancel, pending, &controls, effect)
            }
            Self::Renderer(value) => {
                let mut owners = Vec::with_capacity(value.participants().len() + participants.len() + 1);
                owners.push(value.destination().clone());
                owners.extend_from_slice(value.participants());
                owners.extend_from_slice(participants);
                let mut controls = Vec::with_capacity(value.additional().len() + additional.len() + 2);
                controls.extend([value.cancel(), value.pending()]);
                controls.extend(value.additional().iter());
                controls.extend_from_slice(additional);
                effects.with_owned_effect_many_extra(value.origin(), &owners, cancel, pending, &controls, effect)
            }
        }
    }
}


pub enum DirectWriteStartError {
    Admission(String),
    Io(io::Error),
    IoAndAdmission { error: io::Error, admission: String },
    OpenedButNotAdmitted { error: String, opened: UnadmittedDirectWrite },
}

impl std::fmt::Debug for DirectWriteStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(error) => f.debug_tuple("Admission").field(error).finish(),
            Self::Io(error) => f.debug_tuple("Io").field(error).finish(),
            Self::IoAndAdmission { error, admission } => f.debug_struct("IoAndAdmission").field("error", error).field("admission", admission).finish(),
            Self::OpenedButNotAdmitted { error, opened } => f.debug_struct("OpenedButNotAdmitted").field("error", error).field("path", &opened.path()).finish(),
        }
    }
}

/// Non-cloneable descriptor opened and truncated under the captured admission.
pub struct AdmittedDirectWrite { scope: DirectWriteScope, descriptor: file_commit::DirectWrite }

/// A failed admission after a real truncating open. This holds the affected
/// descriptor but grants no permission to write through it.
pub struct UnadmittedDirectWrite { scope: DirectWriteScope, _descriptor: file_commit::DirectWrite }
impl UnadmittedDirectWrite {
    pub fn path(&self) -> &Path { &self.scope.path }
}

/// A descriptor write completed; this alone grants no later publication effect.
pub struct CompletedDirectWrite { scope: DirectWriteScope, outcome: file_commit::DirectOwnedWriteOutcome }

/// A failed write retains the original descriptor, bytes and exact progress.
pub struct PartialAdmittedDirectWrite { scope: DirectWriteScope, partial: file_commit::PartialDirectWrite }

pub enum DirectWriteResumeError {
    Admission { error: String, partial: PartialAdmittedDirectWrite },
    Io { error: io::Error, partial: PartialAdmittedDirectWrite },
}

pub enum DirectWriteCompletion {
    Committed { publication: SavedPublication, snapshot: file_commit::CommittedWriteSnapshot },
    WrittenWithoutReceipt { error: io::Error, bytes: Vec<u8>, admission: Result<(), String> },
    WrittenButNotAdmitted { error: String, snapshot: file_commit::CommittedWriteSnapshot },
}

pub enum StagedWriteError {
    Admission { error: String, stage: file_commit::OwnedWrittenCommit },
    Io { error: io::Error, stage: file_commit::OwnedWrittenCommit },
    CommittedButNotAdmitted { error: String, snapshot: file_commit::CommittedWriteSnapshot },
}

pub(crate) fn publish_staged<E: OwnedEffects + ?Sized>(
    effects: &E, scope: DirectWriteScope, stage: file_commit::OwnedWrittenCommit,
) -> Result<(SavedPublication, file_commit::CommittedWriteSnapshot), StagedWriteError> {
    publish_staged_inner(effects, scope, stage, None)
}

pub(crate) fn publish_staged_validated<E: OwnedEffects + ?Sized>(
    effects: &E, scope: DirectWriteScope, stage: file_commit::OwnedWrittenCommit,
    validate: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(SavedPublication, file_commit::CommittedWriteSnapshot), StagedWriteError> {
    publish_staged_inner(effects, scope, stage, Some(validate))
}

fn publish_staged_inner<E: OwnedEffects + ?Sized>(
    effects: &E, scope: DirectWriteScope, stage: file_commit::OwnedWrittenCommit,
    mut validate: Option<&mut dyn FnMut() -> Result<(), String>>,
) -> Result<(SavedPublication, file_commit::CommittedWriteSnapshot), StagedWriteError> {
    if stage.path() != scope.path {
        return Err(StagedWriteError::Admission { error: "The staged output does not match its captured destination.".into(), stage });
    }
    let prepared_renderer = if matches!(&scope.origin, DirectWriteOrigin::Renderer { .. }) {
        match RendererCommittedOutput::prepare(scope.clone()) {
            Ok(value) => Some(value),
            Err(error) => return Err(StagedWriteError::Admission { error, stage }),
        }
    } else { None };
    let mut native_selection = match &scope.origin { DirectWriteOrigin::Native(value) => Some(value.clone()), _ => None };
    let native_scope = scope.native_output_scope();
    let mut stage = Some(stage);
    let mut outcome = None;
    let admission = scope.admit(effects, &mut || {
        if let Some(validate) = validate.as_mut() { validate()?; }
        let staged = stage.take().ok_or("The staged output was already consumed.")?;
        match staged.publish() {
            Err((error, staged)) => outcome = Some(Err((error, staged))),
            Ok(snapshot) => {
                let publication = match &scope.origin {
                    DirectWriteOrigin::Native(_) => native_selection.take()
                        .ok_or_else(|| "The native publication was already consumed.".to_owned())
                        .and_then(|selection| NativeCommittedOutput::from_publication_admitted(selection, snapshot.receipt().clone(), native_scope.clone()))
                        .map(SavedPublication::Native),
                    DirectWriteOrigin::Renderer { .. } => prepared_renderer.as_ref()
                        .ok_or_else(|| "The renderer publication was not prepared.".to_owned())
                        .and_then(|prepared| prepared.commit_admitted(snapshot.receipt().clone()).map(|()| SavedPublication::Renderer(prepared.clone()))),
                };
                outcome = Some(Ok((publication, snapshot)));
            }
        }
        Ok(())
    });
    match outcome {
        Some(Err((error, stage))) => Err(StagedWriteError::Io { error, stage }),
        Some(Ok((publication, snapshot))) => match (admission, publication) {
            (Ok(()), Ok(publication)) => Ok((publication, snapshot)),
            (Err(error), _) | (Ok(()), Err(error)) => Err(StagedWriteError::CommittedButNotAdmitted { error, snapshot }),
        },
        None => Err(StagedWriteError::Admission {
            error: admission.err().unwrap_or_else(|| "The staged output was not admitted.".into()),
            stage: stage.expect("unconsumed staged output"),
        }),
    }
}

impl AdmittedDirectWrite {
    pub(crate) fn begin<E: OwnedEffects + ?Sized>(
        effects: &E, scope: DirectWriteScope, prepared: file_commit::PreparedDirectWrite,
    ) -> Result<Self, DirectWriteStartError> {
        Self::begin_inner(effects, scope, prepared, None)
    }

    pub(crate) fn begin_validated<E: OwnedEffects + ?Sized>(
        effects: &E, scope: DirectWriteScope, prepared: file_commit::PreparedDirectWrite,
        validate: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<Self, DirectWriteStartError> {
        Self::begin_inner(effects, scope, prepared, Some(validate))
    }

    fn begin_inner<E: OwnedEffects + ?Sized>(
        effects: &E, scope: DirectWriteScope, prepared: file_commit::PreparedDirectWrite,
        mut validate: Option<&mut dyn FnMut() -> Result<(), String>>,
    ) -> Result<Self, DirectWriteStartError> {
        if prepared.path() != scope.path.as_path() {
            return Err(DirectWriteStartError::Admission("The prepared direct write does not match its captured destination.".into()));
        }
        let mut prepared = Some(prepared);
        let mut opened = None;
        let admission = scope.admit(effects, &mut || {
            if let Some(validate) = validate.as_mut() { validate()?; }
            let prepared = prepared.take().ok_or("The direct write was already admitted.")?;
            opened = Some(prepared.open());
            Ok(())
        });
        match (opened, admission) {
            (Some(Ok(descriptor)), Ok(())) => Ok(Self { scope, descriptor }),
            (Some(Ok(descriptor)), Err(error)) => Err(DirectWriteStartError::OpenedButNotAdmitted {
                error, opened: UnadmittedDirectWrite { scope, _descriptor: descriptor },
            }),
            (Some(Err(error)), Ok(())) => Err(DirectWriteStartError::Io(error)),
            (Some(Err(error)), Err(admission)) => Err(DirectWriteStartError::IoAndAdmission { error, admission }),
            (None, Err(error)) => Err(DirectWriteStartError::Admission(error)),
            (None, Ok(())) => Err(DirectWriteStartError::Admission("The direct write was not admitted.".into())),
        }
    }

    /// Run on the worker after the short opening admission has been released.
    pub fn write_owned(self, bytes: Vec<u8>) -> Result<CompletedDirectWrite, (io::Error, PartialAdmittedDirectWrite)> {
        let Self { scope, descriptor } = self;
        match descriptor.write_owned(bytes) {
            file_commit::DirectOwnedWriteAttempt::Completed(outcome) => Ok(CompletedDirectWrite { scope, outcome }),
            file_commit::DirectOwnedWriteAttempt::Partial { error, partial } => Err((error, PartialAdmittedDirectWrite { scope, partial })),
        }
    }
}

impl PartialAdmittedDirectWrite {
    pub fn written(&self) -> usize { self.partial.written() }
    pub fn bytes(&self) -> &[u8] { self.partial.bytes() }
    /// A retry is re-admitted before writing any remaining bytes.
    pub fn resume<E: OwnedEffects + ?Sized>(self, effects: &E) -> Result<CompletedDirectWrite, DirectWriteResumeError> {
        let Self { scope, partial } = self;
        if let Err(error) = scope.admit(effects, &mut || Ok(())) {
            return Err(DirectWriteResumeError::Admission { error, partial: Self { scope, partial } });
        }
        match partial.resume() {
            file_commit::DirectOwnedWriteAttempt::Completed(outcome) => Ok(CompletedDirectWrite { scope, outcome }),
            file_commit::DirectOwnedWriteAttempt::Partial { error, partial } => Err(DirectWriteResumeError::Io { error, partial: Self { scope, partial } }),
        }
    }

    /// The caller may retain the exact descriptor and progress for its rescue path.
    pub fn into_rescue_parts(self) -> (file_commit::AbandonedDirectWrite, Vec<u8>, usize) {
        self.partial.into_parts()
    }
}

impl CompletedDirectWrite {
    /// Preserve a real write even if proof creation or final admission fails.
    pub fn finish<E: OwnedEffects + ?Sized>(self, effects: &E) -> DirectWriteCompletion {
        let Self { scope, outcome } = self;
        match outcome {
            file_commit::DirectOwnedWriteOutcome::WrittenWithoutReceipt { error, bytes } => {
                let admission = scope.admit(effects, &mut || Ok(()));
                DirectWriteCompletion::WrittenWithoutReceipt { error, bytes, admission }
            }
            file_commit::DirectOwnedWriteOutcome::Committed(snapshot) => {
                let prepared_renderer = if matches!(&scope.origin, DirectWriteOrigin::Renderer { .. }) {
                    match RendererCommittedOutput::prepare(scope.clone()) {
                        Ok(value) => Some(value),
                        Err(error) => return DirectWriteCompletion::WrittenButNotAdmitted { error, snapshot },
                    }
                } else { None };
                let native_scope = scope.native_output_scope();
                let mut native_selection = match &scope.origin { DirectWriteOrigin::Native(value) => Some(value.clone()), _ => None };
                let mut publication = None;
                let admission = scope.admit(effects, &mut || {
                    if publication.is_some() { return Err("The direct write was already finalized.".into()); }
                    publication = Some(match &scope.origin {
                        DirectWriteOrigin::Native(_) => SavedPublication::Native(
                            NativeCommittedOutput::from_publication_admitted(native_selection.take().ok_or("The native publication was already consumed.")?, snapshot.receipt().clone(), native_scope.clone())?),
                        DirectWriteOrigin::Renderer { .. } => {
                            let prepared = prepared_renderer.as_ref().ok_or("The renderer publication was not prepared.")?;
                            prepared.commit_admitted(snapshot.receipt().clone())?;
                            SavedPublication::Renderer(prepared.clone())
                        }
                    });
                    Ok(())
                }).and_then(|()| publication.ok_or_else(|| "The direct write was not finalized.".to_owned()));
                match admission {
                    Ok(publication) => DirectWriteCompletion::Committed { publication, snapshot },
                    Err(error) => DirectWriteCompletion::WrittenButNotAdmitted { error, snapshot },
                }
            }
        }
    }

    pub fn path(&self) -> &Path { &self.scope.path }
}
