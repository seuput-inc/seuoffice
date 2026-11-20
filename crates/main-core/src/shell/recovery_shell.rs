//! `apps/shell/src/main/recovery-shell.ts`: the shell's Electron side of crash
//! recovery: the editors' answers about an untitled document's copy, the
//! openers restored tabs load through, and the launch flow that carries out
//! launch-recovery.ts.
//!
//! What stays with the caller: the shell's one recovery store (the
//! `store ??= createRecoveryStore(...)` of `shellRecoveryStore`: the caller
//! keeps one [`RecoveryStore`] and hands it to every function that reads it),
//! the editors' main modules ([`RecoveryEditors`]), the tab manager
//! ([`LaunchManager`]), the launch list's gate and the rest of the launch's
//! `deps` ([`LaunchDeps`]). The plan types and `planLaunch` and
//! `resolveLaunchDecisions` are those of [`crate::shell::launch_recovery`].
//! The module-level `plannedUntitled` map is a [`RecoveryShell`] the caller
//! owns. [`LaunchMachine`] returns owned requests and resumes only on their
//! actual completion, including the launch list's answer.

use std::collections::HashMap;

#[path = "recovery_shell_async.rs"]
mod async_launch;
pub use async_launch::{LaunchMachine, LaunchNeed, LaunchReply, LaunchStep};

use crate::node_fs::NodeError;
use crate::packages::electron_utils::recovery_contract::RecoveryOpenUntitledRequest;
use crate::packages::electron_utils::recovery_store::{
    RecoveryFileIdentity, RecoveryIdentity, RecoveryLaunchDecision, RecoveryOpenPlan,
    RecoveryStore, RecoveryUntitledIdentity, is_recovery_editor_kind, is_untitled_identity,
};
use crate::shell::launch_recovery::{
    LaunchJudgment, LaunchPlan, PlannedOpen, PlannedTab, plan_launch,
};
use crate::shell::renderer_disposal::Failure;
use crate::shell::session_record::{
    SessionRecordProcess, SessionRecordReadResult, SessionRecorderEnv, SessionRecorderOptions,
    SessionUntitledCopy, judge_previous_exit,
};

/// the part of a `WebContents` this module reads
pub trait RecoveryContents {
    fn id(&self) -> i64;
    fn is_destroyed(&self) -> bool;
}

/// What the editors' main modules answer, by editor (the `switch` of
/// `editorRecoveryIdentity` picks one). Each may throw.
pub trait RecoveryEditors {
    type Contents: RecoveryContents;
    /// the view a editor makes, whose `webContents` the contract opens a copy in
    type View;

    fn docs_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure>;
    fn sheets_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure>;
    fn pdf_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure>;
    fn markdown_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure>;
    fn html_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure>;
    fn mindmap_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure>;
    fn hwp_recovery_identity(
        &self,
        contents: &Self::Contents,
    ) -> Result<Option<RecoveryIdentity>, Failure>;
    fn slides_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure>;
    fn diagram_recovery_identity(&self, id: i64) -> Result<Option<RecoveryIdentity>, Failure>;

    fn discard_docs_recovery(&self, contents: &Self::Contents);
    fn discard_sheets_recovery(&self, contents: &Self::Contents);
    fn discard_pdf_recovery(&self, contents: &Self::Contents);
    fn discard_mindmap_recovery(&self, contents: &Self::Contents);
    fn hwp_discard_recovery_copy(&self, contents: &Self::Contents);
    fn discard_diagram_recovery(&self, contents: &Self::Contents);

    /// Opens and fully initializes the selected recovered view before completion.
    fn open_recovered_view(
        &self,
        kind: &str,
        title: &str,
        file_path: Option<&str>,
        copy: Option<&RecoveryOpenUntitledRequest>,
        done: Box<dyn FnOnce(Result<Self::View, Failure>)>,
    );

    fn has_recovery_editor(&self, kind: &str) -> bool;

