//! Request-scoped slide font metrics. Native code owns the font sources and
//! embedded faces; the untrusted render service supplies only font queries.
mod discovery;
mod dispatch;
mod family_scan;
mod kerning;
mod metrics;
mod policy;
mod registry;
mod renderer;

use metrics::{FontFace, Metrics};
use policy::Script;
use registry::FontSources;
pub(super) use renderer::BoundRenderer;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

/// Implemented by the host with a ViewId authenticated before and after each
/// worker-side renderer operation. A collection index is native-owned; the
/// bridge must register the extracted face, never a service-provided path.
pub(crate) trait FontRenderer: Send {
    fn register(
        &mut self,
        family: &str,
        bold: bool,
        italic: bool,
        weight_range: Option<&str>,
        sfnt: &[u8],
    ) -> Result<(), String>;
    fn measure_100px(
        &mut self,
        text: &str,
        family: &str,
        bold: bool,
        italic: bool,
    ) -> Result<f64, String>;
}

/// Only call with bytes read from an authorized native picker result.
pub(super) fn font_file_families(bytes: &[u8]) -> Vec<String> {
    registry::font_file_families(bytes)
}

#[derive(Clone)]
pub(crate) struct FontEnvironment {
    pub(crate) home: std::path::PathBuf,
    pub(crate) user: Option<std::path::PathBuf>,
    pub(crate) template: Option<std::path::PathBuf>,
    pub(crate) platform: main_core::pdf::font_locate::Platform,
}

impl FontEnvironment {
    /// The roots the Electron registry scanned: the OS font folders below the home folder, the
    /// user font store `userData/fonts`, and the shipped design-template font set.
    pub(crate) fn native(template: std::path::PathBuf) -> Self {
        use shell_contract::runtime::{AppPaths, PathName};
        let paths = crate::paths::ShellPaths::new();
        Self {
            home: paths.path(PathName::Home),
            user: Some(paths.path(PathName::UserData).join("fonts")),
            template: Some(template),
            platform: crate::host::d2::pdf::native_callbacks::platform(),
        }
    }
}

pub(crate) fn install_request(
    id: u64,
    deck: shell_ipc_d1::slides::engine::DeckId,
    generation: u64,
    authorization: crate::views::SlidesMediaAuthorization,
    env: FontEnvironment,
    renderer: Box<dyn FontRenderer>,
) -> Result<(), String> {
    let lease = authorization.lease();
    if !authorization.is_current() {
        return Err("slide document is no longer current".into());
    }
    let paths = discovery::FontPaths {
        home: env.home,
        user: env.user,
        template: env.template,
        platform: env.platform,
    };
    let mut renderer = Some(renderer);
    loop {
        let (epoch, embedded) = dispatch::snapshot()?;
        let mut session = FontSession::new(
            discovery::discover(&paths),
            embedded,
            env.platform == main_core::pdf::font_locate::Platform::Darwin,
        );
        let defaults: &[(ComplexScript, &str, &str)] = match env.platform {
            main_core::pdf::font_locate::Platform::Darwin => &[
                (
                    ComplexScript::Arabic,
                    "/System/Library/Fonts/GeezaPro.ttc",
                    "Geeza Pro",
                ),
                (
                    ComplexScript::Hebrew,
                    "/System/Library/Fonts/Supplemental/Tahoma.ttf",
                    "Tahoma",
                ),
                (
                    ComplexScript::Thai,
                    "/System/Library/Fonts/Supplemental/Thonburi.ttc",
                    "Thonburi",
                ),
                (
                    ComplexScript::Devanagari,
                    "/System/Library/Fonts/Supplemental/DevanagariMT.ttc",
                    "Devanagari MT",
                ),
            ],
            main_core::pdf::font_locate::Platform::Win32 => &[
                (
                    ComplexScript::Arabic,
                    r"C:\Windows\Fonts\tahoma.ttf",
                    "Tahoma",
                ),
                (
                    ComplexScript::Hebrew,
                    r"C:\Windows\Fonts\tahoma.ttf",
                    "Tahoma",
                ),
                (
                    ComplexScript::Thai,
                    r"C:\Windows\Fonts\leelawui.ttf",
                    "Leelawadee UI",
                ),
                (
                    ComplexScript::Devanagari,
                    r"C:\Windows\Fonts\Nirmala.ttf",
                    "Nirmala UI",
                ),
            ],
            main_core::pdf::font_locate::Platform::Other => &[],
        };
        for &(script, path, family) in defaults {
            if let Ok(bytes) = std::fs::read(path) {
                let _ = session.set_script_default(script, family.to_owned(), Arc::from(bytes), 0);
            }
        }
        super::slides_media::authorize(deck, generation, authorization.view, &lease)?;
        if dispatch::install(
            id,
            deck,
            generation,
            authorization.clone(),
            session,
            &mut renderer,
            epoch,
        )? {
            return Ok(());
        }
    }
}

