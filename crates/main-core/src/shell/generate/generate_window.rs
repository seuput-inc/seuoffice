//! `apps/shell/src/main/generate/generate-window.ts`: the generate window
//! ("make a new document from these files"): a small separate window, a child
//! of the shell window (or of whichever window hosts the editor that opened
//! it), so it shows above editor tabs.
//!
//! One window at a time. Opening it again from another entry point starts
//! over with that entry's materials. The window never starts an AI run:
//! "make" opens a new untitled tab of the chosen kind with the request queued
//! for it and closes.
//!
//! What stays with the caller: the `ipcMain.handle` registrations (the
//! handlers call the `handle_*` methods here; [`GenerateWindow::configure`]
//! answers whether to register them), `new BrowserWindow(options)` (the
//! options are [`generate_window_options`]), the listeners on the window
//! (`ready-to-show`, `focus`, `closed`, the theme's `updated`, and the window
//! open handler, which call [`GenerateWindow::on_ready_to_show`],
//! [`GenerateWindow::on_focus`], [`GenerateWindow::on_closed`],
//! [`GenerateWindow::on_theme_updated`] and [`GenerateWindow::window_open`]),
//! the preload path, and the `GenerateHandoff` tab-id resolver
//! (`setGenerateTabIdResolver`, installed on every `configure`). The module
//! state (`deps`, `win`, `session`, `opener`, `ipcRegistered`) is a
//! [`GenerateWindow`] the caller owns, and `deps` is the [`GenerateWindowHost`].
//!
//! Parser, conversion, dialog, save and tab-open work yields an owned
//! [`WindowNeed`]. The caller drops core borrows before dispatching it and
//! resumes the exact [`WindowRun`] with a fresh open-document snapshot.
//! Window setup reports its real native result separately from the original
//! floating refresh through [`WindowOpen`]. Target-tab success is not reported
//! until its final native constructor, seed and project-follow callback.

use serde::Serialize;
use serde_json::{Value, json};

use super::convert::{ConversionJob, ConvertedDocument, MarkdownNoticeJob, NoticeCache};
use super::generate_api::{AiTargetKind, GenerateWindowState, SaveAction, channels};
use super::materials::{MaterialInspectJob, MaterialLengthCache};
use super::materials::supported_material_exts;
use super::open_materials::{
    OpenMaterialsDeps, OpenSnapshot, SaveAnswer, SaveMaterialRun, SaveMaterialStep, SaveOutcome,
    path_of_untitled, save_action_for, save_material_begin, show_untitled,
};
use super::session::{
    GenerateSession, GenerateSessionHost, MaterialRef, SessionRun, SessionStep, SessionAnswer,
    SessionNeed, OpenCommitToken, OpenCommitReceipt,
};
use super::title_bar::generate_title_bar_overlay;
use crate::packages::electron_utils::generate_handoff::GenerateHandoff;
use crate::packages::electron_utils::safe_external_url::safe_external_url;
use crate::packages::i18n::{Lang, product_name};
use crate::shell::renderer_disposal::Failure;
use crate::shell::title_bar_overlay::TitleBarOverlay;
pub use super::session::SessionOutcome as WindowOutcome;

/// `NewDocumentOpener` of `apps/shell/src/main/projects/new-documents.ts`:
/// where the window was opened from (home or an editor window)
#[derive(Debug, Clone, PartialEq)]
pub enum NewDocumentOpener {
    Home,
    Window { web_contents_id: f64 },
}

/// a window the host made, named by the host
pub type WindowId = i64;

/// the renderer strings (`generateText(lang, key)`) this module asks for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerateText {
    WindowTitle,
    DialogAddTitle,
    FilterSupported,
    Untitled,
}

/// what `dialog.showOpenDialog` answers
#[derive(Debug, Clone, PartialEq)]
pub struct OpenDialogResult {
    pub canceled: bool,
    pub file_paths: Vec<Value>,
}

