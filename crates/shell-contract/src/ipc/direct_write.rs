//! One admitted direct descriptor and its exact completed write.
use super::{NativeCommittedOutput, NativeRequestOwner, NativeSaveDestination};
use crate::{hosts::OwnedEffects, runtime::CancelToken};
use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

#[derive(Clone)]
pub(crate) enum DirectWriteOrigin {
    Native(NativeSaveDestination),
    Renderer {
        origin: NativeRequestOwner,
        destination: NativeRequestOwner,
    },
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
    pub(crate) fn admit<E: OwnedEffects + ?Sized>(
        &self,
        effects: &E,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let additional: Vec<_> = self.additional.iter().collect();
        match &self.origin {
            DirectWriteOrigin::Native(selection) => effects
                .with_existing_document_destination_effect(
                    selection,
                    &self.path,
                    &self.participants,
                    &self.cancel,
                    &self.pending,
                    &additional,
                    effect,
                ),
            DirectWriteOrigin::Renderer {
                origin,
                destination,
            } => {
                let mut owners = Vec::with_capacity(self.participants.len() + 1);
                owners.push(destination.clone());
                owners.extend_from_slice(&self.participants);
                effects.with_owned_effect_many_extra(
                    origin,
                    &owners,
                    &self.cancel,
                    &self.pending,
                    &additional,
                    effect,
                )
            }
        }
    }
}

