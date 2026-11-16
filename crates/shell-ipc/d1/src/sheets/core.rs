//! The module-level state of sheets-main.ts, and the two ways handlers reach it.
//!
//! The TypeScript keeps `sheetsTabs`, `pendingEditCounts` and the rest as
//! module-level `Map`s the `ipcMain` listeners share. Here that is one
//! [`AppState`] in an `Rc<RefCell<…>>` on the core thread, wrapped in a
//! [`Core`] together with the hosts. A handler that answers at once works on
//! a `&Core`. A handler that awaits in TypeScript (a dialog, the sidecar, a
//! file copy) runs as straight-line code on a worker through an [`Env`], the
//! `Send` half of the same, and hops to the core thread with [`Env::core`]
//! wherever it reads or changes state, as the TypeScript's awaits resume on
//! Electron's main thread. The core thread itself never waits for the
//! engine, a worker or a renderer.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision;
use main_core::packages::electron_utils::recovery_store::{
    RecoveryIdentity, RecoveryStoreOptions, SharedRecoveryState, create_recovery_store,
};
use main_core::sheets::close_guard::ShutdownLatch;
use main_core::sheets::import_registry::ImportRegistry;
use main_core::sheets::save_as_formats::SaveAsExportTargets;
use main_core::sheets::save_edits_transfer::{SaveEditsTransferStore, SweepTimers};
use shell_contract::ipc::Target;
use shell_contract::runtime::{PathName, Services};
use shell_contract::{ClipboardHost, DialogsHost, IpcError, IpcValue, ViewId, WindowId};

use super::engine::{ScreenCaptureHost, SheetsShellHost, WorkbookEngineHost};
use super::i18n::Tm;
use super::session::SessionInfo;
use crate::host::{RecoveryStateHost, Shared};
use crate::value::thrown;

/// The store's sweep interval is left to the next call: every `begin` and
/// `addChunk` sweeps expired uploads first, so only the early release of an
/// abandoned upload's memory is lost
pub struct NoTimers;

impl SweepTimers for NoTimers {
    fn set_interval(&mut self, _ms: f64) {}
    fn clear_interval(&mut self) {}
}

#[derive(Default)]
pub(super) struct HeadlessWork {
    state: Mutex<HeadlessWorkState>,
}

#[derive(Default)]
struct HeadlessWorkState {
    count: usize,
    closing: bool,
    drained: Option<Box<dyn FnOnce() + Send>>,
}

pub(super) struct HeadlessWorkGuard(Arc<HeadlessWork>);

impl HeadlessWork {
    pub fn begin(self: &Arc<Self>) -> Result<HeadlessWorkGuard, IpcError> {
        let mut state = self.state.lock().unwrap();
        if state.closing {
            return Err(thrown("The headless workbook is closing"));
        }
        state.count = state
            .count
            .checked_add(1)
            .ok_or_else(|| thrown("Too many headless workbook operations"))?;
        Ok(HeadlessWorkGuard(self.clone()))
    }

    pub fn drain(&self, done: Box<dyn FnOnce() + Send>) -> Result<(), String> {
        let done = {
            let mut state = self.state.lock().unwrap();
            if state.drained.is_some() {
                return Err("The headless workbook is already closing".into());
            }
            state.closing = true;
            if state.count == 0 {
                Some(done)
            } else {
                state.drained = Some(done);
                None
            }
        };
        if let Some(done) = done {
            done();
        }
        Ok(())
    }
}

impl Drop for HeadlessWorkGuard {
    fn drop(&mut self) {
        let done = {
            let mut state = self.0.state.lock().unwrap();
            state.count -= 1;
            if state.count == 0 {
                state.drained.take()
            } else {
                None
            }
        };
        if let Some(done) = done {
            done();
        }
    }
}