/// `GenerateWindowDeps` and the Electron objects the module calls. Every
/// window is a [`WindowId`] the host made.
pub trait GenerateWindowHost: OpenMaterialsDeps {
    // ---- deps ----
    /// the window the generate window sits above when the caller names none
    fn parent_window(&mut self) -> Result<Option<WindowId>, Failure>;
    fn lang(&self) -> Lang;
    fn theme(&self) -> String;
    /// `deps.openConvertedTab` was given: the window offers conversions
    fn can_open_converted(&self) -> bool;
    /// every starred file in a Starred-view group
    fn starred_group_paths(&mut self, group: &str) -> Vec<String>;
    // ---- what the session's deps are made of ----
    fn new_id(&mut self) -> String;
    fn now(&mut self) -> f64;
    /// `MAX_OUTLINE_SLIDES`
    fn max_convert_slides(&self) -> usize;
    // ---- electron ----
    /// `generateText(lang, key)` of the renderer's strings
    fn generate_text(&self, lang: Lang, key: GenerateText) -> String;
    /// `nativeTheme.shouldUseDarkColors`
    fn should_use_dark_colors(&self) -> Result<bool, Failure>;
    /// `process.platform === 'darwin'`
    fn is_darwin(&self) -> bool;
    /// `process.env.ELECTRON_RENDERER_URL`
    fn renderer_url(&self) -> Option<String>;
    /// `new BrowserWindow(options)`
    fn create_window(&mut self, options: &GenerateWindowOptions) -> Result<WindowId, Failure>;
    fn window_is_destroyed(&self, window: WindowId) -> Result<bool, Failure>;
    fn window_is_minimized(&self, window: WindowId) -> Result<bool, Failure>;
    fn window_restore(&mut self, window: WindowId) -> Result<(), Failure>;
    fn window_show(&mut self, window: WindowId) -> Result<(), Failure>;
    fn window_focus(&mut self, window: WindowId) -> Result<(), Failure>;
    fn window_close(&mut self, window: WindowId) -> Result<(), Failure>;
    fn window_remove_menu(&mut self, window: WindowId) -> Result<(), Failure>;
    /// `win.webContents.id`
    fn window_web_contents_id(&self, window: WindowId) -> Result<f64, Failure>;
    /// `win.webContents.send(GENERATE_CHANNELS.state, state)`
    fn window_send_state(&mut self, window: WindowId, state: &GenerateWindowState) -> Result<(), Failure>;
    /// `win.setTitleBarOverlay(overlay)`: fails when the window was torn down
    fn window_set_title_bar_overlay(
        &mut self,
        window: WindowId,
        overlay: &TitleBarOverlay,
    ) -> Result<(), Failure>;
    /// `nativeTheme.on('updated', ...)` for this window
    fn watch_theme(&mut self, window: WindowId) -> Result<(), Failure>;
    /// `nativeTheme.off('updated', ...)` for this window
    fn unwatch_theme(&mut self, window: WindowId) -> Result<(), Failure>;
    /// `win.loadURL(url)`
    fn window_load_url(&mut self, window: WindowId, url: &str) -> Result<(), Failure>;
    /// `win.loadFile(join(__dirname, '../renderer/generate.html'))`
    fn window_load_generate_page(&mut self, window: WindowId) -> Result<(), Failure>;
    /// `shell.openExternal(url)`
    fn open_external(&mut self, url: &str) -> Result<(), Failure>;
}

/// `webPreferences` of the window, without the `preload` path the host adds
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GenerateWebPreferences {
    #[serde(rename = "contextIsolation")]
    pub context_isolation: bool,
    #[serde(rename = "nodeIntegration")]
    pub node_integration: bool,
    pub sandbox: bool,
}

/// the options of `new BrowserWindow({...})`, in the order of the literal,
/// without the `preload` path of `webPreferences` (the host adds it)
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GenerateWindowOptions {
    pub width: f64,
    pub height: f64,
    #[serde(rename = "minWidth")]
    pub min_width: f64,
    #[serde(rename = "minHeight")]
    pub min_height: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<WindowId>,
    pub show: bool,
    pub center: bool,
    pub minimizable: bool,
    pub maximizable: bool,
    pub fullscreenable: bool,
    #[serde(rename = "autoHideMenuBar")]
    pub auto_hide_menu_bar: bool,
    #[serde(rename = "backgroundColor")]
    pub background_color: &'static str,
    pub title: String,
    #[serde(rename = "titleBarStyle", skip_serializing_if = "Option::is_none")]
    pub title_bar_style: Option<&'static str>,
    #[serde(rename = "titleBarOverlay", skip_serializing_if = "Option::is_none")]
    pub title_bar_overlay: Option<TitleBarOverlay>,
    #[serde(rename = "webPreferences")]
    pub web_preferences: GenerateWebPreferences,
}

/// The options of the window: windows other than macOS draw the title band
/// themselves, and a live `parent` is passed on.
pub fn generate_window_options(
    lang: Lang,
    parent: Option<WindowId>,
    parent_destroyed: bool,
    dark: bool,
    darwin: bool,
    window_title: &str,
) -> GenerateWindowOptions {
    GenerateWindowOptions {
        width: 560.0,
        height: 720.0,
        min_width: 460.0,
        min_height: 520.0,
        parent: parent.filter(|_| !parent_destroyed),
        show: false,
        center: true,
        minimizable: false,
        maximizable: false,
        fullscreenable: false,
        auto_hide_menu_bar: true,
        background_color: generate_title_bar_overlay(dark).color,
        title: format!("{} - {}", window_title, product_name(Some(lang.as_str()))),
        // Windows paints a native frame light even under a dark app theme; draw the
        // title band ourselves and let Electron theme only its caption buttons.
        title_bar_style: if darwin { None } else { Some("hidden") },
        title_bar_overlay: if darwin {
            None
        } else {
            Some(generate_title_bar_overlay(dark))
        },
        web_preferences: GenerateWebPreferences {
            context_isolation: true,
            node_integration: false,
            sandbox: true,
        },
    }
}

