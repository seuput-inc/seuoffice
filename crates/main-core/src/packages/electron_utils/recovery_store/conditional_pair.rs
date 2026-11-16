//! Retained two-entry recovery cleanup. Both entries are quarantined before removal.

use std::sync::Arc;

use super::super::atomic_write::WriteAdmission;
use super::{RecoveryPinnedSnapshot, conditional_entry::CapturedEntryCleanup};
use crate::node_fs::NodeError;
use file_commit::PrivateDirectory;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairCleanupOutcome {
    Deleted,
    Superseded,
}

#[derive(Clone, Copy)]
enum Phase {
    Copy,
    Metadata,
    DeleteCopy,
    DeleteMetadata,
    RollbackMetadata,
    RollbackCopy,
    Directory(PairCleanupOutcome),
    Done(PairCleanupOutcome),
}

/// Keep this value across failures. It retains the original snapshot and every
/// moved entry until deletion or the full inverse actually completes.
pub struct ConditionalPairCleanup {
    _snapshot: Arc<RecoveryPinnedSnapshot>,
    directory: PrivateDirectory,
    copy: Option<CapturedEntryCleanup>,
    metadata: Option<CapturedEntryCleanup>,
    phase: Phase,
}

fn error(message: impl Into<String>) -> NodeError {
    NodeError {
        code: None,
        message: message.into(),
    }
}

