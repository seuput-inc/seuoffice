//! Native selection and option handling for PDF page tools.

use serde_json::{json, Map, Value};
use shell_contract::{IpcValue, runtime::CancelToken};
use shell_ipc_d2::pdf::tools::ToolError;

use super::super::page_ops;

fn field<'a>(value: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(fields) => fields.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

fn number(value: &IpcValue, name: &str, default: f64) -> f64 {
    match field(value, name) {
        Some(IpcValue::Number(value)) => *value,
        _ => default,
    }
}

fn string<'a>(value: &'a IpcValue, name: &str, default: &'a str) -> &'a str {
    match field(value, name) {
        Some(IpcValue::String(value)) => value,
        _ => default,
    }
}

fn page_indices(value: &str) -> Result<IpcValue, ToolError> {
    let pages = value.split(',').map(|page| {
        let number = page.trim().parse::<f64>().map_err(|_| ToolError::Failed("invalid page number".into()))?;
        Ok(IpcValue::Number(number - 1.0))
    }).collect::<Result<Vec<_>, ToolError>>()?;
    Ok(IpcValue::Array(pages))
}

fn paper(name: &str) -> Option<(f64, f64)> {
    match name {
        "A3" => Some((842.0, 1191.0)),
        "A4" => Some((595.0, 842.0)),
        "A5" => Some((420.0, 595.0)),
        "Letter" => Some((612.0, 792.0)),
        "Legal" => Some((612.0, 1008.0)),
        _ => None,
    }
}

