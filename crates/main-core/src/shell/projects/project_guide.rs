//! `apps/shell/src/main/projects/project-guide.ts`: the project guide
//! (decision 0088): the one place in the shell main process that knows the
//! project list, reads a project folder's instructions and memory, decides
//! identity and trust, and writes into the folder (through the write guard).
//! It holds no Electron objects: the first-time trust prompt is a step the
//! caller shows (see below), and the IPC layer passes the calling window's
//! document path.
//!
//! This file holds the exported types and constants of the TypeScript file,
//! then the `ProjectGuide` class.
//!
//! What the TypeScript awaits is a plain synchronous call here, and the
//! optional callbacks of `ProjectGuideDeps` are optional boxed closures, with
//! one exception: the `await askTrust(prompt)` of `addFolder`,
//! `relocateProject`, `createProject`, `adoptFolderHint` and `decideTrust`.
//! Showing that prompt can wait for a window's events (the person comes back
//! to the page after [view all]), and those arrive on the thread the guide
//! runs on. So a method that may ask returns a [`GuideStep`]: `Done(result)`,
//! or `AskTrust { prompt, pending }`, where the caller shows the prompt and
//! calls the `resume_*` method of that operation with `pending` and the
//! [`ProjectTrustAnswer`] (`None`: closed). The resume continues exactly where
//! the `await` returned: what the TypeScript reads after the await (the
//! settings, the clock) is read at the resume. A call asks at most once, so a
//! resume answers a final result, never another step.
//!
//! The guide is a struct the caller owns. Its state (the unwritable projects,
//! the pending use times, the install id) and the callbacks of the host sit
//! behind `RefCell`s, so every method takes `&self`: one guide is shared (an
//! `Rc`) between the chat store, the tools, the file index and the new
//! document placer, as the one object of the TypeScript is, and a write the
//! guide runs may call the guide again (the chat store asks for the install
//! id while its change holds the write).
//!
//! The use-time timer of `touch` is explicit: the guide records when it
//! would have armed `setTimeout` ([`ProjectGuide::touch_timer`]) and the
//! caller calls [`ProjectGuide::flush_touches`] when that time comes (and at
//! quit).
//!
//! Two defaults of the TypeScript have no counterpart: the screen's local time
//! zone (`localDate`, `memoryBackupName` read the `Date` in local time), which
//! the host answers (`ProjectGuideDeps::local_date`), and `console.warn`, which
//! is `ProjectGuideDeps::warn` (absent: nothing is printed).

mod mutations;
pub use mutations::{
    MutationAcknowledgement, MutationPreparation, MutationRequest, MutationResult, MutationWrite,
};
mod chat_io;
pub use chat_io::{ChatGuideAuthority, ChatGuideSeed, OwnedChatGuide};
mod new_document;
pub use new_document::{NewDocumentAuthority, NewDocumentIo};

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::LazyLock;

use icu_collator::options::CollatorOptions;
use icu_collator::{Collator, CollatorBorrowed, CollatorPreferences};
use jscompat::node_path::host::{basename, is_absolute, join};
use jscompat::{
    JsMap, JsObject, JsonValue, RegexLimit, RegexSyntaxError, base64, js_regex, json_parse,
    math_max, math_random, sort_by_number, to_iso_string, trim, truthy,
};
use serde::{Deserialize, Serialize};

use super::chat_store::{ChatDataChange, ChatStoreError, ChatStoreErrorKind, ChatStoreGuide};
use super::cloud_files::{SyncFolderOptions, is_under_known_sync_folder};
use super::context::assemble_trusted_context;
use super::folder_files::{
    clean_project_name, data_dir_name, hash_text, plain_folder_entries, project_file_text,
    read_data_text, read_project_file,
};
use super::identity::{ClassifyInput, FolderClassification, classify_folder, rebase_opt_outs};
use super::ignore::{
    IgnoreMatcher, IgnoreOptions, add_ignore_line, compile_ignore, ignore_folds_case,
    remove_ignore_line,
};
use super::memory::{
    LocalDateParts, MemoryOpContext, MemoryOpRefusal, MemoryOpResult, apply_memory_op,
    build_memory_text, local_date, memory_backup_name, memory_entries, memory_header,
};
use super::new_documents::NewDocumentsGuide;
use super::paths::{
    RefusalContext, canonical_path, folder_refusal, is_inside_folder, location_refusal,
    relative_inside, same_path,
};
use super::project_index::ProjectIndexGuide;
use super::project_tools::ProjectToolsGuide;
use super::selection::{
    ResolvedSelectionTarget, delete_key, drop_from_list, find_unregistered_project,
    record_outside_chat, record_selection, selection_for,
};
use super::trust::{TrustScanInput, read_hashed_texts, scan_for_trust};
use super::visibility::is_ai_visible_path;
use super::write_guard::{
    DataChange, DataUpdateResult, ProjectWriteGuard, ProjectWriteRefusal, ProjectWriteRefused,
    WriteGuardError,
};
use crate::node_fs::{NodeError, namespaced};
use crate::packages::ai_provider::agent_session_policy::process_platform;
use crate::packages::electron_utils::default_save_dir::Thrown;
use crate::packages::project_store::{
    AddFolderResult, AddFolderStage, ApplyMemoryConsolidationArgs, ApplyMemoryConsolidationResult,
    CreateProjectResult, DecideTrustResult, FolderIdentity, FolderRefusalReason,
    PROJECT_FORMAT_VERSION, PROJECT_NAME_MAX_CHARS, ProjectContextBundle,
    ProjectContextInstructions, ProjectContextMemory, ProjectContextResult, ProjectDetail,
    ProjectFileEntry, ProjectFolderFile, ProjectHashedFile, ProjectId, ProjectListItem,
    ProjectMemoryEditResult, ProjectMemoryOp, ProjectMemoryRefusal, ProjectNotice,
    ProjectSelection, ProjectSettings, ProjectTrustAnswer, ProjectTrustDecision,
    ProjectTrustPrompt, ProjectTrustRecord, ProjectTrustState, ProjectWriteResult,
    SyncFolderService, UndoMemoryConsolidationArgs, UndoMemoryConsolidationResult,
    is_valid_project_id, memory_backup_pattern, project_folder_name,
};
use crate::shell::app_settings::{
    ProjectListEntry as ListEntry, ProjectSwitches, ProjectTrustRecord as StoredTrustRecord,
    StoredProjectsSettings, read_projects_settings, update_projects_settings,
};
use crate::shell::rename_validation::is_valid_rename_name;

pub use super::context::INSTRUCTIONS_LIMIT_CHARS;
pub use super::memory::MEMORY_SOFT_LIMIT_CHARS;

fn unguarded_settings_commit(from: &str, to: &str) -> Result<(), NodeError> {
    std::fs::rename(from, to)
        .map_err(|error| crate::node_fs::node_error2(&error, "rename", from, to))
}
/// What a guide method that may show the first-time trust prompt (spec 5.10)
/// answers: the result, or the prompt to show and the state to resume with
#[derive(Debug, Clone, PartialEq)]
pub enum GuideStep<T, P> {
    Done(T),
    /// `await askTrust(prompt)`: the caller shows `prompt`, then calls the
    /// `resume_*` method of the operation with `pending` and the person's
    /// answer (`None` when the prompt was closed). The prompt is boxed to keep
    /// the two variants of a step near in size.
    AskTrust {
        prompt: Box<ProjectTrustPrompt>,
        pending: P,
    },
}

/// What `adopt` has settled before it asks: the folder and its identity
#[derive(Debug, Clone, PartialEq)]
struct Adoption {
    root: String,
    id: ProjectId,
    name: String,
    identity: FolderIdentity,
    previous: Option<crate::packages::project_store::ProjectListEntry>,
    copied_from: Option<ProjectId>,
    stale_at_path: Option<ProjectId>,
    expected_trust: Option<StoredTrustRecord>,
}
impl Adoption {
    fn validate(
        &self,
        settings: &StoredProjectsSettings,
        platform: &str,
    ) -> Result<(), GuideError> {
        if let Some(stale) = &self.stale_at_path
            && let Some(entry) = settings.list.iter().find(|entry| &entry.id == stale)
            && !same_path(&entry.path, &self.root, platform)
        {
            return Err(GuideError::plain("project changed while awaiting adoption"));
        }
        let taken = settings.list.iter().any(|item| {
            item.id != self.id
                && self.stale_at_path.as_ref() != Some(&item.id)
                && same_path(&item.path, &self.root, platform)
        });
        if taken {
            return Ok(());
        }
        if settings.trust.get(&self.id) != self.expected_trust.as_ref() {
            return Err(GuideError::plain(
                "project trust changed while awaiting adoption",
            ));
        }
        if let Some(previous) = &self.previous {
            let current = settings.list.iter().find(|item| item.id == self.id);
            if !current.is_some_and(|item| {
                same_path(&item.path, &previous.path, platform)
                    || same_path(&item.path, &self.root, platform)
            }) {
                return Err(GuideError::plain("project changed while awaiting adoption"));
            }
        } else if settings
            .list
            .iter()
            .any(|item| item.id == self.id && !same_path(&item.path, &self.root, platform))
        {
            return Err(GuideError::plain("project changed while awaiting adoption"));
        }
        Ok(())
    }
}

pub enum AdoptStart {
    Done(AddFolderResult),
    NeedInstallId(AdoptScan),
    Ask {
        prompt: Box<ProjectTrustPrompt>,
        pending: PendingAdopt,
    },
    Ready(AdoptPlan),
}

pub struct AdoptScan {
    adoption: Adoption,
}

pub struct AdoptPlan {
    adoption: Adoption,
    trust: Option<StoredTrustRecord>,
}

pub enum AdoptPreparation {
    Done(AddFolderResult),
    Write(AdoptWrite),
}

/// Owned post-prompt adoption work for the shared settings queue.
pub struct AdoptWrite {
    adoption: Adoption,
    trust: Option<StoredTrustRecord>,
    at: String,
    platform: String,
    notice: Option<ProjectNotice>,
    copy_file: Option<String>,
}

pub struct AdoptAcknowledgement {
    original_id: String,
    final_id: String,
    identity: FolderIdentity,
    entry: ListEntry,
    notice: Option<ProjectNotice>,
    copy_failure: Option<String>,
    retained_private_dir: Option<String>,
}

pub struct AdoptionCompletion {
    original_id: String,
    final_id: String,
    copy_failed: bool,
    result: AddFolderResult,
    warnings: Vec<(String, String)>,
}

impl AdoptWrite {
    pub fn execute(self, path: &str) -> Result<AdoptAcknowledgement, GuideError> {
        self.execute_authorized(path, |effect| effect(), unguarded_settings_commit)
    }

    pub fn execute_authorized(
        mut self,
        path: &str,
        mut authorize: impl FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
        commit: impl FnOnce(&str, &str) -> Result<(), NodeError>,
    ) -> Result<AdoptAcknowledgement, GuideError> {
        let mut next = read_projects_settings(path);
        self.adoption.validate(&next, &self.platform)?;
        let occupied = next.list.iter().any(|item| {
            item.id != self.adoption.id
                && self.adoption.stale_at_path.as_ref() != Some(&item.id)
                && same_path(&item.path, &self.adoption.root, &self.platform)
        });
        let mut copy_failure = None;
        let mut retained_private_dir = None;
        if !occupied {
            if let Some(file) = &self.copy_file {
                let mut denied = None;
                let result = ProjectWriteGuard::new(&self.platform).write_data_authorized(
                    &self.adoption.root,
                    project_folder_name("projectFile"),
                    file.as_bytes(),
                    None,
                    &mut |effect| {
                        let mut effect_failed = false;
                        let mut called = false;
                        let result = authorize(&mut || {
                            called = true;
                            let result = effect();
                            effect_failed = result.is_err();
                            result
                        });
                        if !called || !effect_failed {
                            denied = result.as_ref().err().cloned();
                        }
                        result
                    },
                );
                if let Some(error) = denied {
                    return Err(error.into());
                }
                match result {
                    Ok(path) if !path.is_empty() => retained_private_dir = Some(path),
                    Ok(_) => {}
                    Err(error) => copy_failure = Some(GuideError::from(error).message()),
                }
            }
        }
        let Adoption {
            root,
            id,
            identity,
            previous,
            copied_from,
            stale_at_path,
            ..
        } = self.adoption;
        let mut final_id = id.clone();
        let mut final_identity = identity;
        let mut show_notice = false;
        {
            if let Some(stale) = &stale_at_path {
                next.list.retain(|item| item.id != *stale);
            }
            if let Some(taken) = next
                .list
                .iter()
                .find(|item| item.id != id && same_path(&item.path, &root, &self.platform))
            {
                final_id = taken.id.clone();
                final_identity = FolderIdentity::Existing;
            } else {
                if next.list.iter().any(|item| item.id == id) {
                    if let Some(previous) = &previous
                        && !same_path(&previous.path, &root, &self.platform)
                    {
                        rebase_opt_outs(&mut next, &id, &previous.path, &root, &self.platform);
                    }
                    if let Some(listed) = next.list.iter_mut().find(|item| item.id == id) {
                        listed.path = root.clone();
                        listed.last_used_at = self.at.clone();
                    }
                } else {
                    next.list.push(ListEntry {
                        id: id.clone(),
                        path: root.clone(),
                        pinned: false,
                        last_used_at: self.at.clone(),
                    });
                }
                if let Some(record) = self.trust.take() {
                    next.trust.set(id.as_str(), record);
                }
                if let Some(source_id) = &copied_from {
                    if let Some(source) = next.trust.get(source_id).map(clone_trust)
                        && next.trust.get(&id).is_none()
                    {
                        next.trust.set(id.as_str(), source);
                    }
                    if let Some(switches) = next.settings.get(source_id).cloned()
                        && next.settings.get(&id).is_none()
                    {
                        next.settings.set(id.as_str(), switches);
                    }
                }
            }
            if self.notice.is_some()
                && !next
                    .sync_notice_shown
                    .iter()
                    .any(|shown| shown == &final_id)
            {
                next.sync_notice_shown.push(final_id.clone());
                show_notice = true;
            }
        }
        crate::shell::app_settings::write_projects_settings_guarded(
            path,
            Some(&next.to_json()),
            commit,
        )?;
        let next = read_projects_settings(path);
        let entry = entry_in(&next, &final_id)
            .ok_or_else(|| GuideError::plain(format!("project {final_id} was not listed")))?;
        Ok(AdoptAcknowledgement {
            original_id: id,
            final_id,
            identity: final_identity,
            entry,
            notice: if show_notice { self.notice } else { None },
            copy_failure,
            retained_private_dir,
        })
    }
}

/// `addFolder`, `relocateProject` or `createProject` waiting for the trust
/// answer: [`ProjectGuide::resume_add_folder`] or
/// [`ProjectGuide::resume_create_project`]
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAdopt {
    adoption: Box<Adoption>,
    /// the hashes of the texts the prompt shows, recorded with the answer
    hashes: JsObject<String>,
}

/// `adoptFolderHint` waiting for the trust answer:
/// [`ProjectGuide::resume_adopt_folder_hint`]
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAdoptFolderHint {
    doc_path: String,
    adopt: PendingAdopt,
}

/// `decideTrust` waiting for the trust answer:
/// [`ProjectGuide::resume_decide_trust`]
#[derive(Debug, Clone, PartialEq)]
pub struct PendingDecideTrust {
    id: ProjectId,
    hashes: JsObject<String>,
    authority: ContextAuthority,
}

/// `NewProjectRequest`: "New project" as main decides it (spec 5.1).
/// `parentDir` is a folder main itself chose (the default save folder, or the
/// person's pick in main's own folder picker); it never comes from a renderer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewProjectRequest {
    pub name: String,
    /// Absent: the default save folder
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_dir: Option<String>,
    /// After a `status: 'exists'` answer the person confirmed: use that folder
    /// as the project
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_existing: Option<bool>,
}

pub enum CreatePreparation {
    Done(CreateProjectResult),
    Adopt(AdoptStart),
    New {
        parent: String,
        name: String,
        target: String,
    },
}

pub enum CreateNew {
    Done(CreateProjectResult),
    Write(CreateWrite),
}

