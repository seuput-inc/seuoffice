//! Owned chat I/O. Capture guide state on core, run filesystem work on the
//! settings worker, validate its authority before applying its state-only ack.
use super::*;
use crate::shell::projects::chat_store::{ChatGuideChanges, ChatIoGuide, ChatWarningSink};
use crate::shell::projects::write_guard::ChatsDirectory;
use std::sync::{Arc, Mutex};

type Authorize =
    dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError> + Send;
type Commit = dyn FnMut(&str, &str) -> Result<(), NodeError> + Send;
type Clock = dyn FnMut() -> Result<String, GuideError> + Send;

/// Contains no guide callbacks, Rc, listeners, or mutable core state.
pub struct ChatGuideSeed {
    project_id: String,
    authority: Option<ContextAuthority>,
    settings: StoredProjectsSettings,
    platform: String,
    refusal: RefusalContext,
    unwritable: bool,
    pending_id_fix: bool,
    install_id: Option<String>,
    trash: Option<Arc<Mutex<ChatTrashScope>>>,
}

#[derive(Debug, Clone)]
pub struct ChatGuideAuthority {
    project_id: String,
    snapshot: Option<ContextAuthority>,
    platform: String,
    trash: Option<Arc<Mutex<ChatTrashScope>>>,
}

#[derive(Debug)]
struct ChatTrashScope {
    chat_id: String,
    targets: Vec<ChatTrashTarget>,
}

#[derive(Debug)]
struct ChatTrashTarget {
    data_dir: String,
    name: String,
    path: String,
    consumed: bool,
}

fn is_chat_name(name: &str, chat_id: &str) -> bool {
    is_valid_project_id(chat_id)
        && ["chatInfoExt", "chatMessagesExt"]
            .iter()
            .any(|key| name.strip_suffix(project_folder_name(key)) == Some(chat_id))
}

impl ChatGuideAuthority {
    fn validate(
        &self,
        settings: &StoredProjectsSettings,
        changes: &ChatGuideChanges,
    ) -> Result<(), GuideError> {
        let current = match &self.snapshot {
            Some(authority) => authority.is_current(settings),
            None => entry_in(settings, &self.project_id).is_none(),
        };
        let marks_match = changes
            .unwritable
            .iter()
            .chain(&changes.identified)
            .all(|id| id == &self.project_id);
        let unknown_is_read_only = self.snapshot.is_some()
            || changes.unwritable.is_empty() && changes.identified.is_empty();
        if current && marks_match && unknown_is_read_only {
            Ok(())
        } else {
            Err(GuideError::plain("project authority changed while writing"))
        }
    }

    /// Called on the worker before preparing native trash witnesses. A target
    /// must have been minted by this operation's data_path, and is consumed once.
    pub fn authorize_chat_trash(
        &self,
        settings_path: &str,
        path: &str,
        chat_id: &str,
    ) -> Result<(), GuideError> {
        let authority = self
            .snapshot
            .as_ref()
            .ok_or_else(|| unknown_project(&self.project_id))?;
        if !authority.is_current(&read_projects_settings(settings_path)) {
            return Err(GuideError::plain("project authority changed while writing"));
        }
        let scope = self
            .trash
            .as_ref()
            .ok_or_else(|| GuideError::plain("chat trash was not captured"))?;
        let target = {
            let mut scope = scope
                .lock()
                .map_err(|_| GuideError::plain("chat trash capture was poisoned"))?;
            if scope.chat_id != chat_id || !is_valid_project_id(chat_id) {
                return Err(GuideError::plain("chat trash identity changed"));
            }
            let target = scope
                .targets
                .iter_mut()
                .find(|target| target.path == path && is_chat_name(&target.name, chat_id))
                .ok_or_else(|| GuideError::plain("chat trash target was not captured"))?;
            if target.consumed {
                return Err(GuideError::plain("chat trash target was already consumed"));
            }
            target.consumed = true;
            (target.data_dir.clone(), target.name.clone())
        };
        let guard = ProjectWriteGuard::new(&self.platform);
        let directory = guard
            .chats_directory(&authority.path, Some(&target.0), false, &mut |_effect| {
                Err(NodeError {
                    code: None,
                    message: "trash validation cannot create directories".into(),
                })
            })?
            .ok_or_else(|| GuideError::plain("chat trash directory disappeared"))?;
        if directory.path(&target.1)? != path {
            return Err(GuideError::plain("chat trash target changed"));
        }
        Ok(())
    }
}

struct ChatAuthorization {
    settings_path: String,
    authority: Option<ContextAuthority>,
    authorize: RefCell<Box<Authorize>>,
}

impl ChatAuthorization {
    fn check(&self) -> Result<(), GuideError> {
        let current = read_projects_settings(&self.settings_path);
        if self
            .authority
            .as_ref()
            .is_some_and(|authority| authority.is_current(&current))
        {
            Ok(())
        } else {
            Err(GuideError::plain("project authority changed while writing"))
        }
    }

