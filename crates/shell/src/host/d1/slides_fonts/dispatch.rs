use std::{collections::HashMap, sync::{Arc, LazyLock, Mutex, atomic::{AtomicBool, Ordering}}};
use serde_json::{Value, json};
use shell_contract::{ViewId, runtime::CancelToken};
use shell_ipc_d1::slides::engine::DeckId;
use crate::views::{SlidesMediaAuthorization, SlidesViewLease};
use super::{FontRenderer, FontSession};

struct FontState {
    authorization: SlidesMediaAuthorization,
    active: AtomicBool,
    font: Mutex<(FontSession, Box<dyn FontRenderer>)>,
}

struct Request {
    deck: DeckId,
    generation: u64,
    view: ViewId,
    active: AtomicBool,
    state: Arc<FontState>,
}
#[derive(Default)]
struct Registry {
    requests: HashMap<u64, Arc<Request>>,
    fonts: HashMap<(DeckId, u64, ViewId), Arc<FontState>>,
    private: Vec<(String, super::metrics::FontFace, bool, bool, SlidesMediaAuthorization)>,
    embedded: Vec<(pptx_engine::embedded_fonts::EmbeddedFontFace, SlidesMediaAuthorization)>,
    epoch: u64,
}
static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

/// Native admission is performed before the engine request starts. The
/// renderer must execute in the exact ingress document's execution context;
/// obtaining a current context by ViewId is not an admissible implementation.
pub(super) fn install(id: u64, deck: DeckId, generation: u64, authorization: SlidesMediaAuthorization, session: FontSession, renderer: &mut Option<Box<dyn FontRenderer>>, epoch: u64) -> Result<bool, String> {
    let view = authorization.view;
    let lease = authorization.lease();
    if !authorization.is_current() { return Err("slide document is no longer current".into()); }
    super::super::slides_media::authorize(deck, generation, view, &lease)?;
    let mut registry = REGISTRY.lock().map_err(|_| "font request registry poisoned")?;
    if registry.epoch != epoch { return Ok(false); }
    if registry.requests.contains_key(&id) { return Err("duplicate font request".into()); }
    let key = (deck, generation, view);
    let reusable = registry.fonts.get(&key).filter(|state| {
        state.active.load(Ordering::Acquire) && state.authorization.same_document(&authorization)
    }).cloned();
    let state = if let Some(state) = reusable { state } else {
        let renderer = renderer.take().ok_or("font renderer already consumed")?;
        let state = Arc::new(FontState { authorization, active: AtomicBool::new(true), font: Mutex::new((session, renderer)) });
        if let Some(old) = registry.fonts.insert(key, state.clone()) { old.active.store(false, Ordering::Release); }
        state
    };
    let request = Arc::new(Request { deck, generation, view, active: AtomicBool::new(true), state });
    registry.requests.insert(id, request);
    Ok(true)
}

pub(super) fn snapshot() -> Result<(u64, Vec<pptx_engine::embedded_fonts::EmbeddedFontFace>), String> {
    let registry = REGISTRY.lock().map_err(|_| "font request registry poisoned")?;
    Ok((registry.epoch, registry.embedded.iter().map(|(face, _)| face.clone()).collect()))
}

pub(in crate::host::d1) fn adopt_embedded_fonts(owner: &SlidesMediaAuthorization, faces: Vec<pptx_engine::embedded_fonts::EmbeddedFontFace>) -> Result<bool, String> {
    if !owner.is_current() { return Err("slide document is no longer current".into()); }
    let mut registry = REGISTRY.lock().map_err(|_| "font request registry poisoned")?;
    let mut added = Vec::new();
    for face in faces {
        let key = main_core::pdf::font_locate::norm(&face.typeface);
        if key.is_empty() { continue; }
        let duplicate = registry.embedded.iter().any(|(old, _)| old.style == face.style && main_core::pdf::font_locate::norm(&old.typeface) == key)
            || added.iter().any(|old: &pptx_engine::embedded_fonts::EmbeddedFontFace| old.style == face.style && main_core::pdf::font_locate::norm(&old.typeface) == key);
        if !duplicate { added.push(face); }
    }
    if added.is_empty() { return Ok(false); }
    let next = registry.epoch.checked_add(1).ok_or("font cache epochs exhausted")?;
    if !owner.is_current() { return Err("slide document is no longer current".into()); }
    registry.embedded.extend(added.into_iter().map(|face| (face, owner.clone())));
    registry.epoch = next;
    for state in registry.fonts.values() { state.active.store(false, Ordering::Release); }
    registry.fonts.clear();
    Ok(true)
}