/// `SheetsTabSession`
pub struct TabState {
    /// sessions by id, in the order they opened (a `Map`)
    pub sessions: Vec<(String, SessionInfo)>,
    pub save_transfers: SaveEditsTransferStore<NoTimers>,
    /// the file this tab shows (merge sources excluded); `null` until the first open lands
    pub current_path: Option<String>,
    pub native_owner: Option<shell_contract::ipc::NativeRequestOwner>,
    pub document: shell_contract::runtime::RendererGeneration,
    pub metadata_revision: Option<u64>,
    pub(super) queue_revision: Rc<()>,
    pub(super) registration_retired: Rc<std::cell::Cell<bool>>,
    pub(super) owned_registration: bool,
    pub(super) browser_owner: Option<shell_contract::ipc::NativeBrowserOwner>,
    pub(super) browser_closing: bool,
    pub(super) saved_load: Rc<std::cell::RefCell<Option<SavedLoad>>>,
    pub headless_target: Option<shell_contract::ipc::NativeInvocationTarget>,
    pub(super) headless_work: Option<Arc<HeadlessWork>>,
    #[allow(clippy::type_complexity)]
    pub(super) headless_drain: Option<Box<dyn FnOnce(Result<(), String>)>>,
}

impl TabState {
    pub(super) fn new() -> TabState {
        TabState {
            sessions: Vec::new(),
            save_transfers: SaveEditsTransferStore::new(NoTimers),
            current_path: None,
            native_owner: None,
            document: shell_contract::runtime::RendererGeneration::new(),
            metadata_revision: Some(0),
            queue_revision: Rc::new(()),
            registration_retired: Rc::new(std::cell::Cell::new(false)),
            owned_registration: false,
            browser_owner: None,
            saved_load: Rc::new(std::cell::RefCell::new(None)),
            browser_closing: false,
            headless_target: None,
            headless_work: None,
            headless_drain: None,
        }
    }

    pub fn session(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, session)| session)
    }

    /// `sessions.set(id, session)`: a key that is there keeps its place
    pub fn set_session(&mut self, id: &str, session: SessionInfo) {
        self.bump_metadata_revision();
        match self.sessions.iter_mut().find(|(key, _)| key == id) {
            Some(entry) => entry.1 = session,
            None => self.sessions.push((id.to_owned(), session)),
        }
    }

    pub fn remove_session(&mut self, id: &str) -> Option<SessionInfo> {
        let at = self.sessions.iter().position(|(key, _)| key == id)?;
        self.bump_metadata_revision();
        Some(self.sessions.remove(at).1)
    }

    pub fn set_current_path(&mut self, path: Option<String>) {
        self.bump_metadata_revision();
        self.current_path = path;
    }

    fn bump_metadata_revision(&mut self) {
        self.metadata_revision = self
            .metadata_revision
            .and_then(|revision| revision.checked_add(1));
    }
}

pub(super) struct SavedLoad {
    pub target: shell_contract::ipc::NativeSavedTarget,
    pub document: Option<shell_contract::ipc::NativeSavedDocument>,
    pub completion:
        Option<shell_contract::editors::Then<shell_contract::editors::SavedDocumentLoadOutcome>>,
    pub id: String,
    pub selection: Option<SavedSelection>,
    pub started: bool,
    pub subscriptions: Vec<shell_contract::runtime::CancelSubscription>,
    pub session_id: Option<String>,
    pub recovery_applied: bool,
    pub failure: Option<String>,
}

impl SavedLoad {
    pub fn new(target: shell_contract::ipc::NativeSavedTarget, id: String) -> Self {
        Self {
            target,
            document: None,
            completion: None,
            id,
            selection: None,
            started: false,
            subscriptions: Vec::new(),
            session_id: None,
            recovery_applied: false,
            failure: None,
        }
    }
}

pub(super) struct SavedSelection {
    pub owner: shell_contract::ipc::NativeRequestOwner,
    pub cancel: shell_contract::runtime::CancelToken,
    pub reply: shell_contract::ipc::Responder,
}

/// `TabRecoveryState`
#[derive(Default)]
pub struct TabRecovery {
    /// copies this tab wrote, by copy path: a clean close removes only these
    pub written: Vec<(String, RecoveryIdentity)>,
    /// copies whose session closed while still unsaved
    pub keep: HashSet<String>,
    /// copies this tab must neither write nor delete
    pub held: HashSet<String>,
    /// "Don't Save" was answered: late copy writes are dropped
    pub discarded: bool,
    /// the renderer died
    pub crashed: bool,
    /// the password notice was shown in this tab
    pub notice_shown: bool,
}

/// `UnsavedNewWorkbook`
#[derive(Clone, Debug, PartialEq)]
pub struct UnsavedNew {
    pub suggest_save_as: String,
    pub temp_dir: String,
    pub recovery: RecoveryIdentity,
    pub restored: bool,
    pub original_path: Option<String>,
    pub ods_converted: bool,
    pub formulas_dropped: Option<f64>,
    pub ods_source_path: Option<String>,
}

