//! Fakes of [`PdfEngine`] and [`PdfShell`] for the tests and ipc-probe.
//!
//! Both answer from `case` (the case's `hosts.d2.pdf`): `case[<method name in
//! camelCase>]` is the answer, an array of answers is consumed one per call
//! (the last one repeats), and `{ "error": "..." }` makes the call fail with
//! that message. Without an answer each method does the neutral thing:
//! byte operations hand back their input (a save writes the source bytes to
//! the target, so the folder shows the write), reads answer `[]` or `null`,
//! and the shell hooks are absent (`createDocument`, `openPdfTools`, notes)
//! or succeed (`redactionSaved`, `renamed`). Bytes in answers are strings,
//! taken as their UTF-8 bytes.
//!
//! In ipc-probe each call also writes one line to the case's host log, named
//! after the TypeScript function the method stands for; the group
//! `tools/ipc-diff/groups/d2/pdf-editor.ts` logs the same lines.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use main_core::pdf::font_locate::Platform;
use main_core::pdf::note_link::{FoundNoteLink, NoteLink, NoteLinks};
use main_core::pdf::ocr::OcrEnv;
use main_core::pdf::system_certificates::CertSummary;
use serde_json::{Value, json};
use shell_contract::pdf_sign_options::SignOptions;
use shell_contract::runtime::CancelToken;
use shell_contract::{IpcValue, ViewId};

use super::hosts::*;
use crate::kit::from_json;

/// The case's host log (ipc-probe inserts it; absent in unit tests)
type Log = Arc<Mutex<Vec<Value>>>;

#[derive(Default)]
struct Answers {
    case: Value,
    used: Mutex<HashMap<String, usize>>,
    log: Option<Log>,
}

impl Answers {
    fn line(&self, line: Value) {
        if let Some(log) = &self.log {
            log.lock().unwrap().push(line)
        }
    }

    fn take(&self, name: &str) -> Option<Value> {
        match self.case.get(name)? {
            Value::Array(items) if !items.is_empty() && name_takes_turns(name) => {
                let mut used = self.used.lock().unwrap();
                let n = used.entry(name.to_owned()).or_insert(0);
                let item = items[(*n).min(items.len() - 1)].clone();
                *n += 1;
                Some(item)
            }
            v => Some(v.clone()),
        }
    }

    /// The answer, or the error it asks for
    fn get(&self, name: &str) -> Result<Option<Value>, String> {
        match self.take(name) {
            Some(Value::Object(o)) if o.get("error").is_some_and(Value::is_string) => {
                Err(o["error"].as_str().unwrap_or_default().to_owned())
            }
            v => Ok(v),
        }
    }
}

/// Methods whose single answer is itself an array take turns only when the
/// case gives an array of arrays; the others take turns over any array
fn name_takes_turns(name: &str) -> bool {
    !matches!(
        name,
        "readStaticFormFills"
            | "readAnnotSubjects"
            | "listPageImages"
            | "validateTextEdits"
            | "readPdfSignatures"
            | "openCertificate"
    )
}

fn bytes(v: &Value) -> Vec<u8> {
    v.as_str().unwrap_or_default().as_bytes().to_vec()
}

fn num(v: Option<&Value>, default: f64) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(default)
}

/// `PdfEngine` answering from the case
pub struct FakeEngine(Arc<Answers>, bool);

struct FakeSigningSession(
    Arc<Answers>,
    CancelToken,
    shell_contract::runtime::RendererGeneration,
    Vec<Vec<u8>>,
);

impl SigningSession for FakeSigningSession {
    fn revoke_token(&self) -> CancelToken {
        self.1.clone()
    }

    fn is_current(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
    ) -> bool {
        owner
            .context
            .renderer()
            .is_some_and(|generation| generation.is_active() && self.2.same_generation(generation))
            && owner.pdf_document().is_none_or(|lease| lease.is_live())
            && owner
                .pdf_revision()
                .is_none_or(|revision| revision.is_live())
            && !cancel.is_cancelled()
            && !self.1.is_cancelled()
    }

