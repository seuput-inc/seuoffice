//! What the Word editor reaches that is not in Rust yet ([`DocsEngine`]: the
//! JavaScript libraries and the main-process modules still to port) and what
//! the shell does for it beyond the shared hosts ([`DocsShell`]: shell hooks,
//! menus, Electron calls the shared hosts do not cover). The shell implements
//! both; [`fakes`] answers from a case for the tests and ipc-probe.

use std::sync::Arc;
use std::time::Duration;

use main_core::docs::doc_format::Materialized;
use main_core::docs::byte_handoff::MediaPermit;
use main_core::docs::hwp_export::{HwpConversion, HwpError, HwpFormat};
use main_core::docs::lazy_media::{LazyMediaBody, OpenedLazyDocx};
use main_core::packages::electron_utils::safe_remote_url::Response;
use main_core::packages::i18n::Lang;
use shell_contract::ViewId;
use shell_contract::runtime::CancelToken;

/// Why `decryptDocx` refused (`DocxDecryptError`): `reason` and `message`
#[derive(Clone, Debug, PartialEq)]
pub struct DecryptError {
    /// `'wrong-password'` or `'unsupported'`
    pub reason: &'static str,
    pub message: String,
}

/// What `docs:analyze-media` hands to `analyzeMediaTool`
#[derive(Clone, Debug, PartialEq)]
pub struct AnalyzeMediaRequest {
    /// `SETTINGS_PATH()`: `<userData>/ai-settings.json`
    pub settings_path: String,
    pub media_urls: Vec<String>,
    pub requirements: String,
    /// `documentMediaRoots(openDocByWc.get(id), <temp>/seuoffice-pasted)`'s two inputs
    pub open_doc: Option<String>,
    pub pasted_dir: String,
}

