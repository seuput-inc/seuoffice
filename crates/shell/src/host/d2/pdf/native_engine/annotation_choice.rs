//! Apply the selected Save As annotation policy after editing.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use shell_contract::{IpcValue, runtime::CancelToken};

pub(super) fn apply(bytes: &[u8], choice: &IpcValue, cancel: &CancelToken) -> Result<Vec<u8>, String> {
    if cancel.is_cancelled() { return Err("PDF save cancelled".into()); }
    let (operation, options) = match choice {
        IpcValue::String(value) if value == "flatten" => ("pdf.flattenAnnotations", None),
        IpcValue::String(value) if value == "strip" => ("pdf.stripAnnots", Some(json!({"kinds":["comments","highlights","drawings","stamps"],"pages":null}))),
        _ => return Ok(bytes.to_vec()),
    };
    let mut input = serde_json::Map::new();
    input.insert("bytesBase64".into(), Value::String(STANDARD.encode(bytes)));
    if let Some(options) = options { input.insert("stripOptions".into(), options); }
    let result = crate::engine_host::call_cancellable(operation, &Value::Object(input), cancel).map_err(|error| error.to_string())?;
    let encoded = if operation == "pdf.stripAnnots" { result.get("bytes").and_then(Value::as_str) } else { result.as_str() }
        .ok_or("invalid PDF annotation response")?;
    STANDARD.decode(encoded).map_err(|_| "invalid PDF annotation response".into())
}
