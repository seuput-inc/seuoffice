use super::*;

use crate::packages::electron_utils::atomic_write::owned::{Admission, admitted};

impl ProjectWriteGuard {
    pub fn write_new_document_authorized(
        &self,
        root: &str,
        target: &str,
        data: &[u8],
        authorize: Admission,
    ) -> Result<(), WriteGuardError> {
        self.write_new_document_authorized_receipt(root, target, data, authorize)
            .map(|_| ())
    }

    pub fn write_new_document_authorized_receipt(
        &self,
        root: &str,
        target: &str,
        data: &[u8],
        authorize: Admission,
    ) -> Result<file_commit::FileCommitReceipt, WriteGuardError> {
        let api = path_api(&self.platform);
        let bad_path = || ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, target);
        if !looks_absolute(target)
            || split_segments(target)
                .iter()
                .any(|segment| *segment == ".." || *segment == ".")
        {
            return Err(bad_path().into());
        }
        self.assert_root(root)?;
        let rel = relative_inside(root, &api.normalize(target), &self.platform)
            .ok_or_else(|| ProjectWriteRefused::new(ProjectWriteRefusal::Outside, target))?;
        let segments: Vec<_> = rel.split('/').collect();
        if !segments.iter().all(|segment| is_plain_segment(segment)) {
            return Err(bad_path().into());
        }
        if is_data_dir_name(segments[0]) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Outside, target).into());
        }
        let (last, folders) = segments
            .split_last()
            .expect("a relative path has a segment");
        let mut dir = root.to_owned();
        for segment in folders {
            dir = join(&[&dir, segment]);
            let info = lstat_or_null(&dir)?.ok_or_else(|| {
                ProjectWriteRefused::new(ProjectWriteRefusal::MissingFolder, &dir)
            })?;
            if info.file_type().is_symlink() {
                return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Link, &dir).into());
            }
            if !info.is_dir() {
                return Err(ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, &dir).into());
            }
        }
        if !same_path(&realpath(&dir)?, &dir, &self.platform) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Link, &dir).into());
        }
        let final_path = join(&[&dir, last]);
        if lstat_or_null(&final_path)?.is_some() {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Exists, &final_path).into());
        }
        let result = publish(&dir, last, &final_path, data, &authorize);
        match result {
            Err(WriteGuardError::Fs(error))
                if error.code == Some("EEXIST") || error.code == Some("ENOTEMPTY") =>
            {
                Err(ProjectWriteRefused::new(ProjectWriteRefusal::Exists, final_path).into())
            }
            other => other,
        }
    }
}

#[cfg(unix)]
fn cleanup_error(original: WriteGuardError, cleanup: ProjectCleanup) -> WriteGuardError {
    match cleanup.retry() {
        Ok(()) => original,
        Err(_) => WriteGuardError::Cleanup {
            original: Box::new(original),
            cleanup,
        },
    }
}

