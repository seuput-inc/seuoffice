//! What the tabs and windows area needs from the shell, in two traits.
//!
//! - [`WindowsHost`] is the platform: windows, views, the screen, timers and
//!   native menus. It is `Send + Sync`, held in [`shell_contract::Hosts`], and
//!   `crates/shell` implements it with Tauri. Every method is one call of
//!   Electron's `BrowserWindow`, `WebContentsView`, `webContents`, `screen`,
//!   `Menu` or a timer; the TypeScript's `await` never appears: a method that
//!   waited in TypeScript (`Menu.popup`, `pageLoaded`, a timer) answers later
//!   through [`HostEvents`].
//! - [`TabHooks`] is the application: the editor modules the tab manager calls
//!   (making a view, asking whether it is dirty, queueing a seed), the shell's
//!   strings and the side effects of a changed tab list. It lives on the core
//!   thread (`Rc`, no `Send`), because the document batches (D1, D2) and the
//!   menu batch (M) keep their state there. The shell adds one to
//!   [`shell_contract::Provided`] before it registers the batches.
//!
//! Neither trait blocks the core thread on a renderer: see `docs/seuoffice`
//! (the stage 3 shell contract), section 3. Calls that return a value
//! (`window_state`, `create_window`) run on the UI thread through `run_on`.

use std::sync::Arc;

use main_core::shell::detached_windows::{RecId, WindowOptions};
use main_core::shell::split_layout::Rect;
use main_core::shell::tab_manager::{
    Deferred, LoadedTabView, Opener, SplitViewState, TabKind, TabResult, UnloadedTabOpenContext,
    ViewSpec,
};
use serde_json::Value;
use shell_contract::editors::Then;
use shell_contract::ids::{ViewId, WindowId};

pub trait NativeBrowserCloseProducer: Send + Sync {
    fn watch(
        &self,
        done: Box<dyn FnOnce(shell_contract::ipc::NativeClosureReceipt) + Send>,
    ) -> Result<(), String>;
    fn dispatch_close(
        &self,
        permit: shell_contract::ipc::NativeBrowserClosePermit,
    ) -> Result<
        shell_contract::ipc::NativeBrowserClosePermit,
        (String, shell_contract::ipc::NativeBrowserClosePermit),
    >;
}

#[derive(Clone)]
pub enum NativeBrowserRegistration {
    Unregistered,
    Registered(shell_contract::ipc::NativeRequestOwner),
}

#[derive(Clone)]
pub struct NativeBrowserBinding {
    pub reservation: shell_contract::ipc::NativeClosureReservation,
    pub owner: shell_contract::ipc::NativeBrowserOwner,
    pub producer: std::sync::Arc<dyn NativeBrowserCloseProducer>,
    pub closure: main_core::shell::tab_manager::ViewClosureWatch<
        shell_contract::ipc::NativeBrowserRetirement,
    >,
    pub registration: NativeBrowserRegistration,
}

pub trait SpareActor {
    fn prepare_close(&self) -> Result<Box<dyn SpareClosePlan + '_>, String>;
    fn prepare_transfer(&self) -> Result<Box<dyn SpareTransferPlan>, String>;
    fn prepare_saved_barrier(
        &self,
        target: &shell_contract::ipc::NativeSavedTarget,
    ) -> Result<Box<dyn SpareSavedBarrier>, String>;
}

pub trait SpareClosePlan {
    fn with_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
}

pub trait SpareTransferPlan {
    fn successor(&self) -> std::rc::Rc<dyn SpareActor>;
    fn successor_receipt(&self) -> std::rc::Rc<dyn EditorSeedRollback>;
    fn with_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        successor: &shell_contract::ipc::NativeBrowserOwner,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
}
pub trait SpareSavedBarrier {
    fn with_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
}
pub trait SpareSeedPlan {
    fn with_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        successor: &shell_contract::ipc::NativeBrowserOwner,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn after_commit(&mut self, cancel: &shell_contract::runtime::CancelToken)
    -> Result<(), String>;
}

pub type SpareActorSlot = std::rc::Rc<std::cell::RefCell<Option<std::rc::Rc<dyn SpareActor>>>>;

#[derive(Clone)]
pub struct RetainedSpareBinding {
    pub browser: NativeBrowserBinding,
    pub rollback: EditorSeedRollbackStore,
    pub spare_actor: Option<std::rc::Rc<dyn SpareActor>>,
    identity: std::rc::Rc<()>,
}

impl RetainedSpareBinding {
    pub fn new(
        browser: NativeBrowserBinding,
        rollback: EditorSeedRollbackStore,
        spare_actor: Option<std::rc::Rc<dyn SpareActor>>,
    ) -> Self {
        Self {
            browser,
            rollback,
            spare_actor,
            identity: std::rc::Rc::new(()),
        }
    }
}