pub(in crate::host::d1) fn reset_font_metrics() -> Result<(), String> {
    let mut registry = REGISTRY.lock().map_err(|_| "font request registry poisoned")?;
    registry.epoch = registry.epoch.checked_add(1).ok_or("font cache epochs exhausted")?;
    for state in registry.fonts.values() { state.active.store(false, Ordering::Release); }
    registry.fonts.clear();
    Ok(())
}

fn check(request: &Request, lease: &SlidesViewLease, cancel: &CancelToken) -> Result<(), String> {
    if cancel.is_cancelled() || !request.active.load(Ordering::Acquire) || !request.state.active.load(Ordering::Acquire) { return Err("slide request cancelled".into()); }
    if !request.state.authorization.is_current() || !request.state.authorization.lease().same_lifetime(lease) { return Err("slide document is no longer current".into()); }
    super::super::slides_media::authorize(request.deck, request.generation, request.view, lease)
}

struct CheckedRenderer<'a> {
    inner: &'a mut dyn FontRenderer,
    request: &'a Request,
    lease: &'a SlidesViewLease,
    cancel: &'a CancelToken,
}
impl FontRenderer for CheckedRenderer<'_> {
    fn register(&mut self, family: &str, bold: bool, italic: bool, weight: Option<&str>, sfnt: &[u8]) -> Result<(), String> {
        check(self.request, self.lease, self.cancel)?;
        self.inner.register(family, bold, italic, weight, sfnt)?;
        check(self.request, self.lease, self.cancel)
    }
    fn measure_100px(&mut self, text: &str, family: &str, bold: bool, italic: bool) -> Result<f64, String> {
        check(self.request, self.lease, self.cancel)?;
        let width = self.inner.measure_100px(text, family, bold, italic)?;
        check(self.request, self.lease, self.cancel)?;
        Ok(width)
    }
}

/// The engine host passes the original immutable pending input and the exact
/// native lease, never authority supplied by the callback body.
pub(super) fn dispatch(id: u64, operation: &str, input: &Value, request_input: &Value, lease: &SlidesViewLease, cancel: &CancelToken) -> Result<Value, String> {
    let request = REGISTRY.lock().map_err(|_| "font request registry poisoned")?.requests.get(&id).cloned().ok_or("slide request is not admitted")?;
    let deck = request_input.get("deckId").and_then(Value::as_u64).ok_or("missing native slide deck")?;
    let generation = request_input.get("generation").and_then(Value::as_u64).ok_or("missing native slide generation")?;
    let view = request_input.get("ownerViewId").and_then(Value::as_u64).and_then(|id| u32::try_from(id).ok()).ok_or("missing native slide view")?;
    if DeckId(deck) != request.deck || generation != request.generation || ViewId(view) != request.view { return Err("slide request authority changed".into()); }
    check(&request, lease, cancel)?;
    let result = match operation {
        "slides.fontMetrics" => {
            let mut state = request.state.font.lock().map_err(|_| "font session poisoned")?;
            let (session, renderer) = &mut *state;
            let mut checked = CheckedRenderer { inner: renderer.as_mut(), request: &request, lease, cancel };
            let result = session.call(input, &mut checked)?;
            check(&request, lease, cancel)?;
            let entries = std::mem::take(&mut session.pending_registered);
            let mut registry = REGISTRY.lock().map_err(|_| "font request registry poisoned")?;
            if !request.state.active.load(Ordering::Acquire)
                || !registry.fonts.get(&(request.deck, request.generation, request.view)).is_some_and(|state| Arc::ptr_eq(state, &request.state)) {
                return Err("slide font session changed".into());
            }
            for (id, face, bold, italic) in entries {
                let entry = (id, face, bold, italic, request.state.authorization.clone());
                if let Some(old) = registry.private.iter_mut().find(|old| old.0 == entry.0) { *old = entry; }
                else { registry.private.push(entry); }
            }
            result
        }
        "slides.mediaResolve" => {
            let items = input.get("items").and_then(Value::as_array).ok_or("missing slide media items")?;
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                check(&request, lease, cancel)?;
                let media = item.get("mediaRef").and_then(Value::as_str).ok_or("missing slide media reference")?;
                let path = item.get("slidePath").and_then(Value::as_str);
                result.push(json!(super::super::slides_media::resolve(request.deck, request.generation, request.view, lease, media, path)?));
            }
            Value::Array(result)
        }
        _ => return Err("unknown slide callback".into()),
    };
    check(&request, lease, cancel)?;
    Ok(result)
}

