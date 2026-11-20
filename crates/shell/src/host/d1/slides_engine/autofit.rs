use pptx_engine::deck::OpenedPptx;
use pptx_engine::types::{Autofit, SlideElement, TextElement};
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::EngineError;
use shell_ipc_d1::value::{as_num, as_str, field, truthy};

fn find(opened: &OpenedPptx, slide: usize, id: &str) -> Result<Option<usize>, EngineError> {
    let Some(elements) = opened
        .deck
        .slides
        .get(slide)
        .and_then(|slide| slide.elements.as_ref())
    else {
        return Ok(None);
    };
    for (index, element) in elements.iter().enumerate() {
        if pptx_engine::identity::matches_element_ref(element, id).map_err(EngineError)? {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

fn text_element(opened: &OpenedPptx, slide: usize, index: usize) -> Option<&TextElement> {
    match opened
        .deck
        .slides
        .get(slide)?
        .elements
        .as_ref()?
        .get(index)?
    {
        SlideElement::Text(element) => Some(element),
        _ => None,
    }
}

fn node<'a>(rendered: &'a IpcValue, id: &str) -> Option<&'a IpcValue> {
    let IpcValue::Array(nodes) = field(rendered, "nodes") else {
        return None;
    };
    nodes.iter().find(|node| {
        as_str(field(node, "sourceId")) == Some(id)
            && matches!(as_str(field(node, "type")), Some("shape" | "text"))
            && !matches!(field(node, "text"), IpcValue::Null | IpcValue::Undefined)
    })
}

fn num(value: &IpcValue, key: &str) -> Result<f64, EngineError> {
    as_num(field(value, key)).ok_or_else(|| EngineError(format!("invalid rendered text {key}")))
}

pub(super) fn resize(
    opened: &mut OpenedPptx,
    slide: usize,
    id: &str,
    rendered: &IpcValue,
    fit: f64,
) -> Result<bool, EngineError> {
    let Some(index) = find(opened, slide, id)? else {
        return Ok(false);
    };
    let Some(element) = text_element(opened, slide, index) else {
        return Ok(false);
    };
    if element.text.as_ref().and_then(|body| *body.autofit) != Some(Autofit::Resize) {
        return Ok(false);
    }
    let Some(node) = node(rendered, element.id.as_deref().unwrap_or("")) else {
        return Ok(false);
    };
    let text = field(node, "text");
    let bbox = field(node, "box");
    let insets = field(text, "insets");
    let need_h = num(text, "contentHeight")? + num(insets, "t")? + num(insets, "b")?;
    let box_w = num(bbox, "w")?;
    let box_h = num(bbox, "h")?;
    let lines = match field(text, "lines") {
        IpcValue::Array(lines) => lines.as_slice(),
        _ => &[],
    };
    let need_w = if !truthy(field(text, "wrap")) && !truthy(field(text, "vert")) {
        let mut left = f64::INFINITY;
        let mut right = f64::NEG_INFINITY;
        for line in lines {
            if let IpcValue::Array(runs) = field(line, "runs") {
                for run in runs {
                    let x = num(run, "x")?;
                    left = left.min(x);
                    right = right.max(x + num(run, "widthPx")?);
                }
            }
        }
        (if right > left { right - left } else { 0.0 }) + num(insets, "l")? + num(insets, "r")?
    } else {
        box_w
    };
    if (need_h - box_h).abs() < 1.0 && (need_w - box_w).abs() < 1.0 {
        return Ok(false);
    }
    let scale = fit / (opened.deck.size.cx.0 / 9525.0);
    let shift = match lines.first().and_then(|line| as_str(field(line, "align"))) {
        Some("right") => box_w - need_w,
        Some("center") => (box_w - need_w) / 2.0,
        _ => 0.0,
    };
    let SlideElement::Text(element) = &mut opened.deck.slides[slide]
        .elements
        .as_mut()
        .ok_or_else(|| EngineError("slide has no elements".into()))?[index]
    else {
        return Ok(false);
    };
    let transform = element
        .transform
        .as_mut()
        .ok_or_else(|| EngineError("text element has no transform".into()))?;
    transform.offset.x.0 += jscompat::math_round(shift / scale * 9525.0);
    transform.offset.cx.0 = jscompat::math_max(jscompat::math_round(need_w / scale * 9525.0), 1.0);
    transform.offset.cy.0 = jscompat::math_max(jscompat::math_round(need_h / scale * 9525.0), 1.0);
    element.dirty_transform.set(true);
    Ok(true)
}

pub(super) fn refit_input(
    opened: &OpenedPptx,
    slide: usize,
    id: &str,
    rendered: &IpcValue,
    fit: f64,
) -> Result<Option<(String, serde_json::Value)>, EngineError> {
    let Some(index) = find(opened, slide, id)? else {
        return Ok(None);
    };
    let Some(element) = text_element(opened, slide, index) else {
        return Ok(None);
    };
    let Some(body) = element.text.as_ref() else {
        return Ok(None);
    };
    if *body.autofit != Some(Autofit::Shrink) {
        return Ok(None);
    }
    let id = element.id.as_deref().unwrap_or("");
    let Some(node) = node(rendered, id) else {
        return Ok(None);
    };
    let bbox = field(node, "box");
    Ok(Some((
        id.to_owned(),
        serde_json::json!({
            "body": jscompat::writing_for_host(|| serde_json::to_value(body)).map_err(|error| EngineError(error.to_string()))?,
            "size": jscompat::writing_for_host(|| serde_json::to_value(opened.deck.size)).map_err(|error| EngineError(error.to_string()))?,
            "fitWidthPx": super::host_number(fit),
            "boxWidthPx": super::host_number(num(bbox, "w")?), "boxHeightPx": super::host_number(num(bbox, "h")?),
        }),
    )))
}

pub(super) fn apply_refit(
    opened: &mut OpenedPptx,
    slide: usize,
    id: &str,
    scale: f64,
    reduction: f64,
) -> Result<bool, EngineError> {
    let Some(index) = find(opened, slide, id)? else {
        return Ok(false);
    };
    let Some(page) = opened.deck.slides.get_mut(slide) else {
        return Ok(false);
    };
    let Some(SlideElement::Text(element)) = page
        .elements
        .as_mut()
        .and_then(|elements| elements.get_mut(index))
    else {
        return Ok(false);
    };
    let Some(body) = element.text.as_mut() else {
        return Ok(false);
    };
    if (scale - body.font_scale.map_or(1.0, |value| value.0)).abs() < 0.005
        && (reduction - body.ln_spc_reduction.map_or(0.0, |value| value.0)).abs() < 0.005
    {
        return Ok(false);
    }
    if element.anchor.as_ref().is_none() {
        return Err(EngineError("text element has no anchor".into()));
    }
    body.font_scale.set(jscompat::JsNum(scale));
    if reduction != 0.0 {
        body.ln_spc_reduction.set(jscompat::JsNum(reduction));
    } else {
        body.ln_spc_reduction.delete();
    }
    let anchor = element
        .anchor
        .as_mut()
        .ok_or_else(|| EngineError("text element has no anchor".into()))?;
    anchor.original_xml = std::sync::Arc::from(pptx_engine::generate::patch_body_pr_autofit(
        &anchor.original_xml,
        scale,
        Some(reduction),
    ));
    page.structure_dirty.set(true);
    Ok(true)
}