/// Returns None when another tool implementation owns the requested operation.
pub(super) fn run(tool: &str, inputs: &[Vec<u8>], options: &IpcValue, cancel: &CancelToken) -> Option<Result<Vec<Vec<u8>>, ToolError>> {
    if !matches!(tool, "merge" | "split" | "extract" | "reverse" | "nup" | "rotate" | "blankPages" | "pageSize" | "splitPages" | "imagesToPdf" | "duplex" | "overlay" | "stripAnnots" | "toText") {
        return None;
    }
    let Some((first, rest)) = inputs.split_first() else {
        return Some(Err(ToolError::Failed("no input".into())));
    };
    Some((|| -> Result<Vec<Vec<u8>>, ToolError> {
        let result = match tool {
            "merge" => vec![page_ops::merge_pdf(first, rest).map_err(ToolError::Failed)?.0],
            "split" => page_ops::split_pdf(first, number(options, "chunkSize", 1.0)).map_err(ToolError::Failed)?,
            "extract" => {
                let pages = page_indices(string(options, "pages", ""))?;
                vec![page_ops::extract_pages(first, &pages).map_err(ToolError::Failed)?]
            }
            "reverse" => {
                let count = page_ops::call("pdf.pageCount", first, Map::new()).map_err(ToolError::Failed)?;
                let count = count.as_u64().ok_or_else(|| ToolError::Failed("invalid PDF page count".into()))?;
                let pages = IpcValue::Array((0..count).rev().map(|page| IpcValue::Number(page as f64)).collect());
                vec![page_ops::extract_pages(first, &pages).map_err(ToolError::Failed)?]
            }
            "rotate" => {
                let count = page_ops::call("pdf.pageCount", first, Map::new()).map_err(ToolError::Failed)?;
                let count = count.as_u64().ok_or_else(|| ToolError::Failed("invalid PDF page count".into()))?;
                let delta = number(options, "rotation", 90.0);
                let rotations = (0..count).map(|page| (page as f64, delta)).collect::<Vec<_>>();
                vec![page_ops::apply_page_edits(first, &rotations, &[], cancel).map_err(ToolError::Failed)?]
            }
            "blankPages" => {
                let IpcValue::Array(pages) = page_indices(string(options, "removePages", ""))? else { return Err(ToolError::Failed("invalid page range".into())); };
                let pages = pages.into_iter().filter_map(|page| match page { IpcValue::Number(page) => Some(page), _ => None }).collect::<Vec<_>>();
                vec![page_ops::apply_page_edits(first, &[], &pages, cancel).map_err(ToolError::Failed)?]
            }
            "nup" => vec![page_ops::merge_pages(first, number(options, "perSheet", 2.0), string(options, "direction", "horizontal") == "horizontal", matches!(field(options, "separator"), Some(IpcValue::Bool(true)))).map_err(ToolError::Failed)?],
            "pageSize" => {
                let (width, height) = paper(string(options, "paper", "A4")).ok_or_else(|| ToolError::Failed("invalid paper size".into()))?;
                vec![page_ops::set_page_size(first, width, height).map_err(ToolError::Failed)?]
            }
            "splitPages" => vec![page_ops::split_pages(first, number(options, "perPage", 2.0)).map_err(ToolError::Failed)?],
            "imagesToPdf" => {
                let name = string(options, "imagePaper", "A4");
                let paper = if name == "image" { Value::Null } else {
                    let (w, h) = paper(name).ok_or_else(|| ToolError::Failed("invalid paper size".into()))?;
                    json!({ "w": w, "h": h })
                };
                let margin = match string(options, "margin", "small") {
                    "none" => 0, "small" => 18, "large" => 36,
                    _ => return Err(ToolError::Failed("invalid margin".into())),
                };
                let mut input = Map::new();
                input.insert("imagesBase64".into(), Value::Array(inputs.iter().map(|bytes| page_ops::encode(bytes)).collect()));
                input.insert("imageOptions".into(), json!({ "paper": paper, "marginPt": margin }));
                let value = page_ops::call("pdf.imagesToPdf", first, input).map_err(ToolError::Failed)?;
                vec![page_ops::bytes(&value).map_err(ToolError::Failed)?]
            }
            "duplex" => {
                let mut input = Map::new();
                input.insert("imagesBase64".into(), Value::Array(inputs.iter().map(|bytes| page_ops::encode(bytes)).collect()));
                input.insert("multiple".into(), json!(2.0 * number(options, "sheetPages", 1.0)));
                let value = page_ops::call("pdf.duplex", first, input).map_err(ToolError::Failed)?;
                vec![page_ops::bytes(&value).map_err(ToolError::Failed)?]
            }
            "overlay" => {
                let background = rest.first().ok_or_else(|| ToolError::Failed("no background".into()))?;
                let pages = match field(options, "applyPages") {
                    Some(IpcValue::String(value)) if !value.is_empty() => {
                        let IpcValue::Array(pages) = page_indices(value)? else { return Err(ToolError::Failed("invalid page range".into())); };
                        Value::Array(pages.into_iter().filter_map(|page| match page { IpcValue::Number(value) => Some(json!(value)), _ => None }).collect())
                    }
                    _ => Value::Null,
                };
                let mut input = Map::new();
                input.insert("otherBase64".into(), page_ops::encode(background));
                input.insert("overlayOptions".into(), json!({
                    "behind": string(options, "layer", "behind") != "front",
                    "backgroundPage": number(options, "backgroundPage", 1.0) - 1.0,
                    "pages": pages
                }));
                let value = page_ops::call("pdf.overlay", first, input).map_err(ToolError::Failed)?;
                vec![page_ops::bytes(&value).map_err(ToolError::Failed)?]
            }
            "stripAnnots" => {
                let kinds = match field(options, "annotKinds") {
                    Some(IpcValue::Array(kinds)) => kinds.iter().map(|kind| match kind {
                        IpcValue::String(kind) => Ok(Value::String(kind.clone())),
                        _ => Err(ToolError::Failed("invalid annotation kind".into())),
                    }).collect::<Result<Vec<_>, _>>()?,
                    _ => ["comments", "highlights", "drawings", "stamps", "other"].into_iter().map(|kind| json!(kind)).collect(),
                };
                let pages = match field(options, "applyPages") {
                    Some(IpcValue::String(value)) if !value.is_empty() => {
                        let IpcValue::Array(pages) = page_indices(value)? else { return Err(ToolError::Failed("invalid page range".into())); };
                        Value::Array(pages.into_iter().filter_map(|page| match page { IpcValue::Number(value) => Some(json!(value)), _ => None }).collect())
                    }
                    _ => Value::Null,
                };
                let mut input = Map::new();
                input.insert("stripOptions".into(), json!({ "kinds": kinds, "pages": pages }));
                let value = page_ops::call("pdf.stripAnnots", first, input).map_err(ToolError::Failed)?;
                let removed = value.get("removed").and_then(Value::as_u64).ok_or_else(|| ToolError::Failed("invalid annotation result".into()))?;
                if removed == 0 { return Err(ToolError::Refusal("no-annots".into())); }
                vec![page_ops::bytes(&value["bytes"]).map_err(ToolError::Failed)?]
            }
            "toText" => {
                let text = page_ops::read_pdf_text(first, None).map_err(ToolError::Failed)?;
                let pages: Vec<&str> = text.pages.iter().map(|page| page.trim_end_matches(jscompat::is_js_space)).filter(|page| !jscompat::trim(page).is_empty()).collect();
                if pages.is_empty() { return Err(ToolError::Refusal("no-text".into())); }
                vec![format!("{}\n", pages.join("\n\n")).into_bytes()]
            }
            _ => return Err(ToolError::Failed(format!("unknown PDF tool: {tool}"))),
        };
        Ok(result)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_tools_reject_missing_input_without_calling_vendor() {
        assert_eq!(
            run("split", &[], &IpcValue::Null, &CancelToken::default()),
            Some(Err(ToolError::Failed("no input".into())))
        );
    }
}