pub struct PendingPick {
    pub pick_id: String,
    pub source: ViewId,
    pub target: ViewId,
}

/// `HeadlessSheetsReport`
#[derive(Clone, Debug, PartialEq)]
pub struct HeadlessReport {
    pub ok: bool,
    pub error: Option<String>,
}

pub struct CloseWaiter {
    pub token: u64,
    pub resume: Box<dyn FnOnce(bool)>,
}

#[derive(Clone)]
pub(super) struct HeadlessPrint {
    pub document: shell_contract::ipc::NativeInvocationDocument,
    pub cancel: shell_contract::runtime::CancelToken,
    pub pending: shell_contract::runtime::CancelToken,
}

#[derive(Clone)]
pub(super) struct GeneratedTransition {
    inner: Arc<GeneratedTransitionInner>,
}

struct GeneratedTransitionInner {
    retired_origin: std::sync::atomic::AtomicBool,
    receipt: Mutex<Option<Arc<shell_contract::hosts::CommittedGeneratedPath>>>,
    cancellation: Mutex<Option<shell_contract::ipc::AcceptedOwnerLosses>>,
    subscriptions: Mutex<Vec<shell_contract::runtime::CancelSubscription>>,
    loss: Arc<dyn Fn() + Send + Sync>,
}

impl GeneratedTransition {
    pub fn new(loss: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(GeneratedTransitionInner {
                retired_origin: std::sync::atomic::AtomicBool::new(false),
                receipt: Mutex::new(None),
                cancellation: Mutex::new(None),
                subscriptions: Mutex::new(Vec::new()),
                loss: Arc::new(loss),
            }),
        }
    }

    pub fn accept(
        &self,
        marker: shell_contract::hosts::AcceptedPdfReload,
        origin: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) {
        if !marker.retires(origin) {
            return;
        }
        let losses = match origin.accepted_reload_losses(&marker, cancel, pending) {
            Ok(losses) => losses,
            Err(_) => {
                (self.inner.loss)();
                return;
            }
        };
        let mut subscriptions = self
            .inner
            .subscriptions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for token in losses.tokens() {
            let loss = self.inner.loss.clone();
            subscriptions.push(token.on_cancel(move || loss()));
        }
        *self
            .inner
            .cancellation
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(losses);
        self.inner
            .retired_origin
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn retired_origin(&self) -> bool {
        self.inner
            .retired_origin
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn expected_dispatcher_cancel(
        &self,
        original: &shell_contract::runtime::CancelToken,
    ) -> bool {
        self.inner
            .cancellation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|losses| {
                !losses.cancel().same_token(original) && !losses.cancel().is_cancelled()
            })
    }

    pub fn store(&self, receipt: Arc<shell_contract::hosts::CommittedGeneratedPath>) {
        *self
            .inner
            .receipt
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(receipt);
    }

    pub fn has_receipt(&self) -> bool {
        self.inner
            .receipt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }

    pub fn terminal(&self, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        self.inner
            .receipt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .ok_or_else(|| "The generated document did not finish opening".to_owned())?
            .with_terminal_commit(effect)
    }
}

pub struct OwnedSaveWaiter {
    pub scope: crate::owned_save::OwnedSave,
    pub target: Option<String>,
    pub then: shell_contract::editors::Then<bool>,
    pub session_id: String,
    pub in_flight: bool,
    pub writes: u8,
    pub export_target: Option<(String, String)>,
    pub require_write: bool,
    pub failure: Option<IpcError>,
    pub subscriptions: Vec<shell_contract::runtime::CancelSubscription>,
    pub(super) transition: GeneratedTransition,
}

pub struct OwnedCloseWaiter {
    pub scope: crate::owned_save::OwnedSave,
    pub then: shell_contract::editors::Then<bool>,
    pub subscriptions: Vec<shell_contract::runtime::CancelSubscription>,
}

pub(super) struct RecoveryOpenWaiter {
    pub view: ViewId,
    pub done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    pub subscriptions: Vec<shell_contract::runtime::CancelSubscription>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueOutcome {
    Pending,
    Consumed,
    Retired,
}

pub struct QueuedWorkbook {
    pub(super) path: String,
    pub(super) identity: Rc<std::cell::Cell<QueueOutcome>>,
}

impl QueuedWorkbook {
    pub(super) fn new(path: String) -> Self {
        Self {
            path,
            identity: Rc::new(std::cell::Cell::new(QueueOutcome::Pending)),
        }
    }

