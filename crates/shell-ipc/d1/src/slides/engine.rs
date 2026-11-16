//! What the slides handlers need that is not in Rust: the deck engine
//! (`@seuoffice/pptx-engine` open/save/reparse, `pptx-ops` `runTxn`,
//! `pptx-render` `buildRenderSlide`) behind [`SlidesEngine`], and the hooks
//! `slides-main.ts` gets from the shell (`runtime`, the opened hook, the
//! host window, the headless export map) behind [`SlidesShell`].
//!
//! Decks live in the engine and are named by a [`DeckId`], so an engine that
//! sits across a process boundary needs no shared object. A render slide, an
//! op, a bundle of a copied slide and every other structure only the engine
//! and the renderer read are [`IpcValue`]s: the handlers move them around as
//! the TypeScript does and look at the few keys the TypeScript looks at.

use std::any::Any;
use std::rc::Rc;
use std::sync::Arc;

use shell_contract::{IpcValue, ViewId, WindowId};

/// A deck the engine holds (`OpenedPptx`)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeckId(pub u64);

/// A recipient and the native view lifetime captured at the trusted boundary.
pub use shell_contract::ipc::NativeRequestOwner as RenderOwner;

/// A call of the engine that threw (`catch (err)` reads `String(err)`)
#[derive(Clone, Debug, PartialEq)]
pub struct EngineError(pub String);

impl EngineError {
    /// `String(err)` of an `Error` is `Error: message`
    pub fn text(&self) -> String {
        format!("Error: {}", self.0)
    }
}

/// `HistorySnapshot`: what the engine copied of a deck. Cloning shares the
/// copy; [`SlidesEngine::restore`] must not hand it to the live deck as is.
#[derive(Clone)]
pub struct Snapshot(pub Rc<dyn Any>);

impl main_core::slides::ai_proposal::SameSnapshot for Snapshot {
    fn same_snapshot(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// `opened.deck.size`
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeckSize {
    pub cx: f64,
    pub cy: f64,
}

/// One slide as `buildSlideUpdate` reads it
pub use main_core::slides::session_state::SlideRef;

/// `TxnRequest['isolation']`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    Atomic,
    PerOp,
}

/// `TxnRequest` of `@seuoffice/pptx-ops`
#[derive(Clone, Debug, PartialEq)]
pub struct TxnRequest {
    /// `Op` objects exactly as the TypeScript builds them
    pub ops: Vec<IpcValue>,
    pub isolation: Isolation,
    pub dry_run: bool,
    /// part paths whose live master slide seeds the executor (`parts`)
    pub parts: Vec<String>,
    pub withhold: bool,
}

impl TxnRequest {
    pub fn atomic(ops: Vec<IpcValue>) -> Self {
        TxnRequest {
            ops,
            isolation: Isolation::Atomic,
            dry_run: false,
            parts: Vec::new(),
            withhold: false,
        }
    }

    pub fn per_op(ops: Vec<IpcValue>) -> Self {
        TxnRequest {
            isolation: Isolation::PerOp,
            ..Self::atomic(ops)
        }
    }

    pub fn with_parts(mut self, parts: Vec<String>) -> Self {
        self.parts = parts;
        self
    }