    fn effect(&self, effect: &mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError> {
        // The settings worker serializes authority changes. Do not read settings
        // inside the native owner gate, which covers only the actual effect.
        self.check().map_err(|error| NodeError {
            code: error.code(),
            message: error.message(),
        })?;
        (self.authorize.borrow_mut())(effect)
    }
}

pub struct OwnedChatGuide {
    seed: ChatGuideSeed,
    authorization: ChatAuthorization,
    guard: ProjectWriteGuard,
    commit: Box<Commit>,
    now_iso: Box<Clock>,
    changes: ChatGuideChanges,
    warnings: ChatWarningSink,
}

impl ChatGuideSeed {
    pub fn authority(&self) -> ChatGuideAuthority {
        ChatGuideAuthority {
            project_id: self.project_id.clone(),
            snapshot: self.authority.clone(),
            platform: self.platform.clone(),
            trash: self.trash.clone(),
        }
    }

    /// Enable only for an actual Delete operation, before exporting authority.
    pub fn capture_chat_trash(&mut self, chat_id: &str) -> Result<(), GuideError> {
        if !is_valid_project_id(chat_id) {
            return Err(GuideError::plain("invalid chat id"));
        }
        if self.trash.is_some() {
            return Err(GuideError::plain("chat trash was already captured"));
        }
        self.trash = Some(Arc::new(Mutex::new(ChatTrashScope {
            chat_id: chat_id.to_owned(),
            targets: Vec::with_capacity(2),
        })));
        Ok(())
    }

    /// Construct on the worker. The clock is the real host clock, not a core
    /// callback moved across threads. It is used only for missing project files.
    pub fn into_worker(
        self,
        settings_path: String,
        authorize: impl FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>
        + Send
        + 'static,
        commit: impl FnMut(&str, &str) -> Result<(), NodeError> + Send + 'static,
        now_iso: impl FnMut() -> Result<String, GuideError> + Send + 'static,
    ) -> OwnedChatGuide {
        let guard = ProjectWriteGuard::new(&self.platform);
        let authorization = ChatAuthorization {
            settings_path,
            authority: self.authority.clone(),
            authorize: RefCell::new(Box::new(authorize)),
        };
        OwnedChatGuide {
            seed: self,
            authorization,
            guard,
            commit: Box::new(commit),
            now_iso: Box::new(now_iso),
            changes: ChatGuideChanges::default(),
            warnings: ChatWarningSink::default(),
        }
    }
}

impl ProjectGuide {
    /// Capture outside the native owner gate because this reads settings.
    /// Unknown projects retain the chat store's existing empty-read behavior.
    pub fn prepare_chat_io(&self, project_id: &str) -> Result<ChatGuideSeed, GuideError> {
        let settings = self.read();
        let authority = ContextAuthority::capture(&settings, project_id);
        let install_id = self
            .cached_install_id
            .borrow()
            .as_ref()
            .filter(|id| !id.is_empty())
            .cloned()
            .or_else(|| (!settings.install_id.is_empty()).then(|| settings.install_id.clone()));
        Ok(ChatGuideSeed {
            project_id: project_id.to_owned(),
            authority,
            settings,
            platform: self.platform.clone(),
            refusal: self.refusal_context(),
            unwritable: self.is_unwritable(project_id),
            pending_id_fix: self
                .pending_id_fix
                .borrow()
                .iter()
                .any(|id| id == project_id),
            install_id,
            trash: None,
        })
    }

    /// Call on core outside the native gate. Settings I/O must not be part of
    /// installing the in-memory acknowledgement.
    pub fn validate_chat_io_ack(
        &self,
        authority: &ChatGuideAuthority,
        changes: &ChatGuideChanges,
    ) -> Result<(), GuideError> {
        let settings = self.read();
        authority.validate(&settings, changes)
    }

