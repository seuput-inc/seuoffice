//! The fakes of the HTML editor's hosts, for unit tests and ipc-probe. The
//! TypeScript side of tools/ipc-diff (groups/d2/html-fakes.ts) answers the
//! same case keys and writes the same log lines.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use main_core::packages::electron_utils::safe_remote_url::{
    FetchError, FetchInit, Fetcher, GlobalDispatcher, LookupAddress, Resolver, Response,
};
use main_core::packages::electron_utils::unsaved_files::UNSAVED_CHANGES_MESSAGE;
use serde_json::{Value, json};
use shell_contract::{ViewId, WindowId};

use super::odt_export::{OdtConversion, ReportItem};
use super::{Handoff, HtmlEngine, HtmlShell, ParsedFile};

#[cfg(test)]
thread_local! {
    static PENDING_FILE_GUARD: std::cell::RefCell<Option<(Box<dyn FnOnce(Result<(), String>)>, Result<(), String>)>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(super) fn finish_file_guard() {
    let pending = PENDING_FILE_GUARD.with(|slot| slot.borrow_mut().take());
    if let Some((then, result)) = pending {
        then(result);
    }
}

/// ipc-probe's call log (tools/ipc-diff/README.md, "Batch hosts")
pub(crate) type ProbeLog = Arc<Mutex<Vec<Value>>>;

/// Answers by method name from a case object: `case[<method>]` is the
/// answer; an array is consumed one item per call (its last item repeats).
/// Every call is recorded as `"<method> <argument>"` for the unit tests; the
/// probe's log gets the lines a fake writes with [`Answers::log`].
pub(crate) struct Answers {
    case: Value,
    queued: Mutex<HashMap<String, VecDeque<Value>>>,
    pub calls: Mutex<Vec<String>>,
    probe: Option<ProbeLog>,
}

impl Answers {
    pub(crate) fn new(case: &Value, probe: Option<ProbeLog>) -> Self {
        let mut queued = HashMap::new();
        if let Value::Object(map) = case {
            for (k, v) in map {
                let q: VecDeque<Value> = match v {
                    Value::Array(items) => items.iter().cloned().collect(),
                    other => VecDeque::from([other.clone()]),
                };
                queued.insert(k.clone(), q);
            }
        }
        Answers {
            case: case.clone(),
            queued: Mutex::new(queued),
            calls: Mutex::new(Vec::new()),
            probe,
        }
    }

    /// one line of the probe's log
    pub(crate) fn log(&self, line: Value) {
        if let Some(probe) = &self.probe {
            probe.lock().unwrap().push(line)
        }
    }

    /// the case's key as written
    pub(crate) fn raw(&self, key: &str) -> Option<&Value> {
        self.case.get(key)
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.raw(key).is_some()
    }

    /// whether `path` is in the case's list under `key`
    pub(crate) fn listed(&self, key: &str, path: &str) -> bool {
        self.raw(key)
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|i| i.as_str() == Some(path)))
    }

    /// the answer for `method`, recording the call
    pub(crate) fn answer(&self, method: &str, arg: impl std::fmt::Display) -> Option<Value> {
        self.calls.lock().unwrap().push(format!("{method} {arg}"));
        let mut queued = self.queued.lock().unwrap();
        let q = queued.get_mut(method)?;
        if q.len() > 1 {
            q.pop_front()
        } else {
            q.front().cloned()
        }
    }

    pub(crate) fn bool(&self, method: &str, arg: impl std::fmt::Display, default: bool) -> bool {
        self.answer(method, arg)
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    }

    pub(crate) fn string(&self, method: &str, arg: impl std::fmt::Display) -> Option<String> {
        self.answer(method, arg)
            .and_then(|v| v.as_str().map(str::to_owned))
    }

    /// a byte answer: a string's UTF-8 bytes; `{ "error": text }` fails
    pub(crate) fn bytes(
        &self,
        method: &str,
        arg: impl std::fmt::Display,
        default: &[u8],
    ) -> Result<Vec<u8>, String> {
        match self.answer(method, arg) {
            Some(Value::String(s)) => Ok(s.into_bytes()),
            Some(Value::Object(o)) => Err(o
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("failed")
                .to_owned()),
            _ => Ok(default.to_vec()),
        }
    }

    /// `Date.now()`: the case's `now`, else the clock
    pub(crate) fn now_ms(&self) -> f64 {
        self.raw("now")
            .and_then(Value::as_f64)
            .unwrap_or_else(crate::text_recovery::ui::now_ms)
    }

    #[cfg(test)]
    pub(crate) fn called(&self, prefix: &str) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .cloned()
            .collect()
    }
}