pub struct CreateWrite {
    id: ProjectId,
    root: String,
    parent: String,
    name: String,
    project_file: String,
    at: String,
    platform: String,
}

pub enum CreateExecution {
    Created {
        entry: ListEntry,
        retained_private_dir: Option<String>,
    },
    Exists {
        folder_path: String,
        detail: String,
    },
    Unwritable {
        detail: String,
    },
}

impl CreateWrite {
    pub fn execute_authorized(
        mut self,
        path: &str,
        mut authorize: impl FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
        commit: impl FnOnce(&str, &str) -> Result<(), NodeError>,
    ) -> Result<CreateExecution, GuideError> {
        let guard = ProjectWriteGuard::new(&self.platform);
        let published = match guard.create_project_authorized(
            &self.parent,
            &self.name,
            self.project_file.as_bytes(),
            &mut authorize,
        ) {
            Ok(published) => published,
            Err(WriteGuardError::Refused(refused))
                if refused.reason == ProjectWriteRefusal::Exists =>
            {
                return Ok(CreateExecution::Exists {
                    folder_path: self.root,
                    detail: refused.message(),
                });
            }
            Err(WriteGuardError::Fs(error)) if error.code.is_some() => {
                return Ok(CreateExecution::Unwritable {
                    detail: error.message,
                });
            }
            Err(error) => return Err(error.into()),
        };
        self.root = published.path.clone();
        let mut settings = read_projects_settings(path);
        let entry = self.apply(&mut settings).map_err(|error| {
            GuideError::plain(format!(
                "{}; project already published at {}; private staging retained: {:?}",
                error.message(),
                published.path,
                published.retained_private_dir,
            ))
        })?;
        crate::shell::app_settings::write_projects_settings_guarded(
            path,
            Some(&settings.to_json()),
            commit,
        )
        .map_err(|error| {
            GuideError::Node(NodeError {
                code: error.code,
                message: format!(
                    "{}; project already published at {}; private staging retained: {:?}",
                    error.message, published.path, published.retained_private_dir
                ),
            })
        })?;
        Ok(CreateExecution::Created {
            entry,
            retained_private_dir: published.retained_private_dir,
        })
    }

    fn apply(self, settings: &mut StoredProjectsSettings) -> Result<ListEntry, GuideError> {
        if settings
            .list
            .iter()
            .any(|entry| entry.id == self.id || same_path(&entry.path, &self.root, &self.platform))
            || settings.trust.get(&self.id).is_some()
        {
            return Err(GuideError::plain("project changed while awaiting creation"));
        }
        let entry = ListEntry {
            id: self.id.clone(),
            path: self.root,
            pinned: false,
            last_used_at: self.at,
        };
        settings.list.push(entry.clone());
        settings.trust.set(
            &self.id,
            StoredTrustRecord {
                decision: "use".into(),
                hashes: JsObject::new(),
            },
        );
        Ok(entry)
    }
}

/// `SelectionTarget`: whose AI panel asks for a selection
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SelectionTarget {
    /// A saved document (absolute path, canonicalized here)
    Document { path: String },
    /// A never-saved document; `projectId` is the project a create flow opened
    /// it in (spec 5.2, 6)
    #[serde(rename_all = "camelCase")]
    Untitled {
        #[serde(skip_serializing_if = "Option::is_none")]
        project_id: Option<ProjectId>,
    },
    /// The home ask panel
    Home,
}

/// `ProjectAiVisibility`: what the AI may see in one project, used by the
/// tools and the file list
#[derive(Debug)]
pub struct ProjectAiVisibility {
    pub root: String,
    pub matcher: IgnoreMatcher,
    /// Same for the same rule (`ignore` text and case folding); a file list
    /// cache keys on it
    pub rule_key: Option<String>,
}

impl ProjectAiVisibility {
    /// `isVisible(relPath, isDir)`: folder-relative path (either separator) is
    /// visible: inside, not hidden, not in the data folder, not ignored.
    /// `isDir` is `false` where the TypeScript leaves it out.
    pub fn is_visible(&self, rel_path: &str, is_dir: bool) -> Result<bool, RegexLimit> {
        is_ai_visible_path(&self.root, rel_path, &self.matcher, is_dir)
    }
}

/// `ContextFilesRequest`
#[derive(Debug)]
pub struct ContextFilesRequest {
    pub id: ProjectId,
    pub root: String,
    pub visibility: ProjectAiVisibility,
}

/// `ContextFiles`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextFiles {
    pub files: Vec<ProjectFileEntry>,
    pub omitted: u64,
}

/// `ProjectGuideDeps.env`: a read of `process.env`
pub type EnvLookup = Box<dyn Fn(&str) -> Option<String>>;

/// `ProjectGuideDeps.contextFiles`: the file names of a project for the
/// context (spec 5.4 item 4)
pub type ContextFilesProvider = Box<dyn FnMut(&ContextFilesRequest) -> ContextFiles>;

/// `console.warn(label, error)` of `ProjectGuideDeps.warn`
pub type WarnFn = Box<dyn FnMut(&str, &str)>;

/// `ProjectGuideDeps`
pub struct ProjectGuideDeps {
    /// `<userData>/app-settings.json`
    pub settings_path: String,
    pub home_dir: String,
    /// The app data folder (protected, never a project)
    pub user_data: String,
    /// The default save folder (`configuredDefaultSaveDir`), parent of a new
    /// project
    pub default_parent_dir: Box<dyn FnMut() -> String>,
    /// `NodeJS.Platform`; absent: the host's
    pub platform: Option<String>,
    /// For the sync-folder rule (`OneDrive*` variables); absent: the process
    /// environment
    pub env: Option<EnvLookup>,
    /// `now()`: the clock in milliseconds since the epoch, where the
    /// TypeScript returns a `Date`; absent: the host's clock
    pub now: Option<Box<dyn FnMut() -> f64>>,
    /// A new project id; must match `PROJECT_ID_PATTERN`; absent: 16 random
    /// bytes in base64url
    pub new_id: Option<Box<dyn FnMut() -> String>>,
    /// The screen language, for the header of a new `memory.md`; absent:
    /// `en`
    pub language: Option<Box<dyn FnMut() -> String>>,
    /// The file names of a project for the context (spec 5.4 item 4); absent
    /// sends none
    pub context_files: Option<ContextFilesProvider>,
    /// At most one write of a project's use time per this long; default
    /// `PROJECT_TOUCH_WRITE_MS`
    pub touch_write_ms: Option<f64>,
    /// The local calendar date and time of a time value, as `getFullYear()`
    /// and the other local getters of a `Date` answer them. The TypeScript
    /// reads the process's time zone; this is the host's.
    pub local_date: Box<dyn FnMut(f64) -> LocalDateParts>,
    /// `console.warn(label, error)`, `error` being its message; absent:
    /// nothing is printed
    pub warn: Option<WarnFn>,
}

/// `PROJECT_TOUCH_WRITE_MS`: a project's "last used" time is written to
/// `app-settings.json` at most once per this long (tunable); newer times are
/// kept in memory meanwhile and flushed at quit.
pub const PROJECT_TOUCH_WRITE_MS: f64 = 60_000.0;

/// `DataUpdateOutcome`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum DataUpdateOutcome {
    Ok { written: bool, text: Option<String> },
    Unwritable,
}

/// `MEMORY_BACKUPS_KEPT`: memory backups kept after a consolidation (spec 3)
pub const MEMORY_BACKUPS_KEPT: usize = 3;

/// `Partial<ProjectSettings>`: what `setSettings` takes; a field that is not
/// a boolean is left alone
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSettingsPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_agents_files: Option<bool>,
}
/// An owned settings mutation. Execute it only after the shared per-file
/// queue admits the worker; it reads and updates the latest disk state.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectSettingsWrite {
    Pin {
        project_id: String,
        pinned: bool,
    },
    Remove {
        project_id: String,
    },
    Switches {
        project_id: String,
        patch: ProjectSettingsPatch,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectSettingsWriteResult {
    Pinned,
    Removed,
    Switches(ProjectSettings),
}

impl ProjectSettingsWrite {
    pub fn project_id(&self) -> &str {
        match self {
            Self::Pin { project_id, .. }
            | Self::Switches { project_id, .. }
            | Self::Remove { project_id } => project_id,
        }
    }

    pub fn execute(&self, path: &str) -> Result<ProjectSettingsWriteResult, GuideError> {
        self.execute_guarded(path, unguarded_settings_commit)
    }

    pub fn execute_guarded(
        &self,
        path: &str,
        commit: impl FnOnce(&str, &str) -> Result<(), NodeError>,
    ) -> Result<ProjectSettingsWriteResult, GuideError> {
        let next = crate::shell::app_settings::update_projects_settings_guarded(
            path,
            |settings| self.apply(settings),
            commit,
        )?;
        Ok(match self {
            Self::Pin { .. } => ProjectSettingsWriteResult::Pinned,
            Self::Remove { .. } => ProjectSettingsWriteResult::Removed,
            Self::Switches { project_id, .. } => {
                ProjectSettingsWriteResult::Switches(settings_of(project_id, &next))
            }
        })
    }

    fn apply(&self, settings: &mut StoredProjectsSettings) {
        match self {
            Self::Pin { project_id, pinned } => {
                if let Some(entry) = settings
                    .list
                    .iter_mut()
                    .find(|entry| entry.id == *project_id)
                {
                    entry.pinned = *pinned;
                }
            }
            Self::Remove { project_id } => drop_from_list(settings, project_id),
            Self::Switches { project_id, patch } => {
                let mut current =
                    settings
                        .settings
                        .get(project_id)
                        .cloned()
                        .unwrap_or(ProjectSwitches {
                            memory_enabled: DEFAULT_SETTINGS.memory_enabled,
                            read_agents_files: DEFAULT_SETTINGS.read_agents_files,
                        });
                if let Some(value) = patch.memory_enabled {
                    current.memory_enabled = value;
                }
                if let Some(value) = patch.read_agents_files {
                    current.read_agents_files = value;
                }
                settings.settings.set(project_id, current);
            }
        }
    }
}

pub enum InstallIdPreparation {
    Ready(String),
    Write(InstallIdWrite),
}

/// Candidate generated on core; the worker keeps the first installed id.
pub struct InstallIdWrite {
    candidate: String,
}

impl InstallIdWrite {
    pub fn execute(self, path: &str) -> Result<String, GuideError> {
        self.execute_guarded(path, unguarded_settings_commit)
    }

    pub fn execute_guarded(
        self,
        path: &str,
        commit: impl FnOnce(&str, &str) -> Result<(), NodeError>,
    ) -> Result<String, GuideError> {
        let settings = crate::shell::app_settings::update_projects_settings_guarded(
            path,
            |settings| self.apply(settings),
            commit,
        )?;
        Ok(settings.install_id)
    }

    fn apply(self, settings: &mut StoredProjectsSettings) {
        if settings.install_id.is_empty() {
            settings.install_id = self.candidate;
        }
    }
}

/// Owned touch snapshot for a queued worker; newer touches remain on core.
#[derive(Clone, Debug)]
pub struct TouchFlush {
    pending: Vec<(String, String)>,
    at_ms: f64,
}

impl TouchFlush {
    pub fn execute(&self, path: &str) -> Result<(), GuideError> {
        update_projects_settings(path, |settings| self.apply(settings))?;
        Ok(())
    }

    fn apply(&self, settings: &mut StoredProjectsSettings) {
        for (id, at) in &self.pending {
            if let Some(entry) = settings.list.iter_mut().find(|entry| entry.id == *id)
                && js_greater(at, &entry.last_used_at)
            {
                entry.last_used_at = at.clone();
            }
        }
    }
}

/// The `by` of `editMemory`: `ai` is the `project_remember` tool, `person` a
/// "[remove]" on a memory line or the screen's per-entry delete
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryEditor {
    Ai,
    Person,
}

/// What a method of the guide throws: an `Error` with a message, a refusal of
/// the write guard, the error of a file system call, or the error of a regular
/// expression
#[derive(Debug, Clone, PartialEq)]
pub enum GuideError {
    Plain(String),
    Refused(ProjectWriteRefused),
    Node(NodeError),
    Syntax(RegexSyntaxError),
    Limit(RegexLimit),
    NeedInstallId,
    #[cfg(unix)]
    Cleanup(WriteGuardError),
}

impl GuideError {
    fn plain(message: impl Into<String>) -> Self {
        Self::Plain(message.into())
    }

    /// `error.message`
    pub fn message(&self) -> String {
        match self {
            Self::Plain(message) => message.clone(),
            Self::Refused(refused) => refused.message(),
            Self::Node(error) => error.message.clone(),
            Self::Syntax(error) => error.to_string(),
            Self::Limit(error) => error.to_string(),
            Self::NeedInstallId => "install identity is required".into(),
            #[cfg(unix)]
            Self::Cleanup(error) => error.message(),
        }
    }

    /// `error.code`; `None` unless a Node file error has one
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Node(error) => error.code,
            #[cfg(unix)]
            Self::Cleanup(error) => error.code(),
            _ => None,
        }
    }
}

/// What the chat store throws for an error of the guide: its message and,
/// for a Node file error, its `code`
fn chat_error(error: GuideError) -> ChatStoreError {
    if matches!(error, GuideError::NeedInstallId) {
        return ChatStoreError::need_install_id();
    }
    ChatStoreError {
        message: error.message(),
        code: error.code(),
        kind: ChatStoreErrorKind::Failure,
    }
}

/// What the guide reports for an error a chat store change threw: a Node
/// file error where it carries a `code`
fn guide_error(error: ChatStoreError) -> GuideError {
    if error.kind == ChatStoreErrorKind::NeedInstallId {
        return GuideError::NeedInstallId;
    }
    if error.code.is_some() {
        GuideError::Node(error.into())
    } else {
        GuideError::Plain(error.message)
    }
}

impl std::fmt::Display for GuideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GuideError {}

impl From<WriteGuardError> for GuideError {
    fn from(error: WriteGuardError) -> Self {
        match error {
            WriteGuardError::Refused(refused) => Self::Refused(refused),
            WriteGuardError::Fs(error) => Self::Node(error),
            #[cfg(unix)]
            error @ WriteGuardError::Cleanup { .. } => Self::Cleanup(error),
        }
    }
}

impl From<ProjectWriteRefused> for GuideError {
    fn from(error: ProjectWriteRefused) -> Self {
        Self::Refused(error)
    }
}

impl From<NodeError> for GuideError {
    fn from(error: NodeError) -> Self {
        Self::Node(error)
    }
}

impl From<RegexSyntaxError> for GuideError {
    fn from(error: RegexSyntaxError) -> Self {
        Self::Syntax(error)
    }
}

impl From<RegexLimit> for GuideError {
    fn from(error: RegexLimit) -> Self {
        Self::Limit(error)
    }
}

/// When `touch` armed its timer and how long it asked to wait: the caller
/// calls `flush_touches` at `armed_at + delay`
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchTimer {
    /// the guide's clock when the timer was set (ms)
    pub armed_at: f64,
    /// `setTimeout`'s delay (ms)
    pub delay: f64,
}

impl TouchTimer {
    /// the time on the guide's clock the timer fires
    pub fn due(&self) -> f64 {
        self.armed_at + self.delay
    }
}

const DEFAULT_SETTINGS: ProjectSettings = ProjectSettings {
    memory_enabled: true,
    read_agents_files: true,
};

/// The environment variables the sync-folder rule reads
const SYNC_ENV_KEYS: [&str; 5] = [
    "OneDrive",
    "OneDriveConsumer",
    "OneDriveCommercial",
    "USERPROFILE",
    "HOME",
];

