//! The fakes of the Markdown editor's hosts, for unit tests and ipc-probe.
//! The TypeScript side of tools/ipc-diff (groups/d2/markdown-fakes.ts)
//! answers the same case keys and writes the same log lines.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use main_core::markdown::image_host::{FetchFailure, FetchInit, FetchResponse, ResponseHeaders};
use main_core::packages::electron_utils::print_window::{JsError, RendererAnswer, RendererCall};
use main_core::packages::electron_utils::safe_remote_url::{Fetcher, Resolver, Response};
use serde_json::{Value, json};
use shell_contract::ViewId;

use super::{MarkdownEngine, MarkdownPrintWindow, MarkdownShell};
use crate::html::fake::{Answers, NoNetwork, ProbeLog, file_text, hex, odt_answer};
use crate::html::odt_export::OdtConversion;

/// A hidden print window that answers `print_pdf`'s bytes and logs the page
struct FakeWindow {
    fake: Arc<Answers>,
    page: Option<PathBuf>,
}

impl MarkdownPrintWindow for FakeWindow {
    fn call(&mut self, call: RendererCall, answer: Box<dyn FnOnce(RendererAnswer) + Send>) {
        match call {
            RendererCall::LoadFile { path } => {
                self.page = Some(path.into());
                answer(RendererAnswer::Loaded(Ok(())))
            }
            RendererCall::PrintToPdf { .. } => {
                let page = self.page.as_deref().map(file_text).unwrap_or_default();
                self.fake.log(json!(["printToPDF", page]));
                answer(RendererAnswer::Pdf(
                    self.fake
                        .bytes("print_pdf", "", b"%PDF")
                        .map_err(JsError::plain),
                ))
            }
            // the markdown PDF job loads and prints, nothing else
            other => answer(RendererAnswer::Loaded(Err(JsError::plain(format!(
                "the print window does not carry out {other:?}"
            ))))),
        }
    }
    fn destroy(&mut self) -> Result<(), JsError> {
        Ok(())
    }
}

/// The Markdown editor's engine and shell hooks in one fake. Case keys, the
/// default in brackets; log lines in parentheses:
/// - engines: `print_pdf` [`"%PDF"`; `{ "error": text }` fails]
///   (`["printToPDF", text of the page]`), `docx_to_odt`
///   [`{ "odt": "ODT", "report": [] }`] (`["docxToOdt", hex]`),
///   `upload_fetch` [a failed request; `{ "status": n, "text": body }`
///   answers] (`["fetch", method]`), `net.fetch` always fails
///   (`["net.fetch", url]`); the network always fails.
/// - process state: `is_headless` [false], `ui_lang` [`"en"`], `now` [the
///   clock], `open_elsewhere` [paths open in another pane: none].
/// - hooks the shell may not set (absent: not called): `open_generated_path`
///   (`["openGeneratedPath", path]`), `new_document_dir`
///   (`["newDocumentDir", view]`), `write_new_document`
///   (`["writeNewDocument", view, name]`).
/// - always called: `["fileSavedHook", view, path]`,
///   `["docxExportedHook", path]`, `["webContents.reload", view]`.
/// - `randomUUID()` of the image exports counts: the n-th id starts with n in
///   six digits (`000001aa-0000-4000-8000-000000000000`), which `mkdtemp`'s
///   suffix takes.
pub(crate) struct FakeMarkdown {
    pub answers: Arc<Answers>,
    ids: AtomicU32,
    os: shell_contract::hosts::OsShellHost,
}

impl FakeMarkdown {
    pub(crate) fn new(
        case: &Value,
        probe: Option<ProbeLog>,
        os: shell_contract::hosts::OsShellHost,
    ) -> Self {
        FakeMarkdown {
            answers: Arc::new(Answers::new(case, probe)),
            ids: AtomicU32::new(0),
            os,
        }
    }
}