    /// `console.warn(prefix, detail)`
    fn warn(&self, prefix: &str, detail: &str);
    /// `setImmediate(() => deps.afterSheetsOpen())`
    fn after_sheets_open_soon(&self);
    /// `setMarkdownRecoveryTitleSource(titleOf)`, `setHtmlRecoveryTitleSource`,
    /// `setMindmapRecoveryTitleHook`, `setDiagramRecoveryTitleHook`
    fn set_recovery_title_source(&self, editor: &str);
    /// `setHwpRecoveryTitle(contents, title)`
    fn set_hwp_recovery_title(&self, contents: &Self::Contents, title: &str);
}

/// `plannedUntitled`: untitled-copy identities of restored tabs, by tab id,
/// until their editor answers itself
#[derive(Debug, Default)]
pub struct RecoveryShell {
    planned_untitled: HashMap<String, RecoveryUntitledIdentity>,
}

/// `UnloadedTabSpec`'s `opener`
#[derive(Debug, Clone, PartialEq)]
pub enum RestoredTabOpener {
    UntitledCopy {
        copy: RecoveryIdentity,
        save_as_default_path: Option<String>,
    },
    File,
}

/// `UnloadedTabSpec`
#[derive(Debug, Clone, PartialEq)]
pub struct UnloadedTabSpec {
    pub kind: String,
    pub title: String,
    pub file_path: Option<String>,
    pub opener: RestoredTabOpener,
}

/// `RestoredTabOpenerDeps`
pub trait RestoredTabOpenerDeps {
    /// the launch list is not answered yet: nothing may load
    fn blocked(&self) -> bool;
}

/// the context the opener is called with (`UnloadedTabOpenContext`)
#[derive(Debug, Clone, PartialEq)]
pub struct OpenContext {
    pub kind: String,
    pub title: String,
    pub file_path: Option<String>,
}

/// a tab of `manager.list()`
#[derive(Debug, Clone, PartialEq)]
pub struct ListedTab {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub file_path: Option<String>,
}

/// the part of the `TabManager` this module drives
pub trait LaunchManager {
    type Contents;
    fn list(&self) -> Result<Vec<ListedTab>, Failure>;
    /// `manager.splitState()` is set
    fn split_state(&self) -> Result<bool, Failure>;
    fn web_contents_for_tab(&self, id: &str) -> Option<Self::Contents>;
}

/// `LaunchRecoveryDeps`; the gate's methods are here too
pub trait LaunchDeps {
    type Manager: LaunchManager;
    /// `deps.gate.blocking()`
    fn gate_blocking(&self) -> bool;
    /// `deps.gate.publishNone()`
    fn gate_publish_none(&mut self);
    /// `deps.manager()`
    fn manager(&mut self) -> &mut Self::Manager;
}

/// `asSessionCopy`
fn as_session_copy(identity: &RecoveryUntitledIdentity) -> SessionUntitledCopy {
    SessionUntitledCopy {
        untitled_id: identity.untitled_id.clone(),
        ext: identity.ext.clone(),
    }
}

/// `editorRecoveryIdentity`: the copy a tab's editor writes (its file's or its
/// untitled slot); `None` when it does not say
pub fn editor_recovery_identity<E: RecoveryEditors>(
    editors: &E,
    kind: &str,
    contents: &E::Contents,
) -> Option<RecoveryIdentity> {
    if contents.is_destroyed() {
        return None;
    }
    let answer = match kind {
        "docs" => editors.docs_recovery_identity(contents.id()),
        "sheets" => editors.sheets_recovery_identity(contents.id()),
        "pdf" => editors.pdf_recovery_identity(contents.id()),
        "markdown" => editors.markdown_recovery_identity(contents.id()),
        "html" => editors.html_recovery_identity(contents.id()),
        "mindmap" => editors.mindmap_recovery_identity(contents.id()),
        "hwp" => editors.hwp_recovery_identity(contents),
        "slides" => editors.slides_recovery_identity(contents.id()),
        "diagram" => editors.diagram_recovery_identity(contents.id()),
        _ => Ok(None),
    };
    answer.unwrap_or(None)
}

impl RecoveryShell {
    pub fn new() -> Self {
        Self::default()
    }

