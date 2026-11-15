use std::sync::{Arc, Mutex};

use file_commit::{OwnedStage, PendingStage, PrivateDirectory};

enum State {
    Pending(PendingStage),
    Stage(OwnedStage),
    Directory(PrivateDirectory),
    Done,
}

/// One transaction's retained cleanup. Clones share the same single operation.
#[derive(Clone)]
pub struct ProjectCleanup(Arc<Mutex<State>>);

impl std::fmt::Debug for ProjectCleanup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ProjectCleanup")
            .field(&Arc::as_ptr(&self.0))
            .finish()
    }
}
impl PartialEq for ProjectCleanup {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl ProjectCleanup {
    pub(super) fn pending(stage: PendingStage) -> Self {
        Self(Arc::new(Mutex::new(State::Pending(stage))))
    }
    pub(super) fn stage(stage: OwnedStage) -> Self {
        Self(Arc::new(Mutex::new(State::Stage(stage))))
    }
    pub(super) fn directory(directory: PrivateDirectory) -> Self {
        Self(Arc::new(Mutex::new(State::Directory(directory))))
    }

    /// Retry only this transaction's bounded inverse or private-stage cleanup.
    /// Failure retains the exact operation for a later retry.
    pub fn retry(&self) -> std::io::Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("project cleanup lock poisoned"))?;
        loop {
            match std::mem::replace(&mut *state, State::Done) {
                State::Done => return Ok(()),
                State::Directory(directory) => match directory.remove() {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        *state = State::Directory(directory);
                        return Err(error);
                    }
                },
                State::Stage(stage) => {
                    let directory = stage.directory();
                    match stage.remove() {
                        Ok(()) => *state = State::Directory(directory),
                        Err((error, stage)) => {
                            *state = State::Stage(stage);
                            return Err(error);
                        }
                    }
                }
                State::Pending(pending) => {
                    let directory = pending.directory();
                    match pending.quarantine() {
                        Ok(stage) => *state = State::Stage(stage),
                        Err((error, pending))
                            if pending.is_quarantined()
                                && error.kind() == std::io::ErrorKind::InvalidData =>
                        {
                            match pending.restore() {
                                Ok(()) => *state = State::Directory(directory),
                                Err((error, pending)) => {
                                    *state = State::Pending(pending);
                                    return Err(error);
                                }
                            }
                        }
                        Err((error, pending)) => {
                            *state = State::Pending(pending);
                            return Err(error);
                        }
                    }
                }
            }
        }
    }
}
