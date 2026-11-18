//! Native single-page slide specification and PPTX construction.
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use jscompat::JsNum;
use pptx_engine::background_promote::promote_slide_background;
use pptx_engine::deck::{open_pptx, save_pptx};
use pptx_engine::deck::shape::edit_picture_src_rect;
use pptx_engine::insert::{add_element, add_picture, NewElementOptions, NewPictureOptions};
use pptx_engine::types::{EmuRect, Insets};
use serde_json::{json, Value};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::GeneratedPage;

const WIDTH: f64 = 1280.0;
const HEIGHT: f64 = 720.0;
const SHAPES: &[&str] = &["rect", "roundRect", "ellipse", "triangle", "rightArrow", "leftArrow", "upArrow", "downArrow", "chevron", "diamond", "parallelogram", "trapezoid", "hexagon", "pentagon", "pie", "donut", "star5", "heart", "cloud", "line", "lineArrow"];

struct Page { background: Option<String>, elements: Vec<Element> }
struct Element { kind: Kind, x: f64, y: f64, w: f64, h: f64 }
enum Kind {
    Image { url: String },
    Text { paragraphs: Vec<Value>, anchor: String },
    Shape { shape: String, fill: Option<String>, stroke: Option<(String, f64)>, paragraphs: Vec<Value>, anchor: String, flip_h: bool, flip_v: bool },
}