#[cfg(unix)]
fn publish(
    dir: &str,
    name: &str,
    target: &str,
    bytes: &[u8],
    authorize: &Admission,
) -> Result<file_commit::FileCommitReceipt, WriteGuardError> {
    use std::sync::{Arc, Mutex};
    let parent = open_stage_dir(dir)?;
    let destination = std::ffi::CString::new(name)
        .map_err(|_| ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, target))?;
    // The original writes a temporary file beside the target, so a folder that
    // takes no new entry fails there with `open` on that name.
    let temp = crate::packages::electron_utils::atomic_write::temp_path_beside(target);
    let private = file_commit::PrivateDirectory::create(&parent)
        .map_err(|error| node_error(&error, "open", &temp))?;
    let pending = match private.create_stage(0o666) {
        Ok(pending) => pending,
        Err(error) => {
            return Err(cleanup_error(
                node_error(&error, "open", &temp).into(),
                ProjectCleanup::directory(private),
            ));
        }
    };
    let mut stage = match pending.quarantine() {
        Ok(stage) => stage,
        Err((error, pending)) => {
            return Err(cleanup_error(
                node_error(&error, "rename", target).into(),
                ProjectCleanup::pending(pending),
            ));
        }
    };
    if let Err(error) = stage.file_mut().write_all(bytes) {
        return Err(cleanup_error(
            node_error(&error, "write", target).into(),
            ProjectCleanup::stage(stage),
        ));
    }
    let plan = match stage.prepare(
        &parent,
        &destination,
        std::path::Path::new(target),
        file_commit::PublicationMode::Exclusive,
    ) {
        Ok(plan) => plan,
        Err((error, stage)) => {
            return Err(cleanup_error(
                node_error(&error, "rename", target).into(),
                ProjectCleanup::stage(stage),
            ));
        }
    };
    let publish_parent = match parent.try_clone() {
        Ok(parent) => parent,
        Err(error) => {
            return Err(cleanup_error(
                node_error(&error, "open", dir).into(),
                ProjectCleanup::stage(plan.into_stage()),
            ));
        }
    };
    let plan = Arc::new(Mutex::new(Some(plan)));
    let effect_plan = plan.clone();
    let publish_dir = dir.to_owned();
    let publish_target = target.to_owned();
    let result = admitted(authorize, move || {
        if !stage_handle_matches(&publish_parent, &publish_dir)? {
            return Err(NodeError {
                code: None,
                message: "project document folder changed".into(),
            });
        }
        let mut plan = effect_plan
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(prepared) = plan.take() else {
            return Err(NodeError {
                code: None,
                message: "project publication already used".into(),
            });
        };
        match prepared.publish() {
            Ok(receipt) => Ok(receipt),
            Err((error, prepared)) => {
                *plan = Some(prepared);
                Err(node_error(&error, "rename", &publish_target))
            }
        }
    });
    let unused = plan
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let cleanup = match unused {
        Some(prepared) => ProjectCleanup::stage(prepared.into_stage()),
        None => ProjectCleanup::directory(private),
    };
    match result {
        Ok(Ok(receipt)) => match cleanup.retry() {
            Ok(()) => Ok(receipt),
            Err(error) => Err(WriteGuardError::Cleanup {
                original: Box::new(
                    NodeError {
                        code: None,
                        message: format!(
                            "document was published, but staging cleanup is pending: {error}"
                        ),
                    }
                    .into(),
                ),
                cleanup,
            }),
        },
        Ok(Err(error)) | Err(error) => Err(cleanup_error(error.into(), cleanup)),
    }
}