    /// `tabUntitledCopy`: the untitled copy a tab stands for in the session record
    pub fn tab_untitled_copy<E: RecoveryEditors>(
        &self,
        editors: &E,
        tab_id: &str,
        tab_file_path: Option<&str>,
        kind: &str,
        contents: Option<&E::Contents>,
    ) -> Option<SessionUntitledCopy> {
        if let Some(contents) = contents
            && !contents.is_destroyed()
            && let Some(identity) = editor_recovery_identity(editors, kind, contents)
        {
            return match &identity {
                RecoveryIdentity::Untitled(untitled) => Some(as_session_copy(untitled)),
                RecoveryIdentity::File(_) => None,
            };
        }
        if tab_file_path.is_some_and(|p| !p.is_empty()) {
            return None;
        }
        self.planned_untitled.get(tab_id).map(as_session_copy)
    }

    /// `crashedUntitledCopy`: the untitled copy to reopen a crashed tab from,
    /// only an untitled document whose copy is on disk
    pub fn crashed_untitled_copy<E: RecoveryEditors>(
        &self,
        store: &RecoveryStore<'_>,
        editors: &E,
        tab_id: Option<&str>,
        tab_kind: &str,
        tab_file_path: Option<&str>,
        contents: &E::Contents,
    ) -> Result<Option<RecoveryUntitledIdentity>, NodeError> {
        if tab_kind == "home" || tab_kind == "help" {
            return Ok(None);
        }
        let mut identity = editor_recovery_identity(editors, tab_kind, contents);
        if identity.is_none()
            && tab_file_path.is_none_or(|p| p.is_empty())
            && let Some(id) = tab_id.filter(|id| !id.is_empty())
        {
            identity = self
                .planned_untitled
                .get(id)
                .cloned()
                .map(RecoveryIdentity::Untitled);
        }
        let Some(RecoveryIdentity::Untitled(untitled)) = identity else {
            return Ok(None);
        };
        let identity = RecoveryIdentity::Untitled(untitled.clone());
        Ok(store.copy_info(&identity)?.map(|_| untitled))
    }

    /// `notePlanned`
    pub fn note_planned(&mut self, tab_id: &str, planned: &PlannedTab) {
        if let PlannedOpen::UntitledCopy { copy, .. } = &planned.open
            && is_untitled_identity(copy)
            && let RecoveryIdentity::Untitled(untitled) = copy
        {
            self.planned_untitled
                .insert(tab_id.to_owned(), untitled.clone());
        }
    }

    /// Prepare the immutable unloaded tab; the caller adds and activates it
    /// through the actual asynchronous tab host and notes the returned id.
    pub fn reopen_untitled_copy_plan(
        kind: &str,
        title: &str,
        copy: &RecoveryUntitledIdentity,
    ) -> PlannedTab {
        PlannedTab {
            key: "crash".to_owned(),
            kind: kind.to_owned(),
            title: title.to_owned(),
            open: PlannedOpen::UntitledCopy {
                copy: RecoveryIdentity::Untitled(copy.clone()),
                save_as_default_path: None,
            },
            file_path: None,
        }
    }
}

/// Completes only after the selected copy has actually been initialized.
pub fn open_untitled_copy_view<E: RecoveryEditors>(
    editors: &E,
    kind: &str,
    request: &RecoveryOpenUntitledRequest,
    done: Box<dyn FnOnce(Result<E::View, Failure>)>,
) {
    if !is_recovery_editor_kind(kind) || !editors.has_recovery_editor(kind) {
        return done(Err(Failure(format!(
            "the {kind} editor cannot open a recovery copy"
        ))));
    }
    editors.open_recovered_view(kind, &request.title, None, Some(request), done);
}

/// `restoredTabOpener`
pub fn restored_tab_opener(planned: &PlannedTab) -> RestoredTabOpener {
    match &planned.open {
        PlannedOpen::UntitledCopy {
            copy,
            save_as_default_path,
        } => RestoredTabOpener::UntitledCopy {
            copy: copy.clone(),
            save_as_default_path: save_as_default_path.clone(),
        },
        PlannedOpen::File => RestoredTabOpener::File,
    }
}

