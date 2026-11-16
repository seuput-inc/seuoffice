//! PDFium extraction boundary for structure jobs.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value};
use shell_contract::{IpcValue, runtime::CancelToken};
use shell_ipc_d2::pdf::tools::{JobFailure, JobOutcome};

use super::super::{job_source::Source, native_callbacks};

fn field<'a>(value: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(fields) => fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn failure(value: &Value) -> JobFailure {
    let name = value.get("name").and_then(Value::as_str).unwrap_or("");
    let code = value.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = match name {
        "PdfLoadError" if matches!(code, "password-required" | "corrupt" | "unsupported") => code,
        "PageRangeError" => "page-range",
        "AssetConflictError" => "asset-conflict",
        "NotAvailableError" => "not-available",
        "WritebackEncryptedError" => "unsupported",
        "AbortError" => "cancelled",
        _ => "failed",
    };
    let mut result = JobFailure::new(kind, value.get("message").and_then(Value::as_str));
    if kind == "page-range" {
        let range = &value["validRange"];
        let count = |value: &Value| {
            value
                .as_f64()
                .filter(|n| *n >= 0.0 && *n <= 9_007_199_254_740_991.0 && n.fract() == 0.0)
        };
        result.valid_range = range
            .get("first")
            .and_then(count)
            .zip(range.get("last").and_then(count));
    }
    result
}

pub(super) fn extract(
    job: &IpcValue,
    source: &Source,
    operation: &str,
    progress: &(dyn Fn(f64, f64) + Sync),
    cancel: &CancelToken,
) -> JobOutcome {
    let mut input = Map::new();
    input.insert("operation".into(), Value::String(operation.to_owned()));
    input.insert(
        "bytesBase64".into(),
        Value::String(STANDARD.encode(&source.bytes)),
    );
    let available = native_callbacks::ocr_available()
        .map_err(|error| JobFailure::new("failed", Some(&error)))?;
    input.insert("ocrAvailable".into(), Value::Bool(available));
    if let Some(job_source) = field(job, "source") {
        if let Some(IpcValue::String(password)) = field(job_source, "password") {
            input.insert("password".into(), Value::String(password.clone()));
        }
        if let Some(IpcValue::Array(pages)) = field(job_source, "pages") {
            let pages = pages
                .iter()
                .map(|page| match page {
                    IpcValue::Number(number) => {
                        serde_json::Number::from_f64(*number).map(Value::Number)
                    }
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| JobFailure::new("failed", Some("invalid PDF page range")))?;
            input.insert("pages".into(), Value::Array(pages));
        }
    }
    if let Some(IpcValue::Number(minimum)) = field(job, "minImagePx") {
        let minimum = serde_json::Number::from_f64(*minimum)
            .ok_or_else(|| JobFailure::new("failed", Some("invalid image size")))?;
        input.insert("minImagePx".into(), Value::Number(minimum));
    }
    let result = crate::engine_host::call_with_progress(
        "pdf.structureOperation",
        &Value::Object(input),
        cancel,
        progress,
    )
    .map_err(|error| {
        JobFailure::new(
            if error.name == "AbortError" {
                "cancelled"
            } else {
                "failed"
            },
            Some(&error.message),
        )
    })?;
    decode_response(&result)
}

fn decode_response(result: &Value) -> JobOutcome {
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(failure(result));
    }
    let header = result
        .get("header")
        .ok_or_else(|| JobFailure::new("failed", Some("invalid PDF structure response")))?;
    let buffers = result
        .get("buffersBase64")
        .and_then(Value::as_array)
        .ok_or_else(|| JobFailure::new("failed", Some("invalid PDF structure buffers")))?
        .iter()
        .map(|buffer| {
            buffer
                .as_str()
                .ok_or(())
                .and_then(|text| STANDARD.decode(text).map_err(|_| ()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| JobFailure::new("failed", Some("invalid PDF structure buffers")))?;
    shell_core::wire::decode(header, &buffers)
        .map_err(|error| JobFailure::new("failed", Some(&error.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn page_range_failure_keeps_the_valid_range() {
        let failure = failure(&json!({
            "name": "PageRangeError",
            "message": "page 9",
            "validRange": { "first": 1, "last": 4 }
        }));
        assert_eq!(failure.error, "page-range");
        assert_eq!(failure.valid_range, Some((1.0, 4.0)));
        assert_eq!(failure.message.as_deref(), Some("page 9"));
    }

    #[test]
    fn pdf_load_error_keeps_its_specific_code() {
        let failure =
            failure(&json!({ "name": "PdfLoadError", "code": "corrupt", "message": "bad PDF" }));
        assert_eq!(failure.error, "corrupt");
    }

    #[test]
    fn extracted_image_keeps_binary_bytes_through_service_wire() {
        let response = json!({
            "ok": true,
            "header": {"images": [{"png": {"$": "b", "k": "u8", "i": 0}}]},
            "buffersBase64": ["AP+A"]
        });
        let value = decode_response(&response).unwrap();
        assert_eq!(
            value,
            IpcValue::Object(vec![(
                "images".into(),
                IpcValue::Array(vec![IpcValue::Object(vec![(
                    "png".into(),
                    IpcValue::Bytes(shell_contract::BytesKind::Uint8Array, vec![0, 255, 128])
                )])])
            )])
        );
    }
}
