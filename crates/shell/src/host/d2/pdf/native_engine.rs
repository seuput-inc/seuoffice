//! Native PDF editor stages and isolated worker entrypoints.

mod annotation_choice;
mod annotation_delete;
mod blank_pages;
mod clipboard;
mod decrypt;
pub(crate) mod document_edits;
mod form_draw_tree;
mod generate_image;
pub(crate) mod image_edits;
mod image_queries;
mod redaction;
mod save;
mod save_policy;
mod save_verify;
mod shrink;
mod signatures;
mod static_fills;
mod text_edits;

use std::sync::Arc;

use super::{crypto, file_ops, note_store, page_ops, signed_commit};
use main_core::pdf::note_link::{FoundNoteLink, NoteLink, NoteLinks};
use main_core::pdf::system_certificates::CertSummary;
use shell_contract::{IpcValue, runtime::CancelToken};
use shell_ipc_d2::pdf::editor::hosts::{
    CertificateOpenError, CertificateSource, DecryptError, Decrypted, PdfEngine, PdfText,
    SaveSkips, Shrunk, SigningSession,
};

/// Not registered until signing and vendor parity have received hosted proof.
pub(crate) struct NativePdfEngine {
    signing_authority: Arc<dyn crypto::SigningAuthority>,
    token_interaction: Arc<dyn crypto::ProtectedTokenInteraction>,
}

impl NativePdfEngine {
    pub(crate) fn new(
        signing_authority: Arc<dyn crypto::SigningAuthority>,
        token_interaction: Arc<dyn crypto::ProtectedTokenInteraction>,
    ) -> Self {
        Self {
            signing_authority,
            token_interaction,
        }
    }
}