impl RestoredTabOpener {
    /// `opener.open(context)`: the view the tab loads through
    pub fn open<E: RecoveryEditors>(
        &self,
        editors: &E,
        deps: &dyn RestoredTabOpenerDeps,
        context: &OpenContext,
        done: Box<dyn FnOnce(Result<E::View, Failure>)>,
    ) {
        if deps.blocked() {
            return done(Err(Failure(
                "the launch recovery list is not answered yet".to_owned(),
            )));
        }
        match self {
            RestoredTabOpener::UntitledCopy {
                copy,
                save_as_default_path,
            } => {
                let request = RecoveryOpenUntitledRequest {
                    copy: copy.clone(),
                    title: context.title.clone(),
                    save_as_default_path: save_as_default_path
                        .clone()
                        .filter(|path| !path.is_empty()),
                };
                open_untitled_copy_view(editors, &context.kind, &request, done);
            }
            RestoredTabOpener::File => editors.open_recovered_view(
                &context.kind,
                &context.title,
                context.file_path.as_deref(),
                None,
                done,
            ),
        }
    }

    /// `opener.needsUser(context)`: opening the document has to ask the user
    /// the per-file recovery question first
    pub fn needs_user(
        &self,
        store: &RecoveryStore<'_>,
        deps: &dyn RestoredTabOpenerDeps,
        context: &OpenContext,
    ) -> bool {
        match self {
            RestoredTabOpener::UntitledCopy { .. } => deps.blocked(),
            RestoredTabOpener::File => {
                if deps.blocked() {
                    return true;
                }
                let Some(path) = context.file_path.as_deref().filter(|p| !p.is_empty()) else {
                    return false;
                };
                if !is_recovery_editor_kind(&context.kind) {
                    return false;
                }
                let identity = RecoveryIdentity::File(RecoveryFileIdentity {
                    kind: context.kind.clone(),
                    file_path: path.to_owned(),
                });
                // an unsettled copy of the file asks the user first (5.4)
                matches!(store.open_plan(&identity), Ok(RecoveryOpenPlan::Ask(_)))
            }
        }
    }
}

/// `tabSpec`
pub fn tab_spec(planned: &PlannedTab) -> UnloadedTabSpec {
    UnloadedTabSpec {
        kind: planned.kind.clone(),
        title: planned.title.clone(),
        file_path: planned.file_path.clone().filter(|p| !p.is_empty()),
        opener: restored_tab_opener(planned),
    }
}

/// `discardEditorRecovery`: the editors' own "Don't Save" for their copy when
/// MCP / AI closes a tab without saving
pub fn discard_editor_recovery<E: RecoveryEditors>(
    editors: &E,
    kind: &str,
    contents: &E::Contents,
) {
    if contents.is_destroyed() {
        return;
    }
    match kind {
        "docs" => editors.discard_docs_recovery(contents),
        "sheets" => editors.discard_sheets_recovery(contents),
        "pdf" => editors.discard_pdf_recovery(contents),
        "mindmap" => editors.discard_mindmap_recovery(contents),
        "hwp" => editors.hwp_discard_recovery_copy(contents),
        "diagram" => editors.discard_diagram_recovery(contents),
        _ => {}
    }
}

/// the editors `installRecoveryTitleSources` gives the title lookup to, in order
pub const RECOVERY_TITLE_SOURCES: [&str; 4] = ["markdown", "html", "mindmap", "diagram"];

/// `installRecoveryTitleSources`: the untitled documents' copies are listed
/// under their real tab titles
pub fn install_recovery_title_sources<E: RecoveryEditors>(editors: &E) {
    for editor in RECOVERY_TITLE_SOURCES {
        editors.set_recovery_title_source(editor);
    }
}

/// `pushHwpRecoveryTitles`: hwp is told its untitled tabs' titles (it does not ask)
pub fn push_hwp_recovery_titles<E, M>(editors: &E, manager: &M) -> Result<(), Failure>
where
    E: RecoveryEditors,
    M: LaunchManager<Contents = E::Contents>,
{
    for tab in manager.list()? {
        if tab.kind != "hwp" || tab.file_path.as_deref().is_some_and(|p| !p.is_empty()) {
            continue;
        }
        if let Some(contents) = manager.web_contents_for_tab(&tab.id)
            && !contents.is_destroyed()
        {
            editors.set_hwp_recovery_title(&contents, &tab.title);
        }
    }
    Ok(())
}