impl DirectWriteScope {
    fn native_output_scope(&self) -> Arc<super::native_saved_target::NativeOutputScope> {
        Arc::new(super::native_saved_target::NativeOutputScope {
            participants: self.participants.clone(),
            cancel: self.cancel.clone(),
            pending: self.pending.clone(),
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
        Ok(Self(Arc::new(RendererPublication {
            scope,
            commit: OnceLock::new(),
        })))
    }
    pub(crate) fn commit_admitted(
        &self,
        commit: file_commit::FileCommitReceipt,
    ) -> Result<(), String> {
        if commit.path() != self.0.scope.path {
            return Err(
                "The committed renderer output does not match its captured destination.".into(),
            );
        }
        self.0
            .commit
            .set(commit)
            .map_err(|_| "The renderer publication was already committed.".into())
    }
    pub fn path(&self) -> &Path {
        &self.0.scope.path
    }
    pub fn commit(&self) -> &file_commit::FileCommitReceipt {
        self.0.commit.get().expect("published renderer output")
    }
    pub fn same_publication(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn origin(&self) -> &NativeRequestOwner {
        match &self.0.scope.origin {
            DirectWriteOrigin::Renderer { origin, .. } => origin,
            _ => unreachable!(),
        }
    }
    pub fn destination(&self) -> &NativeRequestOwner {
        match &self.0.scope.origin {
            DirectWriteOrigin::Renderer { destination, .. } => destination,
            _ => unreachable!(),
        }
    }
    pub fn participants(&self) -> &[NativeRequestOwner] {
        &self.0.scope.participants
    }
    pub fn cancel(&self) -> &CancelToken {
        &self.0.scope.cancel
    }
    pub fn pending(&self) -> &CancelToken {
        &self.0.scope.pending
    }
    pub fn additional(&self) -> &[CancelToken] {
        &self.0.scope.additional
    }
}

/// Saved output retains its actual native or authenticated renderer provenance.
#[derive(Clone)]
pub enum SavedPublication {
    Native(NativeCommittedOutput),
    Renderer(RendererCommittedOutput),
}
impl SavedPublication {
    /// Wraps an already completed commit (a project new-document write) as a renderer
    /// publication. The same scope checks as a direct write run now: every owner, `cancel`,
    /// `pending` and each `additional` token must be live under the canonical admission, and the
    /// receipt must be the commit of exactly `destination_path`. A receipt can only come from an
    /// actual commit, so nothing is minted for a path that was not published.
    #[allow(clippy::too_many_arguments)]
    pub fn renderer_commit<E: OwnedEffects + ?Sized>(
        effects: &E,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        destination_path: &Path,
        commit: file_commit::FileCommitReceipt,
    ) -> Result<Self, String> {
        if commit.path() != destination_path {
            return Err(
                "The committed project output does not match its captured destination.".into(),
            );
        }
        let scope = DirectWriteScope {
            origin: DirectWriteOrigin::Renderer {
                origin: origin.clone(),
                destination: destination.clone(),
            },
            path: destination_path.to_owned(),
            participants: participants.to_vec(),
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
        };
        let output = RendererCommittedOutput::prepare(scope.clone())?;
        scope.admit(effects, &mut || output.commit_admitted(commit.clone()))?;
        Ok(Self::Renderer(output))
    }
}
impl SavedPublication {
    pub fn path(&self) -> &Path {
        match self {
            Self::Native(value) => value.selection().path(),
            Self::Renderer(value) => value.path(),
        }
    }
    pub fn commit(&self) -> &file_commit::FileCommitReceipt {
        match self {
            Self::Native(value) => value.commit(),
            Self::Renderer(value) => value.commit(),
        }
    }
    pub fn source_owner(&self) -> &NativeRequestOwner {
        match self {
            Self::Native(value) => value.selection().binding().owner(),
            Self::Renderer(value) => value.origin(),
        }
    }
    /// Observable lifetimes only; effects still require a canonical admission.
    pub fn loss_tokens(&self) -> Result<Vec<CancelToken>, String> {
        let mut tokens = Vec::new();
        fn append(tokens: &mut Vec<CancelToken>, owner: &NativeRequestOwner) -> Result<(), String> {
            tokens.push(
                owner
                    .loss_token()
                    .ok_or("The saved publication owner is unavailable")?,
            );
            tokens.push(
                owner
                    .context
                    .renderer()
                    .ok_or("The saved publication renderer is unavailable")?
                    .loss_token(),
            );
            Ok(())
        }
        match self {
            Self::Native(value) => {
                let operation = value.selection().binding().operation();
                append(&mut tokens, value.selection().binding().owner())?;
                for owner in value.participants() {
                    append(&mut tokens, owner)?;
                }
                tokens.extend([
                    operation.cancel().clone(),
                    operation.shutdown().clone(),
                    operation.authority_generation().loss_token(),
                    value.cancel().clone(),
                    value.pending().clone(),
                ]);
                tokens.extend(value.additional().iter().cloned());
            }
            Self::Renderer(value) => {
                append(&mut tokens, value.origin())?;
                append(&mut tokens, value.destination())?;
                for owner in value.participants() {
                    append(&mut tokens, owner)?;
                }
                tokens.extend([value.cancel().clone(), value.pending().clone()]);
                tokens.extend(value.additional().iter().cloned());
            }
        }
        Ok(tokens)
    }
    pub fn same_publication(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Native(left), Self::Native(right)) => {
                left.selection()
                    .matches_binding(right.selection().binding())
                    && left.selection().path() == right.selection().path()
                    && left.commit().same_commit(right.commit())
                    && left.same_scope(right)
            }
            (Self::Renderer(left), Self::Renderer(right)) => left.same_publication(right),
            _ => false,
        }
    }
    /// Admit the original publication authority together with new participants.
    pub fn with_effect<E: OwnedEffects + ?Sized>(
        &self,
        effects: &E,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        match self {
            Self::Native(value) => {
                let mut owners =
                    Vec::with_capacity(value.participants().len() + participants.len());
                owners.extend_from_slice(value.participants());
                owners.extend_from_slice(participants);
                let mut controls =
                    Vec::with_capacity(value.additional().len() + additional.len() + 2);
                controls.extend([value.cancel(), value.pending()]);
                controls.extend(value.additional().iter());
                controls.extend_from_slice(additional);
                effects.with_existing_document_destination_effect(
                    value.selection(),
                    value.selection().path(),
                    &owners,
                    cancel,
                    pending,
                    &controls,
                    effect,
                )
            }
            Self::Renderer(value) => {
                let mut owners =
                    Vec::with_capacity(value.participants().len() + participants.len() + 1);
                owners.push(value.destination().clone());
                owners.extend_from_slice(value.participants());
                owners.extend_from_slice(participants);
                let mut controls =
                    Vec::with_capacity(value.additional().len() + additional.len() + 2);
                controls.extend([value.cancel(), value.pending()]);
                controls.extend(value.additional().iter());
                controls.extend_from_slice(additional);
                effects.with_owned_effect_many_extra(
                    value.origin(),
                    &owners,
                    cancel,
                    pending,
                    &controls,
                    effect,
                )
            }
        }
    }
}

pub enum DirectWriteStartError {
    Admission(String),
    Io(io::Error),
    IoAndAdmission {
        error: io::Error,
        admission: String,
    },
    OpenedButNotAdmitted {
        error: String,
        opened: Box<UnadmittedDirectWrite>,
    },
}

impl std::fmt::Debug for DirectWriteStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(error) => f.debug_tuple("Admission").field(error).finish(),
            Self::Io(error) => f.debug_tuple("Io").field(error).finish(),
            Self::IoAndAdmission { error, admission } => f
                .debug_struct("IoAndAdmission")
                .field("error", error)
                .field("admission", admission)
                .finish(),
            Self::OpenedButNotAdmitted { error, opened } => f
                .debug_struct("OpenedButNotAdmitted")
                .field("error", error)
                .field("path", &opened.path())
                .finish(),
        }
    }
}