    pub(super) fn consume(self) -> String {
        self.identity.set(QueueOutcome::Consumed);
        self.path
    }
}

pub(super) struct PendingBlank {
    pub identity: Rc<std::cell::Cell<QueueOutcome>>,
    pub target: Option<(ViewId, shell_contract::runtime::RendererGeneration)>,
}

impl PendingBlank {
    pub fn new(target: Option<(ViewId, shell_contract::runtime::RendererGeneration)>) -> Self {
        Self {
            identity: Rc::new(std::cell::Cell::new(QueueOutcome::Pending)),
            target,
        }
    }
}

/// Every module-level `Map`, `Set` and variable of the TypeScript, per app
pub struct AppState {
    pub tabs: Vec<(ViewId, TabState)>,
    pub active_view: Option<ViewId>,
    pub pending_imports: ImportRegistry,
    pub pending_pick: Option<PendingPick>,
    /// running delimited imports by token: the controller the registry holds
    pub import_controllers: HashMap<u64, main_core::sheets::import_registry::AbortController>,
    pub save_as_export_targets: SaveAsExportTargets,
    /// the native destination a Save As dialog picked for a copy format, by the
    /// view and the path the renderer will export to
    pub native_export_destinations:
        HashMap<(ViewId, String), shell_contract::ipc::NativeSaveDestination>,
    pub untitled_workbook_paths: HashSet<String>,
    pub unsaved_new_workbooks: HashMap<String, UnsavedNew>,
    pub mcp_writable: HashMap<ViewId, HashSet<String>>,
    pub queued_workbooks: HashMap<ViewId, QueuedWorkbook>,
    pub workbook_nudge_generation: u64,
    pub(super) pending_new_blank: Option<PendingBlank>,
    pub(super) blank_revision: Rc<()>,
    pub headless_targets: HashMap<ViewId, String>,
    pub headless_waiters: HashMap<ViewId, Box<dyn FnOnce(HeadlessReport)>>,
    pub pending_edit_counts: HashMap<ViewId, f64>,
    pub tracked_edit_senders: HashSet<ViewId>,
    pub close_waiters: HashMap<ViewId, CloseWaiter>,
    /// the continuation of a `request_save(SaveAs)` by view: resumed by that
    /// view's next `workbook:save` of mode `save-as`
    pub save_as_waiters: HashMap<ViewId, CloseWaiter>,
    pub path_save_waiters: HashMap<u64, (ViewId, shell_contract::editors::Then<()>)>,
    pub owned_saves: HashMap<String, OwnedSaveWaiter>,
    pub owned_closes: HashMap<u64, OwnedCloseWaiter>,
    pub(super) recovery_open_waiters: HashMap<String, RecoveryOpenWaiter>,
    pub(super) generated_publications: HashMap<String, super::save::PendingGeneratedPublication>,
    pub next_token: u64,
    pub shutdown: ShutdownLatch,
    pub tab_recovery: HashMap<ViewId, TabRecovery>,
    pub decided_opens: HashMap<ViewId, (String, RecoveryOpenDecision)>,
    #[allow(clippy::type_complexity)]
    pub write_now_waiters: HashMap<
        String,
        (
            ViewId,
            Box<dyn FnOnce(Result<(), main_core::shell::renderer_disposal::Failure>)>,
        ),
    >,
    pub attachment_text_cache: Vec<(String, (String, String))>,
    pub pasted_image_seq: u64,
    pub pasted_cleanup_started: bool,
}

impl AppState {
    fn new() -> AppState {
        AppState {
            tabs: Vec::new(),
            active_view: None,
            pending_imports: ImportRegistry::new(),
            pending_pick: None,
            import_controllers: HashMap::new(),
            save_as_export_targets: SaveAsExportTargets::new(),
            native_export_destinations: HashMap::new(),
            untitled_workbook_paths: HashSet::new(),
            unsaved_new_workbooks: HashMap::new(),
            mcp_writable: HashMap::new(),
            queued_workbooks: HashMap::new(),
            workbook_nudge_generation: 0,
            pending_new_blank: None,
            blank_revision: Rc::new(()),
            headless_targets: HashMap::new(),
            headless_waiters: HashMap::new(),
            pending_edit_counts: HashMap::new(),
            tracked_edit_senders: HashSet::new(),
            close_waiters: HashMap::new(),
            save_as_waiters: HashMap::new(),
            path_save_waiters: HashMap::new(),
            owned_saves: HashMap::new(),
            owned_closes: HashMap::new(),
            recovery_open_waiters: HashMap::new(),
            generated_publications: HashMap::new(),
            next_token: 0,
            shutdown: ShutdownLatch::new(),
            tab_recovery: HashMap::new(),
            decided_opens: HashMap::new(),
            write_now_waiters: HashMap::new(),
            attachment_text_cache: Vec::new(),
            pasted_image_seq: 0,
            pasted_cleanup_started: false,
        }
    }

