//! Fakes of the slides hosts for tests and for `ipc-probe`: a tiny
//! in-memory deck (slides with text elements, notes and a hidden flag) that
//! can be opened, edited by a few ops, snapshotted and saved, and a shell
//! that records what it was asked. Each call pushes one JSON array named
//! after the TypeScript function it stands for to the probe's log, when the
//! hosts hold one.

use std::any::Any;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use shell_contract::{IpcValue, ViewId, WindowId};

use super::engine::{
    DeckId, DeckSize, Display, EngineError, GeneratedPage, Isolation, PdfExportAnswer,
    PictureMedia, PreparedOpen, SlideRef, SlidesEngine, SlidesEngineHost, SlidesShell,
    SlidesShellHost, Snapshot, TakenHandoff, TxnFailure, TxnRecord, TxnRequest, TxnResult,
};
use super::fixtures::{Fixtures, ipc_of, normalize_sections, spid_of};
use crate::value::{as_num, as_str, field, num, obj, string};

/// the one master part of a fake deck
const MASTER_PART: &str = "ppt/slideMasters/slideMaster1.xml";

/// ops that make an element the executor answers with its id
const CREATING: &[&str] = &[
    "addTable",
    "addChart",
    "addSmartArt",
    "addModel3d",
    "addMedia",
    "groupElements",
    "pasteElements",
];

/// the probe's call log
pub type CallLog = Arc<Mutex<Vec<Value>>>;

#[derive(Clone, Debug, PartialEq)]
struct FakeElement {
    id: String,
    text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct FakeSlide {
    path: String,
    elements: Vec<FakeElement>,
    hidden: bool,
    comments: Vec<(String, String)>,
    // what a probe case's fixtures put into the slide (fixtures.rs)
    notes: String,
    transition: Option<String>,
    advance_ms: Option<f64>,
    narration: Vec<i64>,
    animations: Vec<Value>,
    header_footer: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
struct FakeDeck {
    slides: Vec<FakeSlide>,
    cx: f64,
    cy: f64,
    next_id: u32,
    dirty: bool,
    meta_dirty: bool,
    master: Vec<FakeElement>,
    /// the fill and stroke master ops set, by element id
    master_fill: HashMap<String, IpcValue>,
    master_stroke: HashMap<String, IpcValue>,
}

impl FakeDeck {
    fn blank(count: usize) -> Self {
        FakeDeck {
            slides: (1..=count)
                .map(|n| FakeSlide {
                    path: format!("ppt/slides/slide{n}.xml"),
                    ..Default::default()
                })
                .collect(),
            cx: 9_144_000.0,
            cy: 5_143_500.0,
            next_id: 1,
            dirty: false,
            meta_dirty: false,
            master: vec![FakeElement {
                id: "m0".into(),
                text: "Title".into(),
            }],
            master_fill: HashMap::new(),
            master_stroke: HashMap::new(),
        }
    }

    /// the bytes `savePptx` writes: a line per slide
    fn to_bytes(&self) -> Vec<u8> {
        let mut text = format!("FAKEPPTX {} {} {}\n", self.cx, self.cy, self.next_id);
        for slide in &self.slides {
            let elements: Vec<String> = slide
                .elements
                .iter()
                .map(|e| format!("{}={}", e.id, e.text.replace('|', "/")))
                .collect();
            text.push_str(&format!(
                "{} {} {}\n",
                slide.path,
                u8::from(slide.hidden),
                elements.join("|")
            ));
        }
        text.into_bytes()
    }

    fn from_bytes(bytes: &[u8]) -> Option<FakeDeck> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        let mut head = lines.next()?.strip_prefix("FAKEPPTX ")?.split(' ');
        let cx = head.next()?.parse().ok()?;
        let cy = head.next()?.parse().ok()?;
        let next_id = head.next()?.parse().ok()?;
        let mut slides = Vec::new();
        for line in lines {
            let mut parts = line.splitn(3, ' ');
            let path = parts.next()?.to_owned();
            let hidden = parts.next()? == "1";
            let elements = parts
                .next()
                .unwrap_or("")
                .split('|')
                .filter(|e| !e.is_empty())
                .filter_map(|e| {
                    let (id, text) = e.split_once('=')?;
                    Some(FakeElement {
                        id: id.to_owned(),
                        text: text.to_owned(),
                    })
                })
                .collect();
            slides.push(FakeSlide {
                path,
                elements,
                hidden,
                ..Default::default()
            });
        }
        Some(FakeDeck {
            slides,
            cx,
            cy,
            next_id,
            dirty: false,
            meta_dirty: false,
            master: vec![FakeElement {
                id: "m0".into(),
                text: "Title".into(),
            }],
            master_fill: HashMap::new(),
            master_stroke: HashMap::new(),
        })
    }
}

/// The fake deck engine
pub struct FakeEngine {
    decks: Mutex<HashMap<u64, FakeDeck>>,
    bindings: Mutex<HashMap<DeckId, HashMap<ViewId, shell_contract::runtime::RendererGeneration>>>,
    next: Mutex<u64>,
    blank_slides: usize,
    fx: Fixtures,
    log: Option<CallLog>,
}

impl FakeEngine {
    fn authorize(
        &self,
        deck: DeckId,
        owner: &super::engine::RenderOwner,
    ) -> Result<(), EngineError> {
        let generation = owner
            .context
            .renderer()
            .filter(|generation| generation.is_active())
            .ok_or_else(|| EngineError("renderer is unavailable".into()))?;
        let bindings = self
            .bindings
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !bindings
            .get(&deck)
            .and_then(|views| views.get(&owner.view))
            .is_some_and(|bound| bound.same_generation(generation))
        {
            return Err(EngineError("deck binding does not match".into()));
        }
        Ok(())
    }

    /// `el.type` of an element: a fixture's media or 3D model makes it a picture
    fn kind_of(&self, id: &str) -> &'static str {
        if self.fx.by("media", id).is_null() && self.fx.by("models", id).is_null() {
            "text"
        } else {
            "picture"
        }
    }

    fn record(&self, call: Value) {
        if let Some(log) = &self.log {
            log.lock().unwrap().push(call);
        }
    }

    fn with<R>(&self, deck: DeckId, f: impl FnOnce(&mut FakeDeck) -> R) -> R {
        let mut decks = self.decks.lock().unwrap();
        f(decks.get_mut(&deck.0).expect("a deck the fake opened"))
    }

