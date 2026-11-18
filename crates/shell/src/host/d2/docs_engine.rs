//! Production Word engine: native document state and exact vendor services.

use std::sync::Arc;
use std::time::Duration;

use main_core::docs::doc_format::Materialized;
use main_core::docs::byte_handoff::MediaPermit;
use main_core::docs::hwp_export::{HwpConversion, HwpError, HwpFormat};
use main_core::docs::lazy_media::{LazyMediaBody, OpenedLazyDocx};
use shell_contract::ViewId;
use shell_contract::runtime::CancelToken;
use shell_ipc_d2::docs::hosts::{AnalyzeMediaRequest, DecryptError, DocsEngine, DocsEngineHost};

use super::{docs_crypto, docs_csv, docs_lazy::LazyHost, docs_legacy, docs_media, docs_merge, docs_pdf, docs_vendor};

struct Engine {
    media: &'static LazyHost,
}

pub(super) fn host(media: &'static LazyHost) -> DocsEngineHost {
    Arc::new(Engine { media })
}

impl DocsEngine for Engine {
    fn open_lazy_docx(&self, path: &str, file: Option<Arc<std::fs::File>>, permit: MediaPermit, cancel: CancelToken) -> Result<Option<OpenedLazyDocx>, String> {
        self.media.open(path, file, permit, cancel)
    }

    fn open_lazy_saved(&self, path: &str, input: shell_contract::ipc::NativeSavedInput, permit: MediaPermit, cancel: CancelToken) -> Result<Option<OpenedLazyDocx>, String> {
        self.media.open_saved(path, input, permit, cancel)
    }

    fn adopt_lazy_saved(&self, bytes: &[u8], path: &str, input: shell_contract::ipc::NativeSavedInput, permit: MediaPermit, cancel: CancelToken) -> Result<(), String> {
        self.media.adopt_saved(bytes, path, input, permit, cancel)
    }

    fn adopt_lazy_decrypted(&self, bytes: &[u8], path: &str, plain: Arc<Vec<u8>>, permit: MediaPermit, cancel: CancelToken) -> Result<(), String> {
        self.media.adopt_decrypted(bytes, path, plain, permit, cancel)
    }

    fn adopt_lazy_media_hashes(&self, bytes: &[u8], path: &str, file: Option<Arc<std::fs::File>>, permit: MediaPermit, cancel: CancelToken) -> Result<(), String> {
        self.media.adopt(bytes, path, file, permit, cancel)
    }

    fn materialize_lazy_docx(&self, bytes: Vec<u8>, permit: MediaPermit, cancel: CancelToken) -> Result<Materialized, String> {
        self.media.materialize(bytes, permit, cancel)
    }

    fn point_lazy_media_at(&self, hashes: &[String], path: &str, permit: MediaPermit, plain: Option<Vec<u8>>) -> Result<(), String> {
        self.media.point(hashes, path, permit, plain)
    }

    fn move_lazy_media_source(&self, old_path: &str, new_path: &str) -> Result<(), String> {
        self.media.moved(old_path, new_path)
    }

    fn forget_lazy_media_owner(&self, permit: MediaPermit) -> Result<(), String> {
        self.media.forget(permit)
    }

    fn read_lazy_media(&self, url: &str, permit: MediaPermit, cancel: CancelToken) -> Result<Option<LazyMediaBody>, String> {
        self.media.read(url, permit, cancel)
    }

    fn hand_off_bytes(&self, bytes: Vec<u8>, permit: MediaPermit) -> Result<String, String> {
        self.media.handoff(bytes, permit)
    }

    fn decrypt_docx(&self, bytes: &[u8], password: &str) -> Result<Vec<u8>, DecryptError> {
        docs_crypto::decrypt(bytes, password)
    }

    fn encrypt_docx(&self, bytes: &[u8], password: &str) -> Result<Vec<u8>, String> {
        docs_crypto::encrypt(bytes, password)
    }

    fn convert_legacy_doc(&self, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
        docs_legacy::convert(bytes)
    }

    fn convert_legacy_doc_recovery(&self, bytes: Vec<u8>, receipt: shell_contract::ipc::RecoverySeedReceipt, receiver: shell_contract::ipc::NativeRequestOwner, cancel: CancelToken) -> Result<Vec<u8>, String> {
        docs_legacy::convert_recovery(bytes, receipt, receiver, cancel)
    }

    fn html_to_docx(&self, html: Vec<u8>, timeout: Duration, view: ViewId, cancel: CancelToken) -> Result<Option<Vec<u8>>, String> {
        docs_vendor::html_to_docx(html, timeout, view, cancel)
    }

    fn read_merge_data_source(&self, path: &str) -> Result<serde_json::Value, String> {
        let extension = jscompat::node_path::host::extname(path).to_lowercase();
        let bytes = std::fs::read(main_core::node_fs::namespaced(path))
            .map_err(|error| main_core::node_fs::node_error(&error, "open", path).message)?;
        let rows = match extension.as_str() {
            ".csv" => docs_csv::rows(&bytes)?,
            ".xlsx" => docs_vendor::read_xlsx_rows(&bytes)?,
            _ => return Err(format!("unsupported data source: {}", if extension.is_empty() { path } else { &extension })),
        };
        Ok(docs_merge::from_rows(path, rows))
    }

    fn add_pdf_internal_links(&self, pdf: Vec<u8>, links: serde_json::Value) -> Result<Vec<u8>, String> {
        docs_pdf::add_links(pdf, links)
    }

    fn merge_pdfs(&self, parts: Vec<Vec<u8>>, links: Option<serde_json::Value>) -> Result<Vec<u8>, String> {
        docs_pdf::merge(parts, links)
    }

    fn convert_to_hwp(&self, docx: Vec<u8>, format: HwpFormat) -> Result<HwpConversion, HwpError> {
        docs_vendor::convert_to_hwp(docx, format)
    }

    fn analyze_media(&self, request: AnalyzeMediaRequest, cancel: CancelToken) -> Result<serde_json::Value, String> {
        docs_media::analyze(request, &cancel)
    }
}
