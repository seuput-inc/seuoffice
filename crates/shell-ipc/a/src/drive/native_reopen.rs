use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::{
    NativeDriveMoveReceipt, NativeDrivePendingOutcome, NativeDrivePendingReceipt,
    NativeRequestOwner, SavedPublication,
};
use shell_contract::runtime::{CancelToken, Services};

use super::files::{CacheEntry, CacheGeneration, DriveFiles, native_reopen::Transaction};

#[derive(Clone)]
pub(super) struct Authority {
    pub publication: SavedPublication,
    pub participants: Vec<NativeRequestOwner>,
    pub controls: Vec<CancelToken>,
    pub dispatcher: Vec<(
        NativeRequestOwner,
        shell_contract::ipc::DispatcherCancellation,
    )>,
    pub effects: OwnedEffectsHost,
}

impl Authority {
    pub fn admit(
        &self,
        additional_owners: &[NativeRequestOwner],
        additional_controls: &[&CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut owners = self.participants.clone();
        owners.extend_from_slice(additional_owners);
        let mut controls: Vec<&CancelToken> = self.controls[2..].iter().collect();
        controls.extend_from_slice(additional_controls);
        self.publication.with_effect(
            self.effects.as_ref(),
            &owners,
            &self.controls[0],
            &self.controls[1],
            &controls,
            effect,
        )
    }

    #[allow(clippy::type_complexity)]
    pub fn with_prepared(
        &self,
        additional_owners: &[NativeRequestOwner],
        additional_controls: &[&CancelToken],
        use_plan: &mut dyn FnMut(
            &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut owners = self.participants.clone();
        owners.extend_from_slice(additional_owners);
        let mut controls: Vec<&CancelToken> = self.controls[2..].iter().collect();
        controls.extend_from_slice(additional_controls);
        self.effects.with_prepared_saved_publication(
            &self.publication,
            &owners,
            &self.controls[0],
            &self.controls[1],
            &controls,
            use_plan,
        )
    }

    pub fn origin(&self) -> &NativeRequestOwner {
        self.publication.source_owner()
    }

    fn losses(&self) -> Result<Vec<CancelToken>, String> {
        fn add_owner(
            losses: &mut Vec<CancelToken>,
            owner: &NativeRequestOwner,
        ) -> Result<(), String> {
            losses.push(owner.loss_token().ok_or("The Drive owner is unavailable")?);
            losses.push(
                owner
                    .context
                    .renderer()
                    .ok_or("The Drive renderer is unavailable")?
                    .loss_token(),
            );
            Ok(())
        }
        let mut losses = self.controls.clone();
        for owner in &self.participants {
            add_owner(&mut losses, owner)?;
        }
        losses.extend(self.publication.loss_tokens()?);
        Ok(losses)
    }
}

pub(super) struct Committed {
    pub authority: Authority,
    pub files: Arc<DriveFiles>,
    pub entry: CacheEntry,
    pub generation: Arc<CacheGeneration>,
    pub moved: PathBuf,
    pub moved_commit: file_commit::FileCommitReceipt,
    pub original_commit: file_commit::FileCommitReceipt,
    pub fallback: Option<std::fs::File>,
    pub transaction: Arc<Mutex<Transaction>>,
    pub warning: Arc<Mutex<Option<String>>>,
    pub services: Services,
}

pub struct DriveReopenSnapshot {
    files: Arc<DriveFiles>,
    entry: CacheEntry,
    generation: Arc<CacheGeneration>,
    moved_commit: file_commit::FileCommitReceipt,
    original_commit: file_commit::FileCommitReceipt,
    has_fallback: bool,
    participants: Vec<NativeRequestOwner>,
    controls: Vec<CancelToken>,
    dispatcher: Vec<(
        NativeRequestOwner,
        shell_contract::ipc::DispatcherCancellation,
    )>,
}
impl DriveReopenSnapshot {
    pub fn participants(&self) -> &[NativeRequestOwner] {
        &self.participants
    }
    pub fn controls(&self) -> &[CancelToken] {
        &self.controls
    }
}

impl shell_contract::ipc::NativeDriveReopenSnapshot for DriveReopenSnapshot {
    fn participants(&self) -> &[NativeRequestOwner] {
        &self.participants
    }
    fn controls(&self) -> &[CancelToken] {
        &self.controls
    }
    fn accepted_reload_controls(
        &self,
        accepted: &shell_contract::hosts::AcceptedPdfReload,
    ) -> Result<Vec<CancelToken>, String> {
        accepted_controls(&self.controls, &self.dispatcher, accepted)
    }
    fn with_committed_state(
        &self,
        moved: bool,
        source_commit: &file_commit::FileCommitReceipt,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        with_committed_reopen_snapshot(self, moved, source_commit, effect)
    }
}

fn accepted_controls(
    controls: &[CancelToken],
    dispatcher: &[(
        NativeRequestOwner,
        shell_contract::ipc::DispatcherCancellation,
    )],
    accepted: &shell_contract::hosts::AcceptedPdfReload,
) -> Result<Vec<CancelToken>, String> {
    let Some((retired, provenance)) = dispatcher.iter().find(|(owner, _)| accepted.retires(owner))
    else {
        return Ok(controls.to_vec());
    };
    let primary = controls
        .first()
        .ok_or("The Drive primary cancellation is unavailable")?;
    if !retired
        .context
        .renderer()
        .is_some_and(|renderer| renderer.same_generation(provenance.renderer()))
    {
        return Err("The Drive dispatcher renderer provenance changed".into());
    }
    let inherited = retired.accepted_reload_losses(accepted, primary, primary)?;
    let mut effective: Vec<CancelToken> = inherited.tokens().cloned().collect();
    if dispatcher.iter().any(|(owner, _)| !accepted.retires(owner)) {
        effective.push(primary.clone());
    }
    effective.extend_from_slice(&controls[1..]);
    Ok(effective)
}

impl Committed {
    fn snapshot(&self) -> Result<DriveReopenSnapshot, String> {
        let snapshot = DriveReopenSnapshot {
            files: self.files.clone(),
            entry: self.entry.clone(),
            generation: self.generation.clone(),
            moved_commit: self.moved_commit.clone(),
            original_commit: self.original_commit.clone(),
            has_fallback: self.fallback.is_some(),
            participants: self.authority.participants.clone(),
            controls: self.authority.controls.clone(),
            dispatcher: self.authority.dispatcher.clone(),
        };
        self.files
            .with_native_generation(&self.entry, &self.generation, || Ok(snapshot))
    }
}

pub fn preflight_committed_reopen(
    receipt: &Arc<dyn NativeDriveMoveReceipt>,
) -> Result<DriveReopenSnapshot, String> {
    let value = (receipt.as_ref() as &dyn Any)
        .downcast_ref::<Committed>()
        .ok_or("The Drive receipt was not issued by the Drive service")?;
    value.snapshot()
}

pub fn with_committed_reopen_snapshot(
    snapshot: &DriveReopenSnapshot,
    moved: bool,
    source_commit: &file_commit::FileCommitReceipt,
    effect: &mut dyn FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let expected = if moved {
        &snapshot.moved_commit
    } else {
        &snapshot.original_commit
    };
    if !expected.same_commit(source_commit) || (!moved && !snapshot.has_fallback) {
        return Err("The Drive saved input does not match the committed object".into());
    }
    snapshot
        .files
        .with_native_generation(&snapshot.entry, &snapshot.generation, effect)
}

#[allow(clippy::type_complexity)]
pub fn with_committed_reopen_state(
    receipt: &Arc<dyn NativeDriveMoveReceipt>,
    moved: bool,
    source_commit: &file_commit::FileCommitReceipt,
    effect: &mut dyn FnMut(&[NativeRequestOwner], &[CancelToken]) -> Result<(), String>,
) -> Result<(), String> {
    let value = (receipt.as_ref() as &dyn Any)
        .downcast_ref::<Committed>()
        .ok_or("The Drive receipt was not issued by the Drive service")?;
    let path = if moved {
        &value.moved
    } else {
        value.authority.publication.path()
    };
    let expected_commit = if moved {
        &value.moved_commit
    } else {
        &value.original_commit
    };
    if !expected_commit.same_commit(source_commit) {
        return Err("The Drive saved input does not match the committed object".into());
    }
    if (!moved && value.fallback.is_none())
        || (moved
            && value.entry.local_path
                != path
                    .to_str()
                    .ok_or("The Drive cache path is not valid Unicode")?)
        || path.as_os_str().is_empty()
    {
        return Err("The committed Drive path changed".into());
    }
    value
        .files
        .with_native_generation(&value.entry, &value.generation, || {
            effect(&value.authority.participants, &value.authority.controls)
        })
}

pub fn read_committed_reopen(
    receipt: &Arc<dyn NativeDriveMoveReceipt>,
    moved: bool,
    max_bytes: usize,
    additional: &[CancelToken],
) -> std::io::Result<file_commit::CommittedRead> {
    let value = (receipt.as_ref() as &dyn Any)
        .downcast_ref::<Committed>()
        .ok_or_else(|| {
            std::io::Error::other("The Drive receipt was not issued by the Drive service")
        })?;
    value.read(moved, max_bytes, additional)
}

pub fn committed_reopen_losses(
    receipt: &Arc<dyn NativeDriveMoveReceipt>,
) -> Result<Vec<CancelToken>, String> {
    let value = (receipt.as_ref() as &dyn Any)
        .downcast_ref::<Committed>()
        .ok_or("The Drive receipt was not issued by the Drive service")?;
    let mut losses = value.authority.losses()?;
    losses.push(value.generation.loss.clone());
    value
        .files
        .with_native_generation(&value.entry, &value.generation, || Ok(losses))
}

impl Committed {
    fn read(
        &self,
        moved: bool,
        max_bytes: usize,
        additional: &[CancelToken],
    ) -> std::io::Result<file_commit::CommittedRead> {
        if !moved && self.fallback.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "The selected Drive fallback object is unavailable",
            ));
        }
        self.files
            .with_native_generation(&self.entry, &self.generation, || Ok(()))
            .map_err(std::io::Error::other)?;
        let commit = if moved {
            &self.moved_commit
        } else {
            &self.original_commit
        };
        let losses = self.authority.losses().map_err(std::io::Error::other)?;
        commit.read_bounded(max_bytes, || {
            self.generation.loss.is_cancelled()
                || losses
                    .iter()
                    .chain(additional)
                    .any(CancelToken::is_cancelled)
        })
    }
    fn effect(
        &self,
        moved: bool,
        owners: &[NativeRequestOwner],
        controls: &[&CancelToken],
        effect: &mut dyn FnMut(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        let path = if moved {
            self.moved.as_path()
        } else {
            self.authority.publication.path()
        };
        self.authority
            .with_prepared(owners, controls, &mut |admit| {
                self.files
                    .with_native_generation(&self.entry, &self.generation, || {
                        admit(&mut || effect(path))
                    })
            })
    }
}

