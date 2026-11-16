//! A generated output staged in a private directory beside its destination and
//! published by the shell's admitted staged-write services. Staging runs on a
//! worker and touches no public name; only the publication replaces or creates
//! the destination, and it runs on the core turn under the save's authority.
//!
//! A stage that cannot be removed is never dropped: it moves to a process-wide
//! ledger that the next staging attempt retries, so the inverse stays owned.

use std::io;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use file_commit::{
    CommittedWriteSnapshot, OwnedStage, OwnedWrittenCommit, PendingStage, PrivateDirectory,
    PublicationMode,
};
use shell_contract::IpcError;
use shell_contract::ipc::{NativeSavedInput, SavedPublication, StagedWriteError};

use crate::value::thrown;

enum Retained {
    Pending(PendingStage),
    Owned(OwnedStage),
    Directory(PrivateDirectory),
}

static LEDGER: Mutex<Vec<Retained>> = Mutex::new(Vec::new());

fn ledger() -> MutexGuard<'static, Vec<Retained>> {
    LEDGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// Removes the stage and then its private directory, keeping whatever piece
/// could not be removed so the caller can retry exactly that.
fn settle(mut current: Retained) -> Result<(), (io::Error, Retained)> {
    loop {
        current = match current {
            Retained::Directory(directory) => {
                return match directory.remove() {
                    Err(error) if !gone(&error) => Err((error, Retained::Directory(directory))),
                    _ => Ok(()),
                };
            }
            Retained::Owned(stage) => {
                let directory = stage.directory();
                match stage.remove() {
                    Err((error, stage)) if !gone(&error) => {
                        return Err((error, Retained::Owned(stage)));
                    }
                    _ => Retained::Directory(directory),
                }
            }
            Retained::Pending(stage) => match stage.quarantine() {
                Ok(owned) => Retained::Owned(owned),
                Err((error, stage)) => return Err((error, Retained::Pending(stage))),
            },
        };
    }
}

/// Settles `retained`; a failure parks the remainder in the ledger for retry.
fn abandon(retained: Retained) -> io::Result<()> {
    match settle(retained) {
        Ok(()) => Ok(()),
        Err((error, remainder)) => {
            ledger().push(remainder);
            Err(error)
        }
    }
}

fn retry_retained() {
    let parked = std::mem::take(&mut *ledger());
    for retained in parked {
        // a stage that still cannot be removed is parked again by `abandon`
        let _ = abandon(retained);
    }
}

fn failure(error: String, cleanup: io::Result<()>) -> IpcError {
    match cleanup {
        Ok(()) => thrown(error),
        Err(cleanup) => thrown(format!(
            "{error}; the staged output could not be removed and is kept for retry: {cleanup}"
        )),
    }
}

pub(crate) struct StagedOutput {
    directory: PrivateDirectory,
    commit: Option<OwnedWrittenCommit>,
    settled: bool,
}

#[cfg(unix)]
fn create_pending(directory: &PrivateDirectory, name: &str) -> Result<PendingStage, IpcError> {
    directory
        .create_stage_for(name, 0o666)
        .map_err(|error| thrown(error.to_string()))
}

#[cfg(windows)]
fn create_pending(directory: &PrivateDirectory, name: &str) -> Result<PendingStage, IpcError> {
    match directory
        .create_stage_for(name, 0o666)
        .map_err(|error| thrown(error.to_string()))?
    {
        file_commit::CreatedStage::Movable(stage) => Ok(stage),
        file_commit::CreatedStage::RenameUnavailable(stage) => {
            let error = format!("the staged output cannot be renamed: {}", stage.error());
            Err(failure(error, abandon(Retained::Pending(stage.retry()))))
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
        retry_retained();
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
            Err(error) => {
                let cleanup = abandon(Retained::Directory(directory));
                return Err(failure(error.message, cleanup));
            }
        };
        let pending = match pending.write_owned(bytes) {
            Ok(pending) => pending,
            Err((error, stage, _)) => {
                let cleanup = abandon(Retained::Pending(stage));
                return Err(failure(error.to_string(), cleanup));
            }
        };
        let written = match pending.quarantine() {
            Ok(written) => written,
            Err((error, pending)) => {
                let (stage, _) = pending.into_parts();
                let cleanup = abandon(Retained::Pending(stage));
                return Err(failure(error.to_string(), cleanup));
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
                let cleanup = abandon(Retained::Owned(stage));
                return Err(failure(error.to_string(), cleanup));
            }
        }
        match prepare_commit(written, &parent, name, path, mode) {
            Ok(commit) => Ok(Self {
                directory,
                commit: Some(commit),
                settled: false,
            }),
            Err((error, written)) => {
                let (stage, _) = written.into_parts();
                let cleanup = abandon(Retained::Owned(stage));
                Err(failure(error.to_string(), cleanup))
            }
        }
    }

    pub(crate) fn take(&mut self) -> Result<OwnedWrittenCommit, IpcError> {
        self.commit
            .take()
            .ok_or_else(|| thrown("The staged output was already published"))
    }

    /// Returns a commit that was taken but never handed to a publication.
    pub(crate) fn restore(&mut self, commit: OwnedWrittenCommit) {
        self.commit = Some(commit);
    }

    /// Runs the namespace operation itself, for a gate that has no staged
    /// publication service. Call only inside the caller's admission; no
    /// `SavedPublication` results, so it is for outputs that are never reopened.
    pub(crate) fn publish_in_gate(&mut self) -> Result<(), IpcError> {
        let commit = self.take()?;
        match commit.publish() {
            Ok(_) => Ok(()),
            Err(failed) => {
                let (error, commit) = *failed;
                self.commit = Some(commit);
                Err(thrown(error.to_string()))
            }
        }
    }

    /// Removes whatever of the stage remains, once; a piece that cannot be
    /// removed stays in the ledger and the error is returned.
    fn discard(&mut self) -> io::Result<()> {
        self.settled = true;
        let retained = match self.commit.take() {
            Some(commit) => Retained::Owned(commit.into_parts().0),
            None => Retained::Directory(self.directory.clone()),
        };
        abandon(retained)
    }

    /// Settles a publication attempt. A refused attempt returns its exact stage,
    /// which is removed with the original error reported next to any cleanup
    /// error. A committed one removes the private directory; if that fails the
    /// directory stays in the ledger and the published result still stands.
    pub(crate) fn conclude(
        mut self,
        result: Result<(SavedPublication, CommittedWriteSnapshot), StagedWriteError>,
    ) -> Result<(SavedPublication, NativeSavedInput), IpcError> {
        match result {
            Ok((publication, snapshot)) => {
                let input = NativeSavedInput::from_written_snapshot(publication.clone(), snapshot)
                    .map_err(thrown)?;
                // the output is published; a directory left behind is retried later
                self.discard().ok();
                Ok((publication, input))
            }
            Err(StagedWriteError::Admission { error, stage }) => {
                self.commit = Some(*stage);
                let cleanup = self.discard();
                Err(failure(error, cleanup))
            }
            Err(StagedWriteError::Io { error, stage }) => {
                self.commit = Some(*stage);
                let cleanup = self.discard();
                Err(failure(error.to_string(), cleanup))
            }
            Err(StagedWriteError::CommittedButNotAdmitted { error, .. }) => {
                let cleanup = self.discard();
                Err(failure(error, cleanup))
            }
        }
    }
}

impl Drop for StagedOutput {
    /// An abandoned stage is removed; one that cannot be is parked for retry.
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.discard();
        }
    }
}