/// `stringList(raw)`: the strings of a list that are not empty and at most 4096 long
pub fn string_list(raw: Option<&Value>) -> Vec<String> {
    match raw.and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .filter_map(Value::as_str)
            .filter(|p| !p.is_empty() && p.encode_utf16().count() <= 4096)
            .map(str::to_owned)
            .collect(),
        None => Vec::new(),
    }
}

/// the channels `registerIpc` handles, in the order it registers them
pub const IPC_HANDLE_CHANNELS: [&str; 12] = [
    channels::GET_STATE,
    channels::GET_THEME,
    channels::SET_KIND,
    channels::SET_CONVERT,
    channels::ADD_MATERIALS,
    channels::REMOVE_MATERIAL,
    channels::SAVE_MATERIAL,
    channels::REFRESH,
    channels::MAKE,
    channels::CANCEL,
    channels::HOME_OPEN_WITH_PATHS,
    channels::HOME_OPEN_WITH_GROUP,
];

#[derive(Debug)]
pub struct WindowRun {
    stage: WindowRunStage,
}

#[derive(Debug)]
enum WindowRunStage {
    Session { session_id: u64, run: SessionRun },
    Opening { session_id: u64, run: SessionRun, window: WindowId },
    AddMaterials,
}

#[derive(Debug)]
pub enum WindowNeed {
    Save(super::open_materials::SaveNeed),
    Reveal(String),
    Sleep(f64),
    Inspect(MaterialInspectJob),
    Notice(MarkdownNoticeJob),
    Convert(ConversionJob),
    OpenDialog { window: WindowId, options: Value },
    OpenTarget { handoff: GenerateHandoff, opener: Option<NewDocumentOpener>, token: OpenCommitToken, window: WindowId },
    OpenConverted { document: ConvertedDocument, opener: Option<NewDocumentOpener>, token: OpenCommitToken, window: WindowId },
}

#[derive(Debug)]
pub enum WindowStep {
    Done(Result<WindowOutcome, Failure>),
    Need(WindowNeed, WindowRun),
}

impl WindowStep {
    fn unit(result: Result<(), Failure>) -> Self {
        Self::Done(result.map(|()| WindowOutcome::Unit))
    }
}

/// Native window setup completes independently of its floating inspection.
#[derive(Debug)]
pub struct WindowOpen {
    pub result: Result<(), Failure>,
    pub background: WindowStep,
}

/// Terminal state completion. This window ID selects an already captured,
/// exact native cleanup handle; it does not grant permission to close a view.
#[derive(Debug)]
pub struct WindowCommitted {
    pub outcome: WindowOutcome,
    pub cleanup_window: WindowId,
}

/// the `init` of `openGenerateWindow`
#[derive(Debug, Clone, PartialEq)]
pub struct OpenInit {
    pub refs: Vec<MaterialRef>,
    pub kind: Option<AiTargetKind>,
    pub parent: Option<WindowId>,
    pub opener: Option<NewDocumentOpener>,
}

/// the module state of generate-window.ts
#[derive(Default)]
pub struct GenerateWindow {
    configured: bool,
    win: Option<WindowId>,
    /// every session a call may still be running on, by id
    sessions: Vec<SessionEntry>,
    /// the session of each window made, by the window's id
    window_sessions: Vec<(WindowId, u64)>,
    /// `session`: the current one
    session_id: Option<u64>,
    /// `lengthCache` of materials.ts
    length_cache: MaterialLengthCache,
    notice_cache: NoticeCache,
    next_session_id: u64,
    /// where the open window was opened from (home or an editor window)
    opener: Option<NewDocumentOpener>,
    ipc_registered: bool,
}

struct SessionEntry {
    id: u64,
    session: GenerateSession,
    /// saves in flight on this session
    pending: usize,
}

/// the deps of a session, over the host and the window state
struct SessionBridge<'a, H: GenerateWindowHost> {
    host: &'a mut H,
    win: &'a Option<WindowId>,
}

/// `closeGenerateWindow`'s window part
fn close_window_state<H: GenerateWindowHost>(
    host: &mut H, win: &mut Option<WindowId>, opener: &mut Option<NewDocumentOpener>,
) -> Result<(), Failure> {
    if let Some(current) = *win
        && !host.window_is_destroyed(current)? {
        host.window_close(current)?;
    }
    *win = None;
    *opener = None;
    Ok(())
}