impl NativeDriveMoveReceipt for Committed {
    fn preflight_reopen(
        &self,
    ) -> Result<Box<dyn shell_contract::ipc::NativeDriveReopenSnapshot>, String> {
        Ok(Box::new(self.snapshot()?))
    }
    fn read_committed_reopen(
        &self,
        moved: bool,
        max_bytes: usize,
        controls: &[CancelToken],
    ) -> std::io::Result<file_commit::CommittedRead> {
        self.read(moved, max_bytes, controls)
    }
    fn publication(&self) -> &SavedPublication {
        &self.authority.publication
    }
    fn moved_path(&self) -> &Path {
        &self.moved
    }
    fn fallback_path(&self) -> &Path {
        self.authority.publication.path()
    }
    fn with_moved_effect(
        &self,
        owners: &[NativeRequestOwner],
        controls: &[&CancelToken],
        effect: &mut dyn FnMut(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        self.effect(true, owners, controls, effect)
    }
    fn with_fallback_effect(
        &self,
        owners: &[NativeRequestOwner],
        controls: &[&CancelToken],
        effect: &mut dyn FnMut(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        self.effect(false, owners, controls, effect)
    }
    fn cleanup_warning(&self) -> Option<String> {
        self.warning
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    fn retry_cleanup(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        let transaction = self.transaction.clone();
        let warning = self.warning.clone();
        let core = self.services.core.clone();
        self.services.workers.spawn(Box::new(move || {
            let result = transaction
                .lock()
                .map_err(|_| "The Drive cleanup state is poisoned".to_string())
                .and_then(|mut state| state.cleanup());
            *warning.lock().unwrap_or_else(|error| error.into_inner()) =
                result.as_ref().err().cloned();
            core.post(Box::new(move || done(result)));
        }));
    }
}

#[derive(Clone)]
pub(super) struct Pending {
    pub authority: Authority,
    pub transaction: Arc<Mutex<Transaction>>,
    pub original_error: String,
    pub cleanup_error: Arc<Mutex<Option<String>>>,
    pub services: Services,
}

impl NativeDrivePendingReceipt for Pending {
    fn publication(&self) -> &SavedPublication {
        &self.authority.publication
    }
    fn original_error(&self) -> &str {
        &self.original_error
    }
    fn cleanup_error(&self) -> Option<String> {
        self.cleanup_error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    fn retry(&self, done: Box<dyn FnOnce(NativeDrivePendingOutcome) + Send>) {
        let transaction = self.transaction.clone();
        let original_error = self.original_error.clone();
        let core = self.services.core.clone();
        let retry = self.clone();
        self.services.workers.spawn(Box::new(move || {
            let result = transaction
                .lock()
                .map_err(|_| "The Drive rollback state is poisoned".to_string())
                .and_then(|mut state| state.rollback());
            core.post(Box::new(move || {
                done(match result {
                    Ok(()) => NativeDrivePendingOutcome::Restored { original_error },
                    Err(error) => {
                        *retry
                            .cleanup_error
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Some(error.clone());
                        NativeDrivePendingOutcome::StillPending {
                            receipt: Arc::new(retry),
                            error,
                        }
                    }
                })
            }));
        }));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use shell_contract::ipc::NativeDriveReopenSnapshot;

    fn committed(
        parent: &std::fs::File,
        path: &Path,
        bytes: &[u8],
    ) -> file_commit::FileCommitReceipt {
        let write = file_commit::prepare_direct(parent, path)
            .unwrap()
            .open()
            .unwrap();
        match write.write_all(bytes).unwrap() {
            file_commit::DirectWriteOutcome::Committed(receipt) => receipt,
            file_commit::DirectWriteOutcome::WrittenWithoutReceipt(error) => {
                panic!("publication proof failed: {error}")
            }
        }
    }

    #[test]
    fn state_only_admission_rejects_foreign_commit_and_replaced_epoch() {
        let rig = super::super::testing::rig("native-drive-state-admission");
        let parent = std::fs::File::open(&rig.dir).unwrap();
        let path = rig.dir.join("saved.drawio");
        let other_path = rig.dir.join("other.drawio");
        let actual = committed(&parent, &path, b"same bytes");
        let foreign = committed(&parent, &other_path, b"same bytes");
        let entry = CacheEntry {
            source_id: "source".into(),
            google: false,
            name: "saved.drawio".into(),
            parent_id: None,
            local_path: path.to_str().unwrap().into(),
            synced_sha: "hash".into(),
            target_id: None,
            target_md5: None,
            target_modified: None,
            source_modified: "revision".into(),
            detached: false,
            opened_at: 0,
        };
        let files = rig.port.inner.files.clone();
        files.insert_test_entry(entry.clone());
        let (_, generation) = files
            .capture_native_entry(&entry.local_path)
            .unwrap()
            .unwrap();
        let cancel = CancelToken::default();
        let snapshot = DriveReopenSnapshot {
            files: files.clone(),
            entry: entry.clone(),
            generation: generation.clone(),
            moved_commit: actual.clone(),
            original_commit: actual.clone(),
            has_fallback: true,
            participants: Vec::new(),
            controls: vec![cancel.clone(), CancelToken::default()],
            dispatcher: Vec::new(),
        };
        let mut called = false;
        snapshot
            .with_committed_state(true, &actual, &mut || {
                called = true;
                Ok(())
            })
            .unwrap();
        assert!(called);
        called = false;
        assert!(
            snapshot
                .with_committed_state(true, &foreign, &mut || {
                    called = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!called);
        cancel.cancel();
        assert!(
            snapshot
                .with_committed_state(true, &actual, &mut || {
                    cancel
                        .with_active(|| {
                            called = true;
                            Ok(())
                        })
                        .ok_or("cancelled")?
                })
                .is_err()
        );
        assert!(!called);
        files.insert_test_entry(entry);
        assert!(generation.loss.is_cancelled());
        assert!(
            snapshot
                .with_committed_state(true, &actual, &mut || {
                    called = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!called);
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use shell_contract::ViewId;
    use shell_contract::ipc::{NativeContext, PdfDocumentLease};
    use shell_contract::runtime::RendererGeneration;

    fn fixture() -> (
        NativeRequestOwner,
        shell_contract::hosts::AcceptedPdfReload,
        CancelToken,
    ) {
        let renderer = RendererGeneration::new();
        let dispatch = CancelToken::default();
        let identity = Arc::new(7_u8);
        let owner = NativeRequestOwner {
            view: ViewId(17),
            context: NativeContext::with_owner_loss(
                identity.clone(),
                renderer,
                CancelToken::default(),
            )
            .with_independent_native_loss(CancelToken::default())
            .with_pdf_document(PdfDocumentLease::new())
            .with_pdf_revision(PdfDocumentLease::new())
            .bind_dispatcher_cancellation(
                ViewId(17),
                dispatch.clone(),
                CancelToken::default(),
            ),
        };
        let accepted = shell_contract::hosts::AcceptedPdfReload::from_native_admission(
            owner.clone(),
            RendererGeneration::new(),
            CancelToken::default(),
            CancelToken::default(),
            Arc::new(move |candidate| {
                candidate
                    .context
                    .downcast_ref::<Arc<u8>>()
                    .is_some_and(|value| Arc::ptr_eq(value, &identity))
            }),
        );
        (owner, accepted, dispatch)
    }

    #[test]
    fn accepted_retirement_preserves_alias_and_independent_cancellation() {
        let (owner, accepted, dispatch) = fixture();
        let provenance = owner
            .dispatcher_cancellation_for(&dispatch)
            .unwrap()
            .clone();
        let independent = CancelToken::default();
        let controls = accepted_controls(
            &[dispatch.clone(), independent.clone()],
            &[(owner, provenance)],
            &accepted,
        )
        .unwrap();
        dispatch.cancel_renderer_retired();
        assert!(controls.iter().all(|token| !token.is_cancelled()));
        independent.cancel();
        assert!(controls.iter().any(CancelToken::is_cancelled));

        let (owner, accepted, dispatch) = fixture();
        let provenance = owner
            .dispatcher_cancellation_for(&dispatch)
            .unwrap()
            .clone();
        let controls = accepted_controls(
            &[dispatch.clone(), dispatch.clone()],
            &[(owner, provenance)],
            &accepted,
        )
        .unwrap();
        dispatch.cancel_renderer_retired();
        assert!(controls.iter().any(CancelToken::is_cancelled));

        let (owner, accepted, dispatch) = fixture();
        let provenance = owner
            .dispatcher_cancellation_for(&dispatch)
            .unwrap()
            .clone();
        let controls = accepted_controls(
            &[dispatch.clone(), CancelToken::default()],
            &[(owner, provenance)],
            &accepted,
        )
        .unwrap();
        dispatch.cancel();
        assert!(controls.iter().any(CancelToken::is_cancelled));
    }
}
