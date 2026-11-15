//! Fakes of batch M's hosts for crates/ipc-probe (tools/ipc-diff) and this
//! crate's tests. Every answer comes from the case's `hosts.m` object (keys
//! below, each with a default); every call that does something is one line
//! of the case's host log (`Arc<Mutex<Vec<Value>>>` in `Hosts`), a JSON
//! array named after the Electron or app call it fakes. The TypeScript side
//! of the groups (tools/ipc-diff/groups/m) logs the same lines.
//!
//! ```json
//! { "now": 1700000000000, "appLocale": "en-US", "systemLocale": "en-US",
//!   "envLang": null, "username": "tester", "revealHome": true,
//!   "isPackaged": false, "appPath": "/app/out", "resourcesPath": "/app/resources",
//!   "headless": false, "openGenerated": false, "defaultSaveDir": "/docs",
//!   "spellLanguages": ["en-US"], "spellAvailable": ["en-US", "ko"], "addWord": true,
//!   "printers": [{ "name": "q", "displayName": "Q" }],
//!   "print": { "success": true, "failureReason": "" }, "pdf": "%PDF-fake",
//!   "pdfError": null, "loadError": null, "exec": "settled",
//!   "commands": { "/usr/bin/lp": { "code": 0, "stdout": "", "stderr": "" } },
//!   "familyInfo": [], "metrics": {}, "aliases": {}, "editFonts": [] }
//! ```

use std::sync::{Arc, Mutex};

use main_core::packages::electron_utils::print_window::{ExecOutcome, PrintReport};
use main_core::packages::electron_utils::printer_options::CommandOutcome;
use serde_json::{Value, json};
use shell_contract::{Hosts, ViewId};

use crate::hosts::{
    FontQueries, FontQueriesRef, KeyInput, MHost, MHostRef, PrintProfile, PrintSurface,
    PrintWindowSpec, Printer,
};
use crate::state::{MState, MStateRef};

/// The case's host log, shared with the other batches' fakes
pub type ProbeLog = Arc<Mutex<Vec<Value>>>;

struct SyntheticGeneratedState {
    observer: Option<shell_contract::hosts::GeneratedPathObserver>,
    wake: Option<shell_contract::hosts::GeneratedPathWake>,
    restored: Option<Result<shell_contract::hosts::GeneratedPathOutcome, String>>,
    observed: bool,
    acknowledged: bool,
    cancelling: bool,
}
struct SyntheticGeneratedTask {
    this: std::sync::Weak<SyntheticGeneratedTask>,
    state: Mutex<SyntheticGeneratedState>,
}
impl SyntheticGeneratedTask {
    fn new() -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            state: Mutex::new(SyntheticGeneratedState {
                observer: None,
                wake: None,
                restored: None,
                observed: false,
                acknowledged: false,
                cancelling: false,
            }),
        })
    }
    fn deliver(&self, result: Result<shell_contract::hosts::GeneratedPathOutcome, String>) {
        let (observer, wake) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            (state.observer.take(), state.wake.clone())
        };
        if let (Some(observer), Some(task)) = (observer, self.this.upgrade()) {
            observer(shell_contract::hosts::GeneratedTerminalDelivery::new(
                result, task,
            ));
        } else {
            shell_contract::hosts::GeneratedTerminalAck::restore(self, result);
        }
        if let Some(wake) = wake {
            wake();
        }
    }
}
impl shell_contract::hosts::GeneratedTerminalAck for SyntheticGeneratedTask {
    fn complete(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.acknowledged {
            return Err("The synthetic generated output was already acknowledged".into());
        }
        state.acknowledged = true;
        Ok(())
    }
    fn restore(&self, result: Result<shell_contract::hosts::GeneratedPathOutcome, String>) {
        let wake = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.restored = Some(result);
            state.wake.clone()
        };
        if let Some(wake) = wake {
            wake();
        }
    }
}
impl shell_contract::hosts::GeneratedPathPending for SyntheticGeneratedTask {
    fn observe(
        &self,
        observer: shell_contract::hosts::GeneratedPathObserver,
        wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<(), shell_contract::hosts::GeneratedObserveFailure> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.observed {
            return Err(shell_contract::hosts::GeneratedObserveFailure {
                error: "The synthetic observer is already installed".into(),
                observer,
                wake,
            });
        }
        state.observed = true;
        state.observer = Some(observer);
        state.wake = Some(wake);
        Ok(())
    }
    fn wake_observer(&self) -> Result<(), String> {
        let wake = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .wake
            .clone()
            .ok_or("The synthetic observer is unavailable")?;
        wake();
        Ok(())
    }
    fn take_redelivery(
        &self,
    ) -> Result<Option<shell_contract::hosts::GeneratedTerminalDelivery>, String> {
        let result = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .restored
            .take();
        let task = self
            .this
            .upgrade()
            .ok_or("The synthetic task is unavailable")?;
        Ok(
            result
                .map(|result| shell_contract::hosts::GeneratedTerminalDelivery::new(result, task)),
        )
    }
    fn is_cancelling(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancelling
    }
    fn retry(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        done(Err("No synthetic retry is pending".into()));
    }
    fn cancel_and_drain(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        let (wake, acknowledged) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.cancelling = true;
            (state.wake.clone(), state.acknowledged)
        };
        if let Some(wake) = wake {
            wake();
        }
        done(if acknowledged {
            Ok(())
        } else {
            Err("The synthetic output has not been acknowledged".into())
        });
    }
}
struct SyntheticGeneratedPrepared {
    task: Arc<SyntheticGeneratedTask>,
    action:
        Option<Box<dyn FnOnce() -> Result<shell_contract::hosts::GeneratedPathOutcome, String>>>,
}
impl shell_contract::hosts::PreparedGeneratedPath for SyntheticGeneratedPrepared {
    fn task(&self) -> Arc<dyn shell_contract::hosts::GeneratedPathPending> {
        self.task.clone()
    }
    fn start(mut self: Box<Self>) -> Result<(), shell_contract::hosts::GeneratedStartFailure> {
        let observed = self
            .task
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observed;
        if !observed {
            return Err(shell_contract::hosts::GeneratedStartFailure {
                error: "The synthetic observer is not installed".into(),
                prepared: self,
            });
        }
        if let Some(action) = self.action.take() {
            self.task.deliver(action());
        }
        Ok(())
    }
}
impl Drop for SyntheticGeneratedPrepared {
    fn drop(&mut self) {
        if self.action.take().is_some() {
            self.task
                .deliver(Err("The synthetic generated output was cancelled".into()));
        }
    }
}