/// `defaultNewId()`: `randomBytes(16).toString('base64url')`
fn default_new_id() -> String {
    let mut bytes = [0u8; 16];
    for byte in &mut bytes {
        *byte = (math_random() * 256.0) as u8;
    }
    base64(&bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// `new Date()` as milliseconds since the epoch
fn system_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// `isFolder(path)`: `stat` follows links
fn is_folder(path: &str) -> bool {
    std::fs::metadata(namespaced(path)).is_ok_and(|info| info.is_dir())
}

/// `exists(path)`: `lstat` answers
fn exists(path: &str) -> bool {
    std::fs::symlink_metadata(namespaced(path)).is_ok()
}

/// `cloneTrust(record)`
fn clone_trust(record: &StoredTrustRecord) -> StoredTrustRecord {
    StoredTrustRecord {
        decision: record.decision.clone(),
        hashes: record.hashes.clone(),
    }
}

/// `a > b` of two strings: by UTF-16 code units
fn js_greater(a: &str, b: &str) -> bool {
    a.encode_utf16().cmp(b.encode_utf16()) == std::cmp::Ordering::Greater
}

/// `names.sort()`: by UTF-16 code units
fn sort_utf16(names: &mut [String]) {
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
}

/// `a.localeCompare(b)`, in the root collation
fn locale_compare(a: &str, b: &str) -> f64 {
    static COLLATOR: LazyLock<CollatorBorrowed<'static>> = LazyLock::new(|| {
        Collator::try_new(CollatorPreferences::default(), CollatorOptions::default())
            .expect("the compiled root collation")
    });
    match COLLATOR.compare(a, b) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
        std::cmp::Ordering::Greater => 1.0,
    }
}

/// `new Error(`unknown project ${id}`)`
fn unknown_project(id: &str) -> GuideError {
    GuideError::plain(format!("unknown project {id}"))
}

/// the decision a stored trust record holds (`use` or `without`)
fn decision_of(text: &str) -> ProjectTrustDecision {
    if text == "use" {
        ProjectTrustDecision::Use
    } else {
        ProjectTrustDecision::Without
    }
}

/// `{ ...DEFAULT_SETTINGS, ...settings.settings[id] }`
fn settings_of(id: &str, settings: &StoredProjectsSettings) -> ProjectSettings {
    match settings.settings.get(id) {
        Some(switches) => ProjectSettings {
            memory_enabled: switches.memory_enabled,
            read_agents_files: switches.read_agents_files,
        },
        None => DEFAULT_SETTINGS,
    }
}

/// `settings.list.find((item) => item.id === id)`
fn entry_in(settings: &StoredProjectsSettings, id: &str) -> Option<ListEntry> {
    settings.list.iter().find(|item| item.id == id).cloned()
}
/// Authority under which project context was assembled. A queued context
/// result must still match this snapshot before its contents are exposed.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextAuthority {
    project_id: String,
    path: String,
    trust: Option<StoredTrustRecord>,
    switches: ProjectSettings,
}

impl ContextAuthority {
    pub fn capture(settings: &StoredProjectsSettings, project_id: &str) -> Option<Self> {
        let entry = entry_in(settings, project_id)?;
        Some(Self {
            project_id: project_id.to_owned(),
            path: entry.path,
            trust: settings.trust.get(project_id).cloned(),
            switches: settings_of(project_id, settings),
        })
    }

    pub fn is_current(&self, settings: &StoredProjectsSettings) -> bool {
        entry_in(settings, &self.project_id).is_some_and(|entry| entry.path == self.path)
            && settings.trust.get(&self.project_id) == self.trust.as_ref()
            && settings_of(&self.project_id, settings) == self.switches
    }

    fn apply_hash_updates(&mut self, updates: &[(ProjectHashedFile, Option<String>)]) {
        if let Some(record) = &mut self.trust {
            for (file, hash) in updates {
                match hash {
                    Some(hash) => record.hashes.set(file.as_str(), hash.clone()),
                    None => {
                        delete_key(&mut record.hashes, file.as_str());
                    }
                }
            }
        }
    }

    fn apply_if_current(
        &mut self,
        settings: &mut StoredProjectsSettings,
        updates: &[(ProjectHashedFile, Option<String>)],
    ) -> bool {
        if !self.is_current(settings) {
            return false;
        }
        self.apply_hash_updates(updates);
        if let Some(record) = &self.trust {
            settings.trust.set(&self.project_id, record.clone());
        }
        true
    }
}

/// Selection is validated on core, then applied to the latest settings on the
/// queue worker. Due touches travel with that same transaction.
pub struct SelectionWrite {
    target: ResolvedSelectionTarget,
    project_id: Option<String>,
    platform: String,
    refusal: RefusalContext,
    touches: Option<TouchFlush>,
}

pub struct SelectionAcknowledgement {
    selection: ProjectSelection,
    touches: Option<TouchFlush>,
}

impl SelectionWrite {
    pub fn execute(self, path: &str) -> Result<SelectionAcknowledgement, GuideError> {
        self.execute_guarded(path, unguarded_settings_commit)
    }

    pub fn execute_guarded(
        self,
        path: &str,
        commit: impl FnOnce(&str, &str) -> Result<(), NodeError>,
    ) -> Result<SelectionAcknowledgement, GuideError> {
        let mut settings = read_projects_settings(path);
        if let Some(id) = &self.project_id
            && entry_in(&settings, id).is_none()
        {
            return Err(unknown_project(id));
        }
        record_selection(
            &mut settings,
            &self.target,
            self.project_id.as_deref(),
            &self.platform,
        );
        if let Some(touches) = &self.touches {
            touches.apply(&mut settings);
        }
        crate::shell::app_settings::write_projects_settings_guarded(
            path,
            Some(&settings.to_json()),
            commit,
        )?;
        let settings = read_projects_settings(path);
        let mut selection = selection_for(&settings, &self.target, &self.platform);
        if let ResolvedSelectionTarget::Document { doc_key } = &self.target {
            selection.unregistered_project_hint =
                find_unregistered_project(doc_key, &settings, &self.refusal);
        }
        if selection.project_id != self.project_id {
            selection.project_id = self.project_id;
            selection.initial_chat_id = None;
        }
        Ok(SelectionAcknowledgement {
            selection,
            touches: self.touches,
        })
    }
}

/// Owned outside-document chat association for a settings queue worker.
pub struct OutsideChatWrite {
    doc_key: String,
    project_id: String,
    chat_id: String,
    platform: String,
}

impl OutsideChatWrite {
    pub fn execute(self, path: &str) -> Result<(), GuideError> {
        self.execute_guarded(path, unguarded_settings_commit)
    }

    pub fn execute_guarded(
        self,
        path: &str,
        commit: impl FnOnce(&str, &str) -> Result<(), NodeError>,
    ) -> Result<(), GuideError> {
        crate::shell::app_settings::update_projects_settings_guarded(
            path,
            |settings| {
                record_outside_chat(
                    settings,
                    &self.doc_key,
                    &self.project_id,
                    &self.chat_id,
                    &self.platform,
                );
            },
            commit,
        )?;
        Ok(())
    }
}

pub enum ContextPreparation {
    Done(ProjectContextResult),
    Write(ContextWrite),
}

/// Context assembled on core and an owned conditional settings transaction.
pub struct ContextWrite {
    authority: ContextAuthority,
    context: ProjectContextBundle,
    hash_updates: Vec<(ProjectHashedFile, Option<String>)>,
    sync_notice: Option<ProjectNotice>,
    touches: Option<TouchFlush>,
}

pub struct ContextAcknowledgement {
    authority: ContextAuthority,
    settings: StoredProjectsSettings,
    context: ProjectContextBundle,
    touches: Option<TouchFlush>,
}

impl ContextWrite {
    pub fn execute(self, path: &str) -> Result<ContextAcknowledgement, GuideError> {
        self.execute_guarded(path, unguarded_settings_commit)
    }

    pub fn execute_guarded(
        mut self,
        path: &str,
        commit: impl FnOnce(&str, &str) -> Result<(), NodeError>,
    ) -> Result<ContextAcknowledgement, GuideError> {
        let mut settings = read_projects_settings(path);
        if !self
            .authority
            .apply_if_current(&mut settings, &self.hash_updates)
        {
            return Err(GuideError::plain(
                "project authority changed while reading context",
            ));
        }
        let show_notice = self.sync_notice.is_some()
            && !settings
                .sync_notice_shown
                .iter()
                .any(|id| id == &self.authority.project_id);
        if show_notice {
            settings
                .sync_notice_shown
                .push(self.authority.project_id.clone());
        }
        if let Some(touches) = &self.touches {
            touches.apply(&mut settings);
        }
        if !self.hash_updates.is_empty() || show_notice || self.touches.is_some() {
            crate::shell::app_settings::write_projects_settings_guarded(
                path,
                Some(&settings.to_json()),
                commit,
            )?;
            settings = read_projects_settings(path);
        }
        if show_notice && let Some(notice) = self.sync_notice {
            self.context.notices.push(notice);
        }
        Ok(ContextAcknowledgement {
            authority: self.authority,
            settings,
            context: self.context,
            touches: self.touches,
        })
    }
}

pub enum TrustPreparation {
    Done(DecideTrustResult),
    Write(TrustWrite),
}

pub enum TrustUsePreparation {
    NeedInstallId,
    Done(DecideTrustResult),
    Ask {
        prompt: Box<ProjectTrustPrompt>,
        pending: PendingDecideTrust,
    },
    Write(TrustWrite),
}

/// A trust approval is conditional on the project and trust state seen before
/// the person was asked; neither a modal nor a network wait holds the queue.
pub struct TrustWrite {
    authority: ContextAuthority,
    change: TrustChange,
}

enum TrustChange {
    Approve(JsObject<String>),
    Without,
}

pub struct TrustAcknowledgement {
    authority: ContextAuthority,
    settings: StoredProjectsSettings,
    decision: ProjectTrustDecision,
}

impl TrustWrite {
    pub fn execute(self, path: &str) -> Result<TrustAcknowledgement, GuideError> {
        self.execute_guarded(path, unguarded_settings_commit)
    }

    pub fn execute_guarded(
        self,
        path: &str,
        commit: impl FnOnce(&str, &str) -> Result<(), NodeError>,
    ) -> Result<TrustAcknowledgement, GuideError> {
        let mut settings = read_projects_settings(path);
        let (authority, decision) = self.apply(&mut settings)?;
        crate::shell::app_settings::write_projects_settings_guarded(
            path,
            Some(&settings.to_json()),
            commit,
        )?;
        Ok(TrustAcknowledgement {
            authority,
            decision,
            settings: read_projects_settings(path),
        })
    }

    fn apply(
        mut self,
        settings: &mut StoredProjectsSettings,
    ) -> Result<(ContextAuthority, ProjectTrustDecision), GuideError> {
        let (record, decision) = match self.change {
            TrustChange::Approve(hashes) => {
                if !self.authority.is_current(settings) {
                    return Err(GuideError::plain(
                        "project authority changed while awaiting trust",
                    ));
                }
                (
                    StoredTrustRecord {
                        decision: "use".into(),
                        hashes,
                    },
                    ProjectTrustDecision::Use,
                )
            }
            TrustChange::Without => {
                if !entry_in(settings, &self.authority.project_id)
                    .is_some_and(|entry| entry.path == self.authority.path)
                {
                    return Err(GuideError::plain("project changed while awaiting trust"));
                }
                let hashes = settings
                    .trust
                    .get(&self.authority.project_id)
                    .map(|record| record.hashes.clone())
                    .unwrap_or_default();
                self.authority.switches = settings_of(&self.authority.project_id, settings);
                (
                    StoredTrustRecord {
                        decision: "without".into(),
                        hashes,
                    },
                    ProjectTrustDecision::Without,
                )
            }
        };
        settings
            .trust
            .set(&self.authority.project_id, record.clone());
        self.authority.trust = Some(record);
        Ok((self.authority, decision))
    }
}

