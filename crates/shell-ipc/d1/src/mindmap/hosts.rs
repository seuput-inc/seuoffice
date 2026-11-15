//! What the mind map handlers need that is not in Rust yet or that only the
//! shell knows. Each trait is held as `Arc<dyn Trait>` in `Hosts` and taken
//! with `need` when the routes are built. Each method mirrors one TypeScript
//! function (its module and name are in the doc comment), arguments in the
//! TypeScript order.

use std::sync::Arc;

use main_core::packages::electron_utils::generate_handoff::GenerateHandoff;
use main_core::packages::electron_utils::safe_remote_url::{Fetcher, Resolver};
use shell_contract::ViewId;

/// How a file opened, as `@seuoffice/xmind-engine` reports it
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenStatus {
    Ok,
    LegacyFormat,
    Encrypted,
    Unreadable,
    Rejected,
}

impl OpenStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OpenStatus::Ok => "ok",
            OpenStatus::LegacyFormat => "legacy-format",
            OpenStatus::Encrypted => "encrypted",
            OpenStatus::Unreadable => "unreadable",
            OpenStatus::Rejected => "rejected",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "ok" => OpenStatus::Ok,
            "legacy-format" => OpenStatus::LegacyFormat,
            "encrypted" => OpenStatus::Encrypted,
            "unreadable" => OpenStatus::Unreadable,
            "rejected" => OpenStatus::Rejected,
            _ => return None,
        })
    }
}

/// What `openXmind(bytes)` answers
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenOutcome {
    pub status: OpenStatus,
    pub detail: Option<String>,
}

/// The xmind engine (not in Rust)
pub trait XmindEngine: Send + Sync {
    /// `openXmind(bytes)` of `@seuoffice/xmind-engine`
    fn open_xmind(&self, bytes: &[u8]) -> Result<OpenOutcome, String>;
}

pub type XmindEngineHost = Arc<dyn XmindEngine>;

/// What `parseFileToText(path)` answers, the fields the handlers read
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedText {
    pub ok: bool,
    pub kind: Option<String>,
    pub text: Option<String>,
    pub damaged: bool,
    pub error: Option<String>,
}

/// `@seuoffice/file-parse` (not in Rust): the attachment limits and
/// `parseFileToText`
pub trait FileParse: Send + Sync {
    /// `ATTACHABLE_EXTS`, in the order the set lists them
    fn attachable_exts(&self) -> Vec<String>;
    /// `ATTACHABLE_IMAGE_EXTS`
    fn attachable_image_exts(&self) -> Vec<String>;
    /// `ATTACHABLE_MAX_BYTES`
    fn attachable_max_bytes_default(&self) -> u64;
    /// `attachableMaxBytes(ext)`
    fn attachable_max_bytes(&self, ext: &str) -> u64;
    /// `ATTACHABLE_IMAGE_MAX_BYTES`
    fn attachable_image_max_bytes(&self) -> u64;
    /// `diagramPictureExtOf(path)`
    fn diagram_picture_ext_of(&self, path: &str) -> Option<String>;
    /// `diagramPictureExtOfName(path)`
    fn diagram_picture_ext_of_name(&self, path: &str) -> Option<String>;
    /// `parseFileToText(path)`
    fn parse_file_to_text(&self, path: &str) -> ParsedText;
}

pub type FileParseHost = Arc<dyn FileParse>;

/// The lowest level of the network under `main-core`'s SSRF guard
/// (`fetch_with_ssrf_guard`, `fetch_image_bytes`): the resolver is
/// `dns.lookup(host, { all: true })` and the fetcher is undici's `fetch` with
/// its global dispatcher and per-hop `Agent`. The guard's policy, the
/// redirects, the size caps and the retries run in Rust over these. Both are
/// made and used on the worker that runs one request, so they need not be
/// `Send`.
pub trait LinkNet: Send + Sync {
    /// `dns.lookup` of `node:dns/promises`
    fn resolver(&self) -> Box<dyn Resolver>;
    /// `undici.fetch`, `undici.getGlobalDispatcher` and `new undici.Agent`
    fn fetcher(&self) -> Box<dyn Fetcher>;
}

pub type LinkNetHost = Arc<dyn LinkNet>;

