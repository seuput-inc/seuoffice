use super::registry::{OpContext, SlideTarget, field, op_name, record, resolve_slide, string};
use base64::Engine;
use jscompat::JsNum;
use pptx_engine::background_promote::promote_slide_background;
use pptx_engine::comments::{
    SlideCommentOptions, SlideCommentRef, add_slide_comment, delete_slide_comment,
};
use pptx_engine::deck::background::{self, BackgroundImageSource, SlideBackgroundFill};
use pptx_engine::deck::shape::autofit_generated_text_boxes;
use pptx_engine::deck::slide_props::{
    set_slide_advance_time, set_slide_hidden, set_slide_transition,
};
use pptx_engine::deck::slides::{
    DuplicateSlideOptions, delete_slide, duplicate_slide, insert_blank_slide,
    insert_slide_with_layout, reset_slide_layout, set_slide_layout, set_slide_size,
};
use pptx_engine::deck::slides::{
    MergeSlideOptions, MergeSlideSource, PasteSlideOptions, merge_slide_from_source, paste_slide,
};
use pptx_engine::deck::text::{ReplaceOptions, replace_all_in_deck};
use pptx_engine::deck::{commit_saved, materialize_slide, reparse_deck};
use pptx_engine::generate::GradientFillPatch;
use pptx_engine::generate::SlideTransitionKind;
use pptx_engine::headerfooter::{HeaderFooterOptions, apply_header_footer};
use pptx_engine::identity::slide_durable_id;
use pptx_engine::insert::{NewPictureOptions, add_picture};
use pptx_engine::layout::list_slide_layouts;
use pptx_engine::notes::set_slide_notes;
use pptx_engine::sections::{
    SectionDirection, SectionInfo, add_section, move_section, move_slide, remove_section,
    rename_section, set_sections,
};
use pptx_engine::slide_transfer::SlideBundle;
use pptx_engine::theme_apply::{ThemeSpec, apply_theme_to_archive, remap_deck_colors};
use pptx_engine::types::EmuRect;
use pptx_engine::types::{Fill, GradientFill, GradientPath, GradientStop, SlideElement};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;