    fn sign(
        &mut self,
        bytes: &[u8],
        options: &SignOptions,
        chain: &[Vec<u8>],
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, SignFailure> {
        if !self.is_current(owner, cancel) {
            return Err(SignFailure::Message("PDF signing cancelled".into()));
        }
        if chain != self.3 {
            return Err(SignFailure::Message(
                "PDF signing certificate chain changed".into(),
            ));
        }
        self.0
            .line(json!(["signPdf", options.reason, options.certify_level]));
        match self.0.take("signPdf") {
            Some(v) if v.get("code").is_some() => Err(SignFailure::Code(
                v["code"].as_str().unwrap_or_default().into(),
            )),
            Some(v) if v.get("error").is_some() => Err(SignFailure::Message(
                v["error"].as_str().unwrap_or_default().into(),
            )),
            _ => Ok([bytes, b"%signed"].concat()),
        }
    }
}

fn js(v: &IpcValue) -> Value {
    crate::kit::to_json(v).unwrap_or(Value::Null)
}

impl FakeEngine {
    fn ipc(&self, name: &str, default: IpcValue) -> Result<IpcValue, String> {
        Ok(self.0.get(name)?.as_ref().map_or(default, from_json))
    }
    fn bytes_or(&self, name: &str, input: &[u8]) -> Result<Vec<u8>, String> {
        Ok(self
            .0
            .get(name)?
            .as_ref()
            .map_or_else(|| input.to_vec(), bytes))
    }
    fn skips(&self, name: &str) -> Result<SaveSkips, String> {
        let v = self.0.get(name)?.unwrap_or(Value::Null);
        let list = |k: &str| match v.get(k) {
            Some(Value::Array(items)) => items.iter().map(from_json).collect(),
            _ => Vec::new(),
        };
        Ok(SaveSkips {
            text_edits: list("skippedTextEdits"),
            text_inserts: list("skippedTextInserts"),
            image_edits: list("skippedImageEdits"),
        })
    }
    fn line(&self, line: Value) {
        self.0.line(line)
    }
}

fn write(target: &str, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(target, bytes).map_err(|e| e.to_string())
}

fn read(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| e.to_string())
}

