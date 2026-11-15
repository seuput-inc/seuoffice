//! What the generate window reaches beyond `main-core` and the shared hosts:
//! the Electron window it lives in (`new BrowserWindow`, `nativeTheme`), the
//! shell entry's tabs and settings (`apps/shell/src/main/index.ts` hands them
//! to `configureGenerateWindow` as `GenerateWindowDeps`), and the engines the
//! conversions and the material reader use, which are not in `main-core` on
//! this branch (`parseFileToText` of `@seuoffice/file-parse`, the map
//! engine's outline reader, the deck engine of `@seuoffice/pptx-ops`).
//!
//! Window setup and state run on the core thread. Inspection, conversion and
//! dialogs run on workers with the captured request authority. Save, timer and
//! tab-open callbacks may arrive from any thread; the port resumes on core with
//! a fresh snapshot. No core or UI task may wait for renderer work.
//!
//! Windows are the shell's [`WindowId`]s. The window host (batch W's
//! `WindowsHost`) makes the window and keeps the views; this trait is the
//! slice of it the generate window uses, so the integrator implements it in
//! `crates/shell/src/host/` over `Windows::open_window` and the tab manager.

use std::sync::Arc;

use jscompat::JsonValue;
use main_core::packages::electron_utils::generate_handoff::GenerateHandoff;
use main_core::packages::i18n::Lang;
use main_core::shell::app_routing::DocumentTabKind;
use main_core::shell::generate::convert::{ConvertedDocument, OpOutcome, XmindTopic};
use main_core::shell::generate::generate_api::GenerateWindowState;
use main_core::shell::generate::generate_window::{
    GenerateText, GenerateWindowOptions, NewDocumentOpener,
};
use main_core::shell::generate::materials::ParsedFile;
use main_core::shell::generate::open_materials::{OpenDocumentTab, SaveAnswer, SaveNeed};
use main_core::shell::title_bar_overlay::TitleBarOverlay;
use shell_contract::hosts::{DialogsHost, FileDialog, OsShellHost};
use shell_contract::ipc::{CallContext, EventSink, NativeContext, Sender};
use shell_contract::runtime::{CancelToken, RendererGeneration};
use shell_contract::{ViewId, WindowId};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub enum GenerateWindowEvent {
    Ready { origin: NativeContext },
    Focus,
    Closed,
    Theme,
    OpenLink { origin: NativeContext },
}

#[derive(Clone, Debug)]
pub enum GenerateOrigin {
    Renderer {
        sender: Sender,
        native: Option<NativeContext>,
    },
    NativeMenu {
        native: NativeContext,
    },
    WindowEvent {
        window: WindowId,
        identity: NativeContext,
        event: GenerateWindowEvent,
    },
}

#[derive(Clone, Debug)]
pub struct GenerateAuthority {
    pub origin: GenerateOrigin,
    pub cancel: CancelToken,
}

impl GenerateAuthority {
    pub fn renderer(cx: &CallContext) -> Self {
        Self {
            origin: GenerateOrigin::Renderer {
                sender: cx.sender,
                native: cx.native.clone(),
            },
            cancel: cx.cancel.clone(),
        }
    }

    pub fn native_menu(native: NativeContext, cancel: CancelToken) -> Self {
        Self {
            origin: GenerateOrigin::NativeMenu { native },
            cancel,
        }
    }

    pub fn native_context(&self) -> Option<&NativeContext> {
        match &self.origin {
            GenerateOrigin::Renderer { native, .. } => native.as_ref(),
            GenerateOrigin::NativeMenu { native } => Some(native),
            GenerateOrigin::WindowEvent { identity, .. } => Some(identity),
        }
    }

    pub fn is_closed_event(&self) -> bool {
        matches!(
            &self.origin,
            GenerateOrigin::WindowEvent {
                event: GenerateWindowEvent::Closed,
                ..
            }
        )
    }

    /// The production host separately validates the opaque native proof.
    pub fn check_cancelled(&self) -> Result<(), String> {
        if self.is_closed_event() {
            return Ok(());
        }
        let lost = self.native_context().is_some_and(|native| {
            native.owner_loss().is_some_and(|loss| loss.is_cancelled())
                || native
                    .renderer()
                    .is_some_and(|renderer| !renderer.is_active())
        });
        if self.cancel.is_cancelled() || lost {
            Err("The generating document is no longer available.".into())
        } else {
            Ok(())
        }
    }
}

pub struct GenerateOpenRequest {
    pub token: main_core::shell::generate::session::OpenCommitToken,
    pub window: WindowId,
}

/// A producer-owned, move-only witness of actual target construction and load.
/// Production adapters retain the native committed-tab receipt behind this trait.
pub trait GenerateCommittedTarget: Send {
    fn created(&self) -> &RendererGeneration;
    fn validate_target(&self) -> Result<(), String>;
    fn with_terminal_commit(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
}

pub type GenerateCleanupDone = Box<dyn FnOnce(Result<(), String>) + Send>;

pub struct GenerateWindowCleanup {
    pub(super) window: WindowId,
    close: Box<dyn FnOnce(GenerateCleanupDone) + Send>,
}

impl GenerateWindowCleanup {
    pub fn new(window: WindowId, close: Box<dyn FnOnce(GenerateCleanupDone) + Send>) -> Self {
        Self { window, close }
    }

