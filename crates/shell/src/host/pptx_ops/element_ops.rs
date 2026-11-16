use pptx_engine::deck::{
    ConnectionPatch, EffectsPatch, GlowPatch, ImageFillOpts, ImageFillSource, InsetsPatch,
    ReflectionPatch, ReorderDirection, ShadowPatch, TextBodyPropsPatch, VertPatch,
    edit_group_child_transform, edit_picture_src_rect, find_group_child, group_elements,
    materialize_slide, reorder_element, resize_table, set_element_connection, set_element_effects,
    set_element_hidden, set_element_image_fill, set_element_name, set_element_redaction,
    set_element_text_anchor, set_element_text_body_props, set_group_child_shape_adjust_values,
    set_group_child_shape_custom_geometry, set_group_child_shape_preset_geometry,
    set_picture_opacity, set_shape_adjust_values, set_shape_custom_geometry,
    set_shape_preset_geometry, ungroup_element, update_connectors_for_moved,
};
use pptx_engine::identity::{element_cnvpr_id, matches_element_ref};
use pptx_engine::types::{
    Autofit, ConnectionEnd, ElementType, EmuRect, Insets, Slide, SlideElement, TextAnchor,
};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;

use super::registry::{
    OpContext, SlideTarget, field, op_name, record, resolve_element, resolve_group,
    resolve_group_child_id, resolve_slide, string,
};