impl PdfEngine for FakeEngine {
    fn synthetic_save_capability(&self) -> bool {
        self.1
    }
    fn prepare_pdf_save(
        &self,
        source: &[u8],
        _request: &IpcValue,
        preserve_unchanged: bool,
        cancel: &CancelToken,
    ) -> Result<(Vec<u8>, SaveSkips), String> {
        if cancel.is_cancelled() {
            return Err("PDF save cancelled".into());
        }
        let name = if preserve_unchanged {
            "savePdfBytesToPath"
        } else {
            "savePdfToPath"
        };
        Ok((source.to_vec(), self.skips(name)?))
    }
    fn save_pdf_to_path(
        &self,
        source: &str,
        target: &str,
        _r: &IpcValue,
        _cancel: &CancelToken,
    ) -> Result<SaveSkips, String> {
        self.line(json!(["savePdfToPath", source, target]));
        let skips = self.skips("savePdfToPath")?;
        write(target, &read(source)?)?;
        Ok(skips)
    }
    fn save_pdf_bytes_to_path(
        &self,
        source: &[u8],
        target: &str,
        _r: &IpcValue,
        _cancel: &CancelToken,
    ) -> Result<SaveSkips, String> {
        self.line(json!(["savePdfBytesToPath", target]));
        let skips = self.skips("savePdfBytesToPath")?;
        write(target, source)?;
        Ok(skips)
    }
    fn copy_pdf_to_path(
        &self,
        source: &str,
        target: &str,
        _cancel: &CancelToken,
    ) -> Result<(), String> {
        self.line(json!(["copyPdfToPath", source, target]));
        self.0.get("copyPdfToPath")?;
        write(target, &read(source)?)
    }
    fn pdf_bytes_with_edits(
        &self,
        source: &str,
        _r: &IpcValue,
        _cancel: &CancelToken,
    ) -> Result<Vec<u8>, String> {
        self.line(json!(["pdfBytesWithEdits", source]));
        let input = read(source)?;
        self.bytes_or("pdfBytesWithEdits", &input)
    }
    fn pdf_bytes_with_edits_of(
        &self,
        file: &[u8],
        _r: &IpcValue,
        _cancel: &CancelToken,
    ) -> Result<Vec<u8>, String> {
        self.line(json!(["pdfBytesWithEditsOf"]));
        self.bytes_or("pdfBytesWithEditsOf", file)
    }
    fn pdf_bytes_with_edits_from(
        &self,
        source: &[u8],
        _r: &IpcValue,
        _cancel: &CancelToken,
    ) -> Result<Vec<u8>, String> {
        self.line(json!(["pdfBytesWithEditsFrom"]));
        self.bytes_or("pdfBytesWithEditsFrom", source)
    }
    fn read_static_form_fills(&self, _b: &[u8]) -> Result<IpcValue, String> {
        self.line(json!(["readStaticFormFills"]));
        self.ipc("readStaticFormFills", IpcValue::Array(Vec::new()))
    }
    fn extract_pages(&self, b: &[u8], pages: &IpcValue) -> Result<Vec<u8>, String> {
        self.line(json!(["extractPagesBytes", js(pages)]));
        self.bytes_or("extractPages", b)
    }
    fn insert_pdf(
        &self,
        target: &[u8],
        other: &[u8],
        after: f64,
    ) -> Result<(Vec<u8>, f64), String> {
        self.line(json!(["insertPdfBytes", after]));
        let count = num(self.0.get("insertPdf")?.as_ref(), 1.0);
        Ok(([target, other].concat(), count))
    }
    fn insert_blank_page(&self, b: &[u8], after: f64) -> Result<Vec<u8>, String> {
        self.line(json!(["insertBlankPageBytes", after]));
        self.bytes_or("insertBlankPage", b)
    }
    fn split_pdf(&self, b: &[u8], chunk: f64) -> Result<Vec<Vec<u8>>, String> {
        self.line(json!(["splitPdfBytes", chunk]));
        Ok(match self.0.get("splitPdf")? {
            Some(Value::Array(parts)) => parts.iter().map(bytes).collect(),
            _ => vec![b.to_vec()],
        })
    }
    fn merge_pages(
        &self,
        b: &[u8],
        per_sheet: f64,
        horizontal: bool,
        separator: bool,
    ) -> Result<Vec<u8>, String> {
        let direction = if horizontal { "horizontal" } else { "vertical" };
        self.line(json!(["mergePagesBytes", per_sheet, direction, separator]));
        self.bytes_or("mergePages", b)
    }
    fn merge_pdf(&self, base: &[u8], others: &[Vec<u8>]) -> Result<(Vec<u8>, f64), String> {
        self.line(json!(["mergePdfBytes", others.len()]));
        self.0.get("mergePdf")?;
        Ok((
            [base.to_vec(), others.concat()].concat(),
            others.len() as f64,
        ))
    }
    fn replace_pages(
        &self,
        target: &[u8],
        other: &[u8],
        pages: &IpcValue,
    ) -> Result<(Vec<u8>, IpcValue, IpcValue), String> {
        self.line(json!(["replacePagesBytes", js(pages)]));
        self.0.get("replacePages")?;
        Ok((
            [target, other].concat(),
            pages.clone(),
            IpcValue::Number(1.0),
        ))
    }
    fn set_page_size(&self, b: &[u8], width: f64, height: f64) -> Result<Vec<u8>, String> {
        self.line(json!(["setPageSizeBytes", width, height]));
        self.bytes_or("setPageSize", b)
    }
    fn split_pages(&self, b: &[u8], per_page: f64) -> Result<Vec<u8>, String> {
        self.line(json!(["splitPagesBytes", per_page]));
        self.bytes_or("splitPages", b)
    }
    fn crop_pages(&self, b: &[u8], pages: &IpcValue, rect: &IpcValue) -> Result<Vec<u8>, String> {
        self.line(json!(["cropPagesBytes", js(pages), js(rect)]));
        self.bytes_or("cropPages", b)
    }
    fn copied_pages_of(
        &self,
        source: &[u8],
        _r: &IpcValue,
        positions: &IpcValue,
        _cancel: &CancelToken,
    ) -> Result<Option<(Vec<u8>, f64)>, String> {
        self.line(json!(["copiedPagesOf", js(positions)]));
        match self.0.get("copiedPagesOf")? {
            Some(Value::Null) => Ok(None),
            Some(v) => Ok(Some((bytes(&v), 1.0))),
            None => {
                let n = match positions {
                    IpcValue::Array(p) => p.len() as f64,
                    _ => 0.0,
                };
                Ok((n > 0.0).then(|| (source.to_vec(), n)))
            }
        }
    }
    fn shrink(&self, source: &[u8], _cancel: &CancelToken) -> Result<Shrunk, String> {
        self.line(json!(["shrinkDocument"]));
        let v = self.0.get("shrink")?.unwrap_or(Value::Null);
        Ok(Shrunk {
            bytes: v.get("bytes").map_or_else(|| source.to_vec(), bytes),
            streams_recompressed: num(v.get("streamsRecompressed"), 0.0),
            images_downsampled: num(v.get("imagesDownsampled"), 0.0),
        })
    }
    fn decrypt_document(&self, b: &[u8], password: &str) -> Result<Decrypted, DecryptError> {
        self.line(json!(["decryptDocument", password]));
        match self.0.get("decryptDocument") {
            Err(e) if e == "wrong-password" => Err(DecryptError::WrongPassword),
            Err(e) if e == "certificate-encryption" => Err(DecryptError::CertificateEncryption),
            Err(e) => Err(DecryptError::Other(e)),
            Ok(_) => Ok(Decrypted {
                bytes: b.to_vec(),
                permissions: IpcValue::Object(Vec::new()),
                needed_password: !password.is_empty(),
                unlocked_as: "user".into(),
            }),
        }
    }
    fn read_annot_subjects(&self, _b: &[u8]) -> Result<IpcValue, String> {
        self.line(json!(["readAnnotSubjects"]));
        self.ipc("readAnnotSubjects", IpcValue::Array(Vec::new()))
    }
    fn list_page_images(&self, _b: &[u8], _cancel: &CancelToken) -> Result<IpcValue, String> {
        self.line(json!(["listPageImages"]));
        self.ipc("listPageImages", IpcValue::Array(Vec::new()))
    }
    fn render_image_png(
        &self,
        _b: &[u8],
        page_index: f64,
        rect: &IpcValue,
        scale: f64,
        _cancel: &CancelToken,
    ) -> Result<IpcValue, String> {
        self.line(json!(["renderImagePng", page_index, js(rect), scale]));
        self.ipc("renderImagePng", IpcValue::Null)
    }
    fn render_page_preview_png(
        &self,
        _b: &[u8],
        request: &IpcValue,
        _cancel: &CancelToken,
    ) -> Result<IpcValue, String> {
        self.line(json!(["renderPagePreviewPng", js(request)]));
        self.ipc("renderPagePreviewPng", IpcValue::Null)
    }
    fn validate_text_edits(
        &self,
        _b: &[u8],
        edits: &IpcValue,
        _cancel: &CancelToken,
    ) -> Result<IpcValue, String> {
        self.line(json!(["validateTextEdits", js(edits)]));
        self.ipc("validateTextEdits", IpcValue::Array(Vec::new()))
    }
    fn can_draw_text(
        &self,
        text: &str,
        font: Option<&str>,
        bold: bool,
        italic: bool,
    ) -> Result<bool, String> {
        self.line(json!(["canDrawText", text, font, bold, italic]));
        Ok(self
            .0
            .get("canDrawText")?
            .and_then(|v| v.as_bool())
            .unwrap_or(true))
    }
    fn read_pdf_text(&self, b: &[u8], password: Option<&str>) -> Result<PdfText, String> {
        self.line(json!(["readPdfText", password]));
        let v = self.0.get("readPdfText")?;
        let pages: Vec<String> = match v.as_ref().and_then(|v| v.get("pages")) {
            Some(Value::Array(p)) => p
                .iter()
                .map(|s| s.as_str().unwrap_or_default().to_owned())
                .collect(),
            _ => vec![String::from_utf8_lossy(b).into_owned()],
        };
        Ok(PdfText {
            page_count: pages.len() as f64,
            pages,
        })
    }
    fn read_pdf_signatures(&self, _b: &[u8]) -> Result<IpcValue, String> {
        self.line(json!(["readPdfSignatures"]));
        self.ipc("readPdfSignatures", IpcValue::Array(Vec::new()))
    }
    fn describe_certificate(&self, _input: &[u8]) -> Option<CertSummary> {
        None
    }
    fn open_signing_session(
        &self,
        source: &CertificateSource,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
        entry_cancel: &CancelToken,
    ) -> Result<(Box<dyn SigningSession>, Vec<IpcValue>, Vec<Vec<u8>>), CertificateOpenError> {
        if !self.sign_owner_current(owner, cancel) || entry_cancel.is_cancelled() {
            return Err(CertificateOpenError::Message(
                CertificateOpenMessage::Revoked,
            ));
        }
        let generation = owner
            .context
            .renderer()
            .cloned()
            .ok_or(CertificateOpenError::Message(
                CertificateOpenMessage::Revoked,
            ))?;
        match source {
            CertificateSource::P12 { password, .. } => {
                self.line(json!(["openCertificate", password]))
            }
            CertificateSource::System { .. } => self.line(json!(["openExternalCertificate"])),
        }
        let message = match self
            .0
            .case
            .get("openCertificateMessage")
            .and_then(Value::as_str)
        {
            Some("worker-unavailable") => Some(CertificateOpenMessage::WorkerUnavailable),
            Some("revoked") => Some(CertificateOpenMessage::Revoked),
            Some("regex-limit") => Some(CertificateOpenMessage::RegexLimit),
            _ => None,
        };
        if let Some(message) = message {
            return Err(CertificateOpenError::Message(message));
        }
        let summaries = match self.0.get("openCertificate").map_err(|error| {
            CertificateOpenError::Code(
                CertificateOpenCode::parse(&error).unwrap_or(CertificateOpenCode::Invalid),
            )
        })? {
            Some(Value::Array(info)) => info.iter().map(from_json).collect(),
            _ => vec![crate::kit::obj([(
                "commonName",
                IpcValue::String("Fake".into()),
            )])],
        };
        let chain = vec![vec![0x30, 0x00]];
        Ok((
            Box::new(FakeSigningSession(
                self.0.clone(),
                entry_cancel.clone(),
                generation,
                chain.clone(),
            )),
            summaries,
            chain,
        ))
    }
    fn sign_owner_current(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
    ) -> bool {
        owner
            .context
            .renderer()
            .is_some_and(|generation| generation.is_active())
            && owner.pdf_document().is_none_or(|lease| lease.is_live())
            && owner
                .pdf_revision()
                .is_none_or(|revision| revision.is_live())
            && !cancel.is_cancelled()
    }
    fn write_signed_pdf(
        &self,
        target: &str,
        bytes: &[u8],
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        if !self.sign_owner_current(owner, cancel) {
            return Err("PDF signing cancelled".into());
        }
        main_core::pdf::atomic_write::write_pdf_atomically(target, bytes)
            .map_err(|error| error.message)
    }
    fn image_subscription_statuses(&self) -> jscompat::JsonValue {
        jscompat::json_value_from_serde(
            &self
                .0
                .take("imageSubscriptionStatuses")
                .unwrap_or_else(|| json!({})),
        )
    }
    fn generate_image(
        &self,
        _s: &str,
        prompt: &str,
        aspect: Option<&str>,
        open: Option<&str>,
        _d: &str,
        statuses: jscompat::JsonValue,
        _c: &CancelToken,
    ) -> Result<IpcValue, String> {
        self.line(json!(["generateImageTool", prompt, aspect, open]));
        if self.0.case.get("generateImageEchoStatuses") == Some(&Value::Bool(true)) {
            return Ok(crate::kit::from_js_json(&statuses));
        }
        self.ipc("generateImage", IpcValue::Null)
    }
    fn read_note_links(&self, _u: &str) -> Result<NoteLinks, String> {
        self.line(json!(["ResultStore.readNoteLinks"]));
        self.0.get("readNoteLinks")?;
        Ok(NoteLinks {
            links: jscompat::JsMap::new(),
        })
    }
    fn find_note_link(
        &self,
        _u: &str,
        pdf_path: &str,
        hash: &str,
    ) -> Result<Option<FoundNoteLink>, String> {
        self.line(json!(["ResultStore.findNoteLink", pdf_path, hash]));
        self.0.get("findNoteLink")?;
        Ok(None)
    }
    fn set_note_link(&self, _u: &str, pdf_path: &str, link: &NoteLink) -> Result<(), String> {
        self.line(json!([
            "ResultStore.setNoteLink",
            pdf_path,
            link.note_path,
            link.doc_hash
        ]));
        self.0.get("setNoteLink").map(|_| ())
    }
    fn update_note_link_doc_hash_guarded(
        &self,
        user_data: &str,
        path: &str,
        hash: &str,
        admit: &mut main_core::packages::pdf_structure::files::StoreAdmission<'_>,
    ) -> Result<bool, String> {
        let mut result = false;
        admit(&mut || {
            result = self
                .update_note_link_doc_hash(user_data, path, hash)
                .map_err(|message| main_core::node_fs::NodeError {
                    code: None,
                    message,
                })?;
            Ok(())
        })
        .map_err(|error| error.message)?;
        Ok(result)
    }
    fn update_note_link_doc_hash(
        &self,
        _u: &str,
        pdf_path: &str,
        hash: &str,
    ) -> Result<bool, String> {
        self.line(json!(["ResultStore.updateNoteLinkDocHash", pdf_path, hash]));
        self.0.get("updateNoteLinkDocHash").map(|_| true)
    }
    fn document_hash_of_file(&self, path: &str) -> Result<String, String> {
        self.line(json!(["documentHashOfFile", path]));
        read(path).map(|b| main_core::markdown::fs_util::sha256_hex(&b))
    }
}

