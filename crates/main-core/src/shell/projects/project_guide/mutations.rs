//! Owned, request-authorized mutations of project instructions and memory.
use super::*;
use crate::packages::project_store::ProjectChangedPart;
use crate::shell::projects::write_guard::ProjectWriteGuard;

#[derive(Debug, Clone, PartialEq)]
pub enum MutationRequest {
    SaveInstructions {
        project_id: String,
        text: String,
    },
    SaveMemory {
        project_id: String,
        text: String,
    },
    EditMemory {
        project_id: String,
        change: ProjectMemoryOp,
        by: MemoryEditor,
    },
    ApplyMemoryConsolidation {
        project_id: String,
        entries: Vec<JsonValue>,
    },
    UndoMemoryConsolidation {
        project_id: String,
        backup_name: String,
    },
    AddIgnore {
        project_id: String,
        rel_path: String,
    },
    RemoveIgnore {
        project_id: String,
        rel_path: String,
    },
    RenameProject {
        project_id: String,
        name: String,
    },
    RetryUnwritable {
        project_id: String,
    },
}
impl MutationRequest {
    pub fn project_id(&self) -> &str {
        match self {
            Self::SaveInstructions { project_id, .. }
            | Self::SaveMemory { project_id, .. }
            | Self::EditMemory { project_id, .. }
            | Self::ApplyMemoryConsolidation { project_id, .. }
            | Self::UndoMemoryConsolidation { project_id, .. }
            | Self::AddIgnore { project_id, .. }
            | Self::RemoveIgnore { project_id, .. }
            | Self::RenameProject { project_id, .. }
            | Self::RetryUnwritable { project_id } => project_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MutationResult {
    pub reply: JsonValue,
    pub changed: Option<ProjectChangedPart>,
    pub warnings: Vec<(String, String)>,
}
pub enum MutationPreparation {
    Done(MutationResult),
    Write(MutationWrite),
}

/// A plan holds only owned, Send data; it never retains a guide or host callback.
pub struct MutationWrite {
    request: MutationRequest,
    authority: ContextAuthority,
    root: String,
    data_dir: String,
    platform: String,
    fresh_identity: Option<String>,
    rename_date: Option<Result<String, GuideError>>,
    pending_id_fix: bool,
    memory_context: Option<MemoryOpContext>,
    backup_at: Option<f64>,
    backup_name: Option<String>,
    identity_done: bool,
    retained: Vec<String>,
    diagnostics: Vec<String>,
}
pub enum MutationAcknowledgement {
    Done {
        result: MutationResult,
        authority: ContextAuthority,
        unwritable: bool,
        warning: Option<String>,
        retained: Vec<String>,
        identified: bool,
    },
    NeedBackupName(MutationWrite),
}

fn reply<T: serde::Serialize>(value: &T, changed: Option<ProjectChangedPart>) -> MutationResult {
    MutationResult {
        reply: jscompat::json_value_from_serde(
            &serde_json::to_value(value).expect("project mutation result serializes"),
        ),
        changed,
        warnings: Vec::new(),
    }
}

impl ProjectGuide {
    /// State-only entry step: invoke under the request owner gate before
    /// preparing a retry (the original clears even for an unknown project).
    pub fn begin_retry_mutation(&self, request: &MutationRequest) {
        if let MutationRequest::RetryUnwritable { project_id } = request {
            self.unwritable.borrow_mut().retain(|id| id != project_id);
        }
    }

    pub fn prepare_mutation(
        &self,
        request: MutationRequest,
    ) -> Result<MutationPreparation, GuideError> {
        let id = request.project_id();
        let settings = self.read();
        if let MutationRequest::AddIgnore { rel_path, .. }
        | MutationRequest::RemoveIgnore { rel_path, .. } = &request
        {
            assert_folder_relative(rel_path)?;
        }
        if let MutationRequest::RenameProject { name, .. } = &request {
            if clean_project_name(name).is_empty() {
                return Err(GuideError::plain("a project name cannot be empty"));
            }
        }
        let authority =
            ContextAuthority::capture(&settings, id).ok_or_else(|| unknown_project(id))?;
        if let MutationRequest::ApplyMemoryConsolidation { entries, .. } = &request {
            if entries
                .iter()
                .any(|entry| !matches!(entry, JsonValue::String(_)))
            {
                return Err(GuideError::plain("entries must be a list of strings"));
            }
        }
        if let MutationRequest::EditMemory {
            by: MemoryEditor::Ai,
            ..
        } = &request
        {
            if !settings_of(id, &settings).memory_enabled {
                return Ok(MutationPreparation::Done(reply(
                    &ProjectMemoryEditResult::Refused {
                        reason: ProjectMemoryRefusal::Disabled,
                    },
                    None,
                )));
            }
            if self.trust_state(id) != ProjectTrustState::Use {
                return Ok(MutationPreparation::Done(reply(
                    &ProjectMemoryEditResult::Refused {
                        reason: ProjectMemoryRefusal::Without,
                    },
                    None,
                )));
            }
        }
        let root = authority.path.clone();
        let data_dir = data_dir_name(&root);
        if let MutationRequest::UndoMemoryConsolidation { backup_name, .. } = &request {
            let names = backup_names(&root, &data_dir);
            if !memory_backup_pattern().test(backup_name)
                || names.first() != Some(backup_name)
                || read_data_text(&root, backup_name, Some(&data_dir)).is_none()
            {
                return Ok(MutationPreparation::Done(reply(
                    &UndoMemoryConsolidationResult::NoBackup,
                    None,
                )));
            }
        }
        if !matches!(&request, MutationRequest::RetryUnwritable { .. }) && self.is_unwritable(id) {
            return Ok(MutationPreparation::Done(unwritable_result(&request)));
        }
        let rename = matches!(&request, MutationRequest::RenameProject { .. });
        let fresh_identity = if rename {
            None
        } else {
            Some(project_file_text(
                &self.project_file_for(id, &root, &data_dir)?,
            ))
        };
        let rename_date = rename.then(|| self.now_iso());
        let pending_id_fix = self.pending_id_fix.borrow().iter().any(|item| item == id);
        let memory_context = match &request {
            MutationRequest::EditMemory { .. }
            | MutationRequest::ApplyMemoryConsolidation { .. } => Some(self.memory_context()),
            _ => None,
        };
        let backup_at = matches!(&request, MutationRequest::ApplyMemoryConsolidation { .. })
            .then(|| self.clock());
        let backup_name = backup_at.map(|at| self.mutation_backup_name(&root, &data_dir, at));
        Ok(MutationPreparation::Write(MutationWrite {
            request,
            authority,
            root,
            data_dir,
            platform: self.platform.clone(),
            fresh_identity,
            rename_date,
            pending_id_fix,
            memory_context,
            backup_at,
            backup_name,
            identity_done: false,
            retained: Vec::new(),
            diagnostics: Vec::new(),
        }))
    }

    fn mutation_backup_name(&self, root: &str, data_dir: &str, mut at: f64) -> String {
        let taken = self.backup_names(root, data_dir);
        loop {
            let name = memory_backup_name(&(self.local_date.borrow_mut())(at));
            if !taken.contains(&name) {
                return name;
            }
            at += 1000.0;
        }
    }

    pub fn resume_mutation_backup(
        &self,
        mut write: MutationWrite,
    ) -> Result<MutationWrite, GuideError> {
        if !write.authority.is_current(&self.read()) {
            return Err(GuideError::plain("project authority changed while writing"));
        }
        let at = write
            .backup_at
            .ok_or_else(|| GuideError::plain("backup continuation is invalid"))?;
        write.backup_name = Some(self.mutation_backup_name(&write.root, &write.data_dir, at));
        Ok(write)
    }

    pub fn validate_mutation_ack(
        &self,
        ack: MutationAcknowledgement,
    ) -> Result<MutationAcknowledgement, GuideError> {
        let authority = match &ack {
            MutationAcknowledgement::Done { authority, .. } => authority,
            MutationAcknowledgement::NeedBackupName(write) => &write.authority,
        };
        if !authority.is_current(&self.read()) {
            return Err(GuideError::plain("project authority changed while writing"));
        }
        Ok(ack)
    }

    pub fn report_write_warnings(&self, warnings: &[(String, String)]) {
        for (label, detail) in warnings {
            self.warn(label, detail);
        }
    }

    pub fn finish_mutation(
        &self,
        ack: MutationAcknowledgement,
    ) -> Result<MutationResult, GuideError> {
        match ack {
            MutationAcknowledgement::NeedBackupName(_) => {
                Err(GuideError::plain("backup name continuation required"))
            }
            MutationAcknowledgement::Done {
                mut result,
                authority,
                unwritable,
                warning,
                retained,
                identified,
            } => {
                if identified {
                    self.pending_id_fix
                        .borrow_mut()
                        .retain(|id| id != &authority.project_id);
                }
                if unwritable {
                    self.mark_unwritable(&authority.project_id);
                    if let Some(warning) = warning {
                        result.warnings.push((
                            format!(
                                "[projects] cannot write into project {}:",
                                authority.project_id
                            ),
                            warning,
                        ));
                    }
                }
                for path in retained {
                    result
                        .warnings
                        .push(("[projects] private staging retained:".into(), path));
                }
                Ok(result)
            }
        }
    }
}

fn unwritable_result(request: &MutationRequest) -> MutationResult {
    match request {
        MutationRequest::EditMemory { .. } => reply(
            &ProjectMemoryEditResult::Refused {
                reason: ProjectMemoryRefusal::Unwritable,
            },
            None,
        ),
        MutationRequest::ApplyMemoryConsolidation { .. } => {
            reply(&ApplyMemoryConsolidationResult::Unwritable, None)
        }
        MutationRequest::UndoMemoryConsolidation { .. } => {
            reply(&UndoMemoryConsolidationResult::Unwritable, None)
        }
        _ => reply(&ProjectWriteResult::Unwritable, None),
    }
}

impl MutationWrite {
    fn check_authority(&self, settings_path: &str) -> Result<(), GuideError> {
        if self
            .authority
            .is_current(&read_projects_settings(settings_path))
        {
            Ok(())
        } else {
            Err(GuideError::plain("project authority changed while writing"))
        }
    }

    fn identify(
        &mut self,
        guard: &ProjectWriteGuard,
        settings_path: &str,
        authorize: &mut dyn FnMut(
            &mut dyn FnMut() -> Result<(), NodeError>,
        ) -> Result<(), NodeError>,
    ) -> Result<(), GuideError> {
        if self.identity_done {
            return Ok(());
        }
        self.check_authority(settings_path)?;
        let settings = read_projects_settings(settings_path);
        let id = self.authority.project_id.clone();
        let root = self.root.clone();
        let fresh = self
            .fresh_identity
            .clone()
            .ok_or_else(|| GuideError::plain("identity text is missing"))?;
        let pending = self.pending_id_fix;
        let platform = self.platform.clone();
        let mut change = move |current: Option<&str>| -> Result<Option<String>, GuideError> {
            let Some(text) = current else {
                return Ok(Some(fresh.clone()));
            };
            let fix = identity_needs_fix(&id, &root, text, pending, &settings, &platform);
            Ok(fix.then(|| fresh.clone()))
        };
        let (_, retained) = guard.update_data_authorized::<GuideError>(
            &self.root,
            project_folder_name("projectFile"),
            &mut change,
            Some(&self.data_dir),
            authorize,
        )?;
        self.retained.extend(retained);
        self.identity_done = true;
        Ok(())
    }

    /// Called after the shared settings queue admits this owned plan. All
    /// project effects and the settings replace use the captured owner guard.
    pub fn execute_authorized(
        mut self,
        settings_path: &str,
        mut authorize: impl FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
        commit: impl FnOnce(&str, &str) -> Result<(), NodeError>,
    ) -> Result<MutationAcknowledgement, GuideError> {
        self.check_authority(settings_path)?;
        let guard = ProjectWriteGuard::new(&self.platform);
        let mut owner_error: Option<NodeError> = None;
        let authority = self.authority.clone();
        let mut guarded = |effect: &mut dyn FnMut() -> Result<(), NodeError>| {
            if !authority.is_current(&read_projects_settings(settings_path)) {
                let error = NodeError {
                    code: None,
                    message: "project authority changed while writing".into(),
                };
                owner_error = Some(error.clone());
                return Err(error);
            }
            let mut effect_error = None;
            let result = authorize(&mut || {
                let result = effect();
                if let Err(error) = &result {
                    effect_error = Some(error.clone());
                }
                result
            });
            if let Err(error) = &result {
                // Admitted filesystem errors keep the original unwritable
                // policy; refusal of the captured owner is not such an error.
                if effect_error.as_ref() != Some(error) {
                    owner_error = Some(error.clone());
                }
            }
            result
        };
        let performed = self.perform(&guard, settings_path, &mut guarded);
        drop(guarded);
        if let Some(error) = owner_error {
            return Err(error.into());
        }
        let (mut result, hash) = match performed {
            Ok(Performed::Done(result, hash)) => (result, hash),
            Ok(Performed::BackupCollision) => {
                return Ok(MutationAcknowledgement::NeedBackupName(self));
            }
            Err(error) => {
                let mut result = unwritable_result(&self.request);
                result.warnings.extend(
                    self.diagnostics
                        .into_iter()
                        .map(|detail| ("[projects] backup creation collided:".into(), detail)),
                );
                return Ok(MutationAcknowledgement::Done {
                    result,
                    authority: self.authority,
                    unwritable: true,
                    warning: Some(error.message()),
                    retained: self.retained,
                    identified: self.identity_done,
                });
            }
        };
        if let Some((file, text)) = hash {
            let mut authority = self.authority.clone();
            let hash = hash_text(&text);
            let rejected = std::cell::Cell::new(false);
            crate::shell::app_settings::update_projects_settings_guarded(
                settings_path,
                |settings| {
                    if !authority.apply_if_current(settings, &[(file, Some(hash))]) {
                        rejected.set(true);
                    }
                },
                |from, to| {
                    if rejected.get() {
                        return Err(NodeError {
                            code: None,
                            message: "project authority changed while writing".into(),
                        });
                    }
                    commit(from, to)
                },
            )?;
            self.authority = authority;
        }
        result.warnings.extend(
            self.diagnostics
                .into_iter()
                .map(|detail| ("[projects] backup creation collided:".into(), detail)),
        );
        Ok(MutationAcknowledgement::Done {
            result,
            authority: self.authority,
            unwritable: false,
            warning: None,
            retained: self.retained,
            identified: self.identity_done,
        })
    }
}

enum Performed {
    Done(MutationResult, Option<(ProjectHashedFile, String)>),
    BackupCollision,
}

impl MutationWrite {
    fn write_text(
        &mut self,
        guard: &ProjectWriteGuard,
        settings: &str,
        name: &str,
        text: &str,
        authorize: &mut dyn FnMut(
            &mut dyn FnMut() -> Result<(), NodeError>,
        ) -> Result<(), NodeError>,
    ) -> Result<(), GuideError> {
        self.identify(guard, settings, authorize)?;
        self.check_authority(settings)?;
        let retained = guard.write_data_authorized(
            &self.root,
            name,
            text.as_bytes(),
            Some(&self.data_dir),
            authorize,
        )?;
        if !retained.is_empty() {
            self.retained.push(retained);
        }
        Ok(())
    }

    fn update_text(
        &mut self,
        guard: &ProjectWriteGuard,
        settings: &str,
        name: &str,
        change: &mut dyn FnMut(Option<&str>) -> Result<Option<String>, GuideError>,
        authorize: &mut dyn FnMut(
            &mut dyn FnMut() -> Result<(), NodeError>,
        ) -> Result<(), NodeError>,
    ) -> Result<DataUpdateResult, GuideError> {
        self.check_authority(settings)?;
        let root = self.root.clone();
        let data_dir = self.data_dir.clone();
        // Nested identity publication uses a separate file. The enclosing
        // target's witness remains held by the guarded update implementation.
        let authorize = RefCell::new(authorize);
        let mut transform = |current: Option<&str>| {
            let next = change(current)?;
            if next.is_some() {
                self.identify(guard, settings, &mut **authorize.borrow_mut())?;
            }
            Ok(next)
        };
        let mut publish =
            |effect: &mut dyn FnMut() -> Result<(), NodeError>| (**authorize.borrow_mut())(effect);
        let (result, retained) = guard.update_data_authorized::<GuideError>(
            &root,
            name,
            &mut transform,
            Some(&data_dir),
            &mut publish,
        )?;
        self.retained.extend(retained);
        Ok(result)
    }

    fn perform(
        &mut self,
        guard: &ProjectWriteGuard,
        settings: &str,
        authorize: &mut dyn FnMut(
            &mut dyn FnMut() -> Result<(), NodeError>,
        ) -> Result<(), NodeError>,
    ) -> Result<Performed, GuideError> {
        match self.request.clone() {
            MutationRequest::RenameProject { project_id, name } => {
                let clean = clean_project_name(&name);
                let fallback = self
                    .rename_date
                    .clone()
                    .ok_or_else(|| GuideError::plain("rename date is missing"))?;
                let mut change = |current: Option<&str>| -> Result<Option<String>, GuideError> {
                    let created_at = match current.and_then(parse_created_at) {
                        Some(at) => at,
                        None => fallback.clone()?,
                    };
                    Ok(Some(project_file_text(&ProjectFolderFile {
                        version: PROJECT_FORMAT_VERSION,
                        id: project_id.clone(),
                        name: clean.clone(),
                        created_at,
                    })))
                };
                let (_, retained) = guard.update_data_authorized::<GuideError>(
                    &self.root,
                    project_folder_name("projectFile"),
                    &mut change,
                    Some(&self.data_dir),
                    authorize,
                )?;
                self.retained.extend(retained);
                Ok(Performed::Done(
                    reply(&ProjectWriteResult::Ok, Some(ProjectChangedPart::Settings)),
                    None,
                ))
            }
            MutationRequest::RetryUnwritable { project_id } => {
                let fresh = self
                    .fresh_identity
                    .clone()
                    .ok_or_else(|| GuideError::plain("identity text is missing"))?;
                let latest = read_projects_settings(settings);
                let mut change = |current: Option<&str>| -> Result<Option<String>, GuideError> {
                    Ok(Some(match current {
                        Some(text)
                            if !identity_needs_fix(
                                &project_id,
                                &self.root,
                                text,
                                self.pending_id_fix,
                                &latest,
                                &self.platform,
                            ) =>
                        {
                            text.to_owned()
                        }
                        _ => fresh.clone(),
                    }))
                };
                let (_, retained) = guard.update_data_authorized::<GuideError>(
                    &self.root,
                    project_folder_name("projectFile"),
                    &mut change,
                    Some(&self.data_dir),
                    authorize,
                )?;
                self.retained.extend(retained);
                self.identity_done = true;
                Ok(Performed::Done(reply(&ProjectWriteResult::Ok, None), None))
            }
            MutationRequest::SaveInstructions { text, .. } => {
                self.write_text(
                    guard,
                    settings,
                    project_folder_name("instructionsFile"),
                    &text,
                    authorize,
                )?;
                Ok(Performed::Done(
                    reply(
                        &ProjectWriteResult::Ok,
                        Some(ProjectChangedPart::Instructions),
                    ),
                    Some((ProjectHashedFile::Instructions, text)),
                ))
            }
            MutationRequest::SaveMemory { text, .. } => {
                self.write_text(
                    guard,
                    settings,
                    project_folder_name("memoryFile"),
                    &text,
                    authorize,
                )?;
                Ok(Performed::Done(
                    reply(&ProjectWriteResult::Ok, Some(ProjectChangedPart::Memory)),
                    Some((ProjectHashedFile::Memory, text)),
                ))
            }
            MutationRequest::EditMemory { change, .. } => {
                let context = self
                    .memory_context
                    .clone()
                    .ok_or_else(|| GuideError::plain("memory context is missing"))?;
                let mut applied = None;
                let outcome = self.update_text(
                    guard,
                    settings,
                    project_folder_name("memoryFile"),
                    &mut |current| {
                        let result = apply_memory_op(current, &change, &context);
                        let next = match &result {
                            MemoryOpResult::Ok { text, .. } if current != Some(text.as_str()) => {
                                Some(text.clone())
                            }
                            _ => None,
                        };
                        applied = Some(result);
                        Ok(next)
                    },
                    authorize,
                )?;
                let result = match applied {
                    Some(MemoryOpResult::Ok { change, .. }) => {
                        ProjectMemoryEditResult::Ok { change }
                    }
                    Some(MemoryOpResult::Refused { reason }) => ProjectMemoryEditResult::Refused {
                        reason: reason.into(),
                    },
                    None => ProjectMemoryEditResult::Refused {
                        reason: ProjectMemoryRefusal::Invalid,
                    },
                };
                let changed = matches!(result, ProjectMemoryEditResult::Ok { .. })
                    .then_some(ProjectChangedPart::Memory);
                Ok(Performed::Done(
                    reply(&result, changed),
                    outcome
                        .text
                        .filter(|_| outcome.written)
                        .map(|text| (ProjectHashedFile::Memory, text)),
                ))
            }
            MutationRequest::AddIgnore { rel_path, .. }
            | MutationRequest::RemoveIgnore { rel_path, .. } => {
                let add = matches!(self.request, MutationRequest::AddIgnore { .. });
                self.update_text(
                    guard,
                    settings,
                    project_folder_name("ignoreFile"),
                    &mut |current| {
                        let next = if add {
                            add_ignore_line(current.unwrap_or(""), &rel_path)
                        } else {
                            let Some(current) = current else {
                                return Ok(None);
                            };
                            remove_ignore_line(current, &rel_path)
                        };
                        Ok((current != Some(next.as_str())).then_some(next))
                    },
                    authorize,
                )?;
                Ok(Performed::Done(
                    reply(&ProjectWriteResult::Ok, Some(ProjectChangedPart::Settings)),
                    None,
                ))
            }
            MutationRequest::UndoMemoryConsolidation { backup_name, .. } => {
                let names = backup_names(&self.root, &self.data_dir);
                if !memory_backup_pattern().test(&backup_name)
                    || names.first() != Some(&backup_name)
                {
                    return Ok(Performed::Done(
                        reply(&UndoMemoryConsolidationResult::NoBackup, None),
                        None,
                    ));
                }
                let Some(text) = read_data_text(&self.root, &backup_name, Some(&self.data_dir))
                else {
                    return Ok(Performed::Done(
                        reply(&UndoMemoryConsolidationResult::NoBackup, None),
                        None,
                    ));
                };
                self.write_text(
                    guard,
                    settings,
                    project_folder_name("memoryFile"),
                    &text,
                    authorize,
                )?;
                let result = UndoMemoryConsolidationResult::Ok {
                    entries: memory_entries(&text).len() as u64,
                };
                Ok(Performed::Done(
                    reply(&result, Some(ProjectChangedPart::Memory)),
                    Some((ProjectHashedFile::Memory, text)),
                ))
            }
            MutationRequest::ApplyMemoryConsolidation { entries, .. } => {
                let entries: Vec<String> = entries
                    .into_iter()
                    .map(|entry| match entry {
                        JsonValue::String(text) => Ok(text),
                        _ => Err(GuideError::plain("entries must be a list of strings")),
                    })
                    .collect::<Result<_, _>>()?;
                let context = self
                    .memory_context
                    .clone()
                    .ok_or_else(|| GuideError::plain("memory context is missing"))?;
                let text = build_memory_text(&entries, &context.header, &context.today);
                let backup = self
                    .backup_name
                    .clone()
                    .ok_or_else(|| GuideError::plain("backup name is missing"))?;
                let mut before = 0;
                let mut collision = false;
                self.identify(guard, settings, authorize)?;
                let root = self.root.clone();
                let data_dir = self.data_dir.clone();
                let authorize = RefCell::new(authorize);
                let mut change = |current: Option<&str>| -> Result<Option<String>, GuideError> {
                    match guard.create_data_authorized(
                        &root,
                        &backup,
                        current.unwrap_or("").as_bytes(),
                        Some(&data_dir),
                        &mut **authorize.borrow_mut(),
                    ) {
                        Ok(retained) => {
                            if let Some(path) = retained {
                                self.retained.push(path);
                            }
                        }
                        Err(WriteGuardError::Fs(error)) if error.code == Some("EEXIST") => {
                            self.diagnostics.push(error.message);
                            collision = true;
                            return Ok(None);
                        }
                        Err(error) => return Err(error.into()),
                    }
                    before = memory_entries(current.unwrap_or("")).len() as u64;
                    Ok(Some(text.clone()))
                };
                let mut publish = |effect: &mut dyn FnMut() -> Result<(), NodeError>| {
                    (**authorize.borrow_mut())(effect)
                };
                let (_, retained) = guard.update_data_authorized::<GuideError>(
                    &root,
                    project_folder_name("memoryFile"),
                    &mut change,
                    Some(&data_dir),
                    &mut publish,
                )?;
                self.retained.extend(retained);
                if collision {
                    return Ok(Performed::BackupCollision);
                }
                for name in backup_names(&root, &data_dir)
                    .iter()
                    .skip(MEMORY_BACKUPS_KEPT)
                {
                    self.check_authority(settings)?;
                    guard.remove_data_authorized(
                        &root,
                        name,
                        Some(&data_dir),
                        &mut **authorize.borrow_mut(),
                    )?;
                }
                let result = ApplyMemoryConsolidationResult::Ok {
                    before,
                    after: memory_entries(&text).len() as u64,
                    backup_name: backup,
                };
                Ok(Performed::Done(
                    reply(&result, Some(ProjectChangedPart::Memory)),
                    Some((ProjectHashedFile::Memory, text)),
                ))
            }
        }
    }
}

fn backup_names(root: &str, data_dir: &str) -> Vec<String> {
    let mut names: Vec<_> = plain_folder_entries(root, &[data_dir])
        .into_iter()
        .filter(|name| memory_backup_pattern().test(name))
        .collect();
    sort_utf16(&mut names);
    names.reverse();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(request: MutationRequest) -> (std::path::PathBuf, String, MutationWrite) {
        let serial = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("project-mutation-{}-{serial}", std::process::id()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        let path = base.join("settings.json").to_string_lossy().into_owned();
        let mut settings = crate::shell::app_settings::normalize_projects_settings(None);
        settings.install_id = "installation".into();
        settings.list.push(ListEntry {
            id: "project".into(),
            path: root.clone(),
            pinned: false,
            last_used_at: "2025-01-01T00:00:00.000Z".into(),
        });
        settings.trust.set(
            "project",
            StoredTrustRecord {
                decision: "use".into(),
                hashes: JsObject::new(),
            },
        );
        crate::shell::app_settings::write_projects_settings(&path, Some(&settings.to_json()))
            .unwrap();
        let authority = ContextAuthority::capture(&settings, "project").unwrap();
        let write = MutationWrite {
            request,
            authority,
            root,
            data_dir: ".seuoffice".into(),
            platform: process_platform().into(),
            fresh_identity: Some(project_file_text(&ProjectFolderFile {
                version: PROJECT_FORMAT_VERSION,
                id: "project".into(),
                name: "project".into(),
                created_at: "2025-01-01T00:00:00.000Z".into(),
            })),
            rename_date: Some(Ok("2025-01-02T00:00:00.000Z".into())),
            pending_id_fix: false,
            memory_context: Some(MemoryOpContext {
                today: "2025-01-01".into(),
                header: "<!-- memory -->".into(),
            }),
            backup_at: Some(0.0),
            backup_name: Some("memory.20250101-000000.bak.md".into()),
            identity_done: false,
            retained: Vec::new(),
            diagnostics: Vec::new(),
        };
        (base, path, write)
    }

    #[test]
    fn worker_plan_and_acknowledgement_are_send() {
        fn send<T: Send>() {}
        send::<MutationWrite>();
        send::<MutationAcknowledgement>();
    }

    #[test]
    fn saved_memory_has_a_matching_hash_after_the_authorized_effect() {
        let (base, path, write) = fixture(MutationRequest::SaveMemory {
            project_id: "project".into(),
            text: "remember me".into(),
        });
        let root = write.root.clone();
        let ack = write
            .execute_authorized(
                &path,
                |effect| effect(),
                super::super::unguarded_settings_commit,
            )
            .unwrap();
        let MutationAcknowledgement::Done { result, .. } = ack else {
            panic!("unexpected continuation")
        };
        assert_eq!(result.changed, Some(ProjectChangedPart::Memory));
        assert_eq!(
            read_data_text(&root, project_folder_name("memoryFile"), Some(".seuoffice")).as_deref(),
            Some("remember me")
        );
        assert_eq!(
            read_projects_settings(&path)
                .trust
                .get("project")
                .unwrap()
                .hashes
                .get("memory"),
            Some(&hash_text("remember me"))
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn revoked_owner_between_identity_and_memory_never_commits_hash() {
        let (base, path, write) = fixture(MutationRequest::SaveMemory {
            project_id: "project".into(),
            text: "must not be published".into(),
        });
        let root = write.root.clone();
        let mut identity_published = false;
        let result = write.execute_authorized(
            &path,
            |effect| {
                if identity_published {
                    return Err(NodeError {
                        code: None,
                        message: "owner revoked".into(),
                    });
                }
                let result = effect();
                identity_published = read_project_file(&root, Some(".seuoffice")).is_some();
                result
            },
            super::super::unguarded_settings_commit,
        );
        assert!(result.is_err());
        assert!(
            read_data_text(&root, project_folder_name("memoryFile"), Some(".seuoffice")).is_none()
        );
        assert!(
            read_projects_settings(&path)
                .trust
                .get("project")
                .unwrap()
                .hashes
                .get("memory")
                .is_none()
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn consolidation_keeps_original_backup_and_only_the_newest_three() {
        let request = MutationRequest::ApplyMemoryConsolidation {
            project_id: "project".into(),
            entries: vec![
                JsonValue::String("one".into()),
                JsonValue::String("two".into()),
            ],
        };
        let (base, path, write) = fixture(request);
        let data = std::path::Path::new(&write.root).join(".seuoffice");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("memory.md"), "- before (2024-01-01)\n").unwrap();
        for day in ["20240101", "20240102", "20240103"] {
            std::fs::write(data.join(format!("memory.{day}-000000.bak.md")), "older").unwrap();
        }
        let ack = write
            .execute_authorized(
                &path,
                |effect| effect(),
                super::super::unguarded_settings_commit,
            )
            .unwrap();
        let MutationAcknowledgement::Done { result, .. } = ack else {
            panic!("unexpected continuation")
        };
        assert_eq!(result.changed, Some(ProjectChangedPart::Memory));
        assert_eq!(
            std::fs::read_to_string(data.join("memory.20250101-000000.bak.md")).unwrap(),
            "- before (2024-01-01)\n"
        );
        assert_eq!(
            backup_names(data.parent().unwrap().to_str().unwrap(), ".seuoffice").len(),
            3
        );
        assert!(
            std::fs::read_to_string(data.join("memory.md"))
                .unwrap()
                .contains("- two (2025-01-01)")
        );
        assert!(!data.join("memory.20240101-000000.bak.md").exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn memory_edit_uses_latest_text_and_records_only_a_written_hash() {
        let (base, path, write) = fixture(MutationRequest::EditMemory {
            project_id: "project".into(),
            change: ProjectMemoryOp::Replace {
                old: "before".into(),
                new: "after".into(),
            },
            by: MemoryEditor::Person,
        });
        let data = std::path::Path::new(&write.root).join(".seuoffice");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("memory.md"), "keep\n- before (2024-01-01)\n").unwrap();
        let ack = write
            .execute_authorized(
                &path,
                |effect| effect(),
                super::super::unguarded_settings_commit,
            )
            .unwrap();
        let MutationAcknowledgement::Done { result, .. } = ack else {
            panic!("unexpected continuation")
        };
        assert_eq!(result.changed, Some(ProjectChangedPart::Memory));
        let written = std::fs::read_to_string(data.join("memory.md")).unwrap();
        assert_eq!(written, "keep\n- after (2025-01-01)\n");
        assert_eq!(
            read_projects_settings(&path)
                .trust
                .get("project")
                .unwrap()
                .hashes
                .get("memory"),
            Some(&hash_text(&written))
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn backup_collision_preserves_the_competing_backup_and_resumes_exact_plan() {
        let (base, path, write) = fixture(MutationRequest::ApplyMemoryConsolidation {
            project_id: "project".into(),
            entries: vec![JsonValue::String("new".into())],
        });
        let data = std::path::Path::new(&write.root).join(".seuoffice");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("memory.md"), "before").unwrap();
        std::fs::write(data.join("memory.20250101-000000.bak.md"), "competing").unwrap();
        let ack = write
            .execute_authorized(
                &path,
                |effect| effect(),
                super::super::unguarded_settings_commit,
            )
            .unwrap();
        let MutationAcknowledgement::NeedBackupName(mut write) = ack else {
            panic!("expected collision continuation")
        };
        assert!(write.identity_done);
        assert_eq!(write.diagnostics.len(), 1);
        assert_eq!(
            std::fs::read_to_string(data.join("memory.md")).unwrap(),
            "before"
        );
        write.backup_name = Some("memory.20250101-000001.bak.md".into());
        let ack = write
            .execute_authorized(
                &path,
                |effect| effect(),
                super::super::unguarded_settings_commit,
            )
            .unwrap();
        let MutationAcknowledgement::Done { result, .. } = ack else {
            panic!("unexpected continuation")
        };
        assert!(result.warnings.iter().any(|(label, detail)| label
            == "[projects] backup creation collided:"
            && !detail.is_empty()));
        assert_eq!(
            std::fs::read_to_string(data.join("memory.20250101-000000.bak.md")).unwrap(),
            "competing"
        );
        assert_eq!(
            std::fs::read_to_string(data.join("memory.20250101-000001.bak.md")).unwrap(),
            "before"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn removing_absent_ignore_is_a_successful_noop_without_identity_effect() {
        let (base, path, write) = fixture(MutationRequest::RemoveIgnore {
            project_id: "project".into(),
            rel_path: "private".into(),
        });
        let data = std::path::Path::new(&write.root).join(".seuoffice");
        let ack = write
            .execute_authorized(
                &path,
                |effect| effect(),
                super::super::unguarded_settings_commit,
            )
            .unwrap();
        let MutationAcknowledgement::Done {
            result, identified, ..
        } = ack
        else {
            panic!("unexpected continuation")
        };
        assert_eq!(result.changed, Some(ProjectChangedPart::Settings));
        assert!(!identified);
        assert!(!data.exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn rename_preserves_created_at_and_retry_does_not_announce_settings() {
        let (base, path, write) = fixture(MutationRequest::RenameProject {
            project_id: "project".into(),
            name: " Renamed ".into(),
        });
        let data = std::path::Path::new(&write.root).join(".seuoffice");
        std::fs::create_dir(&data).unwrap();
        let before = project_file_text(&ProjectFolderFile {
            version: PROJECT_FORMAT_VERSION,
            id: "project".into(),
            name: "old".into(),
            created_at: "2024-01-01T00:00:00.000Z".into(),
        });
        std::fs::write(data.join("project.json"), before).unwrap();
        let ack = write
            .execute_authorized(
                &path,
                |effect| effect(),
                super::super::unguarded_settings_commit,
            )
            .unwrap();
        let MutationAcknowledgement::Done { result, .. } = ack else {
            panic!("unexpected continuation")
        };
        assert_eq!(result.changed, Some(ProjectChangedPart::Settings));
        let file = read_project_file(data.parent().unwrap().to_str().unwrap(), Some(".seuoffice"))
            .unwrap();
        assert_eq!(file.name, "Renamed");
        assert_eq!(file.created_at, "2024-01-01T00:00:00.000Z");
        std::fs::remove_dir_all(base).unwrap();

        let (base, path, write) = fixture(MutationRequest::RetryUnwritable {
            project_id: "project".into(),
        });
        let ack = write
            .execute_authorized(
                &path,
                |effect| effect(),
                super::super::unguarded_settings_commit,
            )
            .unwrap();
        let MutationAcknowledgement::Done {
            result, identified, ..
        } = ack
        else {
            panic!("unexpected continuation")
        };
        assert!(identified);
        assert_eq!(result.changed, None);
        std::fs::remove_dir_all(base).unwrap();
    }
}

fn identity_needs_fix(
    id: &str,
    root: &str,
    text: &str,
    pending: bool,
    settings: &StoredProjectsSettings,
    platform: &str,
) -> bool {
    let file_id = match json_parse(text) {
        Ok(JsonValue::Object(raw)) => match raw.get("id") {
            Some(JsonValue::String(id)) => Some(id.clone()),
            _ => None,
        },
        _ => None,
    };
    file_id
        .filter(|other| other != id && is_valid_project_id(other))
        .is_some_and(|other| {
            pending
                || entry_in(settings, &other)
                    .is_some_and(|entry| !same_path(&entry.path, root, platform))
        })
}
