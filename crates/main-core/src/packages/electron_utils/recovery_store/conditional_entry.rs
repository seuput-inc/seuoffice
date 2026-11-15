//! Lossless cleanup of one captured recovery namespace entry.

use std::fs::File;
use std::io;

use file_commit::{OwnedStage, PendingStage, PendingStageLocation, PrivateDirectory};
use crate::node_fs::NodeError;
use super::super::atomic_write::WriteAdmission;

/// A displaced entry remains owned until its inverse succeeds. A caller must
/// retain this state and the original pair witness until the operation settles.
pub enum CapturedEntryCleanup {
    Pending(PendingStage),
    Conflict(PendingStage),
    Restore(PendingStage),
    Quarantined(OwnedStage),
    Deleted,
    DisplacedRestored,
    Restored,
    Unmoved,
}

impl CapturedEntryCleanup {
    /// The file must be the independently retained object selected by the caller.
    pub fn new(directory: &PrivateDirectory, parent: &File, name: &str, file: &File) -> io::Result<Self> {
        let captured = file.try_clone()?;
        #[cfg(unix)]
        let pending = {
            let name = std::ffi::CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid recovery entry name"))?;
            directory.quarantine_public(parent, &name, captured)?
        };
        #[cfg(windows)]
        let pending = directory.quarantine_public(parent, name, captured).map_err(|(error, _file)| error)?;
        Ok(Self::Pending(pending))
    }

    /// Each call performs at most one admitted namespace effect. A failure
    /// returns the exact still-owned phase for a later retry.
    pub fn step(self, admit: &mut WriteAdmission<'_>) -> Result<Self, (NodeError, Self)> {
        match self {
            Self::Pending(pending) => {
                let mut owned = Some(pending);
                let mut result = None;
                let admitted = admit(&mut || {
                    if let Some(pending) = owned.take() { result = Some(pending.quarantine()); }
                });
                match (admitted, result, owned) {
                    (Err(error), Some(Ok(stage)), _) => Err((error, Self::Quarantined(stage))),
                    (Err(error), Some(Err((operation, pending))), _) => Err((error, Self::pending_after_error(pending, &operation))),
                    (Err(error), None, Some(pending)) => Err((error, Self::Pending(pending))),
                    (Ok(()), Some(Ok(stage)), _) => Ok(Self::Quarantined(stage)),
                    (Ok(()), Some(Err((error, pending))), _) => {
                        let next = Self::pending_after_error(pending, &error);
                        Err((failure(error), next))
                    }
                    (Ok(()), None, Some(pending)) => Err((failure(io::Error::other("recovery quarantine was not admitted")), Self::Pending(pending))),
                    _ => unreachable!("the admission must preserve its single pending entry"),
                }
            }
            conflict @ Self::Conflict(_) => Err((failure(io::Error::other("the recovery entry was replaced")), conflict)),
            Self::Restore(pending) => restore(pending, admit),
            Self::Quarantined(stage) => {
                let mut owned = Some(stage);
                let mut result = None;
                let admitted = admit(&mut || {
                    if let Some(stage) = owned.take() { result = Some(stage.remove()); }
                });
                match (admitted, result, owned) {
                    (Err(error), Some(Ok(())), _) => Err((error, Self::Deleted)),
                    (Err(error), Some(Err((_operation, stage))), _) => Err((error, Self::Quarantined(stage))),
                    (Err(error), None, Some(stage)) => Err((error, Self::Quarantined(stage))),
                    (Ok(()), Some(Ok(())), _) => Ok(Self::Deleted),
                    (Ok(()), Some(Err((error, stage))), _) => Err((failure(error), Self::Quarantined(stage))),
                    (Ok(()), None, Some(stage)) => Err((failure(io::Error::other("recovery removal was not admitted")), Self::Quarantined(stage))),
                    _ => unreachable!("the admission must preserve its single quarantined entry"),
                }
            }
            terminal @ (Self::Deleted | Self::DisplacedRestored | Self::Restored | Self::Unmoved) => Ok(terminal),
        }
    }

    /// Reverse a prior quarantine without replacing a later public occupant.
    pub fn rollback(self, admit: &mut WriteAdmission<'_>) -> Result<Self, (NodeError, Self)> {
        match self {
            Self::Pending(_) | Self::Conflict(_) => Ok(Self::Unmoved),
            Self::Restore(pending) => restore(pending, admit),
            Self::Quarantined(stage) => {
                let mut owned = Some(stage);
                let mut result = None;
                let admitted = admit(&mut || {
                    if let Some(stage) = owned.take() { result = Some(stage.restore()); }
                });
                match (admitted, result, owned) {
                    (Err(error), Some(Ok(())), _) => Err((error, Self::Restored)),
                    (Err(error), Some(Err((_operation, stage))), _) => Err((error, Self::Quarantined(stage))),
                    (Err(error), None, Some(stage)) => Err((error, Self::Quarantined(stage))),
                    (Ok(()), Some(Ok(())), _) => Ok(Self::Restored),
                    (Ok(()), Some(Err((error, stage))), _) => Err((failure(error), Self::Quarantined(stage))),
                    (Ok(()), None, Some(stage)) => Err((failure(io::Error::other("recovery inverse was not admitted")), Self::Quarantined(stage))),
                    _ => unreachable!("the admission must preserve its single owned inverse"),
                }
            }
            Self::Deleted => Err((failure(io::Error::other("the recovery entry was already deleted")), Self::Deleted)),
            terminal @ (Self::DisplacedRestored | Self::Restored | Self::Unmoved) => Ok(terminal),
        }
    }

    fn pending_after_error(pending: PendingStage, error: &io::Error) -> Self {
        match pending.location() {
            PendingStageLocation::Public if error.kind() == io::ErrorKind::InvalidData => Self::Conflict(pending),
            PendingStageLocation::Public => Self::Pending(pending),
            PendingStageLocation::Quarantine => Self::Restore(pending),
        }
    }
}

fn failure(error: io::Error) -> NodeError { NodeError { code: None, message: error.to_string() } }

#[cfg(unix)]
fn restore(pending: PendingStage, admit: &mut WriteAdmission<'_>) -> Result<CapturedEntryCleanup, (NodeError, CapturedEntryCleanup)> {
    let mut owned = Some(pending);
    let mut result = None;
    let admitted = admit(&mut || {
        if let Some(pending) = owned.take() { result = Some(pending.restore()); }
    });
    match (admitted, result, owned) {
        (Err(error), Some(Ok(())), _) => Err((error, CapturedEntryCleanup::DisplacedRestored)),
        (Err(error), Some(Err((_operation, pending))), _) => Err((error, CapturedEntryCleanup::Restore(pending))),
        (Err(error), None, Some(pending)) => Err((error, CapturedEntryCleanup::Restore(pending))),
        (Ok(()), Some(Ok(())), _) => Ok(CapturedEntryCleanup::DisplacedRestored),
        (Ok(()), Some(Err((error, pending))), _) => Err((failure(error), CapturedEntryCleanup::Restore(pending))),
        (Ok(()), None, Some(pending)) => Err((failure(io::Error::other("recovery inverse was not admitted")), CapturedEntryCleanup::Restore(pending))),
        _ => unreachable!("the admission must preserve its single inverse"),
    }
}

#[cfg(windows)]
fn restore(pending: PendingStage, _admit: &mut WriteAdmission<'_>) -> Result<CapturedEntryCleanup, (NodeError, CapturedEntryCleanup)> {
    Err((failure(io::Error::other("unexpected Windows quarantine inverse")), CapturedEntryCleanup::Restore(pending)))
}