/// The hidden export window
pub trait PageRenderer: Send + Sync {
    /// `pageToPdf(html)` of `export-main.ts`: the page as a one-page
    /// landscape A4 PDF; blocks, run on a worker
    fn page_to_pdf(&self, html: &str) -> Result<Vec<u8>, String>;
    fn page_to_pdf_owned(
        &self,
        html: &str,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Vec<u8>, String>;
    fn page_to_pdf_native(
        &self,
        html: &str,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Vec<u8>, String>;
}

pub type PageRendererHost = Arc<dyn PageRenderer>;

/// The shell's side of a map view: the hooks the TypeScript main module lets
/// the shell set (`setMindmapFileSavedHook`, `setMindmapMakeFromDocumentHook`,
/// `setMindmapRecoveryTitleHook`, `setMindmapProvisionalTitleHook`,
/// `configureMindmapRuntime`) and the process-wide stores it keeps
/// (`globalThis` slots of `electron-utils`).
pub trait MindmapShell: Send + Sync {
    /// `fileSavedHook(wc, path)`: a save landed on a new path
    fn file_saved(&self, _view: ViewId, _path: &str) {}
    fn file_saved_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        path: std::path::PathBuf,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
    );
    fn file_saved_existing_document(
        &self,
        revision: shell_contract::ipc::NativeSaveRevisionAuthority,
        expected_path: std::path::PathBuf,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        on_revision: std::rc::Rc<
            dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>,
        >,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
    );
    /// `makeFromDocumentHook(wc)`
    fn make_from_document(&self, cx: shell_contract::ipc::CallContext);
    /// `recoveryTitleHook(wc)`: the tab title of an untitled map
    fn recovery_title(&self, _view: ViewId) -> Option<String> {
        None
    }
    /// `provisionalTitleHook(wc, title)`
    fn provisional_title(&self, _view: ViewId, _title: &str) {}
    fn reload_view(&self, view: ViewId);
    /// `runtime.newDocumentDir(wcId)`: the project folder of an untitled map
    fn new_document_dir(&self, _view: ViewId) -> Option<String> {
        None
    }
    /// whether the shell has a writer (the TypeScript's
    /// `runtime.writeNewDocument` is defined)
    fn has_new_document_writer(&self) -> bool {
        false
    }
    fn write_new_document_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<String>, String>;
    /// `isPathOpenElsewhere(path)`: the probe of `save-conflict`
    fn is_path_open_elsewhere(&self, path: &str) -> bool;
    /// `assertFileSaved(path)`: `Err(message)` for a file open with unsaved changes
    fn assert_file_saved(&self, path: String, then: Box<dyn FnOnce(Result<(), String>) + Send>);
    /// `takeGenerateHandoff('mindmap', webContentsId)`
    fn take_generate_handoff_owned(
        &self,
        consumer: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Option<GenerateHandoff>, String>;
    /// `reportGenerateRunStarted('mindmap', webContentsId, paths)`
    fn report_generate_run_started_owned(
        &self,
        consumer: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        paths: &[String],
    ) -> Result<bool, String>;
}

pub type MindmapShellHost = Arc<dyn MindmapShell>;

#[cfg(any(test, feature = "probe"))]
pub use fakes::*;

/// The fakes answer from the case's `hosts.d1.mindmap` value and log every
/// call as one JSON array, named after the TypeScript function it stands for
#[cfg(any(test, feature = "probe"))]
mod fakes {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use main_core::packages::electron_utils::generate_handoff::{GenerateHandoffTarget, Material};
    use serde_json::{Value, json};

    use super::*;
    use crate::mindmap::net;

    /// the case's log of host calls, when the harness provides one
    pub type ProbeLog = Arc<Mutex<Vec<Value>>>;

    /// One fake for every mind map host. The case value (`hosts.d1.mindmap`):
    ///
    /// - `openXmind`: `[{status, detail?}]` or `[{error}]`, one per call;
    ///   otherwise a map is bytes starting `XMIND`, `OLD` is legacy, `ENC`
    ///   encrypted, empty unreadable, anything else rejected
    /// - `attachable`: `{exts, imageExts, maxBytes, imageMaxBytes}`
    /// - `parseFileToText`: `{"<path>": {ok, kind, text, damaged, error}}`;
    ///   a path not listed is read as text
    /// - `diagramPictureExt`: `{"<path>": "<ext>"}`
    /// - the network under the SSRF guard (`net.rs`): `dns`, `network`, `proxy`
    /// - `openElsewhere`, `unsaved`: lists of paths; `handoff`: `{id, text,
    ///   materials: [{path, name}]}`; `projectDir`; `recoveryTitle`;
    ///   `writeNewDocument`: `{path}` or `{error}`; `pdf`: hex or `{error}`
    pub struct Fake {
        script: Value,
        net: Arc<net::Net>,
        log: Option<ProbeLog>,
        open_queue: Mutex<VecDeque<Result<OpenOutcome, String>>>,
        handoff: Mutex<Option<GenerateHandoff>>,
    }

    impl Fake {
        pub fn new(hosts: &shell_contract::Hosts, case: &Value) -> Fake {
            let script = case.get("mindmap").cloned().unwrap_or(Value::Null);
            let open_queue = script
                .get("openXmind")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            if let Some(error) = item.get("error").and_then(Value::as_str) {
                                return Err(error.to_owned());
                            }
                            Ok(OpenOutcome {
                                status: item
                                    .get("status")
                                    .and_then(Value::as_str)
                                    .and_then(OpenStatus::parse)
                                    .unwrap_or(OpenStatus::Ok),
                                detail: item
                                    .get("detail")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let handoff =
                script
                    .get("handoff")
                    .filter(|v| v.is_object())
                    .map(|h| GenerateHandoff {
                        id: text_of(h, "id"),
                        mode: "make".into(),
                        target: GenerateHandoffTarget::Mindmap,
                        materials: h
                            .get("materials")
                            .and_then(Value::as_array)
                            .map(|items| {
                                items
                                    .iter()
                                    .map(|m| Material {
                                        path: text_of(m, "path"),
                                        name: text_of(m, "name"),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        text: text_of(h, "text"),
                        default_text: false,
                        created_at: 0.0,
                    });
            Fake {
                net: Arc::new(net::Net::new(&script, hosts.get::<ProbeLog>().cloned())),
                script,
                log: hosts.get::<ProbeLog>().cloned(),
                open_queue: Mutex::new(open_queue),
                handoff: Mutex::new(handoff),
            }
        }

        fn note(&self, line: Value) {
            if let Some(log) = &self.log {
                log.lock().unwrap().push(line);
            }
        }

        fn table(&self, key: &str, name: &str) -> Option<&Value> {
            self.script.get(key)?.get(name)
        }

        fn list_has(&self, key: &str, path: &str) -> bool {
            self.script
                .get(key)
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(path)))
        }

        fn limit(&self, key: &str, default: u64) -> u64 {
            self.script
                .get("attachable")
                .and_then(|a| a.get(key))
                .and_then(Value::as_u64)
                .unwrap_or(default)
        }

        fn exts(&self, key: &str, default: &[&str]) -> Vec<String> {
            self.script
                .get("attachable")
                .and_then(|a| a.get(key))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|i| i.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_else(|| default.iter().map(|e| (*e).to_owned()).collect())
        }
    }

    fn text_of(value: &Value, key: &str) -> String {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    impl XmindEngine for Fake {
        fn open_xmind(&self, bytes: &[u8]) -> Result<OpenOutcome, String> {
            self.note(json!(["xmind-engine.openXmind", bytes.len()]));
            if let Some(scripted) = self.open_queue.lock().unwrap().pop_front() {
                return scripted;
            }
            let status = if bytes.starts_with(b"XMIND") {
                OpenStatus::Ok
            } else if bytes.starts_with(b"OLD") {
                OpenStatus::LegacyFormat
            } else if bytes.starts_with(b"ENC") {
                OpenStatus::Encrypted
            } else if bytes.is_empty() {
                OpenStatus::Unreadable
            } else {
                OpenStatus::Rejected
            };
            Ok(OpenOutcome {
                status,
                detail: Some("fake".into()),
            })
        }
    }

    impl FileParse for Fake {
        fn attachable_exts(&self) -> Vec<String> {
            self.exts(
                "exts",
                &["txt", "md", "xmind", "png", "jpg", "jpeg", "gif", "webp"],
            )
        }
        fn attachable_image_exts(&self) -> Vec<String> {
            self.exts("imageExts", &["png", "jpg", "jpeg", "gif", "webp"])
        }
        fn attachable_max_bytes_default(&self) -> u64 {
            self.limit("maxBytes", 20 * 1024 * 1024)
        }
        fn attachable_max_bytes(&self, _ext: &str) -> u64 {
            self.limit("maxBytes", 20 * 1024 * 1024)
        }
        fn attachable_image_max_bytes(&self) -> u64 {
            self.limit("imageMaxBytes", 5 * 1024 * 1024)
        }
        fn diagram_picture_ext_of(&self, path: &str) -> Option<String> {
            self.note(json!(["file-parse.diagramPictureExtOf", path]));
            self.table("diagramPictureExt", path)
                .and_then(Value::as_str)
                .map(str::to_owned)
        }
        fn diagram_picture_ext_of_name(&self, path: &str) -> Option<String> {
            self.note(json!(["file-parse.diagramPictureExtOfName", path]));
            self.table("diagramPictureExt", path)
                .and_then(Value::as_str)
                .map(str::to_owned)
        }
        fn parse_file_to_text(&self, path: &str) -> ParsedText {
            self.note(json!(["file-parse.parseFileToText", path]));
            if let Some(scripted) = self.table("parseFileToText", path) {
                let field =
                    |key: &str| scripted.get(key).and_then(Value::as_str).map(str::to_owned);
                return ParsedText {
                    ok: scripted.get("ok").and_then(Value::as_bool).unwrap_or(false),
                    kind: field("kind"),
                    text: field("text"),
                    damaged: scripted
                        .get("damaged")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    error: field("error"),
                };
            }
            match std::fs::read_to_string(path) {
                Ok(text) => ParsedText {
                    ok: true,
                    kind: Some("text".into()),
                    text: Some(text),
                    ..ParsedText::default()
                },
                Err(error) => ParsedText {
                    error: Some(error.to_string()),
                    ..ParsedText::default()
                },
            }
        }
    }

    impl LinkNet for Fake {
        fn resolver(&self) -> Box<dyn Resolver> {
            Box::new(net::FakeResolver(self.net.clone()))
        }

        fn fetcher(&self) -> Box<dyn Fetcher> {
            Box::new(net::FakeFetcher(self.net.clone()))
        }
    }

    impl PageRenderer for Fake {
        fn page_to_pdf(&self, html: &str) -> Result<Vec<u8>, String> {
            self.note(json!(["export-main.pageToPdf", html]));
            match self.script.get("pdf") {
                Some(pdf) if pdf.get("error").is_some() => Err(text_of(pdf, "error")),
                _ => Ok(b"%PDF-1.7".to_vec()),
            }
        }
        fn page_to_pdf_owned(
            &self,
            html: &str,
            _origin: &shell_contract::ipc::NativeRequestOwner,
            _destination: &shell_contract::ipc::NativeRequestOwner,
            _cancel: &shell_contract::runtime::CancelToken,
            _pending: &shell_contract::runtime::CancelToken,
            _participants: &[shell_contract::ipc::NativeRequestOwner],
            _additional: &[shell_contract::runtime::CancelToken],
        ) -> Result<Vec<u8>, String> {
            self.page_to_pdf(html)
        }
        fn page_to_pdf_native(
            &self,
            html: &str,
            _binding: &shell_contract::ipc::NativeExistingDocumentBinding,
            _participants: &[shell_contract::ipc::NativeRequestOwner],
            _cancel: &shell_contract::runtime::CancelToken,
            _pending: &shell_contract::runtime::CancelToken,
            _additional: &[shell_contract::runtime::CancelToken],
        ) -> Result<Vec<u8>, String> {
            self.page_to_pdf(html)
        }
    }

    impl MindmapShell for Fake {
        fn file_saved(&self, view: ViewId, path: &str) {
            self.note(json!(["mindmap-main.fileSavedHook", view.0, path]));
        }
        fn make_from_document(&self, cx: shell_contract::ipc::CallContext) {
            self.note(json!([
                "mindmap-main.makeFromDocumentHook",
                cx.sender.view.0
            ]));
        }
        fn recovery_title(&self, view: ViewId) -> Option<String> {
            self.note(json!(["mindmap-main.recoveryTitleHook", view.0]));
            self.script
                .get("recoveryTitle")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }
        fn provisional_title(&self, view: ViewId, title: &str) {
            self.note(json!([
                "ai-attachments.provisionalTitleHook",
                view.0,
                title
            ]));
        }
        fn reload_view(&self, view: ViewId) {
            self.note(json!(["mindmap.reload", view.0]));
        }
        fn new_document_dir(&self, view: ViewId) -> Option<String> {
            self.note(json!(["mindmap-main.newDocumentDir", view.0]));
            self.script
                .get("projectDir")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }
        fn has_new_document_writer(&self) -> bool {
            self.script.get("writeNewDocument").is_some()
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
        ) -> Result<Option<String>, String> {
            let log = self.log.clone();
            let call = json!([
                "mindmap-main.writeNewDocument",
                destination.view.0,
                file_name,
                bytes.len()
            ]);
            let scripted = self.script.get("writeNewDocument").cloned();
            main_core::packages::electron_utils::atomic_write::owned::admitted(
                &admission,
                move || {
                    if let Some(log) = log {
                        log.lock().unwrap().push(call);
                    }
                    match scripted {
                        Some(w) if w.get("error").is_some() => Err(text_of(&w, "error")),
                        Some(w) => Ok(w.get("path").and_then(Value::as_str).map(str::to_owned)),
                        None => Ok(None),
                    }
                },
            )
            .map_err(|error| error.message)?
        }
        fn file_saved_owned(
            &self,
            origin: &shell_contract::ipc::NativeRequestOwner,
            destination: &shell_contract::ipc::NativeRequestOwner,
            participants: &[shell_contract::ipc::NativeRequestOwner],
            cancel: &shell_contract::runtime::CancelToken,
            pending: &shell_contract::runtime::CancelToken,
            additional: &[shell_contract::runtime::CancelToken],
            effects: &shell_contract::hosts::OwnedEffectsHost,
            path: std::path::PathBuf,
            done: Box<
                dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>),
            >,
        ) {
            let result = effects
                .with_owned_effect_many_extra(
                    origin,
                    participants,
                    cancel,
                    pending,
                    &additional.iter().collect::<Vec<_>>(),
                    &mut || {
                        self.file_saved(destination.view, &path.to_string_lossy());
                        Ok(())
                    },
                )
                .map(|()| shell_contract::hosts::SavedFile {
                    path,
                    session: shell_contract::hosts::SavedSessionReceipt::NoPublication,
                });
            done(result);
        }
        fn file_saved_existing_document(
            &self,
            revision: shell_contract::ipc::NativeSaveRevisionAuthority,
            expected_path: std::path::PathBuf,
            participants: Vec<shell_contract::ipc::NativeRequestOwner>,
            cancel: shell_contract::runtime::CancelToken,
            pending: shell_contract::runtime::CancelToken,
            additional: Vec<shell_contract::runtime::CancelToken>,
            effects: shell_contract::hosts::OwnedEffectsHost,
            _on_revision: std::rc::Rc<
                dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>,
            >,
            done: Box<
                dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>),
            >,
        ) {
            let result = (|| {
                let snapshot = revision.snapshot()?;
                let path = expected_path
                    .to_str()
                    .ok_or("The saved map path is not Unicode")?;
                effects.with_native_save_revision_snapshot_effect(
                    &snapshot,
                    &participants,
                    &cancel,
                    &pending,
                    &additional.iter().collect::<Vec<_>>(),
                    &mut || {
                        if snapshot.selection().path() != expected_path {
                            return Err("The native save destination changed".into());
                        }
                        self.file_saved(snapshot.selection().binding().owner().view, path);
                        Ok(())
                    },
                )?;
                Ok(shell_contract::hosts::SavedFile {
                    path: expected_path,
                    session: shell_contract::hosts::SavedSessionReceipt::NoPublication,
                })
            })();
            done(result);
        }
        fn is_path_open_elsewhere(&self, path: &str) -> bool {
            self.note(json!(["electron-utils.isPathOpenElsewhere", path]));
            self.list_has("openElsewhere", path)
        }
        fn assert_file_saved(
            &self,
            path: String,
            then: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            self.note(json!(["electron-utils.assertFileSaved", path]));
            let result = if self.list_has("unsaved", &path) {
                Err(
                    main_core::packages::electron_utils::unsaved_files::UNSAVED_CHANGES_MESSAGE
                        .to_owned(),
                )
            } else {
                Ok(())
            };
            then(result);
        }
        fn take_generate_handoff_owned(
            &self,
            consumer: &shell_contract::ipc::NativeRequestOwner,
            _cancel: &shell_contract::runtime::CancelToken,
        ) -> Result<Option<GenerateHandoff>, String> {
            let view = consumer.view;
            self.note(json!([
                "generate-handoff.takeGenerateHandoff",
                "mindmap",
                view.0
            ]));
            Ok(self.handoff.lock().unwrap().take())
        }
        fn report_generate_run_started_owned(
            &self,
            consumer: &shell_contract::ipc::NativeRequestOwner,
            _cancel: &shell_contract::runtime::CancelToken,
            paths: &[String],
        ) -> Result<bool, String> {
            let view = consumer.view;
            self.note(json!([
                "generate-handoff.reportGenerateRunStarted",
                "mindmap",
                view.0,
                paths
            ]));
            Ok(true)
        }
    }
}