    /// The text of an element, for tests
    pub fn text_of(&self, deck: DeckId, slide: usize, id: &str) -> Option<String> {
        self.with(deck, |d| {
            d.slides
                .get(slide)?
                .elements
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.text.clone())
        })
    }

    fn render(d: &FakeDeck, index: usize) -> IpcValue {
        let slide = &d.slides[index];
        obj([
            ("partPath", string(slide.path.as_str())),
            ("hidden", IpcValue::Bool(slide.hidden)),
            (
                "elements",
                IpcValue::Array(
                    slide
                        .elements
                        .iter()
                        .map(|e| {
                            obj([
                                ("id", string(e.id.as_str())),
                                ("text", string(e.text.as_str())),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }

    /// the failure of one op against `d`, if it cannot apply
    fn check(d: &FakeDeck, op: &IpcValue) -> Option<String> {
        let name = as_str(field(op, "op")).unwrap_or("");
        let target = field(op, "target");
        if let Some(part) = as_str(field(target, "part")) {
            if part != MASTER_PART {
                return Some("part not found".into());
            }
            return match name {
                "addElement" => None,
                "setText" | "deleteElement" | "setFill" | "setStroke" => {
                    let id = as_str(field(target, "el")).unwrap_or("");
                    (!d.master.iter().any(|e| e.id == id))
                        .then(|| format!("element {id} not found"))
                }
                other => Some(format!("unsupported op {other}")),
            };
        }
        let slide = as_num(field(target, "slide"));
        let slide_ok =
            slide.is_some_and(|s| s.fract() == 0.0 && s >= 0.0 && (s as usize) < d.slides.len());
        match name {
            "addElement" | "setHidden" | "deleteSlide" | "duplicateSlide" | "addComment"
            | "addPicture"
                if !slide_ok =>
            {
                Some("slide not found".into())
            }
            "setText" | "deleteElement" => {
                if !slide_ok {
                    return Some("slide not found".into());
                }
                let id = as_str(field(target, "el")).unwrap_or("");
                let found = d.slides[slide.unwrap() as usize]
                    .elements
                    .iter()
                    .any(|e| e.id == id);
                (!found).then(|| format!("element {id} not found"))
            }
            "deleteComment" => {
                if !slide_ok {
                    return Some("slide not found".into());
                }
                let idx = as_num(field(op, "idx")).unwrap_or(-1.0);
                let count = d.slides[slide.unwrap() as usize].comments.len() as f64;
                (!(idx >= 0.0 && idx < count)).then(|| "no such comment".to_owned())
            }
            "addElement" | "setHidden" | "deleteSlide" | "duplicateSlide" | "addComment"
            | "addPicture" => None,
            // any other op addresses a slide, and an element of it, or none
            _ => {
                if slide.is_none() && field(target, "slide") == &IpcValue::Undefined {
                    return None;
                }
                if !slide_ok {
                    return Some("slide not found".into());
                }
                match as_str(field(target, "el")) {
                    Some(id)
                        if !d.slides[slide.unwrap() as usize]
                            .elements
                            .iter()
                            .any(|e| e.id == id) =>
                    {
                        Some(format!("element {id} not found"))
                    }
                    _ => None,
                }
            }
        }
    }

    fn apply(d: &mut FakeDeck, op: &IpcValue) -> TxnRecord {
        let name = as_str(field(op, "op")).unwrap_or("");
        let target = field(op, "target");
        if as_str(field(target, "part")).is_some() {
            return Self::apply_master(d, op);
        }
        let slide_given = as_num(field(target, "slide")).is_some();
        let slide = as_num(field(target, "slide")).unwrap_or(0.0) as usize;
        let mut created = None;
        match name {
            "addElement" => {
                let id = format!("e{}", d.next_id);
                d.next_id += 1;
                let text = paragraphs_text(field(op, "paragraphs"));
                d.slides[slide].elements.push(FakeElement {
                    id: id.clone(),
                    text,
                });
                created = Some(vec![IpcValue::String(id)]);
            }
            "setText" => {
                let id = as_str(field(target, "el")).unwrap_or("");
                let text = paragraphs_text(field(op, "paragraphs"));
                if let Some(e) = d.slides[slide].elements.iter_mut().find(|e| e.id == id) {
                    e.text = text;
                }
            }
            "deleteElement" => {
                let id = as_str(field(target, "el")).unwrap_or("").to_owned();
                d.slides[slide].elements.retain(|e| e.id != id);
            }
            "setHidden" => {
                d.slides[slide].hidden = matches!(field(op, "hidden"), IpcValue::Bool(true));
            }
            "deleteSlide" => {
                d.slides.remove(slide);
            }
            "duplicateSlide" => {
                let mut copy = d.slides[slide].clone();
                copy.path = format!("ppt/slides/slide{}.xml", d.next_id + 100);
                d.next_id += 1;
                d.slides.insert(slide + 1, copy);
            }
            "addComment" => {
                let author = as_str(field(op, "author")).unwrap_or("").to_owned();
                let text = as_str(field(op, "text")).unwrap_or("").to_owned();
                d.slides[slide].comments.push((author, text));
            }
            "deleteComment" => {
                let idx = as_num(field(op, "idx")).unwrap_or(0.0) as usize;
                d.slides[slide].comments.remove(idx);
            }
            "clearNarration" => d.slides[slide].narration.clear(),
            "addPicture" => {
                let id = format!("e{}", d.next_id);
                d.next_id += 1;
                let ext = as_str(field(op, "ext")).unwrap_or("");
                d.slides[slide].elements.push(FakeElement {
                    id: id.clone(),
                    text: format!("picture:{ext}"),
                });
                created = Some(vec![IpcValue::String(id)]);
            }
            _ if CREATING.contains(&name) => {
                let id = format!("e{}", d.next_id);
                d.next_id += 1;
                if let Some(s) = d.slides.get_mut(slide) {
                    s.elements.push(FakeElement {
                        id: id.clone(),
                        text: name.to_owned(),
                    });
                }
                created = Some(vec![IpcValue::String(id)]);
            }
            _ => {}
        }
        d.dirty = true;
        let after = match name {
            "applyTheme" => obj([("patched", num(1.0)), ("remapped", num(0.0))]),
            "findReplace" => obj([
                ("count", num(1.0)),
                ("changedSlides", IpcValue::Array(vec![num(0.0)])),
            ]),
            "tableStructure" | "tableMerge" => obj([(
                "elementId",
                string(as_str(field(target, "el")).unwrap_or("")),
            )]),
            "pasteSlide" | "insertSlidePptx" => obj([("index", num((slide + 1) as f64))]),
            "addSection" | "renameSection" | "removeSection" | "moveSection" | "setSections" => {
                obj([("section", string(name))])
            }
            "addPicture" | "replacePicture" | "setBackground" => {
                obj([("mediaPath", string("ppt/media/image1.png"))])
            }
            _ => IpcValue::Undefined,
        };
        TxnRecord {
            op: op.clone(),
            slide_id: slide_given.then(|| format!("slide-{slide}")),
            created,
            before: IpcValue::Undefined,
            after,
        }
    }

    /// ops addressed to the master part (`target.part`)
    fn apply_master(d: &mut FakeDeck, op: &IpcValue) -> TxnRecord {
        let name = as_str(field(op, "op")).unwrap_or("");
        let target = field(op, "target");
        let mut created = None;
        match name {
            "addElement" => {
                let id = format!("m{}", d.next_id);
                d.next_id += 1;
                d.master.push(FakeElement {
                    id: id.clone(),
                    text: paragraphs_text(field(op, "paragraphs")),
                });
                created = Some(vec![IpcValue::String(id)]);
            }
            "setText" => {
                let id = as_str(field(target, "el")).unwrap_or("");
                let text = paragraphs_text(field(op, "paragraphs"));
                if let Some(e) = d.master.iter_mut().find(|e| e.id == id) {
                    e.text = text;
                }
            }
            "deleteElement" => {
                let id = as_str(field(target, "el")).unwrap_or("").to_owned();
                d.master.retain(|e| e.id != id);
            }
            "setFill" => {
                let id = as_str(field(target, "el")).unwrap_or("").to_owned();
                d.master_fill.insert(id, field(op, "fill").clone());
            }
            "setStroke" => {
                let id = as_str(field(target, "el")).unwrap_or("").to_owned();
                d.master_stroke.insert(id, field(op, "stroke").clone());
            }
            _ => {}
        }
        d.dirty = true;
        TxnRecord {
            op: op.clone(),
            slide_id: None,
            created,
            before: IpcValue::Undefined,
            after: IpcValue::Undefined,
        }
    }
}

impl SlidesEngine for FakeEngine {
    fn create_blank_pptx(&self) -> Result<Vec<u8>, EngineError> {
        self.record(json!(["pptx-engine.createBlankPptx"]));
        Ok(FakeDeck::blank(self.blank_slides).to_bytes())
    }

    fn open(&self, bytes: &[u8]) -> Result<DeckId, EngineError> {
        self.record(json!(["pptx-engine.openPptx", bytes.len()]));
        let mut deck =
            FakeDeck::from_bytes(bytes).ok_or_else(|| EngineError("not a pptx".into()))?;
        for (index, slide) in deck.slides.iter_mut().enumerate() {
            let at = |key: &str| self.fx.at(key, index);
            slide.notes = at("notes").as_str().unwrap_or("").to_owned();
            slide.transition = at("transitions").as_str().map(str::to_owned);
            slide.advance_ms = at("advanceMs").as_f64();
            slide.narration = at("narration")
                .as_array()
                .map(|spids| spids.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            slide.animations = at("animations").as_array().cloned().unwrap_or_default();
            slide.header_footer = Some(at("headerFooter").clone()).filter(|v| !v.is_null());
        }
        let mut next = self.next.lock().unwrap();
        *next += 1;
        self.decks.lock().unwrap().insert(*next, deck);
        Ok(DeckId(*next))
    }

    fn deck_model(&self, deck: DeckId) -> Result<jscompat::JsonValue, EngineError> {
        use jscompat::{JsObject, JsonValue};
        fn object(fields: impl IntoIterator<Item = (&'static str, JsonValue)>) -> JsonValue {
            let mut object = JsObject::new();
            for (key, value) in fields {
                object.set(key, value);
            }
            JsonValue::Object(object)
        }
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let deck = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        let slides = deck
            .slides
            .iter()
            .map(|slide| {
                let elements = slide
                    .elements
                    .iter()
                    .map(|element| {
                        object([
                            ("id", JsonValue::String(element.id.clone())),
                            ("type", JsonValue::String(self.kind_of(&element.id).into())),
                            (
                                "text",
                                object([(
                                    "paragraphs",
                                    JsonValue::Array(vec![object([(
                                        "runs",
                                        JsonValue::Array(vec![object([(
                                            "text",
                                            JsonValue::String(element.text.clone()),
                                        )])]),
                                    )])]),
                                )]),
                            ),
                        ])
                    })
                    .collect();
                object([
                    ("path", JsonValue::String(slide.path.clone())),
                    ("elements", JsonValue::Array(elements)),
                ])
            })
            .collect();
        Ok(object([
            ("slides", JsonValue::Array(slides)),
            (
                "size",
                object([
                    ("cx", JsonValue::Number(deck.cx)),
                    ("cy", JsonValue::Number(deck.cy)),
                ]),
            ),
        ]))
    }

    fn release(&self, deck: DeckId) {
        self.decks.lock().unwrap().remove(&deck.0);
        self.bindings.lock().unwrap().remove(&deck);
    }

    fn transfer_history(
        &self,
        old: DeckId,
        replacement: DeckId,
        views: &[super::engine::RenderOwner],
    ) -> Result<(), EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if old == replacement || !decks.contains_key(&old.0) || !decks.contains_key(&replacement.0)
        {
            return Err(EngineError("deck is not open".into()));
        }
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let empty = HashMap::new();
        let old_bindings = bindings.get(&old).unwrap_or(&empty);
        if old_bindings.len() != views.len()
            || views
                .iter()
                .map(|owner| owner.view)
                .collect::<std::collections::HashSet<_>>()
                .len()
                != views.len()
            || bindings.get(&replacement).is_some_and(|v| !v.is_empty())
            || views.iter().any(|owner| {
                !old_bindings
                    .get(&owner.view)
                    .zip(owner.context.renderer())
                    .is_some_and(|(bound, current)| {
                        current.is_active() && bound.same_generation(current)
                    })
            })
        {
            return Err(EngineError("deck bindings do not match".into()));
        }
        let moved = bindings.remove(&old).unwrap_or_default();
        bindings.insert(replacement, moved);
        Ok(())
    }

    fn bind_view(
        &self,
        deck: DeckId,
        owner: &super::engine::RenderOwner,
    ) -> Result<(), EngineError> {
        if !self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?
            .contains_key(&deck.0)
        {
            return Err(EngineError("deck is not open".into()));
        }
        let generation = owner
            .context
            .renderer()
            .filter(|generation| generation.is_active())
            .ok_or_else(|| EngineError("renderer is unavailable".into()))?;
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let views = bindings.entry(deck).or_default();
        if views
            .get(&owner.view)
            .is_some_and(|bound| !bound.same_generation(generation))
        {
            return Err(EngineError("deck binding does not match".into()));
        }
        views.insert(owner.view, generation.clone());
        Ok(())
    }

    fn unbind_view(
        &self,
        deck: DeckId,
        owner: &super::engine::RenderOwner,
    ) -> Result<(), EngineError> {
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if let Some(bound) = bindings.get_mut(&deck) {
            if bound
                .get(&owner.view)
                .zip(owner.context.renderer())
                .is_some_and(|(current, owner)| current.same_generation(owner))
            {
                bound.remove(&owner.view);
            }
        }
        Ok(())
    }

    fn replace_view(
        &self,
        old: DeckId,
        new: DeckId,
        owner: &super::engine::RenderOwner,
    ) -> Result<(), EngineError> {
        let generation = owner
            .context
            .renderer()
            .filter(|generation| generation.is_active())
            .ok_or_else(|| EngineError("renderer is unavailable".into()))?;
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if old == new || !decks.contains_key(&old.0) || !decks.contains_key(&new.0) {
            return Err(EngineError("deck is not open".into()));
        }
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !bindings
            .get(&old)
            .and_then(|views| views.get(&owner.view))
            .is_some_and(|bound| bound.same_generation(generation))
            || bindings
                .get(&new)
                .and_then(|views| views.get(&owner.view))
                .is_some_and(|bound| !bound.same_generation(generation))
        {
            return Err(EngineError("deck binding does not match".into()));
        }
        bindings
            .get_mut(&old)
            .expect("binding was checked")
            .remove(&owner.view);
        bindings
            .entry(new)
            .or_default()
            .insert(owner.view, generation.clone());
        Ok(())
    }

    fn set_slide_size(&self, deck: DeckId, cx: f64, cy: f64) -> Result<(), EngineError> {
        self.record(json!(["pptx-engine.setSlideSize", cx, cy]));
        let mut decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get_mut(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        d.cx = cx;
        d.cy = cy;
        Ok(())
    }

    fn size(&self, deck: DeckId) -> Result<DeckSize, EngineError> {
        Ok(self.with(deck, |d| DeckSize { cx: d.cx, cy: d.cy }))
    }

    fn slide_paths(&self, deck: DeckId) -> Result<Vec<String>, EngineError> {
        Ok(self.with(deck, |d| d.slides.iter().map(|s| s.path.clone()).collect()))
    }

    fn slide_refs(&self, deck: DeckId) -> Result<Vec<SlideRef>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        Ok(d.slides
            .iter()
            .map(|s| SlideRef {
                path: s.path.clone(),
                has_number: false,
            })
            .collect())
    }

    fn slides_dirty(&self, deck: DeckId) -> Result<bool, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        Ok(decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?
            .dirty)
    }

    fn default_font(&self, deck: DeckId) -> Result<Option<String>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !decks.contains_key(&deck.0) {
            return Err(EngineError("deck is not open".into()));
        }
        Ok(Some("Calibri".into()))
    }

    fn adopt_embedded_fonts(
        &self,
        _deck: DeckId,
        _owner: &super::engine::RenderOwner,
    ) -> Result<(), EngineError> {
        Ok(())
    }

    fn reset_font_metrics(&self) -> Result<(), EngineError> {
        Ok(())
    }

    fn render_all(
        &self,
        deck: DeckId,
        owner: &super::engine::RenderOwner,
        _fit_width_px: f64,
    ) -> Result<Vec<IpcValue>, EngineError> {
        self.authorize(deck, owner)?;
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        Ok((0..d.slides.len()).map(|i| Self::render(d, i)).collect())
    }

    fn render_slide(
        &self,
        deck: DeckId,
        owner: &super::engine::RenderOwner,
        index: usize,
        _fit: f64,
        _reparse: bool,
    ) -> Result<Option<IpcValue>, EngineError> {
        self.authorize(deck, owner)?;
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        Ok((index < d.slides.len()).then(|| Self::render(d, index)))
    }

    fn run_txn(
        &self,
        deck: DeckId,
        owner: &super::engine::RenderOwner,
        request: &TxnRequest,
    ) -> Result<TxnResult, EngineError> {
        self.authorize(deck, owner)?;
        self.record(json!([
            "pptx-ops.runTxn",
            request.ops.len(),
            request.dry_run
        ]));
        Ok(self.with(deck, |d| {
            let mut working = d.clone();
            let mut failures = Vec::new();
            let mut records = Vec::new();
            for (index, op) in request.ops.iter().enumerate() {
                match Self::check(&working, op) {
                    Some(error) => failures.push(TxnFailure {
                        index,
                        op: op.clone(),
                        error,
                    }),
                    None if !request.dry_run => records.push(Self::apply(&mut working, op)),
                    None => {}
                }
            }
            let blocked = match request.isolation {
                Isolation::Atomic => !failures.is_empty(),
                Isolation::PerOp => failures.len() >= request.ops.len(),
            };
            let applied = !request.dry_run && !blocked && !records.is_empty();
            if applied {
                *d = working;
            }
            TxnResult {
                applied,
                dry_run: request.dry_run,
                failures: (!failures.is_empty()).then_some(failures),
                records: (applied || request.dry_run)
                    .then_some(records)
                    .filter(|r| !r.is_empty()),
                plan: request.dry_run.then(|| IpcValue::Array(Vec::new())),
            }
        }))
    }

    fn snapshot(&self, deck: DeckId, meta_dirty: bool) -> Result<Snapshot, EngineError> {
        Ok(self.with(deck, |d| {
            let mut copy = d.clone();
            copy.meta_dirty = meta_dirty;
            Snapshot(Rc::new(copy) as Rc<dyn Any>)
        }))
    }

    fn restore(&self, deck: DeckId, snapshot: &Snapshot) -> Result<bool, EngineError> {
        let copy = snapshot
            .0
            .downcast_ref::<FakeDeck>()
            .ok_or_else(|| EngineError("invalid fake snapshot".into()))?
            .clone();
        let meta_dirty = copy.meta_dirty;
        self.with(deck, |d| {
            // the master and the id counter live in the package, which a history step restores
            // only by its entries: the fake keeps them
            let (master, next_id) = (std::mem::take(&mut d.master), d.next_id);
            let (fill, stroke) = (
                std::mem::take(&mut d.master_fill),
                std::mem::take(&mut d.master_stroke),
            );
            *d = copy;
            d.master = master;
            d.master_fill = fill;
            d.master_stroke = stroke;
            d.next_id = next_id;
        });
        Ok(meta_dirty)
    }

    fn save_bytes(&self, deck: DeckId) -> Result<Vec<u8>, EngineError> {
        self.record(json!(["pptx-engine.savePptx"]));
        Ok(self.with(deck, |d| d.to_bytes()))
    }

    fn save_to_file(&self, deck: DeckId, path: &str) -> Result<(), EngineError> {
        self.record(json!(["pptx-engine.savePptxToFile"]));
        let bytes = self.with(deck, |d| d.to_bytes());
        std::fs::write(path, bytes).map_err(|e| EngineError(e.to_string()))
    }

    fn commit_saved(&self, deck: DeckId) -> Result<(), EngineError> {
        let mut decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get_mut(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        d.dirty = false;
        Ok(())
    }
    fn commit_saved_owned(
        &self,
        deck: DeckId,
        origin: &super::engine::RenderOwner,
        destination: &super::engine::RenderOwner,
        participants: &[super::engine::RenderOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<(), EngineError> {
        let mut targets = participants.to_vec();
        targets.push(destination.clone());
        let extra = additional.iter().collect::<Vec<_>>();
        effects
            .with_owned_effect_many_extra(origin, &targets, cancel, pending, &extra, &mut || {
                self.commit_saved(deck).map_err(|error| error.0)
            })
            .map_err(EngineError)
    }
    fn commit_saved_existing_document(
        &self,
        deck: DeckId,
        owner: &super::engine::RenderOwner,
        document: &shell_contract::runtime::RendererGeneration,
        destination: &shell_contract::ipc::NativeSaveDestination,
        participants: &[super::engine::RenderOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<(), EngineError> {
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
                .is_some_and(|lease| lease.same_generation(document))
        {
            return Err(EngineError(
                "native save belongs to another presentation".into(),
            ));
        }
        let extra = additional.iter().collect::<Vec<_>>();
        effects
            .with_existing_document_destination_effect(
                destination,
                destination.path(),
                participants,
                cancel,
                pending,
                &extra,
                &mut || self.commit_saved(deck).map_err(|error| error.0),
            )
            .map_err(EngineError)
    }

    fn has_redaction(&self, deck: DeckId) -> Result<bool, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        Ok(d.slides.iter().flat_map(|s| &s.elements).any(|e| {
            self.fx
                .by("redact", &e.id)
                .as_str()
                .is_some_and(|label| !label.is_empty())
        }))
    }

    fn encrypt_ooxml(&self, bytes: &[u8], password: &str) -> Result<Vec<u8>, EngineError> {
        let mut out = format!("ENC {password}\n").into_bytes();
        out.extend_from_slice(bytes);
        Ok(out)
    }

    fn is_encrypted_ooxml(&self, bytes: &[u8]) -> Result<bool, EngineError> {
        Ok(bytes.starts_with(b"ENC "))
    }

    fn decrypt_office_file(&self, bytes: &[u8], password: &str) -> Result<Vec<u8>, EngineError> {
        let header = format!("ENC {password}\n");
        bytes
            .strip_prefix(header.as_bytes())
            .map(<[u8]>::to_vec)
            .ok_or_else(|| EngineError("wrong password".into()))
    }

    fn picture_media(
        &self,
        deck: DeckId,
        slide: usize,
        id: &str,
    ) -> Result<Option<PictureMedia>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        let present = d
            .slides
            .get(slide)
            .is_some_and(|s| s.elements.iter().any(|e| e.id == id));
        let media = self.fx.by("media", id);
        Ok(media["target"]
            .as_str()
            .filter(|t| !t.is_empty())
            .filter(|_| present)
            .map(|target| PictureMedia {
                kind: media["kind"].as_str().unwrap_or("").to_owned(),
                target: target.to_owned(),
                external: media["external"].as_bool().unwrap_or(false),
            }))
    }

    fn model3d_target(
        &self,
        deck: DeckId,
        slide: usize,
        id: &str,
    ) -> Result<Option<String>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        let present = d
            .slides
            .get(slide)
            .is_some_and(|s| s.elements.iter().any(|e| e.id == id));
        Ok(self.fx.by("models", id)["target"]
            .as_str()
            .filter(|t| !t.is_empty() && present)
            .map(str::to_owned))
    }

    fn read_part(&self, deck: DeckId, path: &str) -> Result<Option<Vec<u8>>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !decks.contains_key(&deck.0) {
            return Err(EngineError("deck is not open".into()));
        }
        Ok(self
            .fx
            .by("parts", path)
            .as_str()
            .map(|text| text.as_bytes().to_vec()))
    }

    fn copy_slide(&self, deck: DeckId, index: usize) -> Result<Option<IpcValue>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        Ok((index < d.slides.len()).then(|| Self::render(d, index)))
    }

    fn copy_elements(
        &self,
        deck: DeckId,
        slide: usize,
        ids: &[String],
    ) -> Result<Option<Vec<IpcValue>>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        Ok((|| {
            let slide = d.slides.get(slide)?;
            Some(
                ids.iter()
                    .filter_map(|id| slide.elements.iter().find(|e| &e.id == id))
                    .map(|e| {
                        obj([
                            ("id", string(e.id.as_str())),
                            ("text", string(e.text.as_str())),
                        ])
                    })
                    .collect(),
            )
        })())
    }

    fn template_cover(
        &self,
        _owner: &super::engine::RenderOwner,
        id: &str,
        lang: &str,
        _fit: f64,
    ) -> Result<Option<IpcValue>, EngineError> {
        if id != "plain" {
            return Ok(None);
        }
        // the template's sample cover goes through the page builder and opens in memory
        let spec = format!("\"{lang}\"");
        self.record(json!(["pipelines.buildPagePptx", spec.len()]));
        let mut deck = FakeDeck::blank(1);
        deck.slides[0].elements.push(FakeElement {
            id: "e0".into(),
            text: spec,
        });
        self.record(json!(["pptx-engine.openPptx", deck.to_bytes().len()]));
        Ok(Some(Self::render(&deck, 0)))
    }

    fn font_file_families(&self, path: &str) -> Result<Vec<String>, EngineError> {
        self.record(json!(["fonts.fontFileFamilies", path]));
        let name = path.rsplit('/').next().unwrap_or(path);
        Ok(vec![name.split('.').next().unwrap_or(name).to_owned()])
    }

    fn parse_file_text(&self, path: &str) -> Result<String, String> {
        self.record(json!(["file-parse.parseFileToText", path]));
        std::fs::read_to_string(path)
            .map_err(|e| main_core::node_fs::node_error(&e, "open", path).message)
    }

    fn diagram_picture_ext_of(&self, _path: &str) -> Result<Option<String>, EngineError> {
        Ok(None)
    }

    fn generate_page(
        &self,
        _owner: &super::engine::RenderOwner,
        spec_json: &str,
        _fonts: &IpcValue,
    ) -> Result<GeneratedPage, String> {
        if spec_json.is_empty() {
            return Err("empty page spec".into());
        }
        self.record(json!(["pipelines.buildPagePptx", spec_json.len()]));
        let mut deck = FakeDeck::blank(1);
        deck.slides[0].elements.push(FakeElement {
            id: "e0".into(),
            text: spec_json.to_owned(),
        });
        Ok(GeneratedPage {
            bytes: deck.to_bytes(),
            image_failures: Vec::new(),
        })
    }

    fn assemble_pages(&self, pages: &[Vec<u8>]) -> Result<Vec<u8>, EngineError> {
        let mut decks: Vec<FakeDeck> = pages
            .iter()
            .map(|p| FakeDeck::from_bytes(p).ok_or_else(|| EngineError("not a pptx".into())))
            .collect::<Result<_, _>>()?;
        let mut first = decks.remove(0);
        for (n, deck) in decks.into_iter().enumerate() {
            for mut slide in deck.slides {
                slide.path = format!("ppt/slides/slide{}.xml", first.slides.len() + n + 1);
                first.slides.push(slide);
            }
        }
        Ok(first.to_bytes())
    }

    fn extract_merge_source(&self, bytes: &[u8]) -> Result<Option<IpcValue>, EngineError> {
        Ok(FakeDeck::from_bytes(bytes).map(|d| Self::render(&d, 0)))
    }

    fn sections(&self, _deck: DeckId) -> Result<IpcValue, EngineError> {
        Ok(IpcValue::Array(
            self.fx.list("sections").iter().map(ipc_of).collect(),
        ))
    }

    fn slide_notes(&self, deck: DeckId, slide: usize) -> Result<String, EngineError> {
        Ok(self.with(deck, |d| d.slides[slide].notes.clone()))
    }

    fn slide_comments(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError> {
        Ok(self.with(deck, |d| {
            IpcValue::Array(
                d.slides[slide]
                    .comments
                    .iter()
                    .enumerate()
                    .map(|(idx, (author, text))| {
                        obj([
                            ("author", string(author.as_str())),
                            ("text", string(text.as_str())),
                            ("idx", num(idx as f64)),
                        ])
                    })
                    .collect(),
            )
        }))
    }

    fn element_link(
        &self,
        deck: DeckId,
        slide: &IpcValue,
        id: &IpcValue,
    ) -> Result<IpcValue, EngineError> {
        Ok(self.with(deck, |d| {
            let Some(index) = super::session::slide_index(slide, d.slides.len()) else {
                return IpcValue::Null;
            };
            let Some(id) = as_str(id) else {
                return IpcValue::Null;
            };
            if !d.slides[index].elements.iter().any(|e| e.id == id) {
                return IpcValue::Null;
            }
            ipc_of(self.fx.by("links", id))
        }))
    }

    fn slide_links(&self, deck: DeckId, slide: &IpcValue) -> Result<IpcValue, EngineError> {
        Ok(self.with(deck, |d| {
            let Some(index) = super::session::slide_index(slide, d.slides.len()) else {
                return IpcValue::Array(Vec::new());
            };
            IpcValue::Array(
                d.slides[index]
                    .elements
                    .iter()
                    .filter(|e| !self.fx.by("links", &e.id).is_null())
                    .map(|e| {
                        obj([
                            ("elementId", string(e.id.as_str())),
                            ("target", ipc_of(self.fx.by("links", &e.id))),
                        ])
                    })
                    .collect(),
            )
        }))
    }

    fn run_links(&self, deck: DeckId, slide: &IpcValue) -> Result<IpcValue, EngineError> {
        Ok(self.with(deck, |d| {
            let Some(index) = super::session::slide_index(slide, d.slides.len()) else {
                return IpcValue::Array(Vec::new());
            };
            let mut items = Vec::new();
            for e in &d.slides[index].elements {
                for link in self
                    .fx
                    .by("runLinks", &e.id)
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let mut entries = vec![("elementId".to_owned(), string(e.id.as_str()))];
                    if let IpcValue::Object(rest) = ipc_of(link) {
                        entries.extend(rest);
                    }
                    items.push(IpcValue::Object(entries));
                }
            }
            IpcValue::Array(items)
        }))
    }

    fn header_footer(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError> {
        Ok(self.with(deck, |d| match &d.slides[slide].header_footer {
            Some(settings) => ipc_of(settings),
            None => obj([
                ("footer", IpcValue::Null),
                ("slideNum", IpcValue::Bool(false)),
                ("date", IpcValue::Null),
            ]),
        }))
    }

    fn slide_transition(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError> {
        Ok(self.with(deck, |d| {
            string(d.slides[slide].transition.as_deref().unwrap_or("none"))
        }))
    }

    fn show_timing(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError> {
        Ok(self.with(deck, |d| {
            let s = &d.slides[slide];
            obj([
                ("advanceMs", s.advance_ms.map_or(IpcValue::Null, num)),
                (
                    "narrationIds",
                    IpcValue::Array(
                        s.elements
                            .iter()
                            .filter(|e| {
                                spid_of(&e.id).is_some_and(|spid| s.narration.contains(&spid))
                            })
                            .map(|e| string(e.id.as_str()))
                            .collect(),
                    ),
                ),
            ])
        }))
    }

    fn animations(
        &self,
        deck: DeckId,
        slide: usize,
        labels: &[(String, String)],
    ) -> Result<IpcValue, EngineError> {
        let label = |kind: &str| {
            labels
                .iter()
                .find(|(k, _)| k == kind)
                .or_else(|| labels.iter().find(|(k, _)| k == "object"))
                .map(|(_, text)| text.clone())
                .unwrap_or_default()
        };
        Ok(self.with(deck, |d| {
            let s = &d.slides[slide];
            let mut out = Vec::new();
            for a in &s.animations {
                // the first element with the animation's spid; a deleted target is not echoed back
                let Some(spid) = a["spid"].as_i64() else {
                    continue;
                };
                let Some(el) = s.elements.iter().find(|e| spid_of(&e.id) == Some(spid)) else {
                    continue;
                };
                let mut entries = vec![
                    ("sourceId".to_owned(), string(el.id.as_str())),
                    ("targetName".to_owned(), string(label(self.kind_of(&el.id)))),
                    ("effect".to_owned(), ipc_of(&a["effect"])),
                    ("trigger".to_owned(), ipc_of(&a["trigger"])),
                    ("durationMs".to_owned(), ipc_of(&a["durationMs"])),
                    ("delayMs".to_owned(), ipc_of(&a["delayMs"])),
                ];
                for key in ["direction", "motionPath", "paragraph"] {
                    if !a[key].is_null() {
                        entries.push((key.to_owned(), ipc_of(&a[key])));
                    }
                }
                out.push(IpcValue::Object(entries));
            }
            IpcValue::Array(out)
        }))
    }

    fn shape_keys(&self, deck: DeckId, slide: usize) -> Result<IpcValue, EngineError> {
        Ok(self.with(deck, |d| {
            IpcValue::Array(
                d.slides[slide]
                    .elements
                    .iter()
                    .map(|e| {
                        obj([
                            ("sourceId", string(e.id.as_str())),
                            (
                                "spid",
                                spid_of(&e.id).map_or(IpcValue::Undefined, |s| num(s as f64)),
                            ),
                            ("name", string("")),
                        ])
                    })
                    .collect(),
            )
        }))
    }

    fn has_narration(&self, deck: DeckId, slide: usize) -> Result<bool, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        let slide = d
            .slides
            .get(slide)
            .ok_or_else(|| EngineError("slide is not open".into()))?;
        Ok(!slide.narration.is_empty())
    }

    fn section_slide_indices(
        &self,
        deck: DeckId,
        id: &IpcValue,
    ) -> Result<Option<Vec<usize>>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let total = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?
            .slides
            .len();
        let sections = self.fx.list("sections");
        let (lead, ranges) = normalize_sections(&sections, total);
        if matches!(id, IpcValue::Undefined | IpcValue::Null) {
            return Ok(Some(lead));
        }
        Ok(sections
            .iter()
            .position(|s| &ipc_of(&s["id"]) == id)
            .map(|at| ranges[at].clone()))
    }

    fn master_parts(&self, deck: DeckId) -> Result<Vec<super::engine::MasterPart>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !decks.contains_key(&deck.0) {
            return Err(EngineError("deck is not open".into()));
        }
        Ok(vec![super::engine::MasterPart {
            part_path: MASTER_PART.into(),
            kind: string("master"),
            name: string("Office Theme"),
        }])
    }

    fn master_parse(
        &self,
        deck: DeckId,
        owner: &super::engine::RenderOwner,
        part: &str,
        fit: f64,
    ) -> Result<Option<IpcValue>, EngineError> {
        self.master_render(deck, owner, part, fit)
    }

    fn master_render(
        &self,
        deck: DeckId,
        _owner: &super::engine::RenderOwner,
        part: &str,
        _fit: f64,
    ) -> Result<Option<IpcValue>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        Ok((part == MASTER_PART).then(|| {
            obj([
                ("partPath", string(MASTER_PART)),
                (
                    "elements",
                    IpcValue::Array(
                        d.master
                            .iter()
                            .map(|e| {
                                let mut entries = vec![
                                    ("id".to_owned(), string(e.id.as_str())),
                                    ("text".to_owned(), string(e.text.as_str())),
                                ];
                                if let Some(fill) = d.master_fill.get(&e.id) {
                                    entries.push(("fill".to_owned(), fill.clone()));
                                }
                                if let Some(stroke) = d.master_stroke.get(&e.id) {
                                    entries.push(("stroke".to_owned(), stroke.clone()));
                                }
                                IpcValue::Object(entries)
                            })
                            .collect(),
                    ),
                ),
            ])
        }))
    }

    fn master_has_element(&self, deck: DeckId, part: &str, id: &str) -> Result<bool, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        Ok(part == MASTER_PART && d.master.iter().any(|e| e.id == id))
    }

    fn master_preview_transform(
        &self,
        deck: DeckId,
        _part: &str,
        _id: &str,
        _offset: [f64; 4],
        _rot: f64,
    ) -> Result<(), EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !decks.contains_key(&deck.0) {
            return Err(EngineError("deck is not open".into()));
        }
        Ok(())
    }

    fn image_info(
        &self,
        bytes: &[u8],
        ext: &str,
        _path: Option<&str>,
    ) -> Result<super::engine::ImageInfo, EngineError> {
        // A tiff's decoder rejects the fixture bytes; other images use the fake desktop.
        if ext == "tif" || ext == "tiff" {
            return Ok(super::engine::ImageInfo::default());
        }
        self.record(json!(["nativeImage.info", bytes.len()]));
        Ok(super::engine::ImageInfo {
            natural: (!bytes.is_empty()).then_some((640.0, 480.0)),
            dpi: None,
        })
    }

    fn video_poster(&self, _path: &str) -> Result<Option<Vec<u8>>, EngineError> {
        Ok(None)
    }

    fn background_image(
        &self,
        _deck: DeckId,
        _slide: usize,
    ) -> Result<Option<String>, EngineError> {
        Ok(None)
    }

    fn slide_index_of_id(
        &self,
        deck: DeckId,
        slide_id: &str,
    ) -> Result<Option<usize>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        Ok(slide_id
            .strip_prefix("slide-")
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| *n < d.slides.len()))
    }

    fn materialize_slide(&self, deck: DeckId, index: usize) -> Result<(), EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        let d = decks
            .get(&deck.0)
            .ok_or_else(|| EngineError("deck is not open".into()))?;
        if index >= d.slides.len() {
            return Err(EngineError("slide is not open".into()));
        }
        Ok(())
    }

    fn reparse_deck(&self, deck: DeckId) -> Result<(), EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !decks.contains_key(&deck.0) {
            return Err(EngineError("deck is not open".into()));
        }
        Ok(())
    }

    fn autofit_resize(
        &self,
        deck: DeckId,
        _owner: &super::engine::RenderOwner,
        _index: usize,
        _id: &str,
        rendered: Option<IpcValue>,
        _fit: f64,
    ) -> Result<Option<IpcValue>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !decks.contains_key(&deck.0) {
            return Err(EngineError("deck is not open".into()));
        }
        Ok(rendered)
    }

    fn sync_autofit_scale(
        &self,
        deck: DeckId,
        _owner: &super::engine::RenderOwner,
        _index: usize,
        _id: &str,
        rendered: Option<IpcValue>,
        _fit: f64,
    ) -> Result<Option<IpcValue>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !decks.contains_key(&deck.0) {
            return Err(EngineError("deck is not open".into()));
        }
        Ok(rendered)
    }

    fn map_script_ops(
        &self,
        deck: DeckId,
        script: &IpcValue,
    ) -> Result<Vec<IpcValue>, EngineError> {
        if !self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?
            .contains_key(&deck.0)
        {
            return Err(EngineError("deck is not open".into()));
        }
        let IpcValue::Array(steps) = field(script, "steps") else {
            return Ok(Vec::new());
        };
        Ok(steps
            .iter()
            .map(|step| {
                let mut target = vec![("slide".to_owned(), field(script, "slideIndex").clone())];
                if !matches!(field(step, "el"), IpcValue::Undefined) {
                    target.push(("el".to_owned(), field(step, "el").clone()));
                }
                let mut op = vec![
                    (
                        "op".to_owned(),
                        string(as_str(field(step, "op")).unwrap_or("")),
                    ),
                    ("target".to_owned(), IpcValue::Object(target)),
                ];
                if !matches!(field(step, "paragraphs"), IpcValue::Undefined) {
                    op.push(("paragraphs".to_owned(), field(step, "paragraphs").clone()));
                }
                IpcValue::Object(op)
            })
            .collect())
    }

    fn group_child_frame(
        &self,
        deck: DeckId,
        _slide: usize,
        _group_id: &str,
        _source_id: &str,
    ) -> Result<Option<super::engine::GroupChildFrame>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !decks.contains_key(&deck.0) {
            return Err(EngineError("deck is not open".into()));
        }
        Ok(None)
    }

    fn element_index(
        &self,
        deck: DeckId,
        slide: usize,
        source_id: &str,
    ) -> Result<Option<usize>, EngineError> {
        Ok(self.with(deck, |d| {
            d.slides
                .get(slide)?
                .elements
                .iter()
                .position(|e| e.id == source_id)
        }))
    }

    fn element_id_at(
        &self,
        deck: DeckId,
        slide: usize,
        index: usize,
    ) -> Result<Option<String>, EngineError> {
        Ok(self.with(deck, |d| {
            d.slides
                .get(slide)?
                .elements
                .get(index)
                .map(|e| e.id.clone())
        }))
    }

    fn is_imported_chart(
        &self,
        _deck: DeckId,
        _slide: usize,
        _id: &str,
    ) -> Result<bool, EngineError> {
        Ok(false)
    }

    fn theme_accents(&self, _deck: DeckId) -> Result<Option<Vec<String>>, EngineError> {
        Ok(None)
    }

    fn chart_data(&self, _deck: DeckId, _slide: usize, id: &str) -> Result<IpcValue, EngineError> {
        Ok(ipc_of(self.fx.by("charts", id)))
    }

    fn layouts(&self, _deck: DeckId) -> Result<IpcValue, EngineError> {
        Ok(IpcValue::Array(Vec::new()))
    }

    fn ensure_builtin_layout(
        &self,
        deck: DeckId,
        key: &str,
    ) -> Result<Option<String>, EngineError> {
        let decks = self
            .decks
            .lock()
            .map_err(|error| EngineError(error.to_string()))?;
        if !decks.contains_key(&deck.0) {
            return Err(EngineError("deck is not open".into()));
        }
        Ok(Some(format!("ppt/slideLayouts/builtin-{key}.xml")))
    }

    fn private_font_faces(
        &self,
        owner: &super::engine::RenderOwner,
    ) -> Result<IpcValue, EngineError> {
        if !owner
            .context
            .renderer()
            .is_some_and(|generation| generation.is_active())
        {
            return Err(EngineError("renderer is unavailable".into()));
        }
        Ok(IpcValue::Array(
            self.fx.list("fontFaces").iter().map(ipc_of).collect(),
        ))
    }

    fn private_font_data(
        &self,
        owner: &super::engine::RenderOwner,
        id: &IpcValue,
    ) -> Result<IpcValue, EngineError> {
        if !owner
            .context
            .renderer()
            .is_some_and(|generation| generation.is_active())
        {
            return Err(EngineError("renderer is unavailable".into()));
        }
        Ok(as_str(id).map_or(IpcValue::Null, |id| ipc_of(self.fx.by("fontData", id))))
    }
}

