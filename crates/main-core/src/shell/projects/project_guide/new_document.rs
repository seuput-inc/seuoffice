use super::*;
use crate::shell::projects::project_ipc::WindowProjectLookup;

pub struct NewDocumentIo {
    settings_path: String,
    platform: String,
}

pub struct NewDocumentAuthority {
    settings_path: String,
    platform: String,
    snapshot: ContextAuthority,
}

impl ProjectGuide {
    pub fn new_document_io(&self) -> NewDocumentIo {
        NewDocumentIo {
            settings_path: self.settings_path.clone(),
            platform: self.platform.clone(),
        }
    }
}

impl NewDocumentIo {
    pub fn settings_path(&self) -> &str {
        &self.settings_path
    }

    pub fn resolve(
        self,
        lookup: WindowProjectLookup,
    ) -> Result<Option<NewDocumentAuthority>, WriteGuardError> {
        let settings = read_projects_settings(&self.settings_path);
        let id = match lookup {
            WindowProjectLookup::Chosen(id) => id,
            WindowProjectLookup::Initial(target) => {
                let resolved = match target {
                    SelectionTarget::Home => ResolvedSelectionTarget::Home,
                    SelectionTarget::Untitled { project_id } => {
                        ResolvedSelectionTarget::Untitled { project_id }
                    }
                    SelectionTarget::Document { path } => {
                        if !is_absolute(&path) {
                            return Ok(None);
                        }
                        ResolvedSelectionTarget::Document {
                            doc_key: canonical_path(&path),
                        }
                    }
                };
                selection_for(&settings, &resolved, &self.platform).project_id
            }
        };
        let Some(id) = id.filter(|id| !id.is_empty()) else {
            return Ok(None);
        };
        let Some(snapshot) = ContextAuthority::capture(&settings, &id) else {
            return Ok(None);
        };
        Ok(Some(NewDocumentAuthority {
            settings_path: self.settings_path,
            platform: self.platform,
            snapshot,
        }))
    }
}

impl NewDocumentAuthority {
    pub fn project_id(&self) -> &str {
        &self.snapshot.project_id
    }
    pub fn root(&self) -> &str {
        &self.snapshot.path
    }

    pub fn write(
        &self,
        target: &str,
        bytes: &[u8],
        authorize: crate::packages::electron_utils::atomic_write::owned::Admission,
    ) -> Result<ProjectWriteResult, WriteGuardError> {
        self.write_with_receipt(target, bytes, authorize)
            .map(|_| ProjectWriteResult::Ok)
    }

    pub fn write_with_receipt(
        &self,
        target: &str,
        bytes: &[u8],
        authorize: crate::packages::electron_utils::atomic_write::owned::Admission,
    ) -> Result<file_commit::FileCommitReceipt, WriteGuardError> {
        let snapshot = self.snapshot.clone();
        let settings_path = self.settings_path.clone();
        let checked = std::sync::Arc::new(
            move |effect: crate::packages::electron_utils::atomic_write::owned::ShortEffect| {
                if !snapshot.is_current(&read_projects_settings(&settings_path)) {
                    return Err(NodeError {
                        code: None,
                        message: "project authority changed while saving the document".into(),
                    });
                }
                authorize(effect)
            },
        );
        ProjectWriteGuard::new(&self.platform).write_new_document_authorized_receipt(
            &self.snapshot.path,
            target,
            bytes,
            checked,
        )
    }
}