impl PartialEq for RetainedSpareBinding {
    fn eq(&self, other: &Self) -> bool {
        std::rc::Rc::ptr_eq(&self.identity, &other.identity)
    }
}

impl std::fmt::Debug for RetainedSpareBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RetainedSpareBinding([redacted])")
    }
}
pub type ViewCreationResult<V> = main_core::shell::tab_manager::ViewCreationResult<
    V,
    shell_contract::ipc::NativeBrowserRetirement,
    NativeBrowserBinding,
>;
pub type ViewCreationReady<V> = main_core::shell::tab_manager::ViewCreationReady<
    V,
    shell_contract::ipc::NativeBrowserRetirement,
    NativeBrowserBinding,
>;
pub type ViewCreationFailure = main_core::shell::tab_manager::ViewCreationFailure<
    shell_contract::ipc::NativeBrowserRetirement,
>;
pub type ViewCreationCleanup = main_core::shell::tab_manager::ViewCreationCleanup<
    shell_contract::ipc::NativeBrowserRetirement,
>;
pub type ViewCreationCleanupState = main_core::shell::tab_manager::ViewCreationCleanupState<
    shell_contract::ipc::NativeBrowserRetirement,
>;
pub struct ViewClosureBinding {
    pub reservation: shell_contract::ipc::NativeClosureReservation,
    pub owner: shell_contract::ipc::NativeBrowserOwner,
    pub producer: std::sync::Arc<dyn NativeBrowserCloseProducer>,
    pub closure: main_core::shell::tab_manager::ViewClosureWatch<
        shell_contract::ipc::NativeBrowserRetirement,
    >,
    pub registration: NativeBrowserRegistration,
}

/// `BrowserWindow`'s state as the tab code reads it, taken in one call
#[derive(Clone, Debug, PartialEq)]
pub struct WindowState {
    pub destroyed: bool,
    pub minimized: bool,
    pub focused: bool,
    pub visible: bool,
    /// `getContentBounds()`: screen coordinates
    pub content_bounds: Rect,
    /// `getPosition()`
    pub position: (f64, f64),
    /// `getTitle()`
    pub title: String,
}

/// The shell window and its own view (the strip's renderer)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellWindow {
    pub window: WindowId,
    pub view: ViewId,
}

/// An event of a view the manager listens to (`view_listen`)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewListen {
    /// `enter-html-full-screen` and `leave-html-full-screen`, both
    HtmlFullScreen,
    /// `focus`
    PaneFocus,
    /// `render-process-gone`, once
    RenderProcessGone,
}

/// What a window reports
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowEvent {
    Resize,
    Restore,
    Focus,
    /// `move`
    Move,
    /// `moved`, which only Windows emits
    Moved,
    /// `close`: the window waits for [`HostEvents::answer_close`] before it
    /// goes away (Electron's `event.preventDefault()` then `destroy()`)
    Close,
    Closed,
}

/// A view event as it arrives
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewHappened {
    EnterHtmlFullScreen,
    LeaveHtmlFullScreen,
    Focus,
    RenderProcessGone,
}

/// A timer or a deferred step the core thread asked for
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TimerKey {
    /// the spare sheets view's timer
    SpareSheets,
    /// a detached window's dock dwell
    Dock(RecId),
    /// `setImmediate`: the tab manager's next-tick layout
    Tab(Deferred),
    /// `setImmediate`: a detached window's next-tick layout
    DetachedLayout(RecId),
    /// the debounced split memory write
    SplitMemory,
}

/// Everything the host reports back, always on the core thread
#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent {
    View(ViewId, ViewHappened),
    /// `pageLoaded` settled: `true` on `did-finish-load`, `false` on `destroyed`
    PageLoad {
        token: u64,
        loaded: bool,
    },
    Window(WindowId, WindowEvent),
    Timer(TimerKey),
    /// a popup menu closed: the chosen entry's `id`, `None` when dismissed
    Menu {
        token: u64,
        chosen: Option<u32>,
    },
}

/// Where the host sends [`HostEvent`]s. Callable from any thread; the event
/// is run on the core thread, in the order it was sent.
pub trait HostEvents: Send + Sync {
    fn send(&self, event: HostEvent);
}

