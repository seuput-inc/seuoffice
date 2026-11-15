use jscompat::{JsNum, js_regex};
use pptx_engine::animation::{self, AnimClass, AnimDirection, AnimEffectKind as Effect, AnimTrigger, MediaKind, SlideAnimation};
use pptx_engine::types::{ElementType, MediaKind as PictureMediaKind, SlideElement};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;

use super::registry::{field, op_name, record, resolve_element, resolve_slide, string, OpContext};

const EFFECTS: &[(&str, Effect)] = &[
    ("appear", Effect::Appear), ("fade", Effect::Fade), ("flyIn", Effect::FlyIn),
    ("wipe", Effect::Wipe), ("wipeDown", Effect::WipeDown), ("splitIn", Effect::SplitIn),
    ("bounce", Effect::Bounce), ("flipIn", Effect::FlipIn), ("zoom", Effect::Zoom),
    ("pulse", Effect::Pulse), ("spin", Effect::Spin), ("grow", Effect::Grow),
    ("teeter", Effect::Teeter), ("disappear", Effect::Disappear), ("fadeOut", Effect::FadeOut),
    ("flyOut", Effect::FlyOut), ("wipeOut", Effect::WipeOut), ("shrink", Effect::Shrink),
    ("zoomOut", Effect::ZoomOut), ("motionPath", Effect::MotionPath),
    ("mediaPause", Effect::MediaPause), ("mediaPlay", Effect::MediaPlay), ("mediaStop", Effect::MediaStop),
];