impl ConditionalPairCleanup {
    /// The caller must already have admitted the exact captured pair and own
    /// its recovery FIFO turn. This function alone does not establish either.
    pub fn new(
        snapshot: Arc<RecoveryPinnedSnapshot>,
        directory: &PrivateDirectory,
    ) -> Result<Self, NodeError> {
        snapshot.exact_metadata()?;
        let parent = snapshot
            .parent_handle()
            .ok_or_else(|| error("the captured recovery directory is unavailable"))?;
        let name = std::path::Path::new(&snapshot.path)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| error("the recovery entry name is unavailable"))?;
        let copy = CapturedEntryCleanup::new(directory, parent, name, snapshot.copy_handle())
            .map_err(|e| error(e.to_string()))?;
        let metadata = match snapshot.metadata_handle() {
            Some(file) => Some(
                CapturedEntryCleanup::new(directory, parent, &format!("{name}.meta.json"), file)
                    .map_err(|e| error(e.to_string()))?,
            ),
            None => None,
        };
        Ok(Self {
            _snapshot: snapshot,
            directory: directory.clone(),
            copy: Some(copy),
            metadata,
            phase: Phase::Copy,
        })
    }

    /// Worker entry using an owned native admission. The returned value retains
    /// every unfinished entry even when the admission fails after an effect.
    pub fn step_owned(
        mut self,
        admit: &super::super::atomic_write::owned::Admission,
    ) -> Result<(Self, Option<PairCleanupOutcome>), Box<(NodeError, Self)>> {
        use super::super::atomic_write::owned::admitted;
        if matches!(self.phase, Phase::Copy) && quarantined(&self.copy) {
            self.phase = Phase::Metadata;
        }
        if matches!(self.phase, Phase::Metadata) && self.metadata.is_none() {
            if let Err(problem) = admitted(admit, || ()) {
                return Err(Box::new((problem, self)));
            }
            let observed = (|| {
                let parent = self
                    ._snapshot
                    .parent_handle()
                    .ok_or_else(|| error("the captured recovery directory is unavailable"))?;
                let name = std::path::Path::new(&self._snapshot.path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| error("the recovery entry name is unavailable"))?;
                Ok::<_, NodeError>(file_commit::open_pinned_file(
                    parent,
                    &format!("{name}.meta.json"),
                ))
            })();
            if let Err(problem) = admitted(admit, || ()) {
                return Err(Box::new((problem, self)));
            }
            match observed {
                Err(problem) => return Err(Box::new((problem, self))),
                Ok(Ok(_)) => {
                    self.phase = Phase::RollbackCopy;
                    return Err(Box::new((error("the recovery metadata was replaced"), self)));
                }
                Ok(Err(problem)) if problem.kind() == std::io::ErrorKind::NotFound => {
                    self.phase = Phase::DeleteCopy;
                    return Ok((self, None));
                }
                Ok(Err(problem)) => return Err(Box::new((error(problem.to_string()), self))),
            }
        }
        let state = Arc::new(std::sync::Mutex::new((Some(self), None)));
        let effect_state = state.clone();
        let result = admitted(admit, move || {
            let mut state = effect_state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let next = state
                .0
                .as_mut()
                .expect("cleanup is retained")
                .step(&mut |effect| {
                    effect();
                    Ok(())
                });
            state.1 = Some(next);
        });
        let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
        let cleanup = state.0.take().expect("cleanup is retained");
        match result {
            Err(problem) => Err(Box::new((problem, cleanup))),
            Ok(()) => match state.1.take() {
                Some(Ok(outcome)) => Ok((cleanup, outcome)),
                Some(Err(problem)) => Err(Box::new((problem, cleanup))),
                None => Err(Box::new((error("recovery cleanup was not admitted"), cleanup))),
            },
        }
    }

    /// Performs at most one admitted namespace operation per call. Errors do
    /// not consume the cleanup and callers must retain it for retry.
    pub fn step(
        &mut self,
        admit: &mut WriteAdmission<'_>,
    ) -> Result<Option<PairCleanupOutcome>, NodeError> {
        loop {
            match self.phase {
                Phase::Copy => {
                    if quarantined(&self.copy) {
                        self.phase = Phase::Metadata;
                        continue;
                    }
                    let result = step_entry(&mut self.copy, admit, false);
                    if needs_inverse(&self.copy) {
                        self.phase = Phase::RollbackMetadata;
                    }
                    return result.map(|()| None);
                }
                Phase::Metadata => {
                    if self.metadata.is_none() {
                        let parent = self._snapshot.parent_handle().ok_or_else(|| {
                            error("the captured recovery directory is unavailable")
                        })?;
                        let name = std::path::Path::new(&self._snapshot.path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .ok_or_else(|| error("the recovery entry name is unavailable"))?;
                        let name = format!("{name}.meta.json");
                        admit(&mut || {})?;
                        let observed = file_commit::open_pinned_file(parent, &name);
                        admit(&mut || {})?;
                        match observed {
                            Ok(_) => {
                                self.phase = Phase::RollbackCopy;
                                return Err(error("the recovery metadata was replaced"));
                            }
                            Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => {
                                self.phase = Phase::DeleteCopy;
                                return Ok(None);
                            }
                            Err(problem) => return Err(error(problem.to_string())),
                        }
                    }
                    if quarantined(&self.metadata) {
                        self.phase = Phase::DeleteCopy;
                        continue;
                    }
                    let result = step_entry(&mut self.metadata, admit, false);
                    if needs_inverse(&self.metadata) {
                        self.phase = Phase::RollbackMetadata;
                    }
                    return result.map(|()| None);
                }
                Phase::DeleteCopy => {
                    if deleted(&self.copy) {
                        self.phase = Phase::DeleteMetadata;
                        continue;
                    }
                    return step_entry(&mut self.copy, admit, false).map(|()| None);
                }
                Phase::DeleteMetadata => {
                    if self.metadata.is_none() || deleted(&self.metadata) {
                        self.phase = Phase::Directory(PairCleanupOutcome::Deleted);
                        continue;
                    }
                    return step_entry(&mut self.metadata, admit, false).map(|()| None);
                }
                Phase::RollbackMetadata => {
                    if self.metadata.is_none() || restored(&self.metadata) {
                        self.phase = Phase::RollbackCopy;
                        continue;
                    }
                    return step_entry(&mut self.metadata, admit, true).map(|()| None);
                }
                Phase::RollbackCopy => {
                    if restored(&self.copy) {
                        self.phase = Phase::Directory(PairCleanupOutcome::Superseded);
                        continue;
                    }
                    return step_entry(&mut self.copy, admit, true).map(|()| None);
                }
                Phase::Directory(outcome) => {
                    let mut result = None;
                    let admitted = admit(&mut || {
                        result = Some(self.directory.remove());
                    });
                    if matches!(&result, Some(Ok(()))) {
                        self.phase = Phase::Done(outcome);
                    }
                    admitted?;
                    result
                        .ok_or_else(|| {
                            error("recovery staging directory removal was not admitted")
                        })?
                        .map_err(|e| error(e.to_string()))?;
                    return Ok(Some(outcome));
                }
                Phase::Done(outcome) => return Ok(Some(outcome)),
            }
        }
    }
}

