//! Native qpdf and PDF repair job handling.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value};
use shell_contract::{BytesKind, IpcValue, runtime::CancelToken};
use shell_ipc_d2::pdf::tools::{JobFailure, JobOutcome};

fn field<'a>(value: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(fields) => fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn optional_string(value: &IpcValue, key: &str) -> Option<String> {
    match field(value, key) {
        Some(IpcValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn service_failure(value: &Value) -> JobFailure {
    let name = value.get("name").and_then(Value::as_str).unwrap_or("");
    let code = value.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = match name {
        "PdfLoadError" if matches!(code, "password-required" | "corrupt" | "unsupported") => code,
        "AbortError" => "cancelled",
        _ => "failed",
    };
    JobFailure::new(kind, value.get("message").and_then(Value::as_str))
}

pub(super) fn run(job: &IpcValue, cancel: &CancelToken) -> JobOutcome {
    let source = super::job_source::load(job, cancel)?;
    let operation = optional_string(job, "op")
        .ok_or_else(|| JobFailure::new("failed", Some("invalid PDF job operation")))?;
    let mut input = Map::new();
    input.insert(
        "bytesBase64".into(),
        Value::String(STANDARD.encode(source.bytes)),
    );
    let step = match operation.as_str() {
        "encrypt" => {
            let password = optional_string(job, "userPassword")
                .ok_or_else(|| JobFailure::new("failed", Some("invalid PDF password")))?;
            input.insert("userPassword".into(), Value::String(password));
            if let Some(owner) = optional_string(job, "ownerPassword") {
                input.insert("ownerPassword".into(), Value::String(owner));
            }
            "pdf.qpdfEncrypt"
        }
        "decrypt" => {
            let password = optional_string(job, "password")
                .ok_or_else(|| JobFailure::new("failed", Some("invalid PDF password")))?;
            input.insert("password".into(), Value::String(password));
            "pdf.qpdfDecrypt"
        }
        "repair" => "pdf.repair",
        _ => return Err(JobFailure::new("failed", Some("invalid binary PDF job"))),
    };
    let result = crate::engine_host::call_cancellable(step, &Value::Object(input), cancel)
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
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(service_failure(&result));
    }
    let encoded = result
        .get("bytesBase64")
        .and_then(Value::as_str)
        .ok_or_else(|| JobFailure::new("failed", Some("invalid PDF engine response")))?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| JobFailure::new("failed", Some("invalid PDF engine response")))?;
    Ok(IpcValue::Object(vec![(
        "bytes".into(),
        IpcValue::Bytes(BytesKind::Uint8Array, bytes),
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_distinct_pdf_load_failure_codes() {
        for code in ["password-required", "corrupt", "unsupported"] {
            let error = service_failure(
                &json!({"name": "PdfLoadError", "code": code, "message": "cannot load"}),
            );
            assert_eq!(error.error, code);
        }
    }
}