type WatchListener = Box<dyn Fn(Option<String>) + Send + Sync>;

/// `PdfShell` answering from the case; it records the hooks it was called
/// with and keeps folder-watch listeners so a test can fire them
#[derive(Default)]
pub struct FakeShell {
    answers: Arc<Answers>,
    pub calls: Arc<Mutex<Vec<String>>>,
    pub watchers: Mutex<Vec<(u64, String, WatchListener)>>,
    uuids: Mutex<u64>,
}

impl FakeShell {
    fn log(&self, call: String, line: Value) {
        self.calls.lock().unwrap().push(call);
        self.answers.line(line);
    }
    fn flag(&self, name: &str) -> bool {
        self.answers
            .take(name)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
    fn configured(&self, name: &str) -> bool {
        self.answers.case.get(name).is_some()
    }
    fn create_document_fixture(
        answers: &Answers,
        calls: &Mutex<Vec<String>>,
        request: &IpcValue,
        owner: ViewId,
    ) -> Option<Result<IpcValue, String>> {
        answers.case.get("createDocument")?;
        calls
            .lock()
            .unwrap()
            .push(format!("createDocument {} {request:?}", owner.0));
        answers.line(json!(["createDocument", js(request), owner.0]));
        answers
            .get("createDocument")
            .map(|value| value.map(|value| from_json(&value)))
            .transpose()
    }
}

impl PdfShell for FakeShell {
    fn ui_lang(&self) -> String {
        self.answers
            .take("uiLang")
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "en".into())
    }
    fn username(&self) -> Option<String> {
        self.answers
            .take("username")
            .and_then(|v| v.as_str().map(str::to_owned))
    }
    /// the host's, as the TypeScript's `process.platform`
    fn platform(&self) -> Platform {
        if cfg!(target_os = "macos") {
            Platform::Darwin
        } else if cfg!(windows) {
            Platform::Win32
        } else {
            Platform::Other
        }
    }
    fn is_path_open_elsewhere(&self, _path: &str) -> bool {
        self.flag("isPathOpenElsewhere")
    }
    fn open_generated_path(&self, path: &str) -> Result<bool, String> {
        self.log(
            format!("openGeneratedPath {path}"),
            json!(["openGeneratedPath", path]),
        );
        Ok(self.flag("openGeneratedPath"))
    }
    fn create_document_owned(
        &self,
        request: IpcValue,
        owner: ViewId,
        _origin: shell_contract::ipc::NativeRequestOwner,
        _destination: shell_contract::ipc::NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        done: Box<dyn FnOnce(Result<IpcValue, String>) + Send>,
    ) {
        if cancel.is_cancelled() || pending.is_cancelled() {
            return done(Err("Document creation request expired".into()));
        }
        let answers = self.answers.clone();
        let calls = self.calls.clone();
        let result = main_core::packages::electron_utils::atomic_write::owned::admitted(
            &admission,
            move || Self::create_document_fixture(&answers, &calls, &request, owner),
        )
        .map_err(|error| error.message)
        .and_then(|result| {
            result
                .unwrap_or_else(|| Err("pdf: document creation is unavailable in this host".into()))
        });
        done(result);
    }
    fn create_document_synthetic(
        &self,
        request: &IpcValue,
        owner: ViewId,
    ) -> Option<Result<IpcValue, String>> {
        Self::create_document_fixture(&self.answers, &self.calls, request, owner)
    }
    fn open_pdf_tools(&self, path: &str, tool: &str) -> Option<bool> {
        if !self.configured("openPdfTools") {
            return None;
        }
        self.log(
            format!("openPdfTools {path} {tool}"),
            json!(["openPdfTools", path, tool]),
        );
        self.answers.take("openPdfTools").and_then(|v| v.as_bool())
    }
    fn has_note_hooks(&self) -> bool {
        self.flag("noteHooks")
    }
    fn open_note(&self, path: &str, done: Box<dyn FnOnce(Result<bool, String>)>) {
        self.log(format!("openNote {path}"), json!(["openNote", path]));
        done(Ok(true));
    }
    fn insert_into_note(
        &self,
        path: &str,
        text: &str,
        done: Box<dyn FnOnce(Result<bool, String>)>,
    ) {
        self.log(
            format!("insertIntoNote {path}"),
            json!(["insertIntoNote", path, text]),
        );
        done(Ok(true));
    }
    fn new_document_dir(&self, _view: ViewId) -> Option<String> {
        self.answers
            .take("newDocumentDir")
            .and_then(|v| v.as_str().map(str::to_owned))
    }
    fn redaction_saved_owned(
        &self,
        view: ViewId,
        path: &str,
        origin: shell_contract::ipc::NativeRequestOwner,
        _destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    ) {
        let extra: Vec<_> = additional.iter().collect();
        let result = effects.with_owned_effect_many_extra(
            &origin,
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || self.redaction_saved(view, path),
        );
        done(result.map(|()| shell_contract::hosts::SavedFile {
            path: path.to_owned(),
            session: shell_contract::hosts::SavedSessionReceipt::NoPublication,
        }));
    }
    fn redaction_saved_existing_document(
        &self,
        view: ViewId,
        revision: shell_contract::ipc::NativeSaveRevisionAuthority,
        path: &str,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        _on_revision: std::rc::Rc<
            dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>,
        >,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    ) {
        let snapshot = match revision.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return done(Err(error)),
        };
        let additional: Vec<_> = additional.iter().collect();
        let result = effects.with_native_save_revision_snapshot_effect(
            &snapshot,
            &participants,
            &cancel,
            &pending,
            &additional,
            &mut || {
                if snapshot.selection().path() != std::path::Path::new(path) {
                    return Err("PDF save target does not match its selected destination".into());
                }
                self.redaction_saved(view, path)
            },
        );
        done(result.map(|()| shell_contract::hosts::SavedFile {
            path: path.to_owned(),
            session: shell_contract::hosts::SavedSessionReceipt::NoPublication,
        }));
    }
    fn redaction_saved(&self, view: ViewId, path: &str) -> Result<(), String> {
        self.log(
            format!("redactionSaved {} {path}", view.0),
            json!(["pdfRedactionSavedHook", view.0, path]),
        );
        Ok(())
    }
    fn renamed(&self, view: ViewId, old_path: &str, new_path: &str) -> Result<(), String> {
        self.log(
            format!("renamed {} {old_path} {new_path}", view.0),
            json!(["pdfRenamedHook", view.0, old_path, new_path]),
        );
        Ok(())
    }
    fn reload(&self, view: ViewId) {
        self.log(
            format!("reload {}", view.0),
            json!(["webContents.reload", view.0]),
        );
    }
    fn watch_dir(&self, dir: &str, on_event: WatchListener) -> Option<u64> {
        let mut w = self.watchers.lock().unwrap();
        let id = w.len() as u64 + 1;
        w.push((id, dir.to_owned(), on_event));
        Some(id)
    }
    fn close_watch(&self, handle: u64) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("closeWatch {handle}"));
        self.watchers
            .lock()
            .unwrap()
            .retain(|(id, _, _)| *id != handle);
    }
    /// numbered from 1 in each case, as the group's `randomUUID`
    fn random_uuid(&self) -> String {
        let mut n = self.uuids.lock().unwrap();
        *n += 1;
        format!("00000000-0000-4000-8000-{:012}", *n)
    }
    fn ocr_env(&self) -> OcrEnv {
        OcrEnv {
            platform: Platform::Other,
            here: String::new(),
            resources_path: None,
        }
    }
}