    pub fn tab(&self, view: ViewId) -> Option<&TabState> {
        self.tabs
            .iter()
            .find(|(id, _)| *id == view)
            .map(|(_, tab)| tab)
    }

    pub fn tab_mut(&mut self, view: ViewId) -> Option<&mut TabState> {
        self.tabs
            .iter_mut()
            .find(|(id, _)| *id == view)
            .map(|(_, tab)| tab)
    }

    pub(super) fn replace_queued_workbook(
        &mut self,
        view: ViewId,
        entry: QueuedWorkbook,
    ) -> Option<QueuedWorkbook> {
        if let Some(tab) = self.tab_mut(view) {
            tab.queue_revision = Rc::new(());
        }
        self.queued_workbooks.insert(view, entry)
    }
    pub(super) fn set_queued_workbook_prepared(
        &mut self,
        view: ViewId,
        entry: Option<QueuedWorkbook>,
        revision: Rc<()>,
    ) -> Option<QueuedWorkbook> {
        if let Some(tab) = self.tab_mut(view) {
            tab.queue_revision = revision;
        }
        match entry {
            Some(entry) => self.queued_workbooks.insert(view, entry),
            None => self.queued_workbooks.remove(&view),
        }
    }

    pub(super) fn remove_queued_workbook(&mut self, view: ViewId) -> Option<QueuedWorkbook> {
        let removed = self.queued_workbooks.remove(&view);
        if removed.is_some() && let Some(tab) = self.tab_mut(view) {
            tab.queue_revision = Rc::new(());
        }
        removed
    }

    pub fn recovery_of(&mut self, view: ViewId) -> &mut TabRecovery {
        self.tab_recovery.entry(view).or_default()
    }
}

/// What a worker may hold: every host and the way back to the core thread
#[derive(Clone)]
pub struct Env {
    pub services: Services,
    pub slot: usize,
    pub engine: WorkbookEngineHost,
    pub shell: SheetsShellHost,
    pub screen: ScreenCaptureHost,
    pub dialogs: DialogsHost,
    pub os_shell: shell_contract::OsShellHost,
    pub clipboard: ClipboardHost,
    pub main_window: shell_contract::MainWindowHost,
    pub shared: Arc<Shared>,
    pub recovery: RecoveryStateHost,
    pub tm: Tm,
}

/// What a core-thread closure works on
#[derive(Clone)]
pub struct Core {
    pub env: Env,
    pub state: Rc<RefCell<AppState>>,
}

thread_local! {
    /// the core thread's [`Core`]s, one per `routes` call; a worker names one by slot
    static CORES: RefCell<Vec<Core>> = const { RefCell::new(Vec::new()) };
}

/// Creates the state and registers it on this (the core) thread; `env.slot` is set here
pub fn create_core(mut env: Env) -> Core {
    CORES.with(|cores| {
        let mut cores = cores.borrow_mut();
        env.slot = cores.len();
        let core = Core {
            env,
            state: Rc::new(RefCell::new(AppState::new())),
        };
        cores.push(core.clone());
        core
    })
}

/// The core registered under `slot`; call on the core thread only
pub fn core_at(slot: usize) -> Core {
    CORES.with(|cores| cores.borrow()[slot].clone())
}

impl Env {
    /// Runs `job` on the core thread and waits for its answer. Call from a
    /// worker (or, in tests, where the core thread runs jobs inline); never
    /// from a core-thread job in the shell, which would wait for itself.
    pub fn core<R: Send + 'static>(&self, job: impl FnOnce(&Core) -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::sync_channel(1);
        let slot = self.slot;
        self.services.core.post(Box::new(move || {
            let answer = job(&core_at(slot));
            let _ = tx.send(answer);
        }));
        rx.recv().expect("the core thread dropped a job")
    }