/// `docxToOdt`'s answer from a case value: `{ odt, report }` or `{ error }`
pub(crate) fn odt_answer(answer: Option<Value>) -> Result<OdtConversion, String> {
    match answer {
        Some(Value::Object(o)) if o.contains_key("error") => {
            Err(o["error"].as_str().unwrap_or("failed").to_owned())
        }
        Some(Value::Object(o)) => Ok(OdtConversion {
            odt: o
                .get("odt")
                .and_then(Value::as_str)
                .unwrap_or("ODT")
                .as_bytes()
                .to_vec(),
            report: o
                .get("report")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|i| ReportItem {
                            kind: i["kind"].as_str().unwrap_or_default().to_owned(),
                            feature: i["feature"].as_str().unwrap_or_default().to_owned(),
                            count: i["count"].as_f64().unwrap_or(1.0),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }),
        _ => Ok(OdtConversion {
            odt: b"ODT".to_vec(),
            report: Vec::new(),
        }),
    }
}

/// the hex of bytes, as the log writes them
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// the text of a file the engine is handed, as the log writes it
pub(crate) fn file_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// A resolver and fetcher with no network: every lookup fails
pub(crate) struct NoNetwork;

impl Resolver for NoNetwork {
    fn lookup_all(&mut self, host: &str) -> Result<Vec<LookupAddress>, FetchError> {
        Err(FetchError(format!("getaddrinfo ENOTFOUND {host}")))
    }
}

impl Fetcher for NoNetwork {
    fn global_dispatcher(&mut self) -> GlobalDispatcher {
        GlobalDispatcher::NotAnObject
    }
    fn fetch<'signal>(
        &mut self,
        url: &str,
        _init: &FetchInit<'_, 'signal>,
    ) -> Result<Box<dyn Response + 'signal>, FetchError> {
        Err(FetchError(format!("fetch failed: {url}")))
    }
    fn close_dispatcher(&mut self) {}
    fn sleep(
        &mut self,
        _ms: f64,
        _: Option<&dyn main_core::packages::electron_utils::safe_remote_url::AbortSignal>,
    ) {
    }
}

/// The HTML editor's engine and shell hooks in one fake. Case keys, the
/// default in brackets; log lines in parentheses:
/// - engines: `print_pdf` [`"%PDF"`; `{ "error": text }` fails]
///   (`["printToPDF", text of the page]`), `html_to_docx` [`"DOCX"`]
///   (`["convertHtmlToDocx", text of the page]`), `docx_to_odt`
///   [`{ "odt": "ODT", "report": [] }`] (`["docxToOdt", hex]`),
///   `parse_file_to_text` [`{ "ok": true, "kind": "text", "text": "" }`]
///   (`["parseFileToText", path]`), `diagram_picture_ext_of` [null]; the
///   network always fails.
/// - process state: `is_headless` [false], `ui_lang` [`"en"`], `now`
///   [the clock], `open_elsewhere` [paths open in another pane: none],
///   `unsaved_files` [paths open with unsaved changes: none].
/// - hooks the shell may not set (absent: not called): `open_generated_path`
///   (`["openGeneratedPath", path]`), `new_document_dir` [a folder or null]
///   (`["newDocumentDir", view]`), `write_new_document` [a folder the file
///   is written into, or null] (`["writeNewDocument", view, name]`),
///   `docx_exported` [true sets the hook] (`["docxExportedHook", path]`),
///   `docx_export_prepare` [its answer] (`["docxExportPrepareHook", path]`),
///   `present_open_tab` (`["presentHooks.openTab", owner, title]`),
///   `present_host_window` [a window number].
/// - always called: `["fileSavedHook", view, path]`,
///   `["provisionalTitleHook", view, title]`,
///   `["presentHooks.setBleed", view, on]`, `["webContents.reload", view]`,
///   `["reportGenerateRunStarted", view, paths]` (`report_generate_run_started`
///   [false]), `["openPresentWindow"]` (`open_present_window` [the new
///   view's number, or null]).
/// - `take_generate_handoff`: `{ "<view>": { id, text, materials } }`, each taken once.
/// - macOS full screen: `["BrowserWindow.setFullScreenable", window, false]`
///   and `["BrowserWindow.setSimpleFullScreen", window, true]` to enter,
///   `["BrowserWindow.setFullScreenable", window, true]` to leave.
pub(crate) struct FakeHtml {
    pub answers: Arc<Answers>,
    taken: Mutex<Vec<u32>>,
    pub(crate) export_views: Mutex<Vec<ViewId>>,
    os: shell_contract::hosts::OsShellHost,
}