fn number(value: Option<&Value>) -> Option<f64> {
    let n = match value? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => jscompat::to_number(s),
        _ => return None,
    };
    n.is_finite().then_some(n)
}
fn color(value: Option<&Value>) -> Option<String> {
    let raw = value?.as_str()?.trim();
    let text = raw.strip_prefix('#').unwrap_or(raw).to_ascii_uppercase();
    if !text.bytes().all(|c| c.is_ascii_hexdigit()) { return None; }
    let hex = if text.len() == 3 { text.chars().flat_map(|c| [c, c]).collect() } else { text };
    matches!(hex.len(), 6 | 8).then(|| format!("#{hex}"))
}
fn truncated(s: &str, n: usize) -> String {
    String::from_utf16_lossy(&s.encode_utf16().take(n).collect::<Vec<_>>())
}
fn roles(value: &IpcValue) -> Option<(&str, &str)> {
    let IpcValue::Object(fields) = value else { return None };
    let find = |key: &str| fields.iter().find_map(|(k, v)| match v { IpcValue::String(s) if k == key && s.encode_utf16().count() <= 80 && !s.trim().is_empty() => Some(s.trim()), _ => None });
    Some((find("title")?, find("body")?))
}
fn paragraphs(value: Option<&Value>, fonts: Option<(&str, &str)>) -> Vec<Value> {
    let Some(items) = value.and_then(Value::as_array) else { return Vec::new() };
    items.iter().map(|p| {
        let mut runs: Vec<Value> = p.get("runs").and_then(Value::as_array).into_iter().flatten().map(|r| {
            let text = truncated(r.get("text").and_then(Value::as_str).unwrap_or(""), 4000);
            let size = number(r.get("sizePt")).filter(|n| *n != 0.0).map(|n| n.clamp(6.0, 160.0));
            let font = r.get("font").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(|s| truncated(s, 80))
                .or_else(|| fonts.map(|(title, body)| if size.is_some_and(|n| n >= 28.0) { title.to_owned() } else { body.to_owned() }));
            let mut run = json!({"text": text});
            if let Some(n) = size { run["fontSize"] = json!(n); }
            for key in ["bold", "italic"] { if r.get(key) == Some(&Value::Bool(true)) { run[key] = json!(true); } }
            if let Some(c) = color(r.get("color")) { run["color"] = json!(c); }
            if let Some(font) = font { run["fontFamily"] = json!(font); run["latinFont"] = json!(font); run["eaFont"] = json!(font); }
            run
        }).collect();
        if runs.is_empty() { runs.push(json!({"text": ""})); }
        let mut result = json!({"runs": runs});
        if let Some(a @ ("left" | "center" | "right" | "justify")) = p.get("align").and_then(Value::as_str) { result["align"] = json!(a); }
        if let Some(n) = number(p.get("lineSpacingPct")).filter(|n| *n != 0.0) { result["lineHeight"] = json!(n.clamp(60.0, 300.0)); }
        for (source, dest) in [("spaceBeforePt", "spaceBefore"), ("spaceAfterPt", "spaceAfter")] {
            if let Some(n) = number(p.get(source)) { result[dest] = json!(n.clamp(0.0, 96.0)); }
        }
        if p.get("bullet") == Some(&Value::Bool(true)) { result["bullet"] = json!({"type":"char", "char":"•"}); result["marL"] = json!(228600); result["indent"] = json!(-228600); }
        result
    }).collect()
}
fn has_text(paragraphs: &[Value]) -> bool {
    paragraphs.iter().any(|p| p["runs"].as_array().is_some_and(|runs| runs.iter().any(|r| r["text"].as_str().is_some_and(|s| !s.trim().is_empty()))))
}
fn anchor(value: Option<&Value>, default: &str) -> String {
    match value.and_then(Value::as_str) { Some("top") => "t", Some("middle") => "ctr", Some("bottom") => "b", _ => default }.to_owned()
}
fn parse(raw: &str, fonts: Option<(&str, &str)>) -> Result<Page, String> {
    let start = raw.find('{').ok_or("no JSON object found in the output")?;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut end = None;
    for (offset, c) in raw[start..].char_indices() {
        if quoted {
            if escaped { escaped = false; } else if c == '\\' { escaped = true; } else if c == '"' { quoted = false; }
        } else if c == '"' { quoted = true; } else if c == '{' { depth += 1; } else if c == '}' {
            depth = depth.saturating_sub(1);
            if depth == 0 { end = Some(start + offset); break; }
        }
    }
    let end = end.or_else(|| raw.rfind('}')).filter(|end| *end > start).ok_or("no JSON object found in the output")?;
    let root: Value = serde_json::from_str(&raw[start..=end]).map_err(|e| format!("invalid JSON: {e}"))?;
    let source = root.get("elements").and_then(Value::as_array).filter(|items| !items.is_empty()).ok_or("the \"elements\" array is missing or empty")?;
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let mut image_count = 0;
    for (i, el) in source.iter().enumerate() {
        if out.len() >= 48 { break; }
        let (Some(x), Some(y), Some(w), Some(h)) = (number(el.get("x")), number(el.get("y")), number(el.get("w")), number(el.get("h"))) else {
            warnings.push(format!("element {i}: missing/non-numeric x/y/w/h, dropped")); continue;
        };
        let cx = x.clamp(0.0, WIDTH - 1.0); let cy = y.clamp(0.0, HEIGHT - 1.0);
        let cw = (w - (cx - x)).min(WIDTH - cx).max(0.0); let ch = (h - (cy - y)).min(HEIGHT - cy).max(0.0);
        if x >= WIDTH || y >= HEIGHT || x + w <= 0.0 || y + h <= 0.0 || cw < 1.0 || ch < 1.0 {
            warnings.push(format!("element {i}: outside the 1280x720 canvas, dropped")); continue;
        }
        let kind = match el.get("type").and_then(Value::as_str) {
            Some("image") => {
                let url = el.get("url").and_then(Value::as_str).unwrap_or("").trim();
                let Some((scheme, rest)) = url.split_once("://") else { warnings.push(format!("element {i}: image url must be http(s), dropped")); continue };
                if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") { warnings.push(format!("element {i}: image url must be http(s), dropped")); continue; }
                if image_count >= 8 { warnings.push(format!("element {i}: image cap 8 reached, dropped")); continue; }
                image_count += 1;
                Kind::Image { url: format!("{}://{rest}", scheme.to_ascii_lowercase()) }
            }
            Some("text") => {
                let paragraphs = paragraphs(el.get("paragraphs"), fonts);
                if !has_text(&paragraphs) { warnings.push(format!("element {i}: text element without any text, dropped")); continue; }
                Kind::Text { paragraphs, anchor: anchor(el.get("valign"), "t") }
            }
            Some("shape") => {
                let original = el.get("shape").and_then(Value::as_str).unwrap_or("rect").trim();
                let shape = if SHAPES.contains(&original) { original } else { warnings.push(format!("element {i}: unknown shape \"{original}\", using rect")); "rect" };
                let fill = color(el.get("fill"));
                let stroke = color(el.get("stroke").and_then(|v| v.get("color"))).map(|c| (c, number(el.get("stroke").and_then(|v| v.get("widthPt"))).unwrap_or(1.0).clamp(0.25, 24.0)));
                if fill.is_none() && stroke.is_none() && !matches!(shape, "line" | "lineArrow") { warnings.push(format!("element {i}: shape without fill or stroke, dropped")); continue; }
                let mut paragraphs = paragraphs(el.get("paragraphs"), fonts);
                if !has_text(&paragraphs) { paragraphs.clear(); }
                Kind::Shape { shape: shape.to_owned(), fill, stroke, paragraphs, anchor: anchor(el.get("valign"), "ctr"), flip_h: el.get("flipH") == Some(&Value::Bool(true)), flip_v: el.get("flipV") == Some(&Value::Bool(true)) }
            }
            other => { warnings.push(format!("element {i}: unknown type \"{}\", dropped", other.unwrap_or("undefined"))); continue; }
        };
        out.push(Element { kind, x: cx, y: cy, w: cw, h: ch });
    }
    if out.is_empty() { return Err(format!("no valid elements ({})", if warnings.is_empty() { "all dropped".into() } else { warnings.join("; ") })); }
    Ok(Page { background: color(root.get("background")), elements: out })
}

