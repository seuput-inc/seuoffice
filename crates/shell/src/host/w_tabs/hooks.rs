use tauri_runtime_cef::WebviewCefExt;
use main_core::shell::tab_manager::{DiagramIdentity, TabError, TabKind, TabResult};
use shell_contract::{IpcError, ViewId};
use shell_contract::editors::Then;
use super::Hooks;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use main_core::packages::ai_provider::user_skills_import::{dropped_skill_source_of, SkillImportSource};
use main_core::shell::user_skills::user_skills_ipc::DroppedSkillEntry;
use shell_contract::runtime::{PathName, Views};
use shell_ipc_w::tabs::WindowsHost;

#[path = "native_diagram.rs"]
mod native_diagram;

pub(super) struct DocsRegistrationRollback(pub(super) shell_ipc_d2::docs::editors::DocsRegisteredView);
impl shell_ipc_w::tabs::host::EditorSeedRollback for DocsRegistrationRollback {
    fn retire_closed(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_closed(&proof)); }
    fn retire_adopted(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_adopted(&proof)); }
    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_superseded(&proof)); }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.0.discard().and_then(|removed| if removed { Ok(()) } else { Err("The captured Docs registration was replaced before rollback".into()) }));
    }
}

pub(super) struct SheetsRegistrationRollback(pub(super) shell_ipc_d1::sheets::editors::RegisteredTab);
impl shell_ipc_w::tabs::host::EditorSeedRollback for SheetsRegistrationRollback {
    fn retire_closed(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_closed(&proof)); }
    fn retire_adopted(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_adopted(&proof)); }
    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_superseded(&proof)); }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(self.0.discard().and_then(|removed| if removed { Ok(()) } else { Err("The captured Sheets registration cannot be rolled back".into()) }));
    }
}

struct DocsOpenRollback(shell_ipc_d2::docs::editors::DocsOpenReceipt);
impl shell_ipc_w::tabs::host::EditorSeedRollback for DocsOpenRollback {
    fn retire_closed(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_closed(&proof)); }
    fn retire_adopted(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_adopted(&proof)); }
    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_superseded(&proof)); }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.discard()); }
}

struct WorkbookQueueRollback(shell_ipc_d1::sheets::editors::WorkbookQueueReceipt);
impl shell_ipc_w::tabs::host::EditorSeedRollback for WorkbookQueueRollback {
    fn retire_closed(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_closed(&proof)); }
    fn retire_adopted(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_adopted(&proof)); }
    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_superseded(&proof)); }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.rollback()); }
}

struct DocsBlankRollback(shell_ipc_d2::docs::editors::DocsBlankReceipt);
impl shell_ipc_w::tabs::host::EditorSeedRollback for DocsBlankRollback {
    fn retire_closed(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_closed(&proof)); }
    fn retire_adopted(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_adopted(&proof)); }
    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_superseded(&proof)); }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.rollback()); }
}

struct SheetsBlankRollback(shell_ipc_d1::sheets::editors::BlankSeedReceipt);
impl shell_ipc_w::tabs::host::EditorSeedRollback for SheetsBlankRollback {
    fn retire_closed(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_closed(&proof)); }
    fn retire_adopted(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_adopted(&proof)); }
    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_superseded(&proof)); }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.rollback()); }
}

struct DocsAiRollback(crate::host::d2::docs_tabs::DocsAiSeedRollback);
impl shell_ipc_w::tabs::host::EditorSeedRollback for DocsAiRollback {
    fn retire_closed(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_closed(&proof)); }
    fn retire_adopted(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_adopted(&proof)); }
    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.retire_superseded(&proof)); }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) { done(self.0.rollback()); }
}

type SeedReceipt = std::rc::Rc<dyn shell_ipc_w::tabs::host::EditorSeedRollback>;
type SeedResult = Result<Option<SeedReceipt>, (String, Option<SeedReceipt>)>;

fn docs_ai_result(result: Result<crate::host::d2::docs_tabs::DocsAiSeedRollback, crate::host::d2::docs_tabs::DocsAiSeedFailure>) -> SeedResult {
    match result {
        Ok(receipt) => Ok(Some(std::rc::Rc::new(DocsAiRollback(receipt)))),
        Err(failure) => Err((failure.error, failure.rollback.map(|receipt| std::rc::Rc::new(DocsAiRollback(receipt)) as SeedReceipt))),
    }
}

fn retain_seed_result(result: SeedResult, rollback: &shell_ipc_w::tabs::host::EditorSeedRollbackStore) -> TabResult<()> {
    match result {
        Ok(receipt) => {
            if let Some(receipt) = receipt { rollback.borrow_mut().push(receipt); }
            Ok(())
        }
        Err((error, receipt)) => {
            if let Some(receipt) = receipt { rollback.borrow_mut().push(receipt); }
            Err(TabError(error))
        }
    }
}

struct PlacementRollback {
    placer: std::rc::Rc<RefCell<main_core::shell::projects::new_documents::NewDocumentPlacer<std::rc::Rc<main_core::shell::projects::project_guide::ProjectGuide>>>>,
    receipt: RefCell<Option<main_core::shell::projects::new_documents::NewDocumentPresetReceipt>>,
    view: ViewId,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    cancelled: std::cell::Cell<bool>,
}
impl shell_ipc_w::tabs::host::EditorSeedRollback for PlacementRollback {
    fn retire_closed(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) {
        done(if proof.matches(&self.reservation, self.view, &self.created) { Ok(()) }
            else { Err("The project placement closure does not match its reservation".into()) });
    }
    fn retire_adopted(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) {
        if !proof.matches(&self.reservation, self.view, &self.created) || proof.outcome() != shell_contract::ipc::NativeClosureOutcome::Closed {
            return done(Err("An adopted placement requires its exact closed browser".into()));
        }
        let result = self.receipt.borrow_mut().as_mut().map_or(Ok(()), |receipt| self.placer.borrow_mut().retire_preset(receipt));
        done(result);
    }
    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) {
        if !proof.matches(&self.reservation, &self.owner) {
            return done(Err("The project placement supersession does not match its browser".into()));
        }
        done(self.receipt.borrow_mut().as_mut().map_or(Ok(()), |receipt| self.placer.borrow_mut().retire_preset(receipt)));
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        let result = match self.receipt.borrow_mut().as_mut() {
            Some(receipt) => self.placer.borrow_mut().rollback_preset(receipt),
            None => { self.cancelled.set(true); Ok(()) }
        };
        done(result);
    }
}

enum GeneratedSeedStage {
    Empty,
    Generated(crate::host::net::tools::generate::PreparedGeneratedSeed),
    Docs(crate::host::d2::docs_tabs::PreparedDocsAiSeed),
}

struct PreparedGeneratedSeedStage {
    seed: Option<GeneratedSeedStage>,
    placement: Option<main_core::shell::projects::new_documents::PreparedNewDocumentPreset>,
    placement_receipt: Option<std::rc::Rc<PlacementRollback>>,
}

impl PreparedGeneratedSeedStage {
    fn with_validated(
        &mut self, transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if self.placement_receipt.as_ref().is_some_and(|receipt| receipt.cancelled.get()) {
            return Err("The project placement was cancelled before publication".into());
        }
        let mut placer = self.placement_receipt.as_ref().map(|receipt| receipt.placer.borrow_mut());
        let mut placement = match (self.placement.take(), self.placement_receipt.as_ref()) {
            (Some(prepared), Some(_)) => Some(placer.as_deref_mut().ok_or("The project placement service disappeared")?.validate_preset(prepared)?),
            (None, None) => None,
            _ => return Err("The project placement receipt does not match its plan".into()),
        };
        let mut initialize = || {
            next()?;
            if let (Some(placement), Some(slot)) = (placement.take(), self.placement_receipt.as_ref()) {
                *slot.receipt.borrow_mut() = Some(placement.commit());
            }
            Ok(())
        };
        match self.seed.take().ok_or("The generated seed plan was already consumed")? {
            GeneratedSeedStage::Empty => initialize(),
            GeneratedSeedStage::Generated(plan) => plan.commit(transition, &mut initialize).map(|_| ()),
            GeneratedSeedStage::Docs(plan) => plan.commit(transition, &mut initialize)
                .map(|_| ()).map_err(|failure| failure.error),
        }
    }
}

struct SupersededDrain {
    receipts: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    proof: shell_contract::ipc::NativeBrowserSuperseded,
    running: std::rc::Rc<std::cell::Cell<bool>>,
}

struct SupersededAttempt {
    running: std::rc::Rc<std::cell::Cell<bool>>,
    done: Option<Box<dyn FnOnce(Result<(), String>)>>,
}

impl Drop for SupersededAttempt {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            self.running.set(false);
            done(Err("The superseded editor cleanup discarded its completion".into()));
        }
    }
}

impl SupersededDrain {
    fn begin(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        if self.running.replace(true) {
            return done(Err("The superseded editor cleanup is already running".into()));
        }
        let mut attempt = SupersededAttempt { running: self.running.clone(), done: Some(done) };
        shell_ipc_w::tabs::rollback_editor_receipts(
            self.receipts.clone(),
            Some(shell_contract::ipc::NativeBrowserRetirement::Superseded(self.proof.clone())),
            true,
            Box::new(move |result| {
                attempt.running.set(false);
                if let Some(done) = attempt.done.take() { done(result); }
            }),
        );
    }
}

impl shell_ipc_w::tabs::host::EditorSeedRollback for SupersededDrain {
    fn retire_closed(&self, _: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { self.begin(done); }
    fn retire_adopted(&self, _: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) { self.begin(done); }
    fn retire_superseded(&self, _: shell_contract::ipc::NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) { self.begin(done); }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) { self.begin(done); }
}

type DropCompletion = Box<dyn FnOnce(Option<SkillImportSource>)>;
thread_local! {
    static DROPS: RefCell<HashMap<u64, DropCompletion>> = RefCell::new(HashMap::new());
}
static NEXT_DROP: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static PREPARATION_FAILURES: RefCell<HashMap<u64, Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationFailure)>>> = RefCell::new(HashMap::new());
}
static NEXT_PREPARATION_FAILURE: AtomicU64 = AtomicU64::new(1);

pub(super) fn fail_prepared_view(
    prepared: super::accepted::PreparedEditorView, error: TabError,
    done: Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationFailure)>,
) {
    let id = NEXT_PREPARATION_FAILURE.fetch_add(1, Ordering::Relaxed);
    PREPARATION_FAILURES.with(|entries| { entries.borrow_mut().insert(id, done); });
    let proof = prepared.proof.clone();
    let cx = prepared.cx.clone();
    let core = prepared.core.clone();
    let view = prepared.view;
    let created = prepared.generation.clone();
    drop(prepared);
    let cleanup: shell_ipc_w::tabs::host::ViewCreationCleanup = std::sync::Arc::new(move |done| {
        super::accepted::cleanup_proof(cx.clone(), view, created.clone(), proof.clone(), done);
    });
    let retry = cleanup.clone();
    cleanup(Box::new(move |result| core.post(Box::new(move || {
        let cleanup = match result {
            Ok(receipt) => shell_ipc_w::tabs::host::ViewCreationCleanupState::Acknowledged(receipt),
            Err(_) => shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(retry),
        };
        if let Some(done) = PREPARATION_FAILURES.with(|entries| entries.borrow_mut().remove(&id)) {
            done(shell_ipc_w::tabs::host::ViewCreationFailure { error, cleanup });
        }
    }))));
}

fn view_id(id: i64) -> Option<ViewId> {
    u32::try_from(id).ok().map(ViewId)
}


pub(crate) fn default_save_dir(services: &shell_contract::runtime::Services) -> String {
    let state = services.hosts.get::<shell_ipc_w::SharedSaveDirState>().expect("save directory state unavailable");
    let mut state = state.lock().expect("save directory state poisoned");
    main_core::packages::electron_utils::default_save_dir::configured_default_save_dir(
        &crate::paths::ShellPaths::new(), &mut state, main_core::packages::pdf_structure::files::system_now_ms(),
    ).expect("default save directory unavailable")
}

impl Hooks {
    fn prepare_generated_seed_stage(
        &self, view: ViewId, seed: Option<main_core::shell::tab_manager::EditorSeed>,
        placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        source: &shell_contract::ipc::NativeRequestOwner, participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
        created: &shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation, browser: &shell_contract::ipc::NativeBrowserOwner,
        rollback: &shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    ) -> TabResult<PreparedGeneratedSeedStage> {
        let effects = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned()
            .ok_or_else(|| TabError("Owned initialization effects are unavailable".into()))?;
        rollback.borrow_mut().try_reserve(2).map_err(|error| TabError(error.to_string()))?;
        let (stage, seed_receipt): (GeneratedSeedStage, Option<SeedReceipt>) = match seed {
            Some(main_core::shell::tab_manager::EditorSeed::Generate(value)) => {
                let plan = crate::host::net::tools::generate::prepare_generate_handoff_owned(
                    &self.provided, view, &value, source, created, reservation, browser, effects.clone()).map_err(TabError)?;
                let receipt = plan.receipt();
                (GeneratedSeedStage::Generated(plan), Some(receipt))
            }
            Some(main_core::shell::tab_manager::EditorSeed::Conversion(value)) => {
                let plan = crate::host::net::tools::generate::prepare_converted_content_owned(
                    &self.provided, view, &value, source, cancel, created, reservation, browser, effects).map_err(TabError)?;
                let receipt = plan.receipt();
                (GeneratedSeedStage::Generated(plan), Some(receipt))
            }
            Some(main_core::shell::tab_manager::EditorSeed::DocsAi { new_blank, content }) => {
                let plan = match crate::host::d2::docs_tabs::prepare_docs_ai_seed_owned(
                    &self.provided, view, new_blank, content, source, participants, cancel, pending, created, reservation, browser,
                ) {
                    Ok(plan) => plan,
                    Err(failure) => return match retain_seed_result(docs_ai_result(Err(failure)), rollback) {
                        Err(error) => Err(error),
                        Ok(()) => Err(TabError("The Docs AI seed preparation failed".into())),
                    },
                };
                let receipt: SeedReceipt = std::rc::Rc::new(DocsAiRollback(plan.receipt()));
                (GeneratedSeedStage::Docs(plan), Some(receipt))
            }
            None => (GeneratedSeedStage::Empty, None),
        };
        let (placement, placement_receipt) = match placement {
            Some(placement) => {
                let placer = self.provided.all::<shell_ipc_w::projects::Projects>().into_iter().next()
                    .ok_or_else(|| TabError("Project services are unavailable".into()))?.new_documents();
                let prepared = placer.borrow_mut().prepare_preset(i64::from(view.0), Ok(placement)).map_err(TabError)?;
                let receipt = std::rc::Rc::new(PlacementRollback { placer, receipt: RefCell::new(None), view,
                    created: created.clone(), reservation: reservation.clone(), owner: browser.clone(),
                    cancelled: std::cell::Cell::new(false) });
                (Some(prepared), Some(receipt))
            }
            None => (None, None),
        };
        let mut store = rollback.borrow_mut();
        if let Some(receipt) = seed_receipt { store.push(receipt); }
        if let Some(receipt) = &placement_receipt { store.push(receipt.clone()); }
        drop(store);
        Ok(PreparedGeneratedSeedStage { seed: Some(stage), placement, placement_receipt })
    }
    fn recovery_cleanup_reporter(&self) -> Result<std::rc::Rc<dyn Fn(String)>, String> {
        let tabs = self.provided.all::<shell_ipc_w::tabs::TabsState>().into_iter().next()
            .ok_or("The tab manager is unavailable for recovery cleanup reporting")?;
        Ok(std::rc::Rc::new(move |error| tabs.report_recovery_cleanup_failure(error)))
    }

