use std::collections::HashMap;

use pptx_engine::deck::OpenedPptx;
use pptx_engine::identity::{element_durable_id, matches_element_ref, slide_durable_id};
use pptx_engine::master_edit::{list_master_parts, parse_master_part};
use pptx_engine::types::Slide;
use shell_contract::IpcValue;
use shell_ipc_d1::slides::engine::TxnRecord;

pub(super) struct OpContext<'a> {
    pub opened: &'a mut OpenedPptx,
    pub parts: HashMap<String, Slide>,
    pub touched_parts: Vec<String>,
    pub withhold: bool,
}

#[derive(Clone, Debug)]
pub(super) enum SlideTarget {
    Slide(usize),
    Part(String),
}

impl OpContext<'_> {
    pub fn slide(&self, target: &SlideTarget) -> &Slide {
        match target {
            SlideTarget::Slide(index) => &self.opened.deck.slides[*index],
            SlideTarget::Part(path) => &self.parts[path],
        }
    }

    pub fn slide_mut(&mut self, target: &SlideTarget) -> &mut Slide {
        match target {
            SlideTarget::Slide(index) => &mut self.opened.deck.slides[*index],
            SlideTarget::Part(path) => {
                if !self.touched_parts.contains(path) {
                    self.touched_parts.push(path.clone());
                }
                self.parts.get_mut(path).expect("resolved master part")
            }
        }
    }
}

pub(super) fn field<'a>(value: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(fields) => fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

pub(super) fn string(value: &IpcValue) -> Option<&str> {
    match value {
        IpcValue::String(value) => Some(value),
        _ => None,
    }
}

pub(super) fn op_name(op: &IpcValue) -> &str {
    field(op, "op").and_then(string).unwrap_or("(none)")
}

pub(super) fn record(op: &IpcValue) -> TxnRecord {
    TxnRecord {
        op: op.clone(),
        slide_id: None,
        created: None,
        before: IpcValue::Undefined,
        after: IpcValue::Undefined,
    }
}

pub(super) fn resolve_slide(
    ctx: &mut OpContext<'_>,
    op: &IpcValue,
    allow_part: bool,
) -> Result<SlideTarget, String> {
    let name = op_name(op);
    let target = field(op, "target");
    if let Some(part) = target
        .and_then(|t| field(t, "part"))
        .and_then(string)
        .filter(|s| !s.is_empty())
    {
        if !allow_part {
            return Err(format!(
                "op \"{name}\" cannot target a master/layout part — address a slide via target.slide."
            ));
        }
        if !ctx.parts.contains_key(part) {
            let known = list_master_parts(&ctx.opened.archive)?;
            if !known.iter().any(|p| p.part_path == part) {
                let available = known
                    .iter()
                    .map(|p| p.part_path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "op \"{name}\": no master/layout part \"{part}\". Available: [{available}]."
                ));
            }
            let slide = parse_master_part(&ctx.opened.archive, part, &ctx.opened.uids)?
                .ok_or_else(|| format!("op \"{name}\": part \"{part}\" could not be parsed."))?;
            ctx.parts.insert(part.to_owned(), slide);
        }
        return Ok(SlideTarget::Part(part.to_owned()));
    }
    let slides = &ctx.opened.deck.slides;
    match target.and_then(|t| field(t, "slide")) {
        Some(IpcValue::String(id)) => slides
            .iter()
            .position(|s| slide_durable_id(s) == *id)
            .map(SlideTarget::Slide)
            .ok_or_else(|| {
                let available = slides
                    .iter()
                    .map(slide_durable_id)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("op \"{name}\": no slide \"{id}\". Available: [{available}].")
            }),
        Some(IpcValue::Number(n))
            if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n < slides.len() as f64 =>
        {
            Ok(SlideTarget::Slide(*n as usize))
        }
        Some(IpcValue::Number(n)) => Err(format!(
            "op \"{name}\": slide index {n} is out of range (0-{}).",
            slides.len() as isize - 1
        )),
        _ => Err(format!(
            "op \"{name}\" needs target.slide (a 0-based index or a durable \"s_<n>\" id; this deck has {} slide(s)).",
            slides.len()
        )),
    }
}

pub(super) fn resolve_element(
    ctx: &mut OpContext<'_>,
    op: &IpcValue,
    allow_part: bool,
) -> Result<(SlideTarget, usize), String> {
    let target = resolve_slide(ctx, op, allow_part)?;
    let name = op_name(op);
    let id = field(op, "target")
        .and_then(|t| field(t, "el"))
        .and_then(string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("op \"{name}\" needs target.el (an element id)."))?;
    let slide = ctx.slide(&target);
    for (index, el) in slide.elements.as_ref().into_iter().flatten().enumerate() {
        if matches_element_ref(el, id)? {
            return Ok((target, index));
        }
    }
    let available = slide
        .elements
        .as_ref()
        .into_iter()
        .flatten()
        .map(|el| {
            let id = el.id();
            match element_durable_id(el) {
                Ok(Some(durable)) => format!("{id} ({durable})"),
                _ => id.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "op \"{name}\": no element \"{id}\". Available: [{available}]."
    ))
}

pub(super) fn resolve_group(
    op: &IpcValue,
    target: &SlideTarget,
    slide: &Slide,
) -> Result<String, String> {
    let name = op_name(op);
    let group = field(op, "group")
        .and_then(string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("op \"{name}\": \"group\" must be the group element's id."))?;
    let mut available = Vec::new();
    for el in slide.elements.as_ref().into_iter().flatten() {
        if matches!(el, pptx_engine::types::SlideElement::Group(_)) {
            if matches_element_ref(el, group)? {
                return Ok(el.id().to_owned());
            }
            available.push(match element_durable_id(el)? {
                Some(durable) => format!("{} ({durable})", el.id()),
                None => el.id().to_owned(),
            });
        }
    }
    let index = match target {
        SlideTarget::Slide(index) => *index as isize,
        SlideTarget::Part(_) => -1,
    };
    Err(format!(
        "op \"{name}\": no group \"{group}\" on slide {index}. Available groups: [{}].",
        available.join(", ")
    ))
}

pub(super) fn resolve_group_child_id(
    slide: &Slide,
    group_id: &str,
    reference: &str,
) -> Result<String, String> {
    for element in slide.elements.as_ref().into_iter().flatten() {
        if element.id() != group_id {
            continue;
        }
        if let pptx_engine::types::SlideElement::Group(group) = element {
            for child in group.children.as_ref().into_iter().flatten() {
                if matches_element_ref(child, reference)?
                    || pptx_engine::identity::group_child_durable_id(element, child)?.as_deref()
                        == Some(reference)
                {
                    return Ok(child.id().to_owned());
                }
            }
        }
        break;
    }
    Ok(reference.to_owned())
}