fn fetch_image(url: &str) -> Option<(Vec<u8>, String)> {
    let remote = crate::host::net::ai::fetch::remote_image(url, "")?;
    let mime = match image::guess_format(&remote.bytes).ok() {
        Some(image::ImageFormat::Png) => "png",
        Some(image::ImageFormat::Gif) => "gif",
        Some(image::ImageFormat::WebP) => "webp",
        Some(image::ImageFormat::Bmp) => "bmp",
        Some(image::ImageFormat::Jpeg) => "jpg",
        _ if remote.content_type.contains("png") => "png",
        _ if remote.content_type.contains("gif") => "gif",
        _ if remote.content_type.contains("webp") => "webp",
        _ if remote.content_type.contains("bmp") => "bmp",
        _ => "jpg",
    };
    Some((remote.bytes, mime.to_owned()))
}

fn rect(element: &Element, scale: f64) -> EmuRect {
    EmuRect {
        x: JsNum((element.x * scale).round()),
        y: JsNum((element.y * scale).round()),
        cx: JsNum((element.w * scale).round().max(1.0)),
        cy: JsNum((element.h * scale).round().max(1.0)),
    }
}

fn crop(image_w: f64, image_h: f64, frame_w: f64, frame_h: f64) -> Option<Insets> {
    if image_w <= 0.0 || image_h <= 0.0 || frame_w <= 0.0 || frame_h <= 0.0 { return None; }
    let image_aspect = image_w / image_h;
    let frame_aspect = frame_w / frame_h;
    if (image_aspect - frame_aspect).abs() / image_aspect.max(frame_aspect) < 0.01 { return None; }
    let (horizontal, vertical) = if image_aspect > frame_aspect {
        ((1.0 - frame_aspect / image_aspect) / 2.0, 0.0)
    } else {
        (0.0, (1.0 - image_aspect / frame_aspect) / 2.0)
    };
    Some(Insets { l: JsNum(horizontal), t: JsNum(vertical), r: JsNum(horizontal), b: JsNum(vertical) })
}

fn add_shape(slide: &mut pptx_engine::types::Slide, value: Value, now: f64) -> Result<(), String> {
    let mut options: NewElementOptions = serde_json::from_value(value).map_err(|error| error.to_string())?;
    if let Some(paragraphs) = options.paragraphs.as_mut() {
        if let Some(tagged) = pptx_engine::generate::tag_hangul_paragraphs(paragraphs) {
            *paragraphs = tagged;
        }
    }
    add_element(slide, &options, &jscompat::random_uuid(), now)?;
    Ok(())
}

