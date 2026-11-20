//! PDF-lib operations shared with the PDF engine service.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

fn bytes(value: Value) -> Result<Vec<u8>, String> {
    let text = value
        .as_str()
        .ok_or_else(|| "PDF engine returned a non-string result".to_owned())?;
    STANDARD.decode(text).map_err(|error| error.to_string())
}

pub(super) fn add_links(pdf: Vec<u8>, links: Value) -> Result<Vec<u8>, String> {
    let result = crate::engine_host::call(
        "pdf.docsAddInternalLinks",
        &json!({
            "bytesBase64": STANDARD.encode(pdf), "links": links,
        }),
    )
    .map_err(|error| error.message)?;
    bytes(result)
}

pub(super) fn merge(parts: Vec<Vec<u8>>, links: Option<Value>) -> Result<Vec<u8>, String> {
    let parts: Vec<String> = parts
        .into_iter()
        .map(|part| STANDARD.encode(part))
        .collect();
    let result = crate::engine_host::call(
        "pdf.docsMerge",
        &json!({
            "partsBase64": parts, "links": links,
        }),
    )
    .map_err(|error| error.message)?;
    bytes(result)
}