/// Non-cloneable descriptor opened and truncated under the captured admission.
pub struct AdmittedDirectWrite {
    scope: DirectWriteScope,
    descriptor: file_commit::DirectWrite,
}

/// A failed admission after a real truncating open. This holds the affected
/// descriptor but grants no permission to write through it.
pub struct UnadmittedDirectWrite {
    scope: DirectWriteScope,
    _descriptor: file_commit::DirectWrite,
}
impl UnadmittedDirectWrite {
    pub fn path(&self) -> &Path {
        &self.scope.path
    }
}

/// A descriptor write completed; this alone grants no later publication effect.
pub struct CompletedDirectWrite {
    scope: DirectWriteScope,
    outcome: file_commit::DirectOwnedWriteOutcome,
}

/// A failed write retains the original descriptor, bytes and exact progress.
pub struct PartialAdmittedDirectWrite(Box<PartialParts>);

struct PartialParts {
    scope: DirectWriteScope,
    partial: file_commit::PartialDirectWrite,
}

pub enum DirectWriteResumeError {
    Admission {
        error: String,
        partial: PartialAdmittedDirectWrite,
    },
    Io {
        error: io::Error,
        partial: PartialAdmittedDirectWrite,
    },
}

pub enum DirectWriteCompletion {
    Committed {
        publication: SavedPublication,
        snapshot: file_commit::CommittedWriteSnapshot,
    },
    WrittenWithoutReceipt {
        error: io::Error,
        bytes: Vec<u8>,
        admission: Result<(), String>,
    },
    WrittenButNotAdmitted {
        error: String,
        snapshot: file_commit::CommittedWriteSnapshot,
    },
}

pub enum StagedWriteError {
    Admission {
        error: String,
        stage: Box<file_commit::OwnedWrittenCommit>,
    },
    Io {
        error: io::Error,
        stage: Box<file_commit::OwnedWrittenCommit>,
    },
    CommittedButNotAdmitted {
        error: String,
        snapshot: file_commit::CommittedWriteSnapshot,
    },
    /// The staged output could not replace the destination and the in-place
    /// fallback did not finish the save. `leftover` is the private stage that is
    /// still on disk: it may hold the only copy of the new bytes, so the caller
    /// leaves it and its directory alone. `None` means the stage is already
    /// gone because the bytes sit in the rescue file beside the destination (or
    /// the destination already holds them).
    Unsaved {
        error: String,
        leftover: Option<Box<file_commit::OwnedStage>>,
    },
}