    /// `job` on the core thread, without waiting
    pub fn post(&self, job: impl FnOnce(&Core) + Send + 'static) {
        let slot = self.slot;
        self.services
            .core
            .post(Box::new(move || job(&core_at(slot))));
    }

    /// `job` on a worker
    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        self.services.workers.spawn(Box::new(job));
    }

    /// Waits for the asynchronous cross-editor guard. Worker threads only.
    pub fn assert_file_saved(&self, path: &str) -> Result<(), String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.shell.assert_file_saved(
            path.to_owned(),
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        );
        rx.recv()
            .unwrap_or_else(|_| Err("The file save check did not complete.".into()))
    }

    pub fn window_of(&self, view: ViewId) -> Option<WindowId> {
        self.services.views.sender_of(view).map(|s| s.window)
    }

    /// `!webContents.isDestroyed()`
    pub fn alive(&self, view: ViewId) -> bool {
        self.services.views.sender_of(view).is_some()
    }

    /// `webContents.send(channel, ...args)`
    pub fn send(&self, view: ViewId, channel: &'static str, args: Vec<IpcValue>) {
        self.services.events.emit(Target::View(view), channel, args);
    }

    /// `app.getPath('temp')`
    /// `randomUUID()`
    pub fn uuid(&self) -> String {
        self.shell.random_uuid()
    }

    pub fn temp_dir(&self) -> String {
        crate::host::path_string(self.services.paths.path(PathName::Temp))
    }

    pub fn user_data_dir(&self) -> String {
        crate::host::path_string(self.services.paths.path(PathName::UserData))
    }

    /// Runs `f` with the recovery store over the one shared state. The store
    /// is only ever used from short blocking calls, so the lock is brief.
    pub fn with_store<R>(
        &self,
        f: impl FnOnce(&main_core::packages::electron_utils::recovery_store::RecoveryStore<'_>) -> R,
    ) -> R {
        let shared: std::sync::MutexGuard<'_, SharedRecoveryState> = lock(&self.recovery);
        let user_data = self.user_data_dir();
        let user_data_dir = move || user_data.clone();
        let shell = self.shell.clone();
        let now = move || shell.now_ms();
        let uuid = jscompat::random_uuid;
        let store = create_recovery_store(
            RecoveryStoreOptions {
                user_data_dir: &user_data_dir,
                now_ms: &now,
                random_uuid: &uuid,
            },
            &shared,
        );
        f(&store)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Core {
    pub(super) fn validate_headless_sender(
        &self,
        cx: &shell_contract::ipc::CallContext,
    ) -> Result<(), IpcError> {
        let state = self.state.borrow();
        let Some(target) = state
            .tab(cx.sender.view)
            .and_then(|tab| tab.headless_target.as_ref())
        else {
            return Ok(());
        };
        let expected = target.created();
        let actual = cx.native.as_ref().and_then(|context| context.renderer());
        if !expected.is_active()
            || target.operation().shutdown().is_cancelled()
            || target.operation().cancel().is_cancelled()
            || !actual.is_some_and(|actual| actual.same_generation(expected))
        {
            return Err(thrown("The headless workbook renderer was replaced"));
        }
        Ok(())
    }

    pub(super) fn begin_headless_work(
        &self,
        cx: &shell_contract::ipc::CallContext,
    ) -> Result<Option<HeadlessWorkGuard>, IpcError> {
        self.validate_headless_sender(cx)?;
        let state = self.state.borrow();
        match state.tab(cx.sender.view) {
            Some(tab) if tab.headless_target.is_some() => tab
                .headless_work
                .as_ref()
                .ok_or_else(|| thrown("The headless workbook work tracker is unavailable"))?
                .begin()
                .map(Some),
            _ => Ok(None),
        }
    }

    pub(super) fn headless_effect<T>(
        &self,
        cx: &shell_contract::ipc::CallContext,
        effect: impl FnOnce() -> T,
    ) -> Result<T, IpcError> {
        let target = self
            .state
            .borrow()
            .tab(cx.sender.view)
            .and_then(|tab| tab.headless_target.clone());
        let Some(target) = target else {
            return Ok(effect());
        };
        let document = target.registered_document().map_err(thrown)?;
        let context = cx
            .native
            .clone()
            .ok_or_else(|| thrown("The headless sender has no native authority"))?;
        let sender = shell_contract::ipc::NativeRequestOwner {
            view: cx.sender.view,
            context,
        };
        let effects = self
            .env
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .ok_or_else(|| thrown("Native invocation effects are unavailable"))?;
        let pending = shell_contract::runtime::CancelToken::default();
        let mut effect = Some(effect);
        let mut output = None;
        effects
            .with_native_invocation_effect(
                &document,
                &sender,
                &[],
                &cx.cancel,
                &pending,
                &mut || {
                    output = Some(effect
                        .take()
                        .ok_or("The headless effect was already admitted")?(
                    ));
                    Ok(())
                },
            )
            .map_err(thrown)?;
        output.ok_or_else(|| thrown("The headless effect was not admitted"))
    }

    /// `sessionFor`: the tab `register_tab` made for `view`, or the thrown
    /// `Untrusted IPC sender.` when the shell never registered the view
    pub fn require_tab(&self, view: ViewId) -> Result<(), IpcError> {
        if self.state.borrow().tab(view).is_some() {
            Ok(())
        } else {
            Err(thrown("Untrusted IPC sender."))
        }
    }

    pub fn session_of(&self, view: ViewId, id: &str) -> Option<SessionInfo> {
        self.state
            .borrow()
            .tab(view)
            .and_then(|tab| tab.session(id).cloned())
    }

    pub fn tab_views(&self) -> Vec<ViewId> {
        self.state.borrow().tabs.iter().map(|(id, _)| *id).collect()
    }

    pub fn send(&self, view: ViewId, channel: &'static str, args: Vec<IpcValue>) {
        self.env.send(view, channel, args);
    }
}

#[cfg(test)]
mod generated_transition_tests {
    use super::*;
    use shell_contract::ipc::{NativeContext, NativeRequestOwner, PdfDocumentLease};
    use shell_contract::runtime::{CancelToken, RendererGeneration};

    #[test]
    fn accepted_lineage_without_native_provenance_fails_closed() {
        let renderer = RendererGeneration::new();
        let origin = NativeRequestOwner {
            view: ViewId(7),
            context: NativeContext::with_owner_loss((), renderer.clone(), renderer.loss_token())
                .with_pdf_document(PdfDocumentLease::new())
                .with_pdf_revision(PdfDocumentLease::new()),
        };
        let other = NativeRequestOwner {
            view: ViewId(8),
            context: NativeContext::with_owner_loss((), renderer.clone(), renderer.loss_token())
                .with_pdf_document(PdfDocumentLease::new())
                .with_pdf_revision(PdfDocumentLease::new()),
        };
        let unrelated = GeneratedTransition::new(|| {});
        let other_marker = shell_contract::hosts::AcceptedPdfReload::from_native_admission(
            other,
            RendererGeneration::new(),
            CancelToken::default(),
            CancelToken::default(),
            Arc::new(|_| true),
        );
        unrelated.accept(
            other_marker,
            &origin,
            &CancelToken::default(),
            &CancelToken::default(),
        );
        assert!(!unrelated.retired_origin());
        let browser_loss = CancelToken::default();
        let reservation_loss = CancelToken::default();
        let cancelled = CancelToken::default();
        let notify = cancelled.clone();
        let transition = GeneratedTransition::new(move || notify.cancel());
        let marker = shell_contract::hosts::AcceptedPdfReload::from_native_admission(
            origin.clone(),
            RendererGeneration::new(),
            browser_loss.clone(),
            reservation_loss,
            Arc::new(|_| true),
        );
        transition.accept(
            marker,
            &origin,
            &CancelToken::default(),
            &CancelToken::default(),
        );
        assert!(!transition.retired_origin());
        assert!(cancelled.is_cancelled());
    }

    #[test]
    fn workbook_metadata_revision_never_reuses_an_overflowed_value() {
        let mut tab = TabState::new();
        tab.set_session("one", SessionInfo::default());
        assert_eq!(tab.metadata_revision, Some(1));
        tab.set_current_path(Some("one.xlsx".into()));
        assert_eq!(tab.metadata_revision, Some(2));
        tab.metadata_revision = Some(u64::MAX);
        tab.remove_session("one");
        assert_eq!(tab.metadata_revision, None);
        tab.set_current_path(None);
        assert_eq!(tab.metadata_revision, None);
    }
}
