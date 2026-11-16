use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Weak, mpsc};
use std::thread::{self, ThreadId};

use pptx_engine::deck::{OpenedPptx, commit_saved, open_pptx, reparse_deck, save_pptx};
use pptx_engine::types::{Slide, SlideDeck};
use shell_ipc_d1::slides::engine::{
    DeckId, DeckSize, EngineError, MasterPart, RenderOwner, Snapshot, TxnRequest, TxnResult,
};

use crate::host::pptx_ops;

mod autofit;
mod image_dpi;
mod script_map;
mod trait_impl;

type Job = Box<dyn FnOnce(&mut ActorState) + Send>;

#[derive(Clone)]
pub(crate) struct SlidesRequestAuthority {
    pub(crate) authorization: crate::views::SlidesMediaAuthorization,
    pub(crate) token: crate::native::DocumentContext,
}

pub(crate) type FontRendererFactory = Arc<
    dyn Fn(&SlidesRequestAuthority) -> Result<Box<dyn super::slides_fonts::FontRenderer>, String>
        + Send
        + Sync,
>;

pub(crate) fn native_font_renderer_factory() -> FontRendererFactory {
    Arc::new(|authority| {
        let authority = authority.clone();
        if !authority.authorization.is_current() || !authority.token.is_live() {
            return Err("The slide font document was released".into());
        }
        Ok(Box::new(super::slides_fonts::BoundRenderer::new(
            move |source: &str| {
                if !authority.authorization.is_current() || !authority.token.is_live() {
                    return Err("The slide font document was released".into());
                }
                let (send, receive) = mpsc::sync_channel(1);
                let evaluation = authority
                    .token
                    .evaluate(source.to_owned(), move |answer| {
                        let _ = send.send(answer);
                    })
                    .map_err(|error| error.to_string())?;
                let answer = receive
                    .recv_timeout(std::time::Duration::from_secs(120))
                    .map_err(|error| {
                        format!("The slide font evaluation did not complete: {error}")
                    })?
                    .map_err(|error| error.to_string());
                drop(evaluation);
                if !authority.authorization.is_current() || !authority.token.is_live() {
                    return Err("The slide font document was released".into());
                }
                answer
            },
        )) as Box<dyn super::slides_fonts::FontRenderer>)
    })
}

// Media grants are process-wide, so decks from different actors need distinct ids.
static NEXT_DECK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_generation(current: u64) -> Result<u64, EngineError> {
    if current >= 9_007_199_254_740_991 {
        return Err(EngineError(
            "slides generation identifiers exhausted".into(),
        ));
    }
    Ok(current + 1)
}

fn host_number(value: f64) -> serde_json::Value {
    if value.is_finite() {
        serde_json::json!(value)
    } else {
        serde_json::json!({ "$num": jscompat::number_to_string(value) })
    }
}

fn read_host_number(value: &serde_json::Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    match value
        .as_object()
        .filter(|fields| fields.len() == 1)?
        .get("$num")?
        .as_str()?
    {
        "NaN" => Some(f64::NAN),
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        _ => None,
    }
}

pub(crate) fn owner_authority(
    owner: &RenderOwner,
) -> Result<Arc<SlidesRequestAuthority>, EngineError> {
    let authority = owner
        .context
        .as_arc()
        .downcast::<SlidesRequestAuthority>()
        .map_err(|_| EngineError("invalid slide document authorization".into()))?;
    let document = authority.token.browser().document();
    if authority.authorization.view != owner.view
        || !authority.authorization.is_current()
        || !authority.token.is_live()
        || !owner
            .context
            .renderer()
            .is_some_and(|renderer| renderer.is_active())
        || !authority.authorization.matches_native_snapshot(
            Some(authority.token.identity().browser_id()),
            authority.token.browser(),
            document.as_ref(),
        )
    {
        return Err(EngineError(
            "slide document authorization is no longer current".into(),
        ));
    }
    Ok(authority)
}

fn owner_authorization(
    owner: &RenderOwner,
) -> Result<Arc<crate::views::SlidesMediaAuthorization>, EngineError> {
    Ok(Arc::new(owner_authority(owner)?.authorization.clone()))
}

fn captured_lease(owner: &RenderOwner) -> Result<Arc<crate::views::SlidesViewLease>, EngineError> {
    if let Some(authority) = owner.context.downcast_ref::<SlidesRequestAuthority>() {
        if authority.authorization.view != owner.view {
            return Err(EngineError(
                "slide document view does not match owner".into(),
            ));
        }
        return Ok(Arc::new(authority.authorization.lease()));
    }
    #[cfg(test)]
    if let Some(lease) = owner
        .context
        .downcast_ref::<crate::views::SlidesViewLease>()
    {
        return Ok(Arc::new(lease.clone()));
    }
    Err(EngineError("invalid slide document authorization".into()))
}

fn owner_lease(owner: &RenderOwner) -> Result<Arc<crate::views::SlidesViewLease>, EngineError> {
    #[cfg(not(test))]
    let lease = Arc::new(owner_authorization(owner)?.lease());
    #[cfg(test)]
    let lease = if owner
        .context
        .downcast_ref::<crate::views::SlidesViewLease>()
        .is_some()
    {
        captured_lease(owner)?
    } else {
        Arc::new(owner_authorization(owner)?.lease())
    };
    if !lease.is_active() {
        return Err(EngineError("slide view lease is no longer active".into()));
    }
    Ok(lease)
}

fn same_owner(a: &RenderOwner, b: &RenderOwner) -> bool {
    if a.view != b.view {
        return false;
    }
    #[cfg(test)]
    if let (Some(a_lease), Some(b_lease)) = (
        a.context.downcast_ref::<crate::views::SlidesViewLease>(),
        b.context.downcast_ref::<crate::views::SlidesViewLease>(),
    ) {
        return a_lease.same_lifetime(b_lease)
            && match (a.context.renderer(), b.context.renderer()) {
                (Some(a), Some(b)) => a.same_generation(b),
                (None, None) => true,
                _ => false,
            };
    }
    let (Some(a_token), Some(b_token)) = (
        a.context.downcast_ref::<SlidesRequestAuthority>(),
        b.context.downcast_ref::<SlidesRequestAuthority>(),
    ) else {
        return false;
    };
    a_token.token == b_token.token
        && a_token
            .authorization
            .same_document_identity(&b_token.authorization)
        && matches!((a.context.renderer(), b.context.renderer()), (Some(a), Some(b)) if a.same_generation(b))
}

struct BoundView {
    owner: RenderOwner,
    lease: crate::views::SlidesViewLease,
}

fn deck_sections(
    opened: &OpenedPptx,
) -> Result<Vec<pptx_engine::sections::SectionInfo>, EngineError> {
    let mut random_error = None;
    let mut uuid = || {
        let mut bytes = [0u8; 16];
        if let Err(error) = getrandom::fill(&mut bytes) {
            random_error = Some(error.to_string());
            return String::new();
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
            bytes[9],
            bytes[10],
            bytes[11],
            bytes[12],
            bytes[13],
            bytes[14],
            bytes[15]
        )
    };
    let sections = pptx_engine::sections::get_sections(opened, &mut uuid).map_err(EngineError)?;
    if let Some(error) = random_error {
        return Err(EngineError(error));
    }
    Ok(sections)
}

struct DeckState {
    opened: OpenedPptx,
    masters: HashMap<String, Slide>,
    generation: u64,
    views: HashMap<shell_contract::ViewId, BoundView>,
}

struct DeckSnapshot {
    deck: SlideDeck,
    entries: jscompat::JsMap<Vec<u8>>,
    original_hash: String,
    save_snapshot: Option<jscompat::JsMap<String>>,
    masters: HashMap<String, Slide>,
    meta_dirty: bool,
    owner: DeckId,
    generation: u64,
    next_uid: u64,
}

struct Cover {
    view: shell_contract::ViewId,
    lease: crate::views::SlidesViewLease,
    owner: RenderOwner,
    id: String,
    lang: String,
    deck: DeckId,
}

#[derive(Default)]
struct ActorState {
    decks: HashMap<DeckId, DeckState>,
    snapshots: HashMap<u64, DeckSnapshot>,
    covers: Vec<Cover>,
    next_snapshot: u64,
    next_generation: u64,
}

impl Drop for ActorState {
    fn drop(&mut self) {
        for id in self.decks.keys().copied() {
            super::slides_media::release(id);
            super::slides_fonts::release(id);
        }
    }
}

impl ActorState {
    fn deck(&mut self, id: DeckId) -> Result<&mut DeckState, EngineError> {
        self.decks
            .get_mut(&id)
            .ok_or_else(|| EngineError(format!("unknown slides deck {}", id.0)))
    }

    fn authorize(&mut self, id: DeckId, owner: &RenderOwner) -> Result<(), EngineError> {
        owner_lease(owner)?;
        if !self
            .deck(id)?
            .views
            .get(&owner.view)
            .is_some_and(|bound| same_owner(&bound.owner, owner))
        {
            return Err(EngineError("slide view is not bound to this deck".into()));
        }
        Ok(())
    }

    fn write_deck(&mut self, id: DeckId) -> Result<&mut DeckState, EngineError> {
        if !self.decks.contains_key(&id) {
            return Err(EngineError(format!("unknown slides deck {}", id.0)));
        }
        let generation = next_generation(self.next_generation)?;
        self.next_generation = generation;
        let deck = self
            .decks
            .get_mut(&id)
            .ok_or_else(|| EngineError(format!("unknown slides deck {}", id.0)))?;
        deck.generation = generation;
        Ok(deck)
    }
}

struct Actor {
    sender: mpsc::Sender<Job>,
    thread: ThreadId,
}

struct SnapshotHandle {
    actor: Weak<Actor>,
    id: u64,
}

impl Drop for SnapshotHandle {
    fn drop(&mut self) {
        if let Some(actor) = self.actor.upgrade() {
            let id = self.id;
            let _ = actor.sender.send(Box::new(move |state| {
                if let Some(snapshot) = state.snapshots.remove(&id) {
                    let mut retained: Vec<_> = state
                        .snapshots
                        .values()
                        .filter(|other| other.owner == snapshot.owner)
                        .map(|other| other.generation)
                        .collect();
                    if let Some(deck) = state.decks.get(&snapshot.owner) {
                        retained.push(deck.generation);
                    }
                    super::slides_media::prune(snapshot.owner, &retained);
                    super::slides_fonts::prune(snapshot.owner, &retained);
                }
            }));
        }
    }
}

pub(crate) struct NativeSlidesEngine {
    actor: Arc<Actor>,
    ui: Arc<dyn shell_contract::runtime::Thread>,
    font_env: super::slides_fonts::FontEnvironment,
    font_renderer: FontRendererFactory,
}

impl NativeSlidesEngine {
    pub(crate) fn new(
        ui: Arc<dyn shell_contract::runtime::Thread>,
        font_env: super::slides_fonts::FontEnvironment,
        font_renderer: FontRendererFactory,
    ) -> Result<Self, EngineError> {
        let (sender, receiver) = mpsc::channel::<Job>();
        let worker = thread::Builder::new()
            .name("slides-engine".into())
            .spawn(move || {
                let mut state = ActorState::default();
                while let Ok(job) = receiver.recv() {
                    job(&mut state);
                }
            })
            .map_err(|error| EngineError(format!("could not start slides engine: {error}")))?;
        Ok(Self {
            actor: Arc::new(Actor {
                sender,
                thread: worker.thread().id(),
            }),
            ui,
            font_env,
            font_renderer,
        })
    }