pub(super) fn close_request(id: u64) {
    if let Ok(mut registry) = REGISTRY.lock() {
        if let Some(request) = registry.requests.remove(&id) { request.active.store(false, Ordering::Release); }
    }
}

fn private_entries(authorization: &SlidesMediaAuthorization) -> Result<Vec<(String, super::metrics::FontFace, bool, bool)>, String> {
    if !authorization.is_current() { return Err("slide document is no longer current".into()); }
    let registry = REGISTRY.lock().map_err(|_| "font request registry poisoned")?;
    let entries = registry.private.iter().map(|(id, face, bold, italic, _owner)| (id.clone(), face.clone(), *bold, *italic)).collect();
    if !authorization.is_current() { return Err("slide document is no longer current".into()); }
    Ok(entries)
}

pub(in crate::host::d1) fn private_faces(authorization: &SlidesMediaAuthorization) -> Result<Vec<Value>, String> {
    let entries = private_entries(authorization)?;
    let result = entries.into_iter().map(|(id, face, bold, italic)| {
        let mut value = json!({"id":id,"family":face.family,"bold":bold,"italic":italic});
        if let Some(weight) = face.weight_range { value["weight"] = json!(weight); }
        value
    }).collect();
    if !authorization.is_current() { return Err("slide document is no longer current".into()); }
    Ok(result)
}

pub(in crate::host::d1) fn private_font_data(authorization: &SlidesMediaAuthorization, id: &str) -> Result<Option<Vec<u8>>, String> {
    let entries = private_entries(authorization)?;
    let result = entries.iter().find(|(key, _, _, _)| key == id).map(|(_, face, _, _)| super::extracted_font(face)).transpose()?;
    if !authorization.is_current() { return Err("slide document is no longer current".into()); }
    Ok(result)
}

pub(in crate::host::d1) fn release(deck: DeckId) {
    if let Ok(mut registry) = REGISTRY.lock() {
        registry.fonts.retain(|(owner, _, _), state| {
            if *owner == deck { state.active.store(false, Ordering::Release); false } else { true }
        });
    }
}

pub(in crate::host::d1) fn prune(deck: DeckId, retained: &[u64]) {
    if let Ok(mut registry) = REGISTRY.lock() {
        registry.fonts.retain(|(owner, generation, _), state| {
            if *owner == deck && !retained.contains(generation) { state.active.store(false, Ordering::Release); false } else { true }
        });
    }
}

pub(in crate::host::d1) fn transfer_history_with(old: DeckId, new: DeckId, generations: &[(u64, u64)], transfer_media: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    if old == new { return Err("cannot transfer slide font history to the same deck".into()); }
    let mut registry = REGISTRY.lock().map_err(|_| "font request registry poisoned")?;
    let mut mapping = HashMap::new();
    let mut destinations = std::collections::HashSet::new();
    for &(from, to) in generations {
        if mapping.insert(from, to).is_some() || !destinations.insert(to)
            || registry.fonts.keys().any(|(deck, generation, _)| *deck == new && *generation == to) {
            return Err("conflicting slide font history generations".into());
        }
    }
    let keys: Vec<_> = registry.fonts.keys().filter_map(|key| {
        if key.0 == old { mapping.get(&key.1).map(|generation| (*key, *generation)) } else { None }
    }).collect();
    transfer_media()?;
    for (key, generation) in keys {
        if let Some(state) = registry.fonts.remove(&key) {
            registry.fonts.insert((new, generation, key.2), state);
        }
    }
    Ok(())
}

pub(in crate::host::d1) fn revoke_view(view: ViewId, lease: &SlidesViewLease) {
    if let Ok(mut registry) = REGISTRY.lock() {
        registry.fonts.retain(|(_, _, owner), state| {
            if *owner == view && state.authorization.lease().same_lifetime(lease) {
                state.active.store(false, Ordering::Release);
                false
            } else { true }
        });
    }
}
