//! What the PDF editor reaches that is not in Rust: the engines it calls
//! ([`PdfEngine`]: pdf-lib, pdfium, node-forge, `pdf-crypt`, `pdf-shrink`,
//! ai-search, the structure result store) and what the shell does for it
//! ([`PdfShell`]: the `RuntimePaths` hooks of `configurePdfRuntime`, the
//! split-pane probe, the UI language, folder watching, view reloads).
//!
//! Every method takes and returns owned data, so a handler can run it on a
//! worker. The TypeScript around each call (argument checks, state, the reply
//! shape) stays in the handlers.

use jscompat::JsonValue;
use std::sync::Arc;

use main_core::pdf::font_locate::Platform;
use main_core::pdf::note_link::{FoundNoteLink, NoteLink, NoteLinks};
use main_core::pdf::ocr::OcrEnv;
use main_core::pdf::system_certificates::{CertSummary, InPlaceSigner};
use shell_contract::IpcValue;
use shell_contract::ViewId;
use shell_contract::runtime::CancelToken;

/// What a save wrote around: the edits it could not apply (`SavePdfSkips`),
/// each list as the renderer receives it
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SaveSkips {
    pub text_edits: Vec<IpcValue>,
    pub text_inserts: Vec<IpcValue>,
    pub image_edits: Vec<IpcValue>,
}

/// `decryptDocument`'s answer, the fields `pdf:decrypt` passes on
#[derive(Clone, Debug, PartialEq)]
pub struct Decrypted {
    pub bytes: Vec<u8>,
    pub permissions: IpcValue,
    pub needed_password: bool,
    /// `'user' | 'owner'`
    pub unlocked_as: String,
}

/// What `decryptDocument` throws
#[derive(Clone, Debug, PartialEq)]
pub enum DecryptError {
    /// `WrongPasswordError`
    WrongPassword,
    /// `CertificateEncryptionError`
    CertificateEncryption,
    /// any other error, its message
    Other(String),
}

/// `PDFDocument.load` + `shrinkDocument` + `doc.save({ useObjectStreams: true })`
#[derive(Clone, Debug, PartialEq)]
pub struct Shrunk {
    pub bytes: Vec<u8>,
    pub streams_recompressed: f64,
    pub images_downsampled: f64,
}

/// `readPdfText`'s answer as compare uses it
#[derive(Clone, Debug, PartialEq)]
pub struct PdfText {
    pub page_count: f64,
    pub pages: Vec<String>,
}

/// What signs: PKCS#12 bytes with their password, or an OS store that signs
/// in place (`CertMaterial`)
#[derive(Clone, Debug, PartialEq)]
pub enum CertMaterial {
    P12 { p12: Vec<u8>, password: String },
    InPlace(InPlaceSigner),
}

/// The selected certificate, resolved from the editor's native certificate entry.
#[derive(Clone)]
pub enum CertificateSource {
    P12 {
        p12: Vec<u8>,
        password: String,
    },
    System {
        reference: main_core::pdf::system_certificates::SystemCertificateRef,
        store_password: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateOpenCode {
    Invalid,
    Password,
    NoKey,
    UnsupportedKey,
    NotExportable,
    StoreToolMissing,
    StoreLocked,
    StoreUnsupported,
}

impl CertificateOpenCode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cert-invalid" => Some(Self::Invalid),
            "cert-password" => Some(Self::Password),
            "cert-no-key" => Some(Self::NoKey),
            "cert-unsupported-key" => Some(Self::UnsupportedKey),
            "cert-not-exportable" => Some(Self::NotExportable),
            "cert-store-tool-missing" => Some(Self::StoreToolMissing),
            "cert-store-locked" => Some(Self::StoreLocked),
            "cert-store-unsupported" => Some(Self::StoreUnsupported),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "cert-invalid",
            Self::Password => "cert-password",
            Self::NoKey => "cert-no-key",
            Self::UnsupportedKey => "cert-unsupported-key",
            Self::NotExportable => "cert-not-exportable",
            Self::StoreToolMissing => "cert-store-tool-missing",
            Self::StoreLocked => "cert-store-locked",
            Self::StoreUnsupported => "cert-store-unsupported",
        }
    }
}