fn answers(case: &Value, key: &str, log: Option<Log>) -> Answers {
    Answers {
        case: case.get(key).cloned().unwrap_or(Value::Null),
        used: Mutex::default(),
        log,
    }
}

#[cfg(test)]
pub fn engine(case: &Value) -> Arc<FakeEngine> {
    Arc::new(FakeEngine(Arc::new(answers(case, "engine", None)), true))
}

#[cfg(test)]
pub fn engine_requiring_native_authority(case: &Value) -> Arc<FakeEngine> {
    Arc::new(FakeEngine(Arc::new(answers(case, "engine", None)), false))
}

#[cfg(test)]
pub fn shell(case: &Value) -> Arc<FakeShell> {
    Arc::new(FakeShell {
        answers: Arc::new(answers(case, "shell", None)),
        ..Default::default()
    })
}

/// The engine answers from `case.engine`, the shell from `case.shell`; both
/// log to the case's host log when there is one
pub(super) fn insert(hosts: &mut shell_contract::Hosts, case: &Value) {
    let log = hosts.get::<Log>().cloned();
    hosts.insert::<PdfEngineHost>(Arc::new(FakeEngine(
        Arc::new(answers(case, "engine", log.clone())),
        true,
    )));
    hosts.insert::<PdfShellHost>(Arc::new(FakeShell {
        answers: Arc::new(answers(case, "shell", log)),
        ..Default::default()
    }));
}