    /// Run inside the same admitted owner action that installs ChatIo state.
    pub fn finish_chat_io(&self, changes: ChatGuideChanges) {
        for id in changes.identified {
            self.pending_id_fix
                .borrow_mut()
                .retain(|pending| pending != &id);
        }
        for id in changes.unwritable {
            self.mark_unwritable(&id);
        }
    }
}

fn chat_basename(rel_path: &str) -> Result<&str, GuideError> {
    let name = rel_path.strip_prefix("chats/").filter(|name| {
        !name.is_empty() && !name.contains(['/', '\\']) && *name != "." && *name != ".."
    });
    name.ok_or_else(|| {
        GuideError::Refused(ProjectWriteRefused::new(
            ProjectWriteRefusal::BadPath,
            rel_path,
        ))
    })
}

fn needs_identity_fix(
    id: &str,
    root: &str,
    text: &str,
    pending: bool,
    settings: &StoredProjectsSettings,
    platform: &str,
) -> bool {
    let Ok(JsonValue::Object(raw)) = json_parse(text) else {
        return false;
    };
    let Some(JsonValue::String(other)) = raw.get("id") else {
        return false;
    };
    other != id
        && is_valid_project_id(other)
        && (pending
            || entry_in(settings, other)
                .is_some_and(|entry| !same_path(&entry.path, root, platform)))
}

fn identify(
    seed: &ChatGuideSeed,
    guard: &ProjectWriteGuard,
    authorization: &ChatAuthorization,
    data_dir: &str,
    fresh: &str,
) -> Result<Vec<String>, GuideError> {
    let authority = seed
        .authority
        .as_ref()
        .ok_or_else(|| unknown_project(&seed.project_id))?;
    authorization.check()?;
    let (_, retained) = guard.update_data_authorized::<GuideError>(
        &authority.path,
        project_folder_name("projectFile"),
        &mut |current| {
            Ok(match current {
                None => Some(fresh.to_owned()),
                Some(text)
                    if needs_identity_fix(
                        &seed.project_id,
                        &authority.path,
                        text,
                        seed.pending_id_fix,
                        &seed.settings,
                        &seed.platform,
                    ) =>
                {
                    Some(fresh.to_owned())
                }
                Some(_) => None,
            })
        },
        Some(data_dir),
        &mut |effect| authorization.effect(effect),
    )?;
    Ok(retained)
}

impl OwnedChatGuide {
    fn root(&self, id: &str) -> Option<&str> {
        (id == self.seed.project_id).then_some(self.seed.authority.as_ref()?.path.as_str())
    }

    fn data_dir(&self, id: &str, known: Option<&str>) -> Result<String, GuideError> {
        let root = self.root(id).ok_or_else(|| unknown_project(id))?;
        Ok(known
            .map(str::to_owned)
            .unwrap_or_else(|| data_dir_name(root)))
    }

    fn fresh_identity(&mut self, data_dir: &str) -> Result<String, GuideError> {
        let root = self
            .root(&self.seed.project_id)
            .ok_or_else(|| unknown_project(&self.seed.project_id))?;
        let (name, created_at) = match read_project_file(root, Some(data_dir)) {
            Some(file) => (file.name, file.created_at),
            None => (basename(root, None), (self.now_iso)()?),
        };
        Ok(project_file_text(&ProjectFolderFile {
            version: PROJECT_FORMAT_VERSION,
            id: self.seed.project_id.clone(),
            name,
            created_at,
        }))
    }

    fn directory(
        &self,
        data_dir: &str,
        create: bool,
    ) -> Result<Option<ChatsDirectory>, GuideError> {
        self.authorization.check()?;
        let root = self
            .root(&self.seed.project_id)
            .ok_or_else(|| unknown_project(&self.seed.project_id))?;
        Ok(self
            .guard
            .chats_directory(root, Some(data_dir), create, &mut |effect| {
                self.authorization.effect(effect)
            })?)
    }

    fn identified(&mut self, retained: Vec<String>) {
        self.seed.pending_id_fix = false;
        if !self.changes.identified.contains(&self.seed.project_id) {
            self.changes.identified.push(self.seed.project_id.clone());
        }
        for path in retained {
            self.warnings
                .push("[projects] private staging retained:", Some(&path));
        }
    }

    fn failed_write(
        &mut self,
        id: &str,
        error: GuideError,
    ) -> Result<ProjectWriteResult, ChatStoreError> {
        if matches!(error, GuideError::NeedInstallId) {
            return Err(ChatStoreError::need_install_id());
        }
        self.mark_unwritable(id);
        self.warnings.push(
            &format!("[projects] cannot write into project {id}:"),
            Some(&error.message()),
        );
        Ok(ProjectWriteResult::Unwritable)
    }
}

impl ChatStoreGuide for OwnedChatGuide {
    fn append_data(
        &mut self,
        id: &str,
        rel_path: &str,
        text: &str,
        data_dir: Option<&str>,
    ) -> ProjectWriteResult {
        if id != self.seed.project_id || self.seed.authority.is_none() {
            return ProjectWriteResult::Unwritable;
        }
        if self.is_unwritable(id) {
            return ProjectWriteResult::Unwritable;
        }
        let result = (|| -> Result<(), GuideError> {
            let name = chat_basename(rel_path)?;
            let data_dir = self.data_dir(id, data_dir)?;
            let fresh = self.fresh_identity(&data_dir)?;
            let retained = identify(
                &self.seed,
                &self.guard,
                &self.authorization,
                &data_dir,
                &fresh,
            )?;
            self.identified(retained);
            let directory = self
                .directory(&data_dir, true)?
                .ok_or_else(|| GuideError::plain("chat directory was not created"))?;
            directory
                .append_authorized(name, text, &mut |effect| self.authorization.effect(effect))?;
            Ok(())
        })();
        match result {
            Ok(()) => ProjectWriteResult::Ok,
            Err(error) => self
                .failed_write(id, error)
                .unwrap_or(ProjectWriteResult::Unwritable),
        }
    }