pub(crate) fn publish_staged<E: OwnedEffects + ?Sized>(
    effects: &E,
    scope: DirectWriteScope,
    stage: file_commit::OwnedWrittenCommit,
) -> Result<(SavedPublication, file_commit::CommittedWriteSnapshot), StagedWriteError> {
    publish_staged_inner(effects, scope, stage, None)
}

pub(crate) fn publish_staged_validated<E: OwnedEffects + ?Sized>(
    effects: &E,
    scope: DirectWriteScope,
    stage: file_commit::OwnedWrittenCommit,
    validate: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(SavedPublication, file_commit::CommittedWriteSnapshot), StagedWriteError> {
    publish_staged_inner(effects, scope, stage, Some(validate))
}

/// Pauses between attempts to replace a destination that is only transiently
/// locked (antivirus or an indexer on Windows); the first failure is retried
/// after the first delay, and so on.
const REPLACE_RETRY_DELAYS_MS: [u64; 4] = [50, 100, 200, 400];

/// Most bytes of the destination's file name a rescue file name carries.
const RESCUE_NAME_MAX_BYTES: usize = 200;

fn replace_is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ResourceBusy
    ) || (cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)))
}

/// Where a save that could not be written parks its bytes: beside the
/// destination under one stable name, so a save that keeps failing replaces the
/// same file instead of leaving a new one on every attempt.
fn rescue_file_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let mut end = name.len().min(RESCUE_NAME_MAX_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}.unsaved.tmp", &name[..end]))
}

/// The save landed, so a rescue file an earlier failed attempt left is stale.
fn clear_rescue_copy(path: &Path) {
    if let Some(name) = rescue_file_name(path) {
        let _ = std::fs::remove_file(path.with_file_name(name));
    }
}

/// Publishes the stage, and when the replacement is refused retries a transient
/// refusal, then writes the destination in place, then parks the bytes in a
/// rescue file. Every attempt is admitted again under the original owners and
/// controls and runs `validate` first. The pauses block the calling core turn,
/// as the retry of the Electron writer blocked its main process turn.
fn publish_staged_inner<E: OwnedEffects + ?Sized>(
    effects: &E,
    scope: DirectWriteScope,
    stage: file_commit::OwnedWrittenCommit,
    mut validate: Option<&mut dyn FnMut() -> Result<(), String>>,
) -> Result<(SavedPublication, file_commit::CommittedWriteSnapshot), StagedWriteError> {
    let mut check = || match validate.as_mut() {
        Some(validate) => validate(),
        None => Ok(()),
    };
    let mut delays = REPLACE_RETRY_DELAYS_MS.into_iter();
    let mut result = publish_once(effects, &scope, stage, &mut check);
    loop {
        let (error, stage) = match result {
            Err(StagedWriteError::Io { error, stage }) => (error, stage),
            settled => return settled,
        };
        match delays.next() {
            Some(delay) if replace_is_transient(&error) => {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                result = publish_once(effects, &scope, *stage, &mut check);
            }
            _ => return write_in_place(effects, &scope, error, stage, &mut check),
        }
    }
}