impl<H: GenerateWindowHost> GenerateSessionHost for SessionBridge<'_, H> {
    fn lang(&self) -> Lang {
        self.host.lang()
    }


    fn path_of_untitled(
        &mut self,
        snap: &OpenSnapshot<'_>,
        web_contents_id: f64,
    ) -> Result<Option<String>, Failure> {
        path_of_untitled(&mut *self.host, snap.tabs, web_contents_id)
    }

    fn save_action_for(&mut self, snap: &OpenSnapshot<'_>, path: &str) -> SaveAction {
        save_action_for(&*self.host, snap.tabs, path)
    }

    fn save_material_begin(&mut self, snap: &OpenSnapshot<'_>, path: &str) -> Result<SaveMaterialStep, Failure> {
        save_material_begin(&mut *self.host, snap.tabs, path)
    }

    fn save_material_resume(&mut self, run: SaveMaterialRun, answer: SaveAnswer) -> SaveOutcome {
        run.resume(&mut *self.host, answer)
    }

    fn show_untitled(
        &mut self,
        snap: &OpenSnapshot<'_>,
        web_contents_id: f64,
    ) -> Result<Option<String>, Failure> {
        show_untitled(&mut *self.host, snap.tabs, web_contents_id)
    }


    fn can_convert(&self) -> bool {
        self.host.can_open_converted()
    }


    fn has_redaction_notice(&self) -> bool {
        self.host.can_open_converted()
    }


    fn push(&mut self, state: &GenerateWindowState) -> Result<(), Failure> {
        if let Some(win) = *self.win
            && !self.host.window_is_destroyed(win)? {
            self.host.window_send_state(win, state)?;
        }
        Ok(())
    }


    fn new_id(&mut self) -> String {
        self.host.new_id()
    }

    fn now(&mut self) -> f64 {
        self.host.now()
    }

    fn recheck_delay_ms(&self) -> Option<f64> {
        None
    }

    fn max_convert_slides(&self) -> usize {
        self.host.max_convert_slides()
    }
}