    pub(crate) fn create_blank_pptx(&self) -> Result<Vec<u8>, EngineError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| EngineError(error.to_string()))?
            .as_secs_f64()
            * 1000.0;
        pptx_engine::blank::create_blank_pptx(now).map_err(EngineError)
    }

    pub(crate) fn is_encrypted_ooxml(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        Ok(main_core::packages::office_encryption::is_encrypted_ooxml(
            bytes,
        ))
    }

    pub(crate) fn encrypt_ooxml(
        &self,
        bytes: &[u8],
        password: &str,
    ) -> Result<Vec<u8>, EngineError> {
        main_core::packages::office_encryption::encrypt_ooxml(bytes, password).map_err(EngineError)
    }

    pub(crate) fn decrypt_office_file(
        &self,
        bytes: &[u8],
        password: &str,
    ) -> Result<Vec<u8>, EngineError> {
        main_core::packages::office_encryption::decrypt_office_file(bytes, password)
            .map_err(|error| EngineError(error.message))
    }

    pub(crate) fn parse_file_text(&self, path: &str) -> Result<String, String> {
        let parsed = crate::host::w_files::parse_file_to_text(path)?;
        if parsed.ok {
            parsed
                .text
                .ok_or_else(|| "file parser returned no text".to_owned())
        } else {
            Err(parsed
                .error
                .unwrap_or_else(|| "file parsing failed".to_owned()))
        }
    }

    pub(crate) fn font_file_families(&self, path: &str) -> Result<Vec<String>, EngineError> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(super::slides_fonts::font_file_families(&bytes)),
            Err(_) => Ok(Vec::new()),
        }
    }

    pub(crate) fn reset_font_metrics(&self) -> Result<(), EngineError> {
        super::slides_fonts::reset_font_metrics().map_err(EngineError)
    }

    pub(crate) fn private_font_faces(
        &self,
        owner: &RenderOwner,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        let authorization = owner_authorization(owner)?;
        let faces = super::slides_fonts::private_faces(&authorization).map_err(EngineError)?;
        Ok(shell_ipc_d1::value::from_json(
            &jscompat::json_value_from_serde(&serde_json::Value::Array(faces)),
        ))
    }

    pub(crate) fn private_font_data(
        &self,
        owner: &RenderOwner,
        id: &shell_contract::IpcValue,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        let authorization = owner_authorization(owner)?;
        let shell_contract::IpcValue::String(id) = id else {
            return Ok(shell_contract::IpcValue::Null);
        };
        let bytes =
            super::slides_fonts::private_font_data(&authorization, id).map_err(EngineError)?;
        Ok(match bytes {
            Some(bytes) => {
                shell_contract::IpcValue::Bytes(shell_contract::BytesKind::ArrayBuffer, bytes)
            }
            None => shell_contract::IpcValue::Null,
        })
    }

    fn prepare_font_adoption(
        &self,
        deck: DeckId,
        owner: RenderOwner,
    ) -> Result<
        (
            u64,
            Vec<pptx_engine::embedded_fonts::EmbeddedFontFace>,
            Vec<main_core::slides::metafile_fonts::MetafileFontRequest>,
        ),
        EngineError,
    > {
        self.call(move |state| {
            let authorization = owner_authorization(&owner)?;
            state.authorize(deck, &owner)?;
            let current = state.deck(deck)?;
            let embedded =
                pptx_engine::embedded_fonts::list_embedded_fonts(&current.opened.archive)
                    .map_err(EngineError)?;
            let entries: Vec<_> = current
                .opened
                .archive
                .entries()
                .iter()
                .map(|(path, bytes)| (path.to_owned(), bytes.clone()))
                .collect();
            let metafile = main_core::slides::metafile_fonts::list_metafile_fonts(&entries);
            if !authorization.is_current() {
                return Err(EngineError("slide document is no longer current".into()));
            }
            let generation = current.generation;
            let first_slide = current
                .opened
                .deck
                .slides
                .first()
                .and_then(|slide| slide.path.as_ref())
                .cloned();
            super::slides_media::install(
                deck,
                generation,
                current
                    .views
                    .iter()
                    .map(|(view, bound)| (*view, bound.lease.clone())),
                entries,
                first_slide,
            )
            .map_err(EngineError)?;
            Ok((generation, embedded, metafile))
        })
    }

    pub(crate) fn adopt_embedded_fonts(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
    ) -> Result<(), EngineError> {
        let authority = owner_authority(owner)?;
        let authorization = Arc::new(authority.authorization.clone());
        let (generation, embedded, metafile) = self.prepare_font_adoption(deck, owner.clone())?;
        let renderer = (self.font_renderer)(&authority).map_err(EngineError)?;
        let id = crate::engine_host::allocate_request_id().map_err(EngineError)?;
        let env = self.font_env.clone();
        let cancel = shell_contract::runtime::CancelToken::default();
        super::slides_fonts::adopt_document_fonts(
            id,
            deck,
            generation,
            (*authorization).clone(),
            embedded,
            &metafile,
            env,
            renderer,
            &cancel,
        )
        .map_err(EngineError)?;
        let owner = owner.clone();
        self.call(move |state| {
            if !authorization.is_current() {
                return Err(EngineError("slide document is no longer current".into()));
            }
            state.authorize(deck, &owner)?;
            if state.deck(deck)?.generation != generation {
                return Err(EngineError("slide changed during font adoption".into()));
            }
            Ok(())
        })
    }

    fn call_slides_service(
        &self,
        name: &str,
        input: &serde_json::Value,
        owner: &RenderOwner,
    ) -> Result<serde_json::Value, EngineError> {
        let authority = owner_authority(owner)?;
        let renderer = (self.font_renderer)(&authority).map_err(EngineError)?;
        crate::engine_host::call_with_slides_owner(
            name,
            input,
            owner.clone(),
            self.font_env.clone(),
            renderer,
        )
        .map_err(|error| EngineError(format!("{}: {}", error.name, error.message)))
    }

    pub(crate) fn diagram_picture_ext_of(&self, path: &str) -> Result<Option<String>, EngineError> {
        let path = std::path::Path::new(path);
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(None);
        };
        let Some(ext) = main_core::packages::file_parse::diagram::diagram_picture_ext_of_name(name)
        else {
            return Ok(None);
        };
        let Ok(metadata) = std::fs::metadata(path) else {
            return Ok(Some(ext.to_owned()));
        };
        if metadata.len() > 50 * 1024 * 1024 {
            return Ok(Some(ext.to_owned()));
        }
        let Ok(bytes) = std::fs::read(path) else {
            return Ok(Some(ext.to_owned()));
        };
        let result = seu_host::call(
            "fileparse.readDiagramText",
            &serde_json::json!({
                "fileName": name, "bytesBase64": jscompat::base64(&bytes),
            }),
        )
        .map_err(|error| EngineError(format!("{}: {}", error.name, error.message)))?;
        if result.get("reason").and_then(serde_json::Value::as_str) == Some("no_original") {
            Ok(None)
        } else {
            Ok(Some(ext.to_owned()))
        }
    }

    pub(crate) fn image_info(
        &self,
        bytes: &[u8],
        ext: &str,
        path: Option<&str>,
    ) -> Result<shell_ipc_d1::slides::engine::ImageInfo, EngineError> {
        let decoded = match path {
            Some(path) if !matches!(ext, "tif" | "tiff") => image::open(path),
            _ => image::load_from_memory(bytes),
        };
        let natural = decoded.ok().and_then(|image| {
            (image.width() > 0 && image.height() > 0)
                .then_some((f64::from(image.width()), f64::from(image.height())))
        });
        Ok(shell_ipc_d1::slides::engine::ImageInfo {
            natural,
            dpi: image_dpi::read(bytes),
        })
    }

    pub(crate) fn video_poster(&self, path: &str) -> Result<Option<Vec<u8>>, EngineError> {
        let path = std::path::PathBuf::from(path);
        let ui = Arc::clone(&self.ui);
        self.call(move |_| {
            crate::host::video_thumbnail::video_poster(&path, &ui).map_err(EngineError)
        })
    }

    pub(crate) fn assemble_pages(&self, pages: &[Vec<u8>]) -> Result<Vec<u8>, EngineError> {
        let pages = pages.to_vec();
        self.call(move |_| {
            let (first, rest) = pages
                .split_first()
                .ok_or_else(|| EngineError("no generated pages".into()))?;
            let mut opened = open_pptx(first).map_err(EngineError)?;
            for bytes in rest {
                pptx_engine::deck::merge_slide_from_pptx(&mut opened, bytes, &Default::default())
                    .map_err(EngineError)?;
            }
            let size = opened.deck.size;
            for slide in &mut opened.deck.slides {
                pptx_engine::background_promote::promote_slide_background(slide, &size);
                pptx_engine::deck::autofit_generated_text_boxes(slide);
            }
            save_pptx(&mut opened).map_err(EngineError)
        })
    }

    fn grow_generated_page(&self, owner: &RenderOwner, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
        let lease = owner_lease(owner).map_err(|error| error.0)?;
        let deck = self.open(&bytes).map_err(|error| error.0)?;
        if let Err(error) = self.bind_view(deck, owner) {
            self.release(deck);
            return Err(error.0);
        }
        let result = (|| {
            let Some(snapshot) = self
                .render_input(deck, owner, 1.0, Some(0), false, None)
                .map_err(|error| error.0)?
            else {
                return Ok(bytes);
            };
            let generation = snapshot["generation"]
                .as_u64()
                .ok_or("invalid generated page generation")?;
            let request = serde_json::json!({
                "slide": snapshot["deck"]["slides"][0],
                "size": snapshot["deck"]["size"],
                "deckId": deck.0, "generation": generation, "ownerViewId": owner.view.0,
            });
            let measured = self
                .call_slides_service("slides.measurePageText", &request, owner)
                .map_err(|error| error.0)?;
            if !lease.is_active() {
                return Err("slide view lease expired during text measurement".into());
            }
            let nodes = measured
                .as_array()
                .ok_or("invalid generated text measurements")?
                .clone();
            let captured_owner = owner.clone();
            self.call(move |state| {
                owner_lease(&captured_owner)?;
                state.authorize(deck, &captured_owner)?;
                let current = state.deck(deck)?;
                if current.generation != generation {
                    return Err(EngineError("generated page changed while measuring".into()));
                }
                let current = state.write_deck(deck)?;
                let slide = current
                    .opened
                    .deck
                    .slides
                    .first_mut()
                    .ok_or_else(|| EngineError("generated page has no slide".into()))?;
                let mut changed = false;
                for node in nodes {
                    let id = node
                        .get("sourceId")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| EngineError("invalid text measurement source".into()))?;
                    let number = |value: &serde_json::Value, key: &str| {
                        value
                            .get(key)
                            .and_then(read_host_number)
                            .ok_or_else(|| EngineError(format!("invalid text measurement {key}")))
                    };
                    let height = number(&node, "boxHeight")?;
                    let content = number(&node, "contentHeight")?;
                    let ink = node
                        .get("inkBottom")
                        .filter(|value| !value.is_null())
                        .map(|_| number(&node, "inkBottom"))
                        .transpose()?
                        .unwrap_or(0.0);
                    let insets = node
                        .get("insets")
                        .ok_or_else(|| EngineError("missing text measurement insets".into()))?;
                    let need = jscompat::math_max(content, ink)
                        + number(insets, "t")?
                        + number(insets, "b")?;
                    let grow = need - height;
                    if grow < 0.5 {
                        continue;
                    }
                    let Some(pptx_engine::types::SlideElement::Text(element)) =
                        slide.elements.as_mut().and_then(|elements| {
                            elements.iter_mut().find(|element| element.id() == id)
                        })
                    else {
                        continue;
                    };
                    if *element.r#type != Some(pptx_engine::types::ElementType::Text) {
                        continue;
                    }
                    let Some(transform) = element.transform.as_mut() else {
                        return Err(EngineError("text element has no transform".into()));
                    };
                    transform.offset.cy = jscompat::JsNum(jscompat::math_max(
                        jscompat::math_round(need * 9525.0),
                        1.0,
                    ));
                    match node.get("anchor").and_then(serde_json::Value::as_str) {
                        Some("middle") => {
                            transform.offset.y.0 -= jscompat::math_round(grow * 9525.0 / 2.0)
                        }
                        Some("bottom") => {
                            transform.offset.y.0 -= jscompat::math_round(grow * 9525.0)
                        }
                        _ => {}
                    }
                    element.dirty_transform.set(true);
                    changed = true;
                }
                if changed {
                    save_pptx(&mut current.opened).map_err(EngineError)
                } else {
                    Ok(bytes)
                }
            })
            .map_err(|error| error.0)
        })();
        self.release(deck);
        result
    }

    pub(crate) fn generate_page(
        &self,
        owner: &RenderOwner,
        spec_json: &str,
        default_fonts: &shell_contract::IpcValue,
    ) -> Result<shell_ipc_d1::slides::engine::GeneratedPage, String> {
        super::slides_generate::generate_page(spec_json, default_fonts, |bytes, _fonts| {
            self.grow_generated_page(owner, bytes)
        })
    }

    pub(crate) fn template_cover(
        &self,
        owner: &RenderOwner,
        id: &str,
        lang: &str,
        fit: f64,
    ) -> Result<Option<shell_contract::IpcValue>, EngineError> {
        let Some(spec) = super::slides_templates::sample_cover(id, lang).map_err(EngineError)?
        else {
            return Ok(None);
        };
        let page = self
            .generate_page(owner, &spec, &shell_contract::IpcValue::Undefined)
            .map_err(EngineError)?;
        let deck = self.open(&page.bytes)?;
        if let Err(error) = self.bind_view(deck, owner) {
            self.release(deck);
            return Err(error);
        }
        let result = self.render_slide(deck, owner, 0, fit, false);
        let cover = match result {
            Ok(Some(cover)) => cover,
            Ok(None) => {
                self.release(deck);
                return Ok(None);
            }
            Err(error) => {
                self.release(deck);
                return Err(error);
            }
        };
        let lease = match owner_lease(owner) {
            Ok(lease) => lease,
            Err(error) => {
                self.release(deck);
                return Err(error);
            }
        };
        let view = owner.view;
        let id = id.to_owned();
        let lang = lang.to_owned();
        let owner = owner.clone();
        let register = self.call(move |state| {
            state.authorize(deck, &owner)?;
            if !lease.is_active() {
                return Err(EngineError(
                    "slide view lease expired before cover registration".into(),
                ));
            }
            let mut retained = Vec::new();
            for old in std::mem::take(&mut state.covers) {
                if same_owner(&old.owner, &owner) && old.id == id && old.lang == lang {
                    state.decks.remove(&old.deck);
                    super::slides_media::release(old.deck);
                    super::slides_fonts::release(old.deck);
                } else {
                    retained.push(old);
                }
            }
            retained.push(Cover {
                view,
                lease: (*lease).clone(),
                owner,
                id,
                lang,
                deck,
            });
            state.covers = retained;
            Ok(())
        });
        if let Err(error) = register {
            self.release(deck);
            return Err(error);
        }
        Ok(Some(cover))
    }

    pub(crate) fn extract_merge_source(
        &self,
        bytes: &[u8],
    ) -> Result<Option<shell_contract::IpcValue>, EngineError> {
        use shell_contract::{BytesKind, IpcValue};
        let source = pptx_engine::deck::extract_merge_slide_source(bytes).map_err(EngineError)?;
        let Some(source) = source else {
            return Ok(None);
        };
        let rels =
            serde_json::to_value(source.rels).map_err(|error| EngineError(error.to_string()))?;
        let media = source
            .media
            .into_iter()
            .map(|item| {
                IpcValue::Object(vec![
                    ("path".into(), IpcValue::String(item.path)),
                    (
                        "bytes".into(),
                        IpcValue::Bytes(BytesKind::Uint8Array, item.bytes),
                    ),
                ])
            })
            .collect();
        let chain = source
            .layout_chain
            .into_iter()
            .map(|item| {
                let mut fields = vec![
                    ("path".into(), IpcValue::String(item.path)),
                    (
                        "bytes".into(),
                        IpcValue::Bytes(BytesKind::Uint8Array, item.bytes),
                    ),
                ];
                if let Some(rels) = item.rels {
                    fields.push(("rels".into(), IpcValue::Bytes(BytesKind::Uint8Array, rels)));
                }
                IpcValue::Object(fields)
            })
            .collect();
        Ok(Some(IpcValue::Object(vec![
            (
                "srcSlidePath".into(),
                IpcValue::String(source.src_slide_path),
            ),
            ("slideXml".into(), IpcValue::String(source.slide_xml)),
            (
                "rels".into(),
                shell_ipc_d1::value::from_json(&jscompat::json_value_from_serde(&rels)),
            ),
            ("media".into(), IpcValue::Array(media)),
            ("layoutChain".into(), IpcValue::Array(chain)),
        ])))
    }

    fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut ActorState) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError> {
        if thread::current().id() == self.actor.thread {
            return Err(EngineError("slides engine reentry is not allowed".into()));
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        self.actor
            .sender
            .send(Box::new(move |state| {
                let _ = sender.send(job(state));
            }))
            .map_err(|_| EngineError("slides engine has stopped".into()))?;
        receiver
            .recv()
            .map_err(|_| EngineError("slides engine stopped during a request".into()))?
    }

    pub(crate) fn open(&self, bytes: &[u8]) -> Result<DeckId, EngineError> {
        let bytes = bytes.to_vec();
        self.call(move |state| {
            let opened = open_pptx(&bytes).map_err(EngineError)?;
            let previous = NEXT_DECK_ID
                .fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |id| (id < 9_007_199_254_740_991).then_some(id + 1),
                )
                .map_err(|_| EngineError("slides deck identifiers exhausted".into()))?;
            let id = DeckId(previous + 1);
            let generation = next_generation(state.next_generation)?;
            state.next_generation = generation;
            state.decks.insert(
                id,
                DeckState {
                    opened,
                    masters: HashMap::new(),
                    generation,
                    views: HashMap::new(),
                },
            );
            Ok(id)
        })
    }

    pub(crate) fn release(&self, deck: DeckId) {
        let _ = self.actor.sender.send(Box::new(move |state| {
            state.decks.remove(&deck);
            super::slides_media::release(deck);
            super::slides_fonts::release(deck);
        }));
    }

    pub(crate) fn bind_view(&self, deck: DeckId, owner: &RenderOwner) -> Result<(), EngineError> {
        let lease = owner_lease(owner)?;
        let view = owner.view;
        let owner = owner.clone();
        self.call(move |state| {
            owner_lease(&owner)?;
            if !lease.is_active() {
                return Err(EngineError("slide view lease is no longer active".into()));
            }
            if let Some(existing) = state.deck(deck)?.views.get(&view) {
                if same_owner(&existing.owner, &owner) {
                    return Ok(());
                }
                if owner_lease(&existing.owner).is_ok() {
                    return Err(EngineError(
                        "different slide document is already bound".into(),
                    ));
                }
            }
            state.write_deck(deck)?.views.insert(
                view,
                BoundView {
                    owner,
                    lease: (*lease).clone(),
                },
            );
            Ok(())
        })
    }

    pub(crate) fn unbind_view(&self, deck: DeckId, owner: &RenderOwner) -> Result<(), EngineError> {
        let lease = captured_lease(owner)?;
        let view = owner.view;
        let owner = owner.clone();
        self.call(move |state| {
            let matches = state
                .decks
                .get(&deck)
                .and_then(|deck| deck.views.get(&view))
                .is_some_and(|bound| same_owner(&bound.owner, &owner));
            if !matches {
                return Ok(());
            }
            let generation = next_generation(state.next_generation)?;
            super::slides_media::revoke_deck_view(deck, view, &lease).map_err(EngineError)?;
            let deck = state.deck(deck)?;
            deck.views.remove(&view);
            deck.generation = generation;
            state.next_generation = generation;
            Ok(())
        })
    }

    pub(crate) fn replace_view(
        &self,
        old: DeckId,
        new: DeckId,
        owner: &RenderOwner,
    ) -> Result<(), EngineError> {
        let lease = owner_lease(owner)?;
        let view = owner.view;
        let owner = owner.clone();
        self.call(move |state| {
            owner_lease(&owner)?;
            state.authorize(old, &owner)?;
            if old == new {
                return Ok(());
            }
            let destination = state.deck(new)?;
            let already_bound = match destination.views.get(&view) {
                Some(bound) if same_owner(&bound.owner, &owner) => true,
                Some(_) => {
                    return Err(EngineError(
                        "different slide view lease is already bound".into(),
                    ));
                }
                None => false,
            };
            let source_generation = next_generation(state.next_generation)?;
            let destination_generation = if already_bound {
                source_generation
            } else {
                next_generation(source_generation)?
            };
            super::slides_media::revoke_deck_view(old, view, &lease).map_err(EngineError)?;
            let source = state.deck(old)?;
            source.views.remove(&view);
            source.generation = source_generation;
            if !already_bound {
                let destination = state.deck(new)?;
                destination.views.insert(
                    view,
                    BoundView {
                        owner,
                        lease: (*lease).clone(),
                    },
                );
                destination.generation = destination_generation;
            }
            state.next_generation = destination_generation;
            Ok(())
        })
    }

    pub(crate) fn revoke_view(
        &self,
        view: shell_contract::ViewId,
        lease: crate::views::SlidesViewLease,
    ) {
        let _ = self.actor.sender.send(Box::new(move |state| {
            super::slides_media::revoke_view(view, &lease);
            super::slides_fonts::revoke_view(view, &lease);
            let affected: Vec<_> = state
                .decks
                .iter()
                .filter_map(|(id, deck)| {
                    deck.views
                        .get(&view)
                        .is_some_and(|bound| bound.lease.same_lifetime(&lease))
                        .then_some(*id)
                })
                .collect();
            for id in affected {
                if let Ok(generation) = next_generation(state.next_generation) {
                    state.next_generation = generation;
                    if let Some(deck) = state.decks.get_mut(&id) {
                        deck.views.remove(&view);
                        deck.generation = generation;
                    }
                } else if let Some(deck) = state.decks.get_mut(&id) {
                    deck.views.remove(&view);
                }
            }
            let mut retained = Vec::new();
            for cover in std::mem::take(&mut state.covers) {
                if cover.view == view && cover.lease.same_lifetime(&lease) {
                    state.decks.remove(&cover.deck);
                    super::slides_media::release(cover.deck);
                    super::slides_fonts::release(cover.deck);
                } else {
                    retained.push(cover);
                }
            }
            state.covers = retained;
        }));
    }

    pub(crate) fn transfer_history(
        &self,
        old: DeckId,
        replacement: DeckId,
        owners: &[RenderOwner],
    ) -> Result<(), EngineError> {
        let views = owners
            .iter()
            .map(|owner| Ok((owner.view, (*owner_lease(owner)?).clone())))
            .collect::<Result<Vec<_>, EngineError>>()?;
        let owners = owners.to_vec();
        self.call(move |state| {
            for owner in &owners {
                owner_lease(owner)?;
            }
            if old == replacement {
                return Err(EngineError(
                    "cannot transfer slides history to the same deck".into(),
                ));
            }
            let source = state
                .decks
                .get(&old)
                .ok_or_else(|| EngineError(format!("unknown slides deck {}", old.0)))?;
            let unique: std::collections::HashSet<_> =
                views.iter().map(|(view, _)| *view).collect();
            if unique.len() != views.len() {
                return Err(EngineError("duplicate slide history owner".into()));
            }
            if source.views.len() != owners.len()
                || owners.iter().any(|owner| {
                    !source
                        .views
                        .get(&owner.view)
                        .is_some_and(|bound| same_owner(&bound.owner, owner))
                })
            {
                return Err(EngineError(
                    "slide history owners do not match deck bindings".into(),
                ));
            }
            let destination = state
                .decks
                .get(&replacement)
                .ok_or_else(|| EngineError(format!("unknown slides deck {}", replacement.0)))?;
            if !destination.views.is_empty() {
                return Err(EngineError(
                    "replacement slide deck already has bindings".into(),
                ));
            }
            let generation = destination.generation;
            let entries = destination
                .opened
                .archive
                .entries()
                .iter()
                .map(|(path, bytes)| (path.to_owned(), bytes.clone()))
                .collect();
            let first_slide = destination
                .opened
                .deck
                .slides
                .first()
                .and_then(|slide| slide.path.as_ref())
                .cloned();
            let mut old_generations: Vec<_> = state
                .snapshots
                .values()
                .filter(|snapshot| snapshot.owner == old)
                .map(|snapshot| snapshot.generation)
                .collect();
            old_generations.sort_unstable();
            old_generations.dedup();
            let mut next = state.next_generation;
            let mut mapping = Vec::with_capacity(old_generations.len());
            for from in old_generations {
                next = next_generation(next)?;
                mapping.push((from, next));
            }
            super::slides_fonts::transfer_history_with(old, replacement, &mapping, || {
                super::slides_media::install_and_transfer(
                    old,
                    replacement,
                    generation,
                    views.iter().cloned(),
                    entries,
                    first_slide,
                    &mapping,
                )
            })
            .map_err(EngineError)?;
            if let Some(destination) = state.decks.get_mut(&replacement) {
                destination.views = owners
                    .into_iter()
                    .zip(views)
                    .map(|(owner, (view, lease))| (view, BoundView { owner, lease }))
                    .collect();
            }
            state.next_generation = next;
            for snapshot in state
                .snapshots
                .values_mut()
                .filter(|snapshot| snapshot.owner == old)
            {
                if let Some((_, generation)) = mapping
                    .iter()
                    .find(|(from, _)| *from == snapshot.generation)
                {
                    snapshot.owner = replacement;
                    snapshot.generation = *generation;
                }
            }
            state.decks.remove(&old);
            super::slides_media::release(old);
            super::slides_fonts::release(old);
            Ok(())
        })
    }

    pub(crate) fn deck_model(&self, deck: DeckId) -> Result<jscompat::JsonValue, EngineError> {
        let model = self.call(move |state| {
            serde_json::to_value(&state.deck(deck)?.opened.deck)
                .map_err(|error| EngineError(error.to_string()))
        })?;
        Ok(jscompat::json_value_from_serde(&model))
    }

    pub(crate) fn map_script_ops(
        &self,
        deck: DeckId,
        request: &shell_contract::IpcValue,
    ) -> Result<Vec<shell_contract::IpcValue>, EngineError> {
        let request = request.clone();
        self.call(move |state| script_map::map(&request, state.deck(deck)?.opened.deck.size.cx.0))
    }

    pub(crate) fn set_slide_size(&self, deck: DeckId, cx: f64, cy: f64) -> Result<(), EngineError> {
        self.call(move |state| {
            pptx_engine::deck::set_slide_size(&mut state.write_deck(deck)?.opened, cx, cy)
                .map_err(EngineError)?;
            Ok(())
        })
    }

    pub(crate) fn size(&self, deck: DeckId) -> Result<DeckSize, EngineError> {
        self.call(move |state| {
            let size = state.deck(deck)?.opened.deck.size;
            Ok(DeckSize {
                cx: size.cx.0,
                cy: size.cy.0,
            })
        })
    }

    pub(crate) fn default_font(&self, deck: DeckId) -> Result<Option<String>, EngineError> {
        self.call(move |state| {
            let archive = &state.deck(deck)?.opened.archive;
            let font = (|| {
                let presentation = archive.read_presentation().ok()?;
                let chain = archive
                    .resolve_slide_chain(presentation.slide_paths.first()?)
                    .ok()?;
                let xml = archive.read_text(chain.theme_path.as_deref()?)?;
                let theme = pptx_engine::theme::parse_theme(&xml).ok()?;
                theme.minor_font.as_ref().cloned()
            })();
            Ok(font)
        })
    }

    pub(crate) fn slide_paths(&self, deck: DeckId) -> Result<Vec<String>, EngineError> {
        self.call(move |state| {
            Ok(state
                .deck(deck)?
                .opened
                .deck
                .slides
                .iter()
                .map(|slide| slide.path.as_deref().unwrap_or_default().to_owned())
                .collect())
        })
    }

    pub(crate) fn slide_refs(
        &self,
        deck: DeckId,
    ) -> Result<Vec<main_core::slides::session_state::SlideRef>, EngineError> {
        fn has_number(element: &pptx_engine::types::SlideElement) -> bool {
            use pptx_engine::types::SlideElement;
            match element {
                SlideElement::Group(group) => group
                    .children
                    .as_ref()
                    .is_some_and(|items| items.iter().any(has_number)),
                SlideElement::Text(text) => text
                    .text
                    .as_ref()
                    .and_then(|body| body.paragraphs.as_ref())
                    .is_some_and(|paragraphs| {
                        paragraphs.iter().any(|paragraph| {
                            paragraph.runs.as_ref().is_some_and(|runs| {
                                runs.iter()
                                    .any(|run| run.field.as_deref() == Some("slidenum"))
                            })
                        })
                    }),
                _ => false,
            }
        }
        self.call(move |state| {
            Ok(state
                .deck(deck)?
                .opened
                .deck
                .slides
                .iter()
                .map(|slide| {
                    let has_number = slide
                        .elements
                        .as_ref()
                        .into_iter()
                        .flatten()
                        .chain(slide.decorations.as_ref().into_iter().flatten())
                        .any(has_number);
                    main_core::slides::session_state::SlideRef {
                        path: slide.path.as_deref().unwrap_or_default().to_owned(),
                        has_number,
                    }
                })
                .collect())
        })
    }

    pub(crate) fn slide_index_of_id(
        &self,
        deck: DeckId,
        id: &str,
    ) -> Result<Option<usize>, EngineError> {
        let id = id.to_owned();
        self.call(move |state| {
            Ok(state
                .deck(deck)?
                .opened
                .deck
                .slides
                .iter()
                .position(|slide| pptx_engine::identity::slide_durable_id(slide) == id))
        })
    }

    pub(crate) fn theme_accents(&self, deck: DeckId) -> Result<Option<Vec<String>>, EngineError> {
        self.call(move |state| {
            let opened = &state.deck(deck)?.opened;
            let accents = (|| {
                let path = opened.deck.slides.first()?.path.as_deref()?;
                let chain = opened.archive.resolve_slide_chain(path).ok()?;
                let xml = opened.archive.read_text(chain.theme_path.as_deref()?)?;
                let theme = pptx_engine::theme::parse_theme(&xml).ok()?;
                let colors = theme.colors.as_ref()?;
                let accents: Vec<String> = (1..=6)
                    .filter_map(|index| {
                        colors
                            .get(&format!("accent{index}"))
                            .filter(|color| !color.is_empty())
                            .cloned()
                    })
                    .collect();
                (accents.len() >= 3).then_some(accents)
            })();
            Ok(accents)
        })
    }

    pub(crate) fn is_imported_chart(
        &self,
        deck: DeckId,
        index: usize,
        source_id: &str,
    ) -> Result<bool, EngineError> {
        let source_id = source_id.to_owned();
        self.call(move |state| {
            let chart = state.deck(deck)?.opened.deck.slides.get(index)
                .and_then(|slide| slide.elements.as_ref())
                .and_then(|elements| elements.iter().find(|element| element.id() == source_id));
            Ok(matches!(chart, Some(pptx_engine::types::SlideElement::Chart(chart)) if chart.descr.as_deref() != Some("aislides-chart")))
        })
    }

    pub(crate) fn chart_data(
        &self,
        deck: DeckId,
        index: usize,
        source_id: &str,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        let source_id = source_id.to_owned();
        self.slide_projection(deck, index, move |slide| {
            serde_json::to_value(pptx_engine::deck::get_chart_element_data(slide, &source_id))
                .map_err(|error| EngineError(error.to_string()))
        })
    }

    pub(crate) fn slides_dirty(&self, deck: DeckId) -> Result<bool, EngineError> {
        self.call(move |state| {
            Ok(state.deck(deck)?.opened.deck.slides.iter().any(|slide| {
                *slide.structure_dirty == Some(true)
                    || slide.elements.as_ref().is_some_and(|elements| {
                        elements.iter().any(|element| {
                            pptx_engine::with_element!(element, el =>
                    *el.dirty == Some(true) || *el.dirty_transform == Some(true))
                        })
                    })
            }))
        })
    }

    pub(crate) fn has_redaction(&self, deck: DeckId) -> Result<bool, EngineError> {
        self.call(move |state| {
            let slides = serde_json::to_value(&state.deck(deck)?.opened.deck.slides)
                .map_err(|error| EngineError(error.to_string()))?;
            let mut stack = vec![&slides];
            while let Some(value) = stack.pop() {
                match value {
                    serde_json::Value::Array(items) => stack.extend(items),
                    serde_json::Value::Object(fields) => {
                        if fields
                            .get("redact")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|label| !label.is_empty())
                        {
                            return Ok(true);
                        }
                        stack.extend(fields.values());
                    }
                    _ => {}
                }
            }
            Ok(false)
        })
    }

    pub(crate) fn element_index(
        &self,
        deck: DeckId,
        slide: usize,
        source_id: &str,
    ) -> Result<Option<usize>, EngineError> {
        let source_id = source_id.to_owned();
        self.call(move |state| {
            let deck = state.deck(deck)?;
            let Some(elements) = deck
                .opened
                .deck
                .slides
                .get(slide)
                .and_then(|slide| slide.elements.as_ref())
            else {
                return Ok(None);
            };
            for (index, element) in elements.iter().enumerate() {
                if pptx_engine::identity::matches_element_ref(element, &source_id)
                    .map_err(EngineError)?
                {
                    return Ok(Some(index));
                }
            }
            Ok(None)
        })
    }

    pub(crate) fn element_id_at(
        &self,
        deck: DeckId,
        slide: usize,
        index: usize,
    ) -> Result<Option<String>, EngineError> {
        self.call(move |state| {
            Ok(state
                .deck(deck)?
                .opened
                .deck
                .slides
                .get(slide)
                .and_then(|slide| slide.elements.as_ref())
                .and_then(|elements| elements.get(index))
                .map(|element| element.id().to_owned()))
        })
    }

    pub(crate) fn group_child_frame(
        &self,
        deck: DeckId,
        slide: usize,
        group_id: &str,
        source_id: &str,
    ) -> Result<Option<shell_ipc_d1::slides::engine::GroupChildFrame>, EngineError> {
        let group_id = group_id.to_owned();
        let source_id = source_id.to_owned();
        self.call(move |state| {
            let page = state.deck(deck)?.opened.deck.slides.get(slide);
            let group = page
                .and_then(|page| page.elements.as_ref())
                .and_then(|elements| {
                    elements.iter().find(|element| {
                        element.id() == group_id
                            && matches!(element, pptx_engine::types::SlideElement::Group(_))
                    })
                });
            let Some(group) = group else {
                return Ok(None);
            };
            let pptx_engine::types::SlideElement::Group(value) = group else {
                return Ok(None);
            };
            let Some(children) = value.children.as_ref() else {
                return Ok(None);
            };
            let mut child_id = None;
            for child in children {
                if child.id() == source_id
                    || pptx_engine::identity::group_child_durable_id(group, child)
                        .map_err(EngineError)?
                        .as_deref()
                        == Some(source_id.as_str())
                    || pptx_engine::identity::element_durable_id(child)
                        .map_err(EngineError)?
                        .as_deref()
                        == Some(source_id.as_str())
                {
                    child_id = Some(child.id().to_owned());
                    break;
                }
            }
            let Some(child_id) = child_id else {
                return Ok(None);
            };
            let rect =
                |rect: &pptx_engine::types::EmuRect| [rect.x.0, rect.y.0, rect.cx.0, rect.cy.0];
            let group_offset = rect(
                &value
                    .transform
                    .as_ref()
                    .ok_or_else(|| EngineError("group has no transform".into()))?
                    .offset,
            );
            Ok(Some(shell_ipc_d1::slides::engine::GroupChildFrame {
                child_id,
                group_offset,
                child_offset: value.child_offset.as_ref().map(rect),
            }))
        })
    }

    pub(crate) fn picture_media(
        &self,
        deck: DeckId,
        slide: usize,
        source_id: &str,
    ) -> Result<Option<shell_ipc_d1::slides::engine::PictureMedia>, EngineError> {
        let source_id = source_id.to_owned();
        self.call(move |state| {
            let opened = &state.deck(deck)?.opened;
            let picture = opened
                .deck
                .slides
                .get(slide)
                .and_then(|slide| slide.elements.as_ref())
                .and_then(|elements| elements.iter().find(|element| element.id() == source_id));
            let Some(pptx_engine::types::SlideElement::Picture(picture)) = picture else {
                return Ok(None);
            };
            let Some(media) = picture.media.as_ref() else {
                return Ok(None);
            };
            let Some(target) = media.target.clone() else {
                return Ok(None);
            };
            let kind = match media.kind {
                pptx_engine::types::MediaKind::Audio => "audio",
                pptx_engine::types::MediaKind::Video => "video",
            };
            Ok(Some(shell_ipc_d1::slides::engine::PictureMedia {
                kind: kind.to_owned(),
                target,
                external: media.external.unwrap_or(false),
            }))
        })
    }

    pub(crate) fn model3d_target(
        &self,
        deck: DeckId,
        slide: usize,
        source_id: &str,
    ) -> Result<Option<String>, EngineError> {
        let source_id = source_id.to_owned();
        self.call(move |state| {
            let opened = &state.deck(deck)?.opened;
            let picture = opened
                .deck
                .slides
                .get(slide)
                .and_then(|slide| slide.elements.as_ref())
                .and_then(|elements| elements.iter().find(|element| element.id() == source_id));
            let Some(pptx_engine::types::SlideElement::Picture(picture)) = picture else {
                return Ok(None);
            };
            Ok(picture
                .model3d
                .as_ref()
                .and_then(|model| model.target.clone()))
        })
    }

    pub(crate) fn background_image(
        &self,
        deck: DeckId,
        slide: usize,
    ) -> Result<Option<String>, EngineError> {
        self.call(move |state| {
            let slide = state.deck(deck)?.opened.deck.slides.get(slide);
            Ok(slide.and_then(|slide| match slide.background.as_ref() {
                Some(pptx_engine::types::Fill::Image(image)) => Some(image.media_ref.clone()),
                _ => None,
            }))
        })
    }

    pub(crate) fn master_has_element(
        &self,
        deck: DeckId,
        path: &str,
        source_id: &str,
    ) -> Result<bool, EngineError> {
        let path = path.to_owned();
        let source_id = source_id.to_owned();
        self.call(move |state| {
            let deck = state.deck(deck)?;
            if let Some(elements) = deck
                .masters
                .get(&path)
                .and_then(|slide| slide.elements.as_ref())
            {
                for element in elements {
                    if pptx_engine::identity::matches_element_ref(element, &source_id)
                        .map_err(EngineError)?
                    {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        })
    }

    pub(crate) fn master_preview_transform(
        &self,
        deck: DeckId,
        path: &str,
        source_id: &str,
        offset: [f64; 4],
        rot: f64,
    ) -> Result<(), EngineError> {
        let path = path.to_owned();
        let source_id = source_id.to_owned();
        self.call(move |state| {
            let deck = state.write_deck(deck)?;
            let Some(elements) = deck
                .masters
                .get_mut(&path)
                .and_then(|slide| slide.elements.as_mut())
            else {
                return Ok(());
            };
            for element in elements {
                if pptx_engine::identity::matches_element_ref(element, &source_id)
                    .map_err(EngineError)?
                {
                    pptx_engine::with_element!(element, item => {
                        if let Some(transform) = item.transform.as_mut() {
                            transform.offset = pptx_engine::types::EmuRect {
                                x: jscompat::JsNum(offset[0]), y: jscompat::JsNum(offset[1]),
                                cx: jscompat::JsNum(offset[2]), cy: jscompat::JsNum(offset[3]),
                            };
                            transform.rot = jscompat::JsNum(rot);
                        }
                    });
                    break;
                }
            }
            Ok(())
        })
    }

    pub(crate) fn sections(&self, deck: DeckId) -> Result<shell_contract::IpcValue, EngineError> {
        let sections = self.call(move |state| {
            let sections = deck_sections(&state.deck(deck)?.opened)?;
            serde_json::to_value(sections).map_err(|error| EngineError(error.to_string()))
        })?;
        Ok(shell_ipc_d1::value::from_json(
            &jscompat::json_value_from_serde(&sections),
        ))
    }

    pub(crate) fn section_slide_indices(
        &self,
        deck: DeckId,
        id: &shell_contract::IpcValue,
    ) -> Result<Option<Vec<usize>>, EngineError> {
        let id = id.clone();
        self.call(move |state| {
            let opened = &state.deck(deck)?.opened;
            let sections = deck_sections(opened)?;
            let normalized =
                pptx_engine::sections::normalize_sections(&sections, opened.deck.slides.len());
            match id {
                shell_contract::IpcValue::Null | shell_contract::IpcValue::Undefined => {
                    Ok(Some(normalized.lead))
                }
                shell_contract::IpcValue::String(id) => Ok(normalized
                    .sections
                    .into_iter()
                    .find(|section| section.id == id)
                    .map(|section| section.slide_indices)),
                _ => Ok(None),
            }
        })
    }

    pub(crate) fn read_part(
        &self,
        deck: DeckId,
        path: &str,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let path = path.to_owned();
        self.call(move |state| {
            Ok(state
                .deck(deck)?
                .opened
                .archive
                .read_bytes(&path)
                .map(<[u8]>::to_vec))
        })
    }

    pub(crate) fn copy_slide(
        &self,
        deck: DeckId,
        index: usize,
    ) -> Result<Option<shell_contract::IpcValue>, EngineError> {
        let bundle = self.call(move |state| {
            let opened = &mut state.write_deck(deck)?.opened;
            let bundle =
                pptx_engine::deck::copy_slide(opened, index as f64).map_err(EngineError)?;
            bundle
                .map(serde_json::to_value)
                .transpose()
                .map_err(|error| EngineError(error.to_string()))
        })?;
        Ok(bundle
            .map(|value| shell_ipc_d1::value::from_json(&jscompat::json_value_from_serde(&value))))
    }

    pub(crate) fn copy_elements(
        &self,
        deck: DeckId,
        slide: usize,
        source_ids: &[String],
    ) -> Result<Option<Vec<shell_contract::IpcValue>>, EngineError> {
        let source_ids = source_ids.to_vec();
        let values = self.call(move |state| {
            let opened = &mut state.write_deck(deck)?.opened;
            let Some(page) = opened.deck.slides.get(slide) else {
                return Ok(None);
            };
            let indices: Vec<_> = source_ids
                .iter()
                .filter_map(|id| {
                    page.elements
                        .as_ref()
                        .and_then(|elements| elements.iter().position(|element| element.id() == id))
                })
                .collect();
            let mut values = Vec::with_capacity(indices.len());
            for index in indices {
                let item = pptx_engine::deck::copy_element_data(opened, slide, index)
                    .map_err(EngineError)?;
                values.push(
                    serde_json::to_value(item).map_err(|error| EngineError(error.to_string()))?,
                );
            }
            Ok(Some(values))
        })?;
        Ok(values.map(|values| {
            values
                .into_iter()
                .map(|value| {
                    shell_ipc_d1::value::from_json(&jscompat::json_value_from_serde(&value))
                })
                .collect()
        }))
    }

    pub(crate) fn element_link(
        &self,
        deck: DeckId,
        slide: &shell_contract::IpcValue,
        source_id: &shell_contract::IpcValue,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        let slide = slide.clone();
        let source_id = source_id.clone();
        let link = self.call(move |state| {
            let opened = &state.deck(deck)?.opened;
            let index =
                shell_ipc_d1::slides::session::slide_index(&slide, opened.deck.slides.len())
                    .unwrap_or(usize::MAX);
            let shell_contract::IpcValue::String(source_id) = source_id else {
                return Ok(serde_json::Value::Null);
            };
            let link = pptx_engine::hyperlink::get_element_link(opened, index, &source_id)
                .map_err(EngineError)?;
            serde_json::to_value(link).map_err(|error| EngineError(error.to_string()))
        })?;
        Ok(shell_ipc_d1::value::from_json(
            &jscompat::json_value_from_serde(&link),
        ))
    }

    pub(crate) fn slide_links(
        &self,
        deck: DeckId,
        slide: &shell_contract::IpcValue,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        let slide = slide.clone();
        let links = self.call(move |state| {
            let opened = &state.deck(deck)?.opened;
            let index =
                shell_ipc_d1::slides::session::slide_index(&slide, opened.deck.slides.len())
                    .unwrap_or(usize::MAX);
            let links =
                pptx_engine::hyperlink::get_slide_links(opened, index).map_err(EngineError)?;
            serde_json::to_value(links).map_err(|error| EngineError(error.to_string()))
        })?;
        Ok(shell_ipc_d1::value::from_json(
            &jscompat::json_value_from_serde(&links),
        ))
    }

    pub(crate) fn run_links(
        &self,
        deck: DeckId,
        slide: &shell_contract::IpcValue,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        let slide = slide.clone();
        let links = self.call(move |state| {
            let opened = &state.deck(deck)?.opened;
            let index =
                shell_ipc_d1::slides::session::slide_index(&slide, opened.deck.slides.len())
                    .unwrap_or(usize::MAX);
            let links =
                pptx_engine::hyperlink::get_run_links(opened, index).map_err(EngineError)?;
            serde_json::to_value(links).map_err(|error| EngineError(error.to_string()))
        })?;
        Ok(shell_ipc_d1::value::from_json(
            &jscompat::json_value_from_serde(&links),
        ))
    }

    pub(crate) fn master_parts(&self, deck: DeckId) -> Result<Vec<MasterPart>, EngineError> {
        self.call(move |state| {
            let parts =
                pptx_engine::master_edit::list_master_parts(&state.deck(deck)?.opened.archive)
                    .map_err(EngineError)?;
            Ok(parts
                .into_iter()
                .map(|part| MasterPart {
                    part_path: part.part_path,
                    kind: shell_contract::IpcValue::String(
                        match part.kind {
                            pptx_engine::master_edit::MasterPartKind::Master => "master",
                            pptx_engine::master_edit::MasterPartKind::Layout => "layout",
                        }
                        .to_owned(),
                    ),
                    name: shell_contract::IpcValue::String(part.name),
                })
                .collect())
        })
    }

    fn master_model(
        &self,
        deck: DeckId,
        path: &str,
        parse: bool,
    ) -> Result<Option<serde_json::Value>, EngineError> {
        let path = path.to_owned();
        self.call(move |state| {
            let deck = state.deck(deck)?;
            if parse {
                let slide = pptx_engine::master_edit::parse_master_part(
                    &deck.opened.archive,
                    &path,
                    &deck.opened.uids,
                )
                .map_err(EngineError)?;
                match slide {
                    Some(slide) => {
                        deck.masters.insert(path.clone(), slide);
                    }
                    None => {
                        deck.masters.remove(&path);
                        return Ok(None);
                    }
                }
            }
            deck.masters
                .get(&path)
                .map(serde_json::to_value)
                .transpose()
                .map_err(|error| EngineError(error.to_string()))
        })
    }

    fn render_input(
        &self,
        id: DeckId,
        owner: &RenderOwner,
        fit: f64,
        index: Option<usize>,
        reparse: bool,
        master: Option<(String, bool)>,
    ) -> Result<Option<serde_json::Value>, EngineError> {
        let owner = owner.clone();
        self.call(move |state| {
            let view = owner.view;
            state.authorize(id, &owner)?;
            if reparse || master.as_ref().is_some_and(|(_, parse)| *parse) {
                state.write_deck(id)?;
            }
            let deck = state.deck(id)?;
            if reparse {
                let index = index.ok_or_else(|| EngineError("reparse requires a slide index".into()))?;
                if !pptx_engine::deck::materialize_slide(&mut deck.opened, index).map_err(EngineError)? { return Ok(None); }
            }
            let master_model = if let Some((path, parse)) = master {
                if parse {
                    let slide = pptx_engine::master_edit::parse_master_part(&deck.opened.archive, &path, &deck.opened.uids).map_err(EngineError)?;
                    match slide {
                        Some(slide) => { deck.masters.insert(path.clone(), slide); }
                        None => { deck.masters.remove(&path); return Ok(None); }
                    }
                }
                let Some(slide) = deck.masters.get(&path) else { return Ok(None); };
                Some(jscompat::writing_for_host(|| serde_json::to_value(slide)).map_err(|error| EngineError(error.to_string()))?)
            } else { None };
            let generation = deck.generation;
            let opened = &deck.opened;
            if let Some(index) = index {
                if index >= opened.deck.slides.len() && master_model.is_none() { return Ok(None); }
            }
            let model = jscompat::writing_for_host(|| serde_json::to_value(&opened.deck)).map_err(|error| EngineError(error.to_string()))?;
            let entries: Vec<_> = opened.archive.entries().iter().map(|(path, bytes)| (path.to_owned(), bytes.clone())).collect();
            let first_slide = opened.deck.slides.first().and_then(|slide| slide.path.as_ref()).cloned();
            super::slides_media::install(id, generation, deck.views.iter().map(|(view, bound)| (*view, bound.lease.clone())), entries.clone(), first_slide).map_err(EngineError)?;
            let entries: Vec<_> = entries.into_iter().map(|(path, bytes)| serde_json::json!({ "path": path, "bytes": bytes })).collect();
            let mut request = serde_json::json!({
                "deck": model, "entries": entries, "originalHash": opened.archive.original_hash(),
                "deckId": id.0, "generation": generation, "ownerViewId": view.0, "fitWidthPx": host_number(fit),
            });
            if let Some(index) = index { request["slideIndex"] = serde_json::json!(index); }
            if let Some(master) = master_model { request["master"] = master; }
            Ok(Some(request))
        })
    }

    fn render(
        &self,
        id: DeckId,
        owner: &RenderOwner,
        fit: f64,
        index: Option<usize>,
        reparse: bool,
        master: Option<(String, bool)>,
    ) -> Result<Option<shell_contract::IpcValue>, EngineError> {
        use base64::Engine as _;
        let lease = owner_lease(owner)?;
        let Some(request) = self.render_input(id, owner, fit, index, reparse, master)? else {
            return Ok(None);
        };
        let generation = request["generation"]
            .as_u64()
            .ok_or_else(|| EngineError("invalid slides render generation".into()))?;
        let name = if index.is_none() && request.get("master").is_none() {
            "slides.renderAll"
        } else {
            "slides.renderSlide"
        };
        let reply = self.call_slides_service(name, &request, owner)?;
        let header = reply
            .get("header")
            .ok_or_else(|| EngineError("slides renderer returned no wire header".into()))?;
        let buffers = reply
            .get("buffers")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| EngineError("slides renderer returned no wire buffers".into()))?
            .iter()
            .map(|buffer| {
                let text = buffer
                    .as_str()
                    .ok_or_else(|| EngineError("invalid slides render buffer".into()))?;
                base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map_err(|error| EngineError(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = shell_core::wire::decode(header, &buffers)
            .map_err(|error| EngineError(format!("{error:?}")))?;
        if !lease.is_active() {
            return Err(EngineError(
                "slide view lease expired while rendering".into(),
            ));
        }
        let captured_owner = owner.clone();
        self.call(move |state| {
            owner_lease(&captured_owner)?;
            state.authorize(id, &captured_owner)?;
            if state.deck(id)?.generation != generation {
                return Err(EngineError("slide changed while rendering".into()));
            }
            Ok(())
        })?;
        match (name, result) {
            ("slides.renderAll", value @ shell_contract::IpcValue::Array(_)) => Ok(Some(value)),
            ("slides.renderSlide", value @ shell_contract::IpcValue::Object(_)) => Ok(Some(value)),
            ("slides.renderSlide", shell_contract::IpcValue::Null) => Ok(None),
            _ => Err(EngineError(
                "slides renderer returned an invalid result".into(),
            )),
        }
    }

    pub(crate) fn render_all(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        fit: f64,
    ) -> Result<Vec<shell_contract::IpcValue>, EngineError> {
        match self.render(deck, owner, fit, None, false, None)? {
            Some(shell_contract::IpcValue::Array(slides)) => Ok(slides),
            _ => Err(EngineError(
                "slides renderer returned an invalid slide list".into(),
            )),
        }
    }

    pub(crate) fn render_slide(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        index: usize,
        fit: f64,
        reparse: bool,
    ) -> Result<Option<shell_contract::IpcValue>, EngineError> {
        self.render(deck, owner, fit, Some(index), reparse, None)
    }

    pub(crate) fn autofit_resize(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        index: usize,
        source_id: &str,
        rendered: Option<shell_contract::IpcValue>,
        fit: f64,
    ) -> Result<Option<shell_contract::IpcValue>, EngineError> {
        let Some(rendered) = rendered else {
            return Ok(None);
        };
        let source_id = source_id.to_owned();
        let previous = rendered.clone();
        let captured_owner = owner.clone();
        let changed = self.call(move |state| {
            owner_lease(&captured_owner)?;
            state.authorize(deck, &captured_owner)?;
            let next = next_generation(state.next_generation)?;
            let current = state.deck(deck)?;
            let changed = autofit::resize(&mut current.opened, index, &source_id, &previous, fit)?;
            if changed {
                current.generation = next;
                state.next_generation = next;
            }
            Ok(changed)
        })?;
        if changed {
            self.render_slide(deck, owner, index, fit, false)
        } else {
            Ok(Some(rendered))
        }
    }

    pub(crate) fn sync_autofit_scale(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        index: usize,
        source_id: &str,
        rendered: Option<shell_contract::IpcValue>,
        fit: f64,
    ) -> Result<Option<shell_contract::IpcValue>, EngineError> {
        let Some(rendered) = rendered else {
            return Ok(None);
        };
        let lease = owner_lease(owner)?;
        let Some(snapshot) = self.render_input(deck, owner, fit, Some(index), false, None)? else {
            return Ok(Some(rendered));
        };
        let generation = snapshot["generation"]
            .as_u64()
            .ok_or_else(|| EngineError("invalid slides generation".into()))?;
        let source = source_id.to_owned();
        let previous = rendered.clone();
        let first_owner = owner.clone();
        let input = self.call(move |state| {
            owner_lease(&first_owner)?;
            state.authorize(deck, &first_owner)?;
            let current = state.deck(deck)?;
            if current.generation != generation {
                return Err(EngineError("slide changed before autofit".into()));
            }
            autofit::refit_input(&current.opened, index, &source, &previous, fit)
        })?;
        let Some((id, mut input)) = input else {
            return Ok(Some(rendered));
        };
        input["deckId"] = serde_json::json!(deck.0);
        input["generation"] = serde_json::json!(generation);
        input["ownerViewId"] = serde_json::json!(owner.view.0);
        let answer = self.call_slides_service("slides.measureAutofit", &input, owner)?;
        if !lease.is_active() {
            return Err(EngineError(
                "slide view lease expired during autofit".into(),
            ));
        }
        let scale = answer
            .get("fontScale")
            .and_then(read_host_number)
            .ok_or_else(|| EngineError("invalid autofit font scale".into()))?;
        let reduction = answer
            .get("lnSpcReduction")
            .and_then(read_host_number)
            .ok_or_else(|| EngineError("invalid autofit line spacing".into()))?;
        let captured_owner = owner.clone();
        let changed = self.call(move |state| {
            owner_lease(&captured_owner)?;
            state.authorize(deck, &captured_owner)?;
            if state.deck(deck)?.generation != generation {
                return Err(EngineError("slide changed during autofit".into()));
            }
            let next = next_generation(state.next_generation)?;
            let current = state.deck(deck)?;
            let changed = autofit::apply_refit(&mut current.opened, index, &id, scale, reduction)?;
            if changed {
                current.generation = next;
                state.next_generation = next;
            }
            Ok(changed)
        })?;
        if changed {
            Ok(self
                .render_slide(deck, owner, index, fit, false)?
                .or(Some(rendered)))
        } else {
            Ok(Some(rendered))
        }
    }

    pub(crate) fn master_parse(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        path: &str,
        fit: f64,
    ) -> Result<Option<shell_contract::IpcValue>, EngineError> {
        self.render(deck, owner, fit, None, false, Some((path.to_owned(), true)))
    }

    pub(crate) fn master_render(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        path: &str,
        fit: f64,
    ) -> Result<Option<shell_contract::IpcValue>, EngineError> {
        self.render(
            deck,
            owner,
            fit,
            None,
            false,
            Some((path.to_owned(), false)),
        )
    }

    pub(crate) fn materialize_slide(&self, deck: DeckId, index: usize) -> Result<(), EngineError> {
        self.call(move |state| {
            pptx_engine::deck::materialize_slide(&mut state.write_deck(deck)?.opened, index)
                .map_err(EngineError)?;
            Ok(())
        })
    }

    pub(crate) fn slide_notes(&self, deck: DeckId, index: usize) -> Result<String, EngineError> {
        self.call(move |state| {
            let deck = state.deck(deck)?;
            let slide = deck
                .opened
                .deck
                .slides
                .get(index)
                .ok_or_else(|| EngineError(format!("unknown slide {index}")))?;
            let path = slide
                .path
                .as_deref()
                .ok_or_else(|| EngineError("slide has no part path".into()))?;
            pptx_engine::notes::get_slide_notes(&deck.opened.archive, path).map_err(EngineError)
        })
    }

    pub(crate) fn slide_comments(
        &self,
        deck: DeckId,
        index: usize,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        let comments = self.call(move |state| {
            let deck = state.deck(deck)?;
            let slide = deck
                .opened
                .deck
                .slides
                .get(index)
                .ok_or_else(|| EngineError(format!("unknown slide {index}")))?;
            let path = slide
                .path
                .as_deref()
                .ok_or_else(|| EngineError("slide has no part path".into()))?;
            let comments = pptx_engine::comments::get_slide_comments(&deck.opened.archive, path)
                .map_err(EngineError)?;
            serde_json::to_value(comments).map_err(|error| EngineError(error.to_string()))
        })?;
        Ok(shell_ipc_d1::value::from_json(
            &jscompat::json_value_from_serde(&comments),
        ))
    }

    fn slide_projection(
        &self,
        deck: DeckId,
        index: usize,
        project: impl FnOnce(&Slide) -> Result<serde_json::Value, EngineError> + Send + 'static,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        let result = self.call(move |state| {
            let slide = state
                .deck(deck)?
                .opened
                .deck
                .slides
                .get(index)
                .ok_or_else(|| EngineError(format!("unknown slide {index}")))?;
            project(slide)
        })?;
        Ok(shell_ipc_d1::value::from_json(
            &jscompat::json_value_from_serde(&result),
        ))
    }

    pub(crate) fn header_footer(
        &self,
        deck: DeckId,
        index: usize,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        self.slide_projection(deck, index, |slide| {
            serde_json::to_value(pptx_engine::headerfooter::read_header_footer(slide))
                .map_err(|error| EngineError(error.to_string()))
        })
    }

    pub(crate) fn slide_transition(
        &self,
        deck: DeckId,
        index: usize,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        self.slide_projection(deck, index, |slide| {
            serde_json::to_value(pptx_engine::deck::get_slide_transition(slide))
                .map_err(|error| EngineError(error.to_string()))
        })
    }

    pub(crate) fn show_timing(
        &self,
        deck: DeckId,
        index: usize,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        self.slide_projection(deck, index, |slide| {
            let spids = pptx_engine::narration::narration_spids(slide);
            let ids: Vec<_> = slide
                .elements
                .as_ref()
                .into_iter()
                .flatten()
                .filter(|element| {
                    pptx_engine::animation::element_spid(element)
                        .is_some_and(|id| spids.contains(&id.0))
                })
                .map(|element| element.id().to_owned())
                .collect();
            Ok(serde_json::json!({
                "advanceMs": pptx_engine::deck::get_slide_advance_time(slide),
                "narrationIds": ids,
            }))
        })
    }

    pub(crate) fn has_narration(&self, deck: DeckId, index: usize) -> Result<bool, EngineError> {
        self.call(move |state| {
            let slide = state
                .deck(deck)?
                .opened
                .deck
                .slides
                .get(index)
                .ok_or_else(|| EngineError(format!("unknown slide {index}")))?;
            Ok(!pptx_engine::narration::narration_spids(slide).is_empty())
        })
    }

    pub(crate) fn animations(
        &self,
        deck: DeckId,
        index: usize,
        labels: &[(String, String)],
    ) -> Result<shell_contract::IpcValue, EngineError> {
        let labels = labels.to_vec();
        self.slide_projection(deck, index, move |slide| {
            let mut output = Vec::new();
            for animation in pptx_engine::animation::get_slide_animations(slide) {
                let element = slide.elements.as_ref().and_then(|elements| elements.iter().find(|element|
                    pptx_engine::animation::element_spid(element).is_some_and(|id| id.0 == animation.spid.0)));
                let Some(element) = element else { continue; };
                let kind = element.element_type().and_then(|kind| serde_json::to_value(kind).ok())
                    .and_then(|value| value.as_str().map(str::to_owned));
                let name = pptx_engine::with_element!(element, item => item.name.as_deref())
                    .filter(|name| !name.is_empty())
                    .or_else(|| kind.as_deref().and_then(|kind| labels.iter().find(|(key, _)| key == kind).map(|(_, value)| value.as_str())))
                    .or_else(|| labels.iter().find(|(key, _)| key == "object").map(|(_, value)| value.as_str()))
                    .unwrap_or("");
                let mut item = serde_json::json!({
                    "sourceId": element.id(), "targetName": name, "effect": animation.effect,
                    "trigger": animation.trigger, "durationMs": animation.duration_ms, "delayMs": animation.delay_ms,
                });
                if let Some(fields) = item.as_object_mut() {
                    if let Some(direction) = animation.direction { fields.insert("direction".into(), serde_json::json!(direction)); }
                    if let Some(path) = animation.motion_path { fields.insert("motionPath".into(), serde_json::json!(path)); }
                    if let Some(paragraph) = animation.paragraph { fields.insert("paragraph".into(), serde_json::json!(paragraph)); }
                }
                output.push(item);
            }
            Ok(serde_json::Value::Array(output))
        })
    }

    pub(crate) fn shape_keys(
        &self,
        deck: DeckId,
        index: usize,
    ) -> Result<shell_contract::IpcValue, EngineError> {
        self.slide_projection(deck, index, |slide| {
            let mut output = Vec::new();
            for element in slide.elements.as_ref().into_iter().flatten() {
                let xml = pptx_engine::with_element!(element, item => item.anchor.as_ref().map(|anchor| anchor.original_xml.as_ref())).unwrap_or("");
                let tag = jscompat::js_regex!(r"<p:cNvPr\b[^>]*>").exec(xml).and_then(|found| found.get(0)).unwrap_or("");
                let name = pptx_engine::with_element!(element, item => item.name.as_ref().cloned())
                    .or_else(|| pptx_engine::xml_utils::attribute_of(tag, "name")).unwrap_or_default();
                let mut value = serde_json::json!({
                    "sourceId": element.id(),
                    "spid": pptx_engine::animation::element_spid(element),
                    "name": name,
                });
                if matches!(pptx_engine::xml_utils::attribute_of(tag, "hidden").as_deref(), Some("1" | "true")) {
                    value["hidden"] = serde_json::Value::Bool(true);
                }
                output.push(value);
            }
            Ok(serde_json::Value::Array(output))
        })
    }

    pub(crate) fn save_to_file(&self, deck: DeckId, path: &str) -> Result<(), EngineError> {
        let bytes = self.save_bytes(deck)?;
        std::fs::write(path, bytes).map_err(|error| EngineError(error.to_string()))
    }

    pub(crate) fn layouts(&self, deck: DeckId) -> Result<shell_contract::IpcValue, EngineError> {
        let layouts = self.call(move |state| {
            let opened = &state.deck(deck)?.opened;
            let mut layouts = pptx_engine::layout::list_slide_layouts(&opened.archive);
            if pptx_engine::builtin_layouts::should_offer_builtin_layouts(&layouts) {
                let names = layouts.iter().map(|layout| layout.name.clone()).collect();
                layouts.extend(pptx_engine::builtin_layouts::builtin_layout_infos(
                    &opened.deck.size,
                    &names,
                ));
            }
            serde_json::to_value(layouts).map_err(|error| EngineError(error.to_string()))
        })?;
        Ok(shell_ipc_d1::value::from_json(
            &jscompat::json_value_from_serde(&layouts),
        ))
    }

    pub(crate) fn ensure_builtin_layout(
        &self,
        deck: DeckId,
        key: &str,
    ) -> Result<Option<String>, EngineError> {
        let key = key.to_owned();
        self.call(move |state| {
            let opened = &mut state.write_deck(deck)?.opened;
            pptx_engine::builtin_layouts::ensure_builtin_layout(
                &mut opened.archive,
                &opened.deck.size,
                &key,
            )
            .map_err(EngineError)
        })
    }

    pub(crate) fn run_txn(
        &self,
        deck: DeckId,
        owner: &RenderOwner,
        request: &TxnRequest,
    ) -> Result<TxnResult, EngineError> {
        let request = request.clone();
        let owner = owner.clone();
        self.call(move |state| {
            owner_lease(&owner)?;
            state.authorize(deck, &owner)?;
            let deck = state.write_deck(deck)?;
            Ok(pptx_ops::run_txn(
                &mut deck.opened,
                &mut deck.masters,
                &request,
            ))
        })
    }

    pub(crate) fn save_bytes(&self, deck: DeckId) -> Result<Vec<u8>, EngineError> {
        self.call(move |state| save_pptx(&mut state.write_deck(deck)?.opened).map_err(EngineError))
    }

    pub(crate) fn commit_saved(&self, deck: DeckId) -> Result<(), EngineError> {
        self.call(move |state| {
            commit_saved(&mut state.write_deck(deck)?.opened);
            Ok(())
        })
    }

    pub(crate) fn commit_saved_owned(
        &self,
        deck: DeckId,
        origin: &RenderOwner,
        destination: &RenderOwner,
        participants: &[RenderOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<(), EngineError> {
        let origin = origin.clone();
        let mut targets = participants.to_vec();
        targets.push(destination.clone());
        let cancel = cancel.clone();
        let pending = pending.clone();
        let additional = additional.to_vec();
        let effects = effects.clone();
        self.call(move |state| {
            let extra = additional.iter().collect::<Vec<_>>();
            effects
                .with_owned_effect_many_extra(
                    &origin,
                    &targets,
                    &cancel,
                    &pending,
                    &extra,
                    &mut || {
                        let deck = state.write_deck(deck).map_err(|error| error.0)?;
                        commit_saved(&mut deck.opened);
                        Ok(())
                    },
                )
                .map_err(EngineError)
        })
    }

    pub(crate) fn commit_saved_existing_document(
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
    ) -> Result<(), EngineError> {
        let owner = owner.clone();
        let document = document.clone();
        let destination = destination.clone();
        let cancel = cancel.clone();
        let pending = pending.clone();
        let participants = participants.to_vec();
        let additional = additional.to_vec();
        let effects = effects.clone();
        self.call(move |state| {
            let captured = destination.binding().owner();
            if captured.view != owner.view
                || !captured
                    .context
                    .renderer()
                    .zip(owner.context.renderer())
                    .is_some_and(|(a, b)| a.same_generation(b))
                || !captured
                    .context
                    .document_lease()
                    .is_some_and(|lease| lease.same_generation(&document))
            {
                return Err(EngineError(
                    "native save belongs to another presentation".into(),
                ));
            }
            let extra = additional.iter().collect::<Vec<_>>();
            effects
                .with_existing_document_destination_effect(
                    &destination,
                    destination.path(),
                    &participants,
                    &cancel,
                    &pending,
                    &extra,
                    &mut || {
                        state.authorize(deck, &owner).map_err(|error| error.0)?;
                        let deck = state.write_deck(deck).map_err(|error| error.0)?;
                        commit_saved(&mut deck.opened);
                        Ok(())
                    },
                )
                .map_err(EngineError)
        })
    }

    pub(crate) fn reparse_deck(&self, deck: DeckId) -> Result<(), EngineError> {
        self.call(move |state| {
            let deck = state.write_deck(deck)?;
            reparse_deck(&mut deck.opened).map_err(EngineError)?;
            deck.masters.clear();
            Ok(())
        })
    }

    pub(crate) fn snapshot(&self, deck: DeckId, meta_dirty: bool) -> Result<Snapshot, EngineError> {
        let id = self.call(move |state| {
            let owner = deck;
            let deck = state.deck(deck)?;
            let snapshot = DeckSnapshot {
                deck: deck.opened.deck.clone(),
                entries: deck.opened.archive.entries().clone(),
                original_hash: deck.opened.archive.original_hash().to_owned(),
                save_snapshot: deck.opened.save_snapshot.clone(),
                masters: deck.masters.clone(),
                meta_dirty,
                owner,
                generation: deck.generation,
                next_uid: deck.opened.uids.next(),
            };
            let id = state
                .next_snapshot
                .checked_add(1)
                .ok_or_else(|| EngineError("slides snapshot identifiers exhausted".into()))?;
            state.next_snapshot = id;
            state.snapshots.insert(id, snapshot);
            Ok(id)
        })?;
        Ok(Snapshot(Rc::new(SnapshotHandle {
            actor: Arc::downgrade(&self.actor),
            id,
        })))
    }

    pub(crate) fn restore(&self, deck: DeckId, snapshot: &Snapshot) -> Result<bool, EngineError> {
        let handle = snapshot
            .0
            .downcast_ref::<SnapshotHandle>()
            .ok_or_else(|| EngineError("invalid slides snapshot".into()))?;
        let actor = handle
            .actor
            .upgrade()
            .ok_or_else(|| EngineError("slides snapshot owner has stopped".into()))?;
        if !Arc::ptr_eq(&actor, &self.actor) {
            return Err(EngineError(
                "slides snapshot belongs to another engine".into(),
            ));
        }
        let id = handle.id;
        self.call(move |state| {
            let snapshot = state
                .snapshots
                .get(&id)
                .ok_or_else(|| EngineError("slides snapshot no longer exists".into()))?;
            let restored = (
                snapshot.deck.clone(),
                snapshot.entries.clone(),
                snapshot.save_snapshot.clone(),
                snapshot.masters.clone(),
                snapshot.meta_dirty,
                snapshot.original_hash.clone(),
                snapshot.next_uid,
            );
            let deck = state.write_deck(deck)?;
            deck.opened.deck = restored.0;
            deck.opened.archive =
                pptx_engine::zip::PackageArchive::from_parts(restored.1.into_iter(), restored.5);
            deck.opened.save_snapshot = restored.2;
            deck.opened.uids =
                pptx_engine::parse::Uids::starting_at(deck.opened.uids.next().max(restored.6));
            deck.masters = restored.3;
            Ok(restored.4)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestThread;
    impl shell_contract::runtime::Thread for TestThread {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            job();
        }
        fn is_current(&self) -> bool {
            false
        }
    }

    fn engine() -> NativeSlidesEngine {
        let env = super::super::slides_fonts::FontEnvironment {
            home: std::path::PathBuf::new(),
            user: None,
            template: None,
            platform: main_core::pdf::font_locate::Platform::Other,
        };
        NativeSlidesEngine::new(
            Arc::new(TestThread),
            env,
            Arc::new(|_| Err("test has no renderer".into())),
        )
        .unwrap()
    }

    fn blank() -> Vec<u8> {
        pptx_engine::blank::create_blank_pptx(0.0).unwrap()
    }

    fn owner(view: u32, lease: &crate::views::SlidesViewLease) -> RenderOwner {
        RenderOwner {
            view: shell_contract::ViewId(view),
            context: shell_contract::ipc::NativeContext::new(lease.clone()),
        }
    }

    #[test]
    fn transactions_and_render_require_exact_deck_and_lease() {
        let engine = engine();
        let first = engine.open(&blank()).unwrap();
        let second = engine.open(&blank()).unwrap();
        let lease = crate::views::SlidesViewLease::new();
        let impostor = owner(7, &crate::views::SlidesViewLease::new());
        let owner = owner(7, &lease);
        engine.bind_view(first, &owner).unwrap();
        let request = TxnRequest::atomic(Vec::new());
        assert!(engine.run_txn(first, &owner, &request).is_ok());
        assert!(engine.run_txn(second, &owner, &request).is_err());
        assert!(engine.run_txn(first, &impostor, &request.dry()).is_err());
        assert!(
            engine
                .render_input(second, &owner, 1.0, Some(0), false, None)
                .is_err()
        );
        lease.revoke();
        assert!(engine.run_txn(first, &owner, &request).is_err());
        engine.unbind_view(first, &owner).unwrap();
        engine.bind_view(first, &impostor).unwrap();
        engine.unbind_view(first, &owner).unwrap();
        assert!(engine.run_txn(first, &impostor, &request.dry()).is_ok());
    }

    #[test]
    fn stale_document_cleanup_cannot_remove_same_view_new_generation() {
        let engine = engine();
        let deck = engine.open(&blank()).unwrap();
        let lease = crate::views::SlidesViewLease::new();
        let first = RenderOwner {
            view: shell_contract::ViewId(13),
            context: shell_contract::ipc::NativeContext::with_renderer(
                lease.clone(),
                shell_contract::runtime::RendererGeneration::new(),
            ),
        };
        let second = RenderOwner {
            view: first.view,
            context: shell_contract::ipc::NativeContext::with_renderer(
                lease,
                shell_contract::runtime::RendererGeneration::new(),
            ),
        };
        engine.bind_view(deck, &first).unwrap();
        engine.unbind_view(deck, &second).unwrap();
        assert!(
            engine
                .run_txn(deck, &first, &TxnRequest::atomic(Vec::new()).dry())
                .is_ok()
        );
        assert!(
            engine
                .run_txn(deck, &second, &TxnRequest::atomic(Vec::new()).dry())
                .is_err()
        );
    }

    #[test]
    fn staged_view_replacement_preserves_old_on_failure() {
        let engine = engine();
        let old = engine.open(&blank()).unwrap();
        let new = engine.open(&blank()).unwrap();
        let lease = crate::views::SlidesViewLease::new();
        let owner = owner(8, &lease);
        engine.bind_view(old, &owner).unwrap();
        engine.bind_view(new, &owner).unwrap();
        assert!(engine.replace_view(old, DeckId(u64::MAX), &owner).is_err());
        assert!(
            engine
                .run_txn(old, &owner, &TxnRequest::atomic(Vec::new()))
                .is_ok()
        );
        engine.replace_view(old, new, &owner).unwrap();
        assert!(
            engine
                .run_txn(old, &owner, &TxnRequest::atomic(Vec::new()))
                .is_err()
        );
        assert!(
            engine
                .run_txn(new, &owner, &TxnRequest::atomic(Vec::new()))
                .is_ok()
        );
    }

    #[test]
    fn history_replacement_moves_all_bindings_and_snapshots_atomically() {
        let engine = engine();
        let old = engine.open(&blank()).unwrap();
        let new = engine.open(&blank()).unwrap();
        let first = owner(9, &crate::views::SlidesViewLease::new());
        let second = owner(10, &crate::views::SlidesViewLease::new());
        engine.bind_view(old, &first).unwrap();
        engine.bind_view(old, &second).unwrap();
        let snapshot = engine.snapshot(old, true).unwrap();
        assert!(engine.transfer_history(old, new, &[first.clone()]).is_err());
        assert!(
            engine
                .run_txn(old, &second, &TxnRequest::atomic(Vec::new()).dry())
                .is_ok()
        );
        engine
            .transfer_history(old, new, &[first.clone(), second.clone()])
            .unwrap();
        assert!(engine.size(old).is_err());
        assert!(
            engine
                .run_txn(new, &first, &TxnRequest::atomic(Vec::new()).dry())
                .is_ok()
        );
        assert!(
            engine
                .run_txn(new, &second, &TxnRequest::atomic(Vec::new()).dry())
                .is_ok()
        );
        assert!(engine.restore(new, &snapshot).unwrap());
    }

    #[test]
    fn snapshot_restores_model_archive_and_remains_reusable() {
        let engine = engine();
        let deck = engine.open(&blank()).unwrap();
        let initial = engine.size(deck).unwrap();
        let presentation = engine.read_part(deck, "ppt/presentation.xml").unwrap();
        let snapshot = engine.snapshot(deck, true).unwrap();
        engine
            .set_slide_size(deck, 6_000_000.0, 4_000_000.0)
            .unwrap();
        assert_ne!(engine.size(deck).unwrap(), initial);
        assert!(engine.restore(deck, &snapshot).unwrap());
        assert_eq!(engine.size(deck).unwrap(), initial);
        assert_eq!(
            engine.read_part(deck, "ppt/presentation.xml").unwrap(),
            presentation
        );
        engine
            .set_slide_size(deck, 8_000_000.0, 5_000_000.0)
            .unwrap();
        assert!(engine.restore(deck, &snapshot).unwrap());
        let reopened = engine.open(&engine.save_bytes(deck).unwrap()).unwrap();
        assert_eq!(engine.size(reopened).unwrap(), initial);
    }

    #[test]
    fn invalid_deck_and_foreign_snapshot_are_errors() {
        let first = engine();
        let second = engine();
        let first_deck = first.open(&blank()).unwrap();
        let second_deck = second.open(&blank()).unwrap();
        assert_ne!(first_deck, second_deck);
        let snapshot = first.snapshot(first_deck, false).unwrap();
        assert!(second.restore(second_deck, &snapshot).is_err());
        first.release(first_deck);
        assert!(first.size(first_deck).is_err());
        first.release(first_deck);
    }

    #[test]
    fn snapshot_outlives_source_deck_for_replacement() {
        let engine = engine();
        let source = engine.open(&blank()).unwrap();
        engine
            .set_slide_size(source, 7_000_000.0, 4_000_000.0)
            .unwrap();
        let next_uid = engine
            .call(move |state| {
                let deck = state.deck(source)?;
                for _ in 0..100 {
                    deck.opened.uids.uid("el");
                }
                Ok(deck.opened.uids.next())
            })
            .unwrap();
        let snapshot = engine.snapshot(source, false).unwrap();
        let replacement = engine.open(&blank()).unwrap();
        engine.release(source);
        assert!(!engine.restore(replacement, &snapshot).unwrap());
        let restored_uid = engine
            .call(move |state| Ok(state.deck(replacement)?.opened.uids.next()))
            .unwrap();
        assert!(restored_uid >= next_uid);
        assert_eq!(engine.size(replacement).unwrap().cx, 7_000_000.0);
    }

    #[test]
    fn master_cache_survives_snapshot_and_restoration() {
        let engine = engine();
        let deck = engine.open(&blank()).unwrap();
        let path = engine
            .master_parts(deck)
            .unwrap()
            .into_iter()
            .find(|part| part.kind == shell_contract::IpcValue::String("master".into()))
            .unwrap()
            .part_path;
        let original = engine.master_model(deck, &path, true).unwrap().unwrap();
        let snapshot = engine.snapshot(deck, false).unwrap();
        engine.reparse_deck(deck).unwrap();
        assert!(engine.master_model(deck, &path, false).unwrap().is_none());
        engine.restore(deck, &snapshot).unwrap();
        assert_eq!(
            engine.master_model(deck, &path, false).unwrap(),
            Some(original)
        );
    }

    #[test]
    fn actor_reentry_and_stopped_snapshot_owner_fail() {
        let engine = Arc::new(engine());
        let deck = engine.open(&blank()).unwrap();
        let nested = Arc::clone(&engine);
        assert!(engine.call(move |_| nested.size(deck)).is_err());
        let snapshot = engine.snapshot(deck, false).unwrap();
        drop(engine);
        let replacement = super::tests::engine();
        let other_deck = replacement.open(&blank()).unwrap();
        assert!(replacement.restore(other_deck, &snapshot).is_err());
    }

    #[test]
    fn actor_shutdown_returns_errors_instead_of_empty_results() {
        let engine = engine();
        let deck = engine.open(&blank()).unwrap();
        let failed = engine.call::<()>(|_| panic!("stop test actor"));
        assert!(failed.is_err());
        assert!(engine.size(deck).is_err());
        assert!(engine.save_bytes(deck).is_err());
    }

    #[test]
    fn generated_pages_merge_and_reopen() {
        let engine = engine();
        let pages = vec![blank(), blank()];
        let result = engine.assemble_pages(&pages).unwrap();
        let reopened = engine.open(&result).unwrap();
        assert_eq!(engine.slide_paths(reopened).unwrap().len(), 2);
        assert!(engine.extract_merge_source(&pages[0]).unwrap().is_some());
        assert!(engine.assemble_pages(&[]).is_err());
    }

    #[test]
    fn nonfinite_measurement_numbers_round_trip() {
        assert!(read_host_number(&host_number(f64::NAN)).unwrap().is_nan());
        assert_eq!(
            read_host_number(&host_number(f64::INFINITY)),
            Some(f64::INFINITY)
        );
        assert_eq!(
            read_host_number(&host_number(f64::NEG_INFINITY)),
            Some(f64::NEG_INFINITY)
        );
        assert_eq!(read_host_number(&host_number(12.5)), Some(12.5));
        assert_eq!(
            read_host_number(&serde_json::json!({"$num": "NaN", "other": 1})),
            None
        );
    }
}