impl FakeHtml {
    pub(crate) fn new(
        case: &Value,
        probe: Option<ProbeLog>,
        os: shell_contract::hosts::OsShellHost,
    ) -> Self {
        FakeHtml {
            answers: Arc::new(Answers::new(case, probe)),
            taken: Mutex::default(),
            export_views: Mutex::default(),
            os,
        }
    }
}

impl HtmlEngine for FakeHtml {
    fn print_pdf(
        &self,
        view: ViewId,
        html: &str,
        doc_path: Option<&str>,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        self.export_views.lock().unwrap().push(view);
        #[cfg(test)]
        if self.answers.raw("cancel_print").and_then(Value::as_bool) == Some(true) {
            cancel.cancel();
        }
        #[cfg(not(test))]
        let _ = cancel;
        let base = doc_path.map(|path| {
            main_core::html::preview_document::asset_base_href(&jscompat::node_path::host::dirname(
                path,
            ))
        });
        let page = main_core::html::preview_document::build_preview_document(html, base.as_deref());
        self.answers.log(json!(["printToPDF", page]));
        self.answers.bytes("print_pdf", "print.html", b"%PDF")
    }
    fn print_pdf_owned(
        &self,
        _origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        html: &str,
        doc_path: Option<&str>,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        if cancel.is_cancelled() || pending.is_cancelled() {
            return Err("The export was cancelled".into());
        }
        self.print_pdf(destination.view, html, doc_path, cancel)
    }
    fn print_pdf_native(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        _participants: &[shell_contract::ipc::NativeRequestOwner],
        html: &str,
        doc_path: Option<&str>,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Vec<u8>, String> {
        if cancel.is_cancelled()
            || pending.is_cancelled()
            || additional.iter().any(|token| token.is_cancelled())
            || binding.operation().cancel().is_cancelled()
            || binding.operation().shutdown().is_cancelled()
        {
            return Err("The export was cancelled".into());
        }
        self.print_pdf(binding.owner().view, html, doc_path, cancel)
    }
    fn print_pdf_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &shell_contract::ipc::NativeRequestOwner,
        html: &str,
        doc_path: Option<&str>,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        if cancel.is_cancelled()
            || pending.is_cancelled()
            || document.operation().cancel().is_cancelled()
            || document.operation().shutdown().is_cancelled()
        {
            return Err("The export was cancelled".into());
        }
        self.print_pdf(owner.view, html, doc_path, cancel)
    }
    fn html_to_docx(
        &self,
        view: ViewId,
        html_file: &Path,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        self.export_views.lock().unwrap().push(view);
        #[cfg(test)]
        if self.answers.raw("cancel_docx").and_then(Value::as_bool) == Some(true) {
            cancel.cancel();
        }
        #[cfg(not(test))]
        let _ = cancel;
        self.answers
            .log(json!(["convertHtmlToDocx", file_text(html_file)]));
        self.answers
            .bytes("html_to_docx", html_file.display(), b"DOCX")
    }
    fn html_to_docx_owned(
        &self,
        _origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        path: &Path,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        if cancel.is_cancelled() || pending.is_cancelled() {
            return Err("The export was cancelled".into());
        }
        self.html_to_docx(destination.view, path, cancel)
    }
    fn html_to_docx_native(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        _participants: &[shell_contract::ipc::NativeRequestOwner],
        path: &Path,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Vec<u8>, String> {
        if cancel.is_cancelled()
            || pending.is_cancelled()
            || additional.iter().any(|token| token.is_cancelled())
            || binding.operation().cancel().is_cancelled()
            || binding.operation().shutdown().is_cancelled()
        {
            return Err("The export was cancelled".into());
        }
        self.html_to_docx(binding.owner().view, path, cancel)
    }
    fn html_to_docx_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &shell_contract::ipc::NativeRequestOwner,
        path: &Path,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Vec<u8>, String> {
        if cancel.is_cancelled()
            || pending.is_cancelled()
            || document.operation().cancel().is_cancelled()
            || document.operation().shutdown().is_cancelled()
        {
            return Err("The export was cancelled".into());
        }
        self.html_to_docx(owner.view, path, cancel)
    }
    fn docx_to_odt_cancellable(
        &self,
        docx: Vec<u8>,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<OdtConversion, String> {
        if cancel.is_cancelled() {
            return Err("The export was cancelled".into());
        }
        let converted = self.docx_to_odt(docx);
        if cancel.is_cancelled() {
            return Err("The export was cancelled".into());
        }
        converted
    }
    fn docx_to_odt(&self, docx: Vec<u8>) -> Result<OdtConversion, String> {
        self.answers.log(json!(["docxToOdt", hex(&docx)]));
        odt_answer(
            self.answers
                .answer("docx_to_odt", String::from_utf8_lossy(&docx)),
        )
    }
    fn parse_file_to_text(&self, path: &Path) -> ParsedFile {
        self.answers
            .log(json!(["parseFileToText", path.to_string_lossy()]));
        match self.answers.answer("parse_file_to_text", path.display()) {
            Some(Value::Object(o)) => ParsedFile {
                ok: o.get("ok").and_then(Value::as_bool).unwrap_or(false),
                kind: o.get("kind").and_then(Value::as_str).map(str::to_owned),
                text: o.get("text").and_then(Value::as_str).map(str::to_owned),
                damaged: o.get("damaged").and_then(Value::as_bool).unwrap_or(false),
                error: o.get("error").and_then(Value::as_str).map(str::to_owned),
            },
            _ => ParsedFile {
                ok: true,
                kind: Some("text".into()),
                text: Some(String::new()),
                damaged: false,
                error: None,
            },
        }
    }
    fn diagram_picture_ext_of(&self, path: &Path) -> Option<String> {
        self.answers
            .string("diagram_picture_ext_of", path.display())
    }
    fn resolver(&self) -> Box<dyn Resolver + Send> {
        Box::new(NoNetwork)
    }
    fn fetcher(&self) -> Box<dyn Fetcher + Send> {
        Box::new(NoNetwork)
    }
}

impl HtmlShell for FakeHtml {
    fn is_headless(&self) -> bool {
        self.answers.bool("is_headless", "", false)
    }
    fn ui_lang(&self) -> String {
        self.answers
            .string("ui_lang", "")
            .unwrap_or_else(|| "en".into())
    }
    fn now_ms(&self) -> f64 {
        self.answers.now_ms()
    }
    fn reload(&self, view: ViewId) {
        self.answers.answer("reload", view.0);
        self.answers.log(json!(["webContents.reload", view.0]));
    }
    fn is_path_open_elsewhere(&self, path: &str) -> bool {
        self.answers.listed("open_elsewhere", path)
    }
    fn assert_file_saved(&self, path: &str, then: Box<dyn FnOnce(Result<(), String>)>) {
        let result = if self.answers.listed("unsaved_files", path) {
            Err(UNSAVED_CHANGES_MESSAGE.to_owned())
        } else {
            Ok(())
        };
        #[cfg(test)]
        if self.answers.has("defer_file_guard") {
            PENDING_FILE_GUARD.with(|slot| *slot.borrow_mut() = Some((then, result)));
            return;
        }
        then(result);
    }
    fn open_generated_path(&self, path: &Path) -> bool {
        if !self.answers.has("open_generated_path") {
            return false;
        }
        self.answers
            .log(json!(["openGeneratedPath", path.to_string_lossy()]));
        self.answers
            .bool("open_generated_path", path.display(), false)
    }
    fn new_document_dir(&self, view: ViewId) -> Option<PathBuf> {
        if !self.answers.has("new_document_dir") {
            return None;
        }
        self.answers.log(json!(["newDocumentDir", view.0]));
        self.answers
            .string("new_document_dir", view.0)
            .map(Into::into)
    }
    fn write_new_document(
        &self,
        view: ViewId,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<PathBuf>, String> {
        if !self.answers.has("write_new_document") {
            return Ok(None);
        }
        self.answers
            .log(json!(["writeNewDocument", view.0, file_name]));
        let answer = self
            .answers
            .answer("write_new_document", format!("{} {file_name}", view.0));
        let dir: PathBuf = match answer {
            Some(Value::String(path)) => path.into(),
            Some(Value::Object(error)) => {
                return Err(error
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("failed")
                    .to_owned());
            }
            _ => return Ok(None),
        };
        let path = dir.join(file_name);
        std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
        Ok(Some(path))
    }
    fn write_new_document_owned(
        &self,
        _origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        _cancel: &shell_contract::runtime::CancelToken,
        _pending: &shell_contract::runtime::CancelToken,
        _effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<PathBuf>, String> {
        if !self.answers.has("write_new_document") {
            return Ok(None);
        }
        self.answers
            .log(json!(["writeNewDocument", destination.view.0, file_name]));
        let answer = self.answers.answer(
            "write_new_document",
            format!("{} {file_name}", destination.view.0),
        );
        let dir: PathBuf = match answer {
            Some(Value::String(path)) => path.into(),
            Some(Value::Object(error)) => {
                return Err(error
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("failed")
                    .to_owned());
            }
            _ => return Ok(None),
        };
        let path = dir.join(file_name);
        main_core::packages::electron_utils::atomic_write::owned::write_file(
            &path.to_string_lossy(),
            bytes,
            admission,
        )
        .map_err(|error| error.message)?;
        Ok(Some(path))
    }
    fn write_new_document_existing_document(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        _participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        _additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<PathBuf>, String> {
        let owner = binding.owner();
        self.write_new_document_owned(
            owner, owner, cancel, pending, effects, admission, file_name, bytes,
        )
    }
    fn asset_paths_changed(
        &self,
        view: ViewId,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<(), String> {
        self.answers.answer(
            "asset_paths_changed",
            format!("{} {tracked} {:?} {:?}", view.0, open, save),
        );
        Ok(())
    }
    fn asset_paths_changed_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        captured: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<(), String> {
        let destination = captured
            .first()
            .ok_or("the asset destination was not captured")?;
        let extra = additional.iter().collect::<Vec<_>>();
        effects.with_owned_effect_many_extra(origin, captured, cancel, pending, &extra, &mut || {
            self.asset_paths_changed(destination.view, tracked, open, save)
        })
    }
    fn prepare_native_save_asset_paths(
        &self,
        destination: &shell_contract::ipc::NativeRequestOwner,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<super::RecoveryAssetStage, String> {
        let answers = self.answers.clone();
        let view = destination.view;
        let open = open.map(str::to_owned);
        let save = save.map(str::to_owned);
        let mut once = Some(());
        Ok(super::RecoveryAssetStage {
            commit: Box::new(move || {
                once.take()
                    .ok_or("fake native save grant was already published")?;
                answers.answer(
                    "asset_paths_changed",
                    format!("{} {tracked} {:?} {:?}", view.0, open, save),
                );
                Ok(true)
            }),
            subscribe: Box::new(|| Box::new(|_| Ok(()))),
        })
    }
    fn prepare_saved_document_asset_paths(
        &self,
        destination: &shell_contract::ipc::NativeRequestOwner,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<super::SavedAssetStage, String> {
        let mut stage = self.prepare_native_save_asset_paths(destination, true, open, save)?;
        let published = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let committed = published.clone();
        let mut commit = stage.commit;
        stage.commit = Box::new(move || {
            let done = commit()?;
            if done {
                committed.store(true, std::sync::atomic::Ordering::SeqCst)
            }
            Ok(done)
        });
        let answers = self.answers.clone();
        let view = destination.view;
        let revoke = Box::new(move || {
            if !published.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("fake saved asset grant was not published".into());
            }
            answers.answer("saved_asset_revoke", view.0.to_string());
            Ok(())
        });
        Ok(super::SavedAssetStage { stage, revoke })
    }
    fn prepare_recovery_asset_paths(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<Box<dyn FnMut() -> Result<(), String> + Send>, String> {
        let answers = self.answers.clone();
        let view = target.view();
        let open = open.map(str::to_owned);
        let save = save.map(str::to_owned);
        let mut once = Some(());
        Ok(Box::new(move || {
            once.take()
                .ok_or("fake recovery asset grant was already published")?;
            answers.answer(
                "asset_paths_changed",
                format!("{} {tracked} {:?} {:?}", view.0, open, save),
            );
            Ok(())
        }))
    }
    fn prepare_rename_asset_paths_recovery(
        &self,
        _receipt: &shell_contract::ipc::RecoverySeedReceipt,
        _receiver: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        _participants: &[shell_contract::ipc::NativeRequestOwner],
        _cancel: &shell_contract::runtime::CancelToken,
        _pending: &shell_contract::runtime::CancelToken,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<super::RecoveryAssetStage, String> {
        let answers = self.answers.clone();
        let view = destination.view;
        let open = open.map(str::to_owned);
        let save = save.map(str::to_owned);
        let mut once = Some(());
        Ok(super::RecoveryAssetStage {
            commit: Box::new(move || {
                once.take()
                    .ok_or("fake recovery rename grant was already published")?;
                answers.answer(
                    "asset_paths_changed",
                    format!("{} {tracked} {:?} {:?}", view.0, open, save),
                );
                Ok(true)
            }),
            subscribe: Box::new(|| Box::new(|_| Ok(()))),
        })
    }
    fn asset_present_owner(&self, view: ViewId, owner: Option<ViewId>) -> Result<(), String> {
        self.answers.answer(
            "asset_present_owner",
            format!("{} {:?}", view.0, owner.map(|v| v.0)),
        );
        Ok(())
    }
    fn file_saved(&self, view: ViewId, path: &Path) {
        self.answers
            .answer("file_saved", format!("{} {}", view.0, path.display()));
        self.answers
            .log(json!(["fileSavedHook", view.0, path.to_string_lossy()]));
    }
    fn file_saved_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        path: PathBuf,
        then: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<PathBuf>, String>)>,
    ) {
        let extra = additional.iter().collect::<Vec<_>>();
        let result = effects.with_owned_effect_many_extra(
            &origin,
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || {
                self.file_saved(destination.view, &path);
                Ok(())
            },
        );
        then(result.map(|()| shell_contract::hosts::SavedFile {
            path,
            session: shell_contract::hosts::SavedSessionReceipt::NoPublication,
        }));
    }
    fn file_saved_native(
        &self,
        destination: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        then: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<PathBuf>, String>)>,
    ) {
        let path = destination.path().to_path_buf();
        let view = destination.binding().owner().view;
        let extra = additional.iter().collect::<Vec<_>>();
        let result = effects.with_existing_document_destination_effect(
            &destination,
            &path,
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || {
                self.file_saved(view, &path);
                Ok(())
            },
        );
        then(result.map(|()| shell_contract::hosts::SavedFile {
            path,
            session: shell_contract::hosts::SavedSessionReceipt::NoPublication,
        }));
    }
    fn provisional_title(&self, view: ViewId, title: &str) {
        self.answers
            .answer("provisional_title", format!("{} {title}", view.0));
        self.answers
            .log(json!(["provisionalTitleHook", view.0, title]));
    }
    fn docx_exported(&self, path: &Path) -> bool {
        if !self.answers.bool("docx_exported", path.display(), false) {
            return false;
        }
        self.answers
            .log(json!(["docxExportedHook", path.to_string_lossy()]));
        true
    }
    fn docx_export_prepare(&self, path: &Path, then: Box<dyn FnOnce(bool)>) {
        if !self.answers.has("docx_export_prepare") {
            return then(true);
        }
        self.answers
            .log(json!(["docxExportPrepareHook", path.to_string_lossy()]));
        then(
            self.answers
                .bool("docx_export_prepare", path.display(), true),
        )
    }
    fn docx_export_prepare_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
        path: PathBuf,
        then: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        let mut go = true;
        let result =
            effects.with_owned_effect(&origin, &destination, &cancel, &pending, &mut || {
                if self.answers.has("docx_export_prepare") {
                    self.answers
                        .log(json!(["docxExportPrepareHook", path.to_string_lossy()]));
                    go = self
                        .answers
                        .bool("docx_export_prepare", path.display(), true);
                }
                Ok(())
            });
        then(result.map(|()| go));
    }
    fn docx_export_prepare_native(
        &self,
        selection: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        then: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        let path = selection.path().to_path_buf();
        let extra = additional.iter().collect::<Vec<_>>();
        let mut go = true;
        let result = effects.with_existing_document_destination_effect(
            &selection,
            &path,
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || {
                if self.answers.has("docx_export_prepare") {
                    self.answers
                        .log(json!(["docxExportPrepareHook", path.to_string_lossy()]));
                    go = self
                        .answers
                        .bool("docx_export_prepare", path.display(), true);
                }
                Ok(())
            },
        );
        then(result.map(|()| go));
    }
    fn export_written_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
        path: PathBuf,
        kind: &'static str,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let result =
            effects.with_owned_effect(&origin, &destination, &cancel, &pending, &mut || {
                if !self.is_headless() {
                    let opened = match kind {
                        "pdf" => self.open_generated_path(&path),
                        "docx" | "odt" => self.docx_exported(&path),
                        _ => false,
                    };
                    if !opened {
                        self.os.show_item_in_folder(&path);
                    }
                }
                Ok(())
            });
        then(result);
    }
    fn take_generate_handoff_owned(
        &self,
        consumer: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Option<Handoff>, String> {
        if cancel.is_cancelled() {
            return Err("the handoff request was cancelled".into());
        }
        let view = consumer.view;
        Ok((|| {
            let o = self
                .answers
                .raw("take_generate_handoff")?
                .get(view.0.to_string())?
                .as_object()?
                .clone();
            let mut taken = self.taken.lock().unwrap();
            if taken.contains(&view.0) {
                return None;
            }
            taken.push(view.0);
            Some(Handoff {
                id: o.get("id")?.as_str()?.to_owned(),
                text: o.get("text")?.as_str()?.to_owned(),
                materials: o
                    .get("materials")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .map(|m| {
                                (
                                    m["path"].as_str().unwrap_or_default().to_owned(),
                                    m["name"].as_str().unwrap_or_default().to_owned(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })())
    }
    fn report_generate_run_started_owned(
        &self,
        consumer: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        paths: &[String],
    ) -> Result<bool, String> {
        if cancel.is_cancelled() {
            return Err("the handoff request was cancelled".into());
        }
        let view = consumer.view;
        self.answers
            .log(json!(["reportGenerateRunStarted", view.0, paths]));
        Ok(self.answers.bool(
            "report_generate_run_started",
            format!("{} {}", view.0, paths.join(",")),
            false,
        ))
    }
    fn present_open_tab(&self, owner: ViewId, title: &str, then: Box<dyn FnOnce(bool)>) {
        if !self.answers.has("present_open_tab") {
            return then(false);
        }
        self.answers
            .log(json!(["presentHooks.openTab", owner.0, title]));
        then(
            self.answers
                .bool("present_open_tab", format!("{} {title}", owner.0), false),
        )
    }
    fn present_host_window(&self, view: ViewId) -> Option<WindowId> {
        self.answers
            .answer("present_host_window", view.0)
            .and_then(|v| v.as_u64())
            .map(|n| WindowId(n as u32))
    }
    fn present_set_bleed(&self, view: ViewId, on: bool) {
        self.answers
            .answer("present_set_bleed", format!("{} {on}", view.0));
        self.answers
            .log(json!(["presentHooks.setBleed", view.0, on]));
    }
    fn present_enter_full_screen(&self, window: WindowId) {
        self.answers.answer("present_enter_full_screen", window.0);
        if cfg!(target_os = "macos") {
            self.answers
                .log(json!(["BrowserWindow.setFullScreenable", window.0, false]));
            self.answers
                .log(json!(["BrowserWindow.setSimpleFullScreen", window.0, true]));
        }
    }
    fn present_leave_full_screen(&self, window: WindowId) {
        self.answers.answer("present_leave_full_screen", window.0);
        if cfg!(target_os = "macos") {
            self.answers
                .log(json!(["BrowserWindow.setFullScreenable", window.0, true]));
        }
    }
    fn is_window_destroyed(&self, window: WindowId) -> bool {
        self.answers.bool("is_window_destroyed", window.0, false)
    }
    fn open_present_window(
        &self,
        owner: ViewId,
        title: &str,
        _parent: Option<WindowId>,
    ) -> Option<ViewId> {
        self.answers.log(json!(["openPresentWindow"]));
        self.answers
            .answer("open_present_window", format!("{} {title}", owner.0))
            .and_then(|v| v.as_u64())
            .map(|n| ViewId(n as u32))
    }
    fn close_present_view(&self, view: ViewId) {
        self.answers.answer("close_present_view", view.0);
        self.answers.log(json!(["closePresentView", view.0]));
    }
}
