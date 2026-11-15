//! The probe fake of the `generate` port's batch host ([`GenerateShell`]):
//! the generate window's Electron window, the shell entry's tabs and the file
//! reader, scripted by the case's `hosts.a.generate` (the TypeScript half is
//! tools/ipc-diff/groups/a/generate.ts, which reads the same value and logs
//! the same lines). The UI language is `hosts.a.lang` (`ko` by default).
//!
//! `hosts.a.generate` keys, all optional:
//!
//! - `windows`: the ids of the windows the port makes, in order; a window's
//!   page has the same id as its view (`50`, `51`, ... by default). The case's
//!   `views` name them with the `ShellGenerate` surface.
//! - `theme` (`light`), `dark` (false), `rendererUrl`, `minimized` (list of
//!   window ids), `texts` (`{windowTitle, dialogAddTitle, filterSupported,
//!   untitled}`; a text is its key by default)
//! - `starred`: `{<group>: [paths]}`
//! - `tabs`: the open documents `[{id, kind, title, filePath, active, dirty}]`;
//!   `contents`: `{<tabId>: {id, destroyed}}`; `save`: the kinds that have a
//!   save command; `saveResult`: `{<kind>: false | "throws"}` (saved by
//!   default); `cleanOnSave` (true): a saved tab is clean at once
//! - `openTarget`: `"throws"`, when no tab could open
//! - `parse`: `{<file name>: {ok, kind, text, error} | "throws"}`; by
//!   default the file's text
//!
//! Conversions are not on offer (`openConvertedTab` is not given), so the
//! engines are never asked. The host log gets one line per call, named after
//! the TypeScript call it stands for; windows log `["win", id, what]`.
//! `ready-to-show`, `focus`, the theme's `updated` and the window open
//! handler are not fired: no channel call reaches them (the unit tests do).
//! `closed` follows `close`, as Electron's does.

use std::sync::{Arc, Mutex};

use jscompat::JsonValue;
use main_core::packages::electron_utils::generate_handoff::GenerateHandoff;
use main_core::packages::i18n::Lang;
use main_core::shell::app_routing::DocumentTabKind;
use main_core::shell::generate::convert::{ConvertedDocument, OpOutcome, XmindTopic};
use main_core::shell::generate::generate_window::{
    GenerateText, GenerateWindowOptions, NewDocumentOpener,
};
use main_core::shell::generate::materials::ParsedFile;
use main_core::shell::generate::open_materials::{OpenDocumentTab, SaveAnswer, SaveNeed};
use main_core::shell::renderer_disposal::Failure;
use main_core::shell::title_bar_overlay::TitleBarOverlay;
use serde_json::{Value, json};
use shell_contract::{ViewId, WindowId};

use super::{Log, push};
use crate::generate::{BoundGenerateWindowEvents, GenerateWindowEvents};
use crate::generate::host::{GenerateAuthority, GenerateShell, GenerateShellHost, TabContents, GenerateOpenRequest, GenerateOpenResult, GenerateCommittedOpen, GenerateCommittedTarget, GenerateWindowCleanup};
use shell_contract::hosts::{DialogsHost, FileDialog, OsShellHost};
use shell_contract::ipc::{EventSink, NativeContext, Target};
use shell_contract::runtime::CancelToken;
use main_core::shell::generate::generate_api::GenerateWindowState;
use std::path::PathBuf;

/// Inserts this port's factory and the fake of its host, answering from the
/// case's `hosts.a` value; `log` is the case's host log
pub fn insert(hosts: &mut shell_contract::Hosts, case: &serde_json::Value, log: Option<Log>) {
    hosts.insert(crate::generate::factory());
    hosts.insert::<GenerateShellHost>(Arc::new(FakeGenerateShell::new(case, log)));
}

/// the lower-case name of a tab kind
fn tab_kind_of(name: &str) -> DocumentTabKind {
    match name {
        "docs" => DocumentTabKind::Docs,
        "sheets" => DocumentTabKind::Sheets,
        "slides" => DocumentTabKind::Slides,
        "pdf" => DocumentTabKind::Pdf,
        "markdown" => DocumentTabKind::Markdown,
        "html" => DocumentTabKind::Html,
        "mindmap" => DocumentTabKind::Mindmap,
        "hwp" => DocumentTabKind::Hwp,
        "epub" => DocumentTabKind::Epub,
        _ => DocumentTabKind::Diagram,
    }
}