    fn open_opener_native_command_impl(
        &self, opener: &main_core::shell::tab_manager::Opener,
        context: &main_core::shell::tab_manager::UnloadedTabOpenContext,
        command: shell_contract::ipc::NativeInvocationOperation,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<shell_ipc_w::tabs::OwnedLoadedTabView, String>)>,
    ) {
        let reporter = match self.recovery_cleanup_reporter() { Ok(reporter) => reporter, Err(error) => return done(Err(error)) };
        let mut recovery = match crate::host::recovery_openers::prepare_open_command(opener, context, &command, cancel.clone()) {
            Ok(recovery) => recovery,
            Err(error) => return done(Err(error)),
        };
        let Some(attempt) = recovery.take_attempt(self.services.core.clone()) else {
            return done(Err("The selected recovery attempt is unavailable".into()));
        };
        let Some(operation) = recovery.operation.take() else {
            return attempt.fail_before_creation("The selected recovery operation is unavailable".into(),
                Box::new(move |result| done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())))));
        };
        let Some(before) = recovery.before_load.take() else {
            return attempt.fail_before_creation("The selected recovery initializer is unavailable".into(),
                Box::new(move |result| done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())))));
        };
        let constraint = shell_contract::ipc::CommandRecoveryConstraint {
            command, participants: Vec::new(), cancel, pending,
        };
        let kind = recovery.kind;
        let path = recovery.path.clone();
        let untitled = recovery.untitled;
        let opener = *opener;
        let context = context.clone();
        let callback = std::rc::Rc::new(RefCell::new(Some(done)));
        let completed = callback.clone();
        let completed_attempt = attempt.clone();
        let started = super::create_recovery_view_owned(&self.services, self.provided.clone(), kind, path.as_deref(), untitled,
            shell_contract::ipc::RecoveryOpenContext::NativeStartup(operation), None, None, Some(constraint),
            attempt.rollback_store(), before, Box::new(move |result| {
                let Some(done) = completed.borrow_mut().take() else { return; };
                match result {
                    Err(error) => {
                        let completion = Box::new(move |result: Result<(), String>| {
                            done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                        });
                        match error.cleanup {
                            shell_ipc_w::tabs::host::ViewCreationCleanupState::Complete => completed_attempt.fail_before_creation(error.message, completion),
                            shell_ipc_w::tabs::host::ViewCreationCleanupState::Acknowledged(receipt) => completed_attempt.fail_after_cleanup(error.message, receipt, reporter.clone(), completion),
                            shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(cleanup) => completed_attempt.fail(error.message, Some(cleanup), reporter.clone(), completion),
                        }
                    }
                    Ok(ready) => {
                        let view = ready.view();
                        let document = match ready.invocation_document() {
                            Ok(document) => document,
                            Err(error) => {
                                let (_, discard, _) = ready.into_parts();
                                return completed_attempt.fail(error, Some(discard), reporter.clone(), Box::new(move |result| {
                                    done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                                }));
                            }
                        };
                        match crate::host::recovery_openers::finish_open(&opener, &context, recovery, view) {
                            Ok(loaded) => {
                                let (owner, discard, closure) = ready.into_parts();
                                done(Ok(shell_ipc_w::tabs::OwnedLoadedTabView { loaded, owner, document, discard, closure, attempt: completed_attempt }));
                            }
                            Err(error) => {
                                let (_, discard, _) = ready.into_parts();
                                completed_attempt.fail(error, Some(discard), reporter.clone(), Box::new(move |result| {
                                    done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                                }));
                            }
                        }
                    }
                }
            }));
        if let Err(error) = started {
            if let Some(done) = callback.borrow_mut().take() {
                attempt.fail_before_creation(error, Box::new(move |result| {
                    done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                }));
            }
        }
    }

    fn editor_is_dirty(&self, kind: TabKind, id: i64) -> TabResult<bool> {
        let view = view_id(id).ok_or_else(|| TabError("invalid editor view id".into()))?;
        let unavailable = || TabError(format!("{} editor unavailable", kind.as_str()));
        match kind {
            TabKind::Sheets => Ok(shell_ipc_d1::sheets::editors::views().ok_or_else(unavailable)?.pending_edit_count(view) > 0.0),
            TabKind::Slides => shell_ipc_d1::slides::views().ok_or_else(unavailable)?.is_dirty(view).map_err(|error| TabError(error.message)),
            TabKind::Pdf => Ok(self.editor::<shell_ipc_d2::pdf::editor::editors::PdfEditors>("PDF").map_err(|error| TabError(error.0))?.is_dirty(view)),
            TabKind::Markdown => Ok(self.editor::<shell_ipc_d2::markdown::MarkdownEditors>("Markdown").map_err(|error| TabError(error.0))?.is_dirty(view)),
            TabKind::Html => Ok(self.editor::<shell_ipc_d2::html::HtmlEditors>("HTML").map_err(|error| TabError(error.0))?.is_dirty(view)),
            TabKind::Mindmap => Ok(shell_ipc_d1::mindmap::handle().ok_or_else(unavailable)?.is_dirty(view)),
            TabKind::Epub => Ok(self.editor::<shell_ipc_d2::epub::EpubEditors>("EPUB").map_err(|error| TabError(error.0))?.is_dirty(view)),
            TabKind::Diagram => Ok(shell_ipc_d1::diagram::views().ok_or_else(unavailable)?.is_dirty(view)),
            TabKind::Hwp => Ok(self.editor::<shell_ipc_d2::hwp::HwpEditors>("HWP").map_err(|error| TabError(error.0))?.is_dirty(view)),
            TabKind::Docs | TabKind::Home | TabKind::Help => Ok(false),
        }
    }

    fn query_docs_dirty(&self, view: ViewId, then: Then<bool>) {
        match self.editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs") {
            Ok(editor) => editor.query_dirty(view, then),
            Err(error) => then(Err(IpcError { name: Some("Error".into()), message: error.0 })),
        }
    }

    fn query_docs_dirty_owned(&self, view: ViewId, owner: shell_contract::ipc::NativeRequestOwner, cancel: shell_contract::runtime::CancelToken, then: Then<bool>) {
        match self.editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs") {
            Ok(editor) => editor.query_dirty_owned(view, owner, cancel, then),
            Err(error) => then(Err(IpcError { name: Some("Error".into()), message: error.0 })),
        }
    }

    fn diagram_identity(&self, id: i64) -> Option<DiagramIdentity> {
        let identity = shell_ipc_d1::diagram::views()?.tab_identity(view_id(id)?)?;
        Some(DiagramIdentity { imported: identity.imported.is_some(), title: identity.title, source_path: identity.source_path })
    }
    fn queue_generate(&self, view: ViewId, handoff: &serde_json::Value) {
        if let Err(error) = crate::host::net::tools::generate::queue_generate_handoff(&self.provided, view, handoff) {
            eprintln!("shell: queueing generated handoff: {error}");
        }
    }

    fn queue_conversion(&self, view: ViewId, doc: &serde_json::Value) {
        if let Err(error) = crate::host::net::tools::generate::queue_converted_content(&self.provided, view, doc) {
            eprintln!("shell: queueing converted document: {error}");
        }
    }
    fn classify_skill_drop(&self, path: &str, then: Box<dyn FnOnce(bool)>) {
        let skills = self.provided.all::<shell_ipc_a::user_skills::SkillsCore>().into_iter().next();
        let token = NEXT_DROP.fetch_add(1, Ordering::Relaxed);
        DROPS.with(|pending| pending.borrow_mut().insert(token, Box::new(move |source| {
            let entry = match source {
                Some(SkillImportSource::Zip { file }) => Some(DroppedSkillEntry::Zip { file }),
                Some(SkillImportSource::SkillFile { file }) => Some(DroppedSkillEntry::SkillFile { file }),
                _ => None,
            };
            if let (Some(skills), Some(entry)) = (skills, entry) {
                skills.queue_dropped(entry);
                then(true);
            } else {
                then(false);
            }
        })));
        let path = path.to_owned();
        let home = self.services.paths.path(PathName::Home).to_string_lossy().into_owned();
        let core = self.services.core.clone();
        self.services.workers.spawn(Box::new(move || {
            let source = dropped_skill_source_of(&path, &home);
            core.post(Box::new(move || {
                if let Some(done) = DROPS.with(|pending| pending.borrow_mut().remove(&token)) {
                    done(source);
                }
            }));
        }));
    }
}