    pub(super) fn complete(self, done: GenerateCleanupDone) {
        (self.close)(done);
    }
}

pub struct GenerateCommittedOpen {
    pub(super) receipt: main_core::shell::generate::session::OpenCommitReceipt,
    pub(super) target: Box<dyn GenerateCommittedTarget>,
    pub(super) cleanup: GenerateWindowCleanup,
}

impl GenerateCommittedOpen {
    pub fn new(
        token: main_core::shell::generate::session::OpenCommitToken,
        target: Box<dyn GenerateCommittedTarget>,
        cleanup: GenerateWindowCleanup,
    ) -> Result<Self, String> {
        target.validate_target()?;
        if !target.created().is_active() {
            return Err("The generated target was replaced.".into());
        }
        Ok(Self {
            receipt: token.commit(),
            target,
            cleanup,
        })
    }
}

pub type GenerateOpenResult = Result<GenerateCommittedOpen, String>;

use super::GenerateWindowEvents;

/// What `webContentsFor(tabId)` answers: the webContents behind a tab
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabContents {
    pub view: ViewId,
    /// `webContents.isDestroyed()`
    pub destroyed: bool,
}

/// `GenerateWindowDeps` of generate-window.ts, the Electron calls of the
/// module, and the engines named in the module doc
pub trait GenerateShell: Send + Sync {
    fn check_authority(&self, authority: &GenerateAuthority) -> Result<(), String>;
    fn show_open_dialog(
        &self,
        authority: &GenerateAuthority,
        dialogs: DialogsHost,
        parent: WindowId,
        options: &FileDialog,
    ) -> Result<Option<Vec<PathBuf>>, String>;
    fn open_external(
        &self,
        authority: &GenerateAuthority,
        os_shell: OsShellHost,
        url: &str,
    ) -> Result<(), String>;
    fn send_state(
        &self,
        authority: &GenerateAuthority,
        events: Arc<dyn EventSink>,
        view: ViewId,
        state: &GenerateWindowState,
    ) -> Result<(), String>;
    // ---- the window: `new BrowserWindow` and what the module calls on it ----