fn object(entries: Vec<(&str, IpcValue)>) -> IpcValue {
    IpcValue::Object(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}

fn group(ctx: &OpContext<'_>, op: &IpcValue, target: &SlideTarget) -> Result<String, String> {
    resolve_group(op, target, ctx.slide(target))
}

fn child_id(
    ctx: &OpContext<'_>,
    target: &SlideTarget,
    group_id: &str,
    reference: &str,
) -> Result<String, String> {
    resolve_group_child_id(ctx.slide(target), group_id, reference)
}

fn direction(op: &IpcValue) -> Option<ReorderDirection> {
    match field(op, "dir").and_then(string) {
        Some("front") => Some(ReorderDirection::Front),
        Some("back") => Some(ReorderDirection::Back),
        Some("forward") => Some(ReorderDirection::Forward),
        Some("backward") => Some(ReorderDirection::Backward),
        _ => None,
    }
}

fn anchor(op: &IpcValue) -> Option<TextAnchor> {
    match field(op, "anchor").and_then(string) {
        Some("top") => Some(TextAnchor::Top),
        Some("middle") => Some(TextAnchor::Middle),
        Some("bottom") => Some(TextAnchor::Bottom),
        _ => None,
    }
}

fn crop(op: &IpcValue) -> Result<Option<Insets>, String> {
    let rect = match field(op, "srcRect") {
        Some(IpcValue::Null) => return Ok(None),
        Some(IpcValue::Object(_)) => field(op, "srcRect").expect("present"),
        _ => return Err("op \"setPictureSrcRect\" needs \"srcRect\": fractions {l, t, r, b}, or null to remove the crop.".into()),
    };
    let mut values = [0.0; 4];
    for (i, side) in ["l", "t", "r", "b"].iter().enumerate() {
        match field(rect, side) {
            None | Some(IpcValue::Undefined) => {}
            Some(IpcValue::Number(v)) if v.is_finite() => {
                if *v < 0.0 || *v >= 1.0 {
                    return Err(format!(
                        "op \"setPictureSrcRect\": \"srcRect.{side}\" must be a fraction 0..1."
                    ));
                }
                values[i] = *v;
            }
            _ => {
                return Err(format!(
                    "op \"setPictureSrcRect\": \"srcRect.{side}\" must be a finite number."
                ));
            }
        }
    }
    if values[0] + values[2] >= 1.0 || values[1] + values[3] >= 1.0 {
        return Err(
            "op \"setPictureSrcRect\": opposing crops must leave a visible strip (< 1 combined)."
                .into(),
        );
    }
    Ok(Some(Insets {
        l: jscompat::JsNum(values[0]),
        t: jscompat::JsNum(values[1]),
        r: jscompat::JsNum(values[2]),
        b: jscompat::JsNum(values[3]),
    }))
}

fn rect(op: &IpcValue, key: &str) -> Result<EmuRect, String> {
    let value = field(op, key);
    let mut v = [0.0; 4];
    for (i, part) in ["x", "y", "cx", "cy"].iter().enumerate() {
        let Some(IpcValue::Number(n)) = value.and_then(|b| field(b, part)) else {
            return Err(format!(
                "op \"{}\" needs \"{key}\": an EMU rect {{x, y, cx, cy}}.",
                op_name(op)
            ));
        };
        v[i] = *n;
    }
    for (i, part) in ["x", "y", "cx", "cy"].iter().enumerate() {
        if !v[i].is_finite() {
            return Err(format!(
                "op \"{}\": \"{key}.{part}\" must be a finite number.",
                op_name(op)
            ));
        }
    }
    if v[2] < 0.0 || v[3] < 0.0 {
        return Err(format!(
            "op \"{}\": \"{key}.cx/cy\" must be >= 0 (EMU).",
            op_name(op)
        ));
    }
    v = v.map(jscompat::math_round);
    Ok(EmuRect {
        x: jscompat::JsNum(v[0]),
        y: jscompat::JsNum(v[1]),
        cx: jscompat::JsNum(v[2]),
        cy: jscompat::JsNum(v[3]),
    })
}

fn rect_value(rect: EmuRect) -> IpcValue {
    object(vec![
        ("x", IpcValue::Number(rect.x.0)),
        ("y", IpcValue::Number(rect.y.0)),
        ("cx", IpcValue::Number(rect.cx.0)),
        ("cy", IpcValue::Number(rect.cy.0)),
    ])
}

fn with_rotation(rect: EmuRect, key: &str, rotation: f64) -> IpcValue {
    let IpcValue::Object(mut fields) = rect_value(rect) else {
        unreachable!()
    };
    fields.push((key.into(), IpcValue::Number(rotation)));
    IpcValue::Object(fields)
}

fn has_group(op: &IpcValue) -> bool {
    match field(op, "group") {
        None | Some(IpcValue::Undefined | IpcValue::Null | IpcValue::Bool(false)) => false,
        Some(IpcValue::String(s)) => !s.is_empty(),
        Some(IpcValue::Number(n)) => *n != 0.0 && !n.is_nan(),
        _ => true,
    }
}

fn point(op: &IpcValue, key: &str) -> Result<(f64, f64), String> {
    let p = field(op, key);
    match (p.and_then(|v| field(v, "x")), p.and_then(|v| field(v, "y"))) {
        (Some(IpcValue::Number(x)), Some(IpcValue::Number(y)))
            if x.is_finite() && y.is_finite() =>
        {
            Ok((*x, *y))
        }
        _ => Err(format!(
            "op \"setConnectorEndpoints\" needs \"{key}\": an EMU point {{x, y}}."
        )),
    }
}

fn connection_end(
    slide: &Slide,
    op: &IpcValue,
    side: &str,
) -> Result<Option<Option<ConnectionEnd>>, String> {
    let value = match field(op, side) {
        None | Some(IpcValue::Undefined) => return Ok(None),
        Some(IpcValue::Null) => return Ok(Some(None)),
        Some(v) => v,
    };
    let id = field(value, "targetId").and_then(string).unwrap_or("");
    let mut spid = None;
    for el in slide.elements.as_ref().into_iter().flatten() {
        if matches_element_ref(el, id)? {
            spid = element_cnvpr_id(el)?
                .and_then(|s| s.strip_prefix("e_").and_then(|s| s.parse::<f64>().ok()));
            break;
        }
    }
    let spid = spid.ok_or_else(|| format!("op \"setConnectorEndpoints\": {side}.targetId \"{id}\" does not resolve on this slide (null detaches explicitly)."))?;
    let idx = match field(value, "idx") {
        Some(IpcValue::Number(n)) => *n,
        _ => f64::NAN,
    };
    Ok(Some(Some(ConnectionEnd {
        id: jscompat::JsNum(spid),
        idx: jscompat::JsNum(idx),
    })))
}

fn js_string(value: &IpcValue) -> String {
    match value {
        IpcValue::String(s) => s.clone(),
        IpcValue::Number(n) => jscompat::number_to_string(*n),
        IpcValue::Bool(b) => b.to_string(),
        IpcValue::Null => "null".into(),
        IpcValue::Undefined => "undefined".into(),
        IpcValue::Object(_) => "[object Object]".into(),
        IpcValue::Array(values) => values
            .iter()
            .map(|v| match v {
                IpcValue::Null | IpcValue::Undefined => String::new(),
                _ => js_string(v),
            })
            .collect::<Vec<_>>()
            .join(","),
        IpcValue::Bytes(_, bytes) => bytes
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(","),
        IpcValue::Date(_) => "[object Date]".into(),
    }
}

fn ids(op: &IpcValue) -> Vec<String> {
    match field(op, "els") {
        Some(IpcValue::Array(v)) => v.iter().map(js_string).collect(),
        _ => Vec::new(),
    }
}

fn adjust(op: &IpcValue) -> Result<jscompat::JsObject<f64>, String> {
    let Some(IpcValue::Object(fields)) = field(op, "adjust") else {
        return Err("op \"setShapeAdjust\" needs \"adjust\": a non-empty { gdName: number } map of avLst values.".into());
    };
    if fields.is_empty() {
        return Err("op \"setShapeAdjust\" needs \"adjust\": a non-empty { gdName: number } map of avLst values.".into());
    }
    let mut out = jscompat::JsObject::new();
    for (key, value) in fields {
        let IpcValue::Number(n) = value else {
            return Err("op \"setShapeAdjust\" needs \"adjust\": a non-empty { gdName: number } map of avLst values.".into());
        };
        if !n.is_finite() {
            return Err("op \"setShapeAdjust\" needs \"adjust\": a non-empty { gdName: number } map of avLst values.".into());
        }
        out.set(key.clone(), *n);
    }
    Ok(out)
}

fn custom_path(op: &IpcValue) -> Result<serde_json::Value, String> {
    let invalid = || {
        "op \"setShapeCustomGeometry\" needs \"path\": { w, h, cmds:[{op:\"M\"|\"L\"|\"C\"|\"Q\"|\"Z\", pts:number[]}] } starting with \"M\" (pts: M/L 2, Q 4, C 6, Z 0).".to_owned()
    };
    let p = field(op, "path").ok_or_else(invalid)?;
    let num = |key| match field(p, key) {
        Some(IpcValue::Number(n)) if n.is_finite() && *n > 0.0 => Some(*n),
        _ => None,
    };
    let (w, h) = (num("w").ok_or_else(invalid)?, num("h").ok_or_else(invalid)?);
    let Some(IpcValue::Array(cmds)) = field(p, "cmds") else {
        return Err(invalid());
    };
    if cmds.is_empty() {
        return Err(invalid());
    }
    if matches!(cmds.first(), Some(IpcValue::Null)) {
        return Err("TypeError: Cannot read properties of null (reading 'op')".into());
    }
    let mut normalized = Vec::with_capacity(cmds.len());
    for (i, cmd) in cmds.iter().enumerate() {
        let name = field(cmd, "op").and_then(string).ok_or_else(invalid)?;
        let count = match name {
            "M" | "L" => 2,
            "C" => 6,
            "Q" => 4,
            "Z" => 0,
            _ => return Err(invalid()),
        };
        if i == 0 && name != "M" {
            return Err(invalid());
        }
        let Some(IpcValue::Array(pts)) = field(cmd, "pts") else {
            return Err(invalid());
        };
        if pts.len() != count {
            return Err(invalid());
        }
        let mut numbers = Vec::with_capacity(count);
        for p in pts {
            match p {
                IpcValue::Number(n) if n.is_finite() => numbers.push(*n),
                _ => return Err(invalid()),
            }
        }
        normalized.push(serde_json::json!({"op":name,"pts":numbers}));
    }
    Ok(serde_json::json!({"w":w,"h":h,"cmds":normalized}))
}

fn number(value: &IpcValue, key: &str) -> Option<f64> {
    match field(value, key) {
        Some(IpcValue::Number(n)) => Some(*n),
        _ => None,
    }
}

fn boolean(value: &IpcValue, key: &str) -> Option<bool> {
    match field(value, key) {
        Some(IpcValue::Bool(v)) => Some(*v),
        _ => None,
    }
}

fn effects(op: &IpcValue) -> Result<EffectsPatch, String> {
    let p = field(op, "effects").ok_or_else(|| "op \"setEffects\" needs \"effects\" with at least one of shadow/glow/reflection/softEdge (null clears).".to_owned())?;
    if ["shadow", "glow", "reflection", "softEdge"]
        .iter()
        .all(|key| matches!(field(p, key), None | Some(IpcValue::Undefined)))
    {
        return Err("op \"setEffects\" needs \"effects\" with at least one of shadow/glow/reflection/softEdge (null clears).".into());
    }
    let color = |v: &IpcValue| -> Result<String, String> {
        let c = field(v, "color")
            .and_then(string)
            .ok_or_else(|| "op \"setEffects\": \"color\" must be a hex color.".to_owned())?;
        let hex = c.strip_prefix('#').unwrap_or(c);
        if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("op \"setEffects\": \"color\" must be a hex color.".into());
        }
        Ok(c.to_owned())
    };
    let required = |v: &IpcValue, k: &str| {
        number(v, k).ok_or_else(|| format!("op \"setEffects\": \"{k}\" must be a number."))
    };
    let shadow = match field(p, "shadow") {
        None | Some(IpcValue::Undefined) => None,
        Some(IpcValue::Null) => Some(None),
        Some(v) => Some(Some(ShadowPatch {
            color: color(v)?,
            blur_rad: required(v, "blurRad")?,
            dist: required(v, "dist")?,
            dir_deg: required(v, "dirDeg")?,
            inner: boolean(v, "inner"),
            sx: number(v, "sx"),
            sy: number(v, "sy"),
            kx_deg: number(v, "kxDeg"),
            ky_deg: number(v, "kyDeg"),
            algn: field(v, "algn").and_then(string).map(str::to_owned),
        })),
    };
    let glow = match field(p, "glow") {
        None | Some(IpcValue::Undefined) => None,
        Some(IpcValue::Null) => Some(None),
        Some(v) => Some(Some(GlowPatch {
            color: color(v)?,
            radius: required(v, "radius")?,
        })),
    };
    let reflection = match field(p, "reflection") {
        None | Some(IpcValue::Undefined) => None,
        Some(IpcValue::Null) => Some(None),
        Some(v) => Some(Some(ReflectionPatch {
            blur_rad: required(v, "blurRad")?,
            start_a: required(v, "startA")?,
            end_pos: required(v, "endPos")?,
            dist: required(v, "dist")?,
        })),
    };
    let soft_edge = match field(p, "softEdge") {
        None | Some(IpcValue::Undefined) => None,
        Some(IpcValue::Null) => Some(None),
        Some(IpcValue::Number(n)) => Some(Some(*n)),
        _ => return Err("op \"setEffects\": \"softEdge\" must be a number.".into()),
    };
    Ok(EffectsPatch {
        shadow,
        glow,
        reflection,
        soft_edge,
    })
}

