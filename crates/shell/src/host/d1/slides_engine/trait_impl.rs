use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::{
    DeckId, DeckSize, EngineError, GeneratedPage, GroupChildFrame, ImageInfo, MasterPart,
    PictureMedia, RenderOwner, SlideRef, SlidesEngine, Snapshot, TxnRequest, TxnResult,
};

use super::NativeSlidesEngine;

macro_rules! forward {
    ($(fn $name:ident(&self $(, $arg:ident: $ty:ty)*) -> $ret:ty;)+) => {
        $(fn $name(&self $(, $arg: $ty)*) -> $ret {
            NativeSlidesEngine::$name(self $(, $arg)*)
        })+
    };
}

impl SlidesEngine for NativeSlidesEngine {
    forward! {
        fn create_blank_pptx(&self) -> Result<Vec<u8>, EngineError>;
        fn open(&self, bytes: &[u8]) -> Result<DeckId, EngineError>;
        fn deck_model(&self, deck: DeckId) -> Result<jscompat::JsonValue, EngineError>;
        fn transfer_history(&self, old: DeckId, replacement: DeckId, views: &[RenderOwner]) -> Result<(), EngineError>;
        fn bind_view(&self, deck: DeckId, owner: &RenderOwner) -> Result<(), EngineError>;
        fn unbind_view(&self, deck: DeckId, owner: &RenderOwner) -> Result<(), EngineError>;
        fn replace_view(&self, old: DeckId, new: DeckId, owner: &RenderOwner) -> Result<(), EngineError>;
        fn set_slide_size(&self, deck: DeckId, cx: f64, cy: f64) -> Result<(), EngineError>;
        fn size(&self, deck: DeckId) -> Result<DeckSize, EngineError>;
        fn slide_paths(&self, deck: DeckId) -> Result<Vec<String>, EngineError>;
        fn slide_refs(&self, deck: DeckId) -> Result<Vec<SlideRef>, EngineError>;
        fn slides_dirty(&self, deck: DeckId) -> Result<bool, EngineError>;
        fn default_font(&self, deck: DeckId) -> Result<Option<String>, EngineError>;
        fn adopt_embedded_fonts(&self, deck: DeckId, owner: &RenderOwner) -> Result<(), EngineError>;
        fn reset_font_metrics(&self) -> Result<(), EngineError>;
        fn render_all(&self, deck: DeckId, owner: &RenderOwner, fit_width_px: f64) -> Result<Vec<IpcValue>, EngineError>;
        fn render_slide(&self, deck: DeckId, owner: &RenderOwner, index: usize, fit_width_px: f64, reparse: bool) -> Result<Option<IpcValue>, EngineError>;
        fn run_txn(&self, deck: DeckId, owner: &RenderOwner, request: &TxnRequest) -> Result<TxnResult, EngineError>;
        fn snapshot(&self, deck: DeckId, meta_dirty: bool) -> Result<Snapshot, EngineError>;
        fn restore(&self, deck: DeckId, snapshot: &Snapshot) -> Result<bool, EngineError>;
        fn save_bytes(&self, deck: DeckId) -> Result<Vec<u8>, EngineError>;
        fn save_to_file(&self, deck: DeckId, path: &str) -> Result<(), EngineError>;
        fn commit_saved(&self, deck: DeckId) -> Result<(), EngineError>;
        fn commit_saved_owned(&self, deck: DeckId, origin: &RenderOwner, destination: &RenderOwner, participants: &[RenderOwner], cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken, additional: &[shell_contract::runtime::CancelToken], effects: &shell_contract::hosts::OwnedEffectsHost) -> Result<(), EngineError>;
        fn commit_saved_existing_document(&self, deck: DeckId, owner: &RenderOwner, document: &shell_contract::runtime::RendererGeneration, destination: &shell_contract::ipc::NativeSaveDestination, participants: &[RenderOwner], cancel: &shell_contract::runtime::CancelToken, pending: &shell_contract::runtime::CancelToken, additional: &[shell_contract::runtime::CancelToken], effects: &shell_contract::hosts::OwnedEffectsHost) -> Result<(), EngineError>;
        fn has_redaction(&self, deck: DeckId) -> Result<bool, EngineError>;
        fn encrypt_ooxml(&self, bytes: &[u8], password: &str) -> Result<Vec<u8>, EngineError>;
        fn is_encrypted_ooxml(&self, bytes: &[u8]) -> Result<bool, EngineError>;
        fn decrypt_office_file(&self, bytes: &[u8], password: &str) -> Result<Vec<u8>, EngineError>;
        fn picture_media(&self, deck: DeckId, slide: usize, source_id: &str) -> Result<Option<PictureMedia>, EngineError>;
        fn model3d_target(&self, deck: DeckId, slide: usize, source_id: &str) -> Result<Option<String>, EngineError>;
        fn read_part(&self, deck: DeckId, path: &str) -> Result<Option<Vec<u8>>, EngineError>;
        fn copy_slide(&self, deck: DeckId, index: usize) -> Result<Option<IpcValue>, EngineError>;
        fn copy_elements(&self, deck: DeckId, slide: usize, source_ids: &[String]) -> Result<Option<Vec<IpcValue>>, EngineError>;
        fn template_cover(&self, owner: &RenderOwner, id: &str, lang: &str, fit_width_px: f64) -> Result<Option<IpcValue>, EngineError>;
        fn font_file_families(&self, path: &str) -> Result<Vec<String>, EngineError>;
        fn parse_file_text(&self, path: &str) -> Result<String, String>;
        fn diagram_picture_ext_of(&self, path: &str) -> Result<Option<String>, EngineError>;
        fn sections(&self, deck: DeckId) -> Result<IpcValue, EngineError>;
        fn slide_index_of_id(&self, deck: DeckId, slide_id: &str) -> Result<Option<usize>, EngineError>;
        fn materialize_slide(&self, deck: DeckId, index: usize) -> Result<(), EngineError>;
        fn reparse_deck(&self, deck: DeckId) -> Result<(), EngineError>;
        fn autofit_resize(&self, deck: DeckId, owner: &RenderOwner, index: usize, source_id: &str, rendered: Option<IpcValue>, fit_width_px: f64) -> Result<Option<IpcValue>, EngineError>;
        fn sync_autofit_scale(&self, deck: DeckId, owner: &RenderOwner, index: usize, source_id: &str, rendered: Option<IpcValue>, fit_width_px: f64) -> Result<Option<IpcValue>, EngineError>;
        fn map_script_ops(&self, deck: DeckId, script: &IpcValue) -> Result<Vec<IpcValue>, EngineError>;
        fn group_child_frame(&self, deck: DeckId, slide: usize, group_id: &str, source_id: &str) -> Result<Option<GroupChildFrame>, EngineError>;
        fn element_index(&self, deck: DeckId, slide: usize, source_id: &str) -> Result<Option<usize>, EngineError>;
        fn element_id_at(&self, deck: DeckId, slide: usize, index: usize) -> Result<Option<String>, EngineError>;
        fn is_imported_chart(&self, deck: DeckId, slide: usize, source_id: &str) -> Result<bool, EngineError>;
        fn theme_accents(&self, deck: DeckId) -> Result<Option<Vec<String>>, EngineError>;
        fn chart_data(&self, deck: DeckId, slide: usize, source_id: &str) -> Result<IpcValue, EngineError>;
        fn layouts(&self, deck: DeckId) -> Result<IpcValue, EngineError>;
        fn ensure_builtin_layout(&self, deck: DeckId, key: &str) -> Result<Option<String>, EngineError>;
        fn slide_notes(&self, deck: DeckId, slide: usize) -> Result<String, EngineError>;
        fn slide_comments(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError>;
        fn element_link(&self, deck: DeckId, slide: &IpcValue, source_id: &IpcValue) -> Result<IpcValue, EngineError>;
        fn slide_links(&self, deck: DeckId, slide: &IpcValue) -> Result<IpcValue, EngineError>;
        fn run_links(&self, deck: DeckId, slide: &IpcValue) -> Result<IpcValue, EngineError>;
        fn header_footer(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError>;
        fn slide_transition(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError>;
        fn show_timing(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError>;
        fn animations(&self, deck: DeckId, slide: usize, labels: &[(String, String)]) -> Result<IpcValue, EngineError>;
        fn shape_keys(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError>;
        fn has_narration(&self, deck: DeckId, slide: usize) -> Result<bool, EngineError>;
        fn section_slide_indices(&self, deck: DeckId, id: &IpcValue) -> Result<Option<Vec<usize>>, EngineError>;
        fn master_parts(&self, deck: DeckId) -> Result<Vec<MasterPart>, EngineError>;
        fn master_parse(&self, deck: DeckId, owner: &RenderOwner, part_path: &str, fit_width_px: f64) -> Result<Option<IpcValue>, EngineError>;
        fn master_render(&self, deck: DeckId, owner: &RenderOwner, part_path: &str, fit_width_px: f64) -> Result<Option<IpcValue>, EngineError>;
        fn master_has_element(&self, deck: DeckId, part_path: &str, source_id: &str) -> Result<bool, EngineError>;
        fn master_preview_transform(&self, deck: DeckId, part_path: &str, source_id: &str, offset: [f64; 4], rot: f64) -> Result<(), EngineError>;
        fn image_info(&self, bytes: &[u8], ext: &str, path: Option<&str>) -> Result<ImageInfo, EngineError>;
        fn video_poster(&self, path: &str) -> Result<Option<Vec<u8>>, EngineError>;
        fn background_image(&self, deck: DeckId, slide: usize) -> Result<Option<String>, EngineError>;
        fn generate_page(&self, owner: &RenderOwner, spec_json: &str, default_fonts: &IpcValue) -> Result<GeneratedPage, String>;
        fn assemble_pages(&self, pages: &[Vec<u8>]) -> Result<Vec<u8>, EngineError>;
        fn extract_merge_source(&self, bytes: &[u8]) -> Result<Option<IpcValue>, EngineError>;
        fn private_font_faces(&self, owner: &RenderOwner) -> Result<IpcValue, EngineError>;
        fn private_font_data(&self, owner: &RenderOwner, id: &IpcValue) -> Result<IpcValue, EngineError>;
    }

    fn release(&self, deck: DeckId) {
        NativeSlidesEngine::release(self, deck)
    }
}