#[derive(Default)]
struct SyntheticGeneratedState {
    observer: Option<shell_contract::hosts::GeneratedPathObserver>,
    wake: Option<shell_contract::hosts::GeneratedPathWake>,
    result: Option<Result<shell_contract::hosts::GeneratedPathOutcome, String>>,
    observed: bool,
    in_flight: bool,
    acknowledged: bool,
    cancelling: bool,
}

#[derive(Clone, Default)]
struct SyntheticGeneratedTask(std::sync::Arc<Mutex<SyntheticGeneratedState>>);

impl SyntheticGeneratedTask {
    fn deliver(&self) {
        let delivery = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            if state.in_flight || state.observer.is_none() || state.result.is_none() {
                return;
            }
            let (Some(observer), Some(result)) = (state.observer.take(), state.result.take())
            else {
                return;
            };
            state.in_flight = true;
            (observer, result)
        };
        (delivery.0)(shell_contract::hosts::GeneratedTerminalDelivery::new(
            delivery.1,
            std::sync::Arc::new(self.clone()),
        ));
    }
}

impl shell_contract::hosts::GeneratedTerminalAck for SyntheticGeneratedTask {
    fn complete(&self) -> Result<(), String> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if !state.in_flight || state.acknowledged {
            return Err("The synthetic result is not awaiting acknowledgement".into());
        }
        state.in_flight = false;
        state.acknowledged = true;
        Ok(())
    }
    fn restore(&self, result: Result<shell_contract::hosts::GeneratedPathOutcome, String>) {
        let wake = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            if !state.in_flight || state.acknowledged {
                return;
            }
            state.in_flight = false;
            state.result = Some(result);
            state.wake.clone()
        };
        if let Some(wake) = wake {
            wake();
        }
    }
}

