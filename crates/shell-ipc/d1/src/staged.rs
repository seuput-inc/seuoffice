//! A generated output staged in a private directory beside its destination and
//! published by the shell's admitted staged-write services. Staging runs on a
//! worker and touches no public name; only the publication replaces or creates
//! the destination, and it runs on the core turn under the save's authority.

use std::io;
use std::path::Path;

use file_commit::{
    CommittedWriteSnapshot, OwnedStage, OwnedWrittenCommit, PendingStage, PrivateDirectory,
    PublicationMode,
};
use shell_contract::IpcError;
use shell_contract::ipc::{NativeSavedInput, SavedPublication, StagedWriteError};

use crate::value::thrown;

pub(crate) struct StagedOutput {
    directory: PrivateDirectory,
    commit: Option<OwnedWrittenCommit>,
}

fn discard_pending(stage: PendingStage) -> io::Result<()> {
    let directory = stage.directory();
    let owned = stage.quarantine().map_err(|(error, _)| error)?;
    owned.remove().map_err(|(error, _)| error)?;
    directory.remove()
}

fn discard_owned(stage: OwnedStage) -> io::Result<()> {
    let directory = stage.directory();
    stage.remove().map_err(|(error, _)| error)?;
    directory.remove()
}

fn failure(error: io::Error, cleanup: io::Result<()>) -> IpcError {
    match cleanup {
        Ok(()) => thrown(error.to_string()),
        Err(cleanup) => thrown(format!(
            "{error}; the staged output could not be removed: {cleanup}"
        )),
    }
}

#[cfg(unix)]
fn create_pending(directory: &PrivateDirectory, name: &str) -> io::Result<PendingStage> {
    directory.create_stage_for(name, 0o666)
}

#[cfg(windows)]
fn create_pending(directory: &PrivateDirectory, name: &str) -> io::Result<PendingStage> {
    match directory.create_stage_for(name, 0o666)? {
        file_commit::CreatedStage::Movable(stage) => Ok(stage),
        file_commit::CreatedStage::RenameUnavailable(stage) => {
            let error = io::Error::new(
                stage.error().kind(),
                format!("the staged output cannot be renamed: {}", stage.error()),
            );
            match discard_pending(stage.retry()) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io::Error::new(
                    error.kind(),
                    format!("{error}; the staged output could not be removed: {cleanup}"),
                )),
            }
        }
    }
}

type Prepared = Result<OwnedWrittenCommit, (io::Error, file_commit::OwnedWrittenStage)>;

#[cfg(unix)]
fn prepare_commit(
    written: file_commit::OwnedWrittenStage,
    parent: &std::fs::File,
    name: &str,
    path: &Path,
    mode: PublicationMode,
) -> Prepared {
    match std::ffi::CString::new(name) {
        Ok(name) => written.prepare(parent, &name, path, mode),
        Err(error) => Err((io::Error::new(io::ErrorKind::InvalidInput, error), written)),
    }
}

#[cfg(windows)]
fn prepare_commit(
    written: file_commit::OwnedWrittenStage,
    parent: &std::fs::File,
    name: &str,
    path: &Path,
    mode: PublicationMode,
) -> Prepared {
    written.prepare(parent, name, path, mode)
}

impl StagedOutput {
    /// Writes `bytes` to a private stage beside `path` and prepares its
    /// publication. `Exclusive` creates a new name and never replaces one;
    /// `Replace` is only for a destination the user already approved.
    pub(crate) fn prepare(
        path: &Path,
        bytes: Vec<u8>,
        mode: PublicationMode,
    ) -> Result<Self, IpcError> {
        if !path.is_absolute() {
            return Err(thrown("The output path must be absolute"));
        }
        let parent_path = path
            .parent()
            .ok_or_else(|| thrown("The output path has no parent directory"))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| thrown("The output file name is not valid Unicode"))?;
        let parent = file_commit::open_output_parent(parent_path)
            .map_err(|error| thrown(error.to_string()))?;
        let directory =
            PrivateDirectory::create(&parent).map_err(|error| thrown(error.to_string()))?;
        let pending = match create_pending(&directory, name) {
            Ok(pending) => pending,
            Err(error) => return Err(failure(error, directory.remove())),
        };
        let pending = match pending.write_owned(bytes) {
            Ok(pending) => pending,
            Err((error, stage, _)) => return Err(failure(error, discard_pending(stage))),
        };
        let written = match pending.quarantine() {
            Ok(written) => written,
            Err((error, pending)) => {
                let (stage, _) = pending.into_parts();
                return Err(failure(error, discard_pending(stage)));
            }
        };
        if let Err(error) = written.sync_all() {
            let tolerated = matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Unsupported
                    | io::ErrorKind::InvalidInput
            );
            if !tolerated {
                let (stage, _) = written.into_parts();
                return Err(failure(error, discard_owned(stage)));
            }
        }
        match prepare_commit(written, &parent, name, path, mode) {
            Ok(commit) => Ok(Self {
                directory,
                commit: Some(commit),
            }),
            Err((error, written)) => {
                let (stage, _) = written.into_parts();
                Err(failure(error, discard_owned(stage)))
            }
        }
    }

    /// Returns a commit that was taken but never handed to a publication.
    pub(crate) fn restore(&mut self, commit: OwnedWrittenCommit) {
        self.commit = Some(commit);
    }

    pub(crate) fn take(&mut self) -> Result<OwnedWrittenCommit, IpcError> {
        self.commit
            .take()
            .ok_or_else(|| thrown("The staged output was already published"))
    }

    /// Settles a publication attempt: a refused attempt returns its exact stage
    /// to be discarded, a committed one removes the private directory.
    pub(crate) fn conclude(
        mut self,
        result: Result<(SavedPublication, CommittedWriteSnapshot), StagedWriteError>,
    ) -> Result<(SavedPublication, NativeSavedInput), IpcError> {
        match result {
            Ok((publication, snapshot)) => {
                let input = NativeSavedInput::from_written_snapshot(publication.clone(), snapshot)
                    .map_err(thrown)?;
                self.directory.remove().map_err(|error| {
                    thrown(format!(
                        "The output was published but its staging directory remains: {error}"
                    ))
                })?;
                Ok((publication, input))
            }
            Err(StagedWriteError::Admission { error, stage }) => {
                self.commit = Some(*stage);
                Err(thrown(error))
            }
            Err(StagedWriteError::Io { error, stage }) => {
                self.commit = Some(*stage);
                Err(thrown(error.to_string()))
            }
            Err(StagedWriteError::CommittedButNotAdmitted { error, .. }) => Err(thrown(error)),
        }
    }
}

impl Drop for StagedOutput {
    /// An unpublished stage is removed with its private directory. A failure
    /// here has no caller left to report to; the stage keeps its own unique name.
    fn drop(&mut self) {
        if let Some(commit) = self.commit.take() {
            let (stage, _) = commit.into_parts();
            let _ = discard_owned(stage);
        }
    }
}