/// `parseCreatedAt(text)`
fn parse_created_at(text: &str) -> Option<String> {
    match json_parse(text) {
        Ok(JsonValue::Object(raw)) => match raw.get("createdAt") {
            Some(JsonValue::String(created_at)) => Some(created_at.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// `assertFolderRelative(relPath)`
fn assert_folder_relative(rel_path: &str) -> Result<(), GuideError> {
    if trim(rel_path).is_empty()
        || js_regex!(r"^[\\/]").test(rel_path)
        || js_regex!(r"^[A-Za-z]:").test(rel_path)
        || js_regex!(r"[\\/]").split(rel_path).contains(&"..")
    {
        return Err(GuideError::plain(
            "expected a path relative to the project folder",
        ));
    }
    Ok(())
}

/// `project.json` belongs to the guide (identity, rename); other modules never write it
fn assert_not_project_file(rel_path: &str) -> Result<(), GuideError> {
    if rel_path.replace('\\', "/").to_lowercase() == project_folder_name("projectFile") {
        return Err(GuideError::plain(
            "project.json is written by the project guide only",
        ));
    }
    Ok(())
}

/// Writes into one project's data folder, each through the write guard
/// (`DataWriter`)
struct DataWriter<'a> {
    guide: &'a ProjectGuide,
    id: &'a str,
    root: String,
    /// The data folder name every write of this operation goes to (`dataDirName`, resolved once)
    data_dir: String,
    /// `project.json` text written by the first write of the operation
    fresh: String,
    identified: Cell<bool>,
}

impl DataWriter<'_> {
    /// `identify()`: the first file actually written also writes `project.json`
    /// when the folder has none (or names another id)
    fn identify(&self) -> Result<(), GuideError> {
        if self.identified.get() {
            return Ok(());
        }
        self.identified.set(true);
        self.guide.guard.update_data::<GuideError>(
            &self.root,
            project_folder_name("projectFile"),
            &mut |current: Option<&str>| -> Result<Option<String>, GuideError> {
                Ok(match current {
                    None => Some(self.fresh.clone()),
                    Some(text) if self.guide.id_needs_fix(self.id, &self.root, text) => {
                        Some(self.fresh.clone())
                    }
                    Some(_) => None,
                })
            },
            Some(&self.data_dir),
        )?;
        self.guide
            .pending_id_fix
            .borrow_mut()
            .retain(|pending| pending != self.id);
        Ok(())
    }

    /// `write(relPath, text)`
    fn write(&self, rel_path: &str, text: &str) -> Result<(), GuideError> {
        self.identify()?;
        self.guide
            .guard
            .write_data(&self.root, rel_path, text.as_bytes(), Some(&self.data_dir))?;
        Ok(())
    }

    /// `append(relPath, text)`
    fn append(&self, rel_path: &str, text: &str) -> Result<(), GuideError> {
        self.identify()?;
        self.guide
            .guard
            .append_data(&self.root, rel_path, text, Some(&self.data_dir))?;
        Ok(())
    }

    /// `update(relPath, change)`: `identify` runs when the change gives text
    fn update(
        &self,
        rel_path: &str,
        change: DataChange<'_, GuideError>,
    ) -> Result<DataUpdateResult, GuideError> {
        self.guide.guard.update_data::<GuideError>(
            &self.root,
            rel_path,
            &mut |current: Option<&str>| -> Result<Option<String>, GuideError> {
                let next = change(current)?;
                if next.is_some() {
                    self.identify()?;
                }
                Ok(next)
            },
            Some(&self.data_dir),
        )
    }

    /// `remove(relPath)`
    fn remove(&self, rel_path: &str) -> Result<(), GuideError> {
        self.guide
            .guard
            .remove_data(&self.root, rel_path, Some(&self.data_dir))?;
        Ok(())
    }
}

/// `ProjectGuide`
#[allow(clippy::type_complexity)]
pub struct ProjectGuide {
    settings_path: String,
    home_dir: String,
    user_data: String,
    platform: String,
    env: Option<EnvLookup>,
    default_parent_dir: RefCell<Box<dyn FnMut() -> String>>,
    now: RefCell<Box<dyn FnMut() -> f64>>,
    new_id: RefCell<Box<dyn FnMut() -> String>>,
    language: RefCell<Box<dyn FnMut() -> String>>,
    context_files: Option<RefCell<ContextFilesProvider>>,
    local_date: RefCell<Box<dyn FnMut(f64) -> LocalDateParts>>,
    warn: RefCell<Option<WarnFn>>,
    guard: ProjectWriteGuard,
    /// Projects whose `.seuoffice/` could not be written during this app run
    unwritable: RefCell<Vec<ProjectId>>,
    /// Copied folders listed under a new id whose `project.json` still names the original
    pending_id_fix: RefCell<Vec<ProjectId>>,
    cached_install_id: RefCell<Option<String>>,
    /// use times not written to the settings file yet (see `touch`)
    pending_touches: RefCell<JsMap<String>>,
    /// when each project's use time was last written (ms)
    touch_written_at: RefCell<JsMap<f64>>,
    touch_timer: Cell<Option<TouchTimer>>,
    touch_write_ms: f64,
}

impl ProjectGuide {
    /// `new ProjectGuide(deps)`
    pub fn new(deps: ProjectGuideDeps) -> Self {
        let platform = deps
            .platform
            .unwrap_or_else(|| process_platform().to_owned());
        let guard = ProjectWriteGuard::new(&platform);
        Self {
            settings_path: deps.settings_path,
            home_dir: deps.home_dir,
            user_data: deps.user_data,
            platform,
            env: deps.env,
            default_parent_dir: RefCell::new(deps.default_parent_dir),
            now: RefCell::new(deps.now.unwrap_or_else(|| Box::new(system_now))),
            new_id: RefCell::new(deps.new_id.unwrap_or_else(|| Box::new(default_new_id))),
            language: RefCell::new(
                deps.language
                    .unwrap_or_else(|| Box::new(|| "en".to_owned())),
            ),
            context_files: deps.context_files.map(RefCell::new),
            local_date: RefCell::new(deps.local_date),
            warn: RefCell::new(deps.warn),
            guard,
            unwritable: RefCell::new(Vec::new()),
            pending_id_fix: RefCell::new(Vec::new()),
            cached_install_id: RefCell::new(None),
            pending_touches: RefCell::new(JsMap::new()),
            touch_written_at: RefCell::new(JsMap::new()),
            touch_timer: Cell::new(None),
            touch_write_ms: deps.touch_write_ms.unwrap_or(PROJECT_TOUCH_WRITE_MS),
        }
    }

    // ────────────────────────── host ──────────────────────────

    /// `this.now().getTime()`: one read of the clock
    fn clock(&self) -> f64 {
        let mut now = self.now.borrow_mut();
        now()
    }

    /// `this.now().toISOString()`; an invalid time throws `RangeError`
    fn now_iso(&self) -> Result<String, GuideError> {
        to_iso_string(self.clock()).ok_or_else(|| GuideError::plain("Invalid time value"))
    }

    fn new_id(&self) -> String {
        let mut new_id = self.new_id.borrow_mut();
        new_id()
    }

    fn env_var(&self, key: &str) -> Option<String> {
        match &self.env {
            Some(lookup) => lookup(key),
            None => std::env::var(key).ok(),
        }
    }

    /// `process.env` as the sync-folder rule reads it
    fn env_object(&self) -> JsObject<String> {
        let mut env = JsObject::new();
        for key in SYNC_ENV_KEYS {
            if let Some(value) = self.env_var(key) {
                env.set(key, value);
            }
        }
        env
    }

    fn warn(&self, label: &str, detail: &str) {
        if let Some(warn) = self.warn.borrow_mut().as_mut() {
            warn(label, detail);
        }
    }

    // ────────────────────────── settings ──────────────────────────

    fn read(&self) -> StoredProjectsSettings {
        read_projects_settings(&self.settings_path)
    }

    fn update(
        &self,
        change: impl FnOnce(&mut StoredProjectsSettings),
    ) -> Result<StoredProjectsSettings, GuideError> {
        Ok(update_projects_settings(&self.settings_path, change)?)
    }

    fn refusal_context(&self) -> RefusalContext {
        RefusalContext {
            home_dir: self.home_dir.clone(),
            user_data: self.user_data.clone(),
            platform: self.platform.clone(),
        }
    }

    /// This install's random id, made and stored once
    pub fn install_id(&self) -> Result<String, GuideError> {
        match self.prepare_install_id() {
            InstallIdPreparation::Ready(id) => Ok(id),
            InstallIdPreparation::Write(write) => {
                let id = write.execute(&self.settings_path)?;
                Ok(self.finish_install_id(id))
            }
        }
    }

    pub fn prepare_install_id(&self) -> InstallIdPreparation {
        if let Some(id) = self.cached_install_id.borrow().as_ref()
            && !id.is_empty()
        {
            return InstallIdPreparation::Ready(id.clone());
        }
        let id = self.read().install_id;
        if !id.is_empty() {
            return InstallIdPreparation::Ready(self.finish_install_id(id));
        }
        InstallIdPreparation::Write(InstallIdWrite {
            candidate: self.new_id(),
        })
    }

    pub fn finish_install_id(&self, id: String) -> String {
        *self.cached_install_id.borrow_mut() = Some(id.clone());
        id
    }

    fn entry(&self, id: &str) -> Option<ListEntry> {
        entry_in(&self.read(), id)
    }

    /// The canonical folder of a listed project, or `None` (`null`)
    pub fn project_root(&self, id: &str) -> Option<String> {
        self.entry(id).map(|entry| entry.path)
    }

    pub fn trust_state(&self, id: &str) -> ProjectTrustState {
        match self.read().trust.get(id) {
            Some(record) => match decision_of(&record.decision) {
                ProjectTrustDecision::Use => ProjectTrustState::Use,
                ProjectTrustDecision::Without => ProjectTrustState::Without,
            },
            None => ProjectTrustState::Unconfirmed,
        }
    }

    pub fn is_unwritable(&self, id: &str) -> bool {
        self.unwritable.borrow().iter().any(|item| item == id)
    }

    /// Marks a project "unwritable" for this app run (a write into `.seuoffice/` failed)
    pub fn mark_unwritable(&self, id: &str) {
        let mut unwritable = self.unwritable.borrow_mut();
        if !unwritable.iter().any(|item| item == id) {
            unwritable.push(id.to_owned());
        }
    }

    fn is_sync_folder(&self, path: &str) -> bool {
        is_under_known_sync_folder(
            path,
            &self.env_object(),
            &SyncFolderOptions {
                platform: &self.platform,
                home_dir: Some(&self.home_dir),
                os_home_dir: &self.home_dir,
                extra_roots: &[],
            },
        )
    }

    fn sync_service(&self, path: &str) -> Option<SyncFolderService> {
        for key in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
            let env = self.env_var(key);
            if let Some(root) = truthy(env.as_deref())
                && is_inside_folder(path, root, &self.platform)
            {
                return Some(SyncFolderService::Onedrive);
            }
        }
        if js_regex!(r"onedrive", "i").test(path) {
            return Some(SyncFolderService::Onedrive);
        }
        if self.is_sync_folder(path) {
            Some(SyncFolderService::GoogleDrive)
        } else {
            None
        }
    }

    fn to_item(&self, entry: &ListEntry) -> ProjectListItem {
        let missing = !is_folder(&entry.path);
        let file = if missing {
            None
        } else {
            read_project_file(&entry.path, None)
        };
        ProjectListItem {
            id: entry.id.clone(),
            name: match file {
                Some(file) => file.name,
                None => basename(&entry.path, None),
            },
            path: entry.path.clone(),
            pinned: entry.pinned,
            last_used_at: self.last_used_of(entry),
            missing,
            unwritable: self.is_unwritable(&entry.id),
            sync_folder: self.is_sync_folder(&entry.path),
        }
    }

    /// Marks a project used now. Every send uses its project, so writing
    /// `app-settings.json` each time would rewrite the whole file per message: one
    /// project's time is written at most once per `touchWriteMs`; later times wait
    /// in memory (the list order already follows them) and are written when that
    /// interval is over or at `flushTouches` (app quit).
    fn touch(&self, id: &str) -> Result<(), GuideError> {
        if self.stage_touch(id)? {
            self.flush_touches()?;
        }
        Ok(())
    }

    /// Record the touch on core; `true` means the snapshot is due for a worker.
    fn stage_touch(&self, id: &str) -> Result<bool, GuideError> {
        let at = self.now_iso()?;
        let now_ms = self.clock();
        let last = self.touch_written_at.borrow().get(id).copied();
        let due = last.is_none_or(|last| now_ms - last >= self.touch_write_ms);
        self.pending_touches.borrow_mut().set(id, at);
        if due {
            return Ok(true);
        }
        if self.touch_timer.get().is_none() {
            self.touch_timer.set(Some(TouchTimer {
                armed_at: now_ms,
                delay: math_max(0.0, last.unwrap_or_default() + self.touch_write_ms - now_ms),
            }));
        }
        Ok(false)
    }

    /// The timer `touch` set, if one is armed: the caller calls
    /// `flush_touches` at its `due()` (the TypeScript's `setTimeout`)
    pub fn touch_timer(&self) -> Option<TouchTimer> {
        self.touch_timer.get()
    }

    /// Writes the use times still held in memory (call at app quit, and when
    /// the timer of `touch_timer` is due)
    pub fn flush_touches(&self) -> Result<(), GuideError> {
        let Some(flush) = self.prepare_touch_flush() else {
            return Ok(());
        };
        if let Err(error) = flush.execute(&self.settings_path) {
            self.restore_touch_timer();
            return Err(error);
        }
        self.finish_touch_flush(&flush);
        Ok(())
    }

    /// Captures work without removing touches that may change while it runs.
    pub fn prepare_touch_flush(&self) -> Option<TouchFlush> {
        self.touch_timer.set(None);
        let pending: Vec<_> = self
            .pending_touches
            .borrow()
            .iter()
            .map(|(id, at)| (id.to_owned(), at.clone()))
            .collect();
        if pending.is_empty() {
            return None;
        }
        Some(TouchFlush {
            pending,
            at_ms: self.clock(),
        })
    }

    /// Acknowledge only timestamps included in a successful worker write.
    pub fn finish_touch_flush(&self, flush: &TouchFlush) {
        {
            let mut pending = self.pending_touches.borrow_mut();
            let mut written = self.touch_written_at.borrow_mut();
            for (id, at) in &flush.pending {
                if pending.get(id).is_some_and(|current| current == at) {
                    pending.delete(id);
                }
                if written.get(id).is_none_or(|last| *last < flush.at_ms) {
                    written.set(id.as_str(), flush.at_ms);
                }
            }
        }
        if !self.pending_touches.borrow().is_empty() && self.touch_timer.get().is_none() {
            let now = self.clock();
            let written = self.touch_written_at.borrow();
            let delay =
                self.pending_touches
                    .borrow()
                    .iter()
                    .fold(self.touch_write_ms, |delay, (id, _)| {
                        let due = written
                            .get(id)
                            .map_or(0.0, |last| math_max(0.0, *last + self.touch_write_ms - now));
                        delay.min(due)
                    });
            self.touch_timer.set(Some(TouchTimer {
                armed_at: now,
                delay,
            }));
        }
    }

    /// Keep failed staged touches eligible for a later flush.
    pub fn restore_touch_timer(&self) {
        if !self.pending_touches.borrow().is_empty() && self.touch_timer.get().is_none() {
            self.touch_timer.set(Some(TouchTimer {
                armed_at: self.clock(),
                delay: math_max(0.0, self.touch_write_ms),
            }));
        }
    }

    /// `lastUsedAt` with a use time not written yet
    fn last_used_of(&self, entry: &ListEntry) -> String {
        match self.pending_touches.borrow().get(&entry.id) {
            Some(pending) if js_greater(pending, &entry.last_used_at) => pending.clone(),
            _ => entry.last_used_at.clone(),
        }
    }

    // ────────────────────────── list ──────────────────────────

    /// Pinned projects first, then the rest; each group newest `lastUsedAt` first
    pub fn list_projects(&self) -> Vec<ProjectListItem> {
        let mut entries: Vec<ListEntry> = self
            .read()
            .list
            .iter()
            .map(|entry| ListEntry {
                last_used_at: self.last_used_of(entry),
                ..entry.clone()
            })
            .collect();
        sort_by_number(&mut entries, |a, b| {
            if a.pinned != b.pinned {
                return if a.pinned { -1.0 } else { 1.0 };
            }
            locale_compare(&b.last_used_at, &a.last_used_at)
        });
        entries.iter().map(|entry| self.to_item(entry)).collect()
    }

    /// The list row of one project, or `None` (`null`) when it is not listed
    pub fn get_project(&self, id: &str) -> Option<ProjectListItem> {
        self.entry(id).map(|entry| self.to_item(&entry))
    }

    pub fn get_default_parent_folder(&self) -> String {
        let mut default_parent_dir = self.default_parent_dir.borrow_mut();
        default_parent_dir()
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<(), GuideError> {
        ProjectSettingsWrite::Pin {
            project_id: id.to_owned(),
            pinned,
        }
        .execute(&self.settings_path)?;
        Ok(())
    }

    /// "Remove from list": drops the list entry (with its pin) and the project's
    /// per-document records. The folder, the trust record and its hashes stay, so
    /// adding the folder again does not ask again.
    pub fn remove_from_list(&self, id: &str) -> Result<(), GuideError> {
        ProjectSettingsWrite::Remove {
            project_id: id.to_owned(),
        }
        .execute(&self.settings_path)?;
        self.finish_remove_from_list(id);
        Ok(())
    }

    /// Commit the core-owned portion after the settings transaction succeeds.
    pub fn finish_remove_from_list(&self, id: &str) {
        self.unwritable.borrow_mut().retain(|item| item != id);
        self.pending_id_fix.borrow_mut().retain(|item| item != id);
    }

    /// "Add folder" (spec 5.1): refusal rules, identity (5.11), trust (5.10).
    /// A folder with something to confirm asks first: resume with
    /// [`ProjectGuide::resume_add_folder`].
    pub fn add_folder(
        &self,
        folder_path: &str,
    ) -> Result<GuideStep<AddFolderResult, PendingAdopt>, GuideError> {
        self.adopt(folder_path, None)
    }

    /// Re-pick a "folder missing" project (spec 5.11); resumes like
    /// [`ProjectGuide::add_folder`]
    pub fn relocate_project(
        &self,
        id: &str,
        folder_path: &str,
    ) -> Result<GuideStep<AddFolderResult, PendingAdopt>, GuideError> {
        if self.entry(id).is_none() {
            return Err(unknown_project(id));
        }
        self.adopt(folder_path, Some(id))
    }

    /// The answer of the trust prompt `add_folder` or `relocate_project` asked
    pub fn resume_add_folder(
        &self,
        pending: PendingAdopt,
        answer: ProjectTrustAnswer,
    ) -> Result<AddFolderResult, GuideError> {
        match self.prepare_adopt_answer(pending, answer)? {
            AdoptPreparation::Done(result) => Ok(result),
            AdoptPreparation::Write(write) => {
                self.finish_adoption(write.execute(&self.settings_path)?)
            }
        }
    }

    fn adopt(
        &self,
        folder_path: &str,
        relocate_id: Option<&str>,
    ) -> Result<GuideStep<AddFolderResult, PendingAdopt>, GuideError> {
        let start = match self.prepare_adopt(folder_path, relocate_id)? {
            AdoptStart::NeedInstallId(scan) => self.scan_adoption(scan, &self.install_id()?)?,
            other => other,
        };
        match start {
            AdoptStart::Done(result) => Ok(GuideStep::Done(result)),
            AdoptStart::Ask { prompt, pending } => Ok(GuideStep::AskTrust { prompt, pending }),
            AdoptStart::Ready(plan) => {
                let write = self.prepare_adopt_write(plan)?;
                Ok(GuideStep::Done(
                    self.finish_adoption(write.execute(&self.settings_path)?)?,
                ))
            }
            AdoptStart::NeedInstallId(_) => {
                Err(GuideError::plain("install identity is unavailable"))
            }
        }
    }

    pub fn prepare_adopt(
        &self,
        folder_path: &str,
        relocate_id: Option<&str>,
    ) -> Result<AdoptStart, GuideError> {
        if let Some(id) = relocate_id
            && self.entry(id).is_none()
        {
            return Err(unknown_project(id));
        }
        let platform = self.platform.as_str();
        if !is_absolute(folder_path) {
            return Ok(AdoptStart::Done(AddFolderResult::Refused {
                reason: FolderRefusalReason::NotAFolder,
            }));
        }
        let root = canonical_path(folder_path);
        if let Some(reason) = folder_refusal(&root, &self.refusal_context()) {
            return Ok(AdoptStart::Done(AddFolderResult::Refused { reason }));
        }

        let file = read_project_file(&root, None);
        let settings = self.read();
        let verdict = classify_folder(&ClassifyInput {
            root: &root,
            file: file.as_ref(),
            settings: &settings,
            platform,
            relocate_id,
        });
        let FolderClassification::Ok {
            identity,
            id,
            previous,
            copied_from,
            stale_at_path,
        } = verdict
        else {
            return Ok(AdoptStart::Done(AddFolderResult::Refused {
                reason: FolderRefusalReason::DifferentProject,
            }));
        };
        let id = match id {
            Some(id) => id,
            None => self.unique_new_id(&settings)?,
        };

        let name = match &file {
            Some(file) => file.name.clone(),
            None => basename(&root, None),
        };
        let expected_trust = settings.trust.get(&id).cloned();
        let needs_scan = settings.trust.get(&id).is_none();
        let inherited = copied_from
            .as_deref()
            .and_then(|source| settings.trust.get(source))
            .is_some();
        let adoption = Adoption {
            root,
            id,
            name,
            identity,
            previous,
            copied_from,
            stale_at_path,
            expected_trust,
        };
        if needs_scan && !inherited {
            Ok(AdoptStart::NeedInstallId(AdoptScan { adoption }))
        } else {
            Ok(AdoptStart::Ready(AdoptPlan {
                adoption,
                trust: None,
            }))
        }
    }

    pub fn scan_adoption(
        &self,
        scan: AdoptScan,
        install_id: &str,
    ) -> Result<AdoptStart, GuideError> {
        let adoption = scan.adoption;
        let scanned = scan_for_trust(&TrustScanInput {
            root: adoption.root.clone(),
            project_id: adoption.id.clone(),
            name: adoption.name.clone(),
            install_id: install_id.to_owned(),
        });
        if scanned.foreign {
            Ok(AdoptStart::Ask {
                prompt: Box::new(scanned.prompt),
                pending: PendingAdopt {
                    adoption: Box::new(adoption),
                    hashes: scanned.hashes,
                },
            })
        } else {
            Ok(AdoptStart::Ready(AdoptPlan {
                adoption,
                trust: Some(StoredTrustRecord {
                    decision: ProjectTrustDecision::Use.as_str().to_owned(),
                    hashes: scanned.hashes,
                }),
            }))
        }
    }

    pub fn prepare_adopt_write(&self, plan: AdoptPlan) -> Result<AdoptWrite, GuideError> {
        self.prepare_finish_adopt(plan.adoption, plan.trust)
    }

    /// Prepare the core-owned file/state portion before admitting a settings write.
    fn prepare_finish_adopt(
        &self,
        adoption: Adoption,
        trust: Option<StoredTrustRecord>,
    ) -> Result<AdoptWrite, GuideError> {
        let settings = self.read();
        adoption.validate(&settings, &self.platform)?;
        let copy_file = if adoption.identity == FolderIdentity::Copied {
            Some(project_file_text(&ProjectFolderFile {
                version: PROJECT_FORMAT_VERSION,
                id: adoption.id.clone(),
                name: adoption.name.clone(),
                created_at: self.now_iso()?,
            }))
        } else {
            None
        };
        let notice = self
            .is_sync_folder(&adoption.root)
            .then(|| ProjectNotice::SyncFolder {
                service: self.sync_service(&adoption.root),
            });
        Ok(AdoptWrite {
            adoption,
            trust,
            at: self.now_iso()?,
            platform: self.platform.clone(),
            notice,
            copy_file,
        })
    }

    pub fn prepare_adopt_answer(
        &self,
        pending: PendingAdopt,
        answer: ProjectTrustAnswer,
    ) -> Result<AdoptPreparation, GuideError> {
        let PendingAdopt { adoption, hashes } = pending;
        let Some(decision) = answer else {
            return Ok(AdoptPreparation::Done(AddFolderResult::Cancelled {
                stage: AddFolderStage::Trust,
            }));
        };
        Ok(AdoptPreparation::Write(self.prepare_finish_adopt(
            *adoption,
            Some(StoredTrustRecord {
                decision: decision.as_str().to_owned(),
                hashes,
            }),
        )?))
    }

    pub fn finish_adoption(
        &self,
        ack: AdoptAcknowledgement,
    ) -> Result<AddFolderResult, GuideError> {
        let completion = self.prepare_adoption_ack(ack);
        let (result, warnings) = self.finish_adoption_ack(completion);
        self.report_write_warnings(&warnings);
        Ok(result)
    }

    pub fn prepare_adoption_ack(&self, ack: AdoptAcknowledgement) -> AdoptionCompletion {
        let mut warnings = Vec::new();
        if let Some(path) = ack.retained_private_dir {
            warnings.push((
                "[projects] private staging directory retained:".into(),
                path,
            ));
        }
        let copy_failed = ack.copy_failure.is_some();
        if let Some(error) = ack.copy_failure {
            warnings.push((
                format!("[projects] cannot write into project {}:", ack.original_id),
                error,
            ));
        }
        let mut project = self.to_item(&ack.entry);
        if copy_failed && ack.final_id == ack.original_id {
            project.unwritable = true;
        }
        AdoptionCompletion {
            original_id: ack.original_id,
            final_id: ack.final_id,
            copy_failed,
            warnings,
            result: AddFolderResult::Added {
                project,
                identity: ack.identity,
                notices: ack.notice.into_iter().collect(),
            },
        }
    }

    pub fn finish_adoption_ack(
        &self,
        completion: AdoptionCompletion,
    ) -> (AddFolderResult, Vec<(String, String)>) {
        if completion.copy_failed {
            self.mark_unwritable(&completion.original_id);
            let mut pending = self.pending_id_fix.borrow_mut();
            if !pending.contains(&completion.original_id) {
                pending.push(completion.original_id.clone());
            }
        }
        if completion.final_id != completion.original_id {
            self.pending_id_fix
                .borrow_mut()
                .retain(|item| *item != completion.original_id);
        }
        (completion.result, completion.warnings)
    }

    fn unique_new_id(&self, settings: &StoredProjectsSettings) -> Result<ProjectId, GuideError> {
        loop {
            let id = self.new_id();
            if !is_valid_project_id(&id) {
                return Err(GuideError::plain("newId produced an invalid project id"));
            }
            if settings.trust.get(&id).is_none() && !settings.list.iter().any(|item| item.id == id)
            {
                return Ok(id);
            }
        }
    }

    /// "New project" (spec 5.1): a new folder named `name` under the default save
    /// folder (or `parentDir`) with `.seuoffice/project.json`, trusted without asking.
    /// An existing folder of that name is never written to unless `useExisting` asks
    /// to add it as a project (identity and trust as for "add folder"; that path can
    /// ask, and resumes with [`ProjectGuide::resume_create_project`]).
    pub fn create_project(
        &self,
        args: &NewProjectRequest,
    ) -> Result<GuideStep<CreateProjectResult, PendingAdopt>, GuideError> {
        match self.prepare_create(args)? {
            CreatePreparation::Done(result) => Ok(GuideStep::Done(result)),
            CreatePreparation::Adopt(start) => {
                let start = match start {
                    AdoptStart::NeedInstallId(scan) => {
                        self.scan_adoption(scan, &self.install_id()?)?
                    }
                    other => other,
                };
                match start {
                    AdoptStart::Done(result) => Ok(GuideStep::Done(result.into())),
                    AdoptStart::Ask { prompt, pending } => {
                        Ok(GuideStep::AskTrust { prompt, pending })
                    }
                    AdoptStart::Ready(plan) => {
                        let write = self.prepare_adopt_write(plan)?;
                        Ok(GuideStep::Done(
                            self.finish_adoption(write.execute(&self.settings_path)?)?
                                .into(),
                        ))
                    }
                    AdoptStart::NeedInstallId(_) => {
                        Err(GuideError::plain("install identity is unavailable"))
                    }
                }
            }
            CreatePreparation::New {
                parent,
                name,
                target,
            } => match self.prepare_new_project(&parent, &name, &target)? {
                CreateNew::Done(result) => Ok(GuideStep::Done(result)),
                CreateNew::Write(write) => {
                    let result = write.execute_authorized(
                        &self.settings_path,
                        |effect| effect(),
                        unguarded_settings_commit,
                    )?;
                    Ok(GuideStep::Done(self.finish_create(result)))
                }
            },
        }
    }

    pub fn prepare_create(
        &self,
        args: &NewProjectRequest,
    ) -> Result<CreatePreparation, GuideError> {
        let name = trim(&args.name);
        if !is_valid_rename_name(name) || name.chars().count() > PROJECT_NAME_MAX_CHARS {
            return Ok(CreatePreparation::Done(CreateProjectResult::InvalidName));
        }
        let chosen = args
            .parent_dir
            .clone()
            .unwrap_or_else(|| self.get_default_parent_folder());
        if !is_absolute(&chosen) {
            return Ok(CreatePreparation::Done(CreateProjectResult::Unwritable));
        }
        let parent = canonical_path(&chosen);
        let target = join(&[&parent, name]);
        if let Some(reason) = location_refusal(&target, &self.refusal_context()) {
            return Ok(CreatePreparation::Done(CreateProjectResult::Refused {
                reason,
            }));
        }
        if exists(&target) {
            let listed = self
                .read()
                .list
                .iter()
                .any(|item| same_path(&item.path, &target, &self.platform));
            return if args.use_existing == Some(true) || listed {
                Ok(CreatePreparation::Adopt(self.prepare_adopt(&target, None)?))
            } else {
                Ok(CreatePreparation::Done(CreateProjectResult::Exists {
                    folder_path: target,
                }))
            };
        }
        Ok(CreatePreparation::New {
            parent,
            name: name.to_owned(),
            target,
        })
    }

    pub fn prepare_new_project(
        &self,
        parent: &str,
        name: &str,
        _target: &str,
    ) -> Result<CreateNew, GuideError> {
        let parent = match self.guard.resolve_create_parent(parent) {
            Ok(parent) => parent,
            Err(_) => return Ok(CreateNew::Done(CreateProjectResult::Unwritable)),
        };
        let root = join(&[&parent, name]);
        if let Some(reason) = location_refusal(&root, &self.refusal_context()) {
            return Ok(CreateNew::Done(CreateProjectResult::Refused { reason }));
        }
        let id = self.unique_new_id(&self.read())?;
        let file = ProjectFolderFile {
            version: PROJECT_FORMAT_VERSION,
            id: id.clone(),
            name: clean_project_name(name),
            created_at: self.now_iso()?,
        };
        Ok(CreateNew::Write(CreateWrite {
            id,
            root,
            parent,
            name: name.to_owned(),
            project_file: project_file_text(&file),
            at: self.now_iso()?,
            platform: self.platform.clone(),
        }))
    }

    pub fn finish_create(&self, execution: CreateExecution) -> CreateProjectResult {
        match execution {
            CreateExecution::Created {
                entry,
                retained_private_dir,
            } => {
                if let Some(retained_private_dir) = retained_private_dir {
                    self.warn(
                        "[projects] private staging directory retained:",
                        &retained_private_dir,
                    );
                }
                CreateProjectResult::Created {
                    project: self.to_item(&entry),
                }
            }
            CreateExecution::Exists {
                folder_path,
                detail,
            } => {
                self.warn("[projects] cannot publish project folder:", &detail);
                CreateProjectResult::Exists { folder_path }
            }
            CreateExecution::Unwritable { detail } => {
                self.warn("[projects] cannot stage project folder:", &detail);
                CreateProjectResult::Unwritable
            }
        }
    }

    /// The answer of the trust prompt `create_project` asked
    pub fn resume_create_project(
        &self,
        pending: PendingAdopt,
        answer: ProjectTrustAnswer,
    ) -> Result<CreateProjectResult, GuideError> {
        Ok(self.resume_add_folder(pending, answer)?.into())
    }

    /// Renames in `project.json` only; the folder keeps its name
    pub fn rename_project(&self, id: &str, name: &str) -> Result<ProjectWriteResult, GuideError> {
        let clean = clean_project_name(name);
        if clean.is_empty() {
            return Err(GuideError::plain("a project name cannot be empty"));
        }
        let Some(entry) = self.entry(id) else {
            return Err(unknown_project(id));
        };
        if self.is_unwritable(id) {
            return Ok(ProjectWriteResult::Unwritable);
        }
        Ok(self.try_write(id, || {
            self.guard.update_data::<GuideError>(
                &entry.path,
                project_folder_name("projectFile"),
                &mut |current: Option<&str>| -> Result<Option<String>, GuideError> {
                    let existing = current.and_then(parse_created_at);
                    let created_at = match existing {
                        Some(created_at) => created_at,
                        None => self.now_iso()?,
                    };
                    Ok(Some(project_file_text(&ProjectFolderFile {
                        version: PROJECT_FORMAT_VERSION,
                        id: id.to_owned(),
                        name: clean.clone(),
                        created_at,
                    })))
                },
                None,
            )?;
            Ok(())
        }))
    }

    /// Clears "unwritable" and writes `project.json` again (creating it when absent)
    pub fn retry_unwritable(&self, id: &str) -> Result<ProjectWriteResult, GuideError> {
        self.unwritable.borrow_mut().retain(|item| item != id);
        let Some(entry) = self.entry(id) else {
            return Err(unknown_project(id));
        };
        let data_dir = data_dir_name(&entry.path);
        let fresh = project_file_text(&self.project_file_for(id, &entry.path, &data_dir)?);
        let result = self.try_write(id, || {
            self.guard.update_data::<GuideError>(
                &entry.path,
                project_folder_name("projectFile"),
                &mut |current: Option<&str>| -> Result<Option<String>, GuideError> {
                    Ok(Some(match current {
                        Some(text) if !self.id_needs_fix(id, &entry.path, text) => text.to_owned(),
                        _ => fresh.clone(),
                    }))
                },
                Some(&data_dir),
            )?;
            Ok(())
        });
        if result == ProjectWriteResult::Ok {
            self.pending_id_fix.borrow_mut().retain(|item| item != id);
        }
        Ok(result)
    }

    /// `project.json` text at a listed project's folder names another id that must be
    /// replaced: a copy whose new id could not be written when it was added (this run),
    /// or, after a restart, a folder whose file still names a project listed at another
    /// folder (the original of the copy). Any other id is left alone.
    fn id_needs_fix(&self, id: &str, root: &str, current: &str) -> bool {
        let file_id = match json_parse(current) {
            Ok(JsonValue::Object(raw)) => match raw.get("id") {
                Some(JsonValue::String(file_id)) => Some(file_id.clone()),
                _ => None,
            },
            Ok(_) => None,
            Err(_) => return false,
        };
        let Some(file_id) = file_id else {
            return false;
        };
        if file_id == id || !is_valid_project_id(&file_id) {
            return false;
        }
        if self.pending_id_fix.borrow().iter().any(|item| item == id) {
            return true;
        }
        match self.entry(&file_id) {
            Some(original) => !same_path(&original.path, root, &self.platform),
            None => false,
        }
    }

    fn project_file_for(
        &self,
        id: &str,
        root: &str,
        data_dir: &str,
    ) -> Result<ProjectFolderFile, GuideError> {
        let file = read_project_file(root, Some(data_dir));
        let (name, created_at) = match file {
            Some(file) => (file.name, file.created_at),
            None => (basename(root, None), self.now_iso()?),
        };
        Ok(ProjectFolderFile {
            version: PROJECT_FORMAT_VERSION,
            id: id.to_owned(),
            name,
            created_at,
        })
    }

    // ────────────────────────── writes ──────────────────────────

    /// Runs one write; a failure marks the project unwritable for this run and is reported, not thrown
    fn try_write(
        &self,
        id: &str,
        write: impl FnOnce() -> Result<(), GuideError>,
    ) -> ProjectWriteResult {
        match write() {
            Ok(()) => ProjectWriteResult::Ok,
            Err(error) => {
                self.mark_unwritable(id);
                self.warn(
                    &format!("[projects] cannot write into project {id}:"),
                    &error.message(),
                );
                ProjectWriteResult::Unwritable
            }
        }
    }

    /// A write into a listed project's data folder. Refused without trying while the
    /// project is marked unwritable. The first file actually written also writes
    /// `project.json` when the folder has none, so the folder carries its identity; an
    /// operation that ends up writing nothing creates nothing. The data folder is
    /// resolved once (or given by a caller that already read from it), so an older
    /// version making the other folder meanwhile cannot split one operation across two.
    fn write_with_identity<F>(
        &self,
        id: &str,
        write: F,
        known_data_dir: Option<&str>,
    ) -> Result<ProjectWriteResult, GuideError>
    where
        F: FnOnce(&DataWriter<'_>) -> Result<(), GuideError>,
    {
        let Some(entry) = self.entry(id) else {
            return Err(unknown_project(id));
        };
        if self.is_unwritable(id) {
            return Ok(ProjectWriteResult::Unwritable);
        }
        let root = entry.path;
        let data_dir = match known_data_dir {
            Some(data_dir) => data_dir.to_owned(),
            None => data_dir_name(&root),
        };
        let fresh = project_file_text(&self.project_file_for(id, &root, &data_dir)?);
        let data = DataWriter {
            guide: self,
            id,
            root,
            data_dir,
            fresh,
            identified: Cell::new(false),
        };
        match write(&data) {
            Ok(()) => Ok(ProjectWriteResult::Ok),
            Err(GuideError::NeedInstallId) => Err(GuideError::NeedInstallId),
            Err(error) => {
                self.mark_unwritable(id);
                self.warn(
                    &format!("[projects] cannot write into project {id}:"),
                    &error.message(),
                );
                Ok(ProjectWriteResult::Unwritable)
            }
        }
    }

    /// Replace a file in the data folder (for the chat store). `dataDir` pins the
    /// folder a caller already read from; absent, it is resolved now.
    pub fn write_data(
        &self,
        id: &str,
        rel_path: &str,
        text: &str,
        data_dir: Option<&str>,
    ) -> Result<ProjectWriteResult, GuideError> {
        assert_not_project_file(rel_path)?;
        self.write_with_identity(id, |data| data.write(rel_path, text), data_dir)
    }

    /// Append to a file in the data folder (chat message lines); `dataDir` as for `writeData`
    pub fn append_data(
        &self,
        id: &str,
        rel_path: &str,
        text: &str,
        data_dir: Option<&str>,
    ) -> Result<ProjectWriteResult, GuideError> {
        assert_not_project_file(rel_path)?;
        self.write_with_identity(id, |data| data.append(rel_path, text), data_dir)
    }

    /// Read-change-write one file in the data folder, serialized with every other
    /// write to it. `change` gets the file's text (`None` when absent) and answers
    /// the text to write, or `None` to write nothing; what it fails with is
    /// reported as an unwritable project, as the TypeScript's `tryWrite` does.
    pub fn update_data(
        &self,
        id: &str,
        rel_path: &str,
        change: DataChange<'_, GuideError>,
        data_dir: Option<&str>,
    ) -> Result<DataUpdateOutcome, GuideError> {
        assert_not_project_file(rel_path)?;
        let mut outcome = DataUpdateResult {
            written: false,
            text: None,
        };
        let result = self.write_with_identity(
            id,
            |data| {
                outcome = data.update(rel_path, change)?;
                Ok(())
            },
            data_dir,
        )?;
        Ok(match result {
            ProjectWriteResult::Ok => DataUpdateOutcome::Ok {
                written: outcome.written,
                text: outcome.text,
            },
            ProjectWriteResult::Unwritable => DataUpdateOutcome::Unwritable,
        })
    }

    /// Delete one file in the data folder
    pub fn remove_data(
        &self,
        id: &str,
        rel_path: &str,
        data_dir: Option<&str>,
    ) -> Result<ProjectWriteResult, GuideError> {
        assert_not_project_file(rel_path)?;
        self.write_with_identity(id, |data| data.remove(rel_path), data_dir)
    }

    /// The checked absolute path of a file in a listed project's data folder
    /// (nothing is touched); `None` (`null`) when the project is not listed
    pub fn data_path(
        &self,
        id: &str,
        rel_path: &str,
        data_dir: Option<&str>,
    ) -> Result<Option<String>, ProjectWriteRefused> {
        match self.entry(id) {
            Some(entry) => self
                .guard
                .data_path(&entry.path, rel_path, data_dir)
                .map(Some),
            None => Ok(None),
        }
    }

    /// Write a new document the AI made (spec 6) at `target`: inside the project folder,
    /// not under `.seuoffice/`, in an existing folder, never over an existing entry.
    /// Throws `ProjectWriteRefused` (or the file system error); callers pick a free name.
    pub fn write_new_document(
        &self,
        id: &str,
        target: &str,
        data: &[u8],
    ) -> Result<ProjectWriteResult, WriteGuardError> {
        let Some(entry) = self.entry(id) else {
            return Err(WriteGuardError::Fs(NodeError {
                code: None,
                message: format!("unknown project {id}"),
            }));
        };
        self.guard.write_new_document(&entry.path, target, data)?;
        Ok(ProjectWriteResult::Ok)
    }

    /// A document's path relative to a project folder (`/`-separated), or `None` (`null`) when outside
    pub fn document_relative_path(&self, id: &str, doc_path: &str) -> Option<String> {
        let entry = self.entry(id)?;
        relative_inside(&entry.path, &canonical_path(doc_path), &self.platform)
    }

    fn record_hashes(
        &self,
        id: &str,
        hashes: &[(ProjectHashedFile, Option<String>)],
    ) -> Result<(), GuideError> {
        self.update(|settings| {
            let Some(record) = settings.trust.get_mut(id) else {
                return;
            };
            for (file, hash) in hashes {
                match hash {
                    None => delete_key(&mut record.hashes, file.as_str()),
                    Some(hash) => record.hashes.set(file.as_str(), hash.clone()),
                }
            }
        })?;
        Ok(())
    }

    // ────────────────────────── context ──────────────────────────

    /// The visibility rule of one project (reads its `ignore` now); `None` when not listed
    pub fn ai_visibility(&self, id: &str) -> Result<Option<ProjectAiVisibility>, GuideError> {
        let Some(entry) = self.entry(id) else {
            return Ok(None);
        };
        let root = entry.path;
        let text =
            read_data_text(&root, project_folder_name("ignoreFile"), None).unwrap_or_default();
        let folds = ignore_folds_case(&self.platform);
        let matcher = compile_ignore(
            &text,
            IgnoreOptions {
                case_insensitive: folds,
            },
        )?;
        Ok(Some(ProjectAiVisibility {
            root,
            matcher,
            rule_key: Some(hash_text(&format!(
                "{}:{text}",
                if folds { "i" } else { "s" }
            ))),
        }))
    }

    /// What the next request carries (spec 5.4): name, instructions (cut at the limit),
    /// memory (whole), file names, notices. Under `without` the instructions, memory and
    /// root agent files are left out. A file changed outside the app gives one
    /// "changed" notice, then its new hash is recorded.
    pub fn read_context(&self, id: &str) -> Result<ProjectContextResult, GuideError> {
        match self.prepare_context(id)? {
            ContextPreparation::Done(result) => Ok(result),
            ContextPreparation::Write(write) => match write.execute(&self.settings_path) {
                Ok(ack) => self.finish_context(ack),
                Err(error) => {
                    self.restore_touch_timer();
                    Err(error)
                }
            },
        }
    }

    /// Assemble the content and capture its authority before queuing any write.
    pub fn prepare_context(&self, id: &str) -> Result<ContextPreparation, GuideError> {
        let settings = self.read();
        let Some(entry) = entry_in(&settings, id) else {
            return Ok(ContextPreparation::Done(ProjectContextResult::Unknown));
        };
        let authority =
            ContextAuthority::capture(&settings, id).expect("the project entry was just found");
        let root = entry.path;
        if !is_folder(&root) {
            return Ok(ContextPreparation::Done(ProjectContextResult::Missing));
        }
        let record = settings.trust.get(id);
        // A listed project without a trust record was never confirmed: send nothing foreign.
        let trust = match record {
            Some(record) => decision_of(&record.decision),
            None => ProjectTrustDecision::Without,
        };
        let switches = settings_of(id, &settings);
        let mut notices: Vec<ProjectNotice> = Vec::new();
        let mut instructions: Option<ProjectContextInstructions> = None;
        let mut memory: Option<ProjectContextMemory> = None;
        let mut hash_updates = Vec::new();
        if trust == ProjectTrustDecision::Use
            && let Some(record) = record
        {
            let assembled = assemble_trusted_context(
                &read_hashed_texts(&root),
                &ProjectTrustRecord {
                    decision: trust,
                    hashes: record.hashes.clone(),
                },
                &switches,
                INSTRUCTIONS_LIMIT_CHARS,
                MEMORY_SOFT_LIMIT_CHARS,
            );
            instructions = assembled
                .instructions
                .map(|instructions| ProjectContextInstructions {
                    text: instructions.text,
                    sources: instructions.sources,
                    data_dir: Some(instructions.data_dir),
                });
            memory = assembled.memory.map(|memory| ProjectContextMemory {
                text: memory.text,
                chars: memory.chars,
            });
            notices.extend(assembled.notices);
            hash_updates = assembled.hash_updates;
        }

        let unwritable = self.is_unwritable(id);
        if unwritable {
            notices.push(ProjectNotice::Unwritable);
        }
        let sync_notice = self
            .is_sync_folder(&root)
            .then(|| ProjectNotice::SyncFolder {
                service: self.sync_service(&root),
            });

        let mut files = ContextFiles {
            files: Vec::new(),
            omitted: 0,
        };
        if let Some(provider) = &self.context_files
            && let Some(visibility) = self.ai_visibility(id)?
        {
            let mut provider = provider.borrow_mut();
            files = provider(&ContextFilesRequest {
                id: id.to_owned(),
                root: root.clone(),
                visibility,
            });
        }
        let touches = if self.stage_touch(id)? {
            self.prepare_touch_flush()
        } else {
            None
        };
        let file = read_project_file(&root, None);
        Ok(ContextPreparation::Write(ContextWrite {
            authority,
            hash_updates,
            sync_notice,
            touches,
            context: ProjectContextBundle {
                project_id: id.to_owned(),
                name: match file {
                    Some(file) => file.name,
                    None => basename(&root, None),
                },
                trust,
                instructions,
                memory,
                files: files.files,
                files_omitted: files.omitted,
                can_remember: switches.memory_enabled
                    && trust == ProjectTrustDecision::Use
                    && !unwritable,
                notices,
            },
        }))
    }

    /// Revalidate while the shared queue completion is still held on core.
    pub fn finish_context(
        &self,
        ack: ContextAcknowledgement,
    ) -> Result<ProjectContextResult, GuideError> {
        if !ack.authority.is_current(&ack.settings) {
            self.restore_touch_timer();
            return Err(GuideError::plain(
                "project authority changed while reading context",
            ));
        }
        if let Some(touches) = &ack.touches {
            self.finish_touch_flush(touches);
        }
        Ok(ProjectContextResult::Ok {
            context: Box::new(ack.context),
        })
    }

    // ────────────────────────── project screen ──────────────────────────

    /// Everything the project screen shows, read as it is (trust does not hide it)
    pub fn get_project_detail(&self, id: &str) -> Result<ProjectDetail, GuideError> {
        let Some(entry) = self.entry(id) else {
            return Err(unknown_project(id));
        };
        let root = entry.path.clone();
        let texts = read_hashed_texts(&root);
        let memory = texts.memory.clone().unwrap_or_default();
        let names = plain_folder_entries(&root, &[texts.data_dir.as_str()]);
        let mut conflicts: Vec<String> = names
            .into_iter()
            .filter(|name| {
                js_regex!(r"^memory.*\.md$", "i").test(name)
                    && name.to_lowercase() != project_folder_name("memoryFile")
                    && !memory_backup_pattern().test(name)
            })
            .collect();
        sort_utf16(&mut conflicts);
        Ok(ProjectDetail {
            project: self.to_item(&entry),
            trust: self.trust_state(id),
            settings: settings_of(id, &self.read()),
            instructions: texts.instructions.unwrap_or_default(),
            agents: texts.agents,
            claude: texts.claude,
            memory_entries: memory_entries(&memory),
            memory_chars: memory.encode_utf16().count() as u64,
            memory,
            memory_conflict_copies: conflicts,
        })
    }

    pub fn get_settings(&self, id: &str) -> ProjectSettings {
        settings_of(id, &self.read())
    }

    pub fn set_settings(
        &self,
        id: &str,
        patch: &ProjectSettingsPatch,
    ) -> Result<ProjectSettings, GuideError> {
        match (ProjectSettingsWrite::Switches {
            project_id: id.to_owned(),
            patch: patch.clone(),
        })
        .execute(&self.settings_path)?
        {
            ProjectSettingsWriteResult::Switches(settings) => Ok(settings),
            _ => unreachable!("switches write returned a different result"),
        }
    }

    /// Capture the project identity before queueing a trust revocation.
    pub fn prepare_trust_without(&self, id: &str) -> Result<TrustWrite, GuideError> {
        let settings = self.read();
        let authority =
            ContextAuthority::capture(&settings, id).ok_or_else(|| unknown_project(id))?;
        Ok(TrustWrite {
            authority,
            change: TrustChange::Without,
        })
    }

    /// Change trust from the project screen. `without` applies at once; `use` shows the
    /// trust prompt again when the folder holds anything to confirm (resume with
    /// [`ProjectGuide::resume_decide_trust`]), and a closed prompt leaves the project
    /// as it was.
    pub fn decide_trust(
        &self,
        id: &str,
        decision: ProjectTrustDecision,
    ) -> Result<GuideStep<DecideTrustResult, PendingDecideTrust>, GuideError> {
        if decision == ProjectTrustDecision::Without {
            let write = self.prepare_trust_without(id)?;
            return Ok(GuideStep::Done(
                self.finish_trust_answer(write.execute(&self.settings_path)?)?,
            ));
        }
        let mut preview = self.prepare_decide_trust_use(id, None)?;
        if matches!(preview, TrustUsePreparation::NeedInstallId) {
            let install_id = self.install_id()?;
            preview = self.prepare_decide_trust_use(id, Some(&install_id))?;
        }
        match preview {
            TrustUsePreparation::Done(result) => Ok(GuideStep::Done(result)),
            TrustUsePreparation::Write(write) => Ok(GuideStep::Done(
                self.finish_trust_answer(write.execute(&self.settings_path)?)?,
            )),
            TrustUsePreparation::Ask { prompt, pending } => {
                Ok(GuideStep::AskTrust { prompt, pending })
            }
            TrustUsePreparation::NeedInstallId => {
                Err(GuideError::plain("install identity is unavailable"))
            }
        }
    }

    /// Preview an approval without writing settings or opening a modal.
    pub fn prepare_decide_trust_use(
        &self,
        id: &str,
        install_id: Option<&str>,
    ) -> Result<TrustUsePreparation, GuideError> {
        let initial = self.read();
        let Some(entry) = entry_in(&initial, id) else {
            return Err(unknown_project(id));
        };
        if initial
            .trust
            .get(id)
            .is_some_and(|record| decision_of(&record.decision) == ProjectTrustDecision::Use)
        {
            return Ok(TrustUsePreparation::Done(DecideTrustResult::Ok {
                trust: ProjectTrustDecision::Use,
            }));
        }
        let Some(install_id) = install_id else {
            return Ok(TrustUsePreparation::NeedInstallId);
        };
        let file = read_project_file(&entry.path, None);
        let scan = scan_for_trust(&TrustScanInput {
            root: entry.path.clone(),
            project_id: id.to_owned(),
            name: match file {
                Some(file) => file.name,
                None => basename(&entry.path, None),
            },
            install_id: install_id.to_owned(),
        });
        if scan.foreign {
            // `await askTrust(scan.prompt)`: the rest runs at the resume
            return Ok(TrustUsePreparation::Ask {
                prompt: Box::new(scan.prompt),
                pending: PendingDecideTrust {
                    id: id.to_owned(),
                    hashes: scan.hashes,
                    authority: ContextAuthority::capture(&initial, id)
                        .expect("the project entry was just found"),
                },
            });
        }
        let authority =
            ContextAuthority::capture(&initial, id).expect("the project entry was just found");
        Ok(TrustUsePreparation::Write(TrustWrite {
            authority,
            change: TrustChange::Approve(scan.hashes),
        }))
    }

    /// The answer of the trust prompt `decide_trust` asked
    pub fn resume_decide_trust(
        &self,
        pending: PendingDecideTrust,
        answer: ProjectTrustAnswer,
    ) -> Result<DecideTrustResult, GuideError> {
        match self.prepare_trust_answer(pending, answer) {
            TrustPreparation::Done(result) => Ok(result),
            TrustPreparation::Write(write) => {
                self.finish_trust_answer(write.execute(&self.settings_path)?)
            }
        }
    }

    /// Prepare a trust answer without retaining the settings queue over a modal.
    pub fn prepare_trust_answer(
        &self,
        pending: PendingDecideTrust,
        answer: ProjectTrustAnswer,
    ) -> TrustPreparation {
        let PendingDecideTrust {
            id,
            hashes,
            authority,
        } = pending;
        debug_assert_eq!(id, authority.project_id);
        match answer {
            None => TrustPreparation::Done(DecideTrustResult::Cancelled),
            Some(ProjectTrustDecision::Without) => TrustPreparation::Done(DecideTrustResult::Ok {
                trust: ProjectTrustDecision::Without,
            }),
            Some(ProjectTrustDecision::Use) => TrustPreparation::Write(TrustWrite {
                authority,
                change: TrustChange::Approve(hashes),
            }),
        }
    }

    /// Complete a trust approval while its settings queue completion is held.
    pub fn finish_trust_answer(
        &self,
        ack: TrustAcknowledgement,
    ) -> Result<DecideTrustResult, GuideError> {
        if !ack.authority.is_current(&ack.settings) {
            return Err(GuideError::plain(
                "project authority changed while awaiting trust",
            ));
        }
        Ok(DecideTrustResult::Ok {
            trust: ack.decision,
        })
    }

    pub fn save_instructions(
        &self,
        id: &str,
        text: &str,
    ) -> Result<ProjectWriteResult, GuideError> {
        let result = self.write_with_identity(
            id,
            |data| data.write(project_folder_name("instructionsFile"), text),
            None,
        )?;
        if result == ProjectWriteResult::Ok {
            self.record_hashes(
                id,
                &[(ProjectHashedFile::Instructions, Some(hash_text(text)))],
            )?;
        }
        Ok(result)
    }

    pub fn add_ignore(&self, id: &str, rel_path: &str) -> Result<ProjectWriteResult, GuideError> {
        assert_folder_relative(rel_path)?;
        self.write_with_identity(
            id,
            |data| {
                data.update(project_folder_name("ignoreFile"), &mut |current: Option<
                    &str,
                >| {
                    let next = add_ignore_line(current.unwrap_or(""), rel_path);
                    Ok(if current == Some(next.as_str()) {
                        None
                    } else {
                        Some(next)
                    })
                })?;
                Ok(())
            },
            None,
        )
    }

    pub fn remove_ignore(
        &self,
        id: &str,
        rel_path: &str,
    ) -> Result<ProjectWriteResult, GuideError> {
        assert_folder_relative(rel_path)?;
        self.write_with_identity(
            id,
            |data| {
                data.update(project_folder_name("ignoreFile"), &mut |current: Option<
                    &str,
                >| {
                    let Some(current) = current else {
                        return Ok(None);
                    };
                    let next = remove_ignore_line(current, rel_path);
                    Ok(if next == current { None } else { Some(next) })
                })?;
                Ok(())
            },
            None,
        )
    }

    // ────────────────────────── memory ──────────────────────────

    fn memory_context(&self) -> MemoryOpContext {
        let parts = {
            let at = self.clock();
            let mut local = self.local_date.borrow_mut();
            local(at)
        };
        let language = {
            let mut language = self.language.borrow_mut();
            language()
        };
        MemoryOpContext {
            today: local_date(&parts),
            header: memory_header(&language),
        }
    }

    /// One memory change (spec 5.6), applied to the file as it is right now. `ai` is the
    /// `project_remember` tool: refused while memory is off or trust is `without`. `person`
    /// is a "[remove]" on a memory line or the screen's per-entry delete.
    pub fn edit_memory(
        &self,
        id: &str,
        change: &ProjectMemoryOp,
        by: MemoryEditor,
    ) -> Result<ProjectMemoryEditResult, GuideError> {
        if self.entry(id).is_none() {
            return Err(unknown_project(id));
        }
        let refused =
            |reason: ProjectMemoryRefusal| -> Result<ProjectMemoryEditResult, GuideError> {
                Ok(ProjectMemoryEditResult::Refused { reason })
            };
        if by == MemoryEditor::Ai {
            if !settings_of(id, &self.read()).memory_enabled {
                return refused(ProjectMemoryRefusal::Disabled);
            }
            if self.trust_state(id) != ProjectTrustState::Use {
                return refused(ProjectMemoryRefusal::Without);
            }
        }
        if self.is_unwritable(id) {
            return refused(ProjectMemoryRefusal::Unwritable);
        }
        let mut applied: Option<MemoryOpResult> = None;
        let mut written: Option<String> = None;
        let result = self.write_with_identity(
            id,
            |data| {
                let outcome = data.update(
                    project_folder_name("memoryFile"),
                    &mut |current: Option<&str>| {
                        let result = apply_memory_op(current, change, &self.memory_context());
                        let next = match &result {
                            MemoryOpResult::Ok { text, .. } if current != Some(text.as_str()) => {
                                Some(text.clone())
                            }
                            _ => None,
                        };
                        applied = Some(result);
                        Ok(next)
                    },
                )?;
                if outcome.written {
                    written = outcome.text;
                }
                Ok(())
            },
            None,
        )?;
        if result != ProjectWriteResult::Ok {
            return refused(ProjectMemoryRefusal::Unwritable);
        }
        let Some(done) = applied else {
            return refused(ProjectMemoryRefusal::Invalid);
        };
        let change = match done {
            MemoryOpResult::Refused { reason } => {
                return refused(match reason {
                    MemoryOpRefusal::NotFound => ProjectMemoryRefusal::NotFound,
                    MemoryOpRefusal::Invalid => ProjectMemoryRefusal::Invalid,
                });
            }
            MemoryOpResult::Ok { change, .. } => change,
        };
        if let Some(written) = written {
            self.record_hashes(
                id,
                &[(ProjectHashedFile::Memory, Some(hash_text(&written)))],
            )?;
        }
        Ok(ProjectMemoryEditResult::Ok { change })
    }

    /// The screen's "edit all": replaces `memory.md` with the person's text
    pub fn save_memory(&self, id: &str, text: &str) -> Result<ProjectWriteResult, GuideError> {
        let result = self.write_with_identity(
            id,
            |data| data.write(project_folder_name("memoryFile"), text),
            None,
        )?;
        if result == ProjectWriteResult::Ok {
            self.record_hashes(id, &[(ProjectHashedFile::Memory, Some(hash_text(text)))])?;
        }
        Ok(result)
    }

    /// Replace the memory with a consolidated list (person-triggered only). The current
    /// `memory.md` is first kept as `memory.<YYYYMMDD-HHmmss>.bak.md`; only the newest
    /// three backups stay.
    pub fn apply_memory_consolidation(
        &self,
        args: &ApplyMemoryConsolidationArgs,
    ) -> Result<ApplyMemoryConsolidationResult, GuideError> {
        let id = args.project_id.as_str();
        if self.entry(id).is_none() {
            return Err(unknown_project(id));
        }
        let MemoryOpContext { today, header } = self.memory_context();
        let mut before = 0u64;
        let mut after = 0u64;
        let mut backup_name = String::new();
        let mut written = String::new();
        let result = self.write_with_identity(
            id,
            |data| {
                data.update(project_folder_name("memoryFile"), &mut |current: Option<
                    &str,
                >| {
                    backup_name = self.free_backup_name(data);
                    data.write(&backup_name, current.unwrap_or(""))?;
                    written = build_memory_text(&args.entries, &header, &today);
                    before = memory_entries(current.unwrap_or("")).len() as u64;
                    after = memory_entries(&written).len() as u64;
                    Ok(Some(written.clone()))
                })?;
                self.prune_backups(data)
            },
            None,
        )?;
        if result != ProjectWriteResult::Ok {
            return Ok(ApplyMemoryConsolidationResult::Unwritable);
        }
        self.record_hashes(
            id,
            &[(ProjectHashedFile::Memory, Some(hash_text(&written)))],
        )?;
        Ok(ApplyMemoryConsolidationResult::Ok {
            before,
            after,
            backup_name,
        })
    }

    /// Restore the newest backup (the one the "[undo]" line names)
    pub fn undo_memory_consolidation(
        &self,
        args: &UndoMemoryConsolidationArgs,
    ) -> Result<UndoMemoryConsolidationResult, GuideError> {
        let id = args.project_id.as_str();
        let Some(entry) = self.entry(id) else {
            return Err(unknown_project(id));
        };
        if !memory_backup_pattern().test(&args.backup_name) {
            return Ok(UndoMemoryConsolidationResult::NoBackup);
        }
        let data_dir = data_dir_name(&entry.path);
        let names = self.backup_names(&entry.path, &data_dir);
        let Some(newest) = names.first() else {
            return Ok(UndoMemoryConsolidationResult::NoBackup);
        };
        if *newest != args.backup_name {
            return Ok(UndoMemoryConsolidationResult::NoBackup);
        }
        let Some(text) = read_data_text(&entry.path, newest, Some(&data_dir)) else {
            return Ok(UndoMemoryConsolidationResult::NoBackup);
        };
        let result = self.write_with_identity(
            id,
            |data| data.write(project_folder_name("memoryFile"), &text),
            Some(&data_dir),
        )?;
        if result != ProjectWriteResult::Ok {
            return Ok(UndoMemoryConsolidationResult::Unwritable);
        }
        self.record_hashes(id, &[(ProjectHashedFile::Memory, Some(hash_text(&text)))])?;
        Ok(UndoMemoryConsolidationResult::Ok {
            entries: memory_entries(&text).len() as u64,
        })
    }

    /// Backup names in `dataDir`, newest first
    fn backup_names(&self, root: &str, data_dir: &str) -> Vec<String> {
        let mut names: Vec<String> = plain_folder_entries(root, &[data_dir])
            .into_iter()
            .filter(|name| memory_backup_pattern().test(name))
            .collect();
        sort_utf16(&mut names);
        names.reverse();
        names
    }

    fn free_backup_name(&self, data: &DataWriter<'_>) -> String {
        let taken = self.backup_names(&data.root, &data.data_dir);
        let mut at = self.clock();
        loop {
            let name = {
                let mut local = self.local_date.borrow_mut();
                memory_backup_name(&local(at))
            };
            if !taken.contains(&name) {
                return name;
            }
            at += 1000.0;
        }
    }

    fn prune_backups(&self, data: &DataWriter<'_>) -> Result<(), GuideError> {
        let names = self.backup_names(&data.root, &data.data_dir);
        for name in names.iter().skip(MEMORY_BACKUPS_KEPT) {
            data.remove(name)?;
        }
        Ok(())
    }

    // ────────────────────────── selection ──────────────────────────

    fn resolve_target(
        &self,
        target: &SelectionTarget,
    ) -> Result<ResolvedSelectionTarget, GuideError> {
        Ok(match target {
            SelectionTarget::Document { path } => {
                if !is_absolute(path) {
                    return Err(GuideError::plain("expected an absolute document path"));
                }
                ResolvedSelectionTarget::Document {
                    doc_key: canonical_path(path),
                }
            }
            SelectionTarget::Untitled { project_id } => ResolvedSelectionTarget::Untitled {
                project_id: project_id.clone(),
            },
            SelectionTarget::Home => ResolvedSelectionTarget::Home,
        })
    }

    /// The project a window's AI panel starts with (spec 5.2). `initialChatId` is set
    /// only for a document outside every project (its remembered last chat); the chat
    /// store picks the chat of a document inside a project (spec 5.3.1).
    pub fn get_selection(&self, target: &SelectionTarget) -> Result<ProjectSelection, GuideError> {
        let settings = self.read();
        let resolved = self.resolve_target(target)?;
        let selection = selection_for(&settings, &resolved, &self.platform);
        let ResolvedSelectionTarget::Document { doc_key } = &resolved else {
            return Ok(selection);
        };
        let hint = find_unregistered_project(doc_key, &settings, &self.refusal_context());
        Ok(match hint {
            Some(hint) => ProjectSelection {
                unregistered_project_hint: Some(hint),
                ..selection
            },
            None => selection,
        })
    }

    /// Record a choice made in a window's AI panel (see `recordSelection`); returns the selection now
    pub fn set_selection(
        &self,
        target: &SelectionTarget,
        project_id: Option<&str>,
    ) -> Result<ProjectSelection, GuideError> {
        let write = self.prepare_selection(target, project_id)?;
        match write.execute(&self.settings_path) {
            Ok(ack) => Ok(self.finish_selection(ack)),
            Err(error) => {
                self.restore_touch_timer();
                Err(error)
            }
        }
    }

    pub fn prepare_selection(
        &self,
        target: &SelectionTarget,
        project_id: Option<&str>,
    ) -> Result<SelectionWrite, GuideError> {
        if let Some(id) = project_id
            && self.entry(id).is_none()
        {
            return Err(unknown_project(id));
        }
        let target = self.resolve_target(target)?;
        let touches = match project_id {
            Some(id) if self.stage_touch(id)? => self.prepare_touch_flush(),
            _ => None,
        };
        Ok(SelectionWrite {
            target,
            project_id: project_id.map(str::to_owned),
            platform: self.platform.clone(),
            refusal: self.refusal_context(),
            touches,
        })
    }

    pub fn finish_selection(&self, ack: SelectionAcknowledgement) -> ProjectSelection {
        if let Some(touches) = &ack.touches {
            self.finish_touch_flush(touches);
        }
        ack.selection
    }

    /// A document outside every project used `chatId` of `projectId` last (spec 3, 5.3.1)
    pub fn record_outside_chat(
        &self,
        doc_path: &str,
        project_id: &str,
        chat_id: &str,
    ) -> Result<(), GuideError> {
        self.prepare_outside_chat(doc_path, project_id, chat_id)?
            .execute(&self.settings_path)
    }

    pub fn prepare_outside_chat(
        &self,
        doc_path: &str,
        project_id: &str,
        chat_id: &str,
    ) -> Result<OutsideChatWrite, GuideError> {
        if !is_valid_project_id(chat_id) {
            return Err(GuideError::plain("invalid chat id"));
        }
        if !is_absolute(doc_path) {
            return Err(GuideError::plain("expected an absolute document path"));
        }
        Ok(OutsideChatWrite {
            doc_key: canonical_path(doc_path),
            project_id: project_id.to_owned(),
            chat_id: chat_id.to_owned(),
            platform: self.platform.clone(),
        })
    }

    pub fn prepare_adopt_hint(&self, doc_path: &str) -> Result<AdoptStart, GuideError> {
        let target = SelectionTarget::Document {
            path: doc_path.to_owned(),
        };
        let selection = self.get_selection(&target)?;
        match selection.unregistered_project_hint {
            Some(hint) => self.prepare_adopt(&hint.folder_path, None),
            None => Ok(AdoptStart::Done(AddFolderResult::Refused {
                reason: FolderRefusalReason::NotAFolder,
            })),
        }
    }

    /// "[use]" on the hint: the add-folder path for the hinted folder, then the document
    /// uses it. The add-folder path can ask: resume with
    /// [`ProjectGuide::resume_adopt_folder_hint`].
    pub fn adopt_folder_hint(
        &self,
        doc_path: &str,
    ) -> Result<GuideStep<AddFolderResult, PendingAdoptFolderHint>, GuideError> {
        let target = SelectionTarget::Document {
            path: doc_path.to_owned(),
        };
        let selection = self.get_selection(&target)?;
        let Some(hint) = selection.unregistered_project_hint else {
            return Ok(GuideStep::Done(AddFolderResult::Refused {
                reason: FolderRefusalReason::NotAFolder,
            }));
        };
        Ok(match self.adopt(&hint.folder_path, None)? {
            GuideStep::Done(result) => GuideStep::Done(self.use_hinted_folder(doc_path, result)?),
            GuideStep::AskTrust { prompt, pending } => GuideStep::AskTrust {
                prompt,
                pending: PendingAdoptFolderHint {
                    doc_path: doc_path.to_owned(),
                    adopt: pending,
                },
            },
        })
    }

    /// The answer of the trust prompt `adopt_folder_hint` asked
    pub fn resume_adopt_folder_hint(
        &self,
        pending: PendingAdoptFolderHint,
        answer: ProjectTrustAnswer,
    ) -> Result<AddFolderResult, GuideError> {
        let PendingAdoptFolderHint { doc_path, adopt } = pending;
        let result = self.resume_add_folder(adopt, answer)?;
        self.use_hinted_folder(&doc_path, result)
    }

    /// What `adoptFolderHint` does with the result of the add-folder path: the
    /// document uses the folder that was added
    fn use_hinted_folder(
        &self,
        doc_path: &str,
        result: AddFolderResult,
    ) -> Result<AddFolderResult, GuideError> {
        let target = SelectionTarget::Document {
            path: doc_path.to_owned(),
        };
        if let AddFolderResult::Added { project, .. } = &result {
            self.set_selection(&target, Some(&project.id))?;
        }
        Ok(result)
    }
}

// ────────────────────────── the traits other modules take ──────────────────────────

/// The guide an owner of it hands out: the guide itself, or a shared one
trait AsGuide {
    fn guide(&self) -> &ProjectGuide;
}

impl AsGuide for ProjectGuide {
    fn guide(&self) -> &ProjectGuide {
        self
    }
}

impl AsGuide for Rc<ProjectGuide> {
    fn guide(&self) -> &ProjectGuide {
        self
    }
}

/// The calls of `ProjectToolsGuide`, `ProjectIndexGuide`, `NewDocumentsGuide`
/// and `ChatStoreGuide` over a guide
macro_rules! guide_traits {
    ($owner:ty) => {
        impl ProjectToolsGuide for $owner {
            fn ai_visibility(&mut self, id: &str) -> Result<Option<ProjectAiVisibility>, Thrown> {
                ProjectGuide::ai_visibility(self.guide(), id)
                    .map_err(|error| Thrown(error.message()))
            }
        }

        impl ProjectIndexGuide for $owner {
            fn list_projects(&mut self) -> Result<Vec<ProjectListItem>, Thrown> {
                Ok(ProjectGuide::list_projects(self.guide()))
            }

            fn ai_visibility(&mut self, id: &str) -> Result<Option<ProjectAiVisibility>, Thrown> {
                ProjectGuide::ai_visibility(self.guide(), id)
                    .map_err(|error| Thrown(error.message()))
            }
        }

        impl NewDocumentsGuide for $owner {
            fn project_root(&mut self, id: &str) -> Option<String> {
                ProjectGuide::project_root(self.guide(), id)
            }

            fn write_new_document(
                &mut self,
                id: &str,
                target: &str,
                data: &[u8],
            ) -> Result<ProjectWriteResult, WriteGuardError> {
                ProjectGuide::write_new_document(self.guide(), id, target, data)
            }
        }

        /// A call the TypeScript's guide would reject (an unlisted project,
        /// `project.json`) is `unwritable` here: the trait has no error to give.
        impl ChatStoreGuide for $owner {
            fn append_data(
                &mut self,
                id: &str,
                rel_path: &str,
                text: &str,
                data_dir: Option<&str>,
            ) -> ProjectWriteResult {
                ProjectGuide::append_data(self.guide(), id, rel_path, text, data_dir)
                    .unwrap_or(ProjectWriteResult::Unwritable)
            }

            fn update_data(
                &mut self,
                id: &str,
                rel_path: &str,
                change: ChatDataChange<'_>,
                data_dir: Option<&str>,
            ) -> Result<DataUpdateOutcome, ChatStoreError> {
                let guide = self.guide();
                ProjectGuide::update_data(
                    guide,
                    id,
                    rel_path,
                    &mut |current: Option<&str>| -> Result<Option<String>, GuideError> {
                        change(current, &mut || match guide.prepare_install_id() {
                            InstallIdPreparation::Ready(id) => Ok(id),
                            InstallIdPreparation::Write(_) => {
                                Err(ChatStoreError::need_install_id())
                            }
                        })
                        .map_err(guide_error)
                    },
                    data_dir,
                )
                .map_err(chat_error)
            }

            fn data_path(
                &mut self,
                id: &str,
                rel_path: &str,
                data_dir: Option<&str>,
            ) -> Option<String> {
                ProjectGuide::data_path(self.guide(), id, rel_path, data_dir)
                    .ok()
                    .flatten()
            }

            fn install_id(&mut self) -> Result<String, ChatStoreError> {
                match self.guide().prepare_install_id() {
                    InstallIdPreparation::Ready(id) => Ok(id),
                    InstallIdPreparation::Write(_) => Err(ChatStoreError::need_install_id()),
                }
            }

            fn trust_state(&mut self, id: &str) -> ProjectTrustState {
                ProjectGuide::trust_state(self.guide(), id)
            }

            fn is_unwritable(&mut self, id: &str) -> bool {
                ProjectGuide::is_unwritable(self.guide(), id)
            }

            fn mark_unwritable(&mut self, id: &str) {
                ProjectGuide::mark_unwritable(self.guide(), id);
            }

            fn project_root(&mut self, id: &str) -> Option<String> {
                ProjectGuide::project_root(self.guide(), id)
            }

            fn record_outside_chat(
                &mut self,
                doc_path: &str,
                project_id: &str,
                chat_id: &str,
            ) -> Result<(), String> {
                ProjectGuide::record_outside_chat(self.guide(), doc_path, project_id, chat_id)
                    .map_err(|error| error.message())
            }

            fn document_relative_path(&mut self, id: &str, doc_path: &str) -> Option<String> {
                ProjectGuide::document_relative_path(self.guide(), id, doc_path)
            }

            fn get_selection(&mut self, target: &SelectionTarget) -> ProjectSelection {
                ProjectGuide::get_selection(self.guide(), target).unwrap_or(ProjectSelection {
                    project_id: None,
                    initial_chat_id: None,
                    unregistered_project_hint: None,
                })
            }
        }
    };
}

guide_traits!(ProjectGuide);
guide_traits!(Rc<ProjectGuide>);

#[cfg(test)]
mod context_authority_tests {
    use super::*;
    use crate::shell::app_settings::normalize_projects_settings;

    fn trusted() -> StoredProjectsSettings {
        let mut settings = normalize_projects_settings(None);
        settings.list.push(ListEntry {
            id: "project".into(),
            path: "/project".into(),
            pinned: false,
            last_used_at: String::new(),
        });
        settings.trust.set(
            "project",
            StoredTrustRecord {
                decision: "use".into(),
                hashes: JsObject::new(),
            },
        );
        settings
    }

    #[test]
    fn adoption_rejects_changed_trust_and_relocation() {
        let settings = trusted();
        let entry = &settings.list[0];
        let adoption = Adoption {
            root: "/new-root".into(),
            id: "project".into(),
            name: "project".into(),
            identity: FolderIdentity::Moved,
            previous: Some(crate::packages::project_store::ProjectListEntry {
                id: entry.id.clone(),
                path: entry.path.clone(),
                pinned: entry.pinned,
                last_used_at: entry.last_used_at.clone(),
            }),
            copied_from: None,
            stale_at_path: None,
            expected_trust: settings.trust.get("project").cloned(),
        };
        assert!(adoption.validate(&settings, "linux").is_ok());
        let mut revoked = settings.clone();
        revoked.trust.get_mut("project").unwrap().decision = "without".into();
        assert!(adoption.validate(&revoked, "linux").is_err());
        let mut moved = settings.clone();
        moved.list[0].path = "/elsewhere".into();
        assert!(adoption.validate(&moved, "linux").is_err());
        let mut removed = settings.clone();
        removed.list.clear();
        assert!(adoption.validate(&removed, "linux").is_err());
    }

    #[test]
    fn adoption_reports_settings_write_failure() {
        let write = AdoptWrite {
            adoption: Adoption {
                root: "/new-root".into(),
                id: "new".into(),
                name: "new".into(),
                identity: FolderIdentity::New,
                previous: None,
                copied_from: None,
                stale_at_path: None,
                expected_trust: None,
            },
            trust: None,
            at: String::new(),
            platform: "linux".into(),
            notice: None,
            copy_file: None,
        };
        assert!(write.execute("\0").is_err());
    }

    #[test]
    fn creation_does_not_overwrite_a_competing_project_or_trust() {
        let mut settings = trusted();
        let before = settings.clone();
        let write = CreateWrite {
            id: "project".into(),
            root: "/new-root".into(),
            parent: "/".into(),
            name: "new-root".into(),
            project_file: String::new(),
            at: String::new(),
            platform: "linux".into(),
        };
        assert!(write.apply(&mut settings).is_err());
        assert_eq!(settings, before);
        let write = CreateWrite {
            id: "other".into(),
            root: "/project".into(),
            parent: "/".into(),
            name: "project".into(),
            project_file: String::new(),
            at: String::new(),
            platform: "linux".into(),
        };
        assert!(write.apply(&mut settings).is_err());
        assert_eq!(settings, before);
    }

    #[cfg(unix)]
    #[test]
    fn settings_failure_keeps_the_published_project() {
        let parent =
            std::env::temp_dir().join(format!("project-create-settings-{}", std::process::id()));
        std::fs::create_dir_all(&parent).unwrap();
        let parent = parent.canonicalize().unwrap();
        let root = parent.join("created");
        let path = parent.join("settings.json");
        std::fs::write(&path, "{}").unwrap();
        let write = CreateWrite {
            id: "project".into(),
            root: root.to_string_lossy().into_owned(),
            parent: parent.to_string_lossy().into_owned(),
            name: "created".into(),
            project_file: "project metadata".into(),
            at: String::new(),
            platform: "linux".into(),
        };
        let result = write.execute_authorized(
            path.to_str().unwrap(),
            |effect| effect(),
            |_, _| {
                Err(NodeError {
                    code: None,
                    message: "settings denied".into(),
                })
            },
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(root.join(".seuoffice/project.json")).unwrap(),
            b"project metadata"
        );
        std::fs::remove_file(root.join(".seuoffice/project.json")).unwrap();
        std::fs::remove_dir(root.join(".seuoffice")).unwrap();
        std::fs::remove_dir(&root).unwrap();
        std::fs::remove_file(&path).unwrap();
        for entry in std::fs::read_dir(&parent).unwrap() {
            let entry = entry.unwrap();
            assert!(
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".seuoffice-stage-")
            );
            std::fs::remove_dir(entry.path()).unwrap();
        }
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn pending_context_rejects_trust_revocation_and_relocation() {
        let settings = trusted();
        let mut authority = ContextAuthority::capture(&settings, "project").unwrap();
        assert!(authority.is_current(&settings));

        let mut revoked = settings.clone();
        revoked.trust.get_mut("project").unwrap().decision = "without".into();
        assert!(!authority.is_current(&revoked));
        let before = revoked.clone();
        assert!(!authority.apply_if_current(
            &mut revoked,
            &[(ProjectHashedFile::Instructions, Some("stale".into()))]
        ));
        assert_eq!(revoked, before);

        let mut relocated = settings.clone();
        relocated.list[0].path = "/other".into();
        assert!(!authority.is_current(&relocated));
        let before = relocated.clone();
        assert!(!authority.apply_if_current(
            &mut relocated,
            &[(ProjectHashedFile::Instructions, Some("stale".into()))]
        ));
        assert_eq!(relocated, before);
    }

    #[test]
    fn pending_context_accepts_its_own_hash_update_but_not_another_hash() {
        let mut settings = trusted();
        let mut authority = ContextAuthority::capture(&settings, "project").unwrap();
        let updates = [(ProjectHashedFile::Instructions, Some("one".to_owned()))];
        assert!(authority.apply_if_current(&mut settings, &updates));
        assert!(authority.is_current(&settings));
        settings
            .trust
            .get_mut("project")
            .unwrap()
            .hashes
            .set("instructions", "two".into());
        assert!(!authority.is_current(&settings));
    }

    #[test]
    fn staged_writes_apply_to_latest_project_settings() {
        let mut settings = trusted();
        ProjectSettingsWrite::Switches {
            project_id: "project".into(),
            patch: ProjectSettingsPatch {
                memory_enabled: Some(false),
                read_agents_files: None,
            },
        }
        .apply(&mut settings);
        ProjectSettingsWrite::Switches {
            project_id: "project".into(),
            patch: ProjectSettingsPatch {
                memory_enabled: None,
                read_agents_files: Some(true),
            },
        }
        .apply(&mut settings);
        ProjectSettingsWrite::Pin {
            project_id: "project".into(),
            pinned: true,
        }
        .apply(&mut settings);
        let switches = settings_of("project", &settings);
        assert!(!switches.memory_enabled);
        assert!(switches.read_agents_files);
        assert!(settings.list[0].pinned);
        ProjectSettingsWrite::Remove {
            project_id: "project".into(),
        }
        .apply(&mut settings);
        assert!(settings.list.is_empty());
        assert!(settings.trust.get("project").is_some());
    }

    #[test]
    fn completed_touch_flush_does_not_discard_a_newer_touch() {
        let guide = ProjectGuide::new(ProjectGuideDeps {
            settings_path: String::new(),
            home_dir: String::new(),
            user_data: String::new(),
            default_parent_dir: Box::new(String::new),
            platform: None,
            env: None,
            now: Some(Box::new(|| 10.0)),
            new_id: None,
            language: None,
            context_files: None,
            touch_write_ms: Some(100.0),
            local_date: Box::new(|_| LocalDateParts {
                full_year: 2025,
                month: 0,
                date: 1,
                hours: 0,
                minutes: 0,
                seconds: 0,
            }),
            warn: None,
        });
        guide
            .pending_touches
            .borrow_mut()
            .set("project", "first".into());
        let flush = guide.prepare_touch_flush().unwrap();
        guide
            .pending_touches
            .borrow_mut()
            .set("project", "newer".into());
        guide.finish_touch_flush(&flush);
        assert_eq!(
            guide
                .pending_touches
                .borrow()
                .get("project")
                .map(String::as_str),
            Some("newer")
        );
        assert!(guide.touch_timer().is_some());
    }

    #[test]
    fn trust_answer_cannot_approve_a_relocated_project() {
        let mut settings = trusted();
        settings.trust.get_mut("project").unwrap().decision = "without".into();
        let authority = ContextAuthority::capture(&settings, "project").unwrap();
        let write = TrustWrite {
            authority,
            change: TrustChange::Approve(JsObject::new()),
        };
        settings.list[0].path = "/relocated".into();
        let before = settings.clone();
        assert!(write.apply(&mut settings).is_err());
        assert_eq!(settings, before);
    }

    #[test]
    fn queued_revocation_keeps_the_latest_hashes() {
        let mut settings = trusted();
        let authority = ContextAuthority::capture(&settings, "project").unwrap();
        settings
            .trust
            .get_mut("project")
            .unwrap()
            .hashes
            .set("instructions", "latest".into());
        let write = TrustWrite {
            authority,
            change: TrustChange::Without,
        };
        let (authority, decision) = write.apply(&mut settings).unwrap();
        assert_eq!(decision, ProjectTrustDecision::Without);
        assert!(authority.is_current(&settings));
        let trust = settings.trust.get("project").unwrap();
        assert_eq!(trust.decision, "without");
        assert_eq!(
            trust.hashes.get("instructions").map(String::as_str),
            Some("latest")
        );
    }

    #[test]
    fn queued_install_candidates_keep_the_first_committed_identity() {
        let mut settings = trusted();
        InstallIdWrite {
            candidate: "first".into(),
        }
        .apply(&mut settings);
        InstallIdWrite {
            candidate: "second".into(),
        }
        .apply(&mut settings);
        assert_eq!(settings.install_id, "first");
    }

    #[test]
    fn install_identity_reports_settings_write_failure() {
        let write = InstallIdWrite {
            candidate: "candidate".into(),
        };
        assert!(write.execute("\0").is_err());
    }

    #[test]
    fn revoked_owner_cannot_commit_install_identity() {
        let dir =
            std::env::temp_dir().join(format!("project-install-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let original = "{\"other\":\"preserved\"}";
        std::fs::write(&path, original).unwrap();
        let write = InstallIdWrite {
            candidate: "candidate".into(),
        };
        let result = write.execute_guarded(path.to_str().unwrap(), |_, _| {
            Err(NodeError {
                code: None,
                message: "owner revoked".into(),
            })
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
