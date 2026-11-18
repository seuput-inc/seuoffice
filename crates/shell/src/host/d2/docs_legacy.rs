//! Legacy DOC conversion through the original doc2docx library service.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;

pub(super) fn convert(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    let result = crate::engine_host::call("legacy-doc.convert", &json!({ "bytesBase64": STANDARD.encode(bytes) }))
        .map_err(|error| error.message)?;
    decode(result)
}

pub(super) fn convert_recovery(bytes: Vec<u8>, receipt: shell_contract::ipc::RecoverySeedReceipt, receiver: shell_contract::ipc::NativeRequestOwner, cancel: shell_contract::runtime::CancelToken) -> Result<Vec<u8>, String> {
    let result = crate::engine_host::call_recovery_seed("legacy-doc.convert", &json!({ "bytesBase64": STANDARD.encode(bytes) }), &receipt, &receiver, &cancel)
        .map_err(|error| error.message)?;
    decode(result)
}

fn decode(result: serde_json::Value) -> Result<Vec<u8>, String> {
    let docx = result.get("docx").and_then(serde_json::Value::as_str)
        .ok_or_else(|| "DOC conversion returned no document".to_owned())?;
    STANDARD.decode(docx).map_err(|error| error.to_string())
}
