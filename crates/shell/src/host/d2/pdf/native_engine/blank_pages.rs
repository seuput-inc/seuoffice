//! Append newly created pages before applying content edits.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;
use shell_contract::{IpcValue, runtime::CancelToken};

fn field<'a>(value: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(entries) => entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value),
        _ => None,
    }
}
fn number(value: Option<&IpcValue>) -> Option<f64> {
    match value {
        Some(IpcValue::Number(value)) => Some(*value),
        _ => None,
    }
}

pub(super) fn append(
    source: &[u8],
    blanks: &[IpcValue],
    cancel: &CancelToken,
) -> Result<Vec<u8>, String> {
    if blanks.is_empty() {
        return Ok(source.to_vec());
    }
    let encoded = STANDARD.encode(source);
    let count = crate::engine_host::call_cancellable(
        "pdf.pageCount",
        &json!({"bytesBase64": encoded}),
        cancel,
    )
    .map_err(|error| error.to_string())?;
    let mut expected = count.as_f64().ok_or("invalid PDF page count")?;
    let mut ordered: Vec<_> = blanks.iter().collect();
    ordered.sort_by(|a, b| {
        number(field(a, "pageIndex"))
            .unwrap_or(f64::NAN)
            .total_cmp(&number(field(b, "pageIndex")).unwrap_or(f64::NAN))
    });
    let mut pages = Vec::with_capacity(ordered.len());
    for page in ordered {
        let index = number(field(page, "pageIndex")).ok_or("invalid blank page index")?;
        if index != expected {
            return Err(format!(
                "Blank page index {} does not follow the document ({} pages)",
                jscompat::number_to_string(index),
                jscompat::number_to_string(expected)
            ));
        }
        let width = number(field(page, "width")).ok_or("Blank page needs a positive size")?;
        let height = number(field(page, "height")).ok_or("Blank page needs a positive size")?;
        if !(width > 0.0) || !(height > 0.0) {
            return Err("Blank page needs a positive size".into());
        }
        pages.push(json!({"width": width, "height": height}));
        expected += 1.0;
    }
    let value = crate::engine_host::call_cancellable(
        "pdf.appendBlankPages",
        &json!({"bytesBase64": encoded, "pages": pages}),
        cancel,
    )
    .map_err(|error| error.to_string())?;
    let bytes = value.as_str().ok_or("invalid PDF page response")?;
    STANDARD
        .decode(bytes)
        .map_err(|_| "invalid PDF page response".into())
}
