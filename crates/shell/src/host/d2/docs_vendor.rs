//! Vendor dependencies used by the native Docs engine.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use main_core::docs::hwp_export::{HwpConversion, HwpError, HwpFormat, Loss};
use serde_json::{Value, json};
use shell_contract::IpcValue;
use std::time::Duration;

mod xlsx_rows;

pub(super) fn read_xlsx_rows(bytes: &[u8]) -> Result<Vec<Vec<String>>, String> {
    xlsx_rows::read(bytes)
}

pub(super) fn html_to_docx(html: Vec<u8>, timeout: Duration, view: shell_contract::ViewId, cancel: shell_contract::runtime::CancelToken) -> Result<Option<Vec<u8>>, String> {
    let reply = match crate::engine_host::call_with_timeout_cancellable("docs.htmlToDocx", &json!({
        "htmlBase64": STANDARD.encode(html), "ownerViewId": view.0, "naturalTableWidth": true
    }), timeout, &cancel) {
        Ok(reply) => reply,
        Err(_) => return Ok(None),
    };
    let Some(header) = reply.get("header") else { return Ok(None) };
    let Some(buffers) = reply.get("buffers").and_then(Value::as_array) else { return Ok(None) };
    let buffers = buffers.iter().map(|v| v.as_str().and_then(|s| STANDARD.decode(s).ok())).collect::<Option<Vec<_>>>();
    let Some(buffers) = buffers else { return Ok(None) };
    match shell_core::wire::decode(header, &buffers) {
        Ok(IpcValue::Bytes(_, bytes)) => Ok(Some(bytes)),
        _ => Ok(None),
    }
}

fn field<'a>(value: &'a IpcValue, name: &str) -> Option<&'a IpcValue> {
    let IpcValue::Object(fields) = value else { return None };
    fields.iter().find(|(key, _)| key == name).map(|(_, value)| value)
}

fn decode_conversion(reply: Value) -> Result<HwpConversion, HwpError> {
    let invalid = || HwpError::Error("Hangul conversion returned an invalid reply".into());
    if reply.get("ok").and_then(Value::as_bool) == Some(false) {
        let code = reply.get("code").and_then(Value::as_str).ok_or_else(invalid)?;
        let message = reply.get("error").and_then(Value::as_str).ok_or_else(invalid)?;
        return Err(HwpError::Failure { code: code.into(), message: message.into() });
    }
    if reply.get("ok").and_then(Value::as_bool) != Some(true) { return Err(invalid()); }
    let header = reply.get("header").ok_or_else(invalid)?;
    let buffers = reply.get("buffers").and_then(Value::as_array).ok_or_else(invalid)?
        .iter().map(|buffer| buffer.as_str().ok_or_else(invalid)
            .and_then(|text| STANDARD.decode(text).map_err(|_| invalid())))
        .collect::<Result<Vec<_>, _>>()?;
    let value = shell_core::wire::decode(header, &buffers).map_err(|_| invalid())?;
    let IpcValue::Object(fields) = value else { return Err(invalid()) };
    let mut bytes = None;
    let mut losses = None;
    for (name, value) in fields {
        match (name.as_str(), value) {
            ("bytes", IpcValue::Bytes(_, data)) => bytes = Some(data),
            ("losses", IpcValue::Array(items)) => {
                losses = Some(items.iter().map(|item| {
                    let Some(IpcValue::String(feature)) = field(item, "feature") else { return Err(invalid()) };
                    let Some(IpcValue::String(kind)) = field(item, "kind") else { return Err(invalid()) };
                    let Some(IpcValue::Number(count)) = field(item, "count") else { return Err(invalid()) };
                    Ok(Loss { feature: feature.clone(), kind: kind.clone(), count: *count })
                }).collect::<Result<Vec<_>, _>>()?);
            }
            _ => {}
        }
    }
    Ok(HwpConversion { bytes: bytes.ok_or_else(invalid)?, losses: losses.ok_or_else(invalid)? })
}

pub(super) fn convert_to_hwp(docx: Vec<u8>, format: HwpFormat) -> Result<HwpConversion, HwpError> {
    let reply = crate::engine_host::call_without_timeout("docs.hwp", &json!({
        "bytesBase64": STANDARD.encode(docx), "format": format.as_str(),
    })).map_err(|error| HwpError::Error(error.message))?;
    decode_conversion(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_worker_bytes_losses_and_package_errors() {
        let result = decode_conversion(json!({"ok":true,"header":{
            "bytes":{"$":"b","k":"u8","i":0},
            "losses":[{"feature":"chart","kind":"dropped","count":2}]
        },"buffers":["AQI="]})).unwrap();
        assert_eq!(result.bytes, [1, 2]);
        assert_eq!(result.losses, vec![Loss { feature: "chart".into(), kind: "dropped".into(), count: 2.0 }]);
        assert_eq!(decode_conversion(json!({"ok":false,"code":"not-docx","error":"bad"})),
            Err(HwpError::Failure { code: "not-docx".into(), message: "bad".into() }));
    }

    #[test]
    fn rejects_malformed_conversion_results() {
        assert!(decode_conversion(json!({"ok":true,"header":{"bytes":{"$":"b","k":"u8","i":1},"losses":[]},"buffers":["AQI="]})).is_err());
        assert!(decode_conversion(json!({"ok":false,"code":"not-docx"})).is_err());
    }
}