impl GenerateWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `run` on session `id` with its deps. `None` when there is no
    /// such session. When the session closed the window, no session is
    /// current afterwards; a session nothing runs on any more is dropped.
    fn with_session_id<H: GenerateWindowHost, R>(
        &mut self,
        host: &mut H,
        id: u64,
        run: impl FnOnce(&mut GenerateSession, &mut SessionBridge<'_, H>) -> R,
    ) -> Option<R> {
        let at = self.sessions.iter().position(|e| e.id == id)?;
        let mut entry = self.sessions.remove(at);
        let mut bridge = SessionBridge {
            host,
            win: &self.win,
        };
        let result = run(&mut entry.session, &mut bridge);
        self.sessions.insert(at, entry);
        Some(result)
    }

    /// Runs `run` on the current session (`session?.x`).
    fn with_session<H: GenerateWindowHost, R>(
        &mut self,
        host: &mut H,
        run: impl FnOnce(&mut GenerateSession, &mut SessionBridge<'_, H>) -> R,
    ) -> Option<R> {
        let id = self.session_id?;
        let result = self.with_session_id(host, id, run);
        self.prune_sessions();
        result
    }

    /// a session that is not current and has no save in flight is gone
    fn prune_sessions(&mut self) {
        let current = self.session_id;
        self.sessions
            .retain(|e| Some(e.id) == current || e.pending > 0);
    }

    fn current_session(&self) -> Option<&GenerateSession> {
        let id = self.session_id?;
        self.sessions
            .iter()
            .find(|e| e.id == id)
            .map(|e| &e.session)
    }

    fn has_session(&self) -> bool {
        self.session_id
            .is_some_and(|id| self.sessions.iter().any(|e| e.id == id))
    }

    /// `configureGenerateWindow(next)`: `deps = next`, and the tab id
    /// resolver follows `next.tabIdFor`. Answers whether the caller registers
    /// its IPC handlers now (`IPC_HANDLE_CHANNELS`): only the first time.
    pub fn configure(&mut self) -> bool {
        self.configured = true;
        if self.ipc_registered {
            return false;
        }
        self.ipc_registered = true;
        true
    }

    pub fn from_window<H: GenerateWindowHost>(&self, host: &H, sender_id: f64) -> Result<bool, Failure> {
        match self.win {
            Some(win) => Ok(!host.window_is_destroyed(win)? && host.window_web_contents_id(win)? == sender_id),
            None => Ok(false),
        }
    }

    fn is_from_home<H: GenerateWindowHost>(&self, host: &mut H, sender_id: f64) -> Result<bool, Failure> {
        if !self.configured { return Ok(false); }
        match host.parent_window()? {
            Some(parent) => Ok(!host.window_is_destroyed(parent)? && host.window_web_contents_id(parent)? == sender_id),
            None => Ok(false),
        }
    }

    fn sender_session<H: GenerateWindowHost>(&self, host: &H, sender_id: f64) -> Result<Option<u64>, Failure> {
        Ok(if self.from_window(host, sender_id)? { self.session_id } else { None })
    }

    pub fn handle_get_state<H: GenerateWindowHost>(
        &mut self, host: &mut H, sender_id: f64,
    ) -> Result<Option<GenerateWindowState>, Failure> {
        if !self.from_window(host, sender_id)? { return Ok(None); }
        Ok(self.with_session(host, |session, bridge| session.state(bridge)))
    }

    pub fn handle_get_theme<H: GenerateWindowHost>(&self, host: &H, sender_id: f64) -> Result<String, Failure> {
        Ok(if self.from_window(host, sender_id)? && self.configured { host.theme() } else { "system".into() })
    }

    pub fn handle_set_kind<H: GenerateWindowHost>(
        &mut self, host: &mut H, sender_id: f64, kind: Option<&Value>,
    ) -> Result<(), Failure> {
        if self.from_window(host, sender_id)? {
            self.with_session(host, |session, bridge| session.set_kind(bridge, kind)).unwrap_or(Ok(()))?;
        }
        Ok(())
    }

    pub fn handle_set_convert<H: GenerateWindowHost>(
        &mut self, host: &mut H, sender_id: f64, kind: Option<&Value>,
    ) -> Result<(), Failure> {
        if self.from_window(host, sender_id)? {
            self.with_session(host, |session, bridge| session.set_convert(bridge, kind)).unwrap_or(Ok(()))?;
        }
        Ok(())
    }

    pub fn handle_add_materials<H: GenerateWindowHost>(
        &mut self, host: &mut H, _snap: &OpenSnapshot<'_>, sender_id: f64,
    ) -> WindowStep {
        match self.from_window(host, sender_id) {
            Err(error) => return WindowStep::Done(Err(error)),
            Ok(false) => return WindowStep::unit(Ok(())),
            Ok(true) => {}
        }
        if !self.has_session() || !self.configured { return WindowStep::unit(Ok(())); }
        let Some(window) = self.win else { return WindowStep::unit(Ok(())); };
        let lang = host.lang();
        let options = json!({
            "title": host.generate_text(lang, GenerateText::DialogAddTitle),
            "filters": [{
                "name": host.generate_text(lang, GenerateText::FilterSupported),
                "extensions": supported_material_exts(),
            }],
            "properties": ["openFile", "multiSelections"],
        });
        WindowStep::Need(WindowNeed::OpenDialog { window, options },
            WindowRun { stage: WindowRunStage::AddMaterials })
    }

    pub fn handle_remove_material<H: GenerateWindowHost>(
        &mut self, host: &mut H, sender_id: f64, key: Option<&Value>,
    ) -> Result<(), Failure> {
        if self.from_window(host, sender_id)? {
            self.with_session(host, |session, bridge| session.remove(bridge, key)).unwrap_or(Ok(()))?;
        }
        Ok(())
    }

    pub fn handle_save_material<H: GenerateWindowHost>(
        &mut self, host: &mut H, snap: &OpenSnapshot<'_>, sender_id: f64, key: Option<&Value>,
    ) -> WindowStep {
        let id = match self.sender_session(host, sender_id) {
            Err(error) => return WindowStep::Done(Err(error)),
            Ok(None) => return WindowStep::unit(Ok(())),
            Ok(Some(id)) => id,
        };
        let step = self.with_session_id(host, id, |session, bridge| session.save_begin(bridge, snap, key));
        self.window_step(host, snap, id, step)
    }

    pub fn resume<H: GenerateWindowHost>(
        &mut self, host: &mut H, snap: &OpenSnapshot<'_>, run: WindowRun, answer: SessionAnswer,
    ) -> WindowStep {
        match (run.stage, answer) {
            (WindowRunStage::AddMaterials, SessionAnswer::Dialog(answer)) => {
                let result = match answer {
                    Err(error) => return WindowStep::Done(Err(error)),
                    Ok(result) => result,
                };
                if result.canceled { return WindowStep::unit(Ok(())); }
                let Some(id) = self.session_id else { return WindowStep::unit(Ok(())); };
                let step = self.with_session_id(host, id, |session, bridge| session.add_paths(bridge, snap, &result.file_paths));
                self.window_step(host, snap, id, step)
            }
            (WindowRunStage::Session { session_id, run }, answer)
            | (WindowRunStage::Opening { session_id, run, .. }, answer @ SessionAnswer::OpenFailed(_)) => {
                if Some(session_id) != self.session_id {
                    self.discard_run(WindowRun { stage: WindowRunStage::Session { session_id, run } });
                    return WindowStep::Done(Err(Failure("generate continuation session was replaced".into())));
                }
                let step = self.with_session_id(host, session_id, |session, bridge| session.resume(bridge, snap, run, answer));
                if let Some(entry) = self.sessions.iter_mut().find(|entry| entry.id == session_id) {
                    entry.pending -= 1;
                }
                self.window_step(host, snap, session_id, step)
            }
            (stage, _) => {
                self.discard_run(WindowRun { stage });
                WindowStep::Done(Err(Failure("unexpected generate continuation answer".into())))
            }
        }
    }

    /// Consume only an actual committed open receipt. No fresh source snapshot
    /// or live-source authority is consulted after accepted ownership transfer.
    pub fn complete_committed_open(&mut self, run: WindowRun, receipt: OpenCommitReceipt) -> Result<WindowCommitted, Failure> {
        let (session_id, run, window) = match run.stage {
            WindowRunStage::Opening { session_id, run, window } => (session_id, run, window),
            stage => {
                self.discard_run(WindowRun { stage });
                return Err(Failure("committed generate receipt requires a terminal opening continuation".into()));
            }
        };
        let entry = self.sessions.iter_mut().find(|entry| entry.id == session_id)
            .ok_or_else(|| Failure("committed generate session disappeared".into()))?;
        let result = entry.session.complete_committed_open(run, receipt);
        entry.pending -= 1;
        if result.is_ok() && self.session_id == Some(session_id) {
            self.session_id = None;
            if self.win == Some(window) { self.win = None; self.opener = None; }
        }
        self.prune_sessions();
        result.map(|outcome| WindowCommitted { outcome, cleanup_window: window })
    }

    pub fn discard_run(&mut self, run: WindowRun) {
        let session = match run.stage {
            WindowRunStage::Session { session_id, run } | WindowRunStage::Opening { session_id, run, .. } => Some((session_id, run)),
            WindowRunStage::AddMaterials => None,
        };
        if let Some((session_id, run)) = session
            && let Some(entry) = self.sessions.iter_mut().find(|entry| entry.id == session_id) {
            entry.session.discard_run(run);
            entry.pending -= 1;
        }
        self.prune_sessions();
    }

    fn window_step<H: GenerateWindowHost>(
        &mut self, host: &H, snap: &OpenSnapshot<'_>, id: u64, step: Option<SessionStep>,
    ) -> WindowStep {
        let result = match step {
            None => WindowStep::Done(Err(Failure("generate continuation session disappeared".into()))),
            Some(SessionStep::Done(result)) => WindowStep::Done(result),
            Some(SessionStep::Need(need, run)) => {
                let (need, cleanup_window) = match need {
                    SessionNeed::Save(need) => (WindowNeed::Save(need), None),
                    SessionNeed::Reveal(tab) => (WindowNeed::Reveal(tab), None),
                    SessionNeed::Sleep(ms) => (WindowNeed::Sleep(ms), None),
                    SessionNeed::Inspect(path) => {
                        let open_files = snap.files.capture_snapshot();
                        (WindowNeed::Inspect(MaterialInspectJob::new(path, open_files, self.length_cache.clone())), None)
                    }
                    SessionNeed::Notice(path) => (WindowNeed::Notice(MarkdownNoticeJob::new(path, self.notice_cache.clone())), None),
                    SessionNeed::Convert { kind, path } => (WindowNeed::Convert(ConversionJob {
                        kind, path, untitled: host.generate_text(host.lang(), GenerateText::Untitled),
                    }), None),
                    SessionNeed::OpenTarget { handoff, token } => {
                        let Some(window) = self.win else {
                            if let Some(entry) = self.sessions.iter_mut().find(|entry| entry.id == id) { entry.session.discard_run(run); }
                            return WindowStep::Done(Err(Failure("generate window disappeared before target open".into())));
                        };
                        (WindowNeed::OpenTarget { handoff, token, window, opener: self.opener.clone() }, Some(window))
                    }
                    SessionNeed::OpenConverted { document, token } => {
                        let Some(window) = self.win else {
                            if let Some(entry) = self.sessions.iter_mut().find(|entry| entry.id == id) { entry.session.discard_run(run); }
                            return WindowStep::Done(Err(Failure("generate window disappeared before conversion open".into())));
                        };
                        (WindowNeed::OpenConverted { document, token, window, opener: self.opener.clone() }, Some(window))
                    }
                };
                if let Some(entry) = self.sessions.iter_mut().find(|entry| entry.id == id) { entry.pending += 1; }
                let stage = match cleanup_window {
                    Some(window) => WindowRunStage::Opening { session_id: id, run, window },
                    None => WindowRunStage::Session { session_id: id, run },
                };
                WindowStep::Need(need, WindowRun { stage })
            }
        };
        self.prune_sessions();
        result
    }

    pub fn handle_refresh<H: GenerateWindowHost>(
        &mut self, host: &mut H, snap: &OpenSnapshot<'_>, sender_id: f64,
    ) -> WindowStep {
        let id = match self.sender_session(host, sender_id) {
            Err(error) => return WindowStep::Done(Err(error)),
            Ok(None) => return WindowStep::unit(Ok(())),
            Ok(Some(id)) => id,
        };
        let step = self.with_session_id(host, id, |session, bridge| session.refresh(bridge, snap));
        self.window_step(host, snap, id, step)
    }

    pub fn handle_make<H: GenerateWindowHost>(
        &mut self, host: &mut H, snap: &OpenSnapshot<'_>, sender_id: f64, text: Option<&Value>,
    ) -> WindowStep {
        let id = match self.sender_session(host, sender_id) {
            Err(error) => return WindowStep::Done(Err(error)),
            Ok(None) => return WindowStep::Done(Ok(WindowOutcome::Made(false))),
            Ok(Some(id)) => id,
        };
        let step = self.with_session_id(host, id, |session, bridge| session.make(bridge, snap, text));
        self.window_step(host, snap, id, step)
    }

    pub fn handle_cancel<H: GenerateWindowHost>(&mut self, host: &mut H, sender_id: f64) -> Result<(), Failure> {
        if self.from_window(host, sender_id)? { self.close_generate_window(host)?; }
        Ok(())
    }

    pub fn handle_open_with_paths<H: GenerateWindowHost>(
        &mut self, host: &mut H, snap: &OpenSnapshot<'_>, sender_id: f64, paths: Option<&Value>,
    ) -> WindowOpen {
        match self.is_from_home(host, sender_id) {
            Ok(true) => self.open_generate_window_with_paths(host, snap, string_list(paths), None),
            Ok(false) => WindowOpen { result: Ok(()), background: WindowStep::unit(Ok(())) },
            Err(error) => WindowOpen { result: Err(error), background: WindowStep::unit(Ok(())) },
        }
    }

    pub fn handle_open_with_group<H: GenerateWindowHost>(
        &mut self, host: &mut H, snap: &OpenSnapshot<'_>, sender_id: f64, group: Option<&Value>,
    ) -> WindowOpen {
        match self.is_from_home(host, sender_id) {
            Ok(true) => {}
            Ok(false) => return WindowOpen { result: Ok(()), background: WindowStep::unit(Ok(())) },
            Err(error) => return WindowOpen { result: Err(error), background: WindowStep::unit(Ok(())) },
        }
        let Some(group) = group.and_then(Value::as_str).filter(|group| !group.is_empty()) else {
            return WindowOpen { result: Ok(()), background: WindowStep::unit(Ok(())) };
        };
        let paths = host.starred_group_paths(group);
        self.open_generate_window_with_paths(host, snap, paths, None)
    }

    /// `openGenerateWindow(init)`: open the window (or start the open one
    /// over) with these materials
    pub fn open_generate_window<H: GenerateWindowHost>(
        &mut self, host: &mut H, snap: &OpenSnapshot<'_>, init: OpenInit,
    ) -> WindowOpen {
        let mut background = WindowStep::unit(Ok(()));
        let result = (|| -> Result<(), Failure> {
            if !self.configured { return Err(Failure("the generate window is not configured".into())); }
            self.opener = init.opener;
            if let Some(win) = self.win
                && !host.window_is_destroyed(win)?
                && self.current_session().is_some_and(|session| !session.is_closed()) {
                let id = self.session_id.expect("a current session has its id");
                let rows = self.with_session_id(host, id, |session, bridge| session.reset_begin(bridge, &init.refs, init.kind))
                    .ok_or_else(|| Failure("generate reset session disappeared".into()))??;
                let shown = (|| -> Result<(), Failure> {
                    if host.window_is_minimized(win)? { host.window_restore(win)?; }
                    host.window_show(win)?;
                    host.window_focus(win)
                })();
                let step = self.with_session_id(host, id, |session, bridge| session.reset_finish(bridge, snap, rows));
                background = self.window_step(host, snap, id, step);
                return shown;
            }
            let id = self.next_session_id;
            self.next_session_id += 1;
            self.sessions.push(SessionEntry { id, session: GenerateSession::new(&init.refs, init.kind), pending: 0 });
            self.session_id = Some(id);
            self.prune_sessions();
            let lang = host.lang();
            let parent = match init.parent { Some(parent) => Some(parent), None => host.parent_window()? };
            let parent_destroyed = match parent { Some(parent) => host.window_is_destroyed(parent)?, None => false };
            let options = generate_window_options(lang, parent, parent_destroyed,
                host.should_use_dark_colors()?, host.is_darwin(), &host.generate_text(lang, GenerateText::WindowTitle));
            let next = host.create_window(&options)?;
            self.win = Some(next);
            self.window_sessions.push((next, id));
            host.window_remove_menu(next)?;
            host.watch_theme(next)?;
            match host.renderer_url() {
                Some(url) if !url.is_empty() => host.window_load_url(next, &format!("{url}/generate.html"))?,
                _ => host.window_load_generate_page(next)?,
            }
            let step = self.with_session_id(host, id, |session, bridge| session.refresh(bridge, snap));
            background = self.window_step(host, snap, id, step);
            Ok(())
        })();
        WindowOpen { result, background }
    }

    pub fn on_ready_to_show<H: GenerateWindowHost>(&self, host: &mut H, window: WindowId) -> Result<(), Failure> {
        host.window_show(window)
    }

    pub fn on_focus<H: GenerateWindowHost>(
        &mut self, host: &mut H, snap: &OpenSnapshot<'_>, window: WindowId,
    ) -> WindowStep {
        if let Some(id) = self.session_of(window)
            && Some(id) == self.session_id {
            let step = self.with_session_id(host, id, |session, bridge| session.refresh(bridge, snap));
            return self.window_step(host, snap, id, step);
        }
        WindowStep::unit(Ok(()))
    }

    pub fn on_closed<H: GenerateWindowHost>(&mut self, host: &mut H, window: WindowId) -> Result<(), Failure> {
        let result = host.unwatch_theme(window);
        let owned = self.session_of(window);
        self.window_sessions.retain(|(known, _)| *known != window);
        if self.win == Some(window) { self.win = None; self.opener = None; }
        if owned.is_some() && owned == self.session_id {
            self.session_id = None;
            self.prune_sessions();
        }
        result
    }

    pub fn on_theme_updated<H: GenerateWindowHost>(&self, host: &mut H, window: WindowId) -> Result<(), Failure> {
        if host.window_is_destroyed(window)? || host.is_darwin() { return Ok(()); }
        let overlay = generate_title_bar_overlay(host.should_use_dark_colors()?);
        host.window_set_title_bar_overlay(window, &overlay)
    }

    pub fn window_open<H: GenerateWindowHost>(&self, host: &mut H, url: &str) -> Result<(), Failure> {
        if let Some(target) = safe_external_url(Some(url), None) { host.open_external(&target)?; }
        Ok(())
    }

    fn session_of(&self, window: WindowId) -> Option<u64> {
        self.window_sessions
            .iter()
            .find(|(w, _)| *w == window)
            .map(|(_, id)| *id)
    }

    /// `openGenerateWindowWithPaths(paths, parent)`: Home: these files as the starting materials
    pub fn open_generate_window_with_paths<H: GenerateWindowHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        paths: Vec<String>,
        parent: Option<WindowId>,
    ) -> WindowOpen {
        self.open_generate_window(
            host,
            snap,
            OpenInit {
                refs: paths.into_iter().map(MaterialRef::Path).collect(),
                kind: None,
                parent,
                opener: Some(NewDocumentOpener::Home),
            },
        )
    }

    /// `openGenerateWindowForDocument(doc)`: an editor's File menu, "make
    /// another document from this one". A document never saved has no
    /// `filePath`; it is listed as "save it first".
    pub fn open_generate_window_for_document<H: GenerateWindowHost>(
        &mut self,
        host: &mut H,
        snap: &OpenSnapshot<'_>,
        web_contents_id: f64,
        file_path: Option<&str>,
        title: &str,
        parent: Option<WindowId>,
    ) -> WindowOpen {
        let mref = match file_path.filter(|p| !p.is_empty()) {
            Some(path) => MaterialRef::Path(path.to_owned()),
            None => MaterialRef::Untitled {
                web_contents_id,
                title: title.to_owned(),
            },
        };
        self.open_generate_window(
            host,
            snap,
            OpenInit {
                refs: vec![mref],
                kind: None,
                parent,
                opener: Some(NewDocumentOpener::Window { web_contents_id }),
            },
        )
    }

    /// `closeGenerateWindow()`
    pub fn close_generate_window<H: GenerateWindowHost>(&mut self, host: &mut H) -> Result<(), Failure> {
        close_window_state(host, &mut self.win, &mut self.opener)?;
        self.session_id = None;
        self.prune_sessions();
        Ok(())
    }

    pub fn generate_window_contents<H: GenerateWindowHost>(&self, host: &H) -> Result<Option<f64>, Failure> {
        match self.win {
            Some(win) if !host.window_is_destroyed(win)? => Ok(Some(host.window_web_contents_id(win)?)),
            _ => Ok(None),
        }
    }
}