impl shell_ipc_w::tabs::TabHooks for Hooks {
    fn cleanup_lifetime(&self) -> shell_contract::runtime::CancelToken {
        self.cx.cleanup_shutdown.clone()
    }
    fn register_generated_path_task(
        &self, task: std::sync::Arc<dyn shell_contract::hosts::GeneratedPathPending>,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String> {
        self.cx.headless_cleanup.register_generated(task)
    }
    fn capture_generated_retry_prompt(
        &self, shell: shell_ipc_w::tabs::ShellWindow, owner: shell_contract::ipc::NativeRequestOwner,
        output: shell_contract::ipc::SavedPublication, participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
    ) -> Result<std::rc::Rc<dyn shell_ipc_w::tabs::host::CapturedGeneratedRetryPrompt>, String> {
        super::generated_retry::capture(self, shell, owner, output, participants, cancel, pending, additional)
    }
    fn capture_saved_shell(&self, shell: shell_ipc_w::tabs::ShellWindow, owner: shell_contract::ipc::NativeRequestOwner)
        -> Result<std::rc::Rc<dyn shell_ipc_w::tabs::host::CapturedSavedShell>, String> {
        super::saved_prepare::capture(self, shell, owner)
    }
    fn prepare_docs_disposal_owned(
        &self, view: ViewId, origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner, participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        target: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
    ) -> Result<std::rc::Rc<dyn shell_ipc_w::tabs::DocsDisposalOperation>, String> {
        super::disposal::prepare(self, view, origin, destination, participants, target, cancel, pending)
    }
    fn prepare_activation_ui(
        &self,
        shell: shell_ipc_w::tabs::ShellWindow,
        shell_owner: &shell_contract::ipc::NativeRequestOwner,
        target: Option<ViewId>,
        effects: &[main_core::shell::tab_manager::TabActivationEffect<ViewId, shell_ipc_w::tabs::host::RetainedSpareBinding>],
    ) -> Result<shell_ipc_w::tabs::PreparedTabUiBatch, String> {
        super::activation::prepare(self, shell, shell_owner, target, effects)
    }

    fn prepare_activation_publication_owned(
        &self, origin: shell_ipc_w::tabs::TabActivationAuthority, shell: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken, effects: shell_contract::hosts::OwnedEffectsHost,
        snapshot: shell_ipc_w::tabs::TabActivationPublicationSnapshot,
    ) -> Result<shell_ipc_w::tabs::PreparedTabActivationPublication, String> {
        super::activation_publication::prepare(self, origin, shell, participants, cancel, pending, effects, snapshot)
    }

    fn cancel_update_restart(&self) {
        crate::host::update::cancel_update_restart();
    }
    fn tabs_changed(&self, changed: &shell_ipc_w::tabs::TabsChanged) {
        let tabs = changed.tabs.clone();
        let owner = super::NATIVE_WINDOWS.get().and_then(|windows| windows.shell_window()).map(|shell| shell.window);
        crate::host::w::post_core(move |provided, _| {
            crate::host::menu::doc_links::refresh_document_link_menu();
            if let Err(error) = crate::host::recovery_launch::note_live_session() {
                eprintln!("shell: recording live session: {error}");
            }
            if let Some(owner) = owner {
                let ids = tabs.iter().map(|tab| tab.id.clone()).collect::<Vec<_>>();
                crate::host::recovery_openers::retain_bound(owner, &ids);
            }
            let Some(state) = provided.all::<shell_ipc_w::tabs::TabsState>().into_iter().next() else { return };
            let Some(hwp) = provided.all::<shell_ipc_d2::hwp::HwpEditors>().into_iter().next() else { return };
            for tab in tabs.iter().filter(|tab| tab.kind == TabKind::Hwp) {
                if let Some(view) = state.tab_contents(&tab.id) {
                    hwp.set_recovery_title(view, &tab.title);
                }
            }
        });
    }
    fn prepare_saved_publication_owned(
        &self,origin:shell_contract::ipc::NativeRequestOwner,destination:shell_contract::ipc::NativeRequestOwner,
        participants:Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel:shell_contract::runtime::CancelToken,pending:shell_contract::runtime::CancelToken,additional:Vec<shell_contract::runtime::CancelToken>,
        effects:shell_contract::hosts::OwnedEffectsHost,
    )->Result<shell_ipc_w::tabs::SavedPublication,String> {
        super::saved::prepare(self,origin,destination,participants,cancel,pending,additional,effects)
    }
    fn prepare_saved_publication_recovery(
        &self, receipt: shell_contract::ipc::RecoverySeedReceipt, receiver: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_ipc_w::tabs::SavedPublication, String> {
        super::saved::prepare_recovery(self, receipt, receiver, cancel, pending, effects)
    }
    fn prepare_saved_publication_existing_document(
        &self,destination:shell_contract::ipc::NativeSaveDestination,
        participants:Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel:shell_contract::runtime::CancelToken,pending:shell_contract::runtime::CancelToken,additional:Vec<shell_contract::runtime::CancelToken>,
        effects:shell_contract::hosts::OwnedEffectsHost,
    )->Result<shell_ipc_w::tabs::SavedPublication,String> {
        super::saved::prepare_existing_document(self,destination,participants,cancel,pending,additional,effects)
    }
    fn prepare_saved_publication_saved_document(
        &self, document: shell_contract::ipc::NativeSavedDocument, participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>, effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_ipc_w::tabs::SavedPublication, String> {
        super::saved::prepare_saved_document(self, document, participants, cancel, pending, additional, effects)
    }
    fn split_changed(&self, _: Option<&main_core::shell::tab_manager::SplitViewState>) {
        crate::host::w::post_core(|_, _| {
            if let Err(error) = crate::host::recovery_launch::note_live_session() {
                eprintln!("shell: recording live session: {error}");
            }
        });
    }
    fn detached_changed(&self) {
        crate::host::w::post_core(|_, _| {
            if let Err(error) = crate::host::recovery_launch::note_live_session() {
                eprintln!("shell: recording live session: {error}");
            }
        });
    }
    fn create_view(&self, spec: &main_core::shell::tab_manager::ViewSpec<'_>, done: Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<ViewId>)>) {
        self.create_editor_view(spec, done)
    }
    fn create_spare_view_owned(
        &self, startup: shell_contract::ipc::NativeInvocationOperation,
        shell: shell_contract::ipc::NativeRequestOwner, cleanup: shell_contract::runtime::CancelToken,
        rollback: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
        spare_actor: shell_ipc_w::tabs::host::SpareActorSlot,
        done: Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<shell_contract::ipc::NativeInvocationDocument>)>,
    ) {
        use shell_contract::{ipc::NativeEditorCommand, Surface};
        use main_core::shell::tab_manager::ViewSpec;
        let fail = |error: String| shell_ipc_w::tabs::host::ViewCreationFailure {
            error: TabError(error), cleanup: shell_ipc_w::tabs::host::ViewCreationCleanupState::Complete,
        };
        if !cleanup.same_token(&self.cx.cleanup_shutdown) {
            return done(Err(fail("The spare cleanup lifetime does not match the process.".into())));
        }
        if spare_actor.borrow().is_some() {
            return done(Err(fail("The spare actor slot is already occupied".into())));
        }
        let cancel = startup.cancel().clone();
        let command = NativeEditorCommand::NewDocument { surface: Surface::Sheets, directory: None };
        let child = match crate::host::native_commands::issue_child(&startup, command.clone(), cancel.clone()) {
            Ok(child) => child,
            Err(error) => return done(Err(fail(error))),
        };
        let mut sheets = None;
        let mut actors = super::OrdinaryActorStages::default();
        let prepared = crate::ipc::native_owner::with_startup_command_preparation_effect(
            &startup, &shell, &child, &command, &[], &cancel, &cleanup, || {
                let generation = shell.context.renderer().ok_or_else(|| TabError("The captured startup Shell renderer is unavailable".into()))?;
                let window = self.cx.views.view_window_captured(shell.view, generation).map_err(TabError)?;
                self.prepare_editor_view_impl(&ViewSpec::Sheets { opening_workbook: None }, false,
                    |_| Ok(()), None, Some(&mut sheets), false, Some(window), &mut actors)
            },
        );
        let mut prepared = match prepared {
            Some(Ok(prepared)) => prepared,
            Some(Err(error)) => return done(Err(fail(error.0))),
            None => return done(Err(fail("The startup spare creation authority expired".into()))),
        };
        let view = prepared.view;
        let created = prepared.generation.clone();
        let browser_owner = match prepared.cx.views.prepare_browser_owner(view, &created, prepared.proof.owner()) {
            Ok(owner) => owner,
            Err(error) => return fail_prepared_view(prepared, TabError(error), Box::new(move |failure| done(Err(failure)))),
        };
        let target = match child.reserve_target(view, created.clone()) {
            Ok(target) => target,
            Err(error) => return fail_prepared_view(prepared, TabError(error), Box::new(move |failure| done(Err(failure)))),
        };
        let registered = crate::ipc::native_owner::with_startup_command_creation_effect(
            &startup, &shell, &child, &command, &created, &[], &cancel, &cleanup, || {
                let token = sheets.as_mut().ok_or("The spare Sheets registration is unavailable")?;
                prepared.proof.reservation().with_initial_owner(prepared.proof.owner(), |transition| {
                    let browser = browser_owner.validate(transition)?;
                    let actor = token.validate_in_transition(transition)?;
                    actor.commit();
                    browser.commit();
                    Ok(())
                })
            },
        ).ok_or_else(|| "The startup spare registration authority expired".to_owned()).and_then(|result| result);
        if let Err(error) = registered {
            return fail_prepared_view(prepared, TabError(error), Box::new(move |failure| done(Err(failure))));
        }
        let receipt = match sheets.take().ok_or_else(|| "The spare Sheets registration is unavailable".to_owned()).and_then(|token| token.finish()) {
            Ok(receipt) => receipt,
            Err(error) => return fail_prepared_view(prepared, TabError(error), Box::new(move |failure| done(Err(failure)))),
        };
        let actor = super::spare_actor::SheetsSpareActor::new(receipt);
        rollback.borrow_mut().push(actor.clone());
        *spare_actor.borrow_mut() = Some(actor);
        let cx = prepared.cx.clone();
        let core = prepared.core.clone();
        let captured_webview = std::sync::Arc::new(std::sync::Mutex::new(None::<tauri::Webview>));
        let installed = captured_webview.clone();
        let finish_root = startup.clone();
        let finish_shell = shell.clone();
        let finish_child = child.clone();
        let finish_command = command.clone();
        let finish_cancel = cancel.clone();
        let finish_cleanup = cleanup.clone();
        let finish_core = core.clone();
        super::native_command::start(cx, core, target, command, Vec::new(), cancel, cleanup,
            Some((startup, shell)), Box::new(move |frame| {
                prepared.invocation_on_frame = Some(frame);
                let child = prepared.install()?;
                *installed.lock().unwrap_or_else(|error| error.into_inner()) = Some(child.webview.clone());
                Ok(child)
            }), Box::new(move |result| {
                let ready = match result { Ok(ready) => ready, Err(error) => return done(Err(error)) };
                let webview = captured_webview.lock().unwrap_or_else(|error| error.into_inner()).take();
                let Some(webview) = webview else {
                    return done(Err(shell_ipc_w::tabs::host::ViewCreationFailure {
                        error: TabError("The created spare native view is unavailable".into()),
                        cleanup: shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(ready.cleanup),
                    }));
                };
                let gate_root = finish_root.clone();
                let gate_shell = finish_shell.clone();
                let gate_child = finish_child.clone();
                let gate_command = finish_command.clone();
                let gate_cancel = finish_cancel.clone();
                let gate_cleanup = finish_cleanup.clone();
                let gate_document = ready.view.clone();
                let reply = shell_ipc_w::home_files::back_to_core(finish_core, move |result: Result<(), String>| {
                    let admitted = crate::ipc::native_owner::with_startup_command_target_effect(
                        &finish_root, &finish_shell, &finish_child, &finish_command, &ready.view, ready.view.owner(),
                        &[], &finish_cancel, &finish_cleanup, || (),
                    ).is_some();
                    match result {
                        Ok(()) if admitted => done(Ok(ready)),
                        other => done(Err(shell_ipc_w::tabs::host::ViewCreationFailure {
                            error: TabError(other.err().unwrap_or_else(|| "The spare authority expired after hiding".into())),
                            cleanup: shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(ready.cleanup),
                        })),
                    }
                });
                let reply = std::sync::Arc::new(std::sync::Mutex::new(Some(reply)));
                let completed = reply.clone();
                let dispatch = crate::ipc::native_owner::with_startup_command_target_effect(
                    &gate_root, &gate_shell, &gate_child, &gate_command, &gate_document, gate_document.owner(),
                    &[], &gate_cancel, &gate_cleanup, || move || webview.set_native_visible_checked(false, move |result| {
                        let callback = completed.lock().unwrap_or_else(|error| error.into_inner()).take();
                        if let Some(reply) = callback { reply(result); }
                    }),
                );
                let result = match dispatch {
                    Some(action) => action().map_err(|error| error.to_string()),
                    None => Err("The spare authority expired before hiding".into()),
                };
                if let Err(error) = result {
                    let callback = reply.lock().unwrap_or_else(|error| error.into_inner()).take();
                    if let Some(reply) = callback { reply(Err(error)); }
                }
            }));
    }
    fn prepare_view_native_command(
        &self, spec: &main_core::shell::tab_manager::ViewSpec<'_>, operation: shell_contract::ipc::NativeInvocationOperation,
        rollback: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
        initialize: shell_ipc_w::tabs::host::EditorInitializer,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
    ) -> TabResult<(shell_contract::runtime::RendererGeneration, Box<dyn FnOnce(
        Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<shell_contract::ipc::NativeInvocationDocument>)>,
    )>)> {
        use shell_contract::ipc::NativeInvocationSelection;
        let command = match operation.selection() {
            NativeInvocationSelection::NativeCommand(command) if operation.is_native_command() && super::native_command::matches_spec(spec, command) => command.clone(),
            _ => return Err(TabError("The native command does not match the editor view".into())),
        };
        let mut docs = None;
        let mut sheets = None;
        let mut actors = super::OrdinaryActorStages::default();
        let mut prepare = crate::ipc::native_owner::with_native_command_preparation_effect(
            &operation, &command, &participants, &cancel, &pending,
            || || self.prepare_editor_view_impl(spec, false, |_| Ok(()), Some(&mut docs), Some(&mut sheets), false, None, &mut actors),
        ).ok_or_else(|| TabError("The native command expired before preparation".into()))?;
        let mut prepared = prepare()?;
        drop(prepare);
        let view = prepared.view;
        let created = prepared.generation.clone();
        let target = match operation.reserve_target(view, created.clone()) {
            Ok(target) => target,
            Err(error) => {
                prepared.cx.views.remove_view_if_generation(view, &created);
                return Err(TabError(error));
            }
        };
        let cx = prepared.cx.clone();
        let core = prepared.core.clone();
        let mut unstarted = super::native_command::Unstarted(Some((cx.clone(), view, created.clone())));
        let browser_owner = prepared.cx.views.prepare_browser_owner(view, &created, prepared.proof.owner()).map_err(TabError)?;
        let constructor = Box::new(move |done| {
            let registered = (|| -> Result<(), String> {
                crate::ipc::native_owner::with_native_command_creation_effect(
                    &operation, &command, &prepared.generation, &participants, &cancel, &pending,
                    || -> Result<(), String> {
                        prepared.proof.reservation().with_initial_owner(prepared.proof.owner(), |transition| {
                            let mut browser = Some(browser_owner.validate(transition)?);
                            let mut docs = docs.as_mut().map(|token| token.validate_in_transition(transition, None)).transpose()?;
                            let mut sheets = sheets.as_mut().map(|token| token.validate_in_transition(transition)).transpose()?;
                            actors.with_validated(transition, &mut || {
                                let browser = browser.take().ok_or("The native browser guard was consumed")?;
                                if let Some(actor) = docs.take() { actor.commit(); }
                                if let Some(actor) = sheets.take() { actor.commit(); }
                                browser.commit();
                                Ok(())
                            })
                        })
                    },
                ).ok_or("The native command expired before actor registration")??;
                if let Some(token) = docs {
                    let receipt = token.finish()?;
                    let lease = receipt.lease();
                    rollback.borrow_mut().push(std::rc::Rc::new(DocsRegistrationRollback(receipt)));
                    prepared.cx.views.set_docs_media_lease(view, lease.clone())?;
                    let session = crate::host::document_assets::register_session(view, crate::host::document_assets::AssetKind::Docs)?;
                    prepared.cx.views.set_docs_asset_session(view, session)?;
                    prepared.docs_lease = Some(lease);
                }
                if let Some(token) = sheets {
                    rollback.borrow_mut().push(std::rc::Rc::new(SheetsRegistrationRollback(token.finish()?)));
                }
                initialize(view, &prepared.generation, prepared.proof.reservation(), prepared.proof.owner()).map_err(|error| error.0)?;
                Ok(())
            })();
            if let Err(error) = registered {
                unstarted.0.take();
                return fail_prepared_view(prepared, TabError(error), Box::new(move |failure| done(Err(failure))));
            }
            unstarted.0.take();
            super::native_command::start(cx, core, target, command, participants, cancel, pending, None,
                Box::new(move |frame| {
                    prepared.invocation_on_frame = Some(frame);
                    prepared.install()
                }), done);
        });
        Ok((created, constructor))
    }
    fn docs_query_dirty_owned(&self, view: ViewId, owner: shell_contract::ipc::NativeRequestOwner, cancel: shell_contract::runtime::CancelToken, then: Then<bool>) {
        self.query_docs_dirty_owned(view, owner, cancel, then);
    }
    fn docs_query_dirty_captured_many(
        &self, view: ViewId, origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>, destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken, then: Then<bool>,
    ) {
        match self.editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs") {
            Ok(editor) => editor.query_dirty_captured_many(view, origin, participants, destination, cancel, pending, then),
            Err(error) => then(Err(IpcError { name: Some("Error".into()), message: error.0 })),
        }
    }
    fn docs_request_close_captured_many(
        &self, view: ViewId, parent: Option<shell_contract::WindowId>, origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>, destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken, then: Then<bool>,
    ) {
        match self.editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs") {
            Ok(editor) => editor.request_close_captured_many(view, parent, origin, participants, destination, cancel, pending, then),
            Err(error) => then(Err(IpcError { name: Some("Error".into()), message: error.0 })),
        }
    }
    fn with_request_owner(&self, owner: &shell_contract::ipc::NativeRequestOwner, cancel: &shell_contract::runtime::CancelToken, effect: &mut dyn FnMut() -> Result<(), IpcError>) -> Result<(), IpcError> {
        crate::ipc::native_owner::with_owner_effect(owner, cancel, effect).ok_or_else(|| IpcError {
            name: Some("Error".into()), message: "Renderer document is no longer available.".into(),
        })?
    }
    fn prepare_view_owned(
        &self, spec: &main_core::shell::tab_manager::ViewSpec<'_>,
        rollback: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
        initialize: shell_ipc_w::tabs::host::EditorInitializer,
    ) -> TabResult<(shell_contract::runtime::RendererGeneration, Box<dyn FnOnce(
        shell_contract::ipc::NativeRequestOwner, shell_contract::runtime::CancelToken, shell_contract::runtime::CancelToken,
        Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<shell_contract::ipc::NativeRequestOwner>)>,
    )>)> {
        let mut docs = None;
        let mut sheets = None;
        let mut actors = super::OrdinaryActorStages::default();
        let mut prepared = self.prepare_editor_view_impl(spec, false, |_| Ok(()), Some(&mut docs), Some(&mut sheets), false, None, &mut actors)?;
        let created = prepared.generation.clone();
        let mut unstarted = super::native_command::Unstarted(Some((prepared.cx.clone(), prepared.view, created.clone())));
        let browser_owner = prepared.cx.views.prepare_browser_owner(prepared.view, &created, prepared.proof.owner()).map_err(TabError)?;
        Ok((created, Box::new(move |owner, cancel, pending, then| {
            let registered = (|| -> Result<(), String> {
                crate::ipc::native_owner::with_created_effect(&owner, &prepared.generation, &cancel, &pending, ||
                    prepared.proof.reservation().with_initial_owner(prepared.proof.owner(), |transition| {
                        let mut browser = Some(browser_owner.validate(transition)?);
                        let mut docs = docs.as_mut().map(|token| token.validate_in_transition(transition, None)).transpose()?;
                        let mut sheets = sheets.as_mut().map(|token| token.validate_in_transition(transition)).transpose()?;
                        actors.with_validated(transition, &mut || {
                            let browser = browser.take().ok_or("The native browser guard was consumed")?;
                            if let Some(actor) = docs.take() { actor.commit(); }
                            if let Some(actor) = sheets.take() { actor.commit(); }
                            browser.commit();
                            Ok(())
                        })
                    })).ok_or("The editor creation authority expired")??;
                if let Some(docs) = docs {
                    let receipt = docs.finish()?;
                    let lease = receipt.lease();
                    rollback.borrow_mut().push(std::rc::Rc::new(DocsRegistrationRollback(receipt)));
                    prepared.cx.views.set_docs_media_lease(prepared.view, lease.clone())?;
                    let session = crate::host::document_assets::register_session(prepared.view, crate::host::document_assets::AssetKind::Docs)?;
                    prepared.cx.views.set_docs_asset_session(prepared.view, session)?;
                    prepared.docs_lease = Some(lease);
                }
                if let Some(sheets) = sheets {
                    let receipt = sheets.finish()?;
                    rollback.borrow_mut().push(std::rc::Rc::new(SheetsRegistrationRollback(receipt)));
                }
                Ok(())
            })();
            if let Err(error) = registered {
                unstarted.0.take();
                return fail_prepared_view(prepared, TabError(error), Box::new(move |failure| then(Err(failure))));
            }
            unstarted.0.take();
            super::owned_creation::start(prepared, owner, cancel, pending, initialize, then)
        })))
    }
    fn prepare_generated_view_owned(
        &self, spec: &main_core::shell::tab_manager::ViewSpec<'_>, source: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken, seed: Option<main_core::shell::tab_manager::EditorSeed>,
        placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        rollback: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    ) -> TabResult<shell_ipc_w::tabs::host::PreparedGeneratedView> {
        use shell_ipc_w::tabs::host::{NativeBrowserBinding, NativeBrowserRegistration, PreparedGeneratedView};
        let mut docs = None;
        let mut sheets = None;
        let mut actors = super::OrdinaryActorStages::default();
        let mut prepared = self.prepare_editor_view_impl(spec, true, |_| Ok(()), Some(&mut docs), Some(&mut sheets), false, None, &mut actors)?;
        let view = prepared.view;
        let created = prepared.generation.clone();
        let mut unstarted = super::native_command::Unstarted(Some((prepared.cx.clone(), view, created.clone())));
        rollback.borrow_mut().try_reserve(4).map_err(|error| TabError(error.to_string()))?;
        if let Some(token) = docs.as_mut() {
            let receipt = token.prepare_fresh_receipt().map_err(TabError)?;
            let lease = token.lease();
            prepared.cx.views.set_docs_media_lease(view, lease.clone()).map_err(TabError)?;
            let session = crate::host::document_assets::register_session(view, crate::host::document_assets::AssetKind::Docs).map_err(TabError)?;
            prepared.cx.views.set_docs_asset_session(view, session).map_err(TabError)?;
            prepared.docs_lease = Some(lease);
            rollback.borrow_mut().push(std::rc::Rc::new(super::saved_prepare::SavedActorReceipt(receipt)));
        }
        let spare_actor = sheets.as_ref().map(|token| super::spare_actor::SheetsSpareActor::new(token.prepare_receipt()));
        if let Some(actor) = &spare_actor { rollback.borrow_mut().push(actor.clone()); }
        let proof = prepared.proof.clone();
        let browser = proof.owner().clone();
        let reservation = proof.reservation().clone();
        let owner_slot = prepared.cx.views.prepare_browser_owner(view, &created, &browser).map_err(TabError)?;
        let mut stage = self.prepare_generated_seed_stage(view, seed, placement, &source, &participants, &cancel, &pending,
            &created, &reservation, &browser, &rollback)?;
        let watched = proof.clone();
        let closure: main_core::shell::tab_manager::ViewClosureWatch<shell_contract::ipc::NativeBrowserRetirement> =
            std::sync::Arc::new(move |done| watched.watch(Box::new(move |receipt| done(shell_contract::ipc::NativeBrowserRetirement::Closed(receipt)))));
        let binding = NativeBrowserBinding { reservation: reservation.clone(), owner: browser.clone(),
            producer: std::sync::Arc::new(proof), closure, registration: NativeBrowserRegistration::Unregistered };
        let barrier = std::sync::Arc::new(super::accepted::GeneratedBarrierSlot::default());
        let installer = barrier.installer();
        let result_created = created.clone();
        let result_binding = binding.clone();
        let start_binding = binding.clone();
        let start = Box::new(move |adopt: shell_ipc_w::tabs::host::GeneratedManagerAdoption,
            register: shell_ipc_w::tabs::host::GeneratedManagerRegistration,
            cleanup: shell_ipc_w::tabs::host::ViewCreationCleanup,
            done: Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<shell_ipc_w::tabs::host::GeneratedViewCommit>)>| {
            let cleanup = super::accepted::cleanup_with_barrier(
                barrier.clone(), cleanup, prepared.cx.clone(), view, created.clone(),
            );
            if !barrier.is_installed() {
                unstarted.0.take();
                return done(Err(shell_ipc_w::tabs::host::ViewCreationFailure {
                    error: TabError("The generated preclose barrier is unavailable".into()),
                    cleanup: shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(cleanup),
                }));
            }
            let admitted = crate::ipc::native_owner::with_created_many_effect(&source, &participants, &created, &cancel, &pending, || {
                reservation.with_initial_owner(&browser, |transition| {
                    let owner_slot = owner_slot.validate(transition)?;
                    let mut adopt = Some(adopt);
                    let mut actor = |next: &mut dyn FnMut() -> Result<(), String>| {
                        actors.with_validated(transition, &mut || {
                            let mut with_sheets = || {
                                let mut sheets_valid = sheets.as_mut().map(|token| token.validate_in_transition(transition)).transpose()?;
                                stage.with_validated(transition, &mut || {
                                    next()?;
                                    if let Some(valid) = sheets_valid.take() { valid.commit(); }
                                    Ok(())
                                })
                            };
                            match docs.as_mut() {
                                Some(token) => token.with_saved_validated(transition, &mut || with_sheets().map_err(|message|
                                    shell_contract::IpcError { name: Some("Error".into()), message })).map_err(|error| error.message),
                                None => with_sheets(),
                            }
                        })
                    };
                    adopt.take().ok_or("The generated manager adoption was already consumed")?(transition, &browser, &mut actor)?;
                    owner_slot.commit();
                    Ok(())
                })
            }).unwrap_or_else(|| Err("The generated editor authority expired before adoption".into()));
            if let Err(error) = admitted {
                unstarted.0.take();
                return done(Err(shell_ipc_w::tabs::host::ViewCreationFailure {
                    error: TabError(error), cleanup: shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(cleanup),
                }));
            }
            unstarted.0.take();
            super::accepted::schedule_generated(prepared, source, participants, cancel, pending, start_binding, register, cleanup, done);
        });
        Ok(PreparedGeneratedView { view, created: result_created, binding: result_binding,
            spare_actor: spare_actor.map(|actor| actor as std::rc::Rc<dyn shell_ipc_w::tabs::host::SpareActor>), installer, start })
    }
    fn prepare_initialized_spare_owned(
        &self, view: ViewId, expected: &shell_contract::runtime::RendererGeneration,
        binding: shell_ipc_w::tabs::host::RetainedSpareBinding,
        source: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        rollback: shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    ) -> TabResult<shell_ipc_w::tabs::host::PreparedGeneratedView> {
        use shell_ipc_w::tabs::host::{NativeBrowserRegistration, PreparedGeneratedView, ViewCreationCleanupState, ViewCreationFailure, ViewCreationReady};
        let proof = self.cx.views.closure_proof(view, expected)
            .ok_or_else(|| TabError("The original spare closure reservation is unavailable".into()))?;
        if !binding.browser.reservation.matches(view, expected)
            || !proof.reservation().same_reservation(&binding.browser.reservation) {
            return Err(TabError("The spare browser reservation changed".into()));
        }
        let target = match &binding.browser.registration {
            NativeBrowserRegistration::Registered(owner) if owner.view == view
                && owner.context.renderer().is_some_and(|generation| generation.same_generation(expected)) => owner.clone(),
            _ => return Err(TabError("The spare document registration changed".into())),
        };
        let origin = target.context.authority_ref::<crate::ipc::native_owner::NativeOwnerAuthority>()
            .ok_or_else(|| TabError("The spare document has no native authority".into()))?.origin.clone();
        let captured = crate::ipc::native_owner::capture_owner(&self.cx.views, view, expected).map_err(TabError)?;
        if captured.context.authority_ref::<crate::ipc::native_owner::NativeOwnerAuthority>()
            .is_none_or(|authority| authority.origin != origin) {
            return Err(TabError("The spare native document was replaced".into()));
        }
        let actor = binding.spare_actor.as_ref().ok_or_else(|| TabError("The spare actor is unavailable".into()))?;
        let mut transfer = actor.prepare_transfer().map_err(TabError)?;
        let successor_actor = transfer.successor();
        let successor_receipt = transfer.successor_receipt();
        let replacement = binding.browser.reservation.prepare_replacement(&binding.browser.owner).map_err(TabError)?;
        let successor = replacement.successor().clone();
        let owner_slot = self.cx.views.prepare_browser_owner(view, expected, &successor).map_err(TabError)?;
        let mut proposed = binding.browser.clone();
        proposed.owner = successor.clone();
        if !participants.iter().any(|participant| participant.view == view
            && participant.loss_token().zip(target.loss_token()).is_some_and(|(left, right)| left.same_token(&right))) {
            participants.push(target.clone());
        }
        let mut stage = self.prepare_generated_seed_stage(view, seed, placement, &source, &participants,
            &cancel, &pending, expected, &proposed.reservation, &successor, &rollback)?;
        rollback.borrow_mut().try_reserve(2).map_err(|error| TabError(error.to_string()))?;
        let prior = std::rc::Rc::new(RefCell::new(binding.rollback.borrow().clone()));
        let barrier = std::sync::Arc::new(super::accepted::GeneratedBarrierSlot::default());
        let installer = barrier.installer();
        let created = expected.clone();
        let generation = created.clone();
        let mut ready_binding = proposed.clone();
        let cx = self.cx.clone();
        let start = Box::new(move |adopt: shell_ipc_w::tabs::host::GeneratedManagerAdoption,
            register: shell_ipc_w::tabs::host::GeneratedManagerRegistration,
            cleanup: shell_ipc_w::tabs::host::ViewCreationCleanup,
            done: Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<shell_ipc_w::tabs::host::GeneratedViewCommit>)>| {
            let cleanup = super::accepted::cleanup_with_barrier(
                barrier.clone(), cleanup, cx.clone(), view, generation.clone(),
            );
            if !barrier.is_installed() {
                return done(Err(ViewCreationFailure {
                    error: TabError("The generated preclose barrier is unavailable".into()),
                    cleanup: ViewCreationCleanupState::Complete,
                }));
            }
            let admitted = crate::ipc::native_owner::with_created_many_effect(
                &source, &participants, &generation, &cancel, &pending, || {
                    replacement.commit(|transition, next_owner| {
                        let owner_slot = owner_slot.validate(transition)?;
                        let mut adopt = Some(adopt);
                        let mut actor = |next: &mut dyn FnMut() -> Result<(), String>| {
                            transfer.with_validated(transition, next_owner, &mut || stage.with_validated(transition, next))
                        };
                        adopt.take().ok_or("The spare manager adoption was already consumed")?(transition, next_owner, &mut actor)?;
                        owner_slot.commit();
                        Ok(())
                    })
                },
            ).unwrap_or_else(|| Err("The spare editor authority expired before adoption".into()));
            let (_, proof, ()) = match admitted {
                Ok(admitted) => admitted,
                Err(error) => return done(Err(ViewCreationFailure { error: TabError(error), cleanup: ViewCreationCleanupState::Complete })),
            };
            let prior = std::rc::Rc::new(SupersededDrain {
                receipts: prior, proof, running: std::rc::Rc::new(std::cell::Cell::new(false)),
            });
            {
                let mut receipts = rollback.borrow_mut();
                receipts.push(prior.clone());
                receipts.push(successor_receipt);
            }
            let target = match crate::ipc::native_owner::capture_owner(&cx.views, view, &generation) {
                Ok(target) if target.context.authority_ref::<crate::ipc::native_owner::NativeOwnerAuthority>()
                    .is_some_and(|authority| authority.origin == origin) => target,
                Ok(_) => return done(Err(ViewCreationFailure {
                    error: TabError("The spare native document was replaced after adoption".into()),
                    cleanup: ViewCreationCleanupState::Pending(cleanup),
                })),
                Err(error) => return done(Err(ViewCreationFailure {
                    error: TabError(error), cleanup: ViewCreationCleanupState::Pending(cleanup),
                })),
            };
            participants.push(target.clone());
            let mut published = false;
            let registered = crate::ipc::native_owner::with_created_many_effect(
                &source, &participants, &generation, &cancel, &pending, || {
                    ready_binding.reservation.with_owner(&ready_binding.owner, |transition| {
                        register(&target, transition, &mut || {
                            if std::mem::replace(&mut published, true) {
                                return Err("The spare document registration was repeated".into());
                            }
                            Ok(())
                        })
                    })
                },
            ).unwrap_or_else(|| Err("The spare editor registration authority expired".into()))
                .and_then(|()| if published { Ok(()) } else { Err("The spare document registration was not published".into()) });
            if let Err(error) = registered {
                return done(Err(ViewCreationFailure { error: TabError(error), cleanup: ViewCreationCleanupState::Pending(cleanup) }));
            }
            ready_binding.registration = NativeBrowserRegistration::Registered(target.clone());
            prior.begin(Box::new(move |retired| {
                if let Err(error) = retired {
                    return done(Err(ViewCreationFailure { error: TabError(error), cleanup: ViewCreationCleanupState::Pending(cleanup) }));
                }
                match shell_ipc_w::tabs::host::GeneratedViewCommit::loaded(target, generation) {
                    Ok(commit) => done(Ok(ViewCreationReady {
                        view: commit, cleanup, closure: ready_binding.closure.clone(), reservation: ready_binding,
                    })),
                    Err(error) => done(Err(ViewCreationFailure { error, cleanup: ViewCreationCleanupState::Pending(cleanup) })),
                }
            }));
        });
        Ok(PreparedGeneratedView { view, created, binding: proposed, spare_actor: Some(successor_actor), installer, start })
    }
    fn commit_generated_seed_owned(
        &self, view: ViewId, seed: Option<main_core::shell::tab_manager::EditorSeed>,
        placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        source: &shell_contract::ipc::NativeRequestOwner, participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
        created: &shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        browser: &shell_contract::ipc::NativeBrowserOwner,
        rollback: &shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    ) -> TabResult<()> {
        let effects = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned()
            .ok_or_else(|| TabError("Owned initialization effects are unavailable".into()))?;
        let mut planned = match &seed {
            Some(main_core::shell::tab_manager::EditorSeed::Generate(value)) =>
                Some(crate::host::net::tools::generate::prepare_generate_handoff_owned(
                    &self.provided, view, value, source, created, reservation, browser, effects.clone()).map_err(TabError)?),
            Some(main_core::shell::tab_manager::EditorSeed::Conversion(value)) =>
                Some(crate::host::net::tools::generate::prepare_converted_content_owned(
                    &self.provided, view, value, source, cancel, created, reservation, browser, effects.clone()).map_err(TabError)?),
            _ => None,
        };
        let mut docs_plan = match &seed {
            Some(main_core::shell::tab_manager::EditorSeed::DocsAi { new_blank, content }) =>
                match crate::host::d2::docs_tabs::prepare_docs_ai_seed_owned(
                    &self.provided, view, *new_blank, content.clone(), source, participants,
                    cancel, pending, created, reservation, browser,
                ) {
                    Ok(plan) => Some(plan),
                    Err(error) => return retain_seed_result(docs_ai_result(Err(error)), rollback),
                },
            _ => None,
        };
        rollback.borrow_mut().try_reserve(2).map_err(|error| TabError(error.to_string()))?;
        let placer = if placement.is_some() {
            Some(self.provided.all::<shell_ipc_w::projects::Projects>().into_iter().next()
                .ok_or_else(|| TabError("Project services are unavailable".into()))?.new_documents())
        } else { None };
        let mut prepared_placement = match (placement, &placer) {
            (Some(placement), Some(placer)) => Some(placer.borrow_mut()
                .prepare_preset(i64::from(view.0), Ok(placement)).map_err(TabError)?),
            (None, _) => None,
            _ => return Err(TabError("The project placement service disappeared".into())),
        };
        let mut placement_receipt = None;
        let result: SeedResult = crate::ipc::native_owner::with_created_many_effect(
            source, participants, created, cancel, pending, || {
                reservation.with_owner(browser, |transition| {
                    let mut placer_guard = placer.as_ref().map(|placer| placer.borrow_mut());
                    let mut validated = match prepared_placement.take() {
                        Some(prepared) => Some(placer_guard.as_deref_mut().ok_or("The project placement service disappeared")?
                            .validate_preset(prepared)?),
                        None => None,
                    };
                    let mut initialized = false;
                    let mut initialize = || {
                        if std::mem::replace(&mut initialized, true) { return Err("The project initializer was repeated".into()); }
                        if let Some(validated) = validated.take() { placement_receipt = Some(validated.commit()); }
                        Ok(())
                    };
                    let result = match seed {
                        Some(main_core::shell::tab_manager::EditorSeed::Generate(_))
                        | Some(main_core::shell::tab_manager::EditorSeed::Conversion(_)) =>
                            planned.take().ok_or("The generated seed was already consumed")?
                                .commit(transition, &mut initialize).map(Some).map_err(|error| (error, None)),
                        Some(main_core::shell::tab_manager::EditorSeed::DocsAi { .. }) =>
                            docs_ai_result(docs_plan.take().ok_or("The Docs AI seed was already consumed")?
                                .commit(transition, &mut initialize)),
                        None => initialize().map(|()| None).map_err(|error| (error, None)),
                    };
                    Ok(result)
                })
            },
        ).unwrap_or_else(|| Err("The initialized tab lost a captured owner".into()))
            .unwrap_or_else(|error| Err((error, None)));
        if let Some(receipt) = placement_receipt {
            let placer = placer.ok_or_else(|| TabError("The project placement service disappeared".into()))?;
            rollback.borrow_mut().push(std::rc::Rc::new(PlacementRollback { placer, receipt: RefCell::new(Some(receipt)), view, created: created.clone(), reservation: reservation.clone(), owner: browser.clone(), cancelled: std::cell::Cell::new(false) }));
        }
        retain_seed_result(result, rollback)
    }
    fn prepare_spare_editor_seed(
        &self, view: ViewId, seed: Option<main_core::shell::tab_manager::EditorSeed>,
        docs_open_path: Option<String>, workbook: Option<String>, new_blank: bool,
        target: &shell_contract::ipc::NativeRequestOwner,
        binding: &shell_ipc_w::tabs::host::RetainedSpareBinding,
        rollback: &shell_ipc_w::tabs::host::EditorSeedRollbackStore,
    ) -> TabResult<Box<dyn shell_ipc_w::tabs::host::SpareSeedPlan>> {
        super::spare_seed::prepare(self, view, seed, docs_open_path, workbook, new_blank, target, binding, rollback).map_err(TabError)
    }
    fn commit_editor_seed_owned(
        &self, view: ViewId, seed: Option<main_core::shell::tab_manager::EditorSeed>,
        docs_open_path: Option<String>, workbook: Option<String>,
        reused_target: Option<&shell_contract::ipc::NativeRequestOwner>,
        source: &shell_contract::ipc::NativeRequestOwner,
        rollback: &shell_ipc_w::tabs::host::EditorSeedRollbackStore,
        created: &shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        browser: &shell_contract::ipc::NativeBrowserOwner,
        cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
    ) -> TabResult<()> {
        if reused_target.is_some_and(|owner| owner.view != view
            || owner.context.renderer().is_none_or(|generation| !generation.same_generation(created))) {
            return Err(TabError("The captured spare editor was replaced".into()));
        }
        let participants = reused_target.map(std::slice::from_ref).unwrap_or(&[]);
        let effects = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned()
            .ok_or_else(|| TabError("Owned initialization effects are unavailable".into()))?;
        let mut planned = match &seed {
            Some(main_core::shell::tab_manager::EditorSeed::Generate(value)) =>
                Some(crate::host::net::tools::generate::prepare_generate_handoff_owned(
                    &self.provided, view, value, source, created, reservation, browser, effects.clone()).map_err(TabError)?),
            Some(main_core::shell::tab_manager::EditorSeed::Conversion(value)) =>
                Some(crate::host::net::tools::generate::prepare_converted_content_owned(
                    &self.provided, view, value, source, cancel, created, reservation, browser, effects.clone()).map_err(TabError)?),
            _ => None,
        };
        let mut docs_plan = match &seed {
            Some(main_core::shell::tab_manager::EditorSeed::DocsAi { new_blank, content }) =>
                match crate::host::d2::docs_tabs::prepare_docs_ai_seed_owned(
                    &self.provided, view, *new_blank, content.clone(), source, participants,
                    cancel, pending, created, reservation, browser,
                ) {
                    Ok(plan) => Some(plan),
                    Err(error) => return retain_seed_result(docs_ai_result(Err(error)), rollback),
                },
            _ => None,
        };
        rollback.borrow_mut().try_reserve(3).map_err(|error| TabError(error.to_string()))?;
        let mut docs = match docs_open_path.filter(|path| !path.is_empty()) {
            Some(path) => Some(self.editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs")?
                .prepare_queue_open(view, path, created.clone(), reservation).map_err(TabError)?),
            None => None,
        };
        let mut workbook = match workbook.filter(|path| !path.is_empty()) {
            Some(path) => Some(shell_ipc_d1::sheets::editors::views()
                .ok_or_else(|| TabError("Sheets editor is not registered".into()))?
                .prepare_workbook_queue(view, path, created.clone(), reservation, browser).map_err(TabError)?),
            None => None,
        };
        let nudge = workbook.is_some() && reused_target.is_some();
        let mut docs_committed = false;
        let mut workbook_receipt = None;
        let result: SeedResult = crate::ipc::native_owner::with_created_many_effect(
            source, participants, created, cancel, pending, || {
                reservation.with_owner(browser, |transition| {
                    let mut initialized = false;
                    let mut initialize = || {
                        if std::mem::replace(&mut initialized, true) { return Err("The editor initializer was repeated".into()); }
                        let docs_valid = docs.as_mut().map(|plan| plan.validate_in_transition(transition, browser)).transpose()?;
                        let workbook_valid = workbook.take().map(|plan| plan.validate_in_transition(transition, browser)).transpose()?;
                        if let Some(valid) = docs_valid { valid.commit(); docs_committed = true; }
                        if let Some(valid) = workbook_valid { workbook_receipt = Some(valid.commit_in_transition()); }
                        Ok(())
                    };
                    let result = match seed {
                        Some(main_core::shell::tab_manager::EditorSeed::Generate(_))
                        | Some(main_core::shell::tab_manager::EditorSeed::Conversion(_)) =>
                            planned.take().ok_or("The editor seed was already consumed")?
                                .commit(transition, &mut initialize).map(Some).map_err(|error| (error, None)),
                        Some(main_core::shell::tab_manager::EditorSeed::DocsAi { .. }) =>
                            docs_ai_result(docs_plan.take().ok_or("The Docs AI seed was already consumed")?
                                .commit(transition, &mut initialize)),
                        None => initialize().map(|()| None).map_err(|error| (error, None)),
                    };
                    Ok(result)
                })
            },
        ).unwrap_or_else(|| Err("The initialized editor lost a captured owner".into()))
            .unwrap_or_else(|error| Err((error, None)));
        if docs_committed {
            let finished = docs.take().ok_or_else(|| "The committed Docs initializer is unavailable".to_owned())
                .and_then(|prepared| prepared.finish());
            match finished {
                Ok(receipt) => rollback.borrow_mut().push(std::rc::Rc::new(DocsOpenRollback(receipt))),
                Err(error) => {
                    if let Some(receipt) = workbook_receipt { rollback.borrow_mut().push(std::rc::Rc::new(WorkbookQueueRollback(receipt))); }
                    match result {
                        Ok(Some(receipt)) | Err((_, Some(receipt))) => rollback.borrow_mut().push(receipt),
                        _ => {}
                    }
                    return Err(TabError(error));
                }
            }
        }
        if let Some(receipt) = workbook_receipt {
            rollback.borrow_mut().push(std::rc::Rc::new(WorkbookQueueRollback(receipt)));
        }
        retain_seed_result(result, rollback)?;
        if nudge {
            let target = reused_target.ok_or_else(|| TabError("The captured spare editor is unavailable".into()))?;
            effects.emit_owned_many(source, std::slice::from_ref(target), 0, cancel, pending,
                "menu:action", vec![shell_contract::IpcValue::String("open".into())]).map_err(TabError)?;
        }
        Ok(())
    }
    fn commit_native_editor_seed(
        &self, operation: &shell_contract::ipc::NativeInvocationOperation, expected: &shell_contract::ipc::NativeEditorCommand,
        view: ViewId, seed: Option<main_core::shell::tab_manager::EditorSeed>,
        docs_open_path: Option<String>, workbook: Option<String>, reused_target: Option<&shell_contract::ipc::NativeRequestOwner>,
        rollback: &shell_ipc_w::tabs::host::EditorSeedRollbackStore, created: &shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        browser: &shell_contract::ipc::NativeBrowserOwner,
        cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken,
    ) -> TabResult<()> {
        use shell_contract::{Surface, ipc::NativeEditorCommand};
        use main_core::shell::tab_manager::EditorSeed;
        if reused_target.is_some_and(|owner| owner.view != view || owner.context.renderer().is_none_or(|generation| !generation.same_generation(created))) {
            return Err(TabError("The captured spare editor was replaced".into()));
        }
        let (docs_blank, sheets_blank) = match (expected, &seed) {
            (NativeEditorCommand::OpenFile { surface, path }, None) => {
                let selected = path.to_str().ok_or_else(|| TabError("The selected path is not valid UTF-8".into()))?;
                let valid = match surface {
                    Surface::Docs => docs_open_path.as_deref() == Some(selected) && workbook.is_none(),
                    Surface::Sheets => workbook.as_deref() == Some(selected) && docs_open_path.is_none(),
                    _ => docs_open_path.is_none() && workbook.is_none(),
                };
                if !valid { return Err(TabError("The selected native editor input changed".into())); }
                (false, false)
            }
            (NativeEditorCommand::NewDocument { surface: Surface::Docs, .. }, Some(EditorSeed::DocsAi { new_blank: true, content: None }))
                if docs_open_path.is_none() && workbook.is_none() => (true, false),
            (NativeEditorCommand::NewDocument { surface: Surface::Sheets, .. }, None)
                if docs_open_path.is_none() && workbook.is_none() => (false, true),
            (NativeEditorCommand::NewDocument { surface, .. }, None)
                if *surface != Surface::Docs && *surface != Surface::Sheets && docs_open_path.is_none() && workbook.is_none() => (false, false),
            _ => return Err(TabError("The selected native editor seed does not match the command".into())),
        };
        let nudge = workbook.is_some() && reused_target.is_some();
        let effects = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned()
            .ok_or_else(|| TabError("Owned initialization effects are unavailable".into()))?;
        let docs_editor = if docs_open_path.is_some() || docs_blank {
            Some(self.editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs")?)
        } else { None };
        let sheets = if workbook.is_some() || sheets_blank {
            Some(shell_ipc_d1::sheets::editors::views().ok_or_else(|| TabError("Sheets editor is not registered".into()))?)
        } else { None };
        let mut docs = match docs_open_path {
            Some(path) => Some(docs_editor.as_ref().ok_or_else(|| TabError("Docs editor is unavailable".into()))?
                .prepare_queue_open(view, path, created.clone(), reservation).map_err(TabError)?),
            None => None,
        };
        let mut blank = if docs_blank {
            Some(docs_editor.as_ref().ok_or_else(|| TabError("Docs editor is unavailable".into()))?
                .prepare_blank_seed(view, created.clone(), reservation).map_err(TabError)?)
        } else { None };
        let mut workbook = match workbook {
            Some(path) => Some(sheets.as_ref().ok_or_else(|| TabError("Sheets editor is unavailable".into()))?
                .prepare_workbook_queue(view, path, created.clone(), reservation, browser).map_err(TabError)?),
            None => None,
        };
        let mut sheet_blank = if sheets_blank {
            Some(sheets.as_ref().ok_or_else(|| TabError("Sheets editor is unavailable".into()))?
                .prepare_blank_seed(view, created.clone(), reservation, browser).map_err(TabError)?)
        } else { None };
        let mut docs_committed = false;
        let mut blank_committed = false;
        let mut workbook_receipt = None;
        let mut sheet_blank_receipt = None;
        let participants = reused_target.map(std::slice::from_ref).unwrap_or(&[]);
        rollback.borrow_mut().try_reserve(4).map_err(|error| TabError(error.to_string()))?;
        let result = effects.with_native_command_creation_effect(operation, expected, created, participants, cancel, pending, &mut || {
            reservation.with_owner(browser, |transition| {
                let docs_valid = docs.as_mut().map(|plan| plan.validate_in_transition(transition, browser)).transpose()?;
                let blank_valid = blank.as_mut().map(|plan| plan.validate_in_transition(transition, browser)).transpose()?;
                let workbook_valid = workbook.take().map(|plan| plan.validate_in_transition(transition, browser)).transpose()?;
                let sheet_blank_valid = sheet_blank.take().map(|plan| plan.validate_in_transition(transition, browser)).transpose()?;
                if let Some(valid) = docs_valid { valid.commit(); docs_committed = true; }
                if let Some(valid) = blank_valid { valid.commit(); blank_committed = true; }
                if let Some(valid) = workbook_valid { workbook_receipt = Some(valid.commit_in_transition()); }
                if let Some(valid) = sheet_blank_valid { sheet_blank_receipt = Some(valid.commit_in_transition()); }
                Ok(())
            })
        });
        if docs_committed {
            let receipt = docs.ok_or_else(|| TabError("The committed Docs open is unavailable".into()))?.finish().map_err(TabError)?;
            rollback.borrow_mut().push(std::rc::Rc::new(DocsOpenRollback(receipt)));
        }
        if blank_committed {
            let receipt = blank.ok_or_else(|| TabError("The committed Docs blank is unavailable".into()))?.finish().map_err(TabError)?;
            rollback.borrow_mut().push(std::rc::Rc::new(DocsBlankRollback(receipt)));
        }
        if let Some(receipt) = workbook_receipt { rollback.borrow_mut().push(std::rc::Rc::new(WorkbookQueueRollback(receipt))); }
        if let Some(receipt) = sheet_blank_receipt { rollback.borrow_mut().push(std::rc::Rc::new(SheetsBlankRollback(receipt))); }
        result.map_err(TabError)?;
        if nudge {
            effects.emit_native_command_creation_many(operation, expected, created, participants, 0, cancel, pending,
                "menu:action", vec![shell_contract::IpcValue::String("open".into())]).map_err(TabError)?;
        }
        Ok(())
    }
    fn reveal_written_export_owned(
        &self,path:std::path::PathBuf,
        origin:shell_contract::ipc::NativeRequestOwner,destination:shell_contract::ipc::NativeRequestOwner,
        cancel:shell_contract::runtime::CancelToken,pending:shell_contract::runtime::CancelToken,
        done:Box<dyn FnOnce(Result<(),String>)>,
    ) {
        let Some(effects) = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return done(Err("Owned export reveal effects are unavailable".into()));
        };
        let done = shell_ipc_w::home_files::back_to_core(self.services.core.clone(),done);
        effects.post_ui_owned(origin,destination,cancel,pending,Box::new(move || crate::host::os_shell::show_item_in_folder(&path)),done);
    }
    fn reveal_written_export_existing_document(
        &self, selection: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let Some(effects) = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return done(Err("Native export reveal effects are unavailable".into()));
        };
        let reply = shell_ipc_w::home_files::back_to_core(self.services.core.clone(), done);
        self.services.ui.post(Box::new(move || {
            let extra: Vec<_> = additional.iter().collect();
            let path = selection.path();
            let mut action = Some(|| crate::host::os_shell::show_item_in_folder(path));
            let mut accepted = None;
            let result = effects.with_existing_document_destination_effect(
                &selection, path, &participants, &cancel, &pending, &extra, &mut || {
                    accepted = action.take();
                    Ok(())
                },
            ).and_then(|()| {
                let action = accepted.ok_or("The native export reveal was not admitted.")?;
                action()
            })
             .and_then(|()| effects.with_existing_document_destination_effect(
                &selection, path, &participants, &cancel, &pending, &extra, &mut || Ok(()),
            ));
            reply(result);
        }));
    }
    fn reveal_written_publication(
        &self, output: shell_contract::ipc::SavedPublication,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let Some(effects) = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return done(Err("Saved export reveal authority is unavailable".into()));
        };
        let extra: Vec<_> = additional.iter().collect();
        if let Err(error) = output.with_effect(&*effects, &participants, &cancel, &pending, &extra, &mut || Ok(())) {
            return done(Err(error));
        }
        let reply = shell_ipc_w::home_files::back_to_core(self.services.core.clone(), done);
        self.services.ui.post(Box::new(move || {
            let extra: Vec<_> = additional.iter().collect();
            let mut accepted = false;
            let result = output.with_effect(&*effects, &participants, &cancel, &pending, &extra, &mut || {
                accepted = true;
                Ok(())
            }).and_then(|()| {
                if !accepted { return Err("The saved export reveal was not admitted".into()); }
                crate::host::os_shell::show_item_in_folder(output.path())
            }).and_then(|()| output.with_effect(&*effects, &participants, &cancel, &pending, &extra, &mut || Ok(())));
            reply(result);
        }));
    }
    fn rollback_generated_seed_owned(&self, view: ViewId, expected: &shell_contract::runtime::RendererGeneration) {
        crate::host::d2::docs_tabs::discard_docs_ai_seed(&self.provided, view, expected);
        shell_ipc_a::user_skills::drop_generate_handoff_owned(&self.provided, view, expected);
        if self.cx.views.renderer_generation(view).is_none_or(|current| current.same_generation(expected)) {
            if let Some(projects) = self.provided.all::<shell_ipc_w::projects::Projects>().into_iter().next() {
                projects.new_documents().borrow_mut().forget(i64::from(view.0));
            }
        }
    }
    fn open_opener_owned(
        &self,opener:&main_core::shell::tab_manager::Opener,
        context:&main_core::shell::tab_manager::UnloadedTabOpenContext,
        source:shell_contract::ipc::NativeRequestOwner,cancel:shell_contract::runtime::CancelToken,
        pending:shell_contract::runtime::CancelToken,
        done:Box<dyn FnOnce(Result<shell_ipc_w::tabs::OwnedLoadedTabView,String>)>,
    ) {
        let reporter = match self.recovery_cleanup_reporter() { Ok(reporter) => reporter, Err(error) => return done(Err(error)) };
        let mut recovery=match crate::host::recovery_openers::prepare_open(opener,context) {
            Ok(recovery)=>recovery,Err(error)=>return done(Err(error)),
        };
        let Some(attempt)=recovery.take_attempt(self.services.core.clone()) else {
            return done(Err("The selected recovery attempt is unavailable".into()));
        };
        let Some(operation)=recovery.operation.take() else {
            return attempt.fail_before_creation("The selected recovery operation is unavailable".into(), Box::new(move |result| {
                done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
            }));
        };
        let Some(before)=recovery.before_load.take() else {
            return attempt.fail_before_creation("The selected recovery initializer is unavailable".into(), Box::new(move |result| {
                done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
            }));
        };
        let request=shell_contract::ipc::RecoveryRequestConstraint { source,participants:Vec::new(),cancel,pending };
        let kind=recovery.kind;
        let path=recovery.path.clone();
        let untitled=recovery.untitled;
        let opener=*opener;
        let context=context.clone();
        let callback=std::rc::Rc::new(std::cell::RefCell::new(Some(done)));
        let completed=callback.clone();
        let completed_attempt=attempt.clone();
        let started=super::create_recovery_view_owned(&self.services,self.provided.clone(),kind,path.as_deref(),untitled,
            shell_contract::ipc::RecoveryOpenContext::NativeStartup(operation),Some(request),None,None,attempt.rollback_store(),before,
            Box::new(move |result| {
                let Some(done)=completed.borrow_mut().take() else { return; };
                match result {
                    Err(error)=>{
                        let completion=Box::new(move |result:Result<(),String>| {
                            done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                        });
                        match error.cleanup {
                            shell_ipc_w::tabs::host::ViewCreationCleanupState::Complete => completed_attempt.fail_before_creation(error.message, completion),
                            shell_ipc_w::tabs::host::ViewCreationCleanupState::Acknowledged(receipt) => completed_attempt.fail_after_cleanup(error.message, receipt, reporter.clone(), completion),
                            shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(cleanup) => completed_attempt.fail(error.message, Some(cleanup), reporter.clone(), completion),
                        }
                    }
                    Ok(ready)=>{
                        let view=ready.view();
                        let document=match ready.invocation_document() {
                            Ok(document)=>document,
                            Err(error)=>{
                                let (_,discard,_)=ready.into_parts();
                                return completed_attempt.fail(error,Some(discard),reporter.clone(),Box::new(move |result| {
                                    done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                                }));
                            }
                        };
                        match crate::host::recovery_openers::finish_open(&opener,&context,recovery,view) {
                            Ok(loaded)=>{
                                let (owner,discard,closure)=ready.into_parts();
                                done(Ok(shell_ipc_w::tabs::OwnedLoadedTabView { loaded,owner,document,discard,closure,attempt:completed_attempt }))
                            }
                            Err(error)=>{
                                let (_,discard,_)=ready.into_parts();
                                completed_attempt.fail(error,Some(discard),reporter.clone(),Box::new(move |result| {
                                    done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                                }));
                            }
                        }
                    }
                }
            }));
        if let Err(error)=started {
            let done=callback.borrow_mut().take();
            if let Some(done)=done {
                attempt.fail_before_creation(error,Box::new(move |result| {
                    done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                }));
            }
        }
    }
    fn open_opener_startup(
        &self, opener: &main_core::shell::tab_manager::Opener,
        context: &main_core::shell::tab_manager::UnloadedTabOpenContext,
        startup: shell_ipc_w::tabs::StartupRecoveryConstraint,
        done: Box<dyn FnOnce(Result<shell_ipc_w::tabs::OwnedLoadedTabView, String>)>,
    ) {
        let reporter = match self.recovery_cleanup_reporter() { Ok(reporter) => reporter, Err(error) => return done(Err(error)) };
        let mut recovery = match crate::host::recovery_openers::prepare_open(opener, context) {
            Ok(recovery) => recovery,
            Err(error) => return done(Err(error)),
        };
        let Some(attempt) = recovery.take_attempt(self.services.core.clone()) else {
            return done(Err("The selected recovery attempt is unavailable".into()));
        };
        let Some(operation) = recovery.operation.take() else {
            return attempt.fail_before_creation("The selected recovery operation is unavailable".into(),
                Box::new(move |result| done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())))));
        };
        let Some(before) = recovery.before_load.take() else {
            return attempt.fail_before_creation("The selected recovery initializer is unavailable".into(),
                Box::new(move |result| done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())))));
        };
        let startup = shell_contract::ipc::StartupRecoveryConstraint {
            startup: startup.startup, shell: startup.shell, participants: startup.participants,
            cancel: startup.cancel, pending: startup.pending,
        };
        let kind = recovery.kind;
        let path = recovery.path.clone();
        let untitled = recovery.untitled;
        let opener = *opener;
        let context = context.clone();
        let callback = std::rc::Rc::new(std::cell::RefCell::new(Some(done)));
        let completed = callback.clone();
        let completed_attempt = attempt.clone();
        let started = super::create_recovery_view_owned(&self.services, self.provided.clone(), kind, path.as_deref(), untitled,
            shell_contract::ipc::RecoveryOpenContext::NativeStartup(operation), None, Some(startup), None, attempt.rollback_store(), before,
            Box::new(move |result| {
                let Some(done) = completed.borrow_mut().take() else { return; };
                match result {
                    Err(error) => {
                        let completion = Box::new(move |result: Result<(), String>| {
                            done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                        });
                        match error.cleanup {
                            shell_ipc_w::tabs::host::ViewCreationCleanupState::Complete => completed_attempt.fail_before_creation(error.message, completion),
                            shell_ipc_w::tabs::host::ViewCreationCleanupState::Acknowledged(receipt) => completed_attempt.fail_after_cleanup(error.message, receipt, reporter.clone(), completion),
                            shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(cleanup) => completed_attempt.fail(error.message, Some(cleanup), reporter.clone(), completion),
                        }
                    }
                    Ok(ready) => {
                        let view = ready.view();
                        let document = match ready.invocation_document() {
                            Ok(document) => document,
                            Err(error) => {
                                let (_, discard, _) = ready.into_parts();
                                return completed_attempt.fail(error, Some(discard), reporter.clone(), Box::new(move |result| {
                                    done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                                }));
                            }
                        };
                        match crate::host::recovery_openers::finish_open(&opener, &context, recovery, view) {
                            Ok(loaded) => {
                                let (owner, discard, closure) = ready.into_parts();
                                done(Ok(shell_ipc_w::tabs::OwnedLoadedTabView { loaded, owner, document, discard, closure, attempt: completed_attempt }));
                            }
                            Err(error) => {
                                let (_, discard, _) = ready.into_parts();
                                completed_attempt.fail(error, Some(discard), reporter.clone(), Box::new(move |result| {
                                    done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                                }));
                            }
                        }
                    }
                }
            }));
        if let Err(error) = started {
            if let Some(done) = callback.borrow_mut().take() {
                attempt.fail_before_creation(error, Box::new(move |result| {
                    done(Err(result.err().unwrap_or_else(|| "Recovery failure unexpectedly succeeded".into())));
                }));
            }
        }
    }
    fn open_opener_native_command(
        &self, opener: &main_core::shell::tab_manager::Opener,
        context: &main_core::shell::tab_manager::UnloadedTabOpenContext,
        command: shell_contract::ipc::NativeInvocationOperation,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<shell_ipc_w::tabs::OwnedLoadedTabView, String>)>,
    ) {
        self.open_opener_native_command_impl(opener, context, command, cancel, pending, done);
    }
    fn opener_needs_user_owned(
        &self,opener:&main_core::shell::tab_manager::Opener,context:&main_core::shell::tab_manager::UnloadedTabOpenContext,
        source:shell_contract::ipc::NativeRequestOwner,cancel:shell_contract::runtime::CancelToken,
        pending:shell_contract::runtime::CancelToken,done:Box<dyn FnOnce(Result<bool,String>)>,
    ) {
        crate::host::recovery_openers::query_needs_user(&self.services,opener,context,source,cancel,pending,done);
    }
    fn reveal_view_owned(
        &self, view: ViewId, expected: &shell_contract::runtime::RendererGeneration, owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken, then: Box<dyn FnOnce(TabResult<()>)>,
    ) {
        super::accepted::reveal(&self.cx, self.services.core.clone(), view, expected, owner, cancel, then);
    }
    fn reveal_captured_many(
        &self, authority: shell_ipc_w::tabs::TabActivationAuthority,
        target: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        done: shell_ipc_w::tabs::TabUiDone,
    ) {
        let Some(webview) = self.cx.windows.webview(target.view) else {
            done(Err("The captured editor view is unavailable.".into()));
            return;
        };
        let window = webview.window().clone();
        participants.push(target);
        let Some(effects) = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return done(Err("Native activation effects are unavailable".into()));
        };
        let reply = shell_ipc_w::home_files::back_to_core(self.services.core.clone(), done);
        let ui = self.services.ui.clone();
        let current_ui = ui.clone();
        ui.post(Box::new(move || {
            let result = super::activation::run_accepted_ui(current_ui.as_ref(), &effects, &authority, &participants, &cancel, &pending, move || {
                window.unminimize().map_err(|error| error.to_string())?;
                window.show().map_err(|error| error.to_string())?;
                window.set_focus().map_err(|error| error.to_string())?;
                webview.set_focus().map_err(|error| error.to_string())
            }).and_then(|result| result);
            reply(result);
        }));
    }
    fn adopt_recovery_view_owned(
        &self, shell: shell_ipc_w::tabs::ShellWindow, authority: shell_ipc_w::tabs::TabActivationAuthority,
        target: shell_contract::ipc::NativeRequestOwner, mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        done: shell_ipc_w::tabs::TabUiDone,
    ) {
        let captured = (|| -> Result<_, String> {
            let host = super::NATIVE_WINDOWS.get().cloned().ok_or("The native tab window host is unavailable.")?;
            let effects = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned()
                .ok_or("The native owned effect host is unavailable.")?;
            let shell_generation = self.cx.views.renderer_generation(shell.view).ok_or("The shell renderer is unavailable.")?;
            let shell_owner = crate::ipc::native_owner::capture_owner(&self.cx.views, shell.view, &shell_generation)?;
            let (shell_window, shell_lifecycle) = self.cx.views.view_window_captured(shell.view, &shell_generation)?;
            if shell_window != shell.window {
                return Err("The captured shell view is not in the target window.".into());
            }
            let window = shell_lifecycle.window_handle()?;
            let webview = self.cx.windows.webview(target.view).ok_or("The captured editor view is unavailable.")?;
            let generation = target.context.renderer().cloned().ok_or("The captured editor renderer is unavailable.")?;
            let (previous_id, previous_lifecycle) = self.cx.views.view_window_captured(target.view, &generation)?;
            let previous = previous_lifecycle.window_handle()?;
            let parking = host.parking.lock().map_err(|_| "The parking state is unavailable.")?.clone();
            Ok((host, effects, shell_owner, window, shell_lifecycle, webview, generation, previous_id, previous_lifecycle, previous, parking))
        })();
        let (host, effects, shell_owner, window, shell_lifecycle, webview, generation, previous_id, previous_lifecycle, previous, parking) = match captured {
            Ok(value) => value,
            Err(error) => return done(Err(error)),
        };
        let view = target.view;
        participants.push(target);
        participants.push(shell_owner);
        let reply = std::sync::Arc::new(std::sync::Mutex::new(Some(shell_ipc_w::home_files::back_to_core(self.services.core.clone(), done))));
        let async_reply = reply.clone();
        let ui = self.services.ui.clone();
        let captured_webview = webview.clone();
        let scheduled = webview.with_cef_webview_async(move |native| {
            let browser = match native {
                Ok(native) => native.frame_navigation_state().clone(),
                Err(error) => {
                    let reply = async_reply.lock().unwrap_or_else(|error| error.into_inner()).take();
                    if let Some(reply) = reply { reply(Err(error.to_string())); }
                    return;
                }
            };
            let current_ui = ui.clone();
            ui.post(Box::new(move || {
            let webview = captured_webview;
            let result = super::activation::run_accepted_ui(current_ui.as_ref(), &effects, &authority, &participants, &cancel, &pending, || {
                {
                    let slot = host.parking.lock().map_err(|_| "The parking state is unavailable.")?;
                    let matches = match (slot.as_ref(), parking.as_ref()) {
                        (None, None) => true,
                        (Some(current), Some(captured)) => current.same(captured),
                        _ => false,
                    };
                    if !matches { return Err("The captured parking window changed.".into()); }
                }
                if webview.window().label() != previous.label() { return Err("The captured editor parent changed.".into()); }
                webview.hide().map_err(|error| error.to_string())?;
                if let Err(error) = webview.reparent(&window) {
                    return match webview.reparent(&previous) {
                        Ok(()) => Err(format!("reparenting view: {error}")),
                        Err(rollback) => Err(format!("reparenting view: {error}; restoring parent: {rollback}")),
                    };
                }
                let moved = super::activation_publication::admit(&effects, &authority, &participants, &cancel, &pending, &mut || {
                    host.cx.views.move_view_between_captured(view, &generation, previous_id, &previous_lifecycle, shell.window, &shell_lifecycle)
                });
                if let Err(error) = moved {
                    return match webview.reparent(&previous) {
                        Ok(()) => Err(error),
                        Err(rollback) => Err(format!("{error}; restoring parent: {rollback}")),
                    };
                }
                if let Some(parking) = parking {
                    let mut slot = host.parking.lock().map_err(|_| "The parking state is unavailable.")?;
                    if !slot.as_ref().is_some_and(|current| current.same(&parking)) { return Err("The captured parking window changed.".into()); }
                    if host.cx.views.views_of_window(parking.id).is_empty() {
                        *slot = None;
                        drop(slot);
                        parking.window.destroy().map_err(|error| error.to_string())?;
                        host.cx.views.remove_window_captured(parking.id, &parking.lifecycle)?;
                    }
                }
                super::activation_publication::admit(&effects, &authority, &participants, &cancel, &pending, &mut || {
                    host.register_listener_captured(view, shell_ipc_w::tabs::ViewListen::HtmlFullScreen, &generation, None)?;
                    host.register_listener_captured(view, shell_ipc_w::tabs::ViewListen::PaneFocus, &generation, Some(browser.clone()))
                })
            }).and_then(|result| result);
            let reply = async_reply.lock().unwrap_or_else(|error| error.into_inner()).take();
            if let Some(reply) = reply { reply(result); }
            }));
        });
        if let Err(error) = scheduled {
            let reply = reply.lock().unwrap_or_else(|error| error.into_inner()).take();
            if let Some(reply) = reply { reply(Err(error.to_string())); }
        }
    }
    fn prepare_reopen_owned(
        &self, path: String, owner: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Box<dyn FnOnce(Result<(String, Option<String>, shell_contract::ipc::NativeRequestOwner, shell_contract::runtime::CancelToken), String>)>,
    ) {
        let Some(service) = self.provided.all::<dyn shell_ipc_a::drive::DriveService>().into_iter().next() else {
            then(Ok((path, None, owner, cancel)));
            return;
        };
        let id = super::accepted::store_reopen(then, &destination, &pending);
        let captured_owner = owner.clone();
        let captured_cancel = cancel.clone();
        let admission = std::sync::Arc::new(move |owner: &shell_contract::ipc::NativeRequestOwner, cancel: &shell_contract::runtime::CancelToken, commit: &mut dyn FnMut() -> Result<(), String>| {
            crate::ipc::native_owner::with_paired_effect(owner, &destination, cancel, &pending, commit)
                .ok_or_else(|| "A captured Drive reopen participant is no longer active".to_owned())?
        });
        service.prepare_reopen(&path.clone(), &owner, &cancel, admission, Box::new(move |result| {
            super::accepted::complete_reopen(id, result.map(|prepared| match prepared {
                Some(prepared) => prepared.into_parts(),
                None => (path, None, captured_owner, captured_cancel),
            }));
        }));
    }
    fn prepare_reopen_existing_document(
        &self, output: shell_contract::ipc::SavedPublication,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        then: Box<dyn FnOnce(Result<shell_contract::ipc::NativeDriveReopen, String>)>,
    ) {
        let Some(service) = self.provided.all::<dyn shell_ipc_a::drive::DriveService>().into_iter().next() else {
            return then(Err("The Drive reopen service is unavailable".into()));
        };
        let Some(effects) = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return then(Err("The Drive reopen authority is unavailable".into()));
        };
        let done = shell_ipc_w::home_files::back_to_core(self.services.core.clone(), then);
        service.prepare_native_reopen(output, participants, cancel, pending, additional, effects, done);
    }
    fn read_saved_input(
        &self, output: shell_contract::ipc::SavedPublication, path: shell_contract::ipc::NativeSavedPath,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken, additional: Vec<shell_contract::runtime::CancelToken>,
        then: Box<dyn FnOnce(Result<shell_contract::ipc::NativeSavedInput, String>)>,
    ) {
        let done = shell_ipc_w::home_files::back_to_core(self.services.core.clone(), then);
        self.services.workers.spawn(Box::new(move || {
            let result = crate::ipc::native_owner::read_saved_input(
                &output, &path, usize::MAX, &participants, &cancel, &pending, &additional,
            ).map_err(|error| error.to_string());
            done(result);
        }));
    }
    fn sheets_pending_edit_count(&self, id: i64) -> f64 {
        view_id(id).and_then(|view| shell_ipc_d1::sheets::editors::views().map(|s| s.pending_edit_count(view))).unwrap_or(f64::INFINITY)
    }
    fn family_is_dirty(&self, kind: TabKind, id: i64) -> TabResult<bool> { self.editor_is_dirty(kind, id) }
    fn hwp_memo_modal_open(&self, id: i64) -> bool {
        view_id(id).and_then(|view| self.editor::<shell_ipc_d2::hwp::HwpEditors>("HWP").ok().map(|editor| editor.memo_modal_open(view))).unwrap_or(true)
    }
    fn docs_query_dirty(&self, view: ViewId, then: Then<bool>) { self.query_docs_dirty(view, then); }
    fn set_active_docs_resolver(&self, view: Option<ViewId>, null_if_destroyed: bool) {
        crate::host::d2::set_active_docs_resolver(view, null_if_destroyed);
    }
    fn set_active_sheets_web_contents(&self, view: Option<ViewId>) {
        shell_ipc_d1::sheets::editors::views().expect("Sheets editor unavailable").set_active_view(view);
    }
    fn set_active_slides_web_contents(&self, view: ViewId) {
        shell_ipc_d1::slides::views().expect("Slides editor unavailable").set_active_view(view);
    }
    fn apply_menu_for(&self, kind: TabKind) { crate::host::menu::apply_menu(kind); }
    fn teardown_docs_renderer(&self, view: ViewId) {
        self.editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs").expect("Docs editor unavailable").teardown(view, false);
    }
    fn release_closed_renderer(&self, view: ViewId) { Hooks::release_closed_renderer(self, view); }
    fn mark_docs_new_blank(&self, id: i64) {
        self.editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs").expect("Docs editor unavailable").mark_new_blank(view_id(id).expect("invalid view ID"));
    }
    fn queue_docs_ai_content(&self, _: i64, _: &serde_json::Value) -> TabResult<()> {
        Err(TabError("Native AI document content requires the owned initialized-editor transaction".into()))
    }
    fn queue_docs_copy(&self, id: i64, path: &str, name: &str) {
        self.editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs").expect("Docs editor unavailable").queue_copy(view_id(id).expect("invalid view ID"), path.to_owned(), name.to_owned());
    }
    fn set_sheets_new_blank(&self) { shell_ipc_d1::sheets::editors::views().expect("Sheets editor unavailable").set_new_blank(); }
    fn queue_workbook_for_view(&self, view: ViewId, path: &str) { shell_ipc_d1::sheets::editors::views().expect("Sheets editor unavailable").queue_workbook(view, path); }
    fn nudge_queued_workbook(&self, view: ViewId) { shell_ipc_d1::sheets::editors::views().expect("Sheets editor unavailable").nudge_queued_workbook(view); }
    fn mark_pdf_untitled_copy(&self, path: &str) { self.editor::<shell_ipc_d2::pdf::editor::editors::PdfEditors>("PDF").expect("PDF editor unavailable").mark_untitled_copy(path); }
    fn clear_pdf_dirty(&self, id: i64) { self.editor::<shell_ipc_d2::pdf::editor::editors::PdfEditors>("PDF").expect("PDF editor unavailable").clear_dirty(view_id(id).expect("invalid view ID")); }
    fn queue_markdown_seed_for(&self, view: ViewId, text: &str) { self.editor::<shell_ipc_d2::markdown::MarkdownEditors>("Markdown").expect("Markdown editor unavailable").queue_seed(view, text); }
    fn queue_html_seed_for(&self, view: ViewId, text: &str) { self.editor::<shell_ipc_d2::html::HtmlEditors>("HTML").expect("HTML editor unavailable").queue_seed(view, text); }
    fn queue_mindmap_seed_for(&self, view: ViewId, bytes: &[u8]) { shell_ipc_d1::mindmap::handle().expect("Mindmap editor unavailable").queue_seed(view, shell_ipc_d1::mindmap::Seed::Copy(bytes.to_vec())); }
    fn queue_slides_seed_for(&self, view: ViewId, bytes: &[u8], name: &str) { shell_ipc_d1::slides::views().expect("Slides editor unavailable").queue_seed(view, bytes, name); }
    fn queue_generate_handoff_for(&self, view: ViewId, handoff: &serde_json::Value) { self.queue_generate(view, handoff); }
    fn queue_converted_content(&self, view: ViewId, doc: &serde_json::Value) { self.queue_conversion(view, doc); }
    fn drop_generate_handoff(&self, id: i64) { crate::host::net::tools::generate::drop_generate_handoff(&self.provided, id); }
    fn diagram_tab_identity(&self, id: i64) -> Option<DiagramIdentity> { self.diagram_identity(id) }
    fn opener_open(&self, opener: &main_core::shell::tab_manager::Opener, context: &main_core::shell::tab_manager::UnloadedTabOpenContext) -> Result<main_core::shell::tab_manager::LoadedTabView<ViewId>, String> {
        crate::host::recovery_openers::open(opener, context)
    }
    fn opener_needs_user(&self, opener: &main_core::shell::tab_manager::Opener, context: &main_core::shell::tab_manager::UnloadedTabOpenContext) -> Result<bool, String> {
        crate::host::recovery_openers::needs_user(opener, context)
    }
    fn reset_sheets_shutting_down(&self) { shell_ipc_d1::sheets::editors::views().expect("Sheets editor unavailable").reset_shutting_down(); }
    fn resume_discarded_copies(&self) {
        let state = self.services.hosts.get::<std::sync::Arc<std::sync::Mutex<main_core::packages::electron_utils::recovery_store::SharedRecoveryState>>>().expect("recovery state unavailable");
        let state = state.lock().expect("recovery state poisoned");
        main_core::packages::electron_utils::recovery_store::resume_discarded_copies(&state);
    }
    fn pdf_close_cancelled(&self, view: ViewId) { self.editor::<shell_ipc_d2::pdf::editor::editors::PdfEditors>("PDF").expect("PDF editor unavailable").close_cancelled(view); }
    fn ui_scale(&self) -> f64 { crate::ui_scale::current() }
    fn no_spare_view_env(&self) -> bool { std::env::var_os("SEUOFFICE_NO_SPARE_VIEW").is_some_and(|value| !value.is_empty()) }
    fn renderer_dev_url(&self) -> Option<String> { std::env::var("ELECTRON_RENDERER_URL").ok() }
    fn tm(&self, key: &str) -> String {
        use shell_ipc_w::editor_files::UiLanguage;
        let language = crate::host::w_files::ShellLanguage(self.cx.ui.clone()).lang();
        shell_ipc_m::menu_label(shell_ipc_m::MenuDict::Shell, language, key)
    }
    fn tm_params(&self, key: &str, params: &[(&str, &str)]) -> String {
        let mut text = <Self as shell_ipc_w::tabs::TabHooks>::tm(self, key);
        for (name, value) in params { text = text.replace(&format!("{{{name}}}"), value); }
        text
    }
    fn random_uuid(&self) -> String { jscompat::random_uuid() }
    fn warn(&self, message: &str, error: &str) { eprintln!("{message}: {error}"); }
    fn app_menu_popup(&self) -> Option<Vec<shell_ipc_w::tabs::MenuEntry>> { crate::host::menu::app_menu_popup() }
    fn app_menu_activate(&self, id: u32, cx: shell_contract::ipc::CallContext) { crate::host::menu::app_menu_activate(id, cx); }
    fn visible_home_kinds(&self) -> Vec<String> {
        shell_ipc_w::home_settings::visible_home_kinds(&crate::paths::ShellPaths::new().settings_file().to_string_lossy())
    }
    fn record_recent_file(&self, path: &str) {
        self.provided.all::<shell_ipc_w::home_files::Home>().into_iter().next().expect("Home service unavailable").record_recent_file(path);
    }
    fn record_recent_file_native(
        &self, path: String, operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand, done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if !operation.is_root_native_command() || !matches!(&expected, shell_contract::ipc::NativeEditorCommand::OpenFile { path: selected, .. }
            if selected.as_os_str() == std::ffi::OsStr::new(&path))
            || operation.selection() != &shell_contract::ipc::NativeInvocationSelection::NativeCommand(expected.clone()) {
            return done(Err("The recent file does not match the selected native command".into()));
        }
        let Some(home) = self.provided.all::<shell_ipc_w::home_files::Home>().into_iter().next() else {
            return done(Err("Home service unavailable".into()));
        };
        let Some(effects) = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return done(Err("Native command effects are unavailable".into()));
        };
        match home.prepare_record_recent_file_native(&operation, &expected, &effects) {
            Ok(prepared) => prepared.record(path, done),
            Err(error) => done(Err(error)),
        }
    }
    fn show_app_warning(&self, message: &str, detail: Option<&str>) {
        let dialog = self.services.hosts.get::<shell_contract::DialogsHost>().expect("dialogs unavailable").clone();
        let parent = super::NATIVE_WINDOWS.get().and_then(|windows| windows.shell_window()).map(|window| window.window);
        let options = shell_contract::hosts::MessageBox {
            kind: Some("warning".into()), message: message.to_owned(), detail: detail.map(str::to_owned), ..Default::default()
        };
        self.services.workers.spawn(Box::new(move || { dialog.message(parent, &options); }));
    }
    fn show_app_warning_owned(
        &self, message: String, detail: Option<String>, origin: shell_contract::ipc::NativeRequestOwner,
        mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let captured = (|| -> Result<_, String> {
            let Some(shell) = super::NATIVE_WINDOWS.get().and_then(|windows| windows.shell_window()) else {
                return Ok(None);
            };
            let generation = self.cx.views.renderer_generation(shell.view).ok_or("The shell renderer is unavailable")?;
            let owner = crate::ipc::native_owner::capture_owner(&self.cx.views, shell.view, &generation)?;
            let (window, lifecycle) = self.cx.views.view_window_captured(shell.view, &generation)?;
            if window != shell.window { return Err("The shell dialog parent changed".into()); }
            participants.push(owner);
            Ok(Some(crate::host::dialog::capture_owned_lifecycle(lifecycle)?))
        })();
        let parent = match captured { Ok(parent) => parent, Err(error) => return done(Err(error)) };
        let options = shell_contract::hosts::MessageBox { kind: Some("warning".into()), message, detail, ..Default::default() };
        let done = shell_ipc_w::home_files::back_to_core(self.services.core.clone(), done);
        crate::host::dialog::owned::run_many_extra(self.services.ui.clone(), origin, participants, cancel, pending, Vec::new(),
            move |session| crate::host::dialog::owned::message(&options, crate::host::dialog::checked_owned_parent(&parent, &session)?, &session).map(|_| ()), done);
    }
    fn show_app_warning_native(
        &self, message: String, detail: Option<String>, operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand, done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if !operation.is_root_native_command() || !matches!(&expected, shell_contract::ipc::NativeEditorCommand::OpenFile { .. })
            || operation.selection() != &shell_contract::ipc::NativeInvocationSelection::NativeCommand(expected.clone()) {
            return done(Err("The warning does not match the selected native command".into()));
        }
        let Some(effects) = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return done(Err("Native command effects are unavailable".into()));
        };
        let captured = (|| -> Result<_, String> {
            let Some(shell) = super::NATIVE_WINDOWS.get().and_then(|windows| windows.shell_window()) else {
                return Ok((None, Vec::new()));
            };
            let generation = self.cx.views.renderer_generation(shell.view).ok_or("The shell renderer is unavailable")?;
            let owner = crate::ipc::native_owner::capture_owner(&self.cx.views, shell.view, &generation)?;
            let (window, lifecycle) = self.cx.views.view_window_captured(shell.view, &generation)?;
            if window != shell.window { return Err("The shell dialog parent changed".into()); }
            Ok((Some(crate::host::dialog::capture_owned_lifecycle(lifecycle)?), vec![owner]))
        })();
        let (parent, participants) = match captured { Ok(captured) => captured, Err(error) => return done(Err(error)) };
        let options = shell_contract::hosts::MessageBox { kind: Some("warning".into()), message, detail, ..Default::default() };
        let done = shell_ipc_w::home_files::back_to_core(self.services.core.clone(), done);
        crate::host::dialog::owned::run_native_command(self.services.ui.clone(), effects, operation, expected, participants,
            move |session| crate::host::dialog::owned::message(&options, crate::host::dialog::checked_owned_parent(&parent, &session)?, &session).map(|_| ()), done);
    }
    fn show_error_dialog(&self, title: &str, error: &str) {
        let dialog = self.services.hosts.get::<shell_contract::DialogsHost>().expect("dialogs unavailable").clone();
        let title = title.to_owned();
        let error = error.to_owned();
        self.services.workers.spawn(Box::new(move || dialog.error(&title, &error)));
    }
    fn notify_unsupported_file(&self, path: &str) {
        use shell_ipc_w::tabs::TabHooks;
        let ext = jscompat::node_path::host::extname(path).trim_start_matches('.').to_lowercase();
        let ext = if ext.is_empty() { jscompat::node_path::host::basename(path, None) } else { ext };
        self.show_app_warning(&self.tm_params("errUnsupportedExt", &[("ext", &ext)]), None);
    }
    fn notify_unsupported_file_native(
        &self, path: String, operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand, done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        use shell_ipc_w::tabs::TabHooks;
        if !matches!(&expected, shell_contract::ipc::NativeEditorCommand::OpenFile { path: selected, .. }
            if selected.as_os_str() == std::ffi::OsStr::new(&path)) {
            return done(Err("The unsupported file does not match the selected native command".into()));
        }
        let ext = jscompat::node_path::host::extname(&path).trim_start_matches('.').to_lowercase();
        let ext = if ext.is_empty() { jscompat::node_path::host::basename(&path, None) } else { ext };
        self.show_app_warning_native(self.tm_params("errUnsupportedExt", &[("ext", &ext)]), None, operation, expected, done);
    }
    fn diagram_open_refusal_owned(
        &self, path: String, origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<String>, String>)>,
    ) {
        let Some(effects) = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return done(Err("Owned diagram effects are unavailable".into()));
        };
        let Some(views) = shell_ipc_d1::diagram::views() else {
            return done(Err("The diagram recovery service is unavailable".into()));
        };
        let Some(language) = self.services.hosts.get::<shell_ipc_w::editor_files::UiLanguageHost>() else {
            return done(Err("The application language is unavailable".into()));
        };
        native_diagram::check_owned(&self.services, effects, views.recovery_lookup(), language.lang(),
            path, origin, participants, cancel, pending, done);
    }
    fn diagram_open_refusal_native(
        &self, path: String, operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand, done: Box<dyn FnOnce(Result<Option<String>, String>)>,
    ) {
        let Some(effects) = self.services.hosts.get::<shell_contract::hosts::OwnedEffectsHost>().cloned() else {
            return done(Err("Native command effects are unavailable".into()));
        };
        let Some(views) = shell_ipc_d1::diagram::views() else {
            return done(Err("The diagram recovery service is unavailable".into()));
        };
        let Some(language) = self.services.hosts.get::<shell_ipc_w::editor_files::UiLanguageHost>() else {
            return done(Err("The application language is unavailable".into()));
        };
        native_diagram::check(&self.services, effects, views.recovery_lookup(), language.lang(), path, operation, expected, done);
    }
    fn unique_path_in(&self, dir: &str, name: &str) -> String {
        main_core::shell::mcp::tools::document_tools::unique_path_in(dir, name)
    }
    fn drive_reopen(&self, path: &str) -> bool {
        self.provided.all::<dyn shell_ipc_a::drive::DriveService>().into_iter().next()
            .is_some_and(|service| service.reopen(path, Box::new(|_| {})))
    }
    fn default_save_dir(&self) -> String { default_save_dir(&self.services) }
    fn open_agent_window(&self) {
        self.provided.all::<dyn shell_ipc_a::agent::AgentWindowControl>().into_iter().next()
            .expect("agent window service unavailable").open();
    }
    fn start_queued_workbook_nudge(&self) { shell_ipc_d1::sheets::editors::views().expect("Sheets editor unavailable").start_queued_workbook_nudge(); }
    fn blank_xlsx(&self) -> Result<Vec<u8>, String> {
        Ok(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../apps/shell/build/shell-new/blank.xlsx")).to_vec())
    }
    fn blank_pdf(&self) -> Vec<u8> { super::blank_pdf() }
    fn sheets_suggested_path_taken(&self, path: &str) -> bool { shell_ipc_d1::sheets::editors::views().expect("Sheets editor unavailable").suggested_path_taken(path) }
    fn mark_sheets_unsaved_new(&self, backing: &str, suggested: &str, dir: &str) { shell_ipc_d1::sheets::editors::views().expect("Sheets editor unavailable").mark_unsaved_new(backing, suggested, dir); }
    fn unmark_sheets_unsaved_new(&self, backing: &str) { shell_ipc_d1::sheets::editors::views().expect("Sheets editor unavailable").unmark_unsaved_new(backing); }
    fn mark_sheets_untitled_path(&self, path: &str) { shell_ipc_d1::sheets::editors::views().expect("Sheets editor unavailable").mark_untitled_path(path); }
    fn mark_pdf_untitled_path(&self, path: &str) { self.editor::<shell_ipc_d2::pdf::editor::editors::PdfEditors>("PDF").expect("PDF editor unavailable").mark_untitled_path(path); }
    fn drop_no_skills_found(&self) {
        self.provided.all::<shell_ipc_a::user_skills::SkillsCore>().first().expect("Skills service unavailable").queue_dropped(DroppedSkillEntry::NoSkills);
    }
    fn is_inside_root(&self, root: &str, path: &str) -> bool { main_core::shell::folder_tree::is_inside_root(root, path) }
    fn open_file_via_dialog(&self) {
        use jscompat::{JsObject, JsonValue};
        use main_core::packages::electron_utils::dialog_memory::show_open_dialog_with_memory;
        use shell_ipc_w::tabs::TabHooks;
        let Some(parent) = super::NATIVE_WINDOWS.get().and_then(|windows| windows.shell_window()).map(|shell| shell.window) else { return };
        let dialogs = self.services.hosts.get::<shell_contract::DialogsHost>().expect("dialogs unavailable").clone();
        let memory = self.services.hosts.get::<shell_ipc_w::SharedDialogMemory>().expect("dialog memory unavailable").clone();
        let name = self.tm("filterSupported");
        self.services.workers.spawn(Box::new(move || {
            let extensions = ["docx", "doc", "dot", "odt", "xlsx", "xlsm", "xls", "csv", "tsv", "ods", "pptx", "ppt", "odp", "otp", "pdf", "md", "markdown", "txt", "json", "html", "htm", "xmind", "mm", "hwp", "hwpx", "epub", "drawio", "dio", "drawio.svg", "drawio.png", "vsdx"];
            let mut filter = JsObject::new();
            filter.set("name", JsonValue::String(name));
            filter.set("extensions", JsonValue::Array(extensions.into_iter().map(|s| JsonValue::String(s.into())).collect()));
            let mut options = JsObject::new();
            options.set("filters", JsonValue::Array(vec![JsonValue::Object(filter)]));
            options.set("properties", JsonValue::Array(vec![JsonValue::String("openFile".into()), JsonValue::String("multiSelections".into())]));
            let mut dialog = shell_ipc_d2::hwp::FileDialogs(dialogs);
            let answer = shell_ipc_w::shared_dialog::with_shared_memory(&memory, &mut dialog, |memory, dialog| {
                show_open_dialog_with_memory(memory, dialog, Some(&parent), &options, None)
            });
            let paths = match answer {
                JsonValue::Object(answer) if !matches!(answer.get("canceled"), Some(JsonValue::Bool(true))) => {
                    match answer.get("filePaths") {
                        Some(JsonValue::Array(paths)) => paths.iter().filter_map(|path| match path { JsonValue::String(path) => Some(path.clone()), _ => None }).collect::<Vec<_>>(),
                        _ => Vec::new(),
                    }
                }
                _ => Vec::new(),
            };
            crate::host::w::post_core(move |provided, _| {
                if let Some(tabs) = provided.all::<shell_ipc_w::tabs::TabsState>().into_iter().next() {
                    for path in paths { tabs.open_document_path(&path); }
                }
            });
        }));
    }
    fn tab_document_action(&self, kind: TabKind, view: ViewId, _tab_id: &str, _file_path: Option<&str>, action: shell_ipc_w::tabs::DocAction) {
        use shell_contract::editors::{editor_for, SaveMode};
        use shell_contract::ipc::Target;
        use shell_contract::IpcValue;
        use shell_ipc_w::tabs::DocAction;
        let command = match action { DocAction::Save => "save", DocAction::SaveAs => "save-as", DocAction::Print => "print" };
        let channel = match kind {
            TabKind::Docs => Some("menu:command"),
            TabKind::Sheets => Some("menu:action"),
            TabKind::Slides => Some("slides:menu"),
            _ => None,
        };
        if let Some(channel) = channel {
            self.services.events.emit(Target::View(view), channel, vec![IpcValue::String(command.into())]);
            return;
        }
        if matches!(kind, TabKind::Home | TabKind::Help | TabKind::Epub | TabKind::Diagram) { return; }
        if matches!(action, DocAction::Print) {
            if kind == TabKind::Hwp {
                self.editor::<shell_ipc_d2::hwp::HwpEditors>("HWP").expect("HWP editor unavailable").request_preview(view, "print");
            } else {
                let channel = match kind {
                    TabKind::Pdf => "pdf:print-request",
                    TabKind::Markdown => "markdown:print-request",
                    TabKind::Html => "html:print-request",
                    TabKind::Mindmap => "mindmap:print-request",
                    _ => unreachable!(),
                };
                self.services.events.emit(Target::View(view), channel, Vec::new());
            }
        } else if let Some(editor) = editor_for(&self.provided, view) {
            let mode = if matches!(action, DocAction::SaveAs) { SaveMode::SaveAs } else { SaveMode::Save };
            editor.request_save(view, mode, Box::new(move |result| {
                if let Err(error) = result { eprintln!("shell: saving tab {}: {}", view.0, error.message); }
            }));
        }
    }
    fn drop_skill_file(&self, path: &str, then: Box<dyn FnOnce(bool)>) { self.classify_skill_drop(path, then); }
}