/// whole numbers as integers, as JSON written by JavaScript has them
fn plain(value: Value) -> Value {
    match value {
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Value::from(f as i64),
            _ => Value::Number(n),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(plain).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, plain(v))).collect()),
        other => other,
    }
}

struct State {
    tabs: Vec<OpenDocumentTab>,
    created: usize,
    destroyed: Vec<u32>,
    /// `nativeTheme.on` listeners, one slot per registration
    watchers: Vec<Option<u32>>,
    listeners: Vec<(u32, BoundGenerateWindowEvents)>,
    released_conversions: usize,
}

struct ModeledCommittedTarget(shell_contract::runtime::RendererGeneration, CancelToken);

impl GenerateCommittedTarget for ModeledCommittedTarget {
    fn created(&self) -> &shell_contract::runtime::RendererGeneration { &self.0 }
    fn validate_target(&self) -> Result<(), String> {
        if self.0.is_active() && !self.1.is_cancelled() { Ok(()) } else { Err("modeled generated target was replaced".into()) }
    }
    fn with_terminal_commit(&self, effect: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        self.0.with_active(|| self.1.with_active(effect)).flatten().ok_or("modeled generated target was replaced")?
    }
}

pub struct FakeGenerateShell {
    spec: Value,
    lang: Lang,
    log: Option<Log>,
    state: Arc<Mutex<State>>,
}