/// Only bounded, native-defined messages may cross the certificate-worker boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateOpenMessage {
    RegexLimit,
    WorkerUnavailable,
    ResourceExhausted,
    Revoked,
}

impl CertificateOpenMessage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RegexLimit => "Maximum call stack size exceeded",
            Self::WorkerUnavailable => "certificate worker unavailable",
            Self::ResourceExhausted => "certificate worker resource exhausted",
            Self::Revoked => "pdf: signing request expired",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateOpenError {
    Code(CertificateOpenCode),
    Message(CertificateOpenMessage),
}

use shell_contract::pdf_sign_options::SignOptions;

/// What `signPdf` throws: a `SignError`'s code, or another error's message
#[derive(Clone, Debug, PartialEq)]
pub enum SignFailure {
    Code(String),
    Message(String),
}

/// A request-owned signer. Dropping it terminates and reaps its isolated worker.
pub trait SigningSession: Send {
    fn is_current(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
    ) -> bool;
    fn revoke_token(&self) -> CancelToken;
    fn sign(
        &mut self,
        bytes: &[u8],
        options: &SignOptions,
        chain: &[Vec<u8>],
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, SignFailure>;
}

/// The libraries the editor calls that Rust does not have yet. `Err(String)`
/// is the message of what the TypeScript function throws.
pub trait PdfEngine: Send + Sync {
    /// Installed only by synthetic test/probe hosts, never the native installer.
    fn synthetic_save_capability(&self) -> bool;
    /// Prepare the exact normal save output before a request-owned atomic commit.
    fn prepare_pdf_save(
        &self,
        source: &[u8],
        request: &IpcValue,
        preserve_unchanged: bool,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<(Vec<u8>, SaveSkips), String>;
    /// `savePdfToPath(sourcePath, targetPath, request)` (save-pdf.ts)
    fn save_pdf_to_path(
        &self,
        source: &str,
        target: &str,
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<SaveSkips, String>;
    /// `savePdfBytesToPath(source, targetPath, request)`
    fn save_pdf_bytes_to_path(
        &self,
        source: &[u8],
        target: &str,
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<SaveSkips, String>;
    /// `copyPdfToPath(sourcePath, targetPath)`
    fn copy_pdf_to_path(
        &self,
        source: &str,
        target: &str,
        cancel: &CancelToken,
    ) -> Result<(), String>;
    /// `pdfBytesWithEdits(sourcePath, request)`
    fn pdf_bytes_with_edits(
        &self,
        source: &str,
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, String>;
    /// `pdfBytesWithEditsOf(fileBytes, request)`
    fn pdf_bytes_with_edits_of(
        &self,
        file: &[u8],
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, String>;
    /// `pdfBytesWithEditsFrom(source, request)`
    fn pdf_bytes_with_edits_from(
        &self,
        source: &[u8],
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, String>;
    /// `readStaticFormFills(bytes)`: `StaticFormFillRecord[]`
    fn read_static_form_fills(&self, bytes: &[u8]) -> Result<IpcValue, String>;
    /// `extractPagesBytes(bytes, pages)`
    fn extract_pages(&self, bytes: &[u8], pages: &IpcValue) -> Result<Vec<u8>, String>;
    /// `insertPdfBytes(target, other, afterPageIndex)`: `{ merged, count }`
    fn insert_pdf(&self, target: &[u8], other: &[u8], after: f64)
    -> Result<(Vec<u8>, f64), String>;
    /// `insertBlankPageBytes(bytes, afterPageIndex)`
    fn insert_blank_page(&self, bytes: &[u8], after: f64) -> Result<Vec<u8>, String>;
    /// `splitPdfBytes(bytes, chunkSize)`
    fn split_pdf(&self, bytes: &[u8], chunk_size: f64) -> Result<Vec<Vec<u8>>, String>;
    /// `mergePagesBytes(bytes, { perSheet, direction, separator })`
    fn merge_pages(
        &self,
        bytes: &[u8],
        per_sheet: f64,
        horizontal: bool,
        separator: bool,
    ) -> Result<Vec<u8>, String>;
    /// `mergePdfBytes(base, others)`: `{ merged, appended }`
    fn merge_pdf(&self, base: &[u8], others: &[Vec<u8>]) -> Result<(Vec<u8>, f64), String>;
    /// `replacePagesBytes(target, other, pages)`: `{ merged, removed, inserted }`
    fn replace_pages(
        &self,
        target: &[u8],
        other: &[u8],
        pages: &IpcValue,
    ) -> Result<(Vec<u8>, IpcValue, IpcValue), String>;
    /// `setPageSizeBytes(bytes, width, height)`
    fn set_page_size(&self, bytes: &[u8], width: f64, height: f64) -> Result<Vec<u8>, String>;
    /// `splitPagesBytes(bytes, perPage)`
    fn split_pages(&self, bytes: &[u8], per_page: f64) -> Result<Vec<u8>, String>;
    /// `cropPagesBytes(bytes, pages, rect)`
    fn crop_pages(
        &self,
        bytes: &[u8],
        pages: &IpcValue,
        rect: &IpcValue,
    ) -> Result<Vec<u8>, String>;
    /// `copiedPagesOf(source, request, positions)` (page-clipboard.ts): the
    /// copied pages' PDF and their count, `None` when no position is a page
    fn copied_pages_of(
        &self,
        source: &[u8],
        request: &IpcValue,
        positions: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<Option<(Vec<u8>, f64)>, String>;
    /// `PDFDocument.load(source)` + `shrinkDocument(doc)` + `doc.save` (pdf-shrink.ts)
    fn shrink(&self, source: &[u8], cancel: &CancelToken) -> Result<Shrunk, String>;
    /// `decryptDocument(bytes, password)` (pdf-crypt.ts)
    fn decrypt_document(&self, bytes: &[u8], password: &str) -> Result<Decrypted, DecryptError>;
    /// `readAnnotSubjects(bytes)` (annot-subjects.ts): `PdfAnnotSubject[]`
    fn read_annot_subjects(&self, bytes: &[u8]) -> Result<IpcValue, String>;
    /// `listPageImages(bytes)` (image-edit.ts): `PageImageRef[]`
    fn list_page_images(&self, bytes: &[u8], cancel: &CancelToken) -> Result<IpcValue, String>;
    /// `renderImagePng(bytes, pageIndex, rect, scale)`: base64 PNG or null
    fn render_image_png(
        &self,
        bytes: &[u8],
        page_index: f64,
        rect: &IpcValue,
        scale: f64,
        cancel: &CancelToken,
    ) -> Result<IpcValue, String>;
    /// `renderPagePreviewPng(bytes, request)`: `PagePreviewResult | null`
    fn render_page_preview_png(
        &self,
        bytes: &[u8],
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<IpcValue, String>;
    /// `validateTextEdits(bytes, edits)` (text-edit.ts): `TextEditValidation[]`
    fn validate_text_edits(
        &self,
        bytes: &[u8],
        edits: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<IpcValue, String>;
    /// `canDrawText(text, font, bold, italic)` (text-edit.ts)
    fn can_draw_text(
        &self,
        text: &str,
        font: Option<&str>,
        bold: bool,
        italic: bool,
    ) -> Result<bool, String>;
    /// `readPdfText(bytes, { password })` (read-text.ts over the pdfium wasm)
    fn read_pdf_text(&self, bytes: &[u8], password: Option<&str>) -> Result<PdfText, String>;
    /// `readPdfSignatures(bytes)` (pdf-signatures.ts): `PdfSignatureInfo[]`
    fn read_pdf_signatures(&self, bytes: &[u8]) -> Result<IpcValue, String>;
    /// `describeCertificate(new X509Certificate(input))` (pdf-signatures.ts),
    /// the reader `listSystemCertificates` and `openSystemSigner` take
    fn describe_certificate(&self, input: &[u8]) -> Option<CertSummary>;
    /// Open and retain the selected identity only in a cancellable isolated worker.
    #[allow(clippy::type_complexity)]
    fn open_signing_session(
        &self,
        source: &CertificateSource,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
        entry_cancel: &CancelToken,
    ) -> Result<(Box<dyn SigningSession>, Vec<IpcValue>, Vec<Vec<u8>>), CertificateOpenError>;
    /// Check the authenticated document lifetime before provider use and commit.
    fn sign_owner_current(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
    ) -> bool;
    /// Commit a signed copy only while the exact authenticated document remains live.
    fn write_signed_pdf(
        &self,
        target: &str,
        bytes: &[u8],
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
    ) -> Result<(), String>;
    fn image_subscription_statuses(&self) -> JsonValue;
    /// `generateImageTool(settingsPath, { prompt, aspectRatio }, { mediaRoots:
    /// documentMediaRoots(openPath, pastedDir), context: { path: 'screen',
    /// statuses: sharedSubscriptionStatuses() }, signal })` (ai-search); the
    /// call ends when `cancel` fires
    #[allow(clippy::too_many_arguments)]
    fn generate_image(
        &self,
        settings_path: &str,
        prompt: &str,
        aspect_ratio: Option<&str>,
        open_path: Option<&str>,
        pasted_dir: &str,
        statuses: JsonValue,
        cancel: &CancelToken,
    ) -> Result<IpcValue, String>;
    /// `ResultStore.readNoteLinks()` (packages/pdf-structure/src/store) of
    /// the store under `user_data`
    fn read_note_links(&self, user_data: &str) -> Result<NoteLinks, String>;
    /// `ResultStore.findNoteLink(pdfPath, docHash)`
    fn find_note_link(
        &self,
        user_data: &str,
        pdf_path: &str,
        doc_hash: &str,
    ) -> Result<Option<FoundNoteLink>, String>;
    /// `ResultStore.setNoteLink(pdfPath, link)`
    fn set_note_link(&self, user_data: &str, pdf_path: &str, link: &NoteLink)
    -> Result<(), String>;
    /// `ResultStore.updateNoteLinkDocHash(pdfPath, docHash)`
    fn update_note_link_doc_hash_guarded(
        &self,
        user_data: &str,
        path: &str,
        hash: &str,
        admit: &mut main_core::packages::pdf_structure::files::StoreAdmission<'_>,
    ) -> Result<bool, String>;
    fn update_note_link_doc_hash(
        &self,
        user_data: &str,
        pdf_path: &str,
        doc_hash: &str,
    ) -> Result<bool, String>;
    /// `documentHashOfFile(path)`
    fn document_hash_of_file(&self, path: &str) -> Result<String, String>;
}

pub type PdfEngineHost = Arc<dyn PdfEngine>;

/// What the shell does for the editor: `configurePdfRuntime`'s hooks, the
/// hooks set with `setPdfRedactionSavedHook`/`setPdfRenamedHook`, and the
/// process facts the TypeScript read from Electron and Node.
pub trait PdfShell: Send + Sync {
    /// `getUiLang()`
    fn ui_lang(&self) -> String;
    /// `os.userInfo().username` (what `configuredAuthorName` falls back to)
    fn username(&self) -> Option<String>;
    /// `process.platform`, as the main-core modules distinguish it
    fn platform(&self) -> Platform;
    /// the probe of `isPathOpenElsewhere`: another pane shows `path`
    fn is_path_open_elsewhere(&self, path: &str) -> bool;
    /// `runtime.openGeneratedPath(path)`: a new SeuOffice tab on `path`;
    /// `Err` is what the hook threw
    fn open_generated_path(&self, path: &str) -> Result<bool, String>;
    /// Explicit synthetic probe hook; native requests use the owned asynchronous creator.
    fn create_document_synthetic(
        &self,
        request: &IpcValue,
        owner: ViewId,
    ) -> Option<Result<IpcValue, String>>;
    #[allow(clippy::too_many_arguments)]
    fn create_document_owned(
        &self,
        request: IpcValue,
        owner: ViewId,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        done: Box<dyn FnOnce(Result<IpcValue, String>) + Send>,
    );
    /// `runtime.openPdfTools(path, tool)`; `None` when not configured
    fn open_pdf_tools(&self, path: &str, tool: &str) -> Option<bool>;
    /// whether both `runtime.openNote` and `runtime.insertIntoNote` are configured
    fn has_note_hooks(&self) -> bool;
    /// `runtime.openNote(path)`; `done` runs once on the core thread, `Err` is what it threw
    fn open_note(&self, path: &str, done: Box<dyn FnOnce(Result<bool, String>)>);
    /// `runtime.insertIntoNote(path, text)`; `done` runs once on the core thread
    fn insert_into_note(&self, path: &str, text: &str, done: Box<dyn FnOnce(Result<bool, String>)>);
    /// `runtime.newDocumentDir(webContentsId)`: the view's project folder
    fn new_document_dir(&self, view: ViewId) -> Option<String>;
    /// `pdfRedactionSavedHook(wc, path)`: a derived copy became the tab's document
    fn redaction_saved(&self, view: ViewId, path: &str) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn redaction_saved_owned(
        &self,
        view: ViewId,
        path: &str,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    );
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
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
        on_revision: std::rc::Rc<
            dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>,
        >,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    );
    /// `pdfRenamedHook(wc, oldPath, newPath)`: tab title, recents, projects follow
    fn renamed(&self, view: ViewId, old_path: &str, new_path: &str) -> Result<(), String>;
    /// `contents.reload()`
    fn reload(&self, view: ViewId);
    /// `fs.watch(dir, { persistent: false }, listener)`: `on_event` gets the
    /// changed name (`null` when the platform gives none) on any thread; the
    /// handle closes with [`PdfShell::close_watch`]. `None` when watching failed.
    fn watch_dir(
        &self,
        dir: &str,
        on_event: Box<dyn Fn(Option<String>) + Send + Sync>,
    ) -> Option<u64>;
    /// `watcher.close()`
    fn close_watch(&self, handle: u64);
    /// `crypto.randomUUID()` (the opaque certificate ids a view gets)
    fn random_uuid(&self) -> String;
    /// what `ocr.ts` reads from the process and its own location
    fn ocr_env(&self) -> OcrEnv;
}

pub type PdfShellHost = Arc<dyn PdfShell>;

#[cfg(test)]
mod certificate_error_tests {
    use super::{CertificateOpenCode, CertificateOpenMessage};

    #[test]
    fn certificate_codes_are_closed_and_round_trip() {
        for code in [
            CertificateOpenCode::Invalid,
            CertificateOpenCode::Password,
            CertificateOpenCode::NoKey,
            CertificateOpenCode::UnsupportedKey,
            CertificateOpenCode::NotExportable,
            CertificateOpenCode::StoreToolMissing,
            CertificateOpenCode::StoreLocked,
            CertificateOpenCode::StoreUnsupported,
        ] {
            assert_eq!(CertificateOpenCode::parse(code.as_str()), Some(code));
        }
        assert_eq!(CertificateOpenCode::parse("cert-invalid: secret"), None);
        assert_eq!(
            CertificateOpenMessage::RegexLimit.as_str(),
            "Maximum call stack size exceeded"
        );
    }
}