    fn update_data(
        &mut self,
        id: &str,
        rel_path: &str,
        change: ChatDataChange<'_>,
        data_dir: Option<&str>,
    ) -> Result<DataUpdateOutcome, ChatStoreError> {
        let name = chat_basename(rel_path).map_err(chat_error)?;
        let data_dir = self.data_dir(id, data_dir).map_err(chat_error)?;
        if self.is_unwritable(id) {
            return Ok(DataUpdateOutcome::Unwritable);
        }
        let result = (|| -> Result<DataUpdateResult, GuideError> {
            let fresh = self.fresh_identity(&data_dir)?;
            // A read/no-op must not create the data or chats directory.
            let directory = self.directory(&data_dir, false)?;
            let mut identified = false;
            let mut retained_identity = Vec::new();
            let install_id = self.seed.install_id.as_ref();
            let mut transform = |current: Option<&str>| -> Result<Option<String>, GuideError> {
                let next = change(current, &mut || {
                    install_id
                        .cloned()
                        .ok_or_else(ChatStoreError::need_install_id)
                })
                .map_err(guide_error)?;
                if next.is_some() {
                    retained_identity.extend(identify(
                        &self.seed,
                        &self.guard,
                        &self.authorization,
                        &data_dir,
                        &fresh,
                    )?);
                    identified = true;
                }
                Ok(next)
            };
            let result = (|| -> Result<(DataUpdateResult, Vec<String>), GuideError> {
                match directory {
                    Some(directory) => directory.update_authorized::<GuideError>(
                        name,
                        &mut transform,
                        &mut |effect| self.authorization.effect(effect),
                    ),
                    None => match transform(None)? {
                        None => Ok((
                            DataUpdateResult {
                                written: false,
                                text: None,
                            },
                            Vec::new(),
                        )),
                        Some(next) => {
                            let directory = self.directory(&data_dir, true)?.ok_or_else(|| {
                                GuideError::plain("chat directory was not created")
                            })?;
                            let mut next = Some(next);
                            directory.update_authorized::<GuideError>(
                                name,
                                &mut |current| {
                                    if current.is_some() {
                                        return Err(GuideError::plain(
                                            "project data changed before update",
                                        ));
                                    }
                                    Ok(next.take())
                                },
                                &mut |effect| self.authorization.effect(effect),
                            )
                        }
                    },
                }
            })();
            drop(transform);
            if identified {
                self.identified(retained_identity);
            }
            let (outcome, retained) = result?;
            for path in retained {
                self.warnings
                    .push("[projects] private staging retained:", Some(&path));
            }
            Ok(outcome)
        })();
        match result {
            Ok(outcome) => Ok(DataUpdateOutcome::Ok {
                written: outcome.written,
                text: outcome.text,
            }),
            Err(error) => self
                .failed_write(id, error)
                .map(|_| DataUpdateOutcome::Unwritable),
        }
    }

    fn data_path(&mut self, id: &str, rel_path: &str, data_dir: Option<&str>) -> Option<String> {
        let name = chat_basename(rel_path).ok()?;
        let data_dir = self.data_dir(id, data_dir).ok()?;
        let path = match self.directory(&data_dir, false).ok()? {
            Some(directory) => directory.path(name).ok()?,
            None => self
                .guard
                .data_path(self.root(id)?, rel_path, Some(&data_dir))
                .ok()?,
        };
        if let Some(scope) = &self.seed.trash {
            let mut scope = scope.lock().ok()?;
            if !is_chat_name(name, &scope.chat_id) {
                return None;
            }
            if scope
                .targets
                .first()
                .is_some_and(|target| target.data_dir != data_dir)
            {
                return None;
            }
            if !scope.targets.iter().any(|target| target.path == path) {
                if scope.targets.len() >= 2 {
                    return None;
                }
                scope.targets.push(ChatTrashTarget {
                    data_dir,
                    name: name.to_owned(),
                    path: path.clone(),
                    consumed: false,
                });
            }
        }
        Some(path)
    }

    fn install_id(&mut self) -> Result<String, ChatStoreError> {
        self.seed
            .install_id
            .clone()
            .ok_or_else(ChatStoreError::need_install_id)
    }

    fn trust_state(&mut self, id: &str) -> ProjectTrustState {
        match self
            .seed
            .settings
            .trust
            .get(id)
            .map(|record| decision_of(&record.decision))
        {
            Some(ProjectTrustDecision::Use) => ProjectTrustState::Use,
            Some(ProjectTrustDecision::Without) => ProjectTrustState::Without,
            None => ProjectTrustState::Unconfirmed,
        }
    }