/// The renderer must be bound to the captured document's authenticated
/// execution context. The caller owns the unique request id and cancellation.
#[allow(clippy::too_many_arguments)]
pub(super) fn adopt_document_fonts(
    id: u64,
    deck: shell_ipc_d1::slides::engine::DeckId,
    generation: u64,
    authorization: crate::views::SlidesMediaAuthorization,
    embedded: Vec<pptx_engine::embedded_fonts::EmbeddedFontFace>,
    metafile_requests: &[main_core::slides::metafile_fonts::MetafileFontRequest],
    env: FontEnvironment,
    renderer: Box<dyn FontRenderer>,
    cancel: &shell_contract::runtime::CancelToken,
) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err("slide request cancelled".into());
    }
    dispatch::adopt_embedded_fonts(&authorization, embedded)?;
    if metafile_requests.is_empty() {
        return Ok(());
    }
    let lease = authorization.lease();
    let view = authorization.view;
    install_request(id, deck, generation, authorization, env, renderer)?;
    let request_input =
        json!({ "deckId": deck.0, "generation": generation, "ownerViewId": view.0 });
    let result = metafile_requests.iter().try_for_each(|font| {
        let input = json!({ "items": [{
            "kind": "displayFamily",
            "style": {
                "fontFamily": font.family,
                "fontSizePx": 100,
                "bold": font.bold,
                "italic": font.italic
            }
        }] });
        dispatch::dispatch(
            id,
            "slides.fontMetrics",
            &input,
            &request_input,
            &lease,
            cancel,
        )
        .map(|_| ())
    });
    dispatch::close_request(id);
    result
}

pub(crate) fn dispatch(
    id: u64,
    operation: &str,
    input: &Value,
    request_input: &Value,
    lease: &crate::views::SlidesViewLease,
    cancel: &shell_contract::runtime::CancelToken,
) -> Result<Value, String> {
    dispatch::dispatch(id, operation, input, request_input, lease, cancel)
}

pub(crate) fn close_request(id: u64) {
    dispatch::close_request(id)
}

pub(crate) use dispatch::revoke_view;
pub(super) use dispatch::{
    private_faces, private_font_data, prune, release, reset_font_metrics, transfer_history_with,
};