pub struct Fake {
    config: Value,
    log: ProbeLog,
    /// what `setSpellCheckerLanguages` set, read back by `getSpellCheckerLanguages`
    languages: Mutex<Option<Vec<String>>>,
}

impl Fake {
    fn get(&self, key: &str) -> Option<&Value> {
        self.config.get(key).filter(|v| !v.is_null())
    }

    fn str_or(&self, key: &str, default: &str) -> String {
        self.get(key)
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_owned()
    }

    fn bool_or(&self, key: &str, default: bool) -> bool {
        self.get(key).and_then(Value::as_bool).unwrap_or(default)
    }

    fn strings(&self, key: &str, default: &[&str]) -> Vec<String> {
        match self.get(key).and_then(Value::as_array) {
            Some(items) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            None => default.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn log(&self, line: Value) {
        self.log.lock().unwrap().push(line)
    }
}

impl MHost for Fake {
    fn now_ms(&self) -> f64 {
        self.get("now")
            .and_then(Value::as_f64)
            .unwrap_or(1_700_000_000_000.0)
    }

    fn app_locale(&self) -> String {
        self.str_or("appLocale", "en-US")
    }

    fn system_locale(&self) -> String {
        self.str_or("systemLocale", "en-US")
    }

    fn env_lang(&self) -> Option<String> {
        self.get("envLang")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn username(&self) -> Option<String> {
        match self.config.get("username") {
            Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            _ => Some("tester".to_owned()),
        }
    }

    fn set_native_theme(&self, theme: &str) {
        self.log(json!(["nativeTheme.themeSource", theme]))
    }

    fn refresh_title_bar_overlay(&self) {
        self.log(json!(["refreshTitleBarOverlay"]))
    }

    fn rebuild_menus_for_language(&self) {
        self.log(json!(["rebuildMenusForLanguage"]))
    }

    fn sync_tray_menu(&self) {
        self.log(json!(["syncTrayMenu"]))
    }

    fn reveal_home(&self) -> bool {
        self.log(json!(["revealHome"]));
        self.bool_or("revealHome", true)
    }

    fn is_packaged(&self) -> bool {
        self.bool_or("isPackaged", false)
    }

    fn app_path(&self) -> String {
        self.str_or("appPath", "/app/apps/shell/out")
    }

    fn resources_path(&self) -> String {
        self.str_or("resourcesPath", "/app/resources")
    }

    fn headless(&self) -> bool {
        self.bool_or("headless", false)
    }

    fn open_generated_path(&self, path: &str) -> bool {
        self.log(json!(["openGeneratedPath", path]));
        self.bool_or("openGenerated", false)
    }
    fn export_written_owned(
        &self,
        _path: std::path::PathBuf,
        _origin: shell_contract::ipc::NativeRequestOwner,
        _destination: shell_contract::ipc::NativeRequestOwner,
        _cancel: shell_contract::runtime::CancelToken,
        _pending: shell_contract::runtime::CancelToken,
        _effects: shell_contract::hosts::OwnedEffectsHost,
        _accepted: shell_contract::hosts::PdfReloadAccepted,
        _wake: shell_contract::hosts::GeneratedPathWake,
        done: Box<
            dyn FnOnce(Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String>)
                + Send,
        >,
    ) {
        done(Err("No generated-path native receipt was scripted".into()));
    }
    fn export_written_existing_document(
        &self,
        selection: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        _effects: shell_contract::hosts::OwnedEffectsHost,
        additional: Vec<shell_contract::runtime::CancelToken>,
        _accepted: shell_contract::hosts::PdfReloadAccepted,
        _wake: shell_contract::hosts::GeneratedPathWake,
        done: Box<
            dyn FnOnce(Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String>)
                + Send,
        >,
    ) {
        let log = self.log.clone();
        let should_open = self.bool_or("openGenerated", false);
        let task = SyntheticGeneratedTask::new();
        let action = Box::new(move || {
            let mut opened = None;
            shell_contract::hosts::synthetic::with_existing_document_save_extra(
                selection.binding(),
                &participants,
                &cancel,
                &pending,
                &additional.iter().collect::<Vec<_>>(),
                &mut || {
                    log.lock().unwrap().push(json!([
                        "openGeneratedPath",
                        selection.path().to_string_lossy()
                    ]));
                    opened = Some(should_open);
                    Ok(())
                },
            )?;
            match opened {
                Some(false) => Ok(shell_contract::hosts::GeneratedPathOutcome::NotOpened),
                Some(true) => Err("No generated-path native receipt was scripted".into()),
                None => Err("Synthetic generated path was not admitted".into()),
            }
        });
        done(Ok(Box::new(SyntheticGeneratedPrepared {
            task,
            action: Some(action),
        })));
    }

    fn default_save_dir(&self) -> String {
        self.str_or("defaultSaveDir", "/docs")
    }

    fn spell_add_word(&self, view: ViewId, word: &str) -> bool {
        self.log(json!([
            "session.addWordToSpellCheckerDictionary",
            view.0,
            word
        ]));
        self.bool_or("addWord", true)
    }

    fn spell_remove_word(&self, word: &str) {
        self.log(json!([
            "session.removeWordFromSpellCheckerDictionary",
            word
        ]))
    }

    fn spell_languages(&self, _view: ViewId) -> Vec<String> {
        let set = self.languages.lock().unwrap().clone();
        set.unwrap_or_else(|| self.strings("spellLanguages", &["en-US"]))
    }

    fn spell_available_languages(&self, _view: ViewId) -> Vec<String> {
        self.strings("spellAvailable", &["en-US", "ko"])
    }

    fn spell_set_languages(&self, view: ViewId, languages: &[String]) {
        self.log(json!([
            "session.setSpellCheckerLanguages",
            view.0,
            languages
        ]));
        *self.languages.lock().unwrap() = Some(languages.to_vec());
    }

    fn replace_misspelling(&self, view: ViewId, word: &str) {
        self.log(json!(["webContents.replaceMisspelling", view.0, word]))
    }

    fn send_space(&self, view: ViewId, input: KeyInput) {
        let event = match input {
            KeyInput::KeyDown => json!({ "type": "keyDown", "keyCode": "Space" }),
            KeyInput::Char => json!({ "type": "char", "keyCode": " " }),
            KeyInput::KeyUp => json!({ "type": "keyUp", "keyCode": "Space" }),
        };
        self.log(json!(["webContents.sendInputEvent", view.0, event]))
    }

    fn printers(&self, _view: ViewId) -> Vec<Printer> {
        let Some(items) = self.get("printers").and_then(Value::as_array) else {
            return vec![Printer {
                name: "office".into(),
                display_name: "Office".into(),
            }];
        };
        items
            .iter()
            .map(|p| Printer {
                name: p["name"].as_str().unwrap_or("").to_owned(),
                display_name: p["displayName"].as_str().unwrap_or("").to_owned(),
            })
            .collect()
    }

    fn print_window(&self, spec: &PrintWindowSpec) -> Box<dyn PrintSurface> {
        let profile = match spec.profile {
            PrintProfile::Scripted => "scripted",
            PrintProfile::Static => "static",
            PrintProfile::Diagram => "diagram",
            PrintProfile::MindmapExport => "mindmap-export",
            PrintProfile::MindmapPrint => "mindmap-export",
        };
        self.log(json!(["new BrowserWindow", profile]));
        Box::new(FakeSurface {
            config: self.config.clone(),
            log: self.log.clone(),
            destroyed: false,
        })
    }

    fn print_window_owned(
        &self,
        spec: &PrintWindowSpec,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn PrintSurface>, String> {
        let source = origin
            .context
            .renderer()
            .ok_or("missing fake print origin")?;
        let target = destination
            .context
            .renderer()
            .ok_or("missing fake print destination")?;
        source
            .with_active_pair(target, || {
                shell_contract::runtime::CancelToken::with_active_all(
                    [Some(cancel), Some(pending)],
                    || self.print_window(spec),
                )
            })
            .flatten()
            .ok_or_else(|| "print request expired".into())
    }

    fn print_window_native_invocation(
        &self,
        _spec: &PrintWindowSpec,
        _document: &shell_contract::ipc::NativeInvocationDocument,
        _sender: &shell_contract::ipc::NativeRequestOwner,
        _cancel: &shell_contract::runtime::CancelToken,
        _pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn PrintSurface>, String> {
        Err("No native invocation print surface was scripted".into())
    }

    fn print_window_existing_document(
        &self,
        spec: &PrintWindowSpec,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Box<dyn PrintSurface>, String> {
        let mut surface = None;
        shell_contract::hosts::synthetic::with_existing_document_save_extra(
            binding,
            participants,
            cancel,
            pending,
            &additional.iter().collect::<Vec<_>>(),
            &mut || {
                surface = Some(self.print_window(spec));
                Ok(())
            },
        )?;
        Ok(Box::new(SyntheticOwnedSurface {
            surface: surface.ok_or("Synthetic print window was not admitted")?,
            binding: binding.clone(),
            participants: participants.to_vec(),
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
        }))
    }

    fn run_command(&self, file: &str, args: &[String], stdin: Option<&[u8]>) -> CommandOutcome {
        self.log(json!([
            "execFile",
            file,
            args,
            stdin.map_or(0, <[u8]>::len)
        ]));
        match self.get("commands").and_then(|c| c.get(file)) {
            Some(out) => CommandOutcome::Exited {
                code: out["code"].as_i64().map(|c| c as i32),
                stdout: out["stdout"].as_str().unwrap_or("").as_bytes().to_vec(),
                stderr: out["stderr"].as_str().unwrap_or("").as_bytes().to_vec(),
            },
            None => CommandOutcome::Failed(format!("spawn {file} ENOENT")),
        }
    }
}

struct SyntheticOwnedSurface {
    surface: Box<dyn PrintSurface>,
    binding: shell_contract::ipc::NativeExistingDocumentBinding,
    participants: Vec<shell_contract::ipc::NativeRequestOwner>,
    cancel: shell_contract::runtime::CancelToken,
    pending: shell_contract::runtime::CancelToken,
    additional: Vec<shell_contract::runtime::CancelToken>,
}

impl SyntheticOwnedSurface {
    fn run<T>(&mut self, action: impl FnOnce(&mut dyn PrintSurface) -> T) -> Result<T, String> {
        let mut action = Some(action);
        let mut result = None;
        shell_contract::hosts::synthetic::with_existing_document_save_extra(
            &self.binding,
            &self.participants,
            &self.cancel,
            &self.pending,
            &self.additional.iter().collect::<Vec<_>>(),
            &mut || {
                result = Some(action.take().ok_or("Synthetic print called twice")?(
                    &mut *self.surface,
                ));
                Ok(())
            },
        )?;
        result.ok_or_else(|| "Synthetic print was not admitted".into())
    }
}
impl PrintSurface for SyntheticOwnedSurface {
    fn load_file(&mut self, path: &str) -> Result<(), String> {
        self.run(|surface| surface.load_file(path))?
    }
    fn load_url(&mut self, url: &str) -> Result<(), String> {
        self.run(|surface| surface.load_url(url))?
    }
    fn execute_javascript(
        &mut self,
        script: &str,
        gesture: bool,
        race_ms: Option<f64>,
    ) -> ExecOutcome {
        self.run(|surface| surface.execute_javascript(script, gesture, race_ms))
            .unwrap_or_else(ExecOutcome::Throws)
    }
    fn print(&mut self, options: &Value) -> PrintReport {
        self.run(|surface| surface.print(options))
            .unwrap_or_else(|failure_reason| PrintReport {
                success: false,
                failure_reason,
            })
    }
    fn print_to_pdf(&mut self, options: &Value) -> Result<Vec<u8>, String> {
        self.run(|surface| surface.print_to_pdf(options))?
    }
    fn show(&mut self) {
        let _ = self.run(|surface| surface.show());
    }
    fn focus(&mut self) {
        let _ = self.run(|surface| surface.focus());
    }
    fn is_destroyed(&self) -> bool {
        self.surface.is_destroyed()
    }
    fn destroy(&mut self) {
        self.surface.destroy();
    }
}

/// A hidden print window. Paths are logged by file name: the folder they sit
/// in has a random suffix (`mkdtemp`)
struct FakeSurface {
    config: Value,
    log: ProbeLog,
    destroyed: bool,
}

impl FakeSurface {
    fn log(&self, line: Value) {
        self.log.lock().unwrap().push(line)
    }

    fn loaded(&self) -> Result<(), String> {
        match self.config.get("loadError").and_then(Value::as_str) {
            Some(error) => Err(error.to_owned()),
            None => Ok(()),
        }
    }
}

impl PrintSurface for FakeSurface {
    fn load_file(&mut self, path: &str) -> Result<(), String> {
        let name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // the document the window prints, as the app wrote it
        let content = std::fs::read_to_string(path).unwrap_or_default();
        self.log(json!(["win.loadFile", name, content]));
        self.loaded()
    }

    fn load_url(&mut self, url: &str) -> Result<(), String> {
        // the token at the end is random
        let (base, token) = url.rsplit_once('/').unwrap_or((url, ""));
        // what the diagram protocol would serve for the token, once
        let content = crate::print::take_diagram_print_document(token);
        self.log(json!(["win.loadURL", base, content]));
        self.loaded()
    }

    fn execute_javascript(
        &mut self,
        _script: &str,
        _user_gesture: bool,
        _race_ms: Option<f64>,
    ) -> ExecOutcome {
        self.log(json!(["win.webContents.executeJavaScript"]));
        match self.config.get("exec").and_then(Value::as_str) {
            Some("pending") => ExecOutcome::Pending,
            Some("rejected") => ExecOutcome::Rejected("probe failed".into()),
            _ => ExecOutcome::Settled,
        }
    }

    fn print(&mut self, options: &Value) -> PrintReport {
        self.log(json!(["win.webContents.print", options]));
        let print = self.config.get("print").cloned().unwrap_or(json!({}));
        PrintReport {
            success: print["success"].as_bool().unwrap_or(true),
            failure_reason: print["failureReason"].as_str().unwrap_or("").to_owned(),
        }
    }

    fn print_to_pdf(&mut self, options: &Value) -> Result<Vec<u8>, String> {
        self.log(json!(["win.webContents.printToPDF", options]));
        match self.config.get("pdfError").and_then(Value::as_str) {
            Some(error) => Err(error.to_owned()),
            None => Ok(self
                .config
                .get("pdf")
                .and_then(Value::as_str)
                .unwrap_or("%PDF-fake")
                .as_bytes()
                .to_vec()),
        }
    }

    fn show(&mut self) {
        self.log(json!(["win.show"]))
    }

    fn focus(&mut self) {
        self.log(json!(["win.focus"]))
    }

    fn is_destroyed(&self) -> bool {
        self.destroyed
    }

    fn destroy(&mut self) {
        self.log(json!(["win.destroy"]));
        self.destroyed = true
    }
}

impl FontQueries for Fake {
    fn family_info_for(&self, families: &Value) -> Value {
        self.log(json!(["familyInfoFor", families]));
        self.get("familyInfo").cloned().unwrap_or(json!([]))
    }

    fn family_vertical_metrics(&self, family: &str) -> Option<Value> {
        self.get("metrics")?.get(family).cloned()
    }

    fn family_aliases(&self, name: &str) -> Vec<String> {
        self.get("aliases")
            .and_then(|a| a.get(name))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn list_edit_fonts(&self) -> Vec<String> {
        self.strings("editFonts", &[])
    }
}

/// Inserts the batch's fakes, logging to the case's log (a fresh one when
/// the caller inserted none)
pub fn insert(hosts: &mut Hosts, case: &Value) -> ProbeLog {
    let log = match hosts.get::<ProbeLog>() {
        Some(log) => log.clone(),
        None => {
            let log = ProbeLog::default();
            hosts.insert(log.clone());
            log
        }
    };
    let fake = Arc::new(Fake {
        config: case.clone(),
        log: log.clone(),
        languages: Mutex::default(),
    });
    hosts.insert::<MHostRef>(fake.clone());
    hosts.insert::<FontQueriesRef>(fake);
    hosts.insert::<MStateRef>(Arc::new(MState::new()));
    log
}