    fn is_unwritable(&mut self, id: &str) -> bool {
        id == self.seed.project_id
            && (self.seed.unwritable || self.changes.unwritable.iter().any(|marked| marked == id))
    }

    fn mark_unwritable(&mut self, id: &str) {
        if id == self.seed.project_id && !self.changes.unwritable.iter().any(|marked| marked == id)
        {
            self.changes.unwritable.push(id.to_owned());
        }
    }

    fn project_root(&mut self, id: &str) -> Option<String> {
        self.root(id).map(str::to_owned)
    }

    fn record_outside_chat(
        &mut self,
        doc_path: &str,
        project_id: &str,
        chat_id: &str,
    ) -> Result<(), String> {
        if !is_valid_project_id(chat_id) {
            return Err("invalid chat id".into());
        }
        if !is_absolute(doc_path) {
            return Err("expected an absolute document path".into());
        }
        if self.root(project_id).is_none() {
            return Err(unknown_project(project_id).message());
        }
        self.authorization
            .check()
            .map_err(|error| error.message())?;
        let authorization = &self.authorization;
        let commit = &mut self.commit;
        crate::shell::app_settings::update_projects_settings_guarded(
            &authorization.settings_path,
            |settings| {
                record_outside_chat(
                    settings,
                    &canonical_path(doc_path),
                    project_id,
                    chat_id,
                    &self.seed.platform,
                )
            },
            |staged, target| {
                authorization.check().map_err(|error| NodeError {
                    code: error.code(),
                    message: error.message(),
                })?;
                commit(staged, target)
            },
        )
        .map_err(|error| error.message)?;
        Ok(())
    }

    fn document_relative_path(&mut self, id: &str, doc_path: &str) -> Option<String> {
        relative_inside(
            self.root(id)?,
            &canonical_path(doc_path),
            &self.seed.platform,
        )
    }

