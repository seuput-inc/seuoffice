use pptx_engine::identity::{group_child_durable_id, matches_element_ref};
use pptx_engine::notes::notes_path_for_slide;
use pptx_engine::redaction_xml::has_redact_ext_in;
use pptx_engine::types::{SlideElement, TextBody};
use shell_contract::IpcValue;

use super::registry::{field, op_name, resolve_slide, string, OpContext};

fn text_label(body: &TextBody) -> Option<&str> {
    body.paragraphs.as_ref()?.iter().flat_map(|p| p.runs.as_ref().into_iter().flatten())
        .find_map(|run| run.redact.as_deref().filter(|s| !s.is_empty()))
}

fn element_label(element: &SlideElement) -> Option<&str> {
    if let Some(label) = pptx_engine::with_element!(element, e => e.redact.as_deref()).filter(|s| !s.is_empty()) {
        return Some(label);
    }
    match element {
        SlideElement::Text(e) => e.text.as_ref().and_then(text_label),
        SlideElement::Table(e) => e.rows.as_ref().into_iter().flatten().flatten()
            .find_map(|cell| cell.text.as_ref().and_then(text_label)),
        SlideElement::Group(e) => e.children.as_ref().into_iter().flatten().find_map(element_label),
        _ => None,
    }
}

pub(super) fn guard(op: &IpcValue, ctx: &mut OpContext<'_>) -> Result<(), String> {
    let target = match resolve_slide(ctx, op, true) { Ok(target) => target, Err(_) => return Ok(()) };
    let slide = ctx.slide(&target);
    let name = op_name(op);
    let content = matches!(name, "setText" | "deleteElement" | "setTableCell" | "tableMerge" | "tableStructure" | "ungroupElement" | "replacePicture" | "addMedia" | "insertEquation" | "pasteElements" | "setShapeCustomGeometry" | "setChart");
    let mut label = None;
    let mut what = String::new();
    if content {
        if let Some(id) = field(op, "target").and_then(|t| field(t, "el")).and_then(string) {
            let elements = if let Some(group) = field(op, "group").and_then(string).filter(|s| !s.is_empty()) {
                let mut found = None;
                for element in slide.elements.as_ref().into_iter().flatten() {
                    if let SlideElement::Group(g) = element {
                        if matches_element_ref(element, group)? {
                            for child in g.children.as_ref().into_iter().flatten() {
                                if matches_element_ref(child, id)? || group_child_durable_id(element, child)?.as_deref() == Some(id) {
                                    found = element_label(child);
                                    break;
                                }
                            }
                            break;
                        }
                    }
                }
                label = found;
                None
            } else { slide.elements.as_ref() };
            if let Some(elements) = elements {
                for element in elements {
                    if matches_element_ref(element, id)? {
                        label = element_label(element);
                        break;
                    }
                }
            }
        }
        what = format!("{name} edit");
    } else if name == "deleteSlide" {
        label = slide.elements.as_ref().into_iter().flatten().find_map(element_label);
        what = "page delete".to_owned();
    }
    if let Some(label) = label {
        return Err(format!("this {what} would overwrite text withheld from AI ({{{{{label}}}}}). Its words stay in the deck; work around the placeholder instead of replacing it."));
    }
    if matches!(name, "deleteSlide" | "setNotes") {
        if let Some(path) = notes_path_for_slide(&ctx.opened.archive, slide.path.as_deref().unwrap_or_default())? {
            if let Some(xml) = ctx.opened.archive.read_text(&path) {
                if has_redact_ext_in(&xml)? {
                    let what = if name == "setNotes" { "notes edit" } else { "page delete" };
                    return Err(format!("this {what} would overwrite text withheld from AI ({{{{private}}}}). Its words stay in the deck; work around the placeholder instead of replacing it."));
                }
            }
        }
    }
    Ok(())
}
