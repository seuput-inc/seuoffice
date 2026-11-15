//! Production EPUB engine host over the native book engine and shared result store.

mod chapter;
mod store;
mod translation;

use std::sync::Arc;
use main_core::epub::store_handlers::{StoreError, StoredTranslation};
use main_core::packages::pdf_structure::TranslationMode;
use serde_json::Value;
use shell_ipc_d2::epub::hosts::{BlockEdits, Bytes, ChapterMap, DecodedText, EpubEngine, EpubEngineHost, OpenedBook, Replacement, SavedBytes, Translated, TranslatedRequest};

struct Engine;

pub(super) fn host() -> EpubEngineHost { Arc::new(Engine) }

impl EpubEngine for Engine {
    fn open(&self, bytes: Bytes) -> Result<OpenedBook, String> {
        super::epub_book::open(bytes)
    }
    fn read_text(&self, bytes: Bytes, path: &str) -> Option<DecodedText> {
        super::epub_book::read_text(bytes, path)
    }
    fn open_chapter(&self, text: &str) -> Result<ChapterMap, String> {
        chapter::open(text)
    }
    fn apply_block_edits(&self, map: &ChapterMap, edits: &[Value]) -> Result<BlockEdits, String> {
        chapter::apply(map, edits)
    }
    fn compose_replacements(&self, stored: &str, earlier: &[Replacement], later: &[Replacement]) -> Vec<Replacement> {
        super::epub_ranges::compose(stored, earlier, later)
    }
    fn validate_replacements(&self, replacements: &[Replacement], length: usize) -> Option<String> {
        super::epub_ranges::validate(replacements, length)
    }
    fn apply_replacements(&self, source: &str, replacements: &[Replacement]) -> Result<String, String> {
        super::epub_ranges::apply(source, replacements)
    }
    fn remap_chapter(&self, map: &ChapterMap, text: &str, replacements: &[Replacement]) -> Result<ChapterMap, String> {
        chapter::remap(map, text, replacements)
    }
    fn extract_chapter_text(&self, source: &str) -> Result<String, String> {
        epub_engine::text::extract_chapter_text(source).map_err(|error| error.message)
    }
    fn save(&self, bytes: Bytes, edits: &[(String, Vec<Replacement>)]) -> Result<SavedBytes, String> {
        super::epub_save::save(bytes, edits, main_core::packages::pdf_structure::files::system_now_ms())
    }
    fn migration_info(&self, saved: Bytes, old_doc_hash: &str, new_doc_hash: &str) -> Result<Value, String> {
        translation::migration(saved, old_doc_hash, new_doc_hash)
    }
    fn translation_units(&self, bytes: Bytes, chapters: Option<&[usize]>) -> Result<Value, String> {
        translation::units(bytes, chapters)
    }
    fn build_translated(&self, bytes: Bytes, request: TranslatedRequest) -> Result<Translated, String> {
        translation::build(bytes, request)
    }
    fn parse_file_to_text(&self, path: &str) -> Result<String, Option<String>> {
        let parsed = crate::host::w_files::parsed_value(path).map_err(Some)?;
        if parsed.get("ok").and_then(Value::as_bool) == Some(true)
            && parsed.get("kind").and_then(Value::as_str) == Some("text") {
            return parsed.get("text").and_then(Value::as_str).map(str::to_owned).ok_or(None);
        }
        if parsed.get("damaged").and_then(Value::as_bool) == Some(true) { return Err(None); }
        Err(parsed.get("error").and_then(Value::as_str).map(str::to_owned))
    }
    fn picture_holds_diagram(&self, path: &str) -> bool {
        crate::host::w_files::diagram_picture_ext(std::path::Path::new(path)).is_some()
    }
    fn store_list_translations(&self, dir: &str, doc_hash: &str, mode: Option<TranslationMode>) -> Result<Vec<String>, StoreError> {
        store::store_list_translations(dir, doc_hash, mode)
    }
    fn store_read_translation(&self, dir: &str, doc_hash: &str, lang: &str, mode: Option<TranslationMode>) -> Result<Option<StoredTranslation>, StoreError> {
        store::store_read_translation(dir, doc_hash, lang, mode)
    }
    fn store_unfinished_jobs(&self, dir: &str, doc_hash: &str) -> Result<Vec<Value>, StoreError> {
        store::store_unfinished_jobs(dir, doc_hash)
    }
    fn store_write_translation_meta(&self, dir: &str, doc_hash: &str, lang: &str, provider: &str, model: &str, mode: Option<TranslationMode>) -> Result<(), StoreError> {
        store::store_write_translation_meta(dir, doc_hash, lang, provider, model, mode)
    }
    fn store_write_translation_blocks(&self, dir: &str, doc_hash: &str, lang: &str, blocks: &[Value], mode: Option<TranslationMode>) -> Result<(), StoreError> {
        store::store_write_translation_blocks(dir, doc_hash, lang, blocks, mode)
    }
    fn store_delete_translation(&self, dir: &str, doc_hash: &str, lang: &str, mode: Option<TranslationMode>) -> Result<(), StoreError> {
        store::store_delete_translation(dir, doc_hash, lang, mode)
    }
    fn store_delete_translation_except_edited(&self, dir: &str, doc_hash: &str, lang: &str, mode: Option<TranslationMode>) -> Result<f64, StoreError> {
        store::store_delete_translation_except_edited(dir, doc_hash, lang, mode)
    }
    fn store_write_job(&self, dir: &str, doc_hash: &str, job: &Value) -> Result<(), StoreError> {
        store::store_write_job(dir, doc_hash, job)
    }
    fn store_heartbeat_job(&self, dir: &str, doc_hash: &str, kind: &str, key: Option<&str>, mode: Option<TranslationMode>) -> Result<bool, StoreError> {
        store::store_heartbeat_job(dir, doc_hash, kind, key, mode)
    }
    fn store_migrate_document(&self, dir: &str, info: Value) -> Result<Value, StoreError> {
        store::store_migrate_document(dir, info)
    }
    fn store_migrate_document_owned(&self, dir: &str, info: Value,
        admit: &mut main_core::packages::pdf_structure::files::StoreAdmission<'_>) -> Result<Value, StoreError> {
        store::store_migrate_document_owned(dir, info, admit)
    }
}