impl shell_contract::hosts::GeneratedPathPending for SyntheticGeneratedTask {
    fn observe(
        &self,
        observer: shell_contract::hosts::GeneratedPathObserver,
        wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<(), shell_contract::hosts::GeneratedObserveFailure> {
        {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            if state.observed {
                return Err(shell_contract::hosts::GeneratedObserveFailure {
                    error: "The synthetic observer was already installed".into(),
                    observer,
                    wake,
                });
            }
            state.observed = true;
            state.observer = Some(observer);
            state.wake = Some(wake);
        }
        self.deliver();
        Ok(())
    }
    fn wake_observer(&self) -> Result<(), String> {
        let wake = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .wake
            .clone()
            .ok_or("The synthetic observer is unavailable")?;
        wake();
        Ok(())
    }
    fn take_redelivery(
        &self,
    ) -> Result<Option<shell_contract::hosts::GeneratedTerminalDelivery>, String> {
        let result = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            if state.in_flight || state.acknowledged {
                return Ok(None);
            }
            let result = state.result.take();
            if result.is_some() {
                state.in_flight = true;
            }
            result
        };
        Ok(result.map(|result| {
            shell_contract::hosts::GeneratedTerminalDelivery::new(
                result,
                std::sync::Arc::new(self.clone()),
            )
        }))
    }
    fn is_cancelling(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancelling
    }
    fn retry(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        done(self.wake_observer());
    }
    fn cancel_and_drain(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        let result = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            if state.in_flight {
                Err("The synthetic terminal result is awaiting acknowledgement".into())
            } else {
                state.cancelling = true;
                if !state.acknowledged && state.result.is_none() {
                    state.result =
                        Some(Err("The synthetic generated operation was cancelled".into()));
                }
                Ok(())
            }
        };
        if let Err(error) = result {
            return done(Err(error));
        }
        self.deliver();
        done(Ok(()));
    }
}

