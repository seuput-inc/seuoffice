mod format;
#[cfg(test)]
mod tests;

use super::edit_text::{
    apply_edit_paragraphs, collect_paragraph_format_patches, font_size_drift, levels_changed,
    text_moved,
};
use super::registry::{
    OpContext, SlideTarget, field, op_name, record, resolve_element, resolve_group,
    resolve_group_child_id, resolve_slide, string,
};
use pptx_engine::deck::{self, BulletBlip, ParagraphFormatPatch};
use pptx_engine::generate::is_connector_xml;
use pptx_engine::hyperlink::ensure_run_link_rels;
use pptx_engine::insert::add_image_media_and_rel_for_part;
use pptx_engine::resource_cleanup::cleanup_superseded_part_resources;
use pptx_engine::types::{
    ElementType, PPrDirty, SlideElement, TextAnchor, TextBody, TextElement, TextInsets,
};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;
use std::collections::HashMap;

fn target_id(op: &IpcValue) -> &str {
    field(op, "target")
        .and_then(|t| field(t, "el"))
        .and_then(string)
        .unwrap_or("")
}
fn is_group(op: &IpcValue) -> bool {
    match field(op, "group") {
        None | Some(IpcValue::Undefined | IpcValue::Null | IpcValue::Bool(false)) => false,
        Some(IpcValue::String(s)) => !s.is_empty(),
        Some(IpcValue::Number(n)) => *n != 0.0 && !n.is_nan(),
        _ => true,
    }
}
fn edit_paragraphs(op: &IpcValue) -> &[IpcValue] {
    match field(op, "paragraphs") {
        Some(IpcValue::Array(p)) => p,
        _ => &[],
    }
}
fn validate_links(op: &IpcValue) -> Result<(), String> {
    const ACTIONS: &[&str] = &[
        "nextslide",
        "previousslide",
        "firstslide",
        "lastslide",
        "lastslideviewed",
        "endshow",
    ];
    for p in edit_paragraphs(op) {
        if let Some(IpcValue::Array(runs)) = field(p, "runs") {
            for r in runs {
                let Some(link) = field(r, "link") else {
                    continue;
                };
                if matches!(link, IpcValue::Undefined | IpcValue::Null) {
                    continue;
                }
                let usage = "op \"setText\" \"runs[].link\" must be {kind:\"url\",url}, {kind:\"slide\",slideIndex}, {kind:\"action\",action} or null.";
                match field(link, "kind").and_then(string) {
                    Some("url")
                        if field(link, "url")
                            .and_then(string)
                            .is_some_and(|s| !s.is_empty()) =>
                    {}
                    Some("slide") if matches!(field(link,"slideIndex"),Some(IpcValue::Number(n)) if n.is_finite() && *n >= 0.0 && n.fract() == 0.0) => {}
                    Some("action") => {
                        let action = field(link, "action").and_then(string);
                        if !action.is_some_and(|s| ACTIONS.contains(&s)) {
                            let got = action
                                .map(|s| serde_json::to_string(s).unwrap_or_default())
                                .unwrap_or_else(|| "undefined".into());
                            return Err(format!(
                                "op \"setText\" \"runs[].link.action\" must be one of {} (got {got}).",
                                ACTIONS.join(", ")
                            ));
                        }
                    }
                    _ => return Err(usage.into()),
                }
            }
        }
    }
    Ok(())
}
fn resolve_target(
    ctx: &mut OpContext<'_>,
    op: &IpcValue,
    allow_part: bool,
    only_text: bool,
) -> Result<(), String> {
    if is_group(op) {
        let target = resolve_slide(ctx, op, false)?;
        resolve_group(op, &target, ctx.slide(&target))?;
    } else {
        let (target, index) = resolve_element(ctx, op, allow_part)?;
        if only_text {
            let element = &ctx
                .slide(&target)
                .elements
                .as_ref()
                .ok_or("missing elements")?[index];
            if !matches!(element, SlideElement::Text(_)) {
                return Err(format!(
                    "op \"{}\" targets a {} element — it only works on text/shape elements.",
                    op_name(op),
                    element_type(element)
                ));
            }
        }
    }
    Ok(())
}
fn element_type(el: &SlideElement) -> &'static str {
    match el {
        SlideElement::Text(t) => {
            if t.r#type.as_ref() == Some(&ElementType::Shape) {
                "shape"
            } else {
                "text"
            }
        }
        SlideElement::Picture(_) => "picture",
        SlideElement::Group(_) => "group",
        SlideElement::Table(_) => "table",
        SlideElement::Chart(_) => "chart",
        SlideElement::Passthrough(_) => "passthrough",
    }
}
pub(super) fn validate(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<(), String>> {
    Some(match op_name(op) {
        "setText" => (|| {
            if !matches!(field(op, "paragraphs"), Some(IpcValue::Array(_))) {
                return Err("op \"setText\" needs \"paragraphs\": an EditParagraph array.".into());
            }
            validate_links(op)?;
            resolve_target(ctx, op, true, true)
        })(),
        "setFont" => (|| {
            format::validate_font(op)?;
            resolve_target(ctx, op, false, false)
        })(),
        "setParagraphFormat" => (|| {
            format::validate_format(op)?;
            resolve_target(ctx, op, false, false)
        })(),
        _ => return None,
    })
}
fn locate(
    ctx: &mut OpContext<'_>,
    op: &IpcValue,
    allow_part: bool,
) -> Result<(SlideTarget, Option<String>, String), String> {
    if is_group(op) {
        let target = resolve_slide(ctx, op, false)?;
        let slide = ctx.slide(&target);
        let group = resolve_group(op, &target, slide)?;
        let id = resolve_group_child_id(slide, &group, target_id(op))?;
        Ok((target, Some(group), id))
    } else {
        let (target, index) = resolve_element(ctx, op, allow_part)?;
        let id = ctx
            .slide(&target)
            .elements
            .as_ref()
            .ok_or("missing elements")?[index]
            .id()
            .to_owned();
        Ok((target, None, id))
    }
}
fn land(
    ctx: &mut OpContext<'_>,
    target: &SlideTarget,
    op: &str,
    patch: &mut ParagraphFormatPatch,
    image: Option<(String, String)>,
    landed: &mut HashMap<(String, String), (String, String)>,
) -> Result<Option<IpcValue>, String> {
    let Some((base64, ext)) = image.filter(|_| patch.bullet.as_deref() == Some("blip")) else {
        return Ok(None);
    };
    let key = (ext.clone(), base64.clone());
    let (rid, path) = if let Some(v) = landed.get(&key) {
        v.clone()
    } else {
        let part = ctx.slide(target).path.as_deref().unwrap_or("").to_owned();
        let added = add_image_media_and_rel_for_part(
            ctx.opened,
            &part,
            &jscompat::buffer_from_base64(&base64),
            &ext,
        )?
        .ok_or_else(|| format!("op \"{op}\": unsupported picture bullet format \".{ext}\"."))?;
        let value = (added.rid, added.media_path);
        landed.insert(key, value.clone());
        value
    };
    let mut blip = BulletBlip::default();
    blip.media_ref.set(path.clone());
    blip.blip_embed_id.set(rid.clone());
    patch.bullet_blip = Some(blip);
    Ok(Some(IpcValue::Object(vec![
        ("mediaRef".into(), IpcValue::String(path)),
        ("blipEmbedId".into(), IpcValue::String(rid)),
    ])))
}
fn apply_font(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<TxnRecord, String> {
    let (target, group, id) = locate(ctx, op, false)?;
    let font = field(op, "font").ok_or("missing font")?;
    let patch = format::font_patch(font);
    let step = field(font, "fontSizeStep").filter(|v| !matches!(v, IpcValue::Undefined));
    let mapper = |size| format::font_step(step.expect("validated step"), size);
    let size_map = step.map(|_| &mapper as &dyn Fn(Option<f64>) -> f64);
    let ok = if let Some(group) = group {
        deck::set_group_child_font(ctx.slide_mut(&target), &group, &id, &patch, size_map)?
    } else {
        deck::set_element_font(ctx.slide_mut(&target), &id, &patch, size_map)?
    };
    if !ok {
        return Err(format!(
            "op \"setFont\": element \"{id}\" has no editable text to format."
        ));
    }
    let mut result = record(op);
    result.after = font.clone();
    Ok(result)
}
fn apply_format(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<TxnRecord, String> {
    let (target, group, id) = locate(ctx, op, false)?;
    let fmt = field(op, "format").ok_or("missing format")?;
    let mut patch = format::paragraph_patch(fmt);
    let image = field(fmt, "bulletImage").and_then(|i| {
        Some((
            field(i, "base64").and_then(string)?.to_owned(),
            field(i, "ext").and_then(string)?.to_owned(),
        ))
    });
    let landed = land(
        ctx,
        &target,
        "setParagraphFormat",
        &mut patch,
        image,
        &mut HashMap::new(),
    )?;
    let ok = if let Some(group) = group {
        deck::set_group_child_paragraph_format(ctx.slide_mut(&target), &group, &id, &patch, None)?
    } else {
        deck::set_element_paragraph_format(ctx.slide_mut(&target), &id, &patch, None)?
    };
    if !ok {
        return Err(format!(
            "op \"setParagraphFormat\": element \"{id}\" has no paragraphs to format."
        ));
    }
    let mut after = match fmt {
        IpcValue::Object(fields) => fields
            .iter()
            .filter(|(k, _)| k != "bulletImage")
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    if let Some(blip) = landed {
        after.push(("bulletBlip".into(), blip));
    }
    let mut result = record(op);
    result.after = IpcValue::Object(after);
    Ok(result)
}
fn fresh_body(auto: bool) -> TextBody {
    let mut body = TextBody::default();
    body.paragraphs.set(Vec::new());
    let mut insets = TextInsets::default();
    insets.l.set(jscompat::JsNum(91440.));
    insets.r.set(jscompat::JsNum(91440.));
    insets.t.set(jscompat::JsNum(45720.));
    insets.b.set(jscompat::JsNum(45720.));
    body.insets.set(insets);
    if auto {
        body.anchor.set(TextAnchor::Middle);
    }
    body
}
fn body_edit(
    element: &mut TextElement,
    edit: &[IpcValue],
    xml: &str,
    what: &str,
) -> Result<(String, bool, Option<String>), String> {
    let fresh = element.text.is_none();
    let auto = fresh
        && element.r#type.as_ref() == Some(&ElementType::Shape)
        && element.placeholder.is_none()
        && element.tx_box.as_ref() != Some(&true);
    if fresh {
        if is_connector_xml(xml) {
            return Err(format!(
                "op \"setText\": {what} is a connector, which cannot hold text."
            ));
        }
        element.text.set(fresh_body(auto));
    }
    let body = element.text.as_mut().ok_or("missing text body")?;
    let old = body.paragraphs.as_ref().cloned().unwrap_or_default();
    let before = old
        .iter()
        .map(|p| {
            p.runs
                .as_ref()
                .into_iter()
                .flatten()
                .map(|r| r.text.as_deref().unwrap_or(""))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let dirty = levels_changed(&old, edit);
    let mut paragraphs = apply_edit_paragraphs(&old, edit);
    let note = if fresh {
        None
    } else {
        font_size_drift(&old, edit, &paragraphs)
    };
    if auto {
        for p in &mut paragraphs {
            if p.align.is_none() {
                p.align.set(pptx_engine::types::TextAlign::Center);
            }
        }
    }
    body.paragraphs.set(paragraphs);
    if text_moved(edit) {
        element.text_moved.set(true);
    }
    Ok((before, dirty, note))
}
fn apply_text(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<TxnRecord, String> {
    let (target, group, id) = locate(ctx, op, true)?;
    let previous = deck::patch_slide_xml(ctx.slide_mut(&target));
    let edit = edit_paragraphs(op);
    let (before, level_dirty, note) = if let Some(group_id) = &group {
        let (gi, ci) =
            deck::find_group_child(ctx.slide(&target), group_id, &id).ok_or_else(|| {
                format!("op \"setText\": no text child \"{id}\" in group \"{group_id}\".")
            })?;
        let slide = ctx.slide_mut(&target);
        let elements = slide.elements.as_mut().ok_or("missing elements")?;
        let SlideElement::Group(grp) = &mut elements[gi] else {
            return Err("missing group".into());
        };
        let child_xml = grp
            .anchor
            .as_ref()
            .map(|a| a.original_xml.as_ref())
            .unwrap_or("");
        let slices = deck::group_child_slices(child_xml)?;
        let child = grp
            .children
            .as_mut()
            .and_then(|c| c.get_mut(ci))
            .ok_or("missing group child")?;
        let SlideElement::Text(child) = child else {
            return Err(format!(
                "op \"setText\": no text child \"{id}\" in group \"{group_id}\"."
            ));
        };
        let xml = slices
            .iter()
            .find(|s| s.nv_id.as_deref() == child.nv_id.as_deref())
            .map(|s| s.xml.as_str())
            .unwrap_or("");
        let (before, _, note) = body_edit(child, edit, xml, &format!("group child \"{id}\""))?;
        (before, false, note)
    } else {
        let slide = ctx.slide_mut(&target);
        let el = slide
            .elements
            .as_mut()
            .and_then(|e| e.iter_mut().find(|e| e.id() == id))
            .ok_or("missing element")?;
        let SlideElement::Text(el) = el else {
            return Err("not a text element".into());
        };
        let xml = el
            .anchor
            .as_ref()
            .map(|a| a.original_xml.to_string())
            .unwrap_or_default();
        body_edit(el, edit, &xml, &format!("element \"{id}\""))?
    };
    let mut paras = {
        let slide = ctx.slide_mut(&target);
        let el = if let Some(group_id) = &group {
            let (gi, ci) =
                deck::find_group_child(slide, group_id, &id).ok_or("missing group child")?;
            let SlideElement::Group(g) =
                &mut slide.elements.as_mut().ok_or("missing elements")?[gi]
            else {
                return Err("missing group".into());
            };
            g.children
                .as_mut()
                .and_then(|c| c.get_mut(ci))
                .ok_or("missing group child")?
        } else {
            slide
                .elements
                .as_mut()
                .and_then(|e| e.iter_mut().find(|e| e.id() == id))
                .ok_or("missing element")?
        };
        let SlideElement::Text(el) = el else {
            return Err("not a text element".into());
        };
        el.text
            .as_mut()
            .and_then(|b| b.paragraphs.as_mut())
            .map(std::mem::take)
            .ok_or("missing paragraphs")?
    };
    if let SlideTarget::Slide(index) = target {
        ensure_run_link_rels(ctx.opened, index, &mut paras);
    }
    {
        let slide = ctx.slide_mut(&target);
        let el = if let Some(group_id) = &group {
            let (gi, ci) =
                deck::find_group_child(slide, group_id, &id).ok_or("missing group child")?;
            let SlideElement::Group(g) =
                &mut slide.elements.as_mut().ok_or("missing elements")?[gi]
            else {
                return Err("missing group".into());
            };
            g.children
                .as_mut()
                .and_then(|c| c.get_mut(ci))
                .ok_or("missing group child")?
        } else {
            slide
                .elements
                .as_mut()
                .and_then(|e| e.iter_mut().find(|e| e.id() == id))
                .ok_or("missing element")?
        };
        let SlideElement::Text(el) = el else {
            return Err("not a text element".into());
        };
        el.text
            .as_mut()
            .ok_or("missing text body")?
            .paragraphs
            .set(paras);
        if group.is_none() {
            el.dirty.set(true);
        }
        let child = (**el).clone();
        if let Some(group_id) = &group
            && !deck::patch_group_child_text(slide, group_id, &child)?
        {
            return Err(format!(
                "op \"setText\": the child slice for \"{id}\" could not be located inside group \"{group_id}\"."
            ));
        }
    }
    let mut landed = HashMap::new();
    for (index, mut fmt) in collect_paragraph_format_patches(edit) {
        land(
            ctx,
            &target,
            "setText",
            &mut fmt.patch,
            fmt.bullet_image,
            &mut landed,
        )?;
        let indices = [index as f64];
        if let Some(group_id) = &group {
            deck::set_group_child_paragraph_format(
                ctx.slide_mut(&target),
                group_id,
                &id,
                &fmt.patch,
                Some(&indices),
            )?;
        } else {
            deck::set_element_paragraph_format(
                ctx.slide_mut(&target),
                &id,
                &fmt.patch,
                Some(&indices),
            )?;
        }
    }
    let current = deck::patch_slide_xml(ctx.slide_mut(&target));
    let path = ctx.slide(&target).path.as_deref().unwrap_or("").to_owned();
    cleanup_superseded_part_resources(ctx.opened, &path, &previous, &current)?;
    if level_dirty && let SlideTarget::Slide(index) = target {
        let slide = ctx.slide_mut(&SlideTarget::Slide(index));
        if let Some(SlideElement::Text(el)) = slide
            .elements
            .as_mut()
            .and_then(|e| e.iter_mut().find(|e| e.id() == id))
        {
            let mut dirty = el
                .dirty_p_pr
                .as_ref()
                .cloned()
                .unwrap_or_else(PPrDirty::default);
            dirty.level.set(true);
            dirty.indents.set(true);
            el.dirty_p_pr.set(dirty);
        }
        deck::materialize_slide(ctx.opened, index)?;
    }
    let mut after = vec![("levelDirty".into(), IpcValue::Bool(level_dirty))];
    after.push((
        "note".into(),
        note.map(IpcValue::String).unwrap_or(IpcValue::Undefined),
    ));
    let mut result = record(op);
    result.before = IpcValue::String(before);
    result.after = IpcValue::Object(after);
    Ok(result)
}
pub(super) fn apply(op: &IpcValue, ctx: &mut OpContext<'_>) -> Option<Result<TxnRecord, String>> {
    Some(match op_name(op) {
        "setText" => apply_text(op, ctx),
        "setFont" => apply_font(op, ctx),
        "setParagraphFormat" => apply_format(op, ctx),
        _ => return None,
    })
}