    pub fn dry(&self) -> Self {
        TxnRequest {
            dry_run: true,
            ..self.clone()
        }
    }
}

/// `TxnResult['failures'][n]`
#[derive(Clone, Debug, PartialEq)]
pub struct TxnFailure {
    pub index: usize,
    pub op: IpcValue,
    pub error: String,
}

/// `TxnResult['records'][n]`: the applied op, the slide it landed on (the
/// executor-stamped durable id), the element ids it made and what it says
/// afterwards (`after`, `undefined` as `IpcValue::Undefined`)
#[derive(Clone, Debug, PartialEq)]
pub struct TxnRecord {
    pub op: IpcValue,
    pub slide_id: Option<String>,
    pub created: Option<Vec<IpcValue>>,
    pub before: IpcValue,
    pub after: IpcValue,
}

/// `TxnResult`; `failures` and `records` are `undefined` when empty
#[derive(Clone, Debug, PartialEq, Default)]
pub struct TxnResult {
    pub applied: bool,
    pub dry_run: bool,
    pub failures: Option<Vec<TxnFailure>>,
    pub records: Option<Vec<TxnRecord>>,
    /// the dry run's plan (`TxnResult.plan`), as the renderer receives it
    pub plan: Option<IpcValue>,
}

impl TxnResult {
    pub fn failure_count(&self) -> usize {
        self.failures.as_ref().map_or(0, Vec::len)
    }
}

/// A picture element's `media` with a target (`kind`, `target`, `external`)
#[derive(Clone, Debug, PartialEq)]
pub struct PictureMedia {
    pub kind: String,
    pub target: String,
    pub external: bool,
}

/// The deck engine. Every method is one call (or one small group of calls)
/// of the TypeScript engine; `deck` is a deck this engine opened.
pub trait SlidesEngine: Send + Sync {
    /// `createBlankPptx()`
    fn create_blank_pptx(&self) -> Result<Vec<u8>, EngineError>;
    /// `openPptx(bytes)`
    fn open(&self, bytes: &[u8]) -> Result<DeckId, EngineError>;
    /// The opened deck's model, not its rendered slide projection.
    fn deck_model(&self, deck: DeckId) -> Result<jscompat::JsonValue, EngineError>;
    /// the deck is no longer referenced (garbage collection in the TypeScript)
    fn release(&self, deck: DeckId);
    /// Transfer retained history media before the old deck is released.
    fn transfer_history(
        &self,
        old: DeckId,
        replacement: DeckId,
        views: &[RenderOwner],
    ) -> Result<(), EngineError>;
    /// Bind a captured document capability before exposing the session.
    fn bind_view(&self, deck: DeckId, owner: &RenderOwner) -> Result<(), EngineError>;
    /// Remove only this exact document's binding.
    fn unbind_view(&self, deck: DeckId, owner: &RenderOwner) -> Result<(), EngineError>;
    /// Atomically move one view from an old deck to a prepared replacement.
    fn replace_view(
        &self,
        old: DeckId,
        new: DeckId,
        owner: &RenderOwner,
    ) -> Result<(), EngineError>;
    /// `setSlideSize(opened, cx, cy)`
    fn set_slide_size(&self, deck: DeckId, cx: f64, cy: f64) -> Result<(), EngineError>;
    /// `opened.deck.size`
    fn size(&self, deck: DeckId) -> Result<DeckSize, EngineError>;
    /// `opened.deck.slides.map(s => s.path)`
    fn slide_paths(&self, deck: DeckId) -> Result<Vec<String>, EngineError>;
    /// the slides as `buildSlideUpdate` reads them
    fn slide_refs(&self, deck: DeckId) -> Result<Vec<SlideRef>, EngineError>;
    /// the dirty scan of `sessionDirty` over the slides (`structureDirty`,
    /// `dirty`, `dirtyTransform`)
    fn slides_dirty(&self, deck: DeckId) -> Result<bool, EngineError>;
    /// `deckDefaultFont(opened)`
    fn default_font(&self, deck: DeckId) -> Result<Option<String>, EngineError>;
    /// `adoptEmbeddedFonts(opened)`
    fn adopt_embedded_fonts(&self, deck: DeckId, owner: &RenderOwner) -> Result<(), EngineError>;
    /// `resetFontMetrics()`
    fn reset_font_metrics(&self) -> Result<(), EngineError>;
    /// `buildAllRenderSlides(opened, fitWidthPx)`
    fn render_all(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        fit_width_px: f64,
    ) -> Result<Vec<IpcValue>, EngineError>;
    /// `buildRenderSlide` of one page; `reparse` is `rebuildSlideWithReparse`
    fn render_slide(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        index: usize,
        fit_width_px: f64,
        reparse: bool,
    ) -> Result<Option<IpcValue>, EngineError>;
    /// `runTxn(opened, req)`
    fn run_txn(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        request: &TxnRequest,
    ) -> Result<TxnResult, EngineError>;
    /// `takeSnapshot` of the deck half (a deep copy of the slides and entries)
    fn snapshot(&self, deck: DeckId, meta_dirty: bool) -> Result<Snapshot, EngineError>;
    /// the deck half of `restoreSnapshot`: the live deck becomes a copy of
    /// the snapshot; answers the snapshot's `metaDirty`
    fn restore(&self, deck: DeckId, snapshot: &Snapshot) -> Result<bool, EngineError>;
    /// `savePptx(opened)`
    fn save_bytes(&self, deck: DeckId) -> Result<Vec<u8>, EngineError>;
    /// `savePptxToFile(opened, path)`: writes (truncating) the path
    fn save_to_file(&self, deck: DeckId, path: &str) -> Result<(), EngineError>;
    /// `commitSaved(opened)`
    fn commit_saved(&self, deck: DeckId) -> Result<(), EngineError>;
    fn commit_saved_owned(
        &self,
        deck: DeckId,
        origin: &RenderOwner,
        destination: &RenderOwner,
        participants: &[RenderOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<(), EngineError>;
    fn commit_saved_existing_document(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        document: &shell_contract::runtime::RendererGeneration,
        destination: &shell_contract::ipc::NativeSaveDestination,
        participants: &[RenderOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<(), EngineError>;
    /// `deckHasRedaction(opened)`
    fn has_redaction(&self, deck: DeckId) -> Result<bool, EngineError>;
    /// `encryptOoxml(bytes, password)`
    fn encrypt_ooxml(&self, bytes: &[u8], password: &str) -> Result<Vec<u8>, EngineError>;
    /// `isEncryptedOoxml(bytes)`
    fn is_encrypted_ooxml(&self, bytes: &[u8]) -> Result<bool, EngineError>;
    /// `decryptOfficeFile(bytes, password)`
    fn decrypt_office_file(&self, bytes: &[u8], password: &str) -> Result<Vec<u8>, EngineError>;
    /// the picture element `source_id` of slide `slide`, when it is one
    fn picture_media(
        &self,
        deck: DeckId,
        slide: usize,
        source_id: &str,
    ) -> Result<Option<PictureMedia>, EngineError>;
    /// the picture element's `model3d.target`
    fn model3d_target(
        &self,
        deck: DeckId,
        slide: usize,
        source_id: &str,
    ) -> Result<Option<String>, EngineError>;
    /// `opened.archive.readBytes(path)`
    fn read_part(&self, deck: DeckId, path: &str) -> Result<Option<Vec<u8>>, EngineError>;
    /// `copySlide(opened, index)`: a bundle another deck can paste
    fn copy_slide(&self, deck: DeckId, index: usize) -> Result<Option<IpcValue>, EngineError>;
    /// `copyElementData(opened, slide, el)` of every id found on the slide,
    /// in the order of `source_ids`; `None` when the slide does not exist
    fn copy_elements(
        &self,
        deck: DeckId,
        slide: usize,
        source_ids: &[String],
    ) -> Result<Option<Vec<IpcValue>>, EngineError>;
    /// the page of `renderTemplateCover(id, lang)`: `None` for an id the
    /// catalog does not list or a cover that could not be built
    fn template_cover(
        &self,
        owner: &RenderOwner,
        id: &str,
        lang: &str,
        fit_width_px: f64,
    ) -> Result<Option<IpcValue>, EngineError>;
    /// `fontFileFamilies(path)` of `fonts.ts`: the family names in a font
    /// file's name table (the store's install itself is main-core's)
    fn font_file_families(&self, path: &str) -> Result<Vec<String>, EngineError>;
    /// `parseFileToText(path)` of `@seuoffice/file-parse`: the text, or the
    /// message a failed parse gives
    fn parse_file_text(&self, path: &str) -> Result<String, String>;
    /// `diagramPictureExtOf(filePath)` of `file-parse`: `drawio.svg` or
    /// `drawio.png` when the file is named so and holds its diagram
    fn diagram_picture_ext_of(&self, path: &str) -> Result<Option<String>, EngineError>;
    /// `getSections(opened)`: the deck's sections as the renderer receives them
    fn sections(&self, deck: DeckId) -> Result<IpcValue, EngineError>;
    /// `slideDurableId(slide)` lookup: the page that carries the executor's
    /// durable slide id
    fn slide_index_of_id(&self, deck: DeckId, slide_id: &str)
    -> Result<Option<usize>, EngineError>;
    /// `materializeSlide(opened, index)`: bake pending patches and reparse
    /// the page
    fn materialize_slide(&self, deck: DeckId, index: usize) -> Result<(), EngineError>;
    /// `reparseDeck(opened)`: the whole deck reparsed in memory
    fn reparse_deck(&self, deck: DeckId) -> Result<(), EngineError>;
    /// `applyAutofitResize(session, slideIndex, elementId, rendered)` of
    /// `autofit-writeback.ts`: the render slide after the shape was fitted to its text
    fn autofit_resize(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        index: usize,
        source_id: &str,
        rendered: Option<IpcValue>,
        fit_width_px: f64,
    ) -> Result<Option<IpcValue>, EngineError>;
    /// `syncAutofitScale(session, slideIndex, elementId, rendered)`
    fn sync_autofit_scale(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        index: usize,
        source_id: &str,
        rendered: Option<IpcValue>,
        fit_width_px: f64,
    ) -> Result<Option<IpcValue>, EngineError>;
    /// `mapScriptOps(opened, script)` of `pptx-ops`
    fn map_script_ops(&self, deck: DeckId, script: &IpcValue)
    -> Result<Vec<IpcValue>, EngineError>;
    /// `resolveGroupChildId` and `findGroupChild`: the frame of a group's
    /// child; `None` when the group or the child is missing
    fn group_child_frame(
        &self,
        deck: DeckId,
        slide: usize,
        group_id: &str,
        source_id: &str,
    ) -> Result<Option<GroupChildFrame>, EngineError>;
    /// whether the slide has an element with this id (`matchesElementRef`)
    fn element_index(
        &self,
        deck: DeckId,
        slide: usize,
        source_id: &str,
    ) -> Result<Option<usize>, EngineError>;
    /// the id of the element at `index` of the slide after a reparse
    fn element_id_at(
        &self,
        deck: DeckId,
        slide: usize,
        index: usize,
    ) -> Result<Option<String>, EngineError>;
    /// whether the element is a chart that did not come from this app
    /// (`type === 'chart' && descr !== 'aislides-chart'`)
    fn is_imported_chart(
        &self,
        deck: DeckId,
        slide: usize,
        source_id: &str,
    ) -> Result<bool, EngineError>;
    /// `chartColorSchemes`'s theme input: accent1..6 of the first slide's
    /// theme when it has at least three, else `None`
    fn theme_accents(&self, deck: DeckId) -> Result<Option<Vec<String>>, EngineError>;
    /// `getChartElementData(slide, sourceId)`
    fn chart_data(
        &self,
        deck: DeckId,
        slide: usize,
        source_id: &str,
    ) -> Result<IpcValue, EngineError>;
    /// `listSlideLayouts` plus the built-in set when the deck needs it
    fn layouts(&self, deck: DeckId) -> Result<IpcValue, EngineError>;
    /// `ensureBuiltinLayout(archive, size, key)`: the path of the layout
    /// part for a `builtin:<key>`; `None` when the key is unknown
    fn ensure_builtin_layout(&self, deck: DeckId, key: &str)
    -> Result<Option<String>, EngineError>;
    /// `getSlideNotes(archive, slidePath)` of an existing slide
    fn slide_notes(&self, deck: DeckId, slide: usize) -> Result<String, EngineError>;
    /// `getSlideComments(archive, slidePath)` of an existing slide
    fn slide_comments(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError>;
    /// `getElementLink(opened, slideIndex, sourceId)`; the arguments are the
    /// renderer's, whatever they are
    fn element_link(
        &self,
        deck: DeckId,
        slide: &IpcValue,
        source_id: &IpcValue,
    ) -> Result<IpcValue, EngineError>;
    /// `getSlideLinks(opened, slideIndex)`: `{ elementId, target }` items
    fn slide_links(&self, deck: DeckId, slide: &IpcValue) -> Result<IpcValue, EngineError>;
    /// `getRunLinks(opened, slideIndex)`: `{ elementId, ...rest }` items
    fn run_links(&self, deck: DeckId, slide: &IpcValue) -> Result<IpcValue, EngineError>;
    /// `readHeaderFooter(slide)` of an existing slide
    fn header_footer(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError>;
    /// `getSlideTransition(slide)` of an existing slide
    fn slide_transition(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError>;
    /// `{ advanceMs: getSlideAdvanceTime(slide), narrationIds }` of an existing slide
    fn show_timing(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError>;
    /// the `AnimationItem[]` of an existing slide; `labels` are the localized
    /// names by element type (`text`, `shape`, `picture`, `group`, `table`,
    /// `chart`, `passthrough`, `object` for the fallback)
    fn animations(
        &self,
        deck: DeckId,
        slide: usize,
        labels: &[(String, String)],
    ) -> Result<IpcValue, EngineError>;
    /// the Morph pairing keys of an existing slide's elements
    fn shape_keys(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError>;
    /// `narrationSpids(slide).length > 0`
    fn has_narration(&self, deck: DeckId, slide: usize) -> Result<bool, EngineError>;
    /// the slides of a section, `normalizeSections` applied: `id` null or
    /// undefined is the lead (sectionless) slides; `None` for an unknown id
    fn section_slide_indices(
        &self,
        deck: DeckId,
        id: &IpcValue,
    ) -> Result<Option<Vec<usize>>, EngineError>;
    /// `listMasterParts(archive)`
    fn master_parts(&self, deck: DeckId) -> Result<Vec<MasterPart>, EngineError>;
    /// `parseMasterPart(archive, partPath)` then `buildRenderSlide`: the part
    /// becomes the live master slide the master ops are seeded with; `None`
    /// when it does not parse
    fn master_parse(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        part_path: &str,
        fit_width_px: f64,
    ) -> Result<Option<IpcValue>, EngineError>;
    /// `buildRenderSlide` of the live master slide of `part_path`
    fn master_render(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        part_path: &str,
        fit_width_px: f64,
    ) -> Result<Option<IpcValue>, EngineError>;
    /// the live master slide has an element with this id
    fn master_has_element(
        &self,
        deck: DeckId,
        part_path: &str,
        source_id: &str,
    ) -> Result<bool, EngineError>;
    /// a transform preview: the live master slide's element moves, nothing
    /// is persisted (`el.transform = { ...el.transform, offset, rot }`)
    fn master_preview_transform(
        &self,
        deck: DeckId,
        part_path: &str,
        source_id: &str,
        offset: [f64; 4],
        rot: f64,
    ) -> Result<(), EngineError>;
    /// the picture's natural size and dpi: `nativeImage` (`tiffToPng` for a
    /// tiff) and `imageDpiFromBytes(bytes)`; `path` when the image is a file
    fn image_info(
        &self,
        bytes: &[u8],
        ext: &str,
        path: Option<&str>,
    ) -> Result<ImageInfo, EngineError>;
    /// `nativeImage.createThumbnailFromPath(path)` as PNG: a video's poster
    fn video_poster(&self, path: &str) -> Result<Option<Vec<u8>>, EngineError>;
    /// `slide.background.mediaRef` when the slide's background is an image
    fn background_image(&self, deck: DeckId, slide: usize) -> Result<Option<String>, EngineError>;
    /// `buildPagePptx` of a page spec the renderer's LLM wrote (`parsePageSpec`
    /// first): the one-slide pptx and the images that failed to load. Runs on
    /// a worker (it fetches images); `Err` is the message the handler
    /// answers as `error`.
    fn generate_page(
        &self,
        owner: &RenderOwner,
        spec_json: &str,
        default_fonts: &IpcValue,
    ) -> Result<GeneratedPage, String>;
    /// the replace mode of `slides:land-generated-pages`: the pages (one
    /// pptx each) merged into one deck with the backgrounds promoted and the
    /// text boxes autofitted, as pptx bytes
    fn assemble_pages(&self, pages: &[Vec<u8>]) -> Result<Vec<u8>, EngineError>;
    /// `extractMergeSlideSource(bytes)`: the slide another deck can merge
    fn extract_merge_source(&self, bytes: &[u8]) -> Result<Option<IpcValue>, EngineError>;
    /// `listPrivateFontFaces()` of `fonts.ts`, as the renderer receives it
    fn private_font_faces(&self, owner: &RenderOwner) -> Result<IpcValue, EngineError>;
    /// `getPrivateFontData(id)` of `fonts.ts`, as the renderer receives it
    fn private_font_data(
        &self,
        owner: &RenderOwner,
        id: &IpcValue,
    ) -> Result<IpcValue, EngineError>;
}

pub type SlidesEngineHost = Arc<dyn SlidesEngine>;

/// `listMasterParts` item: `{ partPath, kind, name }`
#[derive(Clone, Debug, PartialEq)]
pub struct MasterPart {
    pub part_path: String,
    pub kind: IpcValue,
    pub name: IpcValue,
}

/// What the shell knows of a picture's pixels
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ImageInfo {
    /// width and height in px
    pub natural: Option<(f64, f64)>,
    /// horizontal and vertical dpi the file carries
    pub dpi: Option<(f64, f64)>,
}

/// `findGroupChild`'s frame of a child inside a group: the group's offset
/// and, when it has one, the child offset space (`chOff`/`chExt`)
#[derive(Clone, Debug, PartialEq)]
pub struct GroupChildFrame {
    pub child_id: String,
    /// `grp.transform.offset`: x, y, cx, cy
    pub group_offset: [f64; 4],
    /// `grp.childOffset`: x, y, cx, cy
    pub child_offset: Option<[f64; 4]>,
}

/// What `buildPagePptx` answers
#[derive(Clone, Debug, PartialEq)]
pub struct GeneratedPage {
    pub bytes: Vec<u8>,
    pub image_failures: Vec<String>,
}

/// What a tab takes of the generate hand-off
#[derive(Clone, Debug, PartialEq)]
pub struct TakenHandoff {
    pub id: String,
    pub text: String,
    /// `(path, name)` of each material
    pub materials: Vec<(String, String)>,
}

/// What preparing a path for open came to (`preparePresentationForOpen` and
/// `preparePptxOpen`, which ask the user about conversions and passwords)
#[derive(Clone, Debug, PartialEq)]
pub enum PreparedOpen {
    /// nothing to open (cancelled, declined, damaged, told why)
    Rejected,
    /// open the deck at this path
    Plain(String),
    /// an encrypted deck the user unlocked: its plain package and password
    Unlocked {
        path: String,
        bytes: Vec<u8>,
        password: String,
    },
}

/// A PDF export the shell runs in a hidden window (`exportSlidesPdf`)
#[derive(Clone, Debug, PartialEq)]
pub struct PdfExportAnswer {
    pub ok: bool,
    pub path: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct HeadlessExportTarget {
    pub path: String,
    pub invocation: shell_contract::ipc::NativeInvocationDocument,
}

/// One display of the machine (`screen.getAllDisplays()`)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Display {
    pub id: i64,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PresenterWindowAction {
    ShowFullscreen {
        window: WindowId,
        on: bool,
    },
    Refocus {
        window: WindowId,
    },
    SwapDisplays {
        presenter: WindowId,
        audience: WindowId,
        presenter_display: Display,
        audience_display: Display,
    },
}

/// The shell side of `slides-main.ts` and `presenter-show.ts`
pub trait SlidesShell: Send + Sync {
    /// `slidesOpenedHook(wc, path)`
    fn opened(&self, view: ViewId, path: &str);
    fn file_saved_owned(
        &self,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
        path: std::path::PathBuf,
        participants: &[RenderOwner],
        additional: &[shell_contract::runtime::CancelToken],
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
    fn set_window_title_existing_document(
        &self,
        destination: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        title: String,
        done: Box<dyn FnOnce(Result<(), String>)>,
    );
    fn set_window_title_owned(
        &self,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        title: String,
        effects: &shell_contract::hosts::OwnedEffectsHost,
        participants: &[RenderOwner],
        additional: &[shell_contract::runtime::CancelToken],
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    fn presenter_window_owned(
        &self,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        action: PresenterWindowAction,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    fn prepare_reveal_export(
        &self,
        output: shell_contract::ipc::SavedPublication,
        written: Option<shell_contract::ipc::NativeSavedInput>,
        participants: Vec<RenderOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        accepted: shell_contract::hosts::PdfReloadAccepted,
        wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String>;
    fn reload_view(&self, view: ViewId);
    /// `dialogParent()`: the focused window, else the shell window
    fn dialog_parent(&self) -> Option<WindowId>;
    /// `hostWindowFor(wc)`: the window a tab view lives in
    fn host_window_for(&self, view: ViewId) -> Option<WindowId>;
    /// `isHeadlessMode()`
    fn is_headless(&self) -> bool;
    /// `runtime.newDocumentDir`: the project folder offered for a new deck
    fn new_document_dir(&self, view: ViewId) -> Option<String>;
    /// `runtime.writeNewDocument` is set
    fn can_write_new_document(&self) -> bool;
    /// `runtime.writeNewDocument(view, name, bytes)`: the path written, or
    /// `None` when no project was chosen
    fn write_new_document_owned(
        &self,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        name: &str,
        bytes: &[u8],
    ) -> Result<Option<String>, String>;
    /// `isPathOpenElsewhere(path)`: the shell's probe over the other panes
    fn is_path_open_elsewhere(&self, path: &str) -> Result<bool, EngineError>;
    /// `headlessExportTargets.get(id)` then delete: the PDF path the hidden
    /// export window must write
    fn take_headless_target_owned(
        &self,
        owner: &RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Option<HeadlessExportTarget>, String>;
    /// `headlessExportWaiters` settle: `slides:headless-export-done`
    fn headless_export_done_owned(
        &self,
        owner: &RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        ok: bool,
        error: Option<String>,
    ) -> Result<(), String>;
    /// `clipboard.readBuffer(format)`
    fn read_clipboard_buffer(&self, format: &str) -> Result<Vec<u8>, EngineError>;
    /// `clipboard.writeBuffer(format, bytes)`
    fn write_clipboard_buffer(&self, format: &str, bytes: &[u8]);
    /// `nativeImage.createFromBuffer(png).toPNG()`; `None` for an empty image
    fn normalize_png(&self, bytes: &[u8]) -> Option<Vec<u8>>;
    /// `clipboard.write({ image, html })`
    fn write_image_and_html(&self, png: &[u8], html: &str);
    /// `webContents.cut()`, `copy()` or `paste()` of the sender
    fn native_clipboard(&self, view: ViewId, op: &str);
    /// the prepare step of `slides:open` and friends, answered through
    /// `done` (from any thread) so a password prompt never blocks the core
    fn prepare_for_open_owned(
        &self,
        parent: Option<WindowId>,
        path: &str,
        source: RenderOwner,
        destination: RenderOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<PreparedOpen, String>) + Send>,
    );
    fn prepare_for_recovery_open(
        &self,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        path: String,
        done: Box<dyn FnOnce(Result<PreparedOpen, String>) + Send>,
    );
    /// Prompt for and unlock the selected encrypted copy before navigation.
    fn unlock_recovery_copy_prepared(
        &self,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        title: String,
        bytes: Vec<u8>,
        done: Box<dyn FnOnce(Result<Option<(Vec<u8>, String)>, String>) + Send>,
    );
    fn unlock_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        title: String,
        done: Box<dyn FnOnce(Result<Option<(Vec<u8>, String)>, String>) + Send>,
    );
    fn export_pdf_owned(
        &self,
        op: &IpcValue,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        participants: &[RenderOwner],
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> PdfExportAnswer;
    fn export_pdf_native_invocation(
        &self,
        op: &IpcValue,
        document: &shell_contract::ipc::NativeInvocationDocument,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        participants: &[RenderOwner],
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> PdfExportAnswer;
    fn export_pdf_existing_document(
        &self,
        op: &IpcValue,
        destination: &shell_contract::ipc::NativeSaveDestination,
        participants: &[RenderOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> PdfExportAnswer;
    /// the microphone permission question (`systemPreferences`), answered
    /// through `done`
    fn ask_microphone(&self, done: Box<dyn FnOnce(Result<bool, String>) + Send>);
    /// `screen.getAllDisplays()`
    fn displays(&self) -> Result<Vec<Display>, EngineError>;
    /// the display the window is mostly on (`getDisplayMatching`), or the primary
    fn display_of(&self, window: Option<WindowId>) -> Result<Display, EngineError>;
    /// Creates the audience window and captures its native owner at creation.
    /// The shell calls [`crate::slides::audience_closed`] when it closes.
    fn open_audience_window(
        &self,
        display: Display,
        source: RenderOwner,
        cancel: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<(RenderOwner, WindowId)>, EngineError>) + Send>,
    );
    /// closes the audience window (leaving simple fullscreen first)
    fn close_audience_window(&self, window: WindowId);
    /// the window title of a detached editor window (`win.setTitle`)
    fn set_window_title(&self, view: ViewId, title: &str);
    /// `takeGenerateHandoff('slides', view)`
    fn take_generate_handoff_owned(
        &self,
        consumer: &RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Option<TakenHandoff>, String>;
    /// `reportGenerateRunStarted('slides', view, paths)`: recorded
    fn report_generate_run_started_owned(
        &self,
        consumer: &RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        paths: &[String],
    ) -> Result<bool, String>;
    /// `assertFileSaved(path)`: `Err` carries the message of the throw (the
    /// file is open with unsaved changes)
    fn assert_file_saved(&self, path: String, then: Box<dyn FnOnce(Result<(), String>) + Send>);
    /// `randomUUID()` of `node:crypto`: names of the clipboard token, the generated page
    /// files and the untitled recovery copies (production: `jscompat::random_uuid`)
    fn random_uuid(&self) -> String;
}

pub type SlidesShellHost = Arc<dyn SlidesShell>;