struct PreparedSyntheticGenerated {
    task: std::sync::Arc<SyntheticGeneratedTask>,
    action:
        Option<Box<dyn FnOnce() -> Result<shell_contract::hosts::GeneratedPathOutcome, String>>>,
}
impl shell_contract::hosts::PreparedGeneratedPath for PreparedSyntheticGenerated {
    fn task(&self) -> std::sync::Arc<dyn shell_contract::hosts::GeneratedPathPending> {
        self.task.clone()
    }
    fn start(mut self: Box<Self>) -> Result<(), shell_contract::hosts::GeneratedStartFailure> {
        let ready = {
            let state = self
                .task
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.observed && !state.in_flight && !state.acknowledged && state.result.is_none()
        };
        if !ready {
            return Err(shell_contract::hosts::GeneratedStartFailure {
                error: "The synthetic operation is not ready to start".into(),
                prepared: self,
            });
        }
        let Some(action) = self.action.take() else {
            return Err(shell_contract::hosts::GeneratedStartFailure {
                error: "The synthetic operation already started".into(),
                prepared: self,
            });
        };
        let result = action();
        self.task
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .result = Some(result);
        self.task.deliver();
        Ok(())
    }
}

impl Drop for PreparedSyntheticGenerated {
    fn drop(&mut self) {
        if self.action.is_none() {
            return;
        }
        {
            let mut state = self
                .task
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !state.in_flight && !state.acknowledged && state.result.is_none() {
                state.result = Some(Err("The synthetic generated operation was cancelled".into()));
            }
        }
        self.task.deliver();
    }
}