    /// `new BrowserWindow(options)` with the preload of
    /// `apps/shell/src/preload/generate.ts` added to its web preferences. The
    /// host calls `events` for what the TypeScript listens to on the window:
    /// `ready-to-show` (once), `focus`, `closed`, `nativeTheme`'s `updated`
    /// (while [`GenerateShell::watch_theme`] is on for the window) and the
    /// window open handler.
    fn create_window(
        &self,
        authority: &GenerateAuthority,
        options: &GenerateWindowOptions,
        events: GenerateWindowEvents,
    ) -> Result<WindowId, String>;
    /// `win.webContents.id` of a window this host made (`create_window`); the
    /// shell window's page is the main window host's, asked there. `None` once it is gone
    fn window_view(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<Option<ViewId>, String>;
    /// `win.isDestroyed()`
    fn window_is_destroyed(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<bool, String>;
    /// `win.isMinimized()`
    fn window_is_minimized(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<bool, String>;
    /// `win.restore()`
    fn window_restore(&self, authority: &GenerateAuthority, window: WindowId)
    -> Result<(), String>;
    /// `win.show()`
    fn window_show(&self, authority: &GenerateAuthority, window: WindowId) -> Result<(), String>;
    /// `win.focus()`
    fn window_focus(&self, authority: &GenerateAuthority, window: WindowId) -> Result<(), String>;
    /// `win.close()`: the window's `closed` follows
    fn window_close(&self, authority: &GenerateAuthority, window: WindowId) -> Result<(), String>;
    /// `win.removeMenu()`
    fn window_remove_menu(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<(), String>;
    /// `win.setTitleBarOverlay(overlay)`: `Err` when the window was torn down
    fn window_set_title_bar_overlay(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
        overlay: &TitleBarOverlay,
    ) -> Result<(), String>;
    /// `nativeTheme.on('updated', ...)` for this window
    fn watch_theme(&self, authority: &GenerateAuthority, window: WindowId) -> Result<(), String>;
    /// `nativeTheme.off('updated', ...)` for this window
    fn unwatch_theme(&self, authority: &GenerateAuthority, window: WindowId) -> Result<(), String>;
    /// `process.env.ELECTRON_RENDERER_URL`
    fn renderer_url(&self) -> Option<String>;
    /// `win.loadURL(url)`
    fn window_load_url(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
        url: &str,
    ) -> Result<(), String>;
    /// `win.loadFile(join(__dirname, '../renderer/generate.html'))`
    fn window_load_generate_page(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<(), String>;
    /// `nativeTheme.shouldUseDarkColors`
    fn should_use_dark_colors(&self, authority: &GenerateAuthority) -> Result<bool, String>;

    // ---- `GenerateWindowDeps` ----

    /// `deps.lang()`
    fn lang(&self) -> Lang;
    /// `deps.theme()`: `light`, `dark` or `system`
    fn theme(&self) -> String;
    /// `generateText(lang, key)` of `renderer/src/i18n/strings-generate.ts`
    fn generate_text(&self, lang: Lang, key: GenerateText) -> String;
    /// `deps.starredGroupPaths(group)`
    fn starred_group_paths(&self, group: &str) -> Vec<String>;
    /// `deps.tabIdFor(webContentsId)`: the shell tab id behind a webContents,
    /// for the hand-off store's run-started records
    fn tab_id_for(&self, web_contents_id: f64) -> Option<String>;
    /// `deps.openTargetTab(handoff, opener)` (`TabManager.openGenerateTargetTab`,
    /// then the new tab follows the opener's project): `Err` when no tab could open
    fn open_target_tab(
        &self,
        authority: &GenerateAuthority,
        handoff: GenerateHandoff,
        opener: Option<NewDocumentOpener>,
        request: GenerateOpenRequest,
        then: Box<dyn FnOnce(GenerateOpenResult) + Send>,
    );
    /// `deps.openConvertedTab` was given: the window offers conversions
    fn can_open_converted(&self) -> bool;
    /// `deps.openConvertedTab(doc, opener)` (`TabManager.openConversionResult`):
    /// `Err` when no tab could open
    fn open_converted_tab(
        &self,
        authority: &GenerateAuthority,
        doc: ConvertedDocument,
        opener: Option<NewDocumentOpener>,
        request: GenerateOpenRequest,
        then: Box<dyn FnOnce(GenerateOpenResult) + Send>,
    );

    // ---- `deps.openMaterials` ----

    /// `deps.list()`: the open tabs (`tabManager.openDocuments()` and the
    /// detached windows' documents). `then` runs once the list is known; the
    /// open files of the TypeScript's `OpenFilesSource` are the same list.
    fn open_documents(
        &self,
        authority: &GenerateAuthority,
        then: Box<dyn FnOnce(Result<Vec<OpenDocumentTab>, String>) + Send>,
    );
    /// `deps.webContentsFor(tabId)`
    fn tab_contents(
        &self,
        authority: &GenerateAuthority,
        tab_id: &str,
    ) -> Result<Option<TabContents>, String>;
    /// `deps.reveal(tabId)`: bring the tab (and its window) forward
    fn reveal_tab(
        &self,
        authority: &GenerateAuthority,
        tab_id: String,
        then: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    /// `!!deps.save[kind]`
    fn has_save(&self, kind: DocumentTabKind) -> bool;
    /// `deps.save[kind](...)`: the editor's save command (`mcpDocsControl.runCommand(wcId,
    /// 'save_document', { path, overwrite: true })`, `requestMarkdownSave(wc, 'save')`, ...).
    /// `then` is told when the editor answered; a rejection is `Err`.
    fn save(
        &self,
        authority: &GenerateAuthority,
        need: SaveNeed,
        then: Box<dyn FnOnce(SaveAnswer) + Send>,
    );
    /// `setTimeout(then, ms)`: the recheck of "save" waits on it
    fn set_timeout(&self, authority: &GenerateAuthority, ms: f64, then: Box<dyn FnOnce() + Send>);

    // ---- `randomUUID()` and `Date.now()` of session.ts ----

    fn new_id(&self) -> String;
    fn now_ms(&self) -> f64;

    // ---- engines not in `main-core` ----

    /// `parseFileToText(path)` of `@seuoffice/file-parse`; `Err` is a throw
    fn parse_file_to_text(
        &self,
        authority: &GenerateAuthority,
        path: &str,
    ) -> Result<ParsedFile, String>;
    /// `OUTLINE_KIND_MARKS.boundary` and `OUTLINE_KIND_MARKS.relationship` of the map editor
    fn not_topic_marks(&self) -> [String; 2];
    /// `outlineToSheets(outline, { untitled })[0].rootTopic`
    fn first_sheet_root(
        &self,
        authority: &GenerateAuthority,
        outline: &str,
        untitled: &str,
    ) -> Result<XmindTopic, String>;
    /// `createXmindDocumentFromOutline(text, { untitled })`
    fn create_xmind_document_from_outline(
        &self,
        authority: &GenerateAuthority,
        text: &str,
        untitled: &str,
    ) -> Result<(), String>;
    /// `openPptx(await createBlankPptx())`: the host keeps the deck and names it
    fn open_blank_deck(&self, authority: &GenerateAuthority) -> Result<u64, String>;
    /// `runTxn(opened, { ops: [op], isolation: 'atomic' })`
    fn apply_deck_op(
        &self,
        authority: &GenerateAuthority,
        deck: u64,
        op: &JsonValue,
    ) -> Result<OpOutcome, String>;
    /// `savePptx(opened)`: the deck is gone after it
    fn save_deck(&self, authority: &GenerateAuthority, deck: u64) -> Result<Vec<u8>, String>;
    /// Discard only this request's private conversion handles on the worker.
    fn release_conversion_resources(&self, authority: &GenerateAuthority);
}

pub type GenerateShellHost = Arc<dyn GenerateShell>;
