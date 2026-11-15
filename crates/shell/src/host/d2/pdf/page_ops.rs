//! Native byte boundary for the isolated PDF vendor page operations.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value};
use shell_contract::IpcValue;
use shell_ipc_d2::pdf::editor::hosts::PdfText;

fn ipc_value(value: Value) -> IpcValue {
    match value {
        Value::Null => IpcValue::Null,
        Value::Bool(value) => IpcValue::Bool(value),
        Value::Number(value) => IpcValue::Number(value.as_f64().unwrap_or(f64::NAN)),
        Value::String(value) => IpcValue::String(value),
        Value::Array(values) => IpcValue::Array(values.into_iter().map(ipc_value).collect()),
        Value::Object(values) => IpcValue::Object(values.into_iter().map(|(key, value)| (key, ipc_value(value))).collect()),
    }
}

pub(super) fn read_pdf_text(source: &[u8], password: Option<&str>) -> Result<PdfText, String> {
    let mut options = Map::new();
    if let Some(password) = password { options.insert("password".into(), Value::String(password.to_owned())); }
    let value = call("pdf.readPdfText", source, options)?;
    let page_count = value["pageCount"].as_f64().ok_or("PDFium returned no page count")?;
    let pages = value["pages"].as_array().ok_or("PDFium returned no pages")?.iter()
        .map(|page| page.as_str().map(str::to_owned).ok_or_else(|| "PDFium returned invalid page text".to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PdfText { page_count, pages })
}

pub(super) fn read_annot_subjects(source: &[u8]) -> Result<IpcValue, String> {
    let value = call("pdf.readAnnotSubjects", source, Map::new())?;
    if !value.is_array() { return Err("PDF engine returned invalid annotation subjects".to_owned()); }
    Ok(ipc_value(value))
}

pub(super) fn read_static_form_fills(source: &[u8]) -> Result<IpcValue, String> {
    let value = call("pdf.readStaticFormFills", source, Map::new())?;
    if !value.is_array() { return Err("PDF engine returned invalid static form fills".to_owned()); }
    Ok(ipc_value(value))
}

fn json_value(value: &IpcValue) -> Result<Value, String> {
    Ok(match value {
        IpcValue::Undefined | IpcValue::Null => Value::Null,
        IpcValue::Bool(value) => Value::Bool(*value),
        IpcValue::Number(value) | IpcValue::Date(value) => serde_json::Number::from_f64(*value).map_or(Value::Null, Value::Number),
        IpcValue::String(value) => Value::String(value.clone()),
        IpcValue::Array(items) => Value::Array(items.iter().map(json_value).collect::<Result<_, _>>()?),
        IpcValue::Object(items) => Value::Object(items.iter().filter(|(_, value)| !matches!(value, IpcValue::Undefined)).map(|(key, value)| Ok((key.clone(), json_value(value)?))).collect::<Result<_, String>>()?),
        IpcValue::Bytes(_, _) => return Err("Unexpected binary value in PDF page options".to_owned()),
    })
}

fn object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Map<String, Value> {
    entries.into_iter().map(|(key, value)| (key.to_owned(), value)).collect()
}

fn binary(name: &str, source: &[u8], input: Map<String, Value>) -> Result<Vec<u8>, String> {
    bytes(&call(name, source, input)?)
}

pub(super) fn extract_pages(source: &[u8], pages: &IpcValue) -> Result<Vec<u8>, String> {
    binary("pdf.extractPages", source, object([("pages", json_value(pages)?)]))
}

pub(super) fn extract_pages_cancellable(source: &[u8], pages: &IpcValue, cancel: &shell_contract::runtime::CancelToken) -> Result<Vec<u8>, String> {
    bytes(&call_cancellable("pdf.extractPages", source, object([("pages", json_value(pages)?)]), cancel)?)
}

pub(super) fn insert_pdf(source: &[u8], other: &[u8], after: f64) -> Result<(Vec<u8>, f64), String> {
    let result = call("pdf.insertPdf", source, object([("otherBase64", encode(other)), ("after", Value::from(after))]))?;
    Ok((bytes(&result["merged"])?, result["count"].as_f64().ok_or("PDF engine returned no page count")?))
}

pub(super) fn insert_blank_page(source: &[u8], after: f64) -> Result<Vec<u8>, String> {
    binary("pdf.insertBlankPage", source, object([("after", Value::from(after))]))
}

pub(super) fn split_pdf(source: &[u8], size: f64) -> Result<Vec<Vec<u8>>, String> {
    let result = call("pdf.splitPdf", source, object([("chunkSize", Value::from(size))]))?;
    result.as_array().ok_or_else(|| "PDF engine returned non-array parts".to_owned())?.iter().map(bytes).collect()
}

pub(super) fn merge_pages(source: &[u8], per_sheet: f64, horizontal: bool, separator: bool) -> Result<Vec<u8>, String> {
    binary("pdf.mergePages", source, object([
        ("perSheet", Value::from(per_sheet)),
        ("direction", Value::from(if horizontal { "horizontal" } else { "vertical" })),
        ("separator", Value::Bool(separator)),
    ]))
}

pub(super) fn merge_pdf(source: &[u8], others: &[Vec<u8>]) -> Result<(Vec<u8>, f64), String> {
    let result = call("pdf.mergePdf", source, object([("othersBase64", Value::Array(others.iter().map(|item| encode(item)).collect()))]))?;
    Ok((bytes(&result["merged"])?, result["appended"].as_f64().ok_or("PDF engine returned no appended count")?))
}

pub(super) fn replace_pages(source: &[u8], other: &[u8], pages: &IpcValue) -> Result<(Vec<u8>, IpcValue, IpcValue), String> {
    let result = call("pdf.replacePages", source, object([("otherBase64", encode(other)), ("pages", json_value(pages)?)]))?;
    let removed = result["removed"].as_f64().ok_or("PDF engine returned no removed count")?;
    let inserted = result["inserted"].as_f64().ok_or("PDF engine returned no inserted count")?;
    Ok((bytes(&result["merged"])?, IpcValue::Number(removed), IpcValue::Number(inserted)))
}

pub(super) fn set_page_size(source: &[u8], width: f64, height: f64) -> Result<Vec<u8>, String> {
    binary("pdf.setPageSize", source, object([("width", Value::from(width)), ("height", Value::from(height))]))
}

pub(super) fn split_pages(source: &[u8], per_page: f64) -> Result<Vec<u8>, String> {
    binary("pdf.splitPages", source, object([("perPage", Value::from(per_page))]))
}

pub(super) fn crop_pages(source: &[u8], pages: &IpcValue, rect: &IpcValue) -> Result<Vec<u8>, String> {
    binary("pdf.cropPages", source, object([("pages", json_value(pages)?), ("rect", json_value(rect)?)]))
}

pub(super) fn apply_page_edits(source: &[u8], rotations: &[(f64, f64)], deleted_pages: &[f64], cancel: &shell_contract::runtime::CancelToken) -> Result<Vec<u8>, String> {
    let rotations = rotations.iter().map(|(page, delta)| serde_json::json!({"pageIndex": page, "delta": delta})).collect();
    let deleted = deleted_pages.iter().copied().map(Value::from).collect();
    let mut input = object([("rotations", Value::Array(rotations)), ("deletedPages", Value::Array(deleted))]);
    input.insert("bytesBase64".into(), encode(source));
    let result = crate::engine_host::call_cancellable("pdf.applyPageEdits", &Value::Object(input), cancel).map_err(|error| error.to_string())?;
    bytes(&result)
}

pub(super) fn call(name: &str, bytes: &[u8], mut input: Map<String, Value>) -> Result<Value, String> {
    input.insert("bytesBase64".to_owned(), Value::String(STANDARD.encode(bytes)));
    crate::engine_host::call(name, &Value::Object(input)).map_err(|error| error.to_string())
}

pub(super) fn call_cancellable(name: &str, bytes: &[u8], mut input: Map<String, Value>, cancel: &shell_contract::runtime::CancelToken) -> Result<Value, String> {
    input.insert("bytesBase64".to_owned(), Value::String(STANDARD.encode(bytes)));
    crate::engine_host::call_cancellable(name, &Value::Object(input), cancel).map_err(|error| error.to_string())
}

pub(super) fn bytes(value: &Value) -> Result<Vec<u8>, String> {
    let text = value.as_str().ok_or_else(|| "PDF engine returned non-string bytes".to_owned())?;
    STANDARD.decode(text).map_err(|error| format!("PDF engine returned invalid base64: {error}"))
}

pub(super) fn encode(bytes: &[u8]) -> Value {
    Value::String(STANDARD.encode(bytes))
}