impl MarkdownEngine for FakeMarkdown {
    fn print_window(&self) -> Box<dyn MarkdownPrintWindow> {
        Box::new(FakeWindow {
            fake: Arc::clone(&self.answers),
            page: None,
        })
    }
    fn print_window_owned(
        &self,
        _origin: &shell_contract::ipc::NativeRequestOwner,
        _destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn MarkdownPrintWindow>, String> {
        if cancel.is_cancelled() || pending.is_cancelled() {
            return Err("The export was cancelled".into());
        }
        Ok(self.print_window())
    }
    fn print_window_native(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        _participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Box<dyn MarkdownPrintWindow>, String> {
        if cancel.is_cancelled()
            || pending.is_cancelled()
            || additional.iter().any(|token| token.is_cancelled())
            || binding.operation().cancel().is_cancelled()
            || binding.operation().shutdown().is_cancelled()
        {
            return Err("The export was cancelled".into());
        }
        Ok(self.print_window())
    }
    fn print_window_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        _owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn MarkdownPrintWindow>, String> {
        if cancel.is_cancelled()
            || pending.is_cancelled()
            || document.operation().cancel().is_cancelled()
            || document.operation().shutdown().is_cancelled()
        {
            return Err("The export was cancelled".into());
        }
        Ok(self.print_window())
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
    fn resolver(&self) -> Box<dyn Resolver + Send> {
        Box::new(NoNetwork)
    }
    fn fetcher(&self) -> Box<dyn Fetcher + Send> {
        Box::new(NoNetwork)
    }
    fn net_fetch(
        &self,
        _view: ViewId,
        url: &str,
        _cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn Response>, String> {
        self.answers.answer("net_fetch", url);
        self.answers.log(json!(["net.fetch", url]));
        Err("TypeError: Failed to fetch".into())
    }
    fn upload_fetch(&self, url: &str, init: &FetchInit<'_>) -> Result<FetchResponse, FetchFailure> {
        self.answers.log(json!(["fetch", init.method]));
        match self.answers.answer("upload_fetch", url) {
            Some(Value::Object(o)) => {
                let status = o.get("status").and_then(Value::as_f64).unwrap_or(200.0);
                Ok(FetchResponse {
                    ok: (200.0..300.0).contains(&status),
                    status,
                    opaque_redirect: false,
                    headers: ResponseHeaders::Get {
                        location: None,
                        content_length: None,
                    },
                    text: Some(Ok(o
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned())),
                })
            }
            _ => Err(FetchFailure::Error("fetch failed".into())),
        }
    }
}

impl MarkdownShell for FakeMarkdown {
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
    ) -> Result<crate::html::RecoveryAssetStage, String> {
        let answers = self.answers.clone();
        let view = destination.view;
        let open = open.map(str::to_owned);
        let save = save.map(str::to_owned);
        let mut once = Some(());
        Ok(crate::html::RecoveryAssetStage {
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
    ) -> Result<crate::html::SavedAssetStage, String> {
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
        Ok(crate::html::SavedAssetStage { stage, revoke })
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
    ) -> Result<crate::html::RecoveryAssetStage, String> {
        let answers = self.answers.clone();
        let view = destination.view;
        let open = open.map(str::to_owned);
        let save = save.map(str::to_owned);
        let mut once = Some(());
        Ok(crate::html::RecoveryAssetStage {
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
    fn docx_exported(&self, path: &Path) {
        self.answers.answer("docx_exported", path.display());
        self.answers
            .log(json!(["docxExportedHook", path.to_string_lossy()]));
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
                    match kind {
                        "pdf" => {
                            if !self.open_generated_path(&path) {
                                self.os.show_item_in_folder(&path);
                            }
                        }
                        "docx" | "odt" => self.docx_exported(&path),
                        _ => self.os.show_item_in_folder(&path),
                    }
                }
                Ok(())
            });
        then(result);
    }
    fn random_uuid(&self) -> String {
        let n = self.ids.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{n:06}aa-0000-4000-8000-000000000000")
    }
}