/// A prepared operation that runs nothing until the shell starts it and then
/// delivers its terminal result to the observer exactly once.
pub(crate) fn synthetic_generated(
    action: Box<dyn FnOnce() -> Result<shell_contract::hosts::GeneratedPathOutcome, String>>,
) -> Box<dyn shell_contract::hosts::PreparedGeneratedPath> {
    Box::new(PreparedSyntheticGenerated {
        task: std::sync::Arc::new(SyntheticGeneratedTask::default()),
        action: Some(action),
    })
}

/// A shell that records what the handlers asked of it
pub struct FakeShell {
    log: Option<CallLog>,
    pub displays: Vec<Display>,
    pub prepared: Mutex<Option<PreparedOpen>>,
    pub microphone: bool,
    pub project_dir: Option<String>,
    pub written: Mutex<Vec<(String, Vec<u8>)>>,
    pub opened: Mutex<Vec<(ViewId, String)>>,
    pub markers: Mutex<HashMap<String, Vec<u8>>>,
    /// the windows in simple fullscreen: the snap is asked only when it changes anything
    pub snapped: Mutex<std::collections::HashSet<u32>>,
    /// the hand-off a case left for the tab, taken once
    pub handoff: Mutex<Option<TakenHandoff>>,
    pub uuids: std::sync::atomic::AtomicU64,
    pub recovery_password: Option<String>,
    saved_effects: Option<shell_contract::hosts::OwnedEffectsHost>,
    pub audience_owner: Mutex<Option<super::engine::RenderOwner>>,
    pub closed_audiences: Mutex<Vec<WindowId>>,
}

impl FakeShell {
    fn record(&self, call: Value) {
        if let Some(log) = &self.log {
            log.lock().unwrap().push(call);
        }
    }
    fn set_bleed(&self, view: ViewId, on: bool) {
        self.record(json!(["showChrome.setBleed", view.0, on]));
    }
    fn show_fullscreen_window(&self, window: WindowId, on: bool) {
        let mut snapped = self.snapped.lock().unwrap();
        let changed = if on {
            snapped.insert(window.0)
        } else {
            snapped.remove(&window.0)
        };
        if changed {
            self.record(json!(["showFullscreenWindow", window.0, on]));
        }
    }
    fn fullscreen_on_display(&self, window: WindowId, display: Display) {
        self.record(json!(["fullScreenOnDisplay", window.0, display.id]));
    }
    fn move_presenter_window(&self, window: WindowId, display: Display) {
        self.record(json!(["movePresenterWindow", window.0, display.id]));
    }
    fn export_pdf(&self, op: &IpcValue) -> PdfExportAnswer {
        let path = as_str(field(op, "filePath")).map(str::to_owned);
        self.record(json!(["exportSlidesPdf", path]));
        if let Some(path) = &path {
            let _ = std::fs::write(path, b"%PDF-1.7");
        }
        PdfExportAnswer {
            ok: path.is_some(),
            path,
            error: None,
        }
    }
}