impl FakeGenerateShell {
    pub fn new(case: &Value, log: Option<Log>) -> Self {
        let spec = case["generate"].clone();
        let tabs = spec["tabs"]
            .as_array()
            .map(|tabs| {
                tabs.iter()
                    .map(|t| OpenDocumentTab {
                        id: t["id"].as_str().unwrap_or_default().to_owned(),
                        kind: tab_kind_of(t["kind"].as_str().unwrap_or_default()),
                        title: t["title"].as_str().unwrap_or_default().to_owned(),
                        file_path: t["filePath"].as_str().map(str::to_owned),
                        active: t["active"].as_bool().unwrap_or(false),
                        dirty: t["dirty"].as_bool().unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        FakeGenerateShell {
            spec,
            lang: super::case_lang(case),
            log,
            state: Arc::new(Mutex::new(State {
                tabs,
                created: 0,
                destroyed: Vec::new(),
                watchers: Vec::new(),
                listeners: Vec::new(),
                released_conversions: 0,
            })),
        }
    }

    fn line(&self, line: Value) {
        push(&self.log, line);
    }

    /// One `parseFileToText` line, keeping the run of read lines at the log's end sorted by
    /// path: the TypeScript side starts the reads of a batch in no fixed order.
    fn read_line(&self, path: &str) {
        let Some(log) = &self.log else { return };
        let mut log = log.lock().expect("the log");
        let is_read = |line: &Value| line[0].as_str() == Some("parseFileToText");
        let start = log
            .iter()
            .rposition(|line| !is_read(line))
            .map_or(0, |i| i + 1);
        log.push(json!(["parseFileToText", path]));
        log[start..].sort_by(|a, b| a[1].as_str().cmp(&b[1].as_str()));
    }

    fn win(&self, window: WindowId, what: &str) {
        self.line(json!(["win", window.0, what]));
    }

    fn listed(&self, key: &str, id: u32) -> bool {
        self.spec[key]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|v| v.as_u64() == Some(u64::from(id))))
    }

    fn contents_spec(&self, tab_id: &str) -> Option<(u32, bool)> {
        let spec = self.spec["contents"].get(tab_id)?;
        Some((
            spec["id"].as_u64().map_or(u32::MAX, |id| id as u32),
            spec["destroyed"].as_bool().unwrap_or(false),
        ))
    }

    /// what a save does to the tabs showing this webContents
    fn saved(&self, web_contents_id: f64) {
        if !self.spec["cleanOnSave"].as_bool().unwrap_or(true) {
            return;
        }
        let mut state = self.state.lock().expect("the fake's state");
        for tab in state.tabs.iter_mut() {
            let wc = self.contents_spec(&tab.id).map(|(id, _)| f64::from(id));
            if wc == Some(web_contents_id) {
                tab.dirty = false;
            }
        }
    }

    fn save_result(&self, kind: DocumentTabKind) -> Result<bool, Failure> {
        match self.spec["saveResult"].get(kind.as_str()) {
            Some(Value::String(_)) => Err(Failure(format!("{} save failed", kind.as_str()))),
            Some(Value::Bool(false)) => Ok(false),
            _ => Ok(true),
        }
    }
}

fn opener_json(opener: Option<&NewDocumentOpener>) -> Value {
    match opener {
        None => Value::Null,
        Some(NewDocumentOpener::Home) => json!({"kind": "home"}),
        Some(NewDocumentOpener::Window { web_contents_id }) => {
            json!({"kind": "window", "webContentsId": web_contents_id})
        }
    }
}

impl GenerateShell for FakeGenerateShell {
    fn check_authority(&self, authority: &GenerateAuthority) -> Result<(), String> {
        // This is the non-native probe model, not production admission.
        authority.check_cancelled()
    }
    fn show_open_dialog(&self, authority: &GenerateAuthority, dialogs: DialogsHost, parent: WindowId, options: &FileDialog) -> Result<Option<Vec<PathBuf>>, String> {
        self.check_authority(authority)?;
        Ok(dialogs.open(Some(parent), options))
    }
    fn open_external(&self, authority: &GenerateAuthority, os: OsShellHost, url: &str) -> Result<(), String> {
        self.check_authority(authority)?;
        os.open_external(url)
    }
    fn send_state(&self, authority: &GenerateAuthority, events: Arc<dyn EventSink>, view: ViewId, state: &GenerateWindowState) -> Result<(), String> {
        self.check_authority(authority)?;
        let value = serde_json::to_value(state).map_err(|error| error.to_string())?;
        events.emit(Target::View(view), "generate:state", vec![crate::generate::ipc_of(&value)]);
        Ok(())
    }
    fn create_window(
        &self,
        _authority: &GenerateAuthority,
        options: &GenerateWindowOptions,
        events: GenerateWindowEvents,
    ) -> Result<WindowId, String> {
        let mut state = self.state.lock().expect("the fake's state");
        let id = self.spec["windows"]
            .get(state.created)
            .and_then(Value::as_u64)
            .map_or(50 + state.created as u32, |id| id as u32);
        state.created += 1;
        let events = events.bind(WindowId(id), NativeContext::new(("probe-generate-window", id)), CancelToken::default())?;
        state.listeners.push((id, events));
        drop(state);
        let options = plain(serde_json::to_value(options).expect("options print"));
        self.line(json!(["new BrowserWindow", id, options]));
        Ok(WindowId(id))
    }

    fn window_view(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<Option<ViewId>, String> {
        let state = self.state.lock().expect("the fake's state");
        Ok(state
            .listeners
            .iter()
            .any(|(id, _)| *id == window.0)
            .then_some(ViewId(window.0)))
    }

    fn window_is_destroyed(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<bool, String> {
        Ok(self.state
            .lock()
            .expect("the fake's state")
            .destroyed
            .contains(&window.0))
    }

    fn window_is_minimized(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<bool, String> {
        Ok(self.listed("minimized", window.0))
    }

    fn window_restore(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.win(window, "restore");
        Ok(())
    }

    fn window_show(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.win(window, "show");
        Ok(())
    }

    fn window_focus(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.win(window, "focus");
        Ok(())
    }

    fn window_close(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.win(window, "close");
        let events = {
            let mut state = self.state.lock().expect("the fake's state");
            state.destroyed.push(window.0);
            state
                .listeners
                .iter()
                .find(|(id, _)| *id == window.0)
                .map(|(_, events)| events.clone())
        };
        // Electron's `closed` follows `close()`
        if let Some(events) = events {
            events.closed();
        }
        Ok(())
    }

    fn window_remove_menu(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.win(window, "removeMenu");
        Ok(())
    }

    fn window_set_title_bar_overlay(
        &self,
        _authority: &GenerateAuthority,
        window: WindowId,
        overlay: &TitleBarOverlay,
    ) -> Result<(), String> {
        let overlay = plain(serde_json::to_value(overlay).expect("overlay prints"));
        self.line(json!(["win", window.0, "setTitleBarOverlay", overlay]));
        Ok(())
    }

    fn watch_theme(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        let mut state = self.state.lock().expect("the fake's state");
        state.watchers.push(Some(window.0));
        let slot = state.watchers.len();
        drop(state);
        self.line(json!(["nativeTheme.on", slot]));
        Ok(())
    }

    fn unwatch_theme(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        let slot = {
            let mut state = self.state.lock().expect("the fake's state");
            let at = state.watchers.iter().position(|w| *w == Some(window.0));
            if let Some(at) = at {
                state.watchers[at] = None;
            }
            at
        };
        if let Some(at) = slot {
            self.line(json!(["nativeTheme.off", at + 1]));
        }
        Ok(())
    }

    fn renderer_url(&self) -> Option<String> {
        self.spec["rendererUrl"].as_str().map(str::to_owned)
    }

    fn window_load_url(&self, _authority: &GenerateAuthority, window: WindowId, url: &str) -> Result<(), String> {
        self.line(json!(["win", window.0, "loadURL", url]));
        Ok(())
    }

    fn window_load_generate_page(&self, _authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.win(window, "loadFile");
        Ok(())
    }

    fn should_use_dark_colors(&self, _authority: &GenerateAuthority) -> Result<bool, String> {
        Ok(self.spec["dark"].as_bool().unwrap_or(false))
    }

    fn lang(&self) -> Lang {
        self.lang
    }

    fn theme(&self) -> String {
        self.spec["theme"].as_str().unwrap_or("light").to_owned()
    }

    fn generate_text(&self, _: Lang, key: GenerateText) -> String {
        let name = match key {
            GenerateText::WindowTitle => "windowTitle",
            GenerateText::DialogAddTitle => "dialogAddTitle",
            GenerateText::FilterSupported => "filterSupported",
            GenerateText::Untitled => "untitled",
        };
        self.spec["texts"][name].as_str().unwrap_or(name).to_owned()
    }

    fn starred_group_paths(&self, group: &str) -> Vec<String> {
        self.line(json!(["starredGroupPaths", group]));
        self.spec["starred"][group]
            .as_array()
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(|p| p.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn tab_id_for(&self, _: f64) -> Option<String> {
        None
    }

    fn open_target_tab(
        &self,
        _authority: &GenerateAuthority,
        handoff: GenerateHandoff,
        opener: Option<NewDocumentOpener>,
        request: GenerateOpenRequest,
        then: Box<dyn FnOnce(GenerateOpenResult) + Send>,
    ) {
        // the id is random and the time is the clock's: both are written fixed
        let mut shown = serde_json::to_value(handoff).expect("a hand-off prints");
        shown["id"] = json!("ID");
        shown["createdAt"] = json!(0);
        self.line(json!(["openTargetTab", plain(shown), opener_json(opener.as_ref())]));
        if self.spec["openTarget"].as_str() == Some("throws") {
            return then(Err("no tab could open".to_owned()));
        }
        let state = self.state.clone();
        let log = self.log.clone();
        let window = request.window;
        let cleanup = GenerateWindowCleanup::new(window, Box::new(move |done| {
            push(&log, json!(["win", window.0, "close"]));
            let events = {
                let mut state = state.lock().expect("the fake's state");
                state.destroyed.push(window.0);
                state.listeners.iter().find(|(id, _)| *id == window.0).map(|(_, events)| events.clone())
            };
            if let Some(events) = events { events.closed(); }
            done(Ok(()));
        }));
        then(GenerateCommittedOpen::new(request.token,
            Box::new(ModeledCommittedTarget(shell_contract::runtime::RendererGeneration::new(), CancelToken::default())), cleanup))
    }

    fn can_open_converted(&self) -> bool {
        false
    }

    fn open_converted_tab(
        &self,
        _authority: &GenerateAuthority,
        _: ConvertedDocument,
        _: Option<NewDocumentOpener>,
        _: GenerateOpenRequest,
        then: Box<dyn FnOnce(GenerateOpenResult) + Send>,
    ) {
        then(Err("conversions are not on offer".to_owned()))
    }

    fn open_documents(&self, _authority: &GenerateAuthority, then: Box<dyn FnOnce(Result<Vec<OpenDocumentTab>, String>) + Send>) {
        let tabs = self.state.lock().expect("the fake's state").tabs.clone();
        then(Ok(tabs))
    }

    fn tab_contents(&self, _authority: &GenerateAuthority, tab_id: &str) -> Result<Option<TabContents>, String> {
        self.line(json!(["webContentsFor", tab_id]));
        Ok(self.contents_spec(tab_id)
            .map(|(id, destroyed)| TabContents {
                view: ViewId(id),
                destroyed,
            }))
    }

    fn reveal_tab(&self, authority: &GenerateAuthority, tab_id: String, then: Box<dyn FnOnce(Result<(), String>) + Send>) {
        let result = self.check_authority(authority).map(|()| self.line(json!(["reveal", tab_id])));
        then(result);
    }

    fn has_save(&self, kind: DocumentTabKind) -> bool {
        self.spec["save"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some(kind.as_str())))
    }

    fn save(&self, _authority: &GenerateAuthority, need: SaveNeed, then: Box<dyn FnOnce(SaveAnswer) + Send>) {
        let answer = match need {
            SaveNeed::ToPath {
                kind,
                web_contents_id,
                path,
            } => {
                self.line(plain(json!(["save", kind.as_str(), web_contents_id, path])));
                let result = self.save_result(kind);
                if result.is_ok() {
                    self.saved(web_contents_id);
                }
                SaveAnswer::ToPath(result.map(|_| ()))
            }
            SaveNeed::Editor {
                kind,
                web_contents_id,
            } => {
                self.line(json!([
                    "save",
                    kind.as_str(),
                    format!("contents:{web_contents_id}")
                ]));
                let result = self.save_result(kind);
                if matches!(result, Ok(true)) {
                    self.saved(web_contents_id);
                }
                SaveAnswer::Editor(result)
            }
        };
        then(answer)
    }

    fn set_timeout(&self, _authority: &GenerateAuthority, _: f64, then: Box<dyn FnOnce() + Send>) {
        // the recheck: the probe does not wait for it
        then()
    }

    fn new_id(&self) -> String {
        "ID".to_owned()
    }

    fn now_ms(&self) -> f64 {
        0.0
    }

    fn parse_file_to_text(&self, _authority: &GenerateAuthority, path: &str) -> Result<ParsedFile, String> {
        self.read_line(path);
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        match self.spec["parse"].get(name) {
            Some(Value::String(_)) => Err("parse failed".to_owned()),
            Some(spec) => Ok(ParsedFile {
                ok: spec["ok"].as_bool().unwrap_or(false),
                error: spec["error"].as_str().map(str::to_owned),
                kind: spec["kind"].as_str().map(str::to_owned),
                text: spec["text"].as_str().map(str::to_owned),
            }),
            None => {
                let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
                Ok(ParsedFile {
                    ok: true,
                    error: None,
                    kind: Some("text".to_owned()),
                    text: Some(text),
                })
            }
        }
    }

    fn not_topic_marks(&self) -> [String; 2] {
        ["(boundary)".to_owned(), "(relationship)".to_owned()]
    }

    fn first_sheet_root(&self, _authority: &GenerateAuthority, _: &str, _: &str) -> Result<XmindTopic, String> {
        Err("conversions are not on offer".to_owned())
    }

    fn create_xmind_document_from_outline(&self, _authority: &GenerateAuthority, _: &str, _: &str) -> Result<(), String> {
        Err("conversions are not on offer".to_owned())
    }

    fn open_blank_deck(&self, _authority: &GenerateAuthority) -> Result<u64, String> {
        Err("conversions are not on offer".to_owned())
    }

    fn apply_deck_op(&self, _authority: &GenerateAuthority, _: u64, _: &JsonValue) -> Result<OpOutcome, String> {
        Err("conversions are not on offer".to_owned())
    }

    fn save_deck(&self, _authority: &GenerateAuthority, _: u64) -> Result<Vec<u8>, String> {
        Err("conversions are not on offer".to_owned())
    }
    fn release_conversion_resources(&self, _: &GenerateAuthority) {
        self.state.lock().expect("the probe's state").released_conversions += 1;
    }
}