fn text_props(op: &IpcValue) -> Result<TextBodyPropsPatch, String> {
    let p = field(op, "props").ok_or_else(|| {
        "op \"setTextBodyProps\" needs \"props\" with at least one of vert/autofit/insets/wrap."
            .to_owned()
    })?;
    if ["vert", "autofit", "insets", "wrap"].iter().all(|k| {
        matches!(
            field(p, k),
            None | Some(IpcValue::Undefined | IpcValue::Null)
        )
    }) {
        return Err("op \"setTextBodyProps\" needs \"props\" with at least one of vert/autofit/insets/wrap.".into());
    }
    let vert =
        match field(p, "vert").and_then(string) {
            None | Some("") => None,
            Some("horz") => Some(VertPatch::Horz),
            Some("eaVert") => Some(VertPatch::EaVert),
            Some("vert") => Some(VertPatch::Vert),
            Some("vert270") => Some(VertPatch::Vert270),
            Some("wordArtVert") => Some(VertPatch::WordArtVert),
            _ => return Err(
                "op \"setTextBodyProps\": \"vert\" must be horz/eaVert/vert/vert270/wordArtVert."
                    .into(),
            ),
        };
    let autofit = match field(p, "autofit").and_then(string) {
        None | Some("") => None,
        Some("none") => Some(Autofit::None),
        Some("shrink") => Some(Autofit::Shrink),
        Some("resize") => Some(Autofit::Resize),
        _ => return Err("op \"setTextBodyProps\": \"autofit\" must be none/shrink/resize.".into()),
    };
    let insets = field(p, "insets")
        .filter(|v| !matches!(v, IpcValue::Undefined | IpcValue::Null))
        .map(|v| InsetsPatch {
            l: number(v, "l"),
            t: number(v, "t"),
            r: number(v, "r"),
            b: number(v, "b"),
        });
    Ok(TextBodyPropsPatch {
        vert,
        autofit,
        insets,
        wrap: boolean(p, "wrap"),
    })
}

fn image_source(op: &IpcValue) -> Result<ImageFillSource, String> {
    use base64::Engine;
    let source = field(op, "source").ok_or_else(|| "op \"setImageFill\" needs \"source\": {bytes, ext} or {mediaPath} of an already-landed image.".to_owned())?;
    if let Some(value) =
        field(source, "bytes").filter(|v| !matches!(v, IpcValue::Null | IpcValue::Undefined))
    {
        let mut inferred = None;
        let bytes = match value {
            IpcValue::Bytes(shell_contract::BytesKind::Uint8Array, bytes) if !bytes.is_empty() => bytes.clone(),
            IpcValue::String(s) if !s.is_empty() => {
                let encoded = if let Some(data) = s.strip_prefix("data:") {
                    let split = data.find(',').unwrap_or(data.len());
                    let head = &data[..split];
                    let mime = head.split(';').next().unwrap_or("");
                    inferred = match mime { "image/png" => Some("png"), "image/jpeg" => Some("jpg"), "image/gif" => Some("gif"), "image/bmp" => Some("bmp"), "image/webp" => Some("webp"), "image/tiff" => Some("tif"), "video/mp4" => Some("mp4"), "video/quicktime" => Some("mov"), "video/webm" => Some("webm"), "audio/mpeg" => Some("mp3"), "audio/wav" => Some("wav"), "audio/mp4" => Some("m4a"), _ => None };
                    data.get(split.saturating_add(1)..).unwrap_or("")
                } else { s.as_str() };
                let clean: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
                let data = clean.trim_end_matches('=');
                if data.is_empty() || clean.len() - data.len() > 2 || !data.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/') {
                    return Err("op \"setImageFill\" needs \"source.bytes\": image/media bytes (base64 string, data URL, or Uint8Array).".into());
                }
                base64::engine::general_purpose::STANDARD_NO_PAD.decode(data).map_err(|_| "op \"setImageFill\" needs \"source.bytes\": image/media bytes (base64 string, data URL, or Uint8Array).".to_owned())?
            }
            _ => return Err("op \"setImageFill\" needs \"source.bytes\": image/media bytes (base64 string, data URL, or Uint8Array).".into()),
        };
        if bytes.is_empty() {
            return Err("op \"setImageFill\" needs \"source.bytes\": image/media bytes (base64 string, data URL, or Uint8Array).".into());
        }
        let ext = field(source, "ext")
            .and_then(string)
            .filter(|s| !s.is_empty())
            .or(inferred)
            .ok_or_else(|| {
                "op \"setImageFill\" needs \"source.ext\": the image extension.".to_owned()
            })?;
        return Ok(ImageFillSource::Bytes {
            bytes,
            ext: ext.to_owned(),
        });
    }
    let media_path = field(source, "mediaPath").and_then(string).filter(|s| !s.is_empty())
        .ok_or_else(|| "op \"setImageFill\" needs \"source\": {bytes, ext} or {mediaPath} of an already-landed image.".to_owned())?;
    Ok(ImageFillSource::MediaPath {
        media_path: media_path.to_owned(),
    })
}

fn link(op: &IpcValue) -> Result<Option<pptx_engine::hyperlink::LinkTarget>, String> {
    use pptx_engine::hyperlink::LinkTarget;
    let usage = || {
        "op \"setLink\" \"link\" must be {kind:\"url\",url}, {kind:\"slide\",slideIndex}, {kind:\"action\",action} or null.".to_owned()
    };
    let v = field(op, "link").ok_or_else(usage)?;
    if matches!(v, IpcValue::Null) {
        return Ok(None);
    }
    let tooltip = field(v, "tooltip").and_then(string).map(str::to_owned);
    Ok(Some(match field(v, "kind").and_then(string) {
        Some("url") => LinkTarget::Url {
            url: field(v, "url")
                .and_then(string)
                .filter(|s| !s.is_empty())
                .ok_or_else(usage)?
                .to_owned(),
            tooltip,
        },
        Some("slide") => {
            let n = number(v, "slideIndex")
                .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0)
                .ok_or_else(usage)?;
            LinkTarget::Slide {
                slide_index: jscompat::JsNum(n),
                tooltip,
            }
        }
        Some("action") => {
            let action = field(v, "action").and_then(string).unwrap_or("");
            if !pptx_engine::named_action::NAMED_ACTIONS.contains(&action) {
                let got = field(v, "action")
                    .and_then(string)
                    .map(|s| format!("\"{s}\""))
                    .unwrap_or_else(|| "undefined".to_owned());
                return Err(format!(
                    "op \"setLink\" \"link.action\" must be one of {} (got {got}).",
                    pptx_engine::named_action::NAMED_ACTIONS.join(", ")
                ));
            }
            LinkTarget::Action {
                action: action.to_owned(),
                tooltip,
            }
        }
        _ => return Err(usage()),
    }))
}

pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    let name = op_name(op);
    if !matches!(
        name,
        "setLink"
            | "setImageFill"
            | "setEffects"
            | "setTextBodyProps"
            | "setShapeAdjust"
            | "setShapeCustomGeometry"
            | "groupElements"
            | "ungroupElement"
            | "flipElements"
            | "setConnectorEndpoints"
            | "setTransform"
            | "setPictureSrcRect"
            | "setRedaction"
            | "setPictureOpacity"
            | "reorderElement"
            | "setShapeGeometry"
            | "setTextAnchor"
            | "setElementHidden"
            | "renameElement"
    ) {
        return None;
    }
    Some((|| {
        if name == "setLink" {
            link(op)?;
        }
        if name == "setImageFill" {
            image_source(op)?;
        }
        if name == "setShapeAdjust" {
            adjust(op)?;
        }
        if name == "setShapeCustomGeometry" {
            custom_path(op)?;
        }
        if matches!(name, "groupElements" | "flipElements") {
            resolve_slide(ctx, op, false)?;
            let len = match field(op, "els") {
                Some(IpcValue::Array(v)) => v.len(),
                _ => 0,
            };
            if name == "groupElements" && len < 2 {
                return Err("op \"groupElements\" needs \"els\": at least two element ids.".into());
            }
            if name == "flipElements" {
                if len == 0 {
                    return Err(
                        "op \"flipElements\" needs \"els\": a non-empty element id array.".into(),
                    );
                }
                if !matches!(field(op, "axis").and_then(string), Some("h" | "v")) {
                    return Err("op \"flipElements\" needs \"axis\": \"h\" or \"v\".".into());
                }
            }
            return Ok(());
        }
        if name == "setTransform" {
            let key = if has_group(op)
                && field(op, "absBox").is_some_and(|v| !matches!(v, IpcValue::Undefined))
            {
                "absBox"
            } else {
                "box"
            };
            rect(op, key)?;
        }
        match name {
            "setShapeGeometry"
                if field(op, "prst")
                    .and_then(string)
                    .filter(|v| !v.is_empty())
                    .is_none() =>
            {
                return Err(
                    "op \"setShapeGeometry\" needs \"prst\": an OOXML preset geometry name.".into(),
                );
            }
            "setElementHidden" if !matches!(field(op, "hidden"), Some(IpcValue::Bool(_))) => {
                return Err(
                    "op \"setElementHidden\" needs \"hidden\": true to hide, false to show.".into(),
                );
            }
            "renameElement"
                if !field(op, "name")
                    .and_then(string)
                    .is_some_and(|v| !v.trim().is_empty() && v.encode_utf16().count() <= 255) =>
            {
                return Err(
                    "op \"renameElement\" needs \"name\": a non-empty string up to 255 characters."
                        .into(),
                );
            }
            _ => {}
        }
        if matches!(
            name,
            "setTransform"
                | "setShapeGeometry"
                | "setShapeAdjust"
                | "setShapeCustomGeometry"
                | "setImageFill"
        ) && has_group(op)
        {
            let target = resolve_slide(ctx, op, false)?;
            group(ctx, op, &target)?;
            return Ok(());
        }
        let (target, index) = resolve_element(ctx, op, name == "setTransform")?;
        let kind = ctx
            .slide(&target)
            .elements
            .as_ref()
            .expect("resolved element")[index]
            .element_type();
        if name == "setShapeCustomGeometry"
            && !matches!(kind, Some(ElementType::Text | ElementType::Shape))
        {
            let kind = kind.map_or("undefined".to_owned(), |v| {
                format!("{v:?}").to_ascii_lowercase()
            });
            return Err(format!(
                "op \"setShapeCustomGeometry\" targets a {kind} element — it only works on text/shape elements."
            ));
        }
        if name == "ungroupElement" && kind != Some(ElementType::Group) {
            let kind = kind.map_or("undefined".to_owned(), |v| {
                format!("{v:?}").to_ascii_lowercase()
            });
            return Err(format!(
                "op \"ungroupElement\" targets a {kind} element — it only works on group elements."
            ));
        }
        if name == "setConnectorEndpoints" {
            let el = &ctx
                .slide(&target)
                .elements
                .as_ref()
                .expect("resolved element")[index];
            let xml = pptx_engine::with_element!(el, e => e.anchor.as_ref().map_or("", |a| &*a.original_xml));
            if !pptx_engine::generate::is_connector_xml(xml) {
                return Err(format!(
                    "op \"setConnectorEndpoints\": element \"{}\" is not a connector.",
                    el.id()
                ));
            }
            point(op, "p1")?;
            point(op, "p2")?;
        }
        if name == "setPictureSrcRect" {
            crop(op)?;
            if field(op, "box").is_some_and(|v| !matches!(v, IpcValue::Undefined)) {
                rect(op, "box")?;
            }
        }
        if name == "setEffects" {
            effects(op)?;
        }
        if name == "setTextBodyProps" {
            text_props(op)?;
        }
        match name {
            "setRedaction" if !matches!(field(op, "label"), None | Some(IpcValue::Undefined | IpcValue::Null | IpcValue::String(_))) => Err("op \"setRedaction\" needs \"label\": a non-empty string, or null to stop withholding.".into()),
            "setPictureOpacity" if !matches!(field(op, "opacity"), Some(IpcValue::Number(n)) if n.is_finite()) => Err("op \"setPictureOpacity\": \"opacity\" must be a finite number.".into()),
            "reorderElement" if direction(op).is_none() => Err("op \"reorderElement\" needs \"dir\": front/back/forward/backward.".into()),
            "setTextAnchor" if anchor(op).is_none() => Err("op \"setTextAnchor\" needs \"anchor\": top/middle/bottom.".into()),
            _ => Ok(()),
        }
    })())
}

pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    let name = op_name(op);
    if !matches!(
        name,
        "setLink"
            | "setImageFill"
            | "setEffects"
            | "setTextBodyProps"
            | "setShapeAdjust"
            | "setShapeCustomGeometry"
            | "groupElements"
            | "ungroupElement"
            | "flipElements"
            | "setConnectorEndpoints"
            | "setTransform"
            | "setPictureSrcRect"
            | "setRedaction"
            | "setPictureOpacity"
            | "reorderElement"
            | "setShapeGeometry"
            | "setTextAnchor"
            | "setElementHidden"
            | "renameElement"
    ) {
        return None;
    }
    Some((|| {
        if name == "setImageFill" {
            let target = resolve_slide(ctx, op, false)?;
            let SlideTarget::Slide(index) = &target else {
                unreachable!()
            };
            let index = *index;
            let group_id = if has_group(op) {
                Some(group(ctx, op, &target)?)
            } else {
                None
            };
            let raw = field(op, "target")
                .and_then(|v| field(v, "el"))
                .and_then(string)
                .unwrap_or("");
            let id = if let Some(group_id) = &group_id {
                child_id(ctx, &target, group_id, raw)?
            } else {
                let (_, i) = resolve_element(ctx, op, false)?;
                ctx.slide(&target)
                    .elements
                    .as_ref()
                    .expect("resolved element")[i]
                    .id()
                    .to_owned()
            };
            let opts = ImageFillOpts {
                group_id,
                tile: Some(matches!(field(op, "tile"), Some(IpcValue::Bool(true)))),
            };
            let used = set_element_image_fill(ctx.opened, index, &id, &image_source(op)?, &opts)?
                .ok_or_else(|| {
                format!("op \"setImageFill\": element \"{id}\" does not support a picture fill.")
            })?;
            let mut result = record(op);
            result.after = object(vec![("mediaPath", IpcValue::String(used))]);
            return Ok(result);
        }
        if name == "setLink" {
            let (target, index) = resolve_element(ctx, op, false)?;
            let id = ctx
                .slide(&target)
                .elements
                .as_ref()
                .expect("resolved element")[index]
                .id()
                .to_owned();
            let SlideTarget::Slide(index) = target else {
                unreachable!()
            };
            if !pptx_engine::hyperlink::set_element_link(
                ctx.opened,
                index,
                &id,
                link(op)?.as_ref(),
            )? {
                return Err(format!(
                    "op \"setLink\": element \"{id}\" does not accept links."
                ));
            }
            let mut result = record(op);
            result.after = field(op, "link").cloned().unwrap_or(IpcValue::Undefined);
            return Ok(result);
        }
        if matches!(name, "setShapeAdjust" | "setShapeCustomGeometry") && has_group(op) {
            let target = resolve_slide(ctx, op, false)?;
            let group_id = group(ctx, op, &target)?;
            let raw = field(op, "target")
                .and_then(|v| field(v, "el"))
                .and_then(string)
                .unwrap_or("");
            let id = child_id(ctx, &target, &group_id, raw)?;
            let success = if name == "setShapeAdjust" {
                let values = adjust(op)?;
                let mut typed = jscompat::JsObject::new();
                for (key, value) in values.entries() {
                    typed.set(key, jscompat::JsNum(*value));
                }
                set_group_child_shape_adjust_values(ctx.slide_mut(&target), &group_id, &id, &typed)?
            } else {
                set_group_child_shape_custom_geometry(
                    ctx.slide_mut(&target),
                    &group_id,
                    &id,
                    &custom_path(op)?,
                )?
            };
            if !success {
                return Err(if name == "setShapeAdjust" {
                    format!(
                        "op \"setShapeAdjust\": no child \"{id}\" in group \"{group_id}\", or it has no preset geometry."
                    )
                } else {
                    format!(
                        "op \"setShapeCustomGeometry\": no shape child \"{id}\" in group \"{group_id}\"."
                    )
                });
            }
            let mut result = record(op);
            result.after = field(
                op,
                if name == "setShapeAdjust" {
                    "adjust"
                } else {
                    "path"
                },
            )
            .cloned()
            .unwrap_or(IpcValue::Undefined);
            return Ok(result);
        }
        if name == "groupElements" {
            let SlideTarget::Slide(index) = resolve_slide(ctx, op, false)? else {
                unreachable!()
            };
            let id = group_elements(ctx.opened, index, &ids(op))?.ok_or_else(|| "op \"groupElements\": the elements could not be grouped (check the ids exist and are groupable).".to_owned())?;
            let mut result = record(op);
            result.created = Some(vec![IpcValue::String(id)]);
            return Ok(result);
        }
        if name == "ungroupElement" {
            let (target, index) = resolve_element(ctx, op, false)?;
            let id = ctx
                .slide(&target)
                .elements
                .as_ref()
                .expect("resolved element")[index]
                .id()
                .to_owned();
            let SlideTarget::Slide(slide) = target else {
                unreachable!()
            };
            if !ungroup_element(ctx.opened, slide, &id)? {
                return Err(format!(
                    "op \"ungroupElement\": group \"{id}\" could not be dissolved."
                ));
            }
            let mut result = record(op);
            result.before = object(vec![("group", IpcValue::String(id))]);
            return Ok(result);
        }
        if name == "flipElements" {
            let target = resolve_slide(ctx, op, false)?;
            let requested = ids(op);
            let group_id = if has_group(op) {
                Some(group(ctx, op, &target)?)
            } else {
                None
            };
            let mut positions = Vec::new();
            for raw in &requested {
                if let Some(group_id) = &group_id {
                    let child = child_id(ctx, &target, group_id, raw)?;
                    if let Some((g, c)) = find_group_child(ctx.slide(&target), group_id, &child) {
                        positions.push((g, Some(c)));
                    }
                } else {
                    for (i, el) in ctx
                        .slide(&target)
                        .elements
                        .as_ref()
                        .into_iter()
                        .flatten()
                        .enumerate()
                    {
                        if matches_element_ref(el, raw)? {
                            positions.push((i, None));
                            break;
                        }
                    }
                }
            }
            if positions.is_empty() {
                let index = match target {
                    SlideTarget::Slide(i) => i,
                    SlideTarget::Part(_) => unreachable!(),
                };
                return Err(format!(
                    "op \"flipElements\": none of [{}] are flippable on slide {index}.",
                    requested.join(", ")
                ));
            }
            let horizontal = field(op, "axis").and_then(string) == Some("h");
            let slide = ctx.slide_mut(&target);
            let mut moved = Vec::new();
            for (i, child) in positions {
                let el = &mut slide.elements.as_mut().expect("resolved elements")[i];
                let el = if let Some(c) = child {
                    let SlideElement::Group(g) = el else {
                        unreachable!()
                    };
                    &mut g.children.as_mut().expect("resolved group")[c]
                } else {
                    el
                };
                if pptx_engine::with_element!(&*el, e => e.transform.as_ref().is_some()) {
                    moved.push(el.id().to_owned());
                    pptx_engine::with_element!(el, e => {
                        if let Some(t) = e.transform.as_mut() { if horizontal { t.flip_h = !t.flip_h; } else { t.flip_v = !t.flip_v; } }
                        e.dirty_transform.set(true);
                    });
                }
            }
            if moved.is_empty() {
                let index = match target {
                    SlideTarget::Slide(i) => i,
                    SlideTarget::Part(_) => unreachable!(),
                };
                return Err(format!(
                    "op \"flipElements\": none of [{}] are flippable on slide {index}.",
                    requested.join(", ")
                ));
            }
            update_connectors_for_moved(slide, &moved);
            let mut result = record(op);
            result.after = object(vec![("flipped", IpcValue::Number(moved.len() as f64))]);
            return Ok(result);
        }
        if name == "setShapeGeometry" && has_group(op) {
            let target = resolve_slide(ctx, op, false)?;
            let group_id = group(ctx, op, &target)?;
            let raw_id = field(op, "target")
                .and_then(|v| field(v, "el"))
                .and_then(string)
                .unwrap_or("");
            let id = child_id(ctx, &target, &group_id, raw_id)?;
            let prst = field(op, "prst")
                .and_then(string)
                .ok_or_else(|| "invalid geometry".to_owned())?;
            if !set_group_child_shape_preset_geometry(ctx.slide_mut(&target), &group_id, &id, prst)?
            {
                return Err(format!(
                    "op \"setShapeGeometry\": no child \"{id}\" in group \"{group_id}\", or it has no preset geometry."
                ));
            }
            let mut result = record(op);
            result.after = IpcValue::String(prst.to_owned());
            return Ok(result);
        }
        if name == "setTransform" && has_group(op) {
            let target = resolve_slide(ctx, op, false)?;
            let group_id = group(ctx, op, &target)?;
            let raw = field(op, "target")
                .and_then(|v| field(v, "el"))
                .and_then(string)
                .unwrap_or("");
            let id = child_id(ctx, &target, &group_id, raw)?;
            let rotation = match field(op, "rotDeg") {
                Some(IpcValue::Number(n)) => *n,
                _ => 0.0,
            };
            let bounds = if field(op, "absBox").is_some_and(|v| !matches!(v, IpcValue::Undefined)) {
                let abs = rect(op, "absBox")?;
                let slide = ctx.slide(&target);
                let (gi, _) = find_group_child(slide, &group_id, &id).ok_or_else(|| {
                    format!("op \"setTransform\": no child \"{id}\" in group \"{group_id}\".")
                })?;
                let SlideElement::Group(g) = &slide.elements.as_ref().expect("resolved group")[gi]
                else {
                    unreachable!()
                };
                let ext = g
                    .transform
                    .as_ref()
                    .ok_or_else(|| "group has no transform".to_owned())?
                    .offset;
                let child = g.child_offset.as_ref();
                let cx = child.map_or(ext.x.0, |r| r.x.0);
                let cy = child.map_or(ext.y.0, |r| r.y.0);
                let sx = child
                    .filter(|r| r.cx.0 != 0.0)
                    .map_or(1.0, |r| ext.cx.0 / r.cx.0);
                let sy = child
                    .filter(|r| r.cy.0 != 0.0)
                    .map_or(1.0, |r| ext.cy.0 / r.cy.0);
                EmuRect {
                    x: jscompat::JsNum(jscompat::math_round((abs.x.0 - ext.x.0) / sx) + cx),
                    y: jscompat::JsNum(jscompat::math_round((abs.y.0 - ext.y.0) / sy) + cy),
                    cx: jscompat::JsNum(jscompat::math_round(abs.cx.0 / sx)),
                    cy: jscompat::JsNum(jscompat::math_round(abs.cy.0 / sy)),
                }
            } else {
                rect(op, "box")?
            };
            if !edit_group_child_transform(
                ctx.slide_mut(&target),
                &group_id,
                &id,
                bounds,
                rotation,
            )? {
                return Err(format!(
                    "op \"setTransform\": the child slice for \"{id}\" could not be located inside group \"{group_id}\"."
                ));
            }
            let mut result = record(op);
            result.after = with_rotation(bounds, "rotDeg", rotation);
            return Ok(result);
        }
        let (target, index) = resolve_element(ctx, op, name == "setTransform")?;
        let el = &ctx
            .slide(&target)
            .elements
            .as_ref()
            .expect("resolved element")[index];
        let id = el.id().to_owned();
        let old_name = if name == "renameElement" {
            pptx_engine::with_element!(el, e => (*e.name).clone())
        } else {
            None
        };
        let mut result = record(op);
        let after = match name {
            "setEffects" => {
                if !set_element_effects(ctx.slide_mut(&target), &id, &effects(op)?)? {
                    return Err(format!(
                        "op \"setEffects\": element \"{id}\" does not support effects."
                    ));
                }
                field(op, "effects").cloned().unwrap_or(IpcValue::Undefined)
            }
            "setTextBodyProps" => {
                if !set_element_text_body_props(ctx.slide_mut(&target), &id, &text_props(op)?) {
                    return Err(format!(
                        "op \"setTextBodyProps\": element \"{id}\" has no text body."
                    ));
                }
                field(op, "props").cloned().unwrap_or(IpcValue::Undefined)
            }
            "setShapeAdjust" => {
                if !set_shape_adjust_values(ctx.slide_mut(&target), &id, &adjust(op)?) {
                    return Err(format!(
                        "op \"setShapeAdjust\": element \"{id}\" has no preset geometry."
                    ));
                }
                field(op, "adjust").cloned().unwrap_or(IpcValue::Undefined)
            }
            "setShapeCustomGeometry" => {
                if !set_shape_custom_geometry(ctx.slide_mut(&target), &id, Some(&custom_path(op)?))?
                {
                    return Err(format!(
                        "op \"setShapeCustomGeometry\": element \"{id}\" has no geometry."
                    ));
                }
                field(op, "path").cloned().unwrap_or(IpcValue::Undefined)
            }
            "setConnectorEndpoints" => {
                let (x1, y1) = point(op, "p1")?;
                let (x2, y2) = point(op, "p2")?;
                let patch = ConnectionPatch {
                    start: connection_end(ctx.slide(&target), op, "start")?,
                    end: connection_end(ctx.slide(&target), op, "end")?,
                };
                let slide = ctx.slide_mut(&target);
                let el = &mut slide.elements.as_mut().expect("resolved element")[index];
                let before = pptx_engine::with_element!(&*el, e => *e.transform)
                    .ok_or_else(|| "connector has no transform".to_owned())?;
                result.before = rect_value(before.offset);
                pptx_engine::with_element!(el, e => {
                    if let Some(t) = e.transform.as_mut() {
                        t.offset = EmuRect { x: jscompat::JsNum(x1.min(x2)), y: jscompat::JsNum(y1.min(y2)),
                            cx: jscompat::JsNum((x2-x1).abs()), cy: jscompat::JsNum((y2-y1).abs()) };
                        t.rot = jscompat::JsNum(0.0);
                        t.flip_h = x1 > x2;
                        t.flip_v = y1 > y2;
                    }
                    e.dirty_transform.set(true);
                });
                set_element_connection(slide, &id, &patch);
                object(vec![
                    (
                        "p1",
                        field(op, "p1").cloned().unwrap_or(IpcValue::Undefined),
                    ),
                    (
                        "p2",
                        field(op, "p2").cloned().unwrap_or(IpcValue::Undefined),
                    ),
                ])
            }
            "setTransform" => {
                let bounds = rect(op, "box")?;
                let rotation = match field(op, "rotDeg") {
                    Some(IpcValue::Number(n)) => *n,
                    _ => 0.0,
                };
                let slide = ctx.slide_mut(&target);
                let before_transform = pptx_engine::with_element!(&slide.elements.as_ref().expect("resolved element")[index], e => *e.transform)
                    .ok_or_else(|| "element has no transform".to_owned())?;
                result.before =
                    with_rotation(before_transform.offset, "rot", before_transform.rot.0);
                let is_table = slide.elements.as_ref().expect("resolved element")[index]
                    .element_type()
                    == Some(ElementType::Table)
                    && matches!(field(op, "resizeTableGrid"), Some(IpcValue::Bool(true)));
                if is_table {
                    resize_table(slide, &id, bounds.cx.0, bounds.cy.0);
                }
                let el = &mut slide.elements.as_mut().expect("resolved element")[index];
                pptx_engine::with_element!(el, e => {
                    if let Some(t) = e.transform.as_mut() {
                        let (cx, cy) = if is_table { (t.offset.cx, t.offset.cy) } else { (bounds.cx, bounds.cy) };
                        t.offset = EmuRect { cx, cy, ..bounds };
                        t.rot = jscompat::JsNum(jscompat::math_round(rotation * 60000.0));
                    }
                    e.dirty_transform.set(true);
                });
                update_connectors_for_moved(slide, std::slice::from_ref(&id));
                with_rotation(bounds, "rotDeg", rotation)
            }
            "setPictureSrcRect" => {
                let src = crop(op)?;
                let box_value =
                    if field(op, "box").is_some_and(|v| !matches!(v, IpcValue::Undefined)) {
                        Some(rect(op, "box")?)
                    } else {
                        None
                    };
                if !edit_picture_src_rect(ctx.slide_mut(&target), &id, src) {
                    return Err(format!(
                        "op \"setPictureSrcRect\": element \"{id}\" is not a croppable picture."
                    ));
                }
                if let Some(bounds) = box_value {
                    let slide = ctx.slide_mut(&target);
                    let el = &mut slide.elements.as_mut().expect("resolved element")[index];
                    pptx_engine::with_element!(el, e => {
                        if let Some(t) = e.transform.as_mut() { t.offset = bounds; }
                        e.dirty_transform.set(true);
                    });
                    update_connectors_for_moved(slide, std::slice::from_ref(&id));
                }
                field(op, "srcRect").cloned().unwrap_or(IpcValue::Undefined)
            }
            "setRedaction" => {
                let label = field(op, "label").and_then(string);
                if !set_element_redaction(ctx.slide_mut(&target), &id, label)? {
                    return Err(format!(
                        "op \"setRedaction\": element \"{id}\" is not a picture, or it already has that state. Text is withheld through the text-editing surface, which marks runs rather than the element."
                    ));
                }
                label.map_or(IpcValue::Null, |s| IpcValue::String(s.to_owned()))
            }
            "setPictureOpacity" => {
                let Some(IpcValue::Number(n)) = field(op, "opacity") else {
                    return Err("invalid opacity".into());
                };
                if !set_picture_opacity(ctx.slide_mut(&target), &id, *n) {
                    return Err(format!(
                        "op \"setPictureOpacity\": element \"{id}\" is not a picture."
                    ));
                }
                IpcValue::Number(*n)
            }
            "reorderElement" => {
                let dir = direction(op).ok_or_else(|| "invalid direction".to_owned())?;
                if !reorder_element(ctx.slide_mut(&target), &id, dir) {
                    return Err(format!(
                        "op \"reorderElement\": element \"{id}\" is already at that extreme."
                    ));
                }
                field(op, "dir").cloned().unwrap_or(IpcValue::Undefined)
            }
            "setShapeGeometry" => {
                let prst = field(op, "prst")
                    .and_then(string)
                    .ok_or_else(|| "invalid geometry".to_owned())?;
                if !set_shape_preset_geometry(ctx.slide_mut(&target), &id, prst) {
                    return Err(format!(
                        "op \"setShapeGeometry\": element \"{id}\" has no preset geometry."
                    ));
                }
                IpcValue::String(prst.to_owned())
            }
            "setTextAnchor" => {
                let value = anchor(op).ok_or_else(|| "invalid anchor".to_owned())?;
                if !set_element_text_anchor(ctx.slide_mut(&target), &id, value) {
                    return Err(format!(
                        "op \"setTextAnchor\": element \"{id}\" has no text body."
                    ));
                }
                field(op, "anchor").cloned().unwrap_or(IpcValue::Undefined)
            }
            "setElementHidden" => {
                let hidden = matches!(field(op, "hidden"), Some(IpcValue::Bool(true)));
                if !set_element_hidden(ctx.slide_mut(&target), &id, hidden)? {
                    return Err(format!(
                        "op \"setElementHidden\": element \"{id}\" has no shape properties."
                    ));
                }
                if let SlideTarget::Slide(i) = target {
                    materialize_slide(ctx.opened, i)?;
                }
                object(vec![("hidden", IpcValue::Bool(hidden))])
            }
            "renameElement" => {
                let value = field(op, "name")
                    .and_then(string)
                    .ok_or_else(|| "invalid name".to_owned())?;
                if !set_element_name(ctx.slide_mut(&target), &id, value)? {
                    return Err(format!(
                        "op \"renameElement\": element \"{id}\" has no shape properties."
                    ));
                }
                result.before = object(vec![(
                    "name",
                    old_name.map_or(IpcValue::Undefined, IpcValue::String),
                )]);
                object(vec![("name", IpcValue::String(value.to_owned()))])
            }
            _ => unreachable!(),
        };
        result.after = after;
        Ok(result)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_rejects_nonfinite_and_overlapping_edges() {
        let make = |l, r| {
            object(vec![(
                "srcRect",
                object(vec![("l", IpcValue::Number(l)), ("r", IpcValue::Number(r))]),
            )])
        };
        assert!(crop(&make(f64::NAN, 0.0)).unwrap_err().contains("finite"));
        assert!(crop(&make(0.5, 0.5)).unwrap_err().contains("visible strip"));
        assert_eq!(crop(&make(0.25, 0.5)).unwrap().unwrap().l.0, 0.25);
    }

    #[test]
    fn geometry_rejects_nonfinite_and_negative_dimensions() {
        let make = |x, cx| {
            object(vec![
                ("op", IpcValue::String("setTransform".into())),
                (
                    "box",
                    object(vec![
                        ("x", IpcValue::Number(x)),
                        ("y", IpcValue::Number(0.0)),
                        ("cx", IpcValue::Number(cx)),
                        ("cy", IpcValue::Number(5.0)),
                    ]),
                ),
            ])
        };
        assert!(
            rect(&make(f64::INFINITY, 1.0), "box")
                .unwrap_err()
                .contains("finite")
        );
        assert!(rect(&make(0.0, -1.0), "box").unwrap_err().contains(">= 0"));
        assert_eq!(rect(&make(-1.5, 2.5), "box").unwrap().x.0, -1.0);
    }

    fn text(s: &str) -> IpcValue {
        IpcValue::String(s.into())
    }
    fn target(id: &str) -> IpcValue {
        object(vec![("slide", IpcValue::Number(0.0)), ("el", text(id))])
    }
    fn fixture() -> (pptx_engine::deck::OpenedPptx, String) {
        use shell_ipc_d1::slides::engine::TxnRequest;
        use std::collections::HashMap;
        let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
        let mut opened = pptx_engine::deck::open_pptx(&bytes).unwrap();
        let add = object(vec![
            ("op", text("addElement")),
            ("target", object(vec![("slide", IpcValue::Number(0.0))])),
            ("kind", text("textbox")),
            (
                "offset",
                object(vec![
                    ("x", IpcValue::Number(0.0)),
                    ("y", IpcValue::Number(0.0)),
                    ("cx", IpcValue::Number(2000000.0)),
                    ("cy", IpcValue::Number(1000000.0)),
                ]),
            ),
            (
                "paragraphs",
                IpcValue::Array(vec![object(vec![(
                    "runs",
                    IpcValue::Array(vec![object(vec![("text", text("Test"))])]),
                )])]),
            ),
        ]);
        let result = super::super::run_txn(
            &mut opened,
            &mut HashMap::new(),
            &TxnRequest::atomic(vec![add]),
        );
        assert!(result.applied, "{:?}", result.failures);
        let IpcValue::String(id) = result.records.unwrap()[0].created.as_ref().unwrap()[0].clone()
        else {
            panic!("expected element id")
        };
        (opened, id)
    }

    #[test]
    fn edits_transform_and_effect_and_records_result() {
        use shell_ipc_d1::slides::engine::TxnRequest;
        use std::collections::HashMap;
        let (mut opened, id) = fixture();
        let bounds = object(vec![
            ("x", IpcValue::Number(20.0)),
            ("y", IpcValue::Number(30.0)),
            ("cx", IpcValue::Number(500000.0)),
            ("cy", IpcValue::Number(600000.0)),
        ]);
        let transform = object(vec![
            ("op", text("setTransform")),
            ("target", target(&id)),
            ("box", bounds.clone()),
            ("rotDeg", IpcValue::Number(10.0)),
        ]);
        let effect = object(vec![
            ("op", text("setEffects")),
            ("target", target(&id)),
            (
                "effects",
                object(vec![(
                    "glow",
                    object(vec![
                        ("color", text("#FF0000")),
                        ("radius", IpcValue::Number(1000.0)),
                    ]),
                )]),
            ),
        ]);
        let result = super::super::run_txn(
            &mut opened,
            &mut HashMap::new(),
            &TxnRequest::atomic(vec![transform, effect]),
        );
        assert!(result.applied, "{:?}", result.failures);
        let records = result.records.unwrap();
        assert_eq!(field(&records[0].after, "x"), Some(&IpcValue::Number(20.0)));
        assert_eq!(
            field(&records[0].after, "rotDeg"),
            Some(&IpcValue::Number(10.0))
        );
        let el = opened.deck.slides[0]
            .elements
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.id() == id)
            .unwrap();
        let transform = pptx_engine::with_element!(el, e => *e.transform).unwrap();
        assert_eq!(transform.offset.x.0, 20.0);
        assert_eq!(transform.rot.0, 600000.0);
        let xml = pptx_engine::with_element!(el, e => e.anchor.as_ref().unwrap().original_xml.to_string());
        assert!(xml.contains("<a:glow"));
    }

    #[test]
    fn missing_element_group_and_bad_color_are_rejected() {
        use shell_ipc_d1::slides::engine::TxnRequest;
        use std::collections::HashMap;
        let (mut opened, id) = fixture();
        let effects_op = object(vec![
            ("op", text("setEffects")),
            ("target", target(&id)),
            (
                "effects",
                object(vec![(
                    "glow",
                    object(vec![
                        ("color", text("#BAD")),
                        ("radius", IpcValue::Number(1.0)),
                    ]),
                )]),
            ),
        ]);
        let missing = object(vec![
            ("op", text("renameElement")),
            ("target", target("no-such-element")),
            ("name", text("x")),
        ]);
        let group = object(vec![
            ("op", text("setShapeGeometry")),
            ("target", target(&id)),
            ("group", text("missing-group")),
            ("prst", text("rect")),
        ]);
        let result = super::super::run_txn(
            &mut opened,
            &mut HashMap::new(),
            &TxnRequest::atomic(vec![effects_op, missing, group]),
        );
        assert!(!result.applied);
        let failures = result.failures.unwrap();
        assert_eq!(failures.len(), 3);
        assert!(failures[0].error.contains("color"));
        assert!(failures[1].error.contains("no element"));
        assert!(failures[2].error.contains("no group"));
    }

    #[test]
    fn group_child_transform_is_applied_and_recorded() {
        use shell_ipc_d1::slides::engine::TxnRequest;
        use std::collections::HashMap;
        let (mut opened, first) = fixture();
        let add = object(vec![
            ("op", text("addElement")),
            ("target", object(vec![("slide", IpcValue::Number(0.0))])),
            ("kind", text("textbox")),
            (
                "offset",
                object(vec![
                    ("x", IpcValue::Number(3000000.0)),
                    ("y", IpcValue::Number(0.0)),
                    ("cx", IpcValue::Number(1000000.0)),
                    ("cy", IpcValue::Number(1000000.0)),
                ]),
            ),
            (
                "paragraphs",
                IpcValue::Array(vec![object(vec![(
                    "runs",
                    IpcValue::Array(vec![object(vec![("text", text("Second"))])]),
                )])]),
            ),
        ]);
        let added = super::super::run_txn(
            &mut opened,
            &mut HashMap::new(),
            &TxnRequest::atomic(vec![add]),
        );
        assert!(added.applied, "{:?}", added.failures);
        let IpcValue::String(second) =
            added.records.unwrap()[0].created.as_ref().unwrap()[0].clone()
        else {
            panic!("expected element id")
        };
        let group = object(vec![
            ("op", text("groupElements")),
            ("target", object(vec![("slide", IpcValue::Number(0.0))])),
            ("els", IpcValue::Array(vec![text(&first), text(&second)])),
        ]);
        let grouped = super::super::run_txn(
            &mut opened,
            &mut HashMap::new(),
            &TxnRequest::atomic(vec![group]),
        );
        assert!(grouped.applied, "{:?}", grouped.failures);
        let IpcValue::String(group_id) =
            grouped.records.unwrap()[0].created.as_ref().unwrap()[0].clone()
        else {
            panic!("expected group id")
        };
        let group = opened.deck.slides[0]
            .elements
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.id() == group_id)
            .unwrap();
        let SlideElement::Group(group) = group else {
            panic!("expected group")
        };
        let child_id = group.children.as_ref().unwrap()[0].id().to_owned();
        let bounds = object(vec![
            ("x", IpcValue::Number(123.0)),
            ("y", IpcValue::Number(456.0)),
            ("cx", IpcValue::Number(10000.0)),
            ("cy", IpcValue::Number(20000.0)),
        ]);
        let op = object(vec![
            ("op", text("setTransform")),
            ("target", target(&child_id)),
            ("group", text(&group_id)),
            ("box", bounds),
            ("rotDeg", IpcValue::Number(5.0)),
        ]);
        let changed = super::super::run_txn(
            &mut opened,
            &mut HashMap::new(),
            &TxnRequest::atomic(vec![op]),
        );
        assert!(changed.applied, "{:?}", changed.failures);
        assert_eq!(
            field(&changed.records.unwrap()[0].after, "x"),
            Some(&IpcValue::Number(123.0))
        );
        let group = opened.deck.slides[0]
            .elements
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.id() == group_id)
            .unwrap();
        let SlideElement::Group(group) = group else {
            panic!("expected group")
        };
        let child = &group.children.as_ref().unwrap()[0];
        let transform = pptx_engine::with_element!(child, e => *e.transform).unwrap();
        assert_eq!(transform.offset.x.0, 123.0);
        assert_eq!(transform.rot.0, 300000.0);
    }
}