    fn get_selection(&mut self, target: &SelectionTarget) -> ProjectSelection {
        let settings = read_projects_settings(&self.authorization.settings_path);
        let resolved = match target {
            SelectionTarget::Document { path } if is_absolute(path) => {
                ResolvedSelectionTarget::Document {
                    doc_key: canonical_path(path),
                }
            }
            SelectionTarget::Document { .. } => {
                return ProjectSelection {
                    project_id: None,
                    initial_chat_id: None,
                    unregistered_project_hint: None,
                };
            }
            SelectionTarget::Untitled { project_id } => ResolvedSelectionTarget::Untitled {
                project_id: project_id.clone(),
            },
            SelectionTarget::Home => ResolvedSelectionTarget::Home,
        };
        let mut selection = selection_for(&settings, &resolved, &self.seed.platform);
        if let ResolvedSelectionTarget::Document { doc_key } = &resolved {
            selection.unregistered_project_hint =
                find_unregistered_project(doc_key, &settings, &self.seed.refusal);
        }
        selection
    }
}

impl ChatIoGuide for OwnedChatGuide {
    fn set_io_warning_sink(&mut self, sink: ChatWarningSink) {
        self.warnings = sink;
    }
    fn take_io_changes(&mut self) -> ChatGuideChanges {
        std::mem::take(&mut self.changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Fixture {
        base: PathBuf,
        root: String,
        path: String,
        seed: ChatGuideSeed,
    }

    fn fixture() -> Fixture {
        let serial = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("chat-guide-{}-{serial}", std::process::id()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        let path = base.join("settings.json").to_string_lossy().into_owned();
        let platform = process_platform().to_owned();
        let mut settings = crate::shell::app_settings::normalize_projects_settings(None);
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
        let seed = ChatGuideSeed {
            project_id: "project".into(),
            authority: ContextAuthority::capture(&settings, "project"),
            settings,
            platform: platform.clone(),
            refusal: RefusalContext {
                home_dir: base.join("home").to_string_lossy().into_owned(),
                user_data: base.join("user-data").to_string_lossy().into_owned(),
                platform,
            },
            unwritable: false,
            pending_id_fix: false,
            install_id: None,
            trash: None,
        };
        Fixture {
            base,
            root,
            path,
            seed,
        }
    }

    fn clock() -> Result<String, GuideError> {
        Ok("2025-01-02T00:00:00.000Z".into())
    }

    #[test]
    fn worker_capture_and_adapter_are_send() {
        fn send<T: Send>() {}
        send::<ChatGuideSeed>();
        send::<ChatGuideAuthority>();
        send::<OwnedChatGuide>();
    }

    #[test]
    fn read_paths_and_missing_install_id_have_no_identity_or_clock_effect() {
        let Fixture {
            base,
            root,
            path,
            seed,
        } = fixture();
        let mut guide = seed.into_worker(
            path,
            |effect| effect(),
            unguarded_settings_commit,
            || {
                panic!("read-only chat operations must not request an identity date");
            },
        );
        assert_eq!(
            guide.project_root("project").as_deref(),
            Some(root.as_str())
        );
        assert!(
            guide
                .data_path("project", "chats/chat.json", None)
                .is_some()
        );
        assert_eq!(
            guide.install_id().unwrap_err().kind,
            ChatStoreErrorKind::NeedInstallId
        );
        assert!(!std::path::Path::new(&root).join(".seuoffice").exists());
        let changes = guide.take_io_changes();
        assert!(changes.identified.is_empty());
        assert!(changes.unwritable.is_empty());
        assert!(guide.warnings.drain().is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn no_op_update_creates_neither_identity_nor_chat_directory() {
        let Fixture {
            base,
            root,
            path,
            seed,
        } = fixture();
        let mut guide = seed.into_worker(path, |effect| effect(), unguarded_settings_commit, clock);
        let answer = guide
            .update_data(
                "project",
                "chats/chat.json",
                &mut |current, _| {
                    assert!(current.is_none());
                    Ok(None)
                },
                None,
            )
            .unwrap();
        assert_eq!(
            answer,
            DataUpdateOutcome::Ok {
                written: false,
                text: None
            }
        );
        assert!(!std::path::Path::new(&root).join(".seuoffice").exists());
        assert!(guide.take_io_changes().identified.is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn append_keeps_original_lines_and_materializes_project_identity() {
        let Fixture {
            base,
            root,
            path,
            seed,
        } = fixture();
        let authority = seed.authority();
        let mut guide = seed.into_worker(
            path.clone(),
            |effect| effect(),
            unguarded_settings_commit,
            clock,
        );
        assert_eq!(
            guide.append_data("project", "chats/chat.jsonl", "one\n", None),
            ProjectWriteResult::Ok
        );
        assert_eq!(
            guide.append_data("project", "chats/chat.jsonl", "two\n", None),
            ProjectWriteResult::Ok
        );
        assert_eq!(
            std::fs::read_to_string(
                std::path::Path::new(&root).join(".seuoffice/chats/chat.jsonl")
            )
            .unwrap(),
            "one\ntwo\n"
        );
        let identity = read_project_file(&root, Some(".seuoffice")).unwrap();
        assert_eq!(identity.id, "project");
        assert_eq!(identity.created_at, "2025-01-02T00:00:00.000Z");
        let changes = guide.take_io_changes();
        assert_eq!(changes.identified, ["project"]);
        assert!(changes.unwritable.is_empty());
        authority
            .validate(&read_projects_settings(&path), &changes)
            .unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn install_id_continuation_does_not_write_or_mark_unwritable() {
        let Fixture {
            base,
            root,
            path,
            seed,
        } = fixture();
        let mut guide = seed.into_worker(path, |effect| effect(), unguarded_settings_commit, clock);
        let error = guide
            .update_data(
                "project",
                "chats/chat.json",
                &mut |_, install_id| Ok(Some(install_id()?)),
                None,
            )
            .unwrap_err();
        assert_eq!(error.kind, ChatStoreErrorKind::NeedInstallId);
        assert!(!std::path::Path::new(&root).join(".seuoffice").exists());
        let changes = guide.take_io_changes();
        assert!(changes.identified.is_empty());
        assert!(changes.unwritable.is_empty());
        assert!(guide.warnings.drain().is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn owner_revoked_after_identity_never_appends_chat_but_retains_identity_ack() {
        let Fixture {
            base,
            root,
            path,
            seed,
        } = fixture();
        let identity = std::path::Path::new(&root).join(".seuoffice/project.json");
        let mut revoked = false;
        let mut guide = seed.into_worker(
            path,
            move |effect| {
                if revoked {
                    return Err(NodeError {
                        code: None,
                        message: "owner revoked".into(),
                    });
                }
                let result = effect();
                if identity.exists() {
                    revoked = true;
                }
                result
            },
            unguarded_settings_commit,
            clock,
        );
        assert_eq!(
            guide.append_data("project", "chats/chat.jsonl", "forbidden\n", None),
            ProjectWriteResult::Unwritable
        );
        assert!(read_project_file(&root, Some(".seuoffice")).is_some());
        assert!(
            !std::path::Path::new(&root)
                .join(".seuoffice/chats/chat.jsonl")
                .exists()
        );
        let changes = guide.take_io_changes();
        assert_eq!(changes.identified, ["project"]);
        assert_eq!(changes.unwritable, ["project"]);
        assert!(guide.warnings.drain().iter().any(|(_, detail)| {
            detail
                .as_deref()
                .is_some_and(|detail| detail.contains("owner revoked"))
        }));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn stale_project_authority_prevents_all_native_write_admissions() {
        let Fixture {
            base,
            root,
            path,
            seed,
        } = fixture();
        let authority = seed.authority();
        let mut settings = read_projects_settings(&path);
        settings.trust.get_mut("project").unwrap().decision = "without".into();
        crate::shell::app_settings::write_projects_settings(&path, Some(&settings.to_json()))
            .unwrap();
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let mut guide = seed.into_worker(
            path,
            move |effect| {
                count.fetch_add(1, Ordering::SeqCst);
                effect()
            },
            unguarded_settings_commit,
            clock,
        );
        assert_eq!(
            guide.append_data("project", "chats/chat.jsonl", "forbidden\n", None),
            ProjectWriteResult::Unwritable
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert!(!std::path::Path::new(&root).join(".seuoffice").exists());
        assert!(
            authority
                .validate(&settings, &guide.take_io_changes())
                .is_err()
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn unknown_capture_and_cross_project_marks_cannot_be_acknowledged_as_current() {
        let Fixture {
            base, path, seed, ..
        } = fixture();
        let mut settings = read_projects_settings(&path);
        let capture = ChatGuideAuthority {
            project_id: "later".into(),
            snapshot: None,
            platform: process_platform().into(),
            trash: None,
        };
        capture
            .validate(&settings, &ChatGuideChanges::default())
            .unwrap();
        settings.list.push(ListEntry {
            id: "later".into(),
            path: base.join("later").to_string_lossy().into_owned(),
            pinned: false,
            last_used_at: String::new(),
        });
        assert!(
            capture
                .validate(&settings, &ChatGuideChanges::default())
                .is_err()
        );
        let changes = ChatGuideChanges {
            unwritable: vec!["another-project".into()],
            ..Default::default()
        };
        assert!(seed.authority().validate(&settings, &changes).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn concurrent_creation_during_change_is_not_overwritten() {
        let Fixture {
            base,
            root,
            path,
            seed,
        } = fixture();
        let target = std::path::Path::new(&root).join(".seuoffice/chats/chat.json");
        let mut guide = seed.into_worker(path, |effect| effect(), unguarded_settings_commit, clock);
        let answer = guide
            .update_data(
                "project",
                "chats/chat.json",
                &mut |current, _| {
                    assert!(current.is_none());
                    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                    std::fs::write(&target, "concurrent").unwrap();
                    Ok(Some("ours".into()))
                },
                None,
            )
            .unwrap();
        assert_eq!(answer, DataUpdateOutcome::Unwritable);
        assert_eq!(std::fs::read_to_string(target).unwrap(), "concurrent");
        let changes = guide.take_io_changes();
        assert_eq!(changes.identified, ["project"]);
        assert_eq!(changes.unwritable, ["project"]);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn cached_legacy_data_directory_does_not_split_identity_and_chat_writes() {
        let Fixture {
            base,
            root,
            path,
            seed,
        } = fixture();
        let old = crate::packages::i18n::OLD_PROJECT_DATA_DIR_NAMES[0];
        let legacy = std::path::Path::new(&root).join(old);
        std::fs::create_dir(&legacy).unwrap();
        let identity = project_file_text(&ProjectFolderFile {
            version: PROJECT_FORMAT_VERSION,
            id: "project".into(),
            name: "preserved".into(),
            created_at: "2024-01-01T00:00:00.000Z".into(),
        });
        std::fs::write(legacy.join("project.json"), &identity).unwrap();
        std::fs::create_dir(std::path::Path::new(&root).join(".seuoffice")).unwrap();
        let mut guide = seed.into_worker(
            path,
            |effect| effect(),
            unguarded_settings_commit,
            || {
                panic!("an existing project identity must preserve its date");
            },
        );
        assert_eq!(
            guide.append_data("project", "chats/chat.jsonl", "legacy\n", Some(old)),
            ProjectWriteResult::Ok
        );
        assert_eq!(
            std::fs::read_to_string(legacy.join("chats/chat.jsonl")).unwrap(),
            "legacy\n"
        );
        assert_eq!(
            std::fs::read_to_string(legacy.join("project.json")).unwrap(),
            identity
        );
        assert!(
            !std::path::Path::new(&root)
                .join(".seuoffice/project.json")
                .exists()
        );
        assert!(
            !std::path::Path::new(&root)
                .join(".seuoffice/chats")
                .exists()
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn host_and_guide_warnings_keep_their_actual_order() {
        let Fixture {
            base, path, seed, ..
        } = fixture();
        let sink = ChatWarningSink::default();
        let mut guide = seed.into_worker(
            path,
            |_effect| {
                Err(NodeError {
                    code: Some("EACCES"),
                    message: "owner refused".into(),
                })
            },
            unguarded_settings_commit,
            clock,
        );
        guide.set_io_warning_sink(sink.clone());
        sink.push("before guide", Some("host read warning"));
        assert_eq!(
            guide.append_data("project", "chats/chat.jsonl", "no write\n", None),
            ProjectWriteResult::Unwritable
        );
        sink.push("after guide", None);
        let warnings = sink.drain();
        assert_eq!(warnings.len(), 3);
        assert_eq!(
            warnings[0],
            ("before guide".into(), Some("host read warning".into()))
        );
        assert_eq!(
            warnings[1].0,
            "[projects] cannot write into project project:"
        );
        assert!(
            warnings[1]
                .1
                .as_deref()
                .is_some_and(|detail| detail.contains("owner refused"))
        );
        assert_eq!(warnings[2], ("after guide".into(), None));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn trash_requires_minted_exact_chat_target_and_cannot_be_replayed() {
        let Fixture {
            base,
            path,
            mut seed,
            ..
        } = fixture();
        seed.capture_chat_trash("chat").unwrap();
        let authority = seed.authority();
        let mut guide = seed.into_worker(
            path.clone(),
            |effect| effect(),
            unguarded_settings_commit,
            clock,
        );
        assert_eq!(
            guide.append_data("project", "chats/chat.jsonl", "keep\n", None),
            ProjectWriteResult::Ok
        );
        let target = guide
            .data_path("project", "chats/chat.jsonl", None)
            .unwrap();
        assert!(
            authority
                .authorize_chat_trash(&path, &target, "other")
                .is_err()
        );
        let sibling = target.replace("chat.jsonl", "other.jsonl");
        assert!(
            authority
                .authorize_chat_trash(&path, &sibling, "chat")
                .is_err()
        );
        authority
            .authorize_chat_trash(&path, &target, "chat")
            .unwrap();
        assert!(
            authority
                .authorize_chat_trash(&path, &target, "chat")
                .is_err()
        );
        assert_eq!(
            guide
                .data_path("project", "chats/chat.jsonl", None)
                .as_deref(),
            Some(target.as_str())
        );
        assert!(
            authority
                .authorize_chat_trash(&path, &target, "chat")
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "keep\n");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn trash_scope_stays_in_the_original_states_legacy_data_directory() {
        let Fixture {
            base,
            root,
            path,
            mut seed,
        } = fixture();
        seed.capture_chat_trash("chat").unwrap();
        let authority = seed.authority();
        let old = crate::packages::i18n::OLD_PROJECT_DATA_DIR_NAMES[0];
        let current_chats = std::path::Path::new(&root).join(".seuoffice/chats");
        std::fs::create_dir_all(&current_chats).unwrap();
        std::fs::write(current_chats.join("chat.jsonl"), "current\n").unwrap();
        let mut guide = seed.into_worker(
            path.clone(),
            |effect| effect(),
            unguarded_settings_commit,
            clock,
        );
        assert_eq!(
            guide.append_data("project", "chats/chat.jsonl", "legacy\n", Some(old)),
            ProjectWriteResult::Ok
        );
        let target = guide
            .data_path("project", "chats/chat.jsonl", Some(old))
            .unwrap();
        assert!(
            guide
                .data_path("project", "chats/chat.jsonl", Some(".seuoffice"))
                .is_none()
        );
        assert!(
            authority
                .authorize_chat_trash(
                    &path,
                    current_chats.join("chat.jsonl").to_str().unwrap(),
                    "chat"
                )
                .is_err()
        );
        authority
            .authorize_chat_trash(&path, &target, "chat")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(current_chats.join("chat.jsonl")).unwrap(),
            "current\n"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn non_delete_capture_cannot_grant_trash_even_for_a_valid_chat_path() {
        let Fixture {
            base, path, seed, ..
        } = fixture();
        let authority = seed.authority();
        let mut guide = seed.into_worker(
            path.clone(),
            |effect| effect(),
            unguarded_settings_commit,
            clock,
        );
        assert_eq!(
            guide.append_data("project", "chats/chat.jsonl", "keep\n", None),
            ProjectWriteResult::Ok
        );
        let target = guide
            .data_path("project", "chats/chat.jsonl", None)
            .unwrap();
        assert!(
            authority
                .authorize_chat_trash(&path, &target, "chat")
                .is_err()
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn revoked_project_authority_cannot_consume_a_minted_trash_target() {
        let Fixture {
            base,
            path,
            mut seed,
            ..
        } = fixture();
        seed.capture_chat_trash("chat").unwrap();
        let authority = seed.authority();
        let mut guide = seed.into_worker(
            path.clone(),
            |effect| effect(),
            unguarded_settings_commit,
            clock,
        );
        assert_eq!(
            guide.append_data("project", "chats/chat.jsonl", "keep\n", None),
            ProjectWriteResult::Ok
        );
        let target = guide
            .data_path("project", "chats/chat.jsonl", None)
            .unwrap();
        let mut settings = read_projects_settings(&path);
        settings.trust.get_mut("project").unwrap().decision = "without".into();
        crate::shell::app_settings::write_projects_settings(&path, Some(&settings.to_json()))
            .unwrap();
        assert!(
            authority
                .authorize_chat_trash(&path, &target, "chat")
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "keep\n");
        std::fs::remove_dir_all(base).unwrap();
    }
}