/// Generate the PPTX natively. `grow_text` is the authenticated renderer's narrow
/// text-layout service; it receives an already-built deck and must preserve it
/// except for the grow-only text-box adjustment.
pub(super) fn generate_page(
    spec_json: &str,
    default_fonts: &IpcValue,
    grow_text: impl FnOnce(Vec<u8>, &IpcValue) -> Result<Vec<u8>, String>,
) -> Result<GeneratedPage, String> {
    let page = parse(spec_json, roles(default_fonts))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs_f64() * 1000.0;
    let bytes = pptx_engine::blank::create_blank_pptx(now)?;
    let mut opened = open_pptx(&bytes)?;
    let size = opened.deck.size;
    let scale = size.cx.0 / WIDTH;
    let mut fetched = HashMap::new();
    for element in &page.elements {
        if let Kind::Image { url } = &element.kind {
            if !fetched.contains_key(url) { fetched.insert(url.clone(), fetch_image(url)); }
        }
    }
    if let Some(background) = page.background {
        let slide = opened.deck.slides.first_mut().ok_or("blank presentation has no slide")?;
        add_shape(slide, json!({ "kind": "rect", "offset": {"x":0,"y":0,"cx":size.cx.0,"cy":size.cy.0}, "fillColor": background }), now)?;
    }
    let mut failures = Vec::new();
    for element in &page.elements {
        let offset = rect(element, scale);
        match &element.kind {
            Kind::Image { url } => {
                let Some(Some((bytes, ext))) = fetched.get(url) else { failures.push(url.clone()); continue };
                let options = NewPictureOptions { bytes: bytes.clone(), ext: ext.clone(), offset, name: None, descr: None };
                let Some(picture) = add_picture(&mut opened, 0, &options, &jscompat::random_uuid(), now)? else {
                    failures.push(url.clone()); continue;
                };
                if let Ok(image) = image::load_from_memory(bytes) {
                    if let Some(rect) = crop(f64::from(image.width()), f64::from(image.height()), element.w, element.h) {
                        let slide = opened.deck.slides.first_mut().ok_or("blank presentation has no slide")?;
                        let id = picture.id.as_deref().ok_or("inserted picture has no id")?;
                        if !edit_picture_src_rect(slide, id, Some(rect)) {
                            return Err("could not crop the inserted picture".into());
                        }
                    }
                }
            }
            Kind::Text { paragraphs, anchor } => {
                let value = json!({"kind":"textbox", "offset":offset, "paragraphs":paragraphs,
                    "bodyPr":{"wrap":"square","anchor":anchor,"insetsEmu":{"l":0,"t":0,"r":0,"b":0}}});
                add_shape(opened.deck.slides.first_mut().ok_or("blank presentation has no slide")?, value, now)?;
            }
            Kind::Shape { shape, fill, stroke, paragraphs, anchor, flip_h, flip_v } => {
                let mut value = json!({"kind":shape,"offset":offset});
                if let Some(fill) = fill { value["fillColor"] = json!(fill); }
                if let Some((color, width)) = stroke { value["stroke"] = json!({"color":color,"widthEmu":(width * 12700.0).round()}); }
                if *flip_h { value["flipH"] = json!(true); }
                if *flip_v { value["flipV"] = json!(true); }
                if !paragraphs.is_empty() {
                    value["paragraphs"] = json!(paragraphs);
                    value["bodyPr"] = json!({"wrap":"square","anchor":anchor,"insetsEmu":{"l":0,"t":0,"r":0,"b":0}});
                }
                add_shape(opened.deck.slides.first_mut().ok_or("blank presentation has no slide")?, value, now)?;
            }
        }
    }
    let slide = opened.deck.slides.first_mut().ok_or("blank presentation has no slide")?;
    promote_slide_background(slide, &size);
    let bytes = grow_text(save_pptx(&mut opened)?, default_fonts)?;
    Ok(GeneratedPage { bytes, image_failures: failures })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_clips_rejects_invalid_and_keeps_z_order() {
        let page = parse(r##"prose {"background":"#abc","elements":[{"type":"shape","shape":"unknown","fill":"#123","x":-20,"y":10,"w":100,"h":50},{"type":"text","x":10,"y":20,"w":40,"h":30,"paragraphs":[{"runs":[{"text":"한국어","sizePt":200}]}]},{"type":"image","url":"file:///secret","x":0,"y":0,"w":10,"h":10}]} trailing"##, None).unwrap();
        assert_eq!(page.background.as_deref(), Some("#AABBCC"));
        assert_eq!(page.elements.len(), 2);
        assert_eq!((page.elements[0].x, page.elements[0].w), (0.0, 80.0));
        assert!(matches!(&page.elements[0].kind, Kind::Shape { shape, .. } if shape == "rect"));
        assert!(matches!(&page.elements[1].kind, Kind::Text { paragraphs, .. } if paragraphs[0]["runs"][0]["fontSize"] == 160.0));
    }

    #[test]
    fn generates_reopenable_presentation_and_invokes_layout() {
        let called = std::cell::Cell::new(false);
        let page = generate_page(
            r##"{"background":"#ffffff","elements":[{"type":"text","x":0,"y":0,"w":400,"h":80,"paragraphs":[{"runs":[{"text":"한국어","sizePt":32}]}]}]}"##,
            &IpcValue::Null,
            |bytes, _| { called.set(true); Ok(bytes) },
        ).unwrap();
        assert!(called.get());
        assert!(page.image_failures.is_empty());
        let deck = open_pptx(&page.bytes).unwrap();
        assert_eq!(deck.deck.slides.len(), 1);
        assert!(deck.deck.slides[0].elements.as_deref().is_some_and(|elements| !elements.is_empty()));
    }

    #[test]
    fn layout_failure_is_not_acknowledged_as_a_generated_page() {
        let result = generate_page(
            r##"{"elements":[{"type":"shape","fill":"#fff","x":0,"y":0,"w":100,"h":100}]}"##,
            &IpcValue::Null,
            |_, _| Err("font layout unavailable".into()),
        );
        assert_eq!(result.unwrap_err(), "font layout unavailable");
    }

    #[test]
    fn center_crop_preserves_proportions() {
        let crop = crop(400.0, 200.0, 100.0, 100.0).unwrap();
        assert_eq!((crop.l.0, crop.r.0, crop.t.0, crop.b.0), (0.25, 0.25, 0.0, 0.0));
        assert!(crop(100.0, 100.0, 100.0, 100.0).is_none());
    }
}