fn quarantined(entry: &Option<CapturedEntryCleanup>) -> bool {
    matches!(entry, Some(CapturedEntryCleanup::Quarantined(_)))
}
fn deleted(entry: &Option<CapturedEntryCleanup>) -> bool {
    matches!(entry, Some(CapturedEntryCleanup::Deleted))
}
fn restored(entry: &Option<CapturedEntryCleanup>) -> bool {
    matches!(
        entry,
        Some(
            CapturedEntryCleanup::Restored
                | CapturedEntryCleanup::DisplacedRestored
                | CapturedEntryCleanup::Unmoved
        )
    )
}
fn needs_inverse(entry: &Option<CapturedEntryCleanup>) -> bool {
    matches!(
        entry,
        Some(CapturedEntryCleanup::Restore(_) | CapturedEntryCleanup::Conflict(_))
    )
}

fn step_entry(
    entry: &mut Option<CapturedEntryCleanup>,
    admit: &mut WriteAdmission<'_>,
    rollback: bool,
) -> Result<(), NodeError> {
    let current = entry
        .take()
        .ok_or_else(|| error("the recovery cleanup entry is unavailable"))?;
    let result = if rollback {
        current.rollback(admit)
    } else {
        current.step(admit)
    };
    match result {
        Ok(next) => {
            *entry = Some(next);
            Ok(())
        }
        Err((error, next)) => {
            *entry = Some(next);
            Err(error)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::packages::electron_utils::recovery_store::{
        RecoveryFileIdentity, RecoveryIdentity, RecoveryPinnedRead, recovery_copy_path,
    };

    #[test]
    fn replacement_is_restored_instead_of_deleted() {
        let root = std::env::temp_dir().join(format!("recovery-pair-{}", jscompat::random_uuid()));
        let identity = RecoveryIdentity::File(RecoveryFileIdentity {
            kind: "docs".into(),
            file_path: "/document.docx".into(),
        });
        let path = recovery_copy_path(root.to_str().unwrap(), &identity).unwrap();
        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(&path, b"original").unwrap();
        let snapshot = Arc::new(
            RecoveryPinnedRead::open_saved(root.to_str().unwrap(), &identity)
                .unwrap()
                .read()
                .unwrap(),
        );
        let original = format!("{path}.old");
        std::fs::rename(&path, &original).unwrap();
        std::fs::write(&path, b"successor").unwrap();
        let directory = PrivateDirectory::create(snapshot.parent_handle().unwrap()).unwrap();
        let mut cleanup = ConditionalPairCleanup::new(snapshot.clone(), &directory).unwrap();
        let mut admit = |effect: &mut dyn FnMut()| {
            effect();
            Ok(())
        };
        assert!(cleanup.step(&mut admit).is_err());
        std::fs::write(&path, b"later occupant").unwrap();
        assert!(cleanup.step(&mut admit).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"later occupant");
        std::fs::remove_file(&path).unwrap();
        let mut outcome = None;
        for _ in 0..8 {
            if let Some(done) = cleanup.step(&mut admit).unwrap() {
                outcome = Some(done);
                break;
            }
        }
        assert_eq!(outcome, Some(PairCleanupOutcome::Superseded));
        assert_eq!(std::fs::read(&path).unwrap(), b"successor");
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        assert_eq!(snapshot.bytes, b"original");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_replacement_reverses_the_copy_quarantine() {
        let root = std::env::temp_dir().join(format!("recovery-pair-{}", jscompat::random_uuid()));
        let identity = RecoveryIdentity::File(RecoveryFileIdentity {
            kind: "docs".into(),
            file_path: "/document.docx".into(),
        });
        let path = recovery_copy_path(root.to_str().unwrap(), &identity).unwrap();
        let metadata = format!("{path}.meta.json");
        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(&path, b"original").unwrap();
        std::fs::write(&metadata, b"original metadata").unwrap();
        let snapshot = Arc::new(
            RecoveryPinnedRead::open_saved(root.to_str().unwrap(), &identity)
                .unwrap()
                .read()
                .unwrap(),
        );
        std::fs::rename(&metadata, format!("{metadata}.old")).unwrap();
        std::fs::write(&metadata, b"successor metadata").unwrap();
        let directory = PrivateDirectory::create(snapshot.parent_handle().unwrap()).unwrap();
        let mut cleanup = ConditionalPairCleanup::new(snapshot, &directory).unwrap();
        let mut admit = |effect: &mut dyn FnMut()| {
            effect();
            Ok(())
        };
        assert!(cleanup.step(&mut admit).unwrap().is_none());
        assert!(cleanup.step(&mut admit).is_err());
        let mut outcome = None;
        for _ in 0..8 {
            if let Some(done) = cleanup.step(&mut admit).unwrap() {
                outcome = Some(done);
                break;
            }
        }
        assert_eq!(outcome, Some(PairCleanupOutcome::Superseded));
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read(&metadata).unwrap(), b"successor metadata");
        assert_eq!(
            std::fs::read(format!("{metadata}.old")).unwrap(),
            b"original metadata"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admitted_effect_error_retains_the_exact_cleanup_for_retry() {
        use crate::packages::electron_utils::atomic_write::owned::Admission;

        let root = std::env::temp_dir().join(format!("recovery-pair-{}", jscompat::random_uuid()));
        let identity = RecoveryIdentity::File(RecoveryFileIdentity {
            kind: "docs".into(),
            file_path: "/document.docx".into(),
        });
        let path = recovery_copy_path(root.to_str().unwrap(), &identity).unwrap();
        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(&path, b"original").unwrap();
        let snapshot = Arc::new(
            RecoveryPinnedRead::open_saved(root.to_str().unwrap(), &identity)
                .unwrap()
                .read()
                .unwrap(),
        );
        let directory = PrivateDirectory::create(snapshot.parent_handle().unwrap()).unwrap();
        let cleanup = ConditionalPairCleanup::new(snapshot, &directory).unwrap();
        let after_effect: Admission = Arc::new(|effect| {
            let _ = effect();
            Err(NodeError {
                code: None,
                message: "after effect".into(),
            })
        });
        let (_, mut cleanup) = match cleanup.step_owned(&after_effect) {
            Err(value) => *value,
            Ok(_) => panic!("the admission error was lost"),
        };
        let admitted: Admission = Arc::new(|effect| Ok(effect()));
        let mut outcome = None;
        for _ in 0..8 {
            match cleanup.step_owned(&admitted) {
                Ok((next, result)) => {
                    cleanup = next;
                    if result.is_some() {
                        outcome = result;
                        break;
                    }
                }
                Err(failure) => panic!("{:?}", failure.0),
            }
        }
        assert_eq!(outcome, Some(PairCleanupOutcome::Deleted));
        assert!(!std::path::Path::new(&path).exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