pub trait EditorSeedRollback {
    fn retire_closed(
        &self,
        receipt: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    fn retire_adopted(
        &self,
        receipt: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>);
}

pub type EditorSeedRollbackStore =
    std::rc::Rc<std::cell::RefCell<Vec<std::rc::Rc<dyn EditorSeedRollback>>>>;

pub struct SavedViewLoaded {
    pub document: shell_contract::ipc::NativeSavedDocument,
    pub outcome: shell_contract::editors::SavedDocumentLoadOutcome,
}

/// Validate the manager, then commit it inside the validated actor transition.
/// Both publications occur in the same saved/browser admission.
pub type SavedManagerAdoption = Box<
    dyn FnOnce(
        &shell_contract::ipc::NativeSavedTarget,
        &shell_contract::ipc::NativeBrowserTransition<'_>,
        &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
    ) -> Result<(), String>,
>;

pub type SavedManagerRegistration = Box<
    dyn FnOnce(
        &shell_contract::ipc::NativeSavedTarget,
        &shell_contract::ipc::NativeRequestOwner,
        &shell_contract::ipc::NativeBrowserTransition<'_>,
        &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>,
>;

pub struct PreparedSavedView {
    pub target: shell_contract::ipc::NativeSavedTarget,
    pub binding: NativeBrowserBinding,
    #[allow(clippy::type_complexity)]
    pub start: Box<
        dyn FnOnce(
            SavedManagerAdoption,
            SavedManagerRegistration,
            Box<dyn FnOnce(ViewCreationResult<SavedViewLoaded>)>,
        ),
    >,
}

/// Native shell window and lifecycle captured before Drive or file I/O.
pub trait CapturedSavedShell {
    #[allow(clippy::too_many_arguments)]
    fn prepare_fresh(
        &self,
        spec: &ViewSpec<'_>,
        input: shell_contract::ipc::NativeSavedInput,
        path: shell_contract::ipc::NativeSavedPath,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        rollback: EditorSeedRollbackStore,
    ) -> TabResult<PreparedSavedView>;
}

pub enum GeneratedRetryChoice {
    Retry,
    Cancel,
}

/// A modal parent and request authority captured before the Drive transaction.
pub trait CapturedGeneratedRetryPrompt {
    fn show(&self, error: String, done: Box<dyn FnOnce(Result<GeneratedRetryChoice, String>)>);
}

pub type EditorInitializer = Box<
    dyn FnOnce(
        ViewId,
        &shell_contract::runtime::RendererGeneration,
        &shell_contract::ipc::NativeClosureReservation,
        &shell_contract::ipc::NativeBrowserOwner,
    ) -> TabResult<()>,
>;
pub type GeneratedPrecloseBarrier =
    std::sync::Arc<dyn Fn(Box<dyn FnOnce(Result<(), String>) + Send>) + Send + Sync>;
pub type GeneratedPrecloseRetry = std::sync::Arc<dyn Fn() + Send + Sync>;
pub type GeneratedPrecloseFailure = std::sync::Arc<dyn Fn(String) + Send + Sync>;
pub type GeneratedPrecloseInstaller = Box<
    dyn FnOnce(
        GeneratedPrecloseBarrier,
        GeneratedPrecloseFailure,
    ) -> Result<GeneratedPrecloseRetry, String>,
>;
pub type GeneratedManagerAdoption = Box<
    dyn FnOnce(
        &shell_contract::ipc::NativeBrowserTransition<'_>,
        &shell_contract::ipc::NativeBrowserOwner,
        &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
    ) -> Result<(), String>,
>;
pub type GeneratedManagerRegistration = Box<
    dyn FnOnce(
        &shell_contract::ipc::NativeRequestOwner,
        &shell_contract::ipc::NativeBrowserTransition<'_>,
        &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>,
>;

pub struct PreparedGeneratedView {
    pub view: ViewId,
    pub created: shell_contract::runtime::RendererGeneration,
    pub binding: NativeBrowserBinding,
    pub spare_actor: Option<std::rc::Rc<dyn SpareActor>>,
    pub installer: GeneratedPrecloseInstaller,
    #[allow(clippy::type_complexity)]
    pub start: Box<
        dyn FnOnce(
            GeneratedManagerAdoption,
            GeneratedManagerRegistration,
            ViewCreationCleanup,
            Box<dyn FnOnce(ViewCreationResult<GeneratedViewCommit>)>,
        ),
    >,
}

/// One entry of a native popup menu
#[derive(Clone, Debug, PartialEq)]
pub struct MenuEntry {
    /// what [`HostEvent::Menu`] answers; unique within one menu
    pub id: u32,
    pub kind: MenuEntryKind,
    pub label: String,
    pub enabled: bool,
    pub checked: bool,
    /// a name of the menu icon set: `docx`, `xlsx`, `pptx`, `pdf`, `md`,
    /// `html`, `mindmap`, `hwp`, `epub`, `diagram`, `home`
    pub icon: Option<&'static str>,
    /// a submenu's entries (the application menu's top-level menus)
    pub submenu: Vec<MenuEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEntryKind {
    Normal,
    Checkbox,
    Separator,
    /// a disabled label above a section (`type: 'header'`)
    Header,
}

/// The platform half. All ids are Electron's: `WindowId` is
/// `BrowserWindow.id`, `ViewId` is `webContents.id`.
pub trait WindowsHost: Send + Sync {
    /// Sets where the host reports events. Called once, before anything else.
    fn attach(&self, events: Arc<dyn HostEvents>);

    // ---- windows ----
    /// The shell window, `None` while there is none (macOS keeps running after
    /// its window closed)
    fn shell_window(&self) -> Option<ShellWindow>;
    /// `createShellWindow`'s window and its renderer view: the geometry
    /// restored from the settings, hidden until the content size is set
    fn create_shell_window(&self) -> ShellWindow;
    fn window_state(&self, window: WindowId) -> WindowState;
    /// `new BrowserWindow(options)` of a detached editor; the host reports
    /// the window's events for it from then on
    fn create_window(&self, options: &WindowOptions) -> super::window_snapshot::WindowReservation;
    fn window_restore(&self, window: WindowId);
    fn window_show(&self, window: WindowId);
    fn window_show_inactive(&self, window: WindowId);
    fn window_focus(&self, window: WindowId);
    /// `win.destroy()`: no `close` event; `Closed` follows
    fn window_destroy(&self, window: WindowId);
    fn window_set_title(&self, window: &super::window_snapshot::WindowReservation, title: &str);
    fn window_set_position(&self, window: WindowId, x: f64, y: f64);
    /// `win.setBounds(frame)`, after `unmaximize()` when the window is maximized
    /// and `setFullScreen(false)` when it is full screen
    fn window_set_frame(&self, window: WindowId, frame: &Rect);
    /// `screen.getDisplayMatching(win.getBounds()).workArea`
    fn window_work_area(&self, window: WindowId) -> Rect;
    /// `win.contentView.addChildView(view)`
    fn add_child_view(&self, window: WindowId, view: ViewId);
    fn remove_child_view(&self, window: WindowId, view: ViewId);

    // ---- views ----
    fn view_destroyed(&self, view: ViewId) -> bool;
    fn view_set_visible(&self, view: ViewId, visible: bool);
    fn view_set_bounds(&self, view: ViewId, bounds: &Rect);
    /// `view.webContents.focus()`
    fn view_focus(&self, view: ViewId);
    /// `view.webContents.close()`
    fn view_close(&self, view: ViewId);
    fn view_reload(&self, view: ViewId);
    fn view_load_url(&self, view: ViewId, url: &str);
    fn view_listen(&self, view: ViewId, listen: ViewListen);
    /// `pageLoaded(view.webContents)`: reports `HostEvent::PageLoad` with this token
    fn watch_page_load(&self, view: ViewId, token: u64);
    /// The manual's view before its page loads: sandboxed, `backgroundThrottling`
    /// off, window-open denied except allowlisted external links
    /// (`help_window_open` of main-core)
    fn create_help_view(&self, done: Box<dyn FnOnce(ViewCreationResult<ViewId>)>);
    fn create_help_native_command(
        &self,
        operation: shell_contract::ipc::NativeInvocationOperation,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(ViewCreationResult<shell_contract::ipc::NativeInvocationDocument>)>,
    );

    // ---- the screen ----
    fn cursor_point(&self) -> (f64, f64);
    /// `screen.getDisplayNearestPoint(at).workArea`
    fn display_work_area(&self, at: (f64, f64)) -> Rect;

    // ---- timers ----
    /// Reports `HostEvent::Timer(key)` after `delay_ms`; `0` is `setImmediate`
    fn set_timer(&self, key: TimerKey, delay_ms: f64);
    fn clear_timer(&self, key: TimerKey);

    // ---- menus ----
    /// `Menu.buildFromTemplate(entries).popup({ window, x, y })`. Returns at
    /// once; reports `HostEvent::Menu` with this token when the menu closes.
    fn popup_menu(
        &self,
        window: WindowId,
        entries: &[MenuEntry],
        at: Option<(f64, f64)>,
        token: u64,
    );
}

/// What the tab list tells the application each time it changed
#[derive(Clone, Debug, PartialEq)]
pub struct TabsChanged {
    /// `manager.openFilePaths()`
    pub open_paths: Vec<String>,
    /// `detachedFilePaths()`
    pub detached_paths: Vec<String>,
    /// `manager.list()` as the strip got it
    pub tabs: Vec<main_core::shell::tab_manager::TabSummary>,
}

/// Native construction, initial payload transfer and load all completed for this exact epoch.
pub struct GeneratedViewCommit {
    target: shell_contract::ipc::NativeRequestOwner,
    created: shell_contract::runtime::RendererGeneration,
}

impl GeneratedViewCommit {
    pub fn loaded(
        target: shell_contract::ipc::NativeRequestOwner,
        created: shell_contract::runtime::RendererGeneration,
    ) -> TabResult<Self> {
        if !target
            .context
            .renderer()
            .is_some_and(|renderer| renderer.same_generation(&created))
            || !created.is_active()
            || target.loss_token().is_none_or(|loss| loss.is_cancelled())
        {
            return Err(main_core::shell::tab_manager::TabError(
                "The loaded generated document has no matching native owner".into(),
            ));
        }
        Ok(Self { target, created })
    }
    pub fn view(&self) -> ViewId {
        self.target.view
    }
    pub fn into_target(self) -> shell_contract::ipc::NativeRequestOwner {
        self.target
    }
    pub fn created(&self) -> &shell_contract::runtime::RendererGeneration {
        &self.created
    }
}
/// The application half, on the core thread. Each method is one call of the
/// TypeScript into an editor module, a callback `createShellWindow` gave the
/// tab manager, or a function of the shell's entry module.
pub trait TabHooks {
    fn cleanup_lifetime(&self) -> shell_contract::runtime::CancelToken;
    fn register_generated_path_task(
        &self,
        task: std::sync::Arc<dyn shell_contract::hosts::GeneratedPathPending>,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String>;
    #[allow(clippy::too_many_arguments)]
    fn capture_generated_retry_prompt(
        &self,
        shell: ShellWindow,
        owner: shell_contract::ipc::NativeRequestOwner,
        output: shell_contract::ipc::SavedPublication,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
    ) -> Result<std::rc::Rc<dyn CapturedGeneratedRetryPrompt>, String>;
    fn capture_saved_shell(
        &self,
        shell: ShellWindow,
        owner: shell_contract::ipc::NativeRequestOwner,
    ) -> Result<std::rc::Rc<dyn CapturedSavedShell>, String>;
    // ---- the editor modules ----
    /// the `create*View` call of `spec` (`createDocsView(path)`, ...)
    fn create_view(&self, spec: &ViewSpec<'_>, done: Box<dyn FnOnce(ViewCreationResult<ViewId>)>);
    fn create_spare_view_owned(
        &self,
        startup: shell_contract::ipc::NativeInvocationOperation,
        shell: shell_contract::ipc::NativeRequestOwner,
        cleanup: shell_contract::runtime::CancelToken,
        rollback: EditorSeedRollbackStore,
        spare_actor: SpareActorSlot,
        done: Box<dyn FnOnce(ViewCreationResult<shell_contract::ipc::NativeInvocationDocument>)>,
    );
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::type_complexity)]
    fn prepare_view_native_command(
        &self,
        spec: &ViewSpec<'_>,
        operation: shell_contract::ipc::NativeInvocationOperation,
        rollback: EditorSeedRollbackStore,
        initialize: EditorInitializer,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
    ) -> TabResult<(
        shell_contract::runtime::RendererGeneration,
        Box<
            dyn FnOnce(
                Box<dyn FnOnce(ViewCreationResult<shell_contract::ipc::NativeInvocationDocument>)>,
            ),
        >,
    )>;
    fn prepare_activation_ui(
        &self,
        shell: ShellWindow,
        shell_owner: &shell_contract::ipc::NativeRequestOwner,
        target: Option<ViewId>,
        effects: &[main_core::shell::tab_manager::TabActivationEffect<
            ViewId,
            RetainedSpareBinding,
        >],
    ) -> Result<super::activation::PreparedTabUiBatch, String>;
    #[allow(clippy::too_many_arguments)]
    fn prepare_activation_publication_owned(
        &self,
        authority: super::activation::TabActivationAuthority,
        shell: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
        snapshot: super::activation::TabActivationPublicationSnapshot,
    ) -> Result<super::activation::PreparedTabActivationPublication, String>;
    #[allow(clippy::too_many_arguments)]
    fn prepare_docs_disposal_owned(
        &self,
        view: ViewId,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        target: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
    ) -> Result<std::rc::Rc<dyn super::activation::DocsDisposalOperation>, String>;
    /// Prepare core editor registration; the owned continuation admits and constructs on UI.
    #[allow(clippy::type_complexity)]
    fn prepare_view_owned(
        &self,
        spec: &ViewSpec<'_>,
        rollback: EditorSeedRollbackStore,
        initialize: EditorInitializer,
    ) -> TabResult<(
        shell_contract::runtime::RendererGeneration,
        Box<
            dyn FnOnce(
                shell_contract::ipc::NativeRequestOwner,
                shell_contract::runtime::CancelToken,
                shell_contract::runtime::CancelToken,
                Box<dyn FnOnce(ViewCreationResult<shell_contract::ipc::NativeRequestOwner>)>,
            ),
        >,
    )>;
    #[allow(clippy::too_many_arguments)]
    fn prepare_generated_view_owned(
        &self,
        spec: &ViewSpec<'_>,
        source: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        rollback: EditorSeedRollbackStore,
    ) -> TabResult<PreparedGeneratedView>;
    #[allow(clippy::too_many_arguments)]
    fn prepare_initialized_spare_owned(
        &self,
        view: ViewId,
        expected: &shell_contract::runtime::RendererGeneration,
        binding: RetainedSpareBinding,
        source: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        rollback: EditorSeedRollbackStore,
    ) -> TabResult<PreparedGeneratedView>;
    #[allow(clippy::too_many_arguments)]
    fn commit_generated_seed_owned(
        &self,
        view: ViewId,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        placement: Option<Option<main_core::shell::projects::new_documents::NewDocumentPlacement>>,
        source: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        created: &shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        owner: &shell_contract::ipc::NativeBrowserOwner,
        rollback: &EditorSeedRollbackStore,
    ) -> TabResult<()>;
    #[allow(clippy::too_many_arguments)]
    fn prepare_spare_editor_seed(
        &self,
        view: ViewId,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        docs_open_path: Option<String>,
        workbook: Option<String>,
        new_blank: bool,
        target: &shell_contract::ipc::NativeRequestOwner,
        binding: &RetainedSpareBinding,
        rollback: &EditorSeedRollbackStore,
    ) -> TabResult<Box<dyn SpareSeedPlan>>;
    #[allow(clippy::too_many_arguments)]
    fn commit_editor_seed_owned(
        &self,
        view: ViewId,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        docs_open_path: Option<String>,
        workbook: Option<String>,
        reused_target: Option<&shell_contract::ipc::NativeRequestOwner>,
        source: &shell_contract::ipc::NativeRequestOwner,
        rollback: &EditorSeedRollbackStore,
        created: &shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        owner: &shell_contract::ipc::NativeBrowserOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> TabResult<()>;
    #[allow(clippy::too_many_arguments)]
    fn commit_native_editor_seed(
        &self,
        operation: &shell_contract::ipc::NativeInvocationOperation,
        expected: &shell_contract::ipc::NativeEditorCommand,
        view: ViewId,
        seed: Option<main_core::shell::tab_manager::EditorSeed>,
        docs_open_path: Option<String>,
        workbook: Option<String>,
        reused_target: Option<&shell_contract::ipc::NativeRequestOwner>,
        rollback: &EditorSeedRollbackStore,
        created: &shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        owner: &shell_contract::ipc::NativeBrowserOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> TabResult<()>;
    fn rollback_generated_seed_owned(
        &self,
        view: ViewId,
        expected: &shell_contract::runtime::RendererGeneration,
    );
    fn reveal_written_export_owned(
        &self,
        path: std::path::PathBuf,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    fn reveal_written_export_existing_document(
        &self,
        selection: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    fn reveal_written_publication(
        &self,
        output: shell_contract::ipc::SavedPublication,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    fn open_opener_owned(
        &self,
        opener: &Opener,
        context: &UnloadedTabOpenContext,
        source: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<super::activation::OwnedLoadedTabView, String>)>,
    );
    fn open_opener_startup(
        &self,
        opener: &Opener,
        context: &UnloadedTabOpenContext,
        constraint: super::activation::StartupRecoveryConstraint,
        done: Box<dyn FnOnce(Result<super::activation::OwnedLoadedTabView, String>)>,
    );
    fn open_opener_native_command(
        &self,
        opener: &Opener,
        context: &UnloadedTabOpenContext,
        command: shell_contract::ipc::NativeInvocationOperation,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<super::activation::OwnedLoadedTabView, String>)>,
    );
    fn opener_needs_user_owned(
        &self,
        opener: &Opener,
        context: &UnloadedTabOpenContext,
        source: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<bool, String>)>,
    );
    fn reveal_view_owned(
        &self,
        view: ViewId,
        expected: &shell_contract::runtime::RendererGeneration,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Box<dyn FnOnce(TabResult<()>)>,
    );
    fn reveal_captured_many(
        &self,
        authority: super::activation::TabActivationAuthority,
        target: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: super::activation::TabUiDone,
    );
    #[allow(clippy::too_many_arguments)]
    fn adopt_recovery_view_owned(
        &self,
        shell: ShellWindow,
        authority: super::activation::TabActivationAuthority,
        target: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: super::activation::TabUiDone,
    );
    /// Resolve Drive refresh without opening a tab or replacing the captured owner.
    #[allow(clippy::type_complexity)]
    fn prepare_reopen_owned(
        &self,
        path: String,
        owner: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Box<
            dyn FnOnce(
                Result<
                    (
                        String,
                        Option<String>,
                        shell_contract::ipc::NativeRequestOwner,
                        shell_contract::runtime::CancelToken,
                    ),
                    String,
                >,
            ),
        >,
    );
    fn prepare_reopen_existing_document(
        &self,
        output: shell_contract::ipc::SavedPublication,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        then: Box<dyn FnOnce(Result<shell_contract::ipc::NativeDriveReopen, String>)>,
    );
    /// Read the exact committed file object on a worker, not by its display path.
    #[allow(clippy::too_many_arguments)]
    fn read_saved_input(
        &self,
        output: shell_contract::ipc::SavedPublication,
        path: shell_contract::ipc::NativeSavedPath,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        then: Box<dyn FnOnce(Result<shell_contract::ipc::NativeSavedInput, String>)>,
    );
    fn sheets_pending_edit_count(&self, wc_id: i64) -> f64;
    /// `pdfIsDirty`, `markdownIsDirty`, ... by `kind`
    fn family_is_dirty(&self, kind: TabKind, wc_id: i64) -> TabResult<bool>;
    fn hwp_memo_modal_open(&self, wc_id: i64) -> bool;
    /// `docsQueryDirty(contents)`: asks the docs renderer; answers in `then`
    fn docs_query_dirty(&self, view: ViewId, then: Then<bool>);
    fn docs_query_dirty_owned(
        &self,
        view: ViewId,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    );
    #[allow(clippy::too_many_arguments)]
    fn docs_query_dirty_captured_many(
        &self,
        view: ViewId,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    );
    #[allow(clippy::too_many_arguments)]
    fn docs_request_close_captured_many(
        &self,
        view: ViewId,
        parent: Option<WindowId>,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    );
    fn with_request_owner(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), shell_contract::IpcError>,
    ) -> Result<(), shell_contract::IpcError>;
    fn set_active_docs_resolver(&self, view: Option<ViewId>, null_if_destroyed: bool);
    fn set_active_sheets_web_contents(&self, view: Option<ViewId>);
    fn set_active_slides_web_contents(&self, view: ViewId);
    /// `applyMenuFor(kind)`: the application menu of the active editor
    fn apply_menu_for(&self, kind: TabKind);
    fn teardown_docs_renderer(&self, view: ViewId);
    fn release_closed_renderer(&self, view: ViewId);
    fn mark_docs_new_blank(&self, wc_id: i64);
    fn queue_docs_ai_content(&self, wc_id: i64, content: &Value) -> TabResult<()>;
    fn queue_docs_copy(&self, wc_id: i64, path: &str, name: &str);
    fn set_sheets_new_blank(&self);
    fn queue_workbook_for_view(&self, view: ViewId, path: &str);
    fn nudge_queued_workbook(&self, view: ViewId);
    fn mark_pdf_untitled_copy(&self, path: &str);
    fn clear_pdf_dirty(&self, wc_id: i64);
    fn queue_markdown_seed_for(&self, view: ViewId, text: &str);
    fn queue_html_seed_for(&self, view: ViewId, text: &str);
    fn queue_mindmap_seed_for(&self, view: ViewId, bytes: &[u8]);
    fn queue_slides_seed_for(&self, view: ViewId, bytes: &[u8], suggested_name: &str);
    fn queue_generate_handoff_for(&self, view: ViewId, handoff: &Value);
    fn queue_converted_content(&self, view: ViewId, doc: &Value);
    fn drop_generate_handoff(&self, wc_id: i64);
    fn diagram_tab_identity(
        &self,
        wc_id: i64,
    ) -> Option<main_core::shell::tab_manager::DiagramIdentity>;
    /// `opener.open(context)` of an unloaded tab
    fn opener_open(
        &self,
        opener: &Opener,
        context: &UnloadedTabOpenContext,
    ) -> Result<LoadedTabView<ViewId>, String>;
    fn opener_needs_user(
        &self,
        opener: &Opener,
        context: &UnloadedTabOpenContext,
    ) -> Result<bool, String>;
    /// the Sheets close guard re-arms after a vetoed close
    fn reset_sheets_shutting_down(&self);
    fn cancel_update_restart(&self);
    fn resume_discarded_copies(&self);
    fn pdf_close_cancelled(&self, view: ViewId);

    // ---- what the shell tells the manager ----
    /// `currentUiScale`
    fn ui_scale(&self) -> f64;
    fn no_spare_view_env(&self) -> bool;
    /// `process.env.ELECTRON_RENDERER_URL`
    fn renderer_dev_url(&self) -> Option<String>;
    /// `tm(key)`: the shell's string in the current language
    fn tm(&self, key: &str) -> String;
    /// `tm(key, params)`
    fn tm_params(&self, key: &str, params: &[(&str, &str)]) -> String;
    /// `randomUUID()`
    fn random_uuid(&self) -> String;
    /// `console.warn(message, error)` / `console.error(message, error)`
    fn warn(&self, message: &str, error: &str);

    // ---- what a changed tab list does ----
    /// `onChanged`'s side effects beyond `tabs:changed`: publish the open
    /// documents, refresh the document link menu, push the HWP recovery
    /// titles, note the live session
    fn tabs_changed(&self, changed: &TabsChanged);
    #[allow(clippy::too_many_arguments)]
    fn prepare_saved_publication_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<super::owned_saved::SavedPublication, String>;
    fn prepare_saved_publication_recovery(
        &self,
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<super::owned_saved::SavedPublication, String>;
    fn prepare_saved_publication_existing_document(
        &self,
        destination: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<super::owned_saved::SavedPublication, String>;
    fn prepare_saved_publication_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<super::owned_saved::SavedPublication, String>;
    /// `handleSplitChanged`'s `noteLiveSession()`
    fn split_changed(&self, state: Option<&SplitViewState>);
    /// `setDetachedChangedListener`'s callback
    fn detached_changed(&self);

    // ---- menus ----
    /// The window menus of `Menu.getApplicationMenu()` as the ☰ popup lists
    /// them (`appMenuPopupTemplate`), `None` while there is no application menu.
    /// Each entry's `id` is what [`TabHooks::app_menu_activate`] takes.
    fn app_menu_popup(&self) -> Option<Vec<MenuEntry>>;
    /// A click on one of those entries: runs the menu item's own `click`
    fn app_menu_activate(&self, id: u32, cx: shell_contract::ipc::CallContext);
    /// `visibleHomeKinds(readHomeKinds())` without `pdfTools`: `docx`, `xlsx`,
    /// `pptx`, `md`, `html`, `xmind`, `drawio`, `hwp`, `pdf`, in home's order
    fn visible_home_kinds(&self) -> Vec<String>;

    // ---- documents the shell routes by extension ----
    /// `driveService?.reopen(path)`: `true` when Drive took the reopen
    fn drive_reopen(&self, path: &str) -> bool;
    /// `routeDocumentPath` as a native `OpenFile` command: `done` runs once on the core thread
    /// with `Ok(true)` when a tab opened or was focused, `Ok(false)` when nothing did
    fn route_path_native(&self, path: String, done: Box<dyn FnOnce(Result<bool, String>)>);
    /// Mints the root native operation of `command` (a menu or OS entry point has no renderer
    /// owner); `cancel` must outlive the operation
    fn issue_native_command(
        &self,
        command: shell_contract::ipc::NativeEditorCommand,
        cancel: shell_contract::runtime::CancelToken,
    ) -> Result<shell_contract::ipc::NativeInvocationOperation, String>;
    fn record_recent_file(&self, path: &str);
    fn record_recent_file_native(
        &self,
        path: String,
        operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    /// `notifyUnsupportedFile(path)`
    fn notify_unsupported_file(&self, path: &str);
    fn notify_unsupported_file_native(
        &self,
        path: String,
        operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    /// `showAppWarning(message, detail?)`
    fn show_app_warning(&self, message: &str, detail: Option<&str>);
    #[allow(clippy::too_many_arguments)]
    fn show_app_warning_owned(
        &self,
        message: String,
        detail: Option<String>,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    fn show_app_warning_native(
        &self,
        message: String,
        detail: Option<String>,
        operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    fn diagram_open_refusal_owned(
        &self,
        path: String,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<String>, String>)>,
    );
    fn diagram_open_refusal_native(
        &self,
        path: String,
        operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        done: Box<dyn FnOnce(Result<Option<String>, String>)>,
    );
    /// `startQueuedWorkbookNudge()`
    fn start_queued_workbook_nudge(&self);
    /// `defaultSaveDir()`
    fn default_save_dir(&self) -> String;
    /// `blankXlsxBuffer()`
    fn blank_xlsx(&self) -> Result<Vec<u8>, String>;
    /// `blankPdfBuffer()`
    fn blank_pdf(&self) -> Vec<u8>;
    /// `uniquePathIn(dir, name)`
    fn unique_path_in(&self, dir: &str, name: &str) -> String;
    /// `sheetsSuggestedPathTaken(path)`
    fn sheets_suggested_path_taken(&self, path: &str) -> bool;
    fn mark_sheets_unsaved_new(&self, backing: &str, suggested: &str, temp_dir: &str);
    fn unmark_sheets_unsaved_new(&self, backing: &str);
    fn mark_sheets_untitled_path(&self, path: &str);
    fn mark_pdf_untitled_path(&self, path: &str);
    /// `showErrorDialog(shellWindow, title, error)`
    fn show_error_dialog(&self, title: &str, error: &str);
    /// `openAgentWindow()`
    fn open_agent_window(&self);
    /// What a dropped skill file is (`droppedSkillSourceOf`) and the import
    /// that follows: `true` when `path` was taken as a skill source
    fn drop_skill_file(&self, path: &str, then: Box<dyn FnOnce(bool)>);
    /// the notice of a dropped zip or `SKILL.md` that held no skill
    fn drop_no_skills_found(&self);
    /// `isInsideRoot(root, path)` of the folder tree
    fn is_inside_root(&self, root: &str, path: &str) -> bool;
    /// The File > Open dialog (`openFileViaDialog`): picks files and opens each
    /// through [`TabsState::open_document_path`](super::TabsState::open_document_path)
    fn open_file_via_dialog(&self);
    /// What an editor does for a tab menu's Save, Save As or Print
    /// (`runTabDocumentAction`'s `save<App>`, `print<App>` calls); the
    /// renderer round trip they start answers through the editor itself
    fn tab_document_action(
        &self,
        kind: TabKind,
        view: ViewId,
        tab_id: &str,
        file_path: Option<&str>,
        action: DocAction,
    );
}

pub type WindowsHostRef = Arc<dyn WindowsHost>;

/// A tab menu's document entry
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocAction {
    Save,
    SaveAs,
    Print,
}