/// The libraries of the Word editor's main process. Every method runs on a
/// worker: it takes and returns owned data. An `Err(String)` is the message
/// of what the TypeScript threw.
pub trait DocsEngine: Send + Sync {
    /// `openLazyDocx`: `None` is the original cache-optimization fallback
    /// for a small, encrypted or unreadable ZIP (apps/docs/src/main/lazy-media.ts:79-103).
    /// A host transport failure is not that fallback and is returned.
    fn open_lazy_docx(&self, path: &str, file: Option<std::sync::Arc<std::fs::File>>, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<Option<OpenedLazyDocx>, String>;
    fn open_lazy_saved(&self, path: &str, input: shell_contract::ipc::NativeSavedInput, permit: MediaPermit, cancel: CancelToken) -> Result<Option<OpenedLazyDocx>, String>;
    /// `adoptLazyMediaHashes` catches malformed ZIPs (lazy-media.ts:107-117);
    /// host transport failures are still returned.
    fn adopt_lazy_media_hashes(&self, bytes: &[u8], path: &str, file: Option<std::sync::Arc<std::fs::File>>, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<(), String>;
    fn adopt_lazy_saved(
        &self, bytes: &[u8], path: &str, input: shell_contract::ipc::NativeSavedInput,
        permit: MediaPermit, cancel: CancelToken,
    ) -> Result<(), String>;
    fn adopt_lazy_decrypted(
        &self, bytes: &[u8], path: &str, plain: Arc<Vec<u8>>,
        permit: MediaPermit, cancel: CancelToken,
    ) -> Result<(), String>;
    /// `materializeLazyDocx(bytes)`: the lazily served pictures put back
    fn materialize_lazy_docx(&self, bytes: Vec<u8>, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<Materialized, String>;
    /// `pointLazyMediaAt(hashes, filePath, wcId, plain)`
    fn point_lazy_media_at(
        &self,
        hashes: &[String],
        path: &str,
        permit: MediaPermit,
        plain: Option<Vec<u8>>,
    ) -> Result<(), String>;
    /// `moveLazyMediaSource(oldPath, newPath)`
    fn move_lazy_media_source(&self, old_path: &str, new_path: &str) -> Result<(), String>;
    /// `forgetLazyMediaOwner(wcId)`
    fn forget_lazy_media_owner(&self, permit: MediaPermit) -> Result<(), String>;
    /// `readLazyMedia(url)`
    fn read_lazy_media(&self, url: &str, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<Option<LazyMediaBody>, String>;
    /// `handOffBytes(bytes)` of `byte-handoff.ts`: the one-shot URL the media
    /// protocol serves the bytes under
    fn hand_off_bytes(&self, bytes: Vec<u8>, permit: MediaPermit) -> Result<String, String>;
    /// `decryptDocx(bytes, password)` of `docx-encryption.ts` (officecrypto-tool)
    fn decrypt_docx(&self, bytes: &[u8], password: &str) -> Result<Vec<u8>, DecryptError>;
    /// `encryptDocx(bytes, password)` (officecrypto-tool, ECMA-376 Agile)
    fn encrypt_docx(&self, bytes: &[u8], password: &str) -> Result<Vec<u8>, String>;
    /// `convertLegacyDocToDocx(bytes).docx` of `packages/doc2docx`
    fn convert_legacy_doc(&self, bytes: Vec<u8>) -> Result<Vec<u8>, String>;
    /// Recovery conversion retains the committed seed and its actual receiver.
    fn convert_legacy_doc_recovery(&self, bytes: Vec<u8>, receipt: shell_contract::ipc::RecoverySeedReceipt, receiver: shell_contract::ipc::NativeRequestOwner, cancel: CancelToken) -> Result<Vec<u8>, String>;
    /// `convertHtmlToDocx({ url }, ElectronBrowserDriver.create(794x1123 @2),
    /// { naturalTableWidth: true }).docx` of `packages/html2docx` for the
    /// generated HTML bytes; `Ok(None)` when `timeout` passed first (the
    /// hidden window is destroyed then, as the TypeScript's watchdog does).
    fn html_to_docx(&self, html: Vec<u8>, timeout: Duration, view: ViewId, cancel: CancelToken) -> Result<Option<Vec<u8>>, String>;
    /// `readMergeDataSource(path)` of `merge-data-source.ts` (xlsx-gateway
    /// readers, `mergeDataFromRows`)
    fn read_merge_data_source(&self, path: &str) -> Result<serde_json::Value, String>;
    /// `withInternalLinks(data, links)` past its empty check: pdf-lib's
    /// `PDFDocument.load`, `addPdfInternalLinks` of `pdf-links.ts`, `save`
    /// (the bytes unchanged when no link was added)
    fn add_pdf_internal_links(
        &self,
        pdf: Vec<u8>,
        links: serde_json::Value,
    ) -> Result<Vec<u8>, String>;
    /// `docs:save-merged-pdf`'s pdf-lib merge: every part's pages in order,
    /// then `addPdfInternalLinks` when `links` is a non-empty array
    fn merge_pdfs(
        &self,
        parts: Vec<Vec<u8>>,
        links: Option<serde_json::Value>,
    ) -> Result<Vec<u8>, String>;
    /// `convertInWorker({ docx, format, wasmPath: rhwpWasmPath() }, ...)` of
    /// `hwp-export.ts` (`@seuoffice/docx2hwp` in a single-use worker)
    fn convert_to_hwp(&self, docx: Vec<u8>, format: HwpFormat) -> Result<HwpConversion, HwpError>;
    /// `analyzeMediaTool(settingsPath, { mediaUrls, requirements }, { mediaRoots,
    /// context: screenToolContext(), signal })` of `@seuoffice/ai-search`;
    /// `cancel` is the signal of `withMediaCallSignal`
    fn analyze_media(
        &self,
        request: AnalyzeMediaRequest,
        cancel: CancelToken,
    ) -> Result<serde_json::Value, String>;
}

pub type DocsEngineHost = Arc<dyn DocsEngine>;

pub type OwnedMenuRefresh = Box<dyn FnOnce(Box<dyn FnOnce(Result<(), String>)>) + Send>;
pub type GeneratedActorWake = Arc<dyn Fn(Box<dyn FnOnce(Result<(), String>) + Send>) + Send + Sync>;

/// What the shell does for the Word editor that the shared hosts do not:
/// the hooks `setDocsShellHooks` and friends injected, the app menu, and the
/// Electron calls of `docs-main.ts` no shared host has.
pub trait DocsShell: Send + Sync {
    fn register_generated_actor_cleanup(
        &self, wake: GeneratedActorWake,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String>;
    fn capture_dirty_query_target(&self, view: ViewId, expected: &shell_contract::runtime::RendererGeneration) -> Result<shell_contract::ipc::NativeRequestOwner, String>;
    /// `getUiLang()`
    fn ui_lang(&self) -> Lang;
    /// `isHeadlessMode()`
    fn is_headless(&self) -> bool;
    fn unlock_recovery_prepared(
        &self, target: shell_contract::ipc::RecoveryOpenTarget, snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot, title: String, bytes: &[u8],
    ) -> Result<Option<(Vec<u8>, String)>, String>;
    fn unlock_saved_document(
        &self, document: shell_contract::ipc::NativeSavedDocument, title: String,
    ) -> Result<Option<(Vec<u8>, String)>, String>;
    /// `crypto.randomUUID()` (recovery slots, temp names, write-copies-now tokens)
    fn random_uuid(&self) -> String;
    /// `Date.now()` (the recovery copies' metadata)
    fn now_ms(&self) -> f64;
    /// `fileSavedHook(wc, path)`: the path the shell filed the new file under
    fn file_saved(&self, view: ViewId, path: &str) -> Option<String>;
    fn file_saved_owned(
        &self, origin: shell_contract::ipc::NativeRequestOwner, destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, effects: shell_contract::hosts::OwnedEffectsHost, path: std::path::PathBuf,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
    );
    fn file_saved_existing_document(
        &self, destination: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
        additional: Vec<CancelToken>, effects: shell_contract::hosts::OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
    );
    fn prepare_docs_menu_refresh_existing_document(
        &self, binding: shell_contract::ipc::NativeExistingDocumentBinding,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
        additional: Vec<CancelToken>, effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<OwnedMenuRefresh>, String>;
    fn prepare_docs_menu_refresh_owned(
        &self, origin: shell_contract::ipc::NativeRequestOwner, destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: CancelToken, pending: CancelToken, effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<OwnedMenuRefresh>, String>;
    fn prepare_docs_menu_refresh_recovery(
        &self, receipt: shell_contract::ipc::RecoverySeedReceipt, receiver: shell_contract::ipc::NativeRequestOwner,
        cancel: CancelToken, pending: CancelToken, effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Option<OwnedMenuRefresh>, String>;
    /// `fileOpenedHook(wcId, path)`
    fn file_opened(&self, view: ViewId, path: &str);
    fn file_opened_recovery(
        &self, path: String, receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: shell_contract::ipc::NativeRequestOwner, cancel: CancelToken, pending: CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost, done: Box<dyn FnOnce(Result<(), String>)>,
    );
    /// `shellHooks.newDocumentDir(wcId)`: a project folder for a new document
    fn new_document_dir(&self, view: ViewId) -> Option<String>;
    /// `shellHooks?.writeNewDocument` is set (a shell that files new
    /// documents into projects)
    fn writes_new_documents(&self) -> bool;
    /// `shellHooks.writeNewDocument`: `None` when there is no such hook
    /// (`write` undefined in the TypeScript); `Some(Ok(None))` when the panel
    /// has no project. Runs on a worker.
    fn write_new_document(
        &self,
        view: ViewId,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Option<Result<Option<String>, String>>;
    fn write_new_document_owned(
        &self, origin: &shell_contract::ipc::NativeRequestOwner, destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken, pending: &CancelToken, effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        file_name: &str, bytes: &[u8],
    ) -> Result<Option<String>, String>;
    fn permits_synthetic_create(&self) -> bool;
    fn open_ai_doc_tab_synthetic(&self, content: serde_json::Value, owner: ViewId) -> Result<(), String>;
    fn open_ai_doc_tab_owned(
        &self, content: serde_json::Value, origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner, cancel: CancelToken, pending: CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost, then: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    /// `shellHooks.openGeneratedPath(path)`: `false` when no tab took it
    fn open_generated_path(&self, path: &str) -> Result<bool, String>;
    fn prepare_generated_output(
        &self, output: shell_contract::ipc::SavedPublication, input: Option<shell_contract::ipc::NativeSavedInput>,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
        additional: Vec<CancelToken>, effects: shell_contract::hosts::OwnedEffectsHost,
        accepted: shell_contract::hosts::PdfReloadAccepted, wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String>;
    /// `activeDocsWebContents()`
    fn active_docs_view(&self) -> Option<ViewId>;
    /// `Menu.getApplicationMenu()?.getMenuItemById(id).checked = checked`
    fn set_menu_item_checked(&self, id: &str, checked: bool);
    /// `...getMenuItemById(id).enabled = enabled`
    fn set_menu_item_enabled(&self, id: &str, enabled: bool);
    /// `buildDocsMenu()` (File > Open Recent follows recent.json)
    fn rebuild_docs_menu(&self);
    /// `webContents.capturePage(rect)` then `isEmpty() ? null : toPNG()`
    fn capture_rect(
        &self,
        view: ViewId,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Result<Option<Vec<u8>>, String>;
    /// `nativeImage.createFromBuffer(bytes)`: its `getSize()`, `None` when `isEmpty()`
    fn image_size(&self, bytes: &[u8]) -> Option<(f64, f64)>;
    /// `clipboard.write({ image: nativeImage.createFromBuffer(bytes), html })`
    fn write_clipboard_image(&self, bytes: &[u8], html: &str);
    /// `printHtmlToPdf(html, () => new BrowserWindow({ show: false, sandbox, javascript: false }))`
    fn print_html_to_pdf(&self, html: String) -> Result<Vec<u8>, String>;
    fn print_html_to_pdf_owned(
        &self, html: String, origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner, cancel: &CancelToken, pending: &CancelToken,
    ) -> Result<Vec<u8>, String>;
    /// `net.fetch(url)` (it must reach the app's media schemes too); `Err`
    /// is `String(err)` of the rejection. Runs on a worker.
    fn net_fetch(&self, url: &str, permit: &MediaPermit, cancel: &CancelToken) -> Result<Box<dyn Response + Send>, String>;
}

pub type DocsShellHost = Arc<dyn DocsShell>;

#[cfg(any(test, feature = "probe"))]
mod fake_generated;

#[cfg(test)]
pub(crate) use fake::Fake;
#[cfg(any(test, feature = "probe"))]
pub(crate) use fake::fakes;

#[cfg(any(test, feature = "probe"))]
mod fake {
    use std::sync::Mutex;

    use main_core::packages::electron_utils::safe_remote_url::FetchError;
    use serde_json::{Value, json};

    use super::*;

    /// The case's host log (tools/ipc-diff/README.md, "Batch hosts")
    type ProbeLog = Arc<Mutex<Vec<Value>>>;

    /// The fake of both hosts, mirrored by tools/ipc-diff/groups/d2/docs-fakes.ts.
    /// `case` is `hosts.d2.docs`: `case[<method name>]` is the answer, an
    /// array is consumed one element per call; absent, a neutral answer.
    /// Bytes are written as a string (its UTF-8 bytes). Shapes per method:
    /// - `open_lazy_docx`: `{bytes, hash}` or null (default: null)
    /// - `materialize_lazy_docx`: `{bytes, hashes}` or `{error}` (default:
    ///   the input, no hashes)
    /// - `read_lazy_media`: `{body, mime}` or null (default: null)
    /// - `hand_off_bytes`: a string (default: `handoff:<the bytes in hex>`)
    /// - `decrypt_docx`: a string (the plain bytes) or `{error}`; an error
    ///   naming "password is incorrect" is a wrong password (default:
    ///   `no engine`)
    /// - `encrypt_docx`: a string (default: `ENC:` + the input)
    /// - `convert_legacy_doc`, `html_to_docx`, `print_html_to_pdf`,
    ///   `add_pdf_internal_links`, `merge_pdfs`: a string or `{error}`
    ///   (defaults: the input; `docx`; `%PDF`; the parts joined)
    /// - `read_merge_data_source`, `analyze_media`: a value or `{error}`
    ///   (defaults: `{path}`, `{ok:true}`)
    /// - `convert_to_hwp`: `{bytes, losses:[{feature,kind,count}]}` or
    ///   `{code, error}` (default: `hwp:` + the input, no losses)
    /// - `now` (the clock, 1700000000000), `ui_lang` (`"en"`), `is_headless`
    ///   (false), `file_saved` (null), `new_document_dir` (null),
    ///   `write_new_document` (null; `"none"` for no hook),
    ///   `open_generated_path` (false), `active_docs_view` (null),
    ///   `capture_rect` (null), `image_size` (`[w, h]`, default `[1, 1]` for
    ///   non-empty bytes), `net_fetch` (`{status, body, type}` or `{error}`,
    ///   default `TypeError: fetch failed`)
    ///
    /// Each call is one JSON line named after the TypeScript function it
    /// stands for, in `calls` and in the case's log when there is one.
    #[derive(Clone)]
    pub(crate) struct Fake {
        case: Arc<Mutex<Value>>,
        pub calls: Arc<Mutex<Vec<Value>>>,
        probe: Option<ProbeLog>,
        uuids: Arc<Mutex<u64>>,
        query_runtime: Arc<Mutex<Option<(Arc<dyn shell_contract::runtime::Views>, Arc<dyn shell_contract::ipc::EventSink>)>>>,
    }

    impl Fake {
        pub fn new(case: &Value) -> Self {
            Fake {
                case: Arc::new(Mutex::new(case.clone())),
                calls: Arc::default(),
                probe: None,
                uuids: Arc::default(),
                query_runtime: Arc::default(),
            }
        }

        pub(crate) fn bind_query_runtime(&self, services: &shell_contract::runtime::Services) {
            *self.query_runtime.lock().unwrap() = Some((services.views.clone(), services.events.clone()));
        }

        fn log(&self, line: Value) {
            if let Some(probe) = &self.probe {
                probe.lock().unwrap().push(line.clone());
            }
            self.calls.lock().unwrap().push(line)
        }

        /// the answer for `method`, if the case has one
        fn answer(&self, method: &str) -> Option<Value> {
            let mut case = self.case.lock().unwrap();
            let slot = case.get_mut(method)?;
            match slot {
                Value::Array(items) if method != "image_size" => {
                    (!items.is_empty()).then(|| items.remove(0))
                }
                other => Some(other.clone()),
            }
        }
    }

    fn bytes(v: &Value) -> Vec<u8> {
        match v {
            Value::String(s) => s.as_bytes().to_vec(),
            Value::Array(items) => items
                .iter()
                .filter_map(|n| n.as_u64())
                .map(|n| n as u8)
                .collect(),
            _ => Vec::new(),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// a number as JavaScript prints it (an integer has no fraction)
    fn num(n: f64) -> Value {
        if n.fract() == 0.0 && n.abs() < 9e15 {
            json!(n as i64)
        } else {
            json!(n)
        }
    }

    fn error_of(v: &Value) -> Option<String> {
        v.get("error").and_then(Value::as_str).map(str::to_owned)
    }

    fn bytes_or_error(v: Option<Value>, default: Vec<u8>) -> Result<Vec<u8>, String> {
        match v {
            None => Ok(default),
            Some(v) => match error_of(&v) {
                Some(e) => Err(e),
                None => Ok(bytes(&v)),
            },
        }
    }

    impl DocsEngine for Fake {
        fn open_lazy_docx(&self, path: &str, _file: Option<std::sync::Arc<std::fs::File>>, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<Option<OpenedLazyDocx>, String> {
            if cancel.is_cancelled() || !permit.is_active() { return Err("Document operation cancelled".into()); }
            self.log(json!(["openLazyDocx", path, permit.owner().view]));
            let Some(v) = self.answer("open_lazy_docx") else { return Ok(None) };
            if let Some(error) = error_of(&v) { return Err(error); }
            Ok(v.get("bytes").zip(v.get("hash").and_then(Value::as_str)).map(|(data, hash)| OpenedLazyDocx {
                bytes: bytes(data),
                hash: hash.to_owned(),
            }))
        }
        fn open_lazy_saved(&self, path: &str, input: shell_contract::ipc::NativeSavedInput, permit: MediaPermit, cancel: CancelToken) -> Result<Option<OpenedLazyDocx>, String> {
            if cancel.is_cancelled() || !permit.is_active() { return Err("Document operation cancelled".into()); }
            self.log(json!(["openLazySaved", path, permit.owner().view, input.bytes().len()]));
            let Some(v) = self.answer("open_lazy_docx") else { return Ok(None) };
            if let Some(error) = error_of(&v) { return Err(error); }
            Ok(v.get("bytes").zip(v.get("hash").and_then(Value::as_str)).map(|(data, hash)| OpenedLazyDocx {
                bytes: bytes(data), hash: hash.to_owned(),
            }))
        }
        fn adopt_lazy_media_hashes(&self, _bytes: &[u8], path: &str, _file: Option<std::sync::Arc<std::fs::File>>, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<(), String> {
            if cancel.is_cancelled() || !permit.is_active() { return Err("Document operation cancelled".into()); }
            self.log(json!(["adoptLazyMediaHashes", path, permit.owner().view]));
            match self.answer("adopt_lazy_media_hashes").as_ref().and_then(error_of) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        fn adopt_lazy_saved(
            &self, bytes: &[u8], path: &str, input: shell_contract::ipc::NativeSavedInput,
            permit: MediaPermit, cancel: CancelToken,
        ) -> Result<(), String> {
            if cancel.is_cancelled() || !permit.is_active() { return Err("Document operation cancelled".into()); }
            self.log(json!(["adoptLazySaved", path, permit.owner().view, bytes.len(), input.bytes().len()]));
            match self.answer("adopt_lazy_saved").as_ref().and_then(error_of) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        fn adopt_lazy_decrypted(
            &self, bytes: &[u8], path: &str, plain: Arc<Vec<u8>>,
            permit: MediaPermit, cancel: CancelToken,
        ) -> Result<(), String> {
            if cancel.is_cancelled() || !permit.is_active() { return Err("Document operation cancelled".into()); }
            self.log(json!(["adoptLazyDecrypted", path, permit.owner().view, bytes.len(), plain.len()]));
            match self.answer("adopt_lazy_decrypted").as_ref().and_then(error_of) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        fn materialize_lazy_docx(&self, input: Vec<u8>, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<Materialized, String> {
            if cancel.is_cancelled() || !permit.is_active() { return Err("Document operation cancelled".into()); }
            self.log(json!(["materializeLazyDocx", hex(&input)]));
            match self.answer("materialize_lazy_docx") {
                None => Ok(Materialized {
                    bytes: input,
                    hashes: Vec::new(),
                }),
                Some(v) => match error_of(&v) {
                    Some(e) => Err(e),
                    None => Ok(Materialized {
                        bytes: v.get("bytes").map(bytes).unwrap_or(input),
                        hashes: v
                            .get("hashes")
                            .and_then(Value::as_array)
                            .map(|h| {
                                h.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    }),
                },
            }
        }
        fn point_lazy_media_at(
            &self,
            hashes: &[String],
            path: &str,
            permit: MediaPermit,
            plain: Option<Vec<u8>>,
        ) -> Result<(), String> {
            self.log(json!([
                "pointLazyMediaAt",
                hashes,
                path,
                permit.owner().view,
                plain.is_some()
            ]));
            match self.answer("point_lazy_media_at").as_ref().and_then(error_of) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        fn move_lazy_media_source(&self, old_path: &str, new_path: &str) -> Result<(), String> {
            self.log(json!(["moveLazyMediaSource", old_path, new_path]));
            match self.answer("move_lazy_media_source").as_ref().and_then(error_of) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        fn forget_lazy_media_owner(&self, permit: MediaPermit) -> Result<(), String> {
            self.log(json!(["forgetLazyMediaOwner", permit.owner().view]));
            match self.answer("forget_lazy_media_owner").as_ref().and_then(error_of) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        fn read_lazy_media(&self, url: &str, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<Option<LazyMediaBody>, String> {
            if cancel.is_cancelled() || !permit.is_active() { return Err("Document operation cancelled".into()); }
            self.log(json!(["readLazyMedia", url]));
            match self.answer("read_lazy_media") {
                None | Some(Value::Null) => Ok(None),
                Some(v) => match error_of(&v) {
                    Some(e) => Err(e),
                    None => Ok(Some(LazyMediaBody {
                        body: v.get("body").map(bytes).unwrap_or_default(),
                        mime: v
                            .get("mime")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    })),
                },
            }
        }
        fn hand_off_bytes(&self, input: Vec<u8>, _permit: MediaPermit) -> Result<String, String> {
            match self.answer("hand_off_bytes") {
                Some(Value::String(s)) => Ok(s),
                Some(v) if error_of(&v).is_some() => Err(error_of(&v).unwrap()),
                _ => Ok(format!("handoff:{}", hex(&input))),
            }
        }
        fn decrypt_docx(&self, _bytes: &[u8], password: &str) -> Result<Vec<u8>, DecryptError> {
            self.log(json!(["officeCrypto.decrypt", password]));
            match self.answer("decrypt_docx") {
                Some(Value::String(s)) => Ok(s.into_bytes()),
                v => {
                    let message = v.as_ref().and_then(error_of).unwrap_or("no engine".into());
                    Err(DecryptError {
                        reason: if message.contains("password is incorrect") {
                            "wrong-password"
                        } else {
                            "unsupported"
                        },
                        message,
                    })
                }
            }
        }
        fn encrypt_docx(&self, input: &[u8], password: &str) -> Result<Vec<u8>, String> {
            self.log(json!(["officeCrypto.encrypt", password]));
            let mut out = b"ENC:".to_vec();
            out.extend_from_slice(input);
            bytes_or_error(self.answer("encrypt_docx"), out)
        }
        fn convert_legacy_doc(&self, input: Vec<u8>) -> Result<Vec<u8>, String> {
            self.log(json!(["convertLegacyDocToDocx", hex(&input)]));
            bytes_or_error(self.answer("convert_legacy_doc"), input)
        }
        fn convert_legacy_doc_recovery(&self, input: Vec<u8>, _receipt: shell_contract::ipc::RecoverySeedReceipt, _receiver: shell_contract::ipc::NativeRequestOwner, cancel: CancelToken) -> Result<Vec<u8>, String> {
            if cancel.is_cancelled() { return Err("The recovery conversion was cancelled.".into()); }
            self.convert_legacy_doc(input)
        }
        fn html_to_docx(
            &self,
            _html: Vec<u8>,
            _timeout: Duration,
            _view: ViewId,
            _cancel: CancelToken,
        ) -> Result<Option<Vec<u8>>, String> {
            self.log(json!(["convertHtmlToDocx"]));
            match self.answer("html_to_docx") {
                Some(Value::Null) => Ok(None),
                v => bytes_or_error(v, b"docx".to_vec()).map(Some),
            }
        }
        fn read_merge_data_source(&self, path: &str) -> Result<Value, String> {
            self.log(json!(["readMergeDataSource", path]));
            match self.answer("read_merge_data_source") {
                None => Ok(json!({ "path": path })),
                Some(v) => error_of(&v).map_or(Ok(v), Err),
            }
        }
        fn add_pdf_internal_links(&self, pdf: Vec<u8>, links: Value) -> Result<Vec<u8>, String> {
            self.log(json!(["addPdfInternalLinks", links]));
            bytes_or_error(self.answer("add_pdf_internal_links"), pdf)
        }
        fn merge_pdfs(&self, parts: Vec<Vec<u8>>, links: Option<Value>) -> Result<Vec<u8>, String> {
            if let Some(links) = links {
                self.log(json!(["addPdfInternalLinks", links]));
                // the links land in the merged document, whose pages are the answer
                if let Some(e) = self
                    .answer("add_pdf_internal_links")
                    .as_ref()
                    .and_then(error_of)
                {
                    return Err(e);
                }
            }
            bytes_or_error(self.answer("merge_pdfs"), parts.concat())
        }
        fn convert_to_hwp(
            &self,
            docx: Vec<u8>,
            format: HwpFormat,
        ) -> Result<HwpConversion, HwpError> {
            self.log(json!(["convertInWorker", format.as_str()]));
            let mut out = b"hwp:".to_vec();
            out.extend_from_slice(&docx);
            match self.answer("convert_to_hwp") {
                None => Ok(HwpConversion {
                    bytes: out,
                    losses: Vec::new(),
                }),
                Some(v) => match v.get("code").and_then(Value::as_str) {
                    Some(code) => Err(HwpError::Failure {
                        code: code.to_owned(),
                        message: error_of(&v).unwrap_or_default(),
                    }),
                    None => Ok(HwpConversion {
                        bytes: v.get("bytes").map(bytes).unwrap_or(out),
                        losses: v
                            .get("losses")
                            .and_then(Value::as_array)
                            .map(|l| {
                                l.iter()
                                    .map(|l| main_core::docs::hwp_export::Loss {
                                        feature: l["feature"].as_str().unwrap_or("").to_owned(),
                                        kind: l["kind"].as_str().unwrap_or("").to_owned(),
                                        count: l["count"].as_f64().unwrap_or(1.0),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    }),
                },
            }
        }
        fn analyze_media(
            &self,
            request: AnalyzeMediaRequest,
            _cancel: CancelToken,
        ) -> Result<Value, String> {
            // documentMediaRoots(openDoc, pasted): the document's folder first
            let roots: Vec<String> = match &request.open_doc {
                Some(open) => vec![
                    jscompat::node_path::host::dirname(open),
                    request.pasted_dir.clone(),
                ],
                None => vec![request.pasted_dir.clone()],
            };
            self.log(json!([
                "analyzeMediaTool",
                request.settings_path,
                request.media_urls,
                request.requirements,
                roots
            ]));
            match self.answer("analyze_media") {
                None => Ok(json!({ "ok": true })),
                Some(v) => error_of(&v).map_or(Ok(v), Err),
            }
        }
    }

    /// One `net.fetch` answer: all of the body in one chunk
    struct FakeResponse {
        status: u16,
        mime: Option<String>,
        body: Option<Vec<u8>>,
    }

    impl Response for FakeResponse {
        fn status(&self) -> u16 {
            self.status
        }
        fn header(&self, name: &str) -> Option<String> {
            (name.eq_ignore_ascii_case("content-type"))
                .then(|| self.mime.clone())
                .flatten()
        }
        fn has_body(&self) -> bool {
            true
        }
        fn cancel_body(&mut self) {}
        fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, FetchError> {
            Ok(self.body.take())
        }
        fn array_buffer(&mut self) -> Result<Vec<u8>, FetchError> {
            Ok(self.body.take().unwrap_or_default())
        }
    }
    struct SyntheticActorCleanup(Arc<Mutex<Vec<Value>>>);
    impl shell_contract::hosts::GeneratedCleanupRegistration for SyntheticActorCleanup {
        fn complete(self: Box<Self>) { self.0.lock().unwrap().push(json!(["generatedActorCleanupComplete"])); }
    }


    impl DocsShell for Fake {
        fn register_generated_actor_cleanup(
            &self, _wake: GeneratedActorWake,
        ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String> {
            Ok(Box::new(SyntheticActorCleanup(self.calls.clone())))
        }
        fn capture_dirty_query_target(&self, view: ViewId, expected: &shell_contract::runtime::RendererGeneration) -> Result<shell_contract::ipc::NativeRequestOwner, String> {
            let runtime = self.query_runtime.lock().unwrap();
            let (views, _) = runtime.as_ref().ok_or("synthetic query runtime unavailable")?;
            if !views.renderer_generation(view).is_some_and(|current| current.is_active() && current.same_generation(expected)) { return Err("synthetic target replaced".into()); }
            Ok(shell_contract::ipc::NativeRequestOwner { view, context: shell_contract::ipc::NativeContext::with_owner_loss((), expected.clone(), expected.loss_token()) })
        }

        fn ui_lang(&self) -> Lang {
            match self.case.lock().unwrap()["ui_lang"].as_str() {
                Some("ko") => Lang::Ko,
                _ => Lang::En,
            }
        }
        fn is_headless(&self) -> bool {
            self.case.lock().unwrap()["is_headless"] == Value::Bool(true)
        }
        fn unlock_recovery_prepared(
            &self, _target: shell_contract::ipc::RecoveryOpenTarget, _snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot, _title: String, _bytes: &[u8],
        ) -> Result<Option<(Vec<u8>, String)>, String> {
            match self.answer("unlock_recovery_prepared") {
                Some(Value::Null) => Ok(None),
                Some(Value::Object(value)) if value.contains_key("error") => Err(error_of(&Value::Object(value)).unwrap_or_else(|| "synthetic recovery decryption failed".into())),
                Some(Value::Object(value)) => {
                    let bytes = value.get("plaintext").and_then(Value::as_str).ok_or("synthetic recovery plaintext is missing")?;
                    let password = value.get("password").and_then(Value::as_str).ok_or("synthetic recovery password is missing")?;
                    Ok(Some((bytes.as_bytes().to_vec(), password.to_owned())))
                }
                _ => Err("synthetic recovery password response is not configured".into()),
            }
        }
        fn unlock_saved_document(&self, _document: shell_contract::ipc::NativeSavedDocument, _title: String) -> Result<Option<(Vec<u8>, String)>, String> {
            match self.answer("unlock_saved_document") {
                Some(Value::Null) => Ok(None),
                Some(Value::Object(value)) if value.contains_key("error") => Err(error_of(&Value::Object(value)).unwrap_or_else(|| "synthetic saved decryption failed".into())),
                Some(Value::Object(value)) => {
                    let bytes = value.get("plaintext").and_then(Value::as_str).ok_or("synthetic saved plaintext is missing")?;
                    let password = value.get("password").and_then(Value::as_str).ok_or("synthetic saved password is missing")?;
                    Ok(Some((bytes.as_bytes().to_vec(), password.to_owned())))
                }
                _ => Err("synthetic saved password response is not configured".into()),
            }
        }
        /// `00000000-0000-4000-8000-<n>`: the n-th id of the case
        fn random_uuid(&self) -> String {
            let mut n = self.uuids.lock().unwrap();
            *n += 1;
            format!("00000000-0000-4000-8000-{:012x}", *n)
        }
        /// the case's `now` (default 1700000000000)
        fn now_ms(&self) -> f64 {
            self.case.lock().unwrap()["now"]
                .as_f64()
                .unwrap_or(1_700_000_000_000.0)
        }
        fn file_saved(&self, view: ViewId, path: &str) -> Option<String> {
            self.log(json!(["fileSavedHook", view.0, path]));
            self.answer("file_saved")
                .and_then(|v| v.as_str().map(str::to_owned))
        }
        fn file_saved_owned(
            &self, origin: shell_contract::ipc::NativeRequestOwner, destination: shell_contract::ipc::NativeRequestOwner,
            mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
            cancel: CancelToken, pending: CancelToken, effects: shell_contract::hosts::OwnedEffectsHost, path: std::path::PathBuf,
            done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
        ) {
            let mut saved = None;
            participants.push(destination.clone());
            let result = effects.with_owned_transition_many(&origin, &participants, &cancel, &pending, &mut |_| {
                saved = Some(self.file_saved(destination.view, &path.to_string_lossy()).map(std::path::PathBuf::from).unwrap_or_else(|| path.clone()));
                Ok(())
            });
            done(result.and_then(|()| saved.ok_or_else(|| "the saved-file hook was not admitted".into())).map(|path| {
                shell_contract::hosts::SavedFile {
                    path,
                    session: shell_contract::hosts::SavedSessionReceipt::NoPublication,
                }
            }));
        }
        fn file_saved_existing_document(
            &self, destination: shell_contract::ipc::NativeSaveDestination,
            participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
            additional: Vec<CancelToken>, effects: shell_contract::hosts::OwnedEffectsHost,
            done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
        ) {
            let mut saved = None;
            let extra = additional.iter().collect::<Vec<_>>();
            let path = destination.path().to_owned();
            let result = effects.with_existing_document_destination_effect(&destination, &path, &participants, &cancel, &pending, &extra, &mut || {
                saved = Some(self.file_saved(destination.binding().owner().view, &path.to_string_lossy())
                    .map(std::path::PathBuf::from).unwrap_or_else(|| path.clone()));
                Ok(())
            });
            done(result.and_then(|()| saved.ok_or_else(|| "the saved-file hook was not admitted".into()))
                .map(|path| shell_contract::hosts::SavedFile { path, session: shell_contract::hosts::SavedSessionReceipt::NoPublication }));
        }
        fn prepare_docs_menu_refresh_existing_document(
            &self, binding: shell_contract::ipc::NativeExistingDocumentBinding,
            participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
            additional: Vec<CancelToken>, effects: shell_contract::hosts::OwnedEffectsHost,
        ) -> Result<Option<OwnedMenuRefresh>, String> {
            let extra = additional.iter().collect::<Vec<_>>();
            effects.with_existing_document_save_effect_extra(&binding, &participants, &cancel, &pending, &extra, &mut || Ok(()))?;
            let fake = self.clone();
            Ok(Some(Box::new(move |done| {
                let extra = additional.iter().collect::<Vec<_>>();
                let result = effects.with_existing_document_save_effect_extra(&binding, &participants, &cancel, &pending, &extra, &mut || {
                    fake.rebuild_docs_menu(); Ok(())
                });
                done(result);
            })))
        }
        fn prepare_docs_menu_refresh_owned(
            &self, origin: shell_contract::ipc::NativeRequestOwner, destination: shell_contract::ipc::NativeRequestOwner,
            mut participants: Vec<shell_contract::ipc::NativeRequestOwner>,
            cancel: CancelToken, pending: CancelToken, effects: shell_contract::hosts::OwnedEffectsHost,
        ) -> Result<Option<OwnedMenuRefresh>, String> {
            participants.push(destination);
            effects.with_owned_transition_many(&origin, &participants, &cancel, &pending, &mut |_| Ok(()))?;
            let fake = self.clone();
            Ok(Some(Box::new(move |done| {
                let result = effects.with_owned_transition_many(&origin, &participants, &cancel, &pending, &mut |_| {
                    fake.rebuild_docs_menu();
                    Ok(())
                });
                done(result);
            })))
        }
        fn prepare_docs_menu_refresh_recovery(
            &self, receipt: shell_contract::ipc::RecoverySeedReceipt, receiver: shell_contract::ipc::NativeRequestOwner,
            cancel: CancelToken, pending: CancelToken, effects: shell_contract::hosts::OwnedEffectsHost,
        ) -> Result<Option<OwnedMenuRefresh>, String> {
            effects.with_recovery_open_effect_pending(&receipt, &receiver, &cancel, &pending, &mut || Ok(()))?;
            let fake = self.clone();
            Ok(Some(Box::new(move |done| {
                let result = effects.with_recovery_open_effect_pending(&receipt, &receiver, &cancel, &pending, &mut || {
                    fake.rebuild_docs_menu();
                    Ok(())
                });
                done(result);
            })))
        }
        fn file_opened(&self, view: ViewId, path: &str) {
            self.log(json!(["fileOpenedHook", view.0, path]))
        }
        fn file_opened_recovery(
            &self, path: String, receipt: shell_contract::ipc::RecoverySeedReceipt,
            receiver: shell_contract::ipc::NativeRequestOwner, cancel: CancelToken, pending: CancelToken,
            effects: shell_contract::hosts::OwnedEffectsHost, done: Box<dyn FnOnce(Result<(), String>)>,
        ) {
            done(effects.with_recovery_open_effect_pending(&receipt, &receiver, &cancel, &pending, &mut || {
                self.file_opened(receiver.view, &path);
                Ok(())
            }));
        }
        fn new_document_dir(&self, view: ViewId) -> Option<String> {
            self.log(json!(["newDocumentDir", view.0]));
            self.answer("new_document_dir")
                .and_then(|v| v.as_str().map(str::to_owned))
        }
        fn writes_new_documents(&self) -> bool {
            self.case.lock().unwrap()["write_new_document"] != "none"
        }
        fn write_new_document(
            &self,
            view: ViewId,
            file_name: &str,
            bytes: Vec<u8>,
        ) -> Option<Result<Option<String>, String>> {
            if !self.writes_new_documents() {
                return None;
            }
            self.log(json!(["writeNewDocument", view.0, file_name, hex(&bytes)]));
            match self.answer("write_new_document") {
                Some(Value::String(s)) => {
                    // the shell writes the file into the project folder
                    std::fs::write(&s, &bytes).ok()?;
                    Some(Ok(Some(s)))
                }
                Some(v) if error_of(&v).is_some() => Some(Err(error_of(&v).unwrap())),
                _ => Some(Ok(None)),
            }
        }
        fn write_new_document_owned(
            &self, _origin: &shell_contract::ipc::NativeRequestOwner, destination: &shell_contract::ipc::NativeRequestOwner,
            _cancel: &CancelToken, _pending: &CancelToken, _effects: &shell_contract::hosts::OwnedEffectsHost,
            admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
            file_name: &str, bytes: &[u8],
        ) -> Result<Option<String>, String> {
            if !self.writes_new_documents() { return Ok(None); }
            self.log(json!(["writeNewDocument", destination.view.0, file_name, hex(bytes)]));
            match self.answer("write_new_document") {
                Some(Value::String(path)) => {
                    main_core::packages::electron_utils::atomic_write::owned::write_file(&path, bytes, admission)
                        .map_err(|error| error.message)?;
                    Ok(Some(path))
                }
                Some(value) if error_of(&value).is_some() => Err(error_of(&value).unwrap()),
                _ => Ok(None),
            }
        }
        fn permits_synthetic_create(&self) -> bool { true }
        fn open_ai_doc_tab_synthetic(&self, content: Value, owner: ViewId) -> Result<(), String> {
            self.log(json!(["openAiDocTabFor", content, owner.0]));
            match self.answer("open_ai_doc_tab") {
                Some(Value::String(error)) => Err(error),
                _ => Ok(()),
            }
        }
        fn open_ai_doc_tab_owned(
            &self, mut content: Value, origin: shell_contract::ipc::NativeRequestOwner,
            destination: shell_contract::ipc::NativeRequestOwner, cancel: CancelToken, pending: CancelToken,
            effects: shell_contract::hosts::OwnedEffectsHost, then: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            let mut answer = None;
            let result = effects.with_owned_effect(&origin, &destination, &cancel, &pending, &mut || {
                answer = Some(self.open_ai_doc_tab_synthetic(content.take(), destination.view));
                Ok(())
            }).and_then(|()| answer.ok_or("The AI document was not admitted")?);
            then(result);
        }
        fn open_generated_path(&self, path: &str) -> Result<bool, String> {
            self.log(json!(["openGeneratedPath", path]));
            Ok(self.answer("open_generated_path") == Some(Value::Bool(true)))
        }
        fn prepare_generated_output(
            &self, output: shell_contract::ipc::SavedPublication, input: Option<shell_contract::ipc::NativeSavedInput>,
            participants: Vec<shell_contract::ipc::NativeRequestOwner>, cancel: CancelToken, pending: CancelToken,
            additional: Vec<CancelToken>, effects: shell_contract::hosts::OwnedEffectsHost,
            _accepted: shell_contract::hosts::PdfReloadAccepted, _wake: shell_contract::hosts::GeneratedPathWake,
        ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
            if input.as_ref().is_some_and(|input| !input.output().same_publication(&output)) {
                return Err("The synthetic saved input does not match the publication".into());
            }
            let fake = self.clone();
            Ok(Box::new(super::fake_generated::Prepared::new(move || {
                let path = output.path().to_str().ok_or("The synthetic generated path is not Unicode")?.to_owned();
                let extra: Vec<_> = additional.iter().collect();
                let mut opened = None;
                output.with_effect(effects.as_ref(), &participants, &cancel, &pending, &extra, &mut || {
                    opened = Some(fake.open_generated_path(&path)?);
                    Ok(())
                })?;
                match opened {
                    Some(false) => Ok(shell_contract::hosts::GeneratedPathOutcome::NotOpened),
                    Some(true) => {
                        let receipt = match &output {
                            shell_contract::ipc::SavedPublication::Native(native) =>
                                shell_contract::hosts::CommittedGeneratedPath::from_existing_document_save(
                                    native.selection().clone(), native.selection().binding().owner().clone(),
                                    participants, cancel, pending, additional, effects,
                                ),
                            shell_contract::ipc::SavedPublication::Renderer(renderer) => {
                                let mut captured = participants;
                                captured.extend_from_slice(renderer.participants());
                                let mut controls = additional;
                                controls.push(renderer.cancel().clone());
                                controls.push(renderer.pending().clone());
                                controls.extend_from_slice(renderer.additional());
                                shell_contract::hosts::CommittedGeneratedPath::from_loaded_target(
                                    renderer.origin().clone(), renderer.destination().clone(), renderer.destination().clone(),
                                    cancel, pending, effects,
                                ).and_then(|receipt| receipt.with_captured_participants(&captured))
                                    .and_then(|receipt| receipt.with_captured_cancellations(&controls))
                            }
                        }?;
                        Ok(shell_contract::hosts::GeneratedPathOutcome::Opened(Arc::new(receipt)))
                    }
                    None => Err("The synthetic generated output was not admitted".into()),
                }
            })))
        }
        fn active_docs_view(&self) -> Option<ViewId> {
            self.case.lock().unwrap()["active_docs_view"]
                .as_u64()
                .map(|n| ViewId(n as u32))
        }
        fn set_menu_item_checked(&self, id: &str, checked: bool) {
            self.log(json!(["MenuItem.checked", id, checked]))
        }
        fn set_menu_item_enabled(&self, id: &str, enabled: bool) {
            self.log(json!(["MenuItem.enabled", id, enabled]))
        }
        /// the menu is rebuilt without a trace (Electron's `Menu` is inert in the comparison)
        fn rebuild_docs_menu(&self) {}
        fn capture_rect(
            &self,
            view: ViewId,
            x: f64,
            y: f64,
            width: f64,
            height: f64,
        ) -> Result<Option<Vec<u8>>, String> {
            self.log(json!([
                "webContents.capturePage",
                view.0,
                { "x": num(x), "y": num(y), "width": num(width), "height": num(height) }
            ]));
            match self.answer("capture_rect") {
                None | Some(Value::Null) => Ok(None),
                v => bytes_or_error(v, Vec::new()).map(|b| (!b.is_empty()).then_some(b)),
            }
        }
        fn image_size(&self, input: &[u8]) -> Option<(f64, f64)> {
            match self.answer("image_size") {
                Some(Value::Array(wh)) if wh.len() == 2 => Some((wh[0].as_f64()?, wh[1].as_f64()?)),
                Some(Value::Null) => None,
                _ => (!input.is_empty()).then_some((1.0, 1.0)),
            }
        }
        fn write_clipboard_image(&self, _input: &[u8], html: &str) {
            self.log(json!(["clipboard.write", { "html": html, "text": null }]))
        }
        fn print_html_to_pdf(&self, _html: String) -> Result<Vec<u8>, String> {
            self.log(json!(["printHtmlToPdf"]));
            bytes_or_error(self.answer("print_html_to_pdf"), b"%PDF".to_vec())
        }
        fn print_html_to_pdf_owned(
            &self, html: String, origin: &shell_contract::ipc::NativeRequestOwner,
            destination: &shell_contract::ipc::NativeRequestOwner, cancel: &CancelToken, pending: &CancelToken,
        ) -> Result<Vec<u8>, String> {
            if cancel.is_cancelled() || pending.is_cancelled()
                || !origin.context.renderer().is_some_and(|generation| generation.is_active())
                || !destination.context.renderer().is_some_and(|generation| generation.is_active())
            { return Err("The PDF creation request is no longer active".into()); }
            self.print_html_to_pdf(html)
        }
        fn net_fetch(&self, url: &str, _permit: &MediaPermit, _cancel: &CancelToken) -> Result<Box<dyn Response + Send>, String> {
            self.log(json!(["net.fetch", url]));
            match self.answer("net_fetch") {
                Some(v) if error_of(&v).is_none() => Ok(Box::new(FakeResponse {
                    status: v.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
                    mime: v.get("type").and_then(Value::as_str).map(str::to_owned),
                    body: Some(v.get("body").map(bytes).unwrap_or_default()),
                })),
                Some(v) => Err(format!("Error: {}", error_of(&v).unwrap())),
                None => Err("TypeError: fetch failed".into()),
            }
        }
    }

    /// Inserts one [`Fake`] as both hosts, logging to the case's log
    pub(crate) fn fakes(hosts: &mut shell_contract::Hosts, case: &Value) {
        let mut fake = Fake::new(case);
        fake.probe = hosts.get::<ProbeLog>().cloned();
        hosts.insert::<DocsEngineHost>(Arc::new(fake.clone()));
        hosts.insert::<DocsShellHost>(Arc::new(fake));
    }
}

/// `basename(path)` as Node's posix/win32 path module gives it on this host
pub(crate) fn basename(path: &str) -> String {
    jscompat::node_path::host::basename(path, None)
}