#[cfg(test)]
mod signing_session_tests {
    use super::*;
    use shell_contract::ipc::{NativeContext, NativeRequestOwner};
    use shell_contract::runtime::RendererGeneration;

    #[test]
    fn signing_session_is_bound_to_opened_chain_and_renderer() {
        let engine = engine(&Value::Null);
        let generation = RendererGeneration::new();
        let owner = NativeRequestOwner {
            view: ViewId(1),
            context: NativeContext::with_renderer((), generation),
        };
        let cancel = CancelToken::default();
        let entry_cancel = CancelToken::default();
        let source = CertificateSource::P12 {
            p12: vec![1],
            password: String::new(),
        };
        let (mut session, _, chain) = engine
            .open_signing_session(&source, &owner, &cancel, &entry_cancel)
            .unwrap();
        let options = SignOptions::default();
        assert!(
            session
                .sign(b"pdf", &options, &[vec![1]], &owner, &cancel)
                .is_err()
        );
        assert_eq!(
            session
                .sign(b"pdf", &options, &chain, &owner, &cancel)
                .unwrap(),
            b"pdf%signed"
        );
        let replacement = NativeRequestOwner {
            view: ViewId(1),
            context: NativeContext::with_renderer((), RendererGeneration::new()),
        };
        assert!(!session.is_current(&replacement, &cancel));
        entry_cancel.cancel();
        assert!(!session.is_current(&owner, &cancel));
    }
}