/// Store operations used by launch. Implementations release any store lock
/// before returning, so editor and tab callbacks can safely use the same store.
pub trait LaunchRecoveryStore {
    fn prune_stale_copies(&self);
    fn has_unresolved_copies(&self) -> Result<bool, Failure>;
    fn plan(
        &self,
        judgment: &LaunchJudgment,
        previous: &SessionRecordReadResult,
        paths: &[String],
    ) -> Result<LaunchPlan, Failure>;
    fn apply_decision(
        &self,
        identity: &RecoveryIdentity,
        decision: RecoveryLaunchDecision,
    ) -> Result<(), Failure>;
}

impl LaunchRecoveryStore for RecoveryStore<'_> {
    fn prune_stale_copies(&self) {
        RecoveryStore::prune_stale_copies(self, None);
    }
    fn has_unresolved_copies(&self) -> Result<bool, Failure> {
        RecoveryStore::has_unresolved_copies(self).map_err(|error| Failure(error.message))
    }
    fn plan(
        &self,
        judgment: &LaunchJudgment,
        previous: &SessionRecordReadResult,
        paths: &[String],
    ) -> Result<LaunchPlan, Failure> {
        plan_launch(judgment, previous, self, paths)
    }
    fn apply_decision(
        &self,
        identity: &RecoveryIdentity,
        decision: RecoveryLaunchDecision,
    ) -> Result<(), Failure> {
        self.apply_launch_decision(identity, decision)
            .map(|_| ())
            .map_err(|error| Failure(error.message))
    }
}

/// Filesystem preparation for one launch. Run on a worker with the retained
/// session process and store; migration must perform the original legacy move.
#[allow(clippy::too_many_arguments)]
pub fn prepare_launch(
    store: &dyn LaunchRecoveryStore,
    session: &mut SessionRecordProcess,
    session_env: &mut dyn SessionRecorderEnv,
    user_data_dir: String,
    launch_paths: &[String],
    reopened_by_installer: bool,
    migrate: impl FnOnce(),
    check: impl Fn() -> Result<(), Failure>,
) -> Result<LaunchPlan, Failure> {
    let (plan, write) = prepare_launch_staged(
        store,
        session,
        || session_env.now(),
        user_data_dir,
        launch_paths,
        reopened_by_installer,
        migrate,
        check,
    )?;
    if let Some(write) = write {
        let result = session_env.write_json(&write.file_path, &write.record);
        if let Some(recorder) = session.session_recorder() {
            recorder.complete_write(&write, result, session_env);
        }
    }
    Ok(plan)
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_launch_staged(
    store: &dyn LaunchRecoveryStore,
    session: &mut SessionRecordProcess,
    now: impl FnOnce() -> f64,
    user_data_dir: String,
    launch_paths: &[String],
    reopened_by_installer: bool,
    migrate: impl FnOnce(),
    check: impl Fn() -> Result<(), Failure>,
) -> Result<(LaunchPlan, Option<super::session_record::SessionWritePlan>), Failure> {
    check()?;
    store.prune_stale_copies();
    check()?;
    migrate();
    check()?;
    let recorder = session.init_session_record(&SessionRecorderOptions {
        user_data_dir,
        write_delay_ms: None,
        crash_loop_window_ms: None,
    });
    let mut unresolved_error = None;
    let judgment = judge_previous_exit(
        &recorder.previous,
        &mut || {
            if let Err(error) = check() {
                unresolved_error = Some(error);
                return false;
            }
            match store.has_unresolved_copies() {
                Ok(unresolved) => unresolved,
                Err(error) => {
                    unresolved_error = Some(error);
                    false
                }
            }
        },
        reopened_by_installer,
    );
    if let Some(error) = unresolved_error {
        return Err(error);
    }
    check()?;
    let plan = store.plan(
        &LaunchJudgment::from(&judgment),
        &recorder.previous,
        launch_paths,
    )?;
    check()?;
    let write = recorder.prepare_start_run(now(), plan.restore_run);
    Ok((plan, write))
}