impl SlidesShell for FakeShell {
    fn opened(&self, view: ViewId, path: &str) {
        self.record(json!(["slidesOpenedHook", view.0, path]));
        self.opened.lock().unwrap().push((view, path.to_owned()));
    }
    fn reload_view(&self, view: ViewId) {
        self.record(json!(["slides.reload", view.0]));
    }
    fn dialog_parent(&self) -> Option<WindowId> {
        None
    }
    fn host_window_for(&self, view: ViewId) -> Option<WindowId> {
        Some(WindowId(view.0))
    }
    fn is_headless(&self) -> bool {
        false
    }
    fn new_document_dir(&self, _view: ViewId) -> Option<String> {
        self.project_dir.clone()
    }
    fn can_write_new_document(&self) -> bool {
        self.project_dir.is_some()
    }
    fn is_path_open_elsewhere(&self, _path: &str) -> Result<bool, EngineError> {
        Ok(false)
    }
    fn take_headless_target_owned(
        &self,
        _owner: &super::engine::RenderOwner,
        _cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Option<super::engine::HeadlessExportTarget>, String> {
        Ok(None)
    }
    fn headless_export_done_owned(
        &self,
        _owner: &super::engine::RenderOwner,
        _cancel: &shell_contract::runtime::CancelToken,
        _ok: bool,
        _error: Option<String>,
    ) -> Result<(), String> {
        Ok(())
    }
    fn read_clipboard_buffer(&self, format: &str) -> Result<Vec<u8>, EngineError> {
        Ok(self
            .markers
            .lock()
            .unwrap()
            .get(format)
            .cloned()
            .unwrap_or_default())
    }
    fn write_clipboard_buffer(&self, format: &str, bytes: &[u8]) {
        self.record(json!(["clipboard.writeBuffer", format, bytes.len()]));
        self.markers
            .lock()
            .unwrap()
            .insert(format.to_owned(), bytes.to_vec());
    }
    fn normalize_png(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        (!bytes.is_empty()).then(|| bytes.to_vec())
    }
    fn write_image_and_html(&self, _png: &[u8], html: &str) {
        self.record(json!(["clipboard.write", {"html": html, "text": null}]));
    }
    fn native_clipboard(&self, view: ViewId, op: &str) {
        self.record(json!(["webContents.clipboard", view.0, op]));
    }
    fn prepare_for_open_owned(
        &self,
        _parent: Option<WindowId>,
        path: &str,
        _source: super::engine::RenderOwner,
        _destination: super::engine::RenderOwner,
        _cancel: shell_contract::runtime::CancelToken,
        _pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<PreparedOpen, String>) + Send>,
    ) {
        let prepared = self
            .prepared
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| PreparedOpen::Plain(path.to_owned()));
        done(Ok(prepared))
    }
    fn prepare_for_recovery_open(
        &self,
        _target: shell_contract::ipc::RecoveryOpenTarget,
        _snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        path: String,
        done: Box<dyn FnOnce(Result<PreparedOpen, String>) + Send>,
    ) {
        let prepared = self
            .prepared
            .lock()
            .unwrap()
            .take()
            .unwrap_or(PreparedOpen::Plain(path));
        done(Ok(prepared));
    }
    fn unlock_recovery_copy_prepared(
        &self,
        _target: shell_contract::ipc::RecoveryOpenTarget,
        _snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        _title: String,
        bytes: Vec<u8>,
        done: Box<dyn FnOnce(Result<Option<(Vec<u8>, String)>, String>) + Send>,
    ) {
        let Some(password) = self.recovery_password.clone() else {
            return done(Ok(None));
        };
        let prefix = format!("ENC {password}\n");
        done(
            bytes
                .strip_prefix(prefix.as_bytes())
                .map(|plain| Some((plain.to_vec(), password)))
                .ok_or_else(|| "wrong password".to_owned()),
        );
    }
    fn unlock_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        _title: String,
        done: Box<dyn FnOnce(Result<Option<(Vec<u8>, String)>, String>) + Send>,
    ) {
        let Some(effects) = &self.saved_effects else {
            return done(Err("Saved document effects are unavailable".into()));
        };
        let target = document.target();
        let additional: Vec<_> = target.additional().iter().collect();
        let mut bytes = None;
        let admitted = effects.with_native_saved_document_input(
            &document,
            target.participants(),
            target.cancel(),
            target.pending(),
            &additional,
            &mut |input| {
                bytes = Some(input.clone());
                Ok(())
            },
        );
        if let Err(error) = admitted {
            return done(Err(error));
        }
        let Some(input) = bytes else {
            return done(Err("Saved document input was not admitted".into()));
        };
        let Some(password) = self.recovery_password.clone() else {
            return done(Ok(None));
        };
        let prefix = format!("ENC {password}\n");
        let result = input
            .bytes()
            .strip_prefix(prefix.as_bytes())
            .map(|plain| Some((plain.to_vec(), password)))
            .ok_or_else(|| "wrong password".to_owned());
        let admitted = effects.with_native_saved_document_input(
            &document,
            target.participants(),
            target.cancel(),
            target.pending(),
            &additional,
            &mut |_| Ok(()),
        );
        done(admitted.and(result));
    }
    fn export_pdf_owned(
        &self,
        op: &IpcValue,
        origin: &super::engine::RenderOwner,
        destination: &super::engine::RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        participants: &[super::engine::RenderOwner],
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> PdfExportAnswer {
        let mut owners = participants.to_vec();
        owners.push(destination.clone());
        let extra = additional.iter().collect::<Vec<_>>();
        let mut answer = None;
        match effects.with_owned_effect_many_extra(
            origin,
            &owners,
            cancel,
            pending,
            &extra,
            &mut || {
                answer = Some(self.export_pdf(op));
                Ok(())
            },
        ) {
            Ok(()) => answer.unwrap_or_else(|| PdfExportAnswer {
                ok: false,
                path: None,
                error: Some("The PDF export was not admitted".into()),
            }),
            Err(error) => PdfExportAnswer {
                ok: false,
                path: None,
                error: Some(error),
            },
        }
    }
    fn export_pdf_native_invocation(
        &self,
        op: &IpcValue,
        document: &shell_contract::ipc::NativeInvocationDocument,
        origin: &super::engine::RenderOwner,
        destination: &super::engine::RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        participants: &[super::engine::RenderOwner],
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> PdfExportAnswer {
        let mut owners = participants.to_vec();
        owners.push(destination.clone());
        let extra = additional.iter().collect::<Vec<_>>();
        let mut answer = None;
        match effects.with_native_invocation_effect_extra(
            document,
            origin,
            &owners,
            cancel,
            pending,
            &extra,
            &mut || {
                answer = Some(self.export_pdf(op));
                Ok(())
            },
        ) {
            Ok(()) => answer.unwrap_or_else(|| PdfExportAnswer {
                ok: false,
                path: None,
                error: Some("The PDF export was not admitted".into()),
            }),
            Err(error) => PdfExportAnswer {
                ok: false,
                path: None,
                error: Some(error),
            },
        }
    }
    fn export_pdf_existing_document(
        &self,
        op: &IpcValue,
        destination: &shell_contract::ipc::NativeSaveDestination,
        participants: &[super::engine::RenderOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> PdfExportAnswer {
        let extra = additional.iter().collect::<Vec<_>>();
        let mut answer = None;
        match effects.with_existing_document_destination_effect(
            destination,
            destination.path(),
            participants,
            cancel,
            pending,
            &extra,
            &mut || {
                answer = Some(self.export_pdf(op));
                Ok(())
            },
        ) {
            Ok(()) => answer.unwrap_or_else(|| PdfExportAnswer {
                ok: false,
                path: None,
                error: Some("The PDF export was not admitted".into()),
            }),
            Err(error) => PdfExportAnswer {
                ok: false,
                path: None,
                error: Some(error),
            },
        }
    }
    fn write_new_document_owned(
        &self,
        _origin: &super::engine::RenderOwner,
        _destination: &super::engine::RenderOwner,
        _cancel: &shell_contract::runtime::CancelToken,
        _pending: &shell_contract::runtime::CancelToken,
        _effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        name: &str,
        bytes: &[u8],
    ) -> Result<Option<String>, String> {
        let Some(dir) = &self.project_dir else {
            return Ok(None);
        };
        let path = format!("{dir}/{name}");
        main_core::packages::electron_utils::atomic_write::owned::write_file(
            &path, bytes, admission,
        )
        .map_err(|error| error.message)?;
        self.written
            .lock()
            .unwrap()
            .push((path.clone(), bytes.to_vec()));
        Ok(Some(path))
    }
    fn file_saved_owned(
        &self,
        origin: &super::engine::RenderOwner,
        destination: &super::engine::RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
        path: std::path::PathBuf,
        participants: &[super::engine::RenderOwner],
        additional: &[shell_contract::runtime::CancelToken],
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
    ) {
        let mut owners = participants.to_vec();
        owners.push(destination.clone());
        let extra = additional.iter().collect::<Vec<_>>();
        let result = effects
            .with_owned_effect_many_extra(origin, &owners, cancel, pending, &extra, &mut || {
                self.opened(destination.view, &path.to_string_lossy());
                Ok(())
            })
            .map(|()| shell_contract::hosts::SavedFile {
                path,
                session: shell_contract::hosts::SavedSessionReceipt::NoPublication,
            });
        done(result);
    }
    fn file_saved_existing_document(
        &self,
        revision: shell_contract::ipc::NativeSaveRevisionAuthority,
        expected_path: std::path::PathBuf,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        _on_revision: std::rc::Rc<
            dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>,
        >,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<std::path::PathBuf>, String>)>,
    ) {
        let extra = additional.iter().collect::<Vec<_>>();
        let result = revision.snapshot().and_then(|snapshot| {
            if snapshot.selection().path() != expected_path {
                return Err("The native save path changed".into());
            }
            effects
                .with_native_save_revision_snapshot_effect(
                    &snapshot,
                    &participants,
                    &cancel,
                    &pending,
                    &extra,
                    &mut || {
                        let recipient = participants
                            .first()
                            .ok_or("The saved presentation recipient is missing")?;
                        self.opened(recipient.view, &expected_path.to_string_lossy());
                        Ok(())
                    },
                )
                .map(|()| shell_contract::hosts::SavedFile {
                    path: expected_path,
                    session: shell_contract::hosts::SavedSessionReceipt::NoPublication,
                })
        });
        done(result);
    }
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
    ) {
        let extra = additional.iter().collect::<Vec<_>>();
        let result = effects.with_existing_document_destination_effect(
            &destination,
            destination.path(),
            &participants,
            &cancel,
            &pending,
            &extra,
            &mut || {
                let recipient = participants
                    .first()
                    .ok_or("The saved presentation recipient is missing")?;
                self.set_window_title(recipient.view, &title);
                Ok(())
            },
        );
        done(result);
    }
    fn ask_microphone(&self, done: Box<dyn FnOnce(Result<bool, String>) + Send>) {
        // the status is asked first; only a status that is not `granted` is asked about
        self.record(json!([
            "systemPreferences.getMediaAccessStatus",
            "microphone"
        ]));
        if !self.microphone {
            self.record(json!(["systemPreferences.askForMediaAccess", "microphone"]));
        }
        done(Ok(self.microphone))
    }
    fn displays(&self) -> Result<Vec<Display>, EngineError> {
        Ok(self.displays.clone())
    }
    fn display_of(&self, window: Option<WindowId>) -> Result<Display, EngineError> {
        let first = self.displays[0];
        Ok(match window {
            Some(w) if w.0 >= 100 => self.displays.get(1).copied().unwrap_or(first),
            _ => first,
        })
    }
    fn open_audience_window(
        &self,
        display: Display,
        _source: super::engine::RenderOwner,
        _cancel: shell_contract::runtime::CancelToken,
        done: Box<
            dyn FnOnce(Result<Option<(super::engine::RenderOwner, WindowId)>, EngineError>) + Send,
        >,
    ) {
        self.record(json!(["BrowserWindow.audience", display.id]));
        done(match self.audience_owner.lock().unwrap().take() {
            Some(owner) => Ok(Some((owner, WindowId(100)))),
            None => {
                self.close_audience_window(WindowId(100));
                Err(EngineError(
                    "audience renderer capability unavailable".into(),
                ))
            }
        })
    }
    fn close_audience_window(&self, window: WindowId) {
        self.record(json!(["closeAudienceWindow", window.0]));
        self.closed_audiences.lock().unwrap().push(window);
    }
    fn set_window_title(&self, _view: ViewId, _title: &str) {
        // only a detached editor window has a title the shell sets; the cases have none
    }
    fn prepare_reveal_export(
        &self,
        output: shell_contract::ipc::SavedPublication,
        written: Option<shell_contract::ipc::NativeSavedInput>,
        participants: Vec<super::engine::RenderOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        _accepted: shell_contract::hosts::PdfReloadAccepted,
        _wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
        if written
            .as_ref()
            .is_some_and(|input| !input.output().same_publication(&output))
        {
            return Err("The synthetic output bytes belong to another publication".into());
        }
        let action = Box::new(move || {
            let extra: Vec<_> = additional.iter().collect();
            output.with_effect(
                &*effects,
                &participants,
                &cancel,
                &pending,
                &extra,
                &mut || Ok(()),
            )?;
            Ok(shell_contract::hosts::GeneratedPathOutcome::NotOpened)
        });
        Ok(synthetic_generated(action))
    }
    fn set_window_title_owned(
        &self,
        origin: &super::engine::RenderOwner,
        destination: &super::engine::RenderOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        title: String,
        effects: &shell_contract::hosts::OwnedEffectsHost,
        participants: &[super::engine::RenderOwner],
        additional: &[shell_contract::runtime::CancelToken],
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let mut owners = participants.to_vec();
        owners.push(destination.clone());
        let extra = additional.iter().collect::<Vec<_>>();
        let result = effects.with_owned_effect_many_extra(
            origin,
            &owners,
            cancel,
            pending,
            &extra,
            &mut || {
                self.set_window_title(destination.view, &title);
                Ok(())
            },
        );
        done(result);
    }
    fn presenter_window_owned(
        &self,
        _origin: &super::engine::RenderOwner,
        destination: &super::engine::RenderOwner,
        _cancel: &shell_contract::runtime::CancelToken,
        _pending: &shell_contract::runtime::CancelToken,
        action: super::engine::PresenterWindowAction,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        use super::engine::PresenterWindowAction;
        match action {
            PresenterWindowAction::ShowFullscreen { window, on } => {
                self.set_bleed(destination.view, on);
                self.show_fullscreen_window(window, on);
            }
            PresenterWindowAction::Refocus { .. } => {}
            PresenterWindowAction::SwapDisplays {
                presenter,
                audience,
                presenter_display,
                audience_display,
            } => {
                self.fullscreen_on_display(audience, presenter_display);
                self.move_presenter_window(presenter, audience_display);
            }
        }
        done(Ok(()));
    }
    fn take_generate_handoff_owned(
        &self,
        consumer: &super::engine::RenderOwner,
        _cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Option<TakenHandoff>, String> {
        let view = consumer.view;
        self.record(json!([
            "generate-handoff.takeGenerateHandoff",
            "slides",
            view.0
        ]));
        Ok(self.handoff.lock().unwrap().take())
    }
    fn report_generate_run_started_owned(
        &self,
        consumer: &super::engine::RenderOwner,
        _cancel: &shell_contract::runtime::CancelToken,
        paths: &[String],
    ) -> Result<bool, String> {
        let view = consumer.view;
        self.record(json!([
            "generate-handoff.reportGenerateRunStarted",
            "slides",
            view.0,
            paths
        ]));
        Ok(true)
    }
    fn random_uuid(&self) -> String {
        let n = self.uuids.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        format!("00000000-0000-4000-8000-{n:012}")
    }
    fn assert_file_saved(&self, _path: String, then: Box<dyn FnOnce(Result<(), String>) + Send>) {
        then(Ok(()));
    }
}

/// This app's fake hosts: the engine and the shell, answering from the
/// case's `hosts.d1.slides` value (blankSlides, projectDir, microphone,
/// displays) with fixed defaults when absent
pub fn insert(hosts: &mut shell_contract::Hosts, case: &Value) {
    let log = hosts.get::<CallLog>().cloned();
    let slides = &case["slides"];
    super::recovery::FIXED_CLOCK.store(
        slides["clock"].as_u64().unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );
    let engine: SlidesEngineHost = Arc::new(FakeEngine {
        decks: Mutex::default(),
        bindings: Mutex::default(),
        next: Mutex::default(),
        blank_slides: slides["blankSlides"].as_u64().unwrap_or(1) as usize,
        fx: Fixtures(slides["fixtures"].clone()),
        log: log.clone(),
    });
    hosts.insert::<SlidesEngineHost>(engine);
    let display = |id: i64| Display {
        id,
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1080.0,
    };
    let shell = Arc::new(FakeShell {
        log,
        displays: if slides["externalDisplay"].as_bool().unwrap_or(false) {
            vec![display(1), display(2)]
        } else {
            vec![display(1)]
        },
        prepared: Mutex::default(),
        microphone: slides["microphone"].as_bool().unwrap_or(true),
        project_dir: slides["projectDir"].as_str().map(str::to_owned),
        written: Mutex::default(),
        opened: Mutex::default(),
        markers: Mutex::default(),
        snapped: Mutex::default(),
        handoff: Mutex::new(slides["handoff"].as_object().map(|h| {
            TakenHandoff {
                id: h["id"].as_str().unwrap_or("").to_owned(),
                text: h["text"].as_str().unwrap_or("").to_owned(),
                materials: h["materials"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|m| {
                                (
                                    m["path"].as_str().unwrap_or("").to_owned(),
                                    m["name"].as_str().unwrap_or("").to_owned(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })),
        uuids: Default::default(),
        recovery_password: slides["recoveryPassword"].as_str().map(str::to_owned),
        saved_effects: hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned(),
        audience_owner: Mutex::new(None),
        closed_audiences: Mutex::new(Vec::new()),
    });
    hosts.insert::<Arc<FakeShell>>(shell.clone());
    hosts.insert::<SlidesShellHost>(shell);
    let _ = num;
}

/// The text of a paragraph list: the runs' text joined, a line per paragraph
fn paragraphs_text(paragraphs: &IpcValue) -> String {
    match paragraphs {
        IpcValue::Array(items) => items
            .iter()
            .map(|paragraph| match field(paragraph, "runs") {
                IpcValue::Array(runs) => runs
                    .iter()
                    .filter_map(|r| as_str(field(r, "text")))
                    .collect::<String>(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