fn names() -> String { EFFECTS.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ") }
fn effect_name(effect: Effect) -> &'static str { EFFECTS.iter().find(|(_, e)| *e == effect).map(|(name, _)| *name).expect("all animation effects have names") }
fn folded(s: &str) -> String { s.to_lowercase().chars().filter(char::is_ascii_lowercase).collect() }
fn number(value: Option<&IpcValue>) -> Option<f64> { match value { Some(IpcValue::Number(n)) => Some(*n), _ => None } }
fn missing(value: Option<&IpcValue>) -> bool { matches!(value, None | Some(IpcValue::Undefined)) }

fn sequence(op: &IpcValue, key: &str, required: bool) -> Result<Option<usize>, String> {
    let value = field(op, key);
    if missing(value) {
        return if required { Err(format!("op \"{}\" needs \"{key}\": a 0-based timeline position.", op_name(op))) } else { Ok(None) };
    }
    match number(value) {
        Some(n) if n.is_finite() && n >= 0.0 && n.fract() == 0.0 => Ok(Some(n as usize)),
        _ => Err(format!("op \"{}\": \"{key}\" must be a 0-based integer timeline position.", op_name(op))),
    }
}

fn milliseconds(op: &IpcValue, key: &str, fallback: f64) -> Result<JsNum, String> {
    let named = format!("{key}Ms");
    let value = field(op, &named).filter(|v| !matches!(v, IpcValue::Null | IpcValue::Undefined)).or_else(|| field(op, key));
    if missing(value) { return Ok(JsNum(fallback)); }
    match number(value) {
        Some(n) if n.is_finite() && n >= 0.0 => Ok(JsNum(jscompat::math_round(n))),
        _ => Err(format!("op \"{}\": \"{key}\" must be >= 0 milliseconds.", op_name(op))),
    }
}

fn parse_effect(op: &IpcValue) -> Result<Option<Effect>, String> {
    let value = field(op, "effect");
    if missing(value) { return Ok(None); }
    let Some(s) = value.and_then(string) else { return Err(format!("op \"{}\": \"effect\" must be one of [{}].", op_name(op), names())); };
    let lower = folded(s);
    let direct = EFFECTS.iter().find(|(name, _)| *name == s || name.to_lowercase() == lower).map(|(_, e)| *e);
    let alias = match lower.as_str() {
        "fly" | "flyin" | "float" | "floatin" => Some(Effect::FlyIn), "fadein" => Some(Effect::Fade),
        "wipein" => Some(Effect::Wipe), "split" => Some(Effect::SplitIn), "flip" => Some(Effect::FlipIn),
        "zoomin" => Some(Effect::Zoom), "fadeout" => Some(Effect::FadeOut), "flyout" => Some(Effect::FlyOut),
        "wipeout" => Some(Effect::WipeOut), "zoomout" => Some(Effect::ZoomOut), "path" | "motion" => Some(Effect::MotionPath),
        "play" => Some(Effect::MediaPlay), "pause" => Some(Effect::MediaPause), "stop" => Some(Effect::MediaStop), _ => None,
    };
    direct.or(alias).map(Some).ok_or_else(|| format!("op \"{}\": unknown effect {}. Use one of [{}].", op_name(op), serde_json::to_string(s).unwrap_or_default(), names()))
}

fn preset(op: &IpcValue, spid: f64) -> Result<Option<(String, String)>, String> {
    let value = field(op, "presetXml");
    if missing(value) { return Ok(None); }
    let xml = value.and_then(string).unwrap_or("").trim();
    let name = op_name(op);
    if !js_regex!(r"^<p:par\b[\s\S]*</p:par>$").test(xml) {
        return Err(format!("op \"{name}\": \"presetXml\" must be one effect <p:par>…</p:par> block as PowerPoint writes it inside the main sequence."));
    }
    let class = js_regex!(r#"<p:cTn\b[^>]*\bpresetClass="([^"]*)""#).exec(xml).and_then(|m| m.get(1).map(str::to_owned));
    if class.as_deref().is_none_or(str::is_empty) || !js_regex!(r#"<p:cTn\b[^>]*\bpresetID="\d+""#).test(xml) {
        return Err(format!("op \"{name}\": \"presetXml\" needs a <p:cTn> with presetID and presetClass attributes."));
    }
    let ids: Vec<f64> = js_regex!(r#"<p:spTgt\s[^>]*\bspid="(\d+)""#, "g").find_iter(xml).filter_map(|m| m.get(1).and_then(|s| s.parse().ok())).collect();
    if ids.is_empty() || ids.iter().any(|id| *id != spid) {
        let found = ids.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
        return Err(format!("op \"{name}\": every <p:spTgt spid> in \"presetXml\" must be {spid} (the target element's shape id); found [{found}]."));
    }
    let opens = js_regex!(r"<p:par\b", "g").find_iter(xml).count();
    let closes = js_regex!(r"</p:par>", "g").find_iter(xml).count();
    if opens != closes { return Err(format!("op \"{name}\": \"presetXml\" is not balanced.")); }
    Ok(Some((xml.to_owned(), class.unwrap_or_default())))
}

fn build(op: &IpcValue, el: &SlideElement) -> Result<SlideAnimation, String> {
    let name = op_name(op);
    let id = pptx_engine::with_element!(el, e => e.id.as_deref().unwrap_or_default());
    let spid = animation::element_spid(el).ok_or_else(|| format!("op \"{name}\": element \"{id}\" has no shape id to animate."))?;
    let preset = preset(op, spid.0)?;
    let effect = match parse_effect(op)? {
        Some(effect) => effect,
        None => match preset.as_ref().map(|(_, cls)| cls.as_str()) {
            Some("emph") => Effect::Pulse, Some("exit") => Effect::FadeOut, Some("path") => Effect::MotionPath,
            Some("mediacall") => Effect::MediaPlay, Some(_) => Effect::Fade,
            None => return Err(format!("op \"{name}\" needs \"effect\": one of [{}] (or \"presetXml\").", names())),
        },
    };
    if let Some(value) = field(op, "kind").filter(|v| !matches!(v, IpcValue::Undefined)) {
        let want = string(value).unwrap_or("");
        let class = animation::anim_class_of(effect);
        let have = match class { AnimClass::Entrance => "entrance", AnimClass::Emphasis => "emphasis", AnimClass::Exit => "exit", AnimClass::Path => "path", AnimClass::Media => "media" };
        if want != have && !(want == "motion" && have == "path") {
            let matching = EFFECTS.iter().filter(|(_, e)| match animation::anim_class_of(*e) { AnimClass::Entrance => want == "entrance", AnimClass::Emphasis => want == "emphasis", AnimClass::Exit => want == "exit", AnimClass::Path => want == "path", AnimClass::Media => want == "media" }).map(|(n, _)| *n).collect::<Vec<_>>().join(", ");
            return Err(format!("op \"{name}\": \"{}\" is an {have} effect, not {want}; pick an effect of that class ({}).", effect_name(effect), if matching.is_empty() { "none" } else { &matching }));
        }
    }
    let trigger = match field(op, "trigger") {
        None | Some(IpcValue::Undefined) => AnimTrigger::OnClick,
        Some(IpcValue::String(s)) => match folded(s).as_str() {
            "onclick" | "click" => AnimTrigger::OnClick, "withprev" | "withprevious" | "with" => AnimTrigger::WithPrev,
            "afterprev" | "afterprevious" | "after" => AnimTrigger::AfterPrev,
            _ => return Err(format!("op \"{name}\": \"trigger\" must be one of [onClick, withPrev, afterPrev] (default onClick).")),
        },
        _ => return Err(format!("op \"{name}\": \"trigger\" must be one of [onClick, withPrev, afterPrev] (default onClick).")),
    };
    let default = match effect { Effect::Appear | Effect::Disappear | Effect::MediaPlay | Effect::MediaPause | Effect::MediaStop => 0.0, Effect::Spin | Effect::Grow | Effect::Bounce | Effect::MotionPath => 2000.0, Effect::Pulse | Effect::Teeter => 1000.0, _ => 500.0 };
    let duration_ms = milliseconds(op, "duration", default)?;
    let delay_ms = milliseconds(op, "delay", 0.0)?;
    let media_kind = if animation::is_media_effect(effect) {
        match el {
            SlideElement::Picture(p) if p.media.is_some() => Some(match p.media.as_ref().unwrap().kind { PictureMediaKind::Video => MediaKind::Video, PictureMediaKind::Audio => MediaKind::Audio }),
            _ => {
                let kind = pptx_engine::with_element!(el, e => match e.r#type.as_ref() {
                    Some(ElementType::Text) => "text", Some(ElementType::Shape) => "shape",
                    Some(ElementType::Picture) => "picture", Some(ElementType::Group) => "group",
                    Some(ElementType::Table) => "table", Some(ElementType::Chart) => "chart",
                    Some(ElementType::Passthrough) => "passthrough", None => "undefined",
                });
                return Err(format!("op \"{name}\": \"{}\" only applies to a video or audio element; \"{id}\" is a {kind}.", effect_name(effect)));
            }
        }
    } else { None };
    let direction = match field(op, "direction") {
        None | Some(IpcValue::Undefined) => None,
        value => {
            let d = match value.and_then(string) { Some("top") => AnimDirection::Top, Some("bottom") => AnimDirection::Bottom, Some("left") => AnimDirection::Left, Some("right") => AnimDirection::Right, _ => return Err(format!("op \"{name}\": \"direction\" must be one of [top, bottom, left, right].")) };
            if !matches!(effect, Effect::FlyIn | Effect::FlyOut | Effect::Wipe | Effect::WipeOut) { return Err(format!("op \"{name}\": \"direction\" only applies to flyIn/flyOut/wipe/wipeOut, not {}.", effect_name(effect))); }
            Some(d)
        }
    };
    let paragraph = match field(op, "paragraph") { None | Some(IpcValue::Undefined) => None, v => match number(v) { Some(n) if n.is_finite() && n >= 0.0 && n.fract() == 0.0 => Some(JsNum(n)), _ => return Err(format!("op \"{name}\": \"paragraph\" must be a 0-based paragraph index.")) } };
    let motion_path = match field(op, "motionPath") { None | Some(IpcValue::Undefined) => None, value => {
        let path = value.and_then(string).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| format!("op \"{name}\": \"motionPath\" must be an SVG-like path (M/L/C/Z, coordinates 0..1 of the slide)."))?;
        if effect != Effect::MotionPath { return Err(format!("op \"{name}\": \"motionPath\" only applies to effect \"motionPath\".")); }
        Some(path.to_owned())
    }};
    Ok(SlideAnimation { spid, effect, trigger, duration_ms, delay_ms, direction, preset_xml: preset.map(|p| p.0), preset: None, paragraph, motion_path, media_kind })
}

fn range(anims: &[SlideAnimation]) -> String { if anims.is_empty() { "empty (no animations on this slide)".to_owned() } else { format!("0-{}", anims.len() - 1) } }
fn result_fields(seq: usize, count: usize) -> IpcValue { IpcValue::Object(vec![("seq".into(), IpcValue::Number(seq as f64)), ("count".into(), IpcValue::Number(count as f64))]) }
fn removed_fields(removed: usize, count: usize) -> IpcValue { IpcValue::Object(vec![("removed".into(), IpcValue::Number(removed as f64)), ("count".into(), IpcValue::Number(count as f64))]) }
fn animation_value(a: &SlideAnimation) -> IpcValue {
    let mut v = vec![("spid".into(), IpcValue::Number(a.spid.0)), ("effect".into(), IpcValue::String(effect_name(a.effect).into())), ("trigger".into(), IpcValue::String(match a.trigger { AnimTrigger::OnClick => "onClick", AnimTrigger::WithPrev => "withPrev", AnimTrigger::AfterPrev => "afterPrev" }.into())), ("durationMs".into(), IpcValue::Number(a.duration_ms.0)), ("delayMs".into(), IpcValue::Number(a.delay_ms.0))];
    if let Some(d) = a.direction { v.push(("direction".into(), IpcValue::String(match d { AnimDirection::Top => "top", AnimDirection::Bottom => "bottom", AnimDirection::Left => "left", AnimDirection::Right => "right" }.into()))); }
    if let Some(xml) = &a.preset_xml { v.push(("presetXml".into(), IpcValue::String(xml.clone()))); }
    if let Some(p) = &a.preset { v.push(("preset".into(), IpcValue::Object(vec![("id".into(), IpcValue::Number(p.id.0)), ("cls".into(), IpcValue::String(p.cls.clone())), ("sub".into(), IpcValue::Number(p.sub.0))]))); }
    if let Some(p) = a.paragraph { v.push(("paragraph".into(), IpcValue::Number(p.0))); }
    if let Some(p) = &a.motion_path { v.push(("motionPath".into(), IpcValue::String(p.clone()))); }
    if let Some(k) = a.media_kind { v.push(("mediaKind".into(), IpcValue::String(match k { MediaKind::Video => "video", MediaKind::Audio => "audio" }.into()))); }
    IpcValue::Object(v)
}

pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    Some(match op_name(op) {
        "addAnimation" => (|| { let (target, index) = resolve_element(ctx, op, false)?; build(op, &ctx.slide(&target).elements.as_deref().unwrap_or(&[])[index])?; sequence(op, "after", false)?; Ok(()) })(),
        "removeAnimation" => (|| { if sequence(op, "seq", false)?.is_some() { resolve_slide(ctx, op, false)?; } else { resolve_element(ctx, op, false)?; } Ok(()) })(),
        "reorderAnimation" => (|| { resolve_slide(ctx, op, false)?; sequence(op, "seq", true)?; sequence(op, "to", true)?; Ok(()) })(),
        _ => return None,
    })
}

pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    Some((|| {
        let mut result = record(op);
        match op_name(op) {
            "addAnimation" => {
                let (target, index) = resolve_element(ctx, op, false)?;
                let anim = build(op, &ctx.slide(&target).elements.as_deref().unwrap_or(&[])[index])?;
                let mut anims = animation::get_slide_animations(ctx.slide(&target));
                let after = sequence(op, "after", false)?;
                if after.is_some_and(|n| n >= anims.len()) { return Err(format!("op \"addAnimation\": \"after\" {} is past the timeline ({}); omit it to append.", after.unwrap_or_default(), range(&anims))); }
                let seq = after.map_or(anims.len(), |n| n + 1);
                anims.insert(seq, anim);
                animation::set_slide_animations(ctx.slide_mut(&target), &anims)?;
                result.after = result_fields(seq, anims.len());
            }
            "removeAnimation" => {
                let seq = sequence(op, "seq", false)?;
                let (target, spid) = if seq.is_some() { (resolve_slide(ctx, op, false)?, None) } else { let (target, index) = resolve_element(ctx, op, false)?; let spid = animation::element_spid(&ctx.slide(&target).elements.as_deref().unwrap_or(&[])[index]); (target, spid) };
                let mut anims = animation::get_slide_animations(ctx.slide(&target));
                if let Some(seq) = seq {
                    if seq >= anims.len() { return Err(format!("op \"removeAnimation\": no animation at seq {seq}; the timeline is {}.", range(&anims))); }
                    result.before = animation_value(&anims.remove(seq));
                    animation::set_slide_animations(ctx.slide_mut(&target), &anims)?;
                    result.after = removed_fields(1, anims.len());
                } else {
                    let old = anims.len();
                    anims.retain(|a| Some(a.spid) != spid);
                    if old != anims.len() { animation::set_slide_animations(ctx.slide_mut(&target), &anims)?; }
                    result.after = removed_fields(old - anims.len(), anims.len());
                }
            }
            "reorderAnimation" => {
                let target = resolve_slide(ctx, op, false)?;
                let seq = sequence(op, "seq", true)?.unwrap_or_default();
                let to = sequence(op, "to", true)?.unwrap_or_default();
                let mut anims = animation::get_slide_animations(ctx.slide(&target));
                if seq >= anims.len() || to >= anims.len() { return Err(format!("op \"reorderAnimation\": seq/to must be inside the timeline ({}).", range(&anims))); }
                if seq != to { let item = anims.remove(seq); anims.insert(to, item); animation::set_slide_animations(ctx.slide_mut(&target), &anims)?; }
                result.after = result_fields(to, anims.len());
            }
            _ => return Ok(None),
        }
        Ok(Some(result))
    })().and_then(|r| r.ok_or_else(|| "unknown animation operation".to_owned())))
        .filter(|_| matches!(op_name(op), "addAnimation" | "removeAnimation" | "reorderAnimation"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pptx_engine::types::Slide;

    fn op(fields: Vec<(&str, IpcValue)>) -> IpcValue {
        let mut entries = vec![("op".to_owned(), IpcValue::String("addAnimation".into()))];
        entries.extend(fields.into_iter().map(|(k, v)| (k.to_owned(), v)));
        IpcValue::Object(entries)
    }

    fn element(kind: &str, media: &str) -> SlideElement {
        let json = format!(r#"{{"id":"shape","type":"{kind}","anchor":{{"spIndex":0,"originalXml":"<p:sp><p:cNvPr id=\"7\"/></p:sp>","range":[0,37]}},"transform":{{"offset":{{"x":0,"y":0,"cx":10,"cy":10}},"rot":0,"flipH":false,"flipV":false}}{media}}}"#);
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn animation_roundtrips_through_slide_timing() {
        let shape = element("shape", "");
        let input = op(vec![("effect", IpcValue::String("fly-in".into())), ("durationMs", IpcValue::Number(499.5)), ("direction", IpcValue::String("left".into()))]);
        let anim = build(&input, &shape).unwrap();
        assert_eq!(anim.effect, Effect::FlyIn);
        assert_eq!(anim.duration_ms.0, 500.0);
        let mut slide: Slide = serde_json::from_str(r#"{"path":"ppt/slides/slide1.xml","originalXml":"<p:sld/>","bodyPrefix":"","bodySuffix":"</p:spTree></p:cSld></p:sld>","elements":[]}"#).unwrap();
        animation::set_slide_animations(&mut slide, &[anim]).unwrap();
        let read = animation::get_slide_animations(&slide);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].spid.0, 7.0);
        assert_eq!(read[0].effect, Effect::FlyIn);
        assert_eq!(read[0].direction, Some(AnimDirection::Left));
    }

    #[test]
    fn media_and_preset_targets_are_checked() {
        let shape = element("shape", "");
        let play = op(vec![("effect", IpcValue::String("play".into()))]);
        assert!(build(&play, &shape).unwrap_err().contains("video or audio"));
        let video = element("picture", r#","media":{"kind":"video"}"#);
        assert_eq!(build(&play, &video).unwrap().media_kind, Some(MediaKind::Video));
        let xml = r#"<p:par><p:cTn presetID="1" presetClass="entr"/><p:spTgt spid="8"/></p:par>"#;
        let invalid = op(vec![("presetXml", IpcValue::String(xml.into()))]);
        assert!(build(&invalid, &shape).unwrap_err().contains("must be 7"));
    }

    #[test]
    fn invalid_time_and_timeline_indices_are_rejected() {
        let shape = element("shape", "");
        let input = op(vec![("effect", IpcValue::String("fade".into())), ("delay", IpcValue::Number(f64::NAN))]);
        assert!(build(&input, &shape).unwrap_err().contains("delay"));
        let input = op(vec![("after", IpcValue::Number(1.5))]);
        assert!(sequence(&input, "after", false).is_err());
    }

    #[test]
    fn operations_update_timeline_and_return_removed_animation() {
        let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
        let mut opened = pptx_engine::deck::open_pptx(&bytes).unwrap();
        opened.deck.slides[0].elements.as_mut().unwrap().push(element("shape", ""));
        let mut ctx = OpContext { opened: &mut opened, parts: Default::default(), touched_parts: Default::default(), withhold: false };
        let target = IpcValue::Object(vec![("slide".into(), IpcValue::Number(0.0)), ("el".into(), IpcValue::String("shape".into()))]);
        let add = op(vec![("target", target.clone()), ("effect", IpcValue::String("fade".into()))]);
        validate(&add, &mut ctx).unwrap().unwrap();
        let added = apply(&add, &mut ctx).unwrap().unwrap();
        assert_eq!(added.after, result_fields(0, 1));
        let add_second = op(vec![("target", target.clone()), ("effect", IpcValue::String("spin".into())), ("after", IpcValue::Number(0.0))]);
        apply(&add_second, &mut ctx).unwrap().unwrap();
        let reorder = IpcValue::Object(vec![("op".into(), IpcValue::String("reorderAnimation".into())), ("target".into(), target.clone()), ("seq".into(), IpcValue::Number(1.0)), ("to".into(), IpcValue::Number(0.0))]);
        apply(&reorder, &mut ctx).unwrap().unwrap();
        let remove = IpcValue::Object(vec![("op".into(), IpcValue::String("removeAnimation".into())), ("target".into(), target), ("seq".into(), IpcValue::Number(0.0))]);
        let removed = apply(&remove, &mut ctx).unwrap().unwrap();
        assert_eq!(field(&removed.before, "effect").and_then(string), Some("spin"));
        assert_eq!(removed.after, removed_fields(1, 1));
        let remaining = animation::get_slide_animations(&ctx.opened.deck.slides[0]);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].effect, Effect::Fade);
    }
}