fn publish_once<E: OwnedEffects + ?Sized>(
    effects: &E,
    scope: &DirectWriteScope,
    stage: file_commit::OwnedWrittenCommit,
    validate: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(SavedPublication, file_commit::CommittedWriteSnapshot), StagedWriteError> {
    if stage.path() != scope.path {
        return Err(StagedWriteError::Admission {
            error: "The staged output does not match its captured destination.".into(),
            stage: Box::new(stage),
        });
    }
    let prepared_renderer = if matches!(&scope.origin, DirectWriteOrigin::Renderer { .. }) {
        match RendererCommittedOutput::prepare(scope.clone()) {
            Ok(value) => Some(value),
            Err(error) => {
                return Err(StagedWriteError::Admission {
                    error,
                    stage: Box::new(stage),
                });
            }
        }
    } else {
        None
    };
    let mut native_selection = match &scope.origin {
        DirectWriteOrigin::Native(value) => Some(value.clone()),
        _ => None,
    };
    let native_scope = scope.native_output_scope();
    let mut stage = Some(stage);
    let mut outcome = None;
    let admission = scope.admit(effects, &mut || {
        validate()?;
        let staged = stage
            .take()
            .ok_or("The staged output was already consumed.")?;
        match staged.publish() {
            Err(failed) => outcome = Some(Err(failed)),
            Ok(snapshot) => {
                clear_rescue_copy(&scope.path);
                let publication = match &scope.origin {
                    DirectWriteOrigin::Native(_) => native_selection
                        .take()
                        .ok_or_else(|| "The native publication was already consumed.".to_owned())
                        .and_then(|selection| {
                            NativeCommittedOutput::from_publication_admitted(
                                selection,
                                snapshot.receipt().clone(),
                                native_scope.clone(),
                            )
                        })
                        .map(SavedPublication::Native),
                    DirectWriteOrigin::Renderer { .. } => prepared_renderer
                        .as_ref()
                        .ok_or_else(|| "The renderer publication was not prepared.".to_owned())
                        .and_then(|prepared| {
                            prepared
                                .commit_admitted(snapshot.receipt().clone())
                                .map(|()| SavedPublication::Renderer(prepared.clone()))
                        }),
                };
                outcome = Some(Ok((publication, snapshot)));
            }
        }
        Ok(())
    });
    match outcome {
        Some(Err(failed)) => {
            let (error, stage) = *failed;
            Err(StagedWriteError::Io {
                error,
                stage: Box::new(stage),
            })
        }
        Some(Ok((publication, snapshot))) => match (admission, publication) {
            (Ok(()), Ok(publication)) => Ok((publication, snapshot)),
            (Err(error), _) | (Ok(()), Err(error)) => {
                Err(StagedWriteError::CommittedButNotAdmitted { error, snapshot })
            }
        },
        None => Err(StagedWriteError::Admission {
            error: admission
                .err()
                .unwrap_or_else(|| "The staged output was not admitted.".into()),
            stage: Box::new(stage.expect("unconsumed staged output")),
        }),
    }
}

/// Removes the staged object; one that stays is handed back because it may
/// still be the only copy of the bytes.
fn remove_stage(stage: file_commit::OwnedStage) -> Option<Box<file_commit::OwnedStage>> {
    match stage.remove() {
        Ok(()) => None,
        Err((error, _)) if error.kind() == io::ErrorKind::NotFound => None,
        Err((_, stage)) => Some(Box::new(stage)),
    }
}

/// The replacement was refused for good. A destination the user already
/// approved replacing is written in place, as the Electron writer fell back to
/// `writeFile`; a destination that must not exist yet (`Exclusive`) is never
/// overwritten. If the in-place write cannot finish, the staged bytes are
/// copied to the rescue file.
fn write_in_place<E: OwnedEffects + ?Sized>(
    effects: &E,
    scope: &DirectWriteScope,
    error: io::Error,
    stage: Box<file_commit::OwnedWrittenCommit>,
    validate: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(SavedPublication, file_commit::CommittedWriteSnapshot), StagedWriteError> {
    if stage.mode() != file_commit::PublicationMode::Replace {
        return Err(StagedWriteError::Io { error, stage });
    }
    let prepared = match scope
        .path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output has no parent"))
        .and_then(file_commit::open_output_parent)
        .and_then(|parent| file_commit::prepare_direct(&parent, &scope.path))
    {
        Ok(prepared) => prepared,
        Err(_) => return Err(StagedWriteError::Io { error, stage }),
    };
    let begin_validate: &mut dyn FnMut() -> Result<(), String> = &mut *validate;
    let direct = match AdmittedDirectWrite::begin_inner(
        effects,
        scope.clone(),
        prepared,
        Some(begin_validate),
    ) {
        Ok(direct) => direct,
        Err(DirectWriteStartError::Admission(error)) => {
            return Err(StagedWriteError::Admission { error, stage });
        }
        Err(DirectWriteStartError::IoAndAdmission { admission, .. }) => {
            return Err(StagedWriteError::Admission {
                error: admission,
                stage,
            });
        }
        Err(DirectWriteStartError::OpenedButNotAdmitted { error, opened }) => {
            // the destination was truncated but the write is no longer admitted
            drop(opened);
            let (stage, _) = (*stage).into_parts();
            return Err(StagedWriteError::Unsaved {
                error,
                leftover: Some(Box::new(stage)),
            });
        }
        Err(DirectWriteStartError::Io(open_error)) => {
            let (stage, _) = (*stage).into_parts();
            return Err(rescue_unsaved(
                effects,
                scope,
                validate,
                open_error.to_string(),
                stage,
            ));
        }
    };
    let (stage, bytes) = (*stage).into_parts();
    match direct.write_owned(bytes) {
        Ok(completed) => match completed.finish(effects) {
            DirectWriteCompletion::Committed {
                publication,
                snapshot,
            } => {
                let _ = remove_stage(stage);
                let _ = scope.admit(effects, &mut || {
                    clear_rescue_copy(&scope.path);
                    Ok(())
                });
                Ok((publication, snapshot))
            }
            DirectWriteCompletion::WrittenWithoutReceipt { error, .. } => {
                Err(StagedWriteError::Unsaved {
                    error: format!("The output was written but could not be confirmed: {error}"),
                    leftover: remove_stage(stage),
                })
            }
            DirectWriteCompletion::WrittenButNotAdmitted { error, snapshot } => {
                let _ = remove_stage(stage);
                Err(StagedWriteError::CommittedButNotAdmitted { error, snapshot })
            }
        },
        Err((write_error, partial)) => {
            drop(partial);
            Err(rescue_unsaved(
                effects,
                scope,
                validate,
                write_error.to_string(),
                stage,
            ))
        }
    }
}

/// The in-place write failed after the destination may already have been
/// truncated: park the staged bytes in the rescue file. The stage is removed
/// only once those bytes are safe there.
fn rescue_unsaved<E: OwnedEffects + ?Sized>(
    effects: &E,
    scope: &DirectWriteScope,
    validate: &mut dyn FnMut() -> Result<(), String>,
    error: String,
    mut stage: file_commit::OwnedStage,
) -> StagedWriteError {
    let leftover = if copy_to_rescue(effects, scope, validate, &mut stage) {
        remove_stage(stage)
    } else {
        Some(Box::new(stage))
    };
    StagedWriteError::Unsaved { error, leftover }
}

/// Only the descriptor-relative open and truncation of the rescue file run
/// inside the admission; the copy itself runs after it.
#[cfg(unix)]
fn copy_to_rescue<E: OwnedEffects + ?Sized>(
    effects: &E,
    scope: &DirectWriteScope,
    validate: &mut dyn FnMut() -> Result<(), String>,
    stage: &mut file_commit::OwnedStage,
) -> bool {
    let Some(name) =
        rescue_file_name(&scope.path).and_then(|name| std::ffi::CString::new(name).ok())
    else {
        return false;
    };
    let Ok(parent) = stage.original_parent() else {
        return false;
    };
    let Ok(prepared) = file_commit::PreparedRescueCopy::new(&parent, &name, stage) else {
        return false;
    };
    let mut prepared = Some(prepared);
    let mut opened = None;
    let admission = scope.admit(effects, &mut || {
        validate()?;
        let prepared = prepared
            .take()
            .ok_or("The rescue open was already admitted.")?;
        opened = Some(prepared.open());
        Ok(())
    });
    match (admission, opened) {
        (Ok(()), Some(Ok(file_commit::RescueOpen::Ready(copy)))) => copy.copy().is_ok(),
        _ => false,
    }
}

/// The Windows stage has no handle-relative rescue copy, so the bytes stay in
/// the private stage.
#[cfg(not(unix))]
fn copy_to_rescue<E: OwnedEffects + ?Sized>(
    _effects: &E,
    _scope: &DirectWriteScope,
    _validate: &mut dyn FnMut() -> Result<(), String>,
    _stage: &mut file_commit::OwnedStage,
) -> bool {
    false
}

impl AdmittedDirectWrite {
    pub(crate) fn begin<E: OwnedEffects + ?Sized>(
        effects: &E,
        scope: DirectWriteScope,
        prepared: file_commit::PreparedDirectWrite,
    ) -> Result<Self, DirectWriteStartError> {
        Self::begin_inner(effects, scope, prepared, None)
    }

    pub(crate) fn begin_validated<E: OwnedEffects + ?Sized>(
        effects: &E,
        scope: DirectWriteScope,
        prepared: file_commit::PreparedDirectWrite,
        validate: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<Self, DirectWriteStartError> {
        Self::begin_inner(effects, scope, prepared, Some(validate))
    }

    fn begin_inner<E: OwnedEffects + ?Sized>(
        effects: &E,
        scope: DirectWriteScope,
        prepared: file_commit::PreparedDirectWrite,
        mut validate: Option<&mut dyn FnMut() -> Result<(), String>>,
    ) -> Result<Self, DirectWriteStartError> {
        if prepared.path() != scope.path.as_path() {
            return Err(DirectWriteStartError::Admission(
                "The prepared direct write does not match its captured destination.".into(),
            ));
        }
        let mut prepared = Some(prepared);
        let mut opened = None;
        let admission = scope.admit(effects, &mut || {
            if let Some(validate) = validate.as_mut() {
                validate()?;
            }
            let prepared = prepared
                .take()
                .ok_or("The direct write was already admitted.")?;
            opened = Some(prepared.open());
            Ok(())
        });
        match (opened, admission) {
            (Some(Ok(descriptor)), Ok(())) => Ok(Self { scope, descriptor }),
            (Some(Ok(descriptor)), Err(error)) => {
                Err(DirectWriteStartError::OpenedButNotAdmitted {
                    error,
                    opened: Box::new(UnadmittedDirectWrite {
                        scope,
                        _descriptor: descriptor,
                    }),
                })
            }
            (Some(Err(error)), Ok(())) => Err(DirectWriteStartError::Io(error)),
            (Some(Err(error)), Err(admission)) => {
                Err(DirectWriteStartError::IoAndAdmission { error, admission })
            }
            (None, Err(error)) => Err(DirectWriteStartError::Admission(error)),
            (None, Ok(())) => Err(DirectWriteStartError::Admission(
                "The direct write was not admitted.".into(),
            )),
        }
    }

    /// Run on the worker after the short opening admission has been released.
    pub fn write_owned(
        self,
        bytes: Vec<u8>,
    ) -> Result<CompletedDirectWrite, (io::Error, PartialAdmittedDirectWrite)> {
        let Self { scope, descriptor } = self;
        match descriptor.write_owned(bytes) {
            file_commit::DirectOwnedWriteAttempt::Completed(outcome) => {
                Ok(CompletedDirectWrite { scope, outcome })
            }
            file_commit::DirectOwnedWriteAttempt::Partial { error, partial } => Err((
                error,
                PartialAdmittedDirectWrite(Box::new(PartialParts { scope, partial })),
            )),
        }
    }
}

impl PartialAdmittedDirectWrite {
    pub fn written(&self) -> usize {
        self.0.partial.written()
    }
    pub fn bytes(&self) -> &[u8] {
        self.0.partial.bytes()
    }
    /// A retry is re-admitted before writing any remaining bytes.
    pub fn resume<E: OwnedEffects + ?Sized>(
        self,
        effects: &E,
    ) -> Result<CompletedDirectWrite, DirectWriteResumeError> {
        let PartialParts { scope, partial } = *self.0;
        if let Err(error) = scope.admit(effects, &mut || Ok(())) {
            return Err(DirectWriteResumeError::Admission {
                error,
                partial: Self(Box::new(PartialParts { scope, partial })),
            });
        }
        match partial.resume() {
            file_commit::DirectOwnedWriteAttempt::Completed(outcome) => {
                Ok(CompletedDirectWrite { scope, outcome })
            }
            file_commit::DirectOwnedWriteAttempt::Partial { error, partial } => {
                Err(DirectWriteResumeError::Io {
                    error,
                    partial: Self(Box::new(PartialParts { scope, partial })),
                })
            }
        }
    }

    /// The caller may retain the exact descriptor and progress for its rescue path.
    pub fn into_rescue_parts(self) -> (file_commit::AbandonedDirectWrite, Vec<u8>, usize) {
        self.0.partial.into_parts()
    }
}

impl CompletedDirectWrite {
    /// Preserve a real write even if proof creation or final admission fails.
    pub fn finish<E: OwnedEffects + ?Sized>(self, effects: &E) -> DirectWriteCompletion {
        let Self { scope, outcome } = self;
        match outcome {
            file_commit::DirectOwnedWriteOutcome::WrittenWithoutReceipt { error, bytes } => {
                let admission = scope.admit(effects, &mut || Ok(()));
                DirectWriteCompletion::WrittenWithoutReceipt {
                    error,
                    bytes,
                    admission,
                }
            }
            file_commit::DirectOwnedWriteOutcome::Committed(snapshot) => {
                let prepared_renderer =
                    if matches!(&scope.origin, DirectWriteOrigin::Renderer { .. }) {
                        match RendererCommittedOutput::prepare(scope.clone()) {
                            Ok(value) => Some(value),
                            Err(error) => {
                                return DirectWriteCompletion::WrittenButNotAdmitted {
                                    error,
                                    snapshot,
                                };
                            }
                        }
                    } else {
                        None
                    };
                let native_scope = scope.native_output_scope();
                let mut native_selection = match &scope.origin {
                    DirectWriteOrigin::Native(value) => Some(value.clone()),
                    _ => None,
                };
                let mut publication = None;
                let admission = scope
                    .admit(effects, &mut || {
                        if publication.is_some() {
                            return Err("The direct write was already finalized.".into());
                        }
                        publication = Some(match &scope.origin {
                            DirectWriteOrigin::Native(_) => SavedPublication::Native(
                                NativeCommittedOutput::from_publication_admitted(
                                    native_selection
                                        .take()
                                        .ok_or("The native publication was already consumed.")?,
                                    snapshot.receipt().clone(),
                                    native_scope.clone(),
                                )?,
                            ),
                            DirectWriteOrigin::Renderer { .. } => {
                                let prepared = prepared_renderer
                                    .as_ref()
                                    .ok_or("The renderer publication was not prepared.")?;
                                prepared.commit_admitted(snapshot.receipt().clone())?;
                                SavedPublication::Renderer(prepared.clone())
                            }
                        });
                        Ok(())
                    })
                    .and_then(|()| {
                        publication.ok_or_else(|| "The direct write was not finalized.".to_owned())
                    });
                match admission {
                    Ok(publication) => DirectWriteCompletion::Committed {
                        publication,
                        snapshot,
                    },
                    Err(error) => DirectWriteCompletion::WrittenButNotAdmitted { error, snapshot },
                }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.scope.path
    }
}