#[derive(Clone)]
struct Style {
    family: String,
    size: f64,
    bold: bool,
    italic: bool,
    kerning: bool,
    script: Option<Script>,
    latin_only: bool,
}
impl Style {
    fn parse(value: &Value) -> Result<Self, String> {
        let family = value
            .get("fontFamily")
            .and_then(Value::as_str)
            .ok_or("missing font family")?
            .to_owned();
        let size = value
            .get("fontSizePx")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite())
            .ok_or("invalid font size")?;
        let bold = value
            .get("bold")
            .and_then(Value::as_bool)
            .ok_or("missing bold flag")?;
        let italic = value
            .get("italic")
            .and_then(Value::as_bool)
            .ok_or("missing italic flag")?;
        let kerning = value
            .get("kerning")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let script = match value.get("substScript").and_then(Value::as_str) {
            Some("ja") => Some(Script::Ja),
            Some("ko") => Some(Script::Ko),
            Some("sc") => Some(Script::Sc),
            Some("tc") => Some(Script::Tc),
            _ => None,
        };
        Ok(Self {
            family,
            size,
            bold,
            italic,
            kerning,
            script,
            latin_only: value
                .get("latinOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
    fn key(&self) -> String {
        format!(
            "{}|{}{}|{:?}|{}",
            self.family,
            u8::from(self.bold),
            u8::from(self.italic),
            self.script,
            self.latin_only
        )
    }
    fn fallback(&self, family: &str) -> Self {
        let mut value = self.clone();
        value.family = family.to_owned();
        value.script = None;
        value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ComplexScript {
    Arabic,
    Hebrew,
    Thai,
    Devanagari,
}
fn complex(text: &str) -> Option<ComplexScript> {
    for (script, ranges) in [
        (
            ComplexScript::Arabic,
            &[
                (0x0600, 0x06ff),
                (0x0750, 0x077f),
                (0x08a0, 0x08ff),
                (0xfb50, 0xfdff),
                (0xfe70, 0xfeff),
            ][..],
        ),
        (
            ComplexScript::Hebrew,
            &[(0x0590, 0x05ff), (0xfb1d, 0xfb4f)][..],
        ),
        (ComplexScript::Thai, &[(0x0e00, 0x0e7f)][..]),
        (ComplexScript::Devanagari, &[(0x0900, 0x097f)][..]),
    ] {
        if text.chars().any(|ch| {
            ranges
                .iter()
                .any(|(first, last)| (*first..=*last).contains(&(ch as u32)))
        }) {
            return Some(script);
        }
    }
    None
}
fn sample(script: ComplexScript) -> &'static str {
    match script {
        ComplexScript::Arabic => "ا",
        ComplexScript::Hebrew => "א",
        ComplexScript::Thai => "ก",
        ComplexScript::Devanagari => "क",
    }
}

fn extracted_font(face: &FontFace) -> Result<Vec<u8>, String> {
    let bytes = &face.bytes;
    let offset = if bytes.starts_with(b"ttcf") {
        let at = 12usize
            .checked_add(
                (face.index as usize)
                    .checked_mul(4)
                    .ok_or("invalid collection index")?,
            )
            .ok_or("invalid collection index")?;
        let raw: [u8; 4] = bytes
            .get(at..at + 4)
            .ok_or("invalid collection index")?
            .try_into()
            .map_err(|_| "invalid collection index")?;
        i64::from(u32::from_be_bytes(raw))
    } else {
        0
    };
    main_core::pdf::font_locate::extract_sfnt_face(bytes.to_vec(), offset)
}

/// Recreate on every authenticated deck generation. `defaults` are loaded by
/// native code from the original platform script-font table, not service input.
pub(super) struct FontSession {
    registry: registry::Registry,
    cache: HashMap<String, Option<FontFace>>,
    defaults: HashMap<ComplexScript, FontFace>,
    registered: Vec<(String, FontFace, bool, bool)>,
    pending_registered: Vec<(String, FontFace, bool, bool)>,
    mac: bool,
}
impl FontSession {
    fn new(
        sources: FontSources,
        embedded: Vec<pptx_engine::embedded_fonts::EmbeddedFontFace>,
        mac: bool,
    ) -> Self {
        Self {
            registry: registry::Registry::new(sources, embedded),
            cache: HashMap::new(),
            defaults: HashMap::new(),
            registered: Vec::new(),
            pending_registered: Vec::new(),
            mac,
        }
    }
    pub(super) fn set_script_default(
        &mut self,
        script: ComplexScript,
        family: String,
        bytes: Arc<[u8]>,
        index: u32,
    ) -> Result<(), String> {
        let face = FontFace {
            bytes,
            index,
            family,
            private: false,
            substituted: false,
            weight_range: None,
        };
        if face.shaped_units(sample(script), true).is_none() {
            return Err("script font does not cover its sample".into());
        }
        self.defaults.insert(script, face);
        Ok(())
    }
    fn resolve(&mut self, style: &Style) -> Option<FontFace> {
        let key = style.key();
        if let Some(value) = self.cache.get(&key) {
            return value.clone();
        }
        let aliases = policy::aliases(&style.family);
        let substitutes =
            policy::substitutes(&style.family, style.script, style.latin_only, self.mac);
        let mut candidates = Vec::new();
        for name in substitutes {
            candidates.push(name);
            candidates.extend(policy::aliases(name));
        }
        let mut result = self.registry.resolve(
            &style.family,
            style.bold,
            style.italic,
            &aliases,
            &candidates,
        );
        if let Some(face) = result.as_mut() {
            let requested = main_core::pdf::font_locate::norm(&style.family);
            let w = requested
                .as_bytes()
                .get(requested.len().saturating_sub(2)..)
                .filter(|s| s.len() == 2 && s[0] == b'w' && s[1].is_ascii_digit());
            if !face.private
                && let Some(w) = w
            {
                let suffix = std::str::from_utf8(w).unwrap_or("");
                if !main_core::pdf::font_locate::norm(&face.family).ends_with(suffix) {
                    face.family.push_str(&format!(" W{}", char::from(w[1])));
                }
            }
            let target = if w.is_some() {
                None
            } else {
                policy::weight_suffix(&style.family)
                    .map(|(_, target)| target)
                    .or_else(|| {
                        jscompat::js_regex!(r"\S\s+bold\s*$", "i")
                            .test(&style.family)
                            .then_some(700)
                    })
            };
            if !face.substituted
                && let Some(target) = target
            {
                let weight = face
                    .parse()
                    .ok()
                    .and_then(|parsed| parsed.tables().os2.map(|os2| os2.weight().to_number()));
                let matches = weight.is_some_and(|weight| {
                    if target == 400 {
                        weight == 400
                    } else {
                        weight != 400 && weight.abs_diff(target) <= 100
                    }
                });
                if main_core::pdf::font_locate::norm(&face.family) != requested && matches {
                    face.family = style.family.trim().to_owned();
                }
            }
        }
        self.cache.insert(key, result.clone());
        result
    }
    fn register(
        &mut self,
        face: &FontFace,
        style: &Style,
        renderer: &mut dyn FontRenderer,
    ) -> Result<(), String> {
        let requested = main_core::pdf::font_locate::norm(&style.family);
        let w = requested
            .as_bytes()
            .get(requested.len().saturating_sub(2)..)
            .is_some_and(|s| s.len() == 2 && s[0] == b'w' && s[1].is_ascii_digit());
        let target = if w {
            None
        } else {
            policy::weight_suffix(&style.family)
                .map(|(_, weight)| weight)
                .or_else(|| {
                    jscompat::js_regex!(r"\S\s+bold\s*$", "i")
                        .test(&style.family)
                        .then_some(700)
                })
        };
        let parsed = face.parse()?;
        let weight = parsed.tables().os2.map(|os2| os2.weight().to_number());
        let own_bold = weight.is_some_and(|weight| weight >= 600);
        let own_italic = parsed.italic_angle().abs() > 0.01;
        let weight_match = target.is_some_and(|target| {
            weight.is_some_and(|weight| {
                if target == 400 {
                    weight == 400
                } else {
                    weight != 400 && weight.abs_diff(target) <= 100
                }
            })
        });
        let synthetic_weight = !face.substituted
            && target.is_some()
            && (requested == main_core::pdf::font_locate::norm(&face.family) || weight_match);
        if !face.private && !w && !synthetic_weight {
            return Ok(());
        }
        let mut bold = if face.private || w {
            style.bold
        } else {
            style.bold && own_bold
        };
        let mut italic = if face.private || w {
            style.italic
        } else {
            style.italic && own_italic
        };
        if face.private && (style.bold || style.italic) {
            let mut regular_style = style.clone();
            regular_style.bold = false;
            regular_style.italic = false;
            if let Some(base) = self.resolve(&regular_style) {
                let parsed = face.parse()?;
                let own_bold = parsed
                    .tables()
                    .os2
                    .is_some_and(|os2| os2.weight().to_number() >= 600);
                let own_italic = parsed.italic_angle().abs() > 0.01;
                if Arc::ptr_eq(&face.bytes, &base.bytes)
                    && face.index == base.index
                    && (own_bold != style.bold || own_italic != style.italic)
                {
                    bold = false;
                    italic = false;
                }
            }
        }
        let key = format!("{}|{}{}", face.family, u8::from(bold), u8::from(italic));
        let index = self.registered.iter().position(|(id, _, _, _)| *id == key);
        let unchanged = index.is_some_and(|index| {
            let old = &self.registered[index].1;
            Arc::ptr_eq(&old.bytes, &face.bytes)
                && old.index == face.index
                && old.weight_range == face.weight_range
        });
        if !unchanged {
            let bytes = extracted_font(face)?;
            renderer.register(
                &face.family,
                bold,
                italic,
                face.weight_range.as_deref(),
                &bytes,
            )?;
            let entry = (key, face.clone(), bold, italic);
            self.pending_registered.push(entry.clone());
            if let Some(index) = index {
                self.registered[index] = entry;
            } else {
                self.registered.push(entry);
            }
        }
        Ok(())
    }
    fn fallback(&mut self, text: &str, face: Option<&FontFace>, style: &Style) -> Option<String> {
        let missing = |ch| face.is_none_or(|font| !font.contains(ch));
        if style.script == Some(Script::Ko)
            && text.chars().any(|ch| ('가'..='힣').contains(&ch))
            && missing('가')
        {
            return Some("Malgun Gothic".into());
        }
        if style.script == Some(Script::Ja)
            && text.chars().any(|ch| ('぀'..='ヿ').contains(&ch))
            && missing('あ')
        {
            return Some("Yu Gothic".into());
        }
        if !text.is_empty()
            && text.chars().all(|ch| "•■□▪▫▲►▼◄◊○●◘◙".contains(ch))
            && text.chars().next().is_some_and(missing)
        {
            let arial = self.resolve(&style.fallback("Arial"));
            if arial.as_ref().is_some_and(|font| {
                !font.substituted && text.chars().next().is_some_and(|ch| font.contains(ch))
            }) {
                return Some("Arial".into());
            }
        }
        None
    }
    fn shaped(&self, text: &str, face: Option<&FontFace>) -> Option<FontFace> {
        let script = complex(text)?;
        if let Some(font) = face.filter(|font| !font.substituted)
            && font.shaped_units(sample(script), true).is_some()
        {
            return Some(font.clone());
        }
        self.defaults.get(&script).cloned()
    }
    fn measure(
        &mut self,
        text: &str,
        style: &Style,
        renderer: &mut dyn FontRenderer,
    ) -> Result<f64, String> {
        let face = self.resolve(style);
        if let Some(shaped) = self.shaped(text, face.as_ref()) {
            self.register(&shaped, style, renderer)?;
            return Ok(
                renderer.measure_100px(text, &shaped.family, style.bold, style.italic)?
                    * style.size
                    / 100.0,
            );
        }
        if let Some(family) = self.fallback(text, face.as_ref(), style) {
            return self.measure(text, &style.fallback(&family), renderer);
        }
        let Some(face) = face else {
            return Ok(metrics::heuristic_width(text, style.size, style.bold));
        };
        self.register(&face, style, renderer)?;
        let parsed = face.parse()?;
        let variable = parsed
            .variation_axes()
            .into_iter()
            .any(|axis| axis.tag == ttf_parser::Tag::from_bytes(b"wght"));
        if face.private || variable {
            return Ok(
                renderer.measure_100px(text, &face.family, style.bold, style.italic)? * style.size
                    / 100.0,
            );
        }
        Ok(face
            .glyph_advance(text, style.size, style.bold, style.kerning, kerning::pair)?
            .unwrap_or_else(|| metrics::heuristic_width(text, style.size, style.bold)))
    }
    fn metrics(
        &mut self,
        style: &Style,
        renderer: &mut dyn FontRenderer,
    ) -> Result<Metrics, String> {
        match self.resolve(style) {
            Some(face) => {
                self.register(&face, style, renderer)?;
                face.metrics(style.size)
            }
            None => Ok(metrics::heuristic_metrics(style.size)),
        }
    }
    /// Service result is positional and contains no paths or font bytes.
    pub(super) fn call(
        &mut self,
        input: &Value,
        renderer: &mut dyn FontRenderer,
    ) -> Result<Value, String> {
        let items = input
            .get("items")
            .and_then(Value::as_array)
            .ok_or("missing font query items")?;
        let mut result = Vec::with_capacity(items.len());
        for item in items {
            let style = Style::parse(item.get("style").ok_or("missing font style")?)?;
            let value = match item.get("kind").and_then(Value::as_str) {
                Some("metrics") => {
                    let metric = self.metrics(&style, renderer)?;
                    let mut value = json!({"ascent":metric.ascent,"descent":metric.descent,"lineHeight":metric.line_height});
                    if let Some(gap) = metric.external_leading {
                        value["externalLeading"] = json!(gap);
                    }
                    value
                }
                Some("measure") => {
                    let text = item
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or("missing measurement text")?;
                    let width = self.measure(text, &style, renderer)?;
                    if !width.is_finite() {
                        return Err("nonfinite font width".into());
                    }
                    json!(width)
                }
                Some("displayFamily") => {
                    let face = self.resolve(&style);
                    if let Some(face) = face.as_ref() {
                        self.register(face, &style, renderer)?;
                    }
                    let text = item.get("text").and_then(Value::as_str);
                    let family = if let Some(fallback) =
                        text.and_then(|text| self.fallback(text, face.as_ref(), &style))
                    {
                        let fallback_style = style.fallback(&fallback);
                        match self.resolve(&fallback_style) {
                            Some(font) => {
                                self.register(&font, &fallback_style, renderer)?;
                                font.family
                            }
                            None => fallback,
                        }
                    } else if let Some(shaped) =
                        text.and_then(|text| self.shaped(text, face.as_ref()))
                    {
                        shaped.family
                    } else {
                        face.map_or(style.family.clone(), |font| font.family)
                    };
                    json!(family)
                }
                Some("substituted") => {
                    let face = self.resolve(&style);
                    if let Some(face) = face.as_ref() {
                        self.register(face, &style, renderer)?;
                    }
                    json!(face.is_some_and(|font| font.substituted))
                }
                _ => return Err("unknown font query kind".into()),
            };
            result.push(value);
        }
        Ok(Value::Array(result))
    }
}