fn number(value: Option<&IpcValue>) -> Option<f64> {
    match value {
        Some(IpcValue::Number(n)) => Some(*n),
        _ => None,
    }
}
fn index(ctx: &mut OpContext<'_>, op: &IpcValue) -> Result<usize, String> {
    match resolve_slide(ctx, op, false)? {
        SlideTarget::Slide(i) => Ok(i),
        SlideTarget::Part(_) => unreachable!(),
    }
}
fn obj(fields: Vec<(&str, IpcValue)>) -> IpcValue {
    IpcValue::Object(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}
fn idx(i: usize) -> IpcValue {
    obj(vec![("index", IpcValue::Number(i as f64))])
}
fn section_value(s: SectionInfo) -> IpcValue {
    obj(vec![
        ("id", IpcValue::String(s.id)),
        ("name", IpcValue::String(s.name)),
        (
            "slideIndices",
            IpcValue::Array(
                s.slide_indices
                    .into_iter()
                    .map(|i| IpcValue::Number(i as f64))
                    .collect(),
            ),
        ),
    ])
}
fn json_value(value: &IpcValue) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    Ok(match value {
        IpcValue::Null => Value::Null,
        IpcValue::Bool(b) => Value::Bool(*b),
        IpcValue::Number(n) => Value::Number(
            serde_json::Number::from_f64(*n).ok_or("non-finite number in slide payload")?,
        ),
        IpcValue::String(s) => Value::String(s.clone()),
        IpcValue::Bytes(_, b) => {
            Value::Array(b.iter().map(|v| Value::Number((*v).into())).collect())
        }
        IpcValue::Array(a) => Value::Array(a.iter().map(json_value).collect::<Result<_, _>>()?),
        IpcValue::Object(fields) => {
            let mut out = serde_json::Map::new();
            for (k, v) in fields {
                if !matches!(v, IpcValue::Undefined) {
                    out.insert(k.clone(), json_value(v)?);
                }
            }
            Value::Object(out)
        }
        _ => return Err("unsupported value in slide payload".into()),
    })
}
fn background_bytes(source: &IpcValue) -> Result<Vec<u8>, String> {
    let fail = || {
        "op \"setBackground\" needs \"source.bytes\": image/media bytes (base64 string, data URL, or Uint8Array).".to_owned()
    };
    match field(source, "bytes") {
        Some(IpcValue::Bytes(shell_contract::BytesKind::Uint8Array, b)) if !b.is_empty() => {
            Ok(b.clone())
        }
        Some(IpcValue::String(s)) if !s.is_empty() => {
            let data = if s.starts_with("data:") {
                s.split_once(',').map_or(s.as_str(), |(_, s)| s)
            } else {
                s
            };
            let clean: String = data.chars().filter(|c| !c.is_whitespace()).collect();
            let raw = clean.trim_end_matches('=');
            if raw.is_empty()
                || clean.len() - raw.len() > 2
                || !raw
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
            {
                return Err(fail());
            }
            let decoded = base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(raw)
                .map_err(|_| fail())?;
            if decoded.is_empty() {
                Err(fail())
            } else {
                Ok(decoded)
            }
        }
        _ => Err(fail()),
    }
}
fn background_ext(source: &IpcValue) -> Option<&str> {
    if let Some(ext) = field(source, "ext")
        .and_then(string)
        .filter(|v| !v.is_empty())
    {
        return Some(ext);
    }
    let data = field(source, "bytes")
        .and_then(string)?
        .strip_prefix("data:")?;
    match data.split([';', ',']).next()? {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/bmp" => Some("bmp"),
        "image/webp" => Some("webp"),
        "image/tiff" => Some("tif"),
        _ => None,
    }
}
fn check_color(op: &IpcValue, key: &str) -> Result<(), String> {
    let value = field(op, key);
    if value.and_then(string).is_some_and(|s| {
        let s = s.trim().strip_prefix('#').unwrap_or(s.trim());
        (s.len() == 6 || s.len() == 8) && s.bytes().all(|b| b.is_ascii_hexdigit())
    }) {
        return Ok(());
    }
    let display = value
        .and_then(|v| json_value(v).ok())
        .map(|v| v.to_string())
        .unwrap_or_else(|| "undefined".into());
    Err(format!(
        "op \"setBackground\": \"{key}\" must be a hex color \"#RRGGBB\" (or \"#RRGGBBAA\"), got {display}."
    ))
}
fn repaint(ctx: &mut OpContext<'_>, index: usize, fill: Fill) {
    let size = &ctx.opened.deck.size;
    let (cx, cy) = (size.cx.0, size.cy.0);
    let Some(elements) = ctx.opened.deck.slides[index].elements.as_mut() else {
        return;
    };
    for element in elements {
        let SlideElement::Text(t) = element else {
            continue;
        };
        if !matches!(
            t.fill.as_ref(),
            Some(Fill::Solid { .. } | Fill::Gradient(_))
        ) {
            continue;
        }
        if t.text
            .as_ref()
            .and_then(|b| b.paragraphs.as_ref())
            .is_some_and(|p| {
                p.iter().any(|p| {
                    p.runs.as_ref().is_some_and(|runs| {
                        runs.iter()
                            .any(|r| !r.text.as_deref().unwrap_or("").trim().is_empty())
                    })
                })
            })
        {
            continue;
        }
        let Some(transform) = t.transform.as_ref() else {
            continue;
        };
        let o = &transform.offset;
        if o.x.0 > cx * 0.05
            || o.x.0 + o.cx.0 < cx * 0.95
            || o.y.0 > cy * 0.05
            || o.y.0 + o.cy.0 < cy * 0.95
        {
            continue;
        }
        t.fill.set(fill.clone());
        t.dirty_fill.set(true);
    }
}
fn has_layout(op: &IpcValue) -> bool {
    ["layout", "layoutPath"].iter().any(|key| {
        !matches!(
            field(op, key),
            None | Some(IpcValue::Null | IpcValue::Undefined)
        )
    })
}
fn layout(op: &IpcValue, ctx: &OpContext<'_>) -> Result<String, String> {
    let layouts = list_slide_layouts(&ctx.opened.archive);
    let available = layouts
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{i}: \"{}\"", l.name))
        .collect::<Vec<_>>()
        .join(", ");
    let name = op_name(op);
    let reference = match field(op, "layout") {
        None | Some(IpcValue::Null | IpcValue::Undefined) => field(op, "layoutPath"),
        value => value,
    };
    if let Some(n) = number(reference) {
        if n.is_finite()
            && n >= 0.0
            && n.fract() == 0.0
            && let Some(found) = layouts.get(n as usize)
        {
            return Ok(found.path.clone());
        }
        return Err(format!(
            "op \"{name}\": layout index {n} is out of range (0-{}). Layouts: [{available}].",
            layouts.len() as isize - 1
        ));
    }
    let Some(reference) = reference.and_then(string).filter(|s| !s.trim().is_empty()) else {
        return Err(format!(
            "op \"{name}\" needs \"layout\": a layout name or 0-based index. Layouts: [{available}]."
        ));
    };
    if let Some(found) = layouts.iter().find(|l| l.path == reference) {
        return Ok(found.path.clone());
    }
    let matches: Vec<_> = layouts
        .iter()
        .filter(|l| l.name.to_lowercase() == reference.trim().to_lowercase())
        .collect();
    match matches.as_slice() {
        [found] => Ok(found.path.clone()),
        [] => Err(format!(
            "op \"{name}\": no layout \"{reference}\". Layouts: [{available}]."
        )),
        _ => Err(format!(
            "op \"{name}\": {} layouts are named \"{reference}\" — pass the index instead. Layouts: [{available}].",
            matches.len()
        )),
    }
}
fn transition(op: &IpcValue) -> Option<SlideTransitionKind> {
    use SlideTransitionKind::*;
    Some(match field(op, "kind").and_then(string)? {
        "none" => None,
        "morph" => Morph,
        "fade" => Fade,
        "push" => Push,
        "wipe" => Wipe,
        "split" => Split,
        "circle" => Circle,
        "cover" => Cover,
        "pull" => Pull,
        "dissolve" => Dissolve,
        "zoom" => Zoom,
        "random" => Random,
        _ => return Option::None,
    })
}
const NAMES: &[&str] = &[
    "deleteSlide",
    "duplicateSlide",
    "addBlankSlide",
    "addSlideWithLayout",
    "pasteSlide",
    "insertSlidePptx",
    "moveSlide",
    "setSlideLayout",
    "setSlideSize",
    "setBackground",
    "setHidden",
    "setTransition",
    "setAdvanceTime",
    "setNotes",
    "addComment",
    "deleteComment",
    "findReplace",
    "applyHeaderFooter",
    "applyTheme",
    "setSections",
    "addSection",
    "renameSection",
    "removeSection",
    "moveSection",
];
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    let name = op_name(op);
    if !NAMES.contains(&name) {
        return None;
    }
    Some((|| {
        match name {
            "applyHeaderFooter" => {
                if !matches!(
                    field(op, "settings"),
                    Some(IpcValue::Object(_) | IpcValue::Array(_))
                ) {
                    return Err("op \"applyHeaderFooter\" needs \"settings\".".into());
                }
            }
            "applyTheme" => {
                if field(op, "name").and_then(string).is_none_or(str::is_empty) {
                    return Err("op \"applyTheme\" needs \"name\": the theme name.".into());
                }
                let Some(IpcValue::Object(colors)) = field(op, "colors") else {
                    return Err("op \"applyTheme\" needs \"colors\": a scheme-slot → \"#RRGGBB\" record (dk1/lt1/dk2/lt2/accent1..6/hlink/folHlink).".into());
                };
                for (key, value) in colors {
                    if !string(value).is_some_and(|s| {
                        let s = s.trim().strip_prefix('#').unwrap_or(s.trim());
                        s.len() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit())
                    }) {
                        return Err(format!(
                            "op \"applyTheme\": colors.{key} must be a \"#RRGGBB\" string."
                        ));
                    }
                }
            }
            "addComment" => {
                index(ctx, op)?;
                if field(op, "text").and_then(string).is_none_or(str::is_empty) {
                    return Err("op \"addComment\" needs \"text\".".into());
                }
                if field(op, "author")
                    .and_then(string)
                    .is_none_or(str::is_empty)
                {
                    return Err("op \"addComment\" needs \"author\".".into());
                }
            }
            "deleteComment" => {
                index(ctx, op)?;
                if number(field(op, "authorId")).is_none() || number(field(op, "idx")).is_none() {
                    return Err("op \"deleteComment\" needs \"authorId\" and \"idx\".".into());
                }
            }
            "setBackground" => {
                index(ctx, op)?;
                match field(op, "kind").and_then(string) {
                    Some("solid") => check_color(op, "color")?,
                    Some("gradient") => {
                        check_color(op, "from")?;
                        check_color(op, "to")?;
                        if field(op, "angleDeg").is_some_and(|v| !matches!(v, IpcValue::Undefined))
                            && !number(field(op, "angleDeg")).is_some_and(f64::is_finite)
                        {
                            return Err(
                                "op \"setBackground\": \"angleDeg\" must be a finite number."
                                    .into(),
                            );
                        }
                    }
                    Some("image") => {
                        let source = field(op, "source").ok_or("op \"setBackground\" image needs \"source\": {bytes, ext} or {mediaPath}.")?;
                        if field(source, "bytes").is_none()
                            && field(source, "mediaPath")
                                .and_then(string)
                                .is_none_or(str::is_empty)
                        {
                            return Err("op \"setBackground\" image needs \"source\": {bytes, ext} or {mediaPath}.".into());
                        }
                        if field(source, "bytes")
                            .is_some_and(|v| !matches!(v, IpcValue::Null | IpcValue::Undefined))
                        {
                            background_bytes(source)?;
                            if background_ext(source).is_none() {
                                return Err(
                                    "op \"setBackground\" image needs \"source.ext\".".into()
                                );
                            }
                        }
                    }
                    Some("reset" | "graphics") => {}
                    _ => return Err(
                        "op \"setBackground\" needs \"kind\": solid/gradient/image/reset/graphics."
                            .into(),
                    ),
                }
            }
            "pasteSlide" => {
                if number(field(op, "afterIndex")).is_none() {
                    return Err(
                        "op \"pasteSlide\" needs \"afterIndex\": the slide index to paste after."
                            .into(),
                    );
                }
                if field(op, "mode").and_then(string) == Some("picture") {
                    if field(op, "png").and_then(string).is_none_or(str::is_empty) {
                        return Err("op \"pasteSlide\" mode \"picture\" needs \"png\": base64 slide bitmap.".into());
                    }
                } else if !matches!(
                    field(op, "bundle"),
                    Some(IpcValue::Object(_) | IpcValue::Array(_))
                ) {
                    return Err("op \"pasteSlide\" needs \"bundle\": a copied slide bundle.".into());
                }
            }
            "insertSlidePptx" => {
                let source = field(op, "source");
                if !matches!(source, Some(IpcValue::Object(_)))
                    || source
                        .and_then(|s| field(s, "slideXml"))
                        .and_then(string)
                        .is_none()
                {
                    return Err("op \"insertSlidePptx\" needs \"source\": an extracted single-slide pptx (main-process payload).".into());
                }
                for f in ["rels", "media", "layoutChain"] {
                    if !matches!(source.and_then(|s| field(s, f)), Some(IpcValue::Array(_))) {
                        return Err(format!(
                            "op \"insertSlidePptx\": \"source.{f}\" must be an array (may be empty)."
                        ));
                    }
                }
                let total = ctx.opened.deck.slides.len();
                let at = field(op, "at");
                if !matches!(at, None | Some(IpcValue::Null | IpcValue::Undefined))
                    && !number(at).is_some_and(|n| {
                        n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= total as f64
                    })
                {
                    return Err(format!(
                        "op \"insertSlidePptx\": \"at\" is out of range (0-{total})."
                    ));
                }
                if matches!(field(op, "replace"), Some(IpcValue::Bool(true)))
                    && !number(at).is_some_and(|n| n < total as f64)
                {
                    return Err("op \"insertSlidePptx\": \"replace\" needs \"at\" pointing at an existing slide.".into());
                }
            }
            "setSections" => {
                if !matches!(field(op, "sections"), Some(IpcValue::Array(_))) {
                    return Err("op \"setSections\" needs \"sections\".".into());
                }
            }
            "addSection" | "renameSection" | "removeSection" | "moveSection" => {}
            "findReplace" => {
                if field(op, "find").and_then(string).is_none_or(str::is_empty) {
                    return Err(
                        "op \"findReplace\" needs \"find\": a non-empty search string.".into(),
                    );
                }
                if field(op, "replace").and_then(string).is_none() {
                    return Err("op \"findReplace\" needs \"replace\".".into());
                }
            }
            "setSlideSize" => {
                if !["cx", "cy"]
                    .iter()
                    .all(|k| number(field(op, k)).is_some_and(|n| n.is_finite() && n > 0.0))
                {
                    return Err("op \"setSlideSize\" needs \"cx\"/\"cy\" (EMU).".into());
                }
            }
            "addSlideWithLayout" => {
                if field(op, "target")
                    .and_then(|t| field(t, "slide"))
                    .is_some_and(|v| !matches!(v, IpcValue::Undefined))
                {
                    index(ctx, op)?;
                }
                layout(op, ctx)?;
            }
            _ => {
                index(ctx, op)?;
                match name {
                    "moveSlide" if number(field(op, "to")).is_none() => return Err("op \"moveSlide\" needs \"to\": a slide index.".into()),
                    "setSlideLayout" if has_layout(op) => { layout(op, ctx)?; }
                    "setTransition" if transition(op).is_none() => return Err("op \"setTransition\" needs \"kind\": one of [none, morph, fade, push, wipe, split, circle, cover, pull, dissolve, zoom, random].".into()),
                    "setAdvanceTime" if !matches!(field(op, "ms"), Some(IpcValue::Null)) && !number(field(op, "ms")).is_some_and(|n| n.is_finite() && n >= 0.0) => return Err("op \"setAdvanceTime\" needs \"ms\": non-negative milliseconds, or null to clear.".into()),
                    "setNotes" if field(op, "text").and_then(string).is_none() => return Err("op \"setNotes\" needs \"text\".".into()),
                    _ => {}
                }
            }
        }
        Ok(())
    })())
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    let name = op_name(op);
    if !NAMES.contains(&name) {
        return None;
    }
    Some((|| {
        let mut r = record(op);
        if name == "applyHeaderFooter" {
            let settings = field(op, "settings").unwrap();
            let opts: HeaderFooterOptions =
                serde_json::from_value(json_value(settings)?).map_err(|e| e.to_string())?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_millis() as f64;
            if !apply_header_footer(ctx.opened, &opts, None, now)? {
                return Err(
                    "op \"applyHeaderFooter\": nothing changed (same settings as before).".into(),
                );
            }
            r.after = settings.clone();
            return Ok(r);
        }
        if name == "applyTheme" {
            let spec: ThemeSpec =
                serde_json::from_value(json_value(op)?).map_err(|e| e.to_string())?;
            commit_saved(ctx.opened);
            let patched = apply_theme_to_archive(&mut ctx.opened.archive, &spec);
            let deck = pptx_engine::theme_apply::Deck {
                slides: ctx
                    .opened
                    .deck
                    .slides
                    .iter()
                    .map(|s| pptx_engine::theme_apply::Slide {
                        path: s.path.as_deref().unwrap_or("").to_owned(),
                    })
                    .collect(),
            };
            let remapped = remap_deck_colors(&mut ctx.opened.archive, &deck, &spec);
            let mut backgrounds = 0usize;
            if (patched > 0 || remapped > 0)
                && let Some(lt1) = spec
                    .colors
                    .get("lt1")
                    .and_then(Option::as_deref)
                    .filter(|s| !s.is_empty())
            {
                reparse_deck(ctx.opened)?;
                let color = format!("#{}", lt1.strip_prefix('#').unwrap_or(lt1));
                for i in 0..ctx.opened.deck.slides.len() {
                    if ctx.opened.deck.slides[i].background.is_some() {
                        continue;
                    }
                    background::set_slide_background(
                        ctx.opened,
                        i,
                        &SlideBackgroundFill::Color(color.clone()),
                    )?;
                    materialize_slide(ctx.opened, i)?;
                    backgrounds += 1;
                }
            }
            r.after = obj(vec![
                ("patched", IpcValue::Number(patched as f64)),
                ("remapped", IpcValue::Number(remapped as f64)),
                ("backgrounds", IpcValue::Number(backgrounds as f64)),
            ]);
            return Ok(r);
        }
        if name == "setBackground" {
            let i = index(ctx, op)?;
            let kind = field(op, "kind").and_then(string).unwrap();
            let mut after = vec![("kind", IpcValue::String(kind.to_owned()))];
            match kind {
                "solid" => {
                    let color = field(op, "color").and_then(string).unwrap().to_owned();
                    background::set_slide_background(
                        ctx.opened,
                        i,
                        &SlideBackgroundFill::Color(color.clone()),
                    )?;
                    repaint(ctx, i, Fill::Solid { color });
                }
                "gradient" => {
                    let stops = vec![
                        GradientStop {
                            pos: JsNum(0.0),
                            color: field(op, "from").and_then(string).unwrap().into(),
                        },
                        GradientStop {
                            pos: JsNum(1.0),
                            color: field(op, "to").and_then(string).unwrap().into(),
                        },
                    ];
                    let radial = matches!(field(op, "radial"), Some(IpcValue::Bool(true)));
                    let angle = JsNum(
                        (number(field(op, "angleDeg")).unwrap_or(0.0) * 60000.0 + 0.5).floor(),
                    );
                    let patch = GradientFillPatch {
                        stops: stops.clone(),
                        angle: (!radial).then_some(angle),
                        radial: radial.then_some(true),
                        path: None,
                        fill_to: None,
                    };
                    background::set_slide_background(
                        ctx.opened,
                        i,
                        &SlideBackgroundFill::Gradient(patch),
                    )?;
                    repaint(
                        ctx,
                        i,
                        Fill::Gradient(GradientFill {
                            stops,
                            angle: (!radial).then_some(angle),
                            scaled: None,
                            path: radial.then_some(GradientPath::Circle),
                            fill_to: None,
                            tile_rect: None,
                        }),
                    );
                }
                "image" => {
                    let source = field(op, "source").unwrap();
                    let image = if field(source, "bytes")
                        .is_some_and(|v| !matches!(v, IpcValue::Null | IpcValue::Undefined))
                    {
                        BackgroundImageSource::Bytes {
                            bytes: background_bytes(source)?,
                            ext: background_ext(source).unwrap().into(),
                        }
                    } else {
                        BackgroundImageSource::MediaPath {
                            media_path: field(source, "mediaPath")
                                .and_then(string)
                                .unwrap_or("")
                                .into(),
                        }
                    };
                    let used = background::set_slide_background_image(
                        ctx.opened,
                        i,
                        &image,
                        Some(matches!(field(op, "tile"), Some(IpcValue::Bool(true)))),
                    )?
                    .ok_or("op \"setBackground\": the image could not be landed.")?;
                    repaint(ctx, i, Fill::None);
                    after.push(("mediaPath", IpcValue::String(used)));
                }
                "reset" => background::reset_slide_background(ctx.opened, i)?,
                _ => {
                    let hidden = matches!(field(op, "hidden"), Some(IpcValue::Bool(true)));
                    background::set_slide_bg_graphics_hidden(ctx.opened, i, hidden)?;
                    after.push(("hidden", IpcValue::Bool(hidden)));
                }
            }
            r.after = obj(after);
            return Ok(r);
        }
        if name == "pasteSlide" {
            let after = number(field(op, "afterIndex")).unwrap();
            if field(op, "mode").and_then(string) == Some("picture") {
                let len = ctx.opened.deck.slides.len();
                if len == 0 || !after.is_finite() || after.fract() != 0.0 {
                    return Err("op \"pasteSlide\": no anchor slide for the bitmap.".into());
                }
                let anchor = after.max(0.0).min((len - 1) as f64) as usize;
                let encoded = field(op, "png").and_then(string).unwrap();
                let clean: String = encoded
                    .chars()
                    .filter(|c| {
                        c.is_ascii_alphanumeric()
                            || *c == '+'
                            || *c == '/'
                            || *c == '-'
                            || *c == '_'
                    })
                    .map(|c| match c {
                        '-' => '+',
                        '_' => '/',
                        _ => c,
                    })
                    .collect();
                let data = base64::engine::general_purpose::STANDARD_NO_PAD
                    .decode(clean.trim_end_matches('='))
                    .map_err(|e| e.to_string())?;
                let size = &ctx.opened.deck.size;
                let opts = NewPictureOptions {
                    bytes: data,
                    ext: "png".into(),
                    offset: EmuRect {
                        x: JsNum(0.0),
                        y: JsNum(0.0),
                        cx: size.cx,
                        cy: size.cy,
                    },
                    name: None,
                    descr: None,
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| e.to_string())?
                    .as_millis() as f64;
                let picture =
                    add_picture(ctx.opened, anchor, &opts, &jscompat::random_uuid(), now)?
                        .ok_or("op \"pasteSlide\": the slide bitmap could not be placed.")?;
                r.created = Some(vec![IpcValue::String(
                    picture.id.as_deref().unwrap_or_default().to_owned(),
                )]);
                r.after = idx(anchor);
            } else {
                let bundle: SlideBundle =
                    serde_json::from_value(json_value(field(op, "bundle").unwrap())?)
                        .map_err(|e| e.to_string())?;
                let opts = PasteSlideOptions {
                    keep_source_formatting: Some(
                        field(op, "mode").and_then(string) == Some("source"),
                    ),
                };
                let created = paste_slide(ctx.opened, after, &bundle, Some(&opts))?
                    .ok_or("op \"pasteSlide\": the copied slide could not be pasted.")?;
                r.after = idx(created);
            }
            return Ok(r);
        }
        if name == "insertSlidePptx" {
            let source: MergeSlideSource =
                serde_json::from_value(json_value(field(op, "source").unwrap())?)
                    .map_err(|e| e.to_string())?;
            let total = ctx.opened.deck.slides.len();
            let created =
                merge_slide_from_source(ctx.opened, &source, &MergeSlideOptions::default())?
                    .ok_or("op \"insertSlidePptx\": the source page could not be merged.")?;
            let size = ctx.opened.deck.size;
            let slide = &mut ctx.opened.deck.slides[created];
            promote_slide_background(slide, &size);
            autofit_generated_text_boxes(slide);
            let id = slide_durable_id(slide);
            let at = number(field(op, "at")).map(|n| n as usize);
            if let Some(at) = at.filter(|at| *at < total)
                && !move_slide(
                    ctx.opened,
                    total as f64,
                    at as f64,
                    &mut jscompat::random_uuid,
                )?
            {
                return Err("op \"insertSlidePptx\": positioning the new page failed.".into());
            }
            if matches!(field(op, "replace"), Some(IpcValue::Bool(true)))
                && !delete_slide(ctx.opened, (at.unwrap() + 1) as f64)?
            {
                return Err("op \"insertSlidePptx\": removing the replaced page failed.".into());
            }
            r.created = Some(vec![IpcValue::String(id)]);
            r.after = idx(at.unwrap_or(total));
            return Ok(r);
        }
        if name == "findReplace" {
            let find = field(op, "find").and_then(string).unwrap();
            let replace = field(op, "replace").and_then(string).unwrap();
            let flag = |key| match field(op, key) {
                Some(IpcValue::Bool(b)) => Some(*b),
                _ => None,
            };
            let opts = ReplaceOptions {
                match_case: flag("matchCase"),
                first_only: flag("firstOnly"),
                slide_index: number(field(op, "slideIndex")),
                element_id: field(op, "elementId").and_then(string).map(str::to_owned),
                skip_withheld: Some(ctx.withhold),
            };
            let result = replace_all_in_deck(&mut ctx.opened.deck, find, replace, &opts)?;
            if result.count == 0 {
                return Err(format!(
                    "op \"findReplace\": \"{find}\" was not found anywhere in the deck."
                ));
            }
            r.after = obj(vec![
                ("count", IpcValue::Number(result.count as f64)),
                (
                    "changedSlides",
                    IpcValue::Array(
                        result
                            .changed_slides
                            .into_iter()
                            .map(|i| IpcValue::Number(i as f64))
                            .collect(),
                    ),
                ),
            ]);
            return Ok(r);
        }
        if matches!(
            name,
            "setSections" | "addSection" | "renameSection" | "removeSection" | "moveSection"
        ) {
            let id = field(op, "id").and_then(string).unwrap_or("");
            let label = field(op, "name").and_then(string).unwrap_or("");
            if name == "setSections" {
                let Some(IpcValue::Array(items)) = field(op, "sections") else {
                    unreachable!()
                };
                let mut sections = Vec::with_capacity(items.len());
                for item in items {
                    let indices = match field(item, "slideIndices") {
                        Some(IpcValue::Array(values)) => values
                            .iter()
                            .map(|v| match v {
                                IpcValue::Number(n)
                                    if n.is_finite()
                                        && *n >= 0.0
                                        && n.fract() == 0.0
                                        && *n < usize::MAX as f64 =>
                                {
                                    Ok(*n as usize)
                                }
                                _ => Err("invalid section slide index".to_owned()),
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                        _ => return Err("invalid section slide indices".into()),
                    };
                    sections.push(SectionInfo {
                        id: field(item, "id").and_then(string).unwrap_or("").to_owned(),
                        name: field(item, "name")
                            .and_then(string)
                            .unwrap_or("")
                            .to_owned(),
                        slide_indices: indices,
                    });
                }
                set_sections(ctx.opened, &sections, &mut jscompat::random_uuid)?;
                r.after = IpcValue::Bool(true);
                return Ok(r);
            }
            let result = match name {
                "addSection" => add_section(
                    ctx.opened,
                    number(field(op, "atSlideIndex")).unwrap_or(f64::NAN),
                    label,
                    &mut jscompat::random_uuid,
                )?,
                "renameSection" => {
                    rename_section(ctx.opened, id, label, &mut jscompat::random_uuid)?
                }
                "removeSection" => {
                    remove_section(ctx.opened, id, None, &mut jscompat::random_uuid)?
                }
                _ => move_section(
                    ctx.opened,
                    id,
                    if field(op, "dir").and_then(string) == Some("up") {
                        SectionDirection::Up
                    } else {
                        SectionDirection::Down
                    },
                    &mut jscompat::random_uuid,
                )?,
            };
            let Some(sections) = result else {
                return Err(format!("op \"{name}\": the section change was rejected."));
            };
            r.after = IpcValue::Array(sections.into_iter().map(section_value).collect());
            return Ok(r);
        }
        if name == "setSlideSize" {
            let cx = number(field(op, "cx")).unwrap();
            let cy = number(field(op, "cy")).unwrap();
            r.before = obj(vec![
                ("cx", IpcValue::Number(ctx.opened.deck.size.cx.0)),
                ("cy", IpcValue::Number(ctx.opened.deck.size.cy.0)),
            ]);
            if !set_slide_size(ctx.opened, cx, cy)? {
                return Err("op \"setSlideSize\": the deck size could not be changed.".into());
            }
            r.after = obj(vec![
                ("cx", IpcValue::Number(cx)),
                ("cy", IpcValue::Number(cy)),
            ]);
            return Ok(r);
        }
        let i = if name == "addSlideWithLayout"
            && !field(op, "target")
                .and_then(|t| field(t, "slide"))
                .is_some_and(|v| !matches!(v, IpcValue::Undefined))
        {
            ctx.opened.deck.slides.len() as f64 - 1.0
        } else {
            index(ctx, op)? as f64
        };
        match name {
            "deleteSlide" => {
                if !delete_slide(ctx.opened, i)? {
                    return Err(format!(
                        "op \"deleteSlide\": slide {i} could not be removed."
                    ));
                }
                r.before = idx(i as usize);
            }
            "duplicateSlide" => {
                let opts = DuplicateSlideOptions {
                    clear_text: Some(matches!(field(op, "clearText"), Some(IpcValue::Bool(true)))),
                };
                if duplicate_slide(ctx.opened, i, Some(&opts))?.is_none() {
                    return Err(format!(
                        "op \"duplicateSlide\": slide {i} could not be duplicated."
                    ));
                }
                r.after = idx(i as usize + 1);
            }
            "addBlankSlide" => {
                if insert_blank_slide(ctx.opened, i)?.is_none() {
                    return Err(format!(
                        "op \"addBlankSlide\": a blank slide could not be inserted after {i}."
                    ));
                }
                r.after = idx(i as usize + 1);
            }
            "addSlideWithLayout" => {
                let path = layout(op, ctx)?;
                let Some(created) = insert_slide_with_layout(ctx.opened, i, &path)? else {
                    return Err(format!(
                        "op \"addSlideWithLayout\": layout \"{path}\" could not be applied."
                    ));
                };
                r.created = Some(vec![IpcValue::String(slide_durable_id(
                    &ctx.opened.deck.slides[created],
                ))]);
                r.after = obj(vec![
                    ("index", IpcValue::Number(i + 1.0)),
                    ("layout", IpcValue::String(path)),
                ]);
            }
            "moveSlide" => {
                let to = number(field(op, "to")).unwrap();
                if !move_slide(ctx.opened, i, to, &mut jscompat::random_uuid)? {
                    return Err(format!("op \"moveSlide\": cannot move slide {i} to {to}."));
                }
                r.before = idx(i as usize);
                r.after = obj(vec![("index", IpcValue::Number(to))]);
            }
            "setSlideLayout" => {
                let path = if has_layout(op) {
                    Some(layout(op, ctx)?)
                } else {
                    None
                };
                let applied = if let Some(path) = &path {
                    set_slide_layout(ctx.opened, i, path)?
                } else {
                    reset_slide_layout(ctx.opened, i)?
                };
                if applied.is_none() {
                    let desc = path
                        .as_ref()
                        .map(|p| format!("\"{p}\""))
                        .unwrap_or_else(|| "(reset)".into());
                    return Err(format!(
                        "op \"setSlideLayout\": layout {desc} could not be applied to slide {i}."
                    ));
                }
                r.after = path.map(IpcValue::String).unwrap_or(IpcValue::Null);
            }
            "setHidden" => {
                let hidden = matches!(field(op, "hidden"), Some(IpcValue::Bool(true)));
                set_slide_hidden(&mut ctx.opened.deck.slides[i as usize], hidden);
                r.after = IpcValue::Bool(hidden);
            }
            "setTransition" => {
                set_slide_transition(
                    &mut ctx.opened.deck.slides[i as usize],
                    transition(op).unwrap(),
                );
                r.after = field(op, "kind").unwrap().clone();
            }
            "setAdvanceTime" => {
                let ms = number(field(op, "ms")).map(|n| (n + 0.5).floor());
                set_slide_advance_time(&mut ctx.opened.deck.slides[i as usize], ms);
                r.after = field(op, "ms").unwrap().clone();
            }
            "setNotes" => {
                let text = field(op, "text").and_then(string).unwrap();
                if !set_slide_notes(ctx.opened, i as usize, text)? {
                    return Err(format!(
                        "op \"setNotes\": notes for slide {i} could not be written."
                    ));
                }
                r.after = IpcValue::String(text.into());
            }
            "addComment" => {
                let opts = SlideCommentOptions {
                    author: field(op, "author").and_then(string).unwrap().into(),
                    text: field(op, "text").and_then(string).unwrap().into(),
                    initials: None,
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| e.to_string())?
                    .as_millis() as f64;
                let dt = jscompat::to_iso_string(now).ok_or("RangeError: Invalid time value")?;
                let c =
                    add_slide_comment(ctx.opened, i as usize, &opts, &dt)?.ok_or_else(|| {
                        format!("op \"addComment\": the comment could not be added to slide {i}.")
                    })?;
                r.after = obj(vec![
                    ("authorId", IpcValue::Number(c.author_id.0)),
                    ("author", IpcValue::String(c.author)),
                    ("initials", IpcValue::String(c.initials)),
                    ("dt", IpcValue::String(c.dt)),
                    ("idx", IpcValue::Number(c.idx.0)),
                    ("text", IpcValue::String(c.text)),
                ]);
            }
            "deleteComment" => {
                let author = number(field(op, "authorId")).unwrap();
                let idx = number(field(op, "idx")).unwrap();
                if !delete_slide_comment(
                    ctx.opened,
                    i as usize,
                    &SlideCommentRef {
                        author_id: author,
                        idx,
                    },
                )? {
                    return Err(format!(
                        "op \"deleteComment\": no comment (author {author}, #{idx}) on slide {i}."
                    ));
                }
            }
            _ => unreachable!(),
        }
        Ok(r)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_ipc_d1::slides::engine::TxnRequest;
    use std::collections::HashMap;

    fn deck() -> pptx_engine::deck::OpenedPptx {
        pptx_engine::deck::open_pptx(&pptx_engine::blank::create_blank_pptx(0.0).unwrap()).unwrap()
    }
    fn operation(name: &str, slide: IpcValue, fields: Vec<(&str, IpcValue)>) -> IpcValue {
        let mut entries = vec![
            ("op".to_owned(), IpcValue::String(name.into())),
            ("target".to_owned(), obj(vec![("slide", slide)])),
        ];
        entries.extend(fields.into_iter().map(|(k, v)| (k.to_owned(), v)));
        IpcValue::Object(entries)
    }
    fn run(
        deck: &mut pptx_engine::deck::OpenedPptx,
        ops: Vec<IpcValue>,
    ) -> shell_ipc_d1::slides::engine::TxnResult {
        super::super::run_txn(deck, &mut HashMap::new(), &TxnRequest::atomic(ops))
    }
    #[test]
    fn duplicate_move_by_durable_id_then_delete() {
        let mut deck = deck();
        let original = slide_durable_id(&deck.deck.slides[0]);
        let result = run(
            &mut deck,
            vec![operation("duplicateSlide", IpcValue::Number(0.0), vec![])],
        );
        assert!(result.applied);
        assert_eq!(deck.deck.slides.len(), 2);
        let duplicate = slide_durable_id(&deck.deck.slides[1]);
        assert_ne!(original, duplicate);
        let result = run(
            &mut deck,
            vec![operation(
                "moveSlide",
                IpcValue::String(duplicate.clone()),
                vec![("to", IpcValue::Number(0.0))],
            )],
        );
        assert!(result.applied);
        assert_eq!(
            result.records.unwrap()[0].slide_id.as_deref(),
            Some(duplicate.as_str())
        );
        assert_eq!(slide_durable_id(&deck.deck.slides[0]), duplicate);
        assert!(
            run(
                &mut deck,
                vec![operation("deleteSlide", IpcValue::String(original), vec![])]
            )
            .applied
        );
        assert_eq!(deck.deck.slides.len(), 1);
    }
    #[test]
    fn layout_size_and_master_target_validation() {
        let mut deck = deck();
        let add = operation(
            "addSlideWithLayout",
            IpcValue::Number(0.0),
            vec![("layout", IpcValue::Number(0.0))],
        );
        let result = run(&mut deck, vec![add]);
        assert!(result.applied);
        assert_eq!(
            result.records.unwrap()[0].created.as_ref().unwrap().len(),
            1
        );
        assert!(
            run(
                &mut deck,
                vec![operation(
                    "setSlideLayout",
                    IpcValue::Number(1.0),
                    vec![("layout", IpcValue::Number(0.0))]
                )]
            )
            .applied
        );
        assert!(
            run(
                &mut deck,
                vec![operation(
                    "setSlideSize",
                    IpcValue::Number(0.0),
                    vec![
                        ("cx", IpcValue::Number(9000000.0)),
                        ("cy", IpcValue::Number(6000000.0))
                    ]
                )]
            )
            .applied
        );
        assert_eq!(deck.deck.size.cx.0, 9000000.0);
        let master = IpcValue::Object(vec![
            ("op".into(), IpcValue::String("setHidden".into())),
            (
                "target".into(),
                obj(vec![(
                    "part",
                    IpcValue::String("ppt/slideMasters/slideMaster1.xml".into()),
                )]),
            ),
        ]);
        assert!(
            run(&mut deck, vec![master]).failures.unwrap()[0]
                .error
                .contains("cannot target a master/layout part")
        );
    }
    #[test]
    fn failed_atomic_delete_preserves_original_deck() {
        let mut deck = deck();
        let before = deck.deck.clone();
        let entries = deck.archive.entries().clone();
        let result = run(
            &mut deck,
            vec![operation("deleteSlide", IpcValue::Number(0.0), vec![])],
        );
        assert!(!result.applied);
        assert!(
            result.failures.unwrap()[0]
                .error
                .contains("could not be removed")
        );
        assert_eq!(deck.deck, before);
        assert!(deck.archive.entries().iter().eq(entries.iter()));
    }
}