#[cfg(windows)]
fn publish(
    dir: &str,
    name: &str,
    target: &str,
    bytes: &[u8],
    authorize: &Admission,
) -> Result<file_commit::FileCommitReceipt, WriteGuardError> {
    let parent = staging_windows::open_existing_dir(dir)?;
    let mut staged = staging_windows::create_project_data_stage(&parent, dir, target, name)?;
    staging_windows::write_project_data_stage(&mut staged, bytes)?;
    let dir = dir.to_owned();
    let plan = staging_windows::prepare_project_data_exclusive_receipt(
        &staged,
        std::path::Path::new(target),
    )?;
    admitted(authorize, move || {
        if !staging_windows::handle_matches_path(&parent, &dir)? {
            return Err(NodeError {
                code: None,
                message: "project document folder changed".into(),
            });
        }
        staging_windows::publish_project_data_exclusive_receipt(&mut staged, plan)
    })?
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Folder(std::path::PathBuf);
    impl Folder {
        fn new() -> Self {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).unwrap();
            let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            let path = std::env::temp_dir().join(format!("seuoffice-new-document-{suffix}"));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn text(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rejected_owner_leaves_no_document_or_temporary_payload() {
        let folder = Folder::new();
        let target = folder.0.join("draft.html").to_string_lossy().into_owned();
        let guard = ProjectWriteGuard::new(
            crate::packages::ai_provider::agent_session_policy::process_platform(),
        );
        let result = guard.write_new_document_authorized(
            folder.text(),
            &target,
            b"draft",
            std::sync::Arc::new(|_| {
                Err(NodeError {
                    code: None,
                    message: "owner released".into(),
                })
            }),
        );
        assert!(result.is_err());
        assert!(!std::path::Path::new(&target).exists());
        assert_eq!(fs::read_dir(&folder.0).unwrap().count(), 0);
    }

    #[test]
    fn collision_at_publication_never_replaces_existing_file() {
        let folder = Folder::new();
        let target = folder.0.join("draft.md").to_string_lossy().into_owned();
        let guard = ProjectWriteGuard::new(
            crate::packages::ai_provider::agent_session_policy::process_platform(),
        );
        let collision = target.clone();
        let result = guard.write_new_document_authorized(
            folder.text(),
            &target,
            b"new",
            std::sync::Arc::new(move |effect| {
                fs::write(&collision, b"other writer").unwrap();
                Ok(effect())
            }),
        );
        assert!(
            matches!(result, Err(WriteGuardError::Refused(error)) if error.reason == ProjectWriteRefusal::Exists)
        );
        assert_eq!(fs::read(&target).unwrap(), b"other writer");
        assert_eq!(fs::read_dir(&folder.0).unwrap().count(), 1);
    }

    #[test]
    fn receipt_exists_only_after_real_exclusive_publication() {
        let folder = Folder::new();
        let target = folder.0.join("draft.md").to_string_lossy().into_owned();
        let expected = target.clone();
        let guard = ProjectWriteGuard::new(
            crate::packages::ai_provider::agent_session_policy::process_platform(),
        );
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let in_gate = observed.clone();
        let receipt = guard
            .write_new_document_authorized_receipt(
                folder.text(),
                &target,
                b"draft",
                std::sync::Arc::new(move |effect| {
                    let result = effect();
                    let commit = result
                        .downcast_ref::<Result<file_commit::FileCommitReceipt, NodeError>>()
                        .and_then(|result| result.as_ref().ok())
                        .expect("successful commit proof inside admission");
                    assert_eq!(commit.path(), std::path::Path::new(&expected));
                    #[cfg(unix)]
                    assert_eq!(fs::read(commit.path()).unwrap(), b"draft");
                    in_gate.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(result)
                }),
            )
            .unwrap();
        assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(receipt.path(), std::path::Path::new(&target));
        assert_eq!(receipt.read_bounded(5, || false).unwrap().bytes(), b"draft");
        assert!(receipt.read_bounded(4, || false).is_err());
        assert!(
            guard
                .write_new_document_authorized_receipt(
                    folder.text(),
                    &target,
                    b"replace",
                    std::sync::Arc::new(|effect| Ok(effect()))
                )
                .is_err()
        );
        assert_eq!(fs::read(&target).unwrap(), b"draft");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_retains_inverse_until_public_occupant_is_removed() {
        let folder = Folder::new();
        let parent = fs::File::open(&folder.0).unwrap();
        let private = file_commit::PrivateDirectory::create(&parent).unwrap();
        let path = folder.0.join("source");
        fs::write(&path, b"original").unwrap();
        let file = fs::File::open(&path).unwrap();
        let name = std::ffi::CString::new("source").unwrap();
        let pending = private.quarantine_public(&parent, &name, file).unwrap();
        fs::rename(&path, folder.0.join("original")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let pending = match pending.quarantine() {
            Ok(_) => panic!("substitution accepted"),
            Err((_, pending)) => pending,
        };
        fs::write(&path, b"occupant").unwrap();
        let cleanup = ProjectCleanup::pending(pending);
        let copy = cleanup.clone();
        let error = WriteGuardError::Cleanup {
            original: Box::new(
                NodeError {
                    code: Some("EACCES"),
                    message: "original failure".into(),
                }
                .into(),
            ),
            cleanup: cleanup.clone(),
        };
        assert_eq!(error.code(), Some("EACCES"));
        assert!(error.message().contains("original failure"));
        assert!(error.message().contains("cleanup is pending"));
        assert_eq!(cleanup, copy);
        assert_eq!(
            cleanup.retry().unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), b"occupant");
        fs::remove_file(&path).unwrap();
        copy.retry().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert_eq!(fs::read(folder.0.join("original")).unwrap(), b"original");
        cleanup.retry().unwrap();
    }
}