impl PdfEngine for NativePdfEngine {
    fn synthetic_save_capability(&self) -> bool {
        false
    }
    fn prepare_pdf_save(
        &self,
        source: &[u8],
        request: &IpcValue,
        preserve_unchanged: bool,
        cancel: &CancelToken,
    ) -> Result<(Vec<u8>, SaveSkips), String> {
        save::prepared(source, request, preserve_unchanged, cancel)
    }
    fn save_pdf_to_path(
        &self,
        source: &str,
        target: &str,
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<SaveSkips, String> {
        if cancel.is_cancelled() {
            return Err("PDF save cancelled".into());
        }
        let bytes = std::fs::read(source).map_err(|error| error.to_string())?;
        save::save_to_path(&bytes, target, request, false, cancel)
    }
    fn save_pdf_bytes_to_path(
        &self,
        source: &[u8],
        target: &str,
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<SaveSkips, String> {
        save::save_to_path(source, target, request, true, cancel)
    }
    fn copy_pdf_to_path(
        &self,
        source: &str,
        target: &str,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        file_ops::copy_pdf_to_path(source, target, cancel)
    }
    fn pdf_bytes_with_edits(
        &self,
        source: &str,
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, String> {
        if cancel.is_cancelled() {
            return Err("PDF save cancelled".into());
        }
        let bytes = std::fs::read(source).map_err(|error| error.to_string())?;
        save::bytes_with_edits(&bytes, request, false, cancel)
    }
    fn pdf_bytes_with_edits_of(
        &self,
        source: &[u8],
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, String> {
        save::bytes_with_edits(source, request, false, cancel)
    }
    fn pdf_bytes_with_edits_from(
        &self,
        source: &[u8],
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>, String> {
        save::bytes_with_edits(source, request, true, cancel)
    }
    fn read_static_form_fills(&self, bytes: &[u8]) -> Result<IpcValue, String> {
        page_ops::read_static_form_fills(bytes)
    }
    fn extract_pages(&self, bytes: &[u8], pages: &IpcValue) -> Result<Vec<u8>, String> {
        page_ops::extract_pages(bytes, pages)
    }
    fn insert_pdf(
        &self,
        target: &[u8],
        other: &[u8],
        after: f64,
    ) -> Result<(Vec<u8>, f64), String> {
        page_ops::insert_pdf(target, other, after)
    }
    fn insert_blank_page(&self, bytes: &[u8], after: f64) -> Result<Vec<u8>, String> {
        page_ops::insert_blank_page(bytes, after)
    }
    fn split_pdf(&self, bytes: &[u8], size: f64) -> Result<Vec<Vec<u8>>, String> {
        page_ops::split_pdf(bytes, size)
    }
    fn merge_pages(
        &self,
        bytes: &[u8],
        per_sheet: f64,
        horizontal: bool,
        separator: bool,
    ) -> Result<Vec<u8>, String> {
        page_ops::merge_pages(bytes, per_sheet, horizontal, separator)
    }
    fn merge_pdf(&self, base: &[u8], others: &[Vec<u8>]) -> Result<(Vec<u8>, f64), String> {
        page_ops::merge_pdf(base, others)
    }
    fn replace_pages(
        &self,
        target: &[u8],
        other: &[u8],
        pages: &IpcValue,
    ) -> Result<(Vec<u8>, IpcValue, IpcValue), String> {
        page_ops::replace_pages(target, other, pages)
    }
    fn set_page_size(&self, bytes: &[u8], width: f64, height: f64) -> Result<Vec<u8>, String> {
        page_ops::set_page_size(bytes, width, height)
    }
    fn split_pages(&self, bytes: &[u8], per_page: f64) -> Result<Vec<u8>, String> {
        page_ops::split_pages(bytes, per_page)
    }
    fn crop_pages(
        &self,
        bytes: &[u8],
        pages: &IpcValue,
        rect: &IpcValue,
    ) -> Result<Vec<u8>, String> {
        page_ops::crop_pages(bytes, pages, rect)
    }
    fn copied_pages_of(
        &self,
        source: &[u8],
        request: &IpcValue,
        positions: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<Option<(Vec<u8>, f64)>, String> {
        let edited = save::bytes_with_edits(source, request, true, cancel)?;
        clipboard::copied_pages(&edited, positions, cancel)
    }
    fn shrink(&self, source: &[u8], cancel: &CancelToken) -> Result<Shrunk, String> {
        shrink::shrink(source, cancel)
    }
    fn decrypt_document(&self, bytes: &[u8], password: &str) -> Result<Decrypted, DecryptError> {
        decrypt::decrypt_document(bytes, password)
    }
    fn read_annot_subjects(&self, bytes: &[u8]) -> Result<IpcValue, String> {
        page_ops::read_annot_subjects(bytes)
    }
    fn list_page_images(&self, bytes: &[u8], cancel: &CancelToken) -> Result<IpcValue, String> {
        image_queries::list_page_images(bytes, cancel)
    }
    fn render_image_png(
        &self,
        bytes: &[u8],
        page_index: f64,
        rect: &IpcValue,
        scale: f64,
        cancel: &CancelToken,
    ) -> Result<IpcValue, String> {
        image_queries::render_image_png(bytes, page_index, rect, scale, cancel)
    }
    fn render_page_preview_png(
        &self,
        bytes: &[u8],
        request: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<IpcValue, String> {
        image_queries::render_page_preview_png(bytes, request, cancel)
    }
    fn validate_text_edits(
        &self,
        bytes: &[u8],
        edits: &IpcValue,
        cancel: &CancelToken,
    ) -> Result<IpcValue, String> {
        match edits {
            IpcValue::Array(edits) => text_edits::validate_text_edits(bytes, edits, cancel),
            _ => Err("invalid text edits".into()),
        }
    }
    fn can_draw_text(
        &self,
        text: &str,
        font: Option<&str>,
        bold: bool,
        italic: bool,
    ) -> Result<bool, String> {
        text_edits::can_draw_text(text, font, bold, italic)
    }
    fn read_pdf_text(&self, bytes: &[u8], password: Option<&str>) -> Result<PdfText, String> {
        page_ops::read_pdf_text(bytes, password)
    }
    fn read_pdf_signatures(&self, bytes: &[u8]) -> Result<IpcValue, String> {
        signatures::read_pdf_signatures(bytes)
    }
    fn describe_certificate(&self, input: &[u8]) -> Option<CertSummary> {
        crypto::describe_certificate(input)
    }
    fn open_signing_session(
        &self,
        source: &CertificateSource,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
        entry_cancel: &CancelToken,
    ) -> Result<(Box<dyn SigningSession>, Vec<IpcValue>, Vec<Vec<u8>>), CertificateOpenError> {
        crypto::open_signing_session_with_authority(
            source,
            owner,
            cancel,
            entry_cancel,
            self.signing_authority.clone(),
            self.token_interaction.clone(),
        )
    }
    fn sign_owner_current(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
    ) -> bool {
        self.signing_authority.is_current(owner, cancel)
    }
    fn write_signed_pdf(
        &self,
        target: &str,
        bytes: &[u8],
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        signed_commit::write_signed_pdf(
            target,
            bytes,
            owner,
            cancel,
            self.signing_authority.as_ref(),
        )
    }
    fn image_subscription_statuses(&self) -> jscompat::JsonValue {
        crate::host::net::ai::subscription_statuses()
    }
    fn generate_image(
        &self,
        settings: &str,
        prompt: &str,
        aspect: Option<&str>,
        open: Option<&str>,
        pasted: &str,
        statuses: jscompat::JsonValue,
        cancel: &CancelToken,
    ) -> Result<IpcValue, String> {
        generate_image::generate_image(
            settings,
            prompt,
            aspect,
            open,
            pasted,
            statuses,
            cancel,
            crate::host::net::ai::media_tools::generate_image::generate_image,
        )
    }
    fn read_note_links(&self, user_data: &str) -> Result<NoteLinks, String> {
        note_store::read_note_links(user_data)
    }
    fn find_note_link(
        &self,
        user_data: &str,
        path: &str,
        hash: &str,
    ) -> Result<Option<FoundNoteLink>, String> {
        note_store::find_note_link(user_data, path, hash)
    }
    fn set_note_link(&self, user_data: &str, path: &str, link: &NoteLink) -> Result<(), String> {
        note_store::set_note_link(user_data, path, link)
    }
    fn update_note_link_doc_hash(
        &self,
        user_data: &str,
        path: &str,
        hash: &str,
    ) -> Result<bool, String> {
        note_store::update_note_link_doc_hash(user_data, path, hash)
    }
    fn update_note_link_doc_hash_guarded(
        &self,
        user_data: &str,
        path: &str,
        hash: &str,
        admit: &mut main_core::packages::pdf_structure::files::StoreAdmission<'_>,
    ) -> Result<bool, String> {
        note_store::update_note_link_doc_hash_guarded(user_data, path, hash, admit)
    }
    fn document_hash_of_file(&self, path: &str) -> Result<String, String> {
        note_store::document_hash_of_file(path)
    }
}

#[cfg(test)]
mod authority_tests {
    use super::*;
    struct NoTokenPrompt;
    impl crypto::ProtectedTokenInteraction for NoTokenPrompt {
        fn confirm(
            &self,
            _: &shell_contract::ipc::NativeRequestOwner,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
        ) -> Result<bool, String> {
            panic!("capability inspection must not open a token prompt")
        }
    }

    #[test]
    fn native_engine_never_installs_synthetic_save_admission() {
        let engine = NativePdfEngine::new(
            Arc::new(signed_commit::NativeSigningAuthority::new()),
            Arc::new(NoTokenPrompt),
        );
        assert!(!engine.synthetic_save_capability());
    }
}
