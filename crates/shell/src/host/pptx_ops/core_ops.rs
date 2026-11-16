use jscompat::JsNum;
use pptx_engine::deck::{
    ElementFill, FillOpts, edit_group_child_stroke, set_element_fill, set_element_fill_on_slide,
    stroke_patch_to_model,
};
use pptx_engine::generate::{GradientFillPatch, StrokeGradient, StrokePatch};
use pptx_engine::types::{
    ArrowEnd, ArrowEndSize, Fill, GradientPath, GradientStop, ImageMode, Insets, LineCap,
    LineCompound, LineJoin, SlideElement, Stroke,
};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;

use super::registry::{
    OpContext, SlideTarget, field, op_name, record, resolve_element, resolve_group,
    resolve_group_child_id, resolve_slide, string,
};

fn number(value: Option<&IpcValue>, name: &str, field_name: &str) -> Result<f64, String> {
    match value {
        Some(IpcValue::Number(n)) if n.is_finite() => Ok(*n),
        _ => Err(format!(
            "op \"{name}\": \"{field_name}\" must be a finite number."
        )),
    }
}
fn color(value: Option<&IpcValue>, name: &str, field_name: &str) -> Result<String, String> {
    let value = value.and_then(string).ok_or_else(|| {
        format!(
            "op \"{name}\": \"{field_name}\" must be a hex color \"#RRGGBB\" (or \"#RRGGBBAA\")."
        )
    })?;
    let hex = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "op \"{name}\": \"{field_name}\" must be a hex color \"#RRGGBB\" (or \"#RRGGBBAA\")."
        ));
    }
    Ok(value.to_owned())
}
fn stops(value: Option<&IpcValue>, name: &str) -> Result<Vec<GradientStop>, String> {
    let Some(IpcValue::Array(stops)) = value else {
        return Err(format!(
            "op \"{name}\": a gradient needs \"stops\": at least two {{pos, color}}."
        ));
    };
    if stops.len() < 2 {
        return Err(format!(
            "op \"{name}\": a gradient needs \"stops\": at least two {{pos, color}}."
        ));
    }
    stops
        .iter()
        .map(|stop| {
            let pos = number(field(stop, "pos"), name, "stops[].pos")?;
            if !(0.0..=1.0).contains(&pos) {
                return Err(format!(
                    "op \"{name}\": \"stops[].pos\" must be a fraction 0..1."
                ));
            }
            Ok(GradientStop {
                pos: JsNum(pos),
                color: color(field(stop, "color"), name, "stops[].color")?,
            })
        })
        .collect()
}
fn fill_patch(op: &IpcValue) -> Result<ElementFill, String> {
    match field(op, "fill") {
        Some(value @ IpcValue::String(s)) => { if s != "none" { color(Some(value), "setFill", "fill")?; } Ok(ElementFill::Color(s.clone())) }
        Some(value @ IpcValue::Object(_)) => {
            let angle = match field(value, "angle") { None | Some(IpcValue::Undefined) => None, v => Some(JsNum(number(v, "setFill", "angle")?)) };
            let path = match field(value, "path").and_then(string) { Some("circle") => Some(GradientPath::Circle), Some("rect") => Some(GradientPath::Rect), Some("shape") => Some(GradientPath::Shape), _ => None };
            let radial = match field(value, "radial") { Some(IpcValue::Bool(v)) => Some(*v), _ => None };
            let fill_to = match field(value, "fillTo") {
                None | Some(IpcValue::Undefined) => None,
                Some(v) => Some(Insets { l: JsNum(number(field(v, "l"), "setFill", "fillTo.l")?), t: JsNum(number(field(v, "t"), "setFill", "fillTo.t")?), r: JsNum(number(field(v, "r"), "setFill", "fillTo.r")?), b: JsNum(number(field(v, "b"), "setFill", "fillTo.b")?) }),
            };
            Ok(ElementFill::Gradient(GradientFillPatch { stops: stops(field(value, "stops"), "setFill")?, angle, radial, path, fill_to }))
        }
        _ => Err("op \"setFill\" needs \"fill\": \"none\", a \"#RRGGBB\" color, or a gradient patch object.".into()),
    }
}
fn stroke_patch(op: &IpcValue) -> Result<Option<StrokePatch>, String> {
    let value = field(op, "stroke").ok_or_else(|| "op \"setStroke\" needs \"stroke\": a stroke patch object, or null to remove the outline.".to_owned())?;
    if matches!(value, IpcValue::Null) {
        return Ok(None);
    }
    if !matches!(value, IpcValue::Object(_)) {
        return Err("op \"setStroke\" needs \"stroke\": a stroke patch object, or null to remove the outline.".into());
    }
    let width = number(field(value, "widthEmu"), "setStroke", "stroke.widthEmu")?;
    if width <= 0.0 {
        return Err("op \"setStroke\": \"stroke.widthEmu\" must be > 0 (12700 EMU = 1pt).".into());
    }
    let gradient = match field(value, "gradient") {
        None | Some(IpcValue::Undefined) => None,
        Some(g) => Some(StrokeGradient {
            stops: stops(field(g, "stops"), "setStroke")?,
            angle: JsNum(number(field(g, "angle"), "setStroke", "gradient.angle")?),
        }),
    };
    let option = |name| field(value, name).and_then(string).map(str::to_owned);
    Ok(Some(StrokePatch {
        color: color(field(value, "color"), "setStroke", "stroke.color")?,
        width_emu: JsNum(width),
        dash: option("dash"),
        cap: option("cap"),
        compound: option("compound"),
        join: option("join"),
        gradient,
    }))
}
fn obj(fields: Vec<(&str, IpcValue)>) -> IpcValue {
    IpcValue::Object(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn text(v: &str) -> IpcValue {
    IpcValue::String(v.to_owned())
}
fn insets(v: Insets) -> IpcValue {
    obj(vec![
        ("l", IpcValue::Number(v.l.0)),
        ("t", IpcValue::Number(v.t.0)),
        ("r", IpcValue::Number(v.r.0)),
        ("b", IpcValue::Number(v.b.0)),
    ])
}
fn fill_value(fill: &Fill) -> IpcValue {
    let mut fields = Vec::new();
    match fill {
        Fill::None => fields.push(("type", text("none"))),
        Fill::Solid { color } => {
            fields.push(("type", text("solid")));
            fields.push(("color", text(color)));
        }
        Fill::Gradient(g) => {
            fields.push(("type", text("gradient")));
            fields.push((
                "stops",
                IpcValue::Array(
                    g.stops
                        .iter()
                        .map(|s| {
                            obj(vec![
                                ("pos", IpcValue::Number(s.pos.0)),
                                ("color", text(&s.color)),
                            ])
                        })
                        .collect(),
                ),
            ));
            if let Some(v) = g.angle {
                fields.push(("angle", IpcValue::Number(v.0)));
            }
            if let Some(v) = g.scaled {
                fields.push(("scaled", IpcValue::Bool(v)));
            }
            if let Some(v) = g.path {
                fields.push((
                    "path",
                    text(match v {
                        GradientPath::Circle => "circle",
                        GradientPath::Rect => "rect",
                        GradientPath::Shape => "shape",
                    }),
                ));
            }
            if let Some(v) = g.fill_to {
                fields.push(("fillTo", insets(v)));
            }
            if let Some(v) = g.tile_rect {
                fields.push(("tileRect", insets(v)));
            }
        }
        Fill::Image(i) => {
            fields.push(("type", text("image")));
            fields.push(("mediaRef", text(&i.media_ref)));
            if let Some(v) = i.mode {
                fields.push((
                    "mode",
                    text(match v {
                        ImageMode::Stretch => "stretch",
                        ImageMode::Tile => "tile",
                    }),
                ));
            }
            if let Some(v) = i.alpha {
                fields.push(("alpha", IpcValue::Number(v.0)));
            }
            if let Some(v) = i.fill_rect {
                fields.push(("fillRect", insets(v)));
            }
            if let Some((a, b)) = &i.duotone {
                fields.push(("duotone", IpcValue::Array(vec![text(a), text(b)])));
            }
            if let Some(v) = &i.clr_change {
                fields.push((
                    "clrChange",
                    obj(vec![("from", text(&v.from)), ("to", text(&v.to))]),
                ));
            }
            if let Some(v) = i.lum {
                fields.push((
                    "lum",
                    obj(vec![
                        ("bright", IpcValue::Number(v.bright.0)),
                        ("contrast", IpcValue::Number(v.contrast.0)),
                    ]),
                ));
            }
            if let Some(v) = i.bi_level {
                fields.push(("biLevel", IpcValue::Number(v.0)));
            }
            if let Some(v) = &i.tile {
                fields.push((
                    "tile",
                    obj(vec![
                        ("tx", IpcValue::Number(v.tx.0)),
                        ("ty", IpcValue::Number(v.ty.0)),
                        ("sx", IpcValue::Number(v.sx.0)),
                        ("sy", IpcValue::Number(v.sy.0)),
                        ("algn", text(&v.algn)),
                    ]),
                ));
            }
        }
        Fill::Pattern(p) => {
            fields.push(("type", text("pattern")));
            fields.push(("fg", text(&p.fg)));
            fields.push(("bg", text(&p.bg)));
            fields.push(("preset", text(&p.preset)));
        }
    }
    obj(fields)
}
fn end_value(v: &ArrowEnd) -> IpcValue {
    let mut fields = vec![("type", text(&v.r#type))];
    let size = |v| {
        text(match v {
            ArrowEndSize::Sm => "sm",
            ArrowEndSize::Med => "med",
            ArrowEndSize::Lg => "lg",
        })
    };
    if let Some(v) = v.w {
        fields.push(("w", size(v)));
    }
    if let Some(v) = v.len {
        fields.push(("len", size(v)));
    }
    obj(fields)
}
fn stroke_value(v: &Stroke) -> IpcValue {
    let mut fields = vec![
        ("fill", fill_value(&v.fill)),
        ("width", IpcValue::Number(v.width.0)),
    ];
    if let Some(v) = &v.dash {
        fields.push(("dash", text(v)));
    }
    if let Some(v) = v.cap {
        fields.push((
            "cap",
            text(match v {
                LineCap::Flat => "flat",
                LineCap::Round => "round",
                LineCap::Square => "square",
            }),
        ));
    }
    if let Some(v) = v.join {
        fields.push((
            "join",
            text(match v {
                LineJoin::Round => "round",
                LineJoin::Bevel => "bevel",
                LineJoin::Miter => "miter",
            }),
        ));
    }
    if let Some(v) = v.compound {
        fields.push((
            "compound",
            text(match v {
                LineCompound::Sng => "sng",
                LineCompound::Dbl => "dbl",
                LineCompound::ThickThin => "thickThin",
                LineCompound::ThinThick => "thinThick",
                LineCompound::Tri => "tri",
            }),
        ));
    }
    if let Some(v) = &v.head_end {
        fields.push(("headEnd", end_value(v)));
    }
    if let Some(v) = &v.tail_end {
        fields.push(("tailEnd", end_value(v)));
    }
    obj(fields)
}
fn group_ids(
    op: &IpcValue,
    ctx: &mut OpContext<'_>,
) -> Result<(SlideTarget, String, String), String> {
    let target = resolve_slide(ctx, op, false)?;
    let slide = ctx.slide(&target);
    let group = resolve_group(op, &target, slide)?;
    let reference = field(op, "target")
        .and_then(|v| field(v, "el"))
        .and_then(string)
        .unwrap_or("");
    let child = resolve_group_child_id(slide, &group, reference)?;
    Ok((target, group, child))
}
fn validate_inner(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<(), String> {
    match op_name(op) {
        "deleteElement" => {
            resolve_element(ctx, op, true)?;
        }
        "setFill" | "setStroke" => {
            if op_name(op) == "setFill" {
                fill_patch(op)?;
            } else {
                stroke_patch(op)?;
            }
            if field(op, "group")
                .and_then(string)
                .is_some_and(|s| !s.is_empty())
            {
                group_ids(op, ctx)?;
            } else {
                let (target, index) = resolve_element(ctx, op, true)?;
                let el = &ctx
                    .slide(&target)
                    .elements
                    .as_ref()
                    .expect("resolved element")[index];
                if !(matches!(el, SlideElement::Text(_))
                    || (op_name(op) == "setStroke" && matches!(el, SlideElement::Picture(_))))
                {
                    return Err(format!(
                        "op \"{}\": element does not support this edit.",
                        op_name(op)
                    ));
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    matches!(op_name(op), "deleteElement" | "setFill" | "setStroke")
        .then(|| validate_inner(op, ctx))
}
fn apply_inner(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<TxnRecord, String> {
    let name = op_name(op);
    let mut result = record(op);
    if name == "deleteElement" {
        let (target, index) = resolve_element(ctx, op, true)?;
        let el = &ctx
            .slide(&target)
            .elements
            .as_ref()
            .expect("resolved element")[index];
        let id = el.id().to_owned();
        let ty = el
            .element_type()
            .map(|ty| {
                text(match ty {
                    pptx_engine::types::ElementType::Text => "text",
                    pptx_engine::types::ElementType::Shape => "shape",
                    pptx_engine::types::ElementType::Picture => "picture",
                    pptx_engine::types::ElementType::Group => "group",
                    pptx_engine::types::ElementType::Table => "table",
                    pptx_engine::types::ElementType::Chart => "chart",
                    pptx_engine::types::ElementType::Passthrough => "passthrough",
                })
            })
            .unwrap_or(IpcValue::Undefined);
        result.before = obj(vec![("type", ty)]);
        match target {
            SlideTarget::Slide(index) => {
                if !pptx_engine::insert::delete_element(ctx.opened, index, &id)? {
                    return Err(format!(
                        "op \"deleteElement\": element \"{id}\" could not be removed."
                    ));
                }
            }
            SlideTarget::Part(_) => {
                let slide = ctx.slide_mut(&target);
                let removed = slide
                    .elements
                    .as_mut()
                    .expect("resolved element")
                    .remove(index);
                let xml = pptx_engine::with_element!(&removed, e => e.anchor.as_ref().map(|a| a.original_xml.clone())).unwrap_or_default();
                slide.structure_dirty.set(true);
                let ids = pptx_engine::animation::c_nv_pr_ids_in_xml(&xml)
                    .into_iter()
                    .map(|n| n.0)
                    .collect::<Vec<_>>();
                pptx_engine::animation::prune_timing_for_spids(slide, &ids)?;
                let path = slide.path.as_deref().unwrap_or_default().to_owned();
                let current = pptx_engine::deck::try_patch_slide_xml(slide)?;
                pptx_engine::resource_cleanup::cleanup_superseded_part_resources(
                    ctx.opened, &path, &xml, &current,
                )?;
            }
        }
        return Ok(result);
    }
    let grouped = field(op, "group")
        .and_then(string)
        .is_some_and(|s| !s.is_empty());
    if grouped {
        let (target, group, child) = group_ids(op, ctx)?;
        if name == "setStroke" {
            let patch = stroke_patch(op)?;
            if !edit_group_child_stroke(ctx.slide_mut(&target), &group, &child, patch.as_ref())? {
                return Err(format!(
                    "op \"setStroke\": no child \"{child}\" in group \"{group}\", or it does not support strokes."
                ));
            }
            result.after = field(op, "stroke").cloned().unwrap_or(IpcValue::Undefined);
        } else {
            let patch = fill_patch(op)?;
            let SlideTarget::Slide(index) = target else {
                unreachable!()
            };
            if !set_element_fill(
                ctx.opened,
                index,
                &child,
                &patch,
                &FillOpts {
                    group_id: Some(group),
                },
            )? {
                return Err(format!("op \"setFill\": no supported child \"{child}\"."));
            }
            result.after = field(op, "fill").cloned().unwrap_or(IpcValue::Undefined);
        }
        return Ok(result);
    }
    let (target, index) = resolve_element(ctx, op, true)?;
    let element = &ctx
        .slide(&target)
        .elements
        .as_ref()
        .expect("resolved element")[index];
    let id = element.id().to_owned();
    if name == "setFill" {
        if let SlideElement::Text(el) = element {
            result.before = el
                .fill
                .as_ref()
                .map(fill_value)
                .unwrap_or(IpcValue::Undefined);
        }
        let patch = fill_patch(op)?;
        match target {
            SlideTarget::Slide(i) => {
                if !set_element_fill(ctx.opened, i, &id, &patch, &FillOpts::default())? {
                    return Err(format!(
                        "op \"setFill\": element \"{id}\" does not support fill."
                    ));
                }
            }
            SlideTarget::Part(_) => {
                let slide = ctx.slide_mut(&target);
                let path = slide.path.as_deref().unwrap_or_default().to_owned();
                let Some((before, after)) =
                    set_element_fill_on_slide(slide, &id, &patch, &FillOpts::default())?
                else {
                    return Err(format!(
                        "op \"setFill\": element \"{id}\" does not support fill."
                    ));
                };
                pptx_engine::resource_cleanup::cleanup_superseded_part_resources(
                    ctx.opened, &path, &before, &after,
                )?;
            }
        }
        result.after = field(op, "fill").cloned().unwrap_or(IpcValue::Undefined);
    } else {
        let patch = stroke_patch(op)?;
        let element = &mut ctx
            .slide_mut(&target)
            .elements
            .as_mut()
            .expect("resolved element")[index];
        match element {
            SlideElement::Text(el) => {
                result.before = el
                    .stroke
                    .as_ref()
                    .map(stroke_value)
                    .unwrap_or(IpcValue::Undefined);
                el.stroke.assign(patch.as_ref().map(stroke_patch_to_model));
                el.dirty_stroke.set(true);
            }
            SlideElement::Picture(el) => {
                result.before = el
                    .stroke
                    .as_ref()
                    .map(stroke_value)
                    .unwrap_or(IpcValue::Undefined);
                el.stroke.assign(patch.as_ref().map(stroke_patch_to_model));
                el.dirty_stroke.set(true);
            }
            _ => {
                return Err(format!(
                    "op \"setStroke\": element \"{id}\" does not support strokes."
                ));
            }
        }
        result.after = field(op, "stroke").cloned().unwrap_or(IpcValue::Undefined);
    }
    Ok(result)
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    matches!(op_name(op), "deleteElement" | "setFill" | "setStroke").then(|| apply_inner(op, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pptx_engine::types::GradientFill;

    #[test]
    fn before_fill_retains_nonfinite_numbers() {
        let fill = Fill::Gradient(GradientFill {
            stops: vec![GradientStop {
                pos: JsNum(f64::NAN),
                color: "#000000".into(),
            }],
            angle: Some(JsNum(f64::INFINITY)),
            scaled: None,
            path: None,
            fill_to: None,
            tile_rect: None,
        });
        let value = fill_value(&fill);
        assert!(matches!(field(&value, "angle"), Some(IpcValue::Number(n)) if n.is_infinite()));
        let Some(IpcValue::Array(stops)) = field(&value, "stops") else {
            panic!("gradient stops");
        };
        assert!(matches!(field(&stops[0], "pos"), Some(IpcValue::Number(n)) if n.is_nan()));
    }
}
