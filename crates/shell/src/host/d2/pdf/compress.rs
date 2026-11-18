//! PDF compression policy; PDF object and image operations run in the vendor worker.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use shell_contract::runtime::CancelToken;

fn call(name: &str, input: &Value, cancel: &CancelToken) -> Result<Value, String> {
    crate::engine_host::call_cancellable(name, input, cancel).map_err(|error| error.to_string())
}

fn eligible(value: &Value) -> bool {
    value.get("subtype").and_then(Value::as_str) == Some("Image") &&
        value.get("mask").and_then(Value::as_bool) == Some(false) &&
        value.get("imageMask").and_then(Value::as_bool) == Some(false) &&
        value.get("hasMask").and_then(Value::as_bool) == Some(false) &&
        value.get("hasDecode").and_then(Value::as_bool) == Some(false) &&
        value.get("bits").and_then(Value::as_f64) == Some(8.0) &&
        value.get("width").and_then(Value::as_f64).is_some_and(|value| value >= 1.0) &&
        value.get("height").and_then(Value::as_f64).is_some_and(|value| value >= 1.0) &&
        value.get("size").and_then(Value::as_u64).is_some_and(|value| value >= 48 * 1024) &&
        matches!(value.get("components").and_then(Value::as_u64), Some(1 | 3)) &&
        matches!(value.get("filter").and_then(Value::as_str), Some("DCTDecode" | "FlateDecode")) &&
        (value.get("filter").and_then(Value::as_str) != Some("FlateDecode") ||
            !value.get("predictor").and_then(Value::as_f64).is_some_and(|value| value > 1.0))
}

pub(super) fn run(bytes: &[u8], level: &str, progress: &(dyn Fn(f64, f64) + Sync), cancel: &CancelToken) -> Result<Vec<u8>, String> {
    let (long_side, quality) = match level {
        "light" => (3000, 85), "medium" => (2000, 75), "strong" => (1400, 60),
        _ => return Err("invalid PDF compression level".into()),
    };
    let source = STANDARD.encode(bytes);
    let streams = call("pdf.compressEnumerate", &json!({"bytesBase64": source}), cancel)?;
    let candidates: Vec<&Value> = streams.as_array().ok_or("invalid PDF stream list")?.iter().filter(|value| eligible(value)).collect();
    let mut pictures = Vec::new();
    for image in candidates {
        if cancel.is_cancelled() { return Err("PDF compression cancelled".into()); }
        let reference = image.get("ref").and_then(Value::as_str).ok_or("invalid PDF stream reference")?;
        let filter = image.get("filter").and_then(Value::as_str).ok_or("invalid PDF stream filter")?;
        let data = call("pdf.compressRead", &json!({"bytesBase64": source, "ref": reference, "inflated": filter == "FlateDecode"}), cancel)?;
        let Some(data) = data.as_str() else { continue };
        let raw = STANDARD.decode(data).map_err(|_| "invalid PDF image data")?;
        let width = image["width"].as_f64().ok_or("invalid PDF image width")?;
        let height = image["height"].as_f64().ok_or("invalid PDF image height")?;
        let components = image["components"].as_u64().ok_or("invalid PDF image components")?;
        let usable = if filter == "DCTDecode" {
            super::image_info::unrotated_image(&raw)
        } else {
            width * height * components as f64 <= raw.len() as f64
        };
        if usable { pictures.push((image, data.to_owned(), width, height, components)); }
    }
    let mut replacements = Vec::new();
    for (index, (image, data, width, height, components)) in pictures.iter().enumerate() {
        if cancel.is_cancelled() { return Err("PDF compression cancelled".into()); }
        let reference = image["ref"].as_str().ok_or("invalid PDF stream reference")?;
        let kind = if image["filter"].as_str() == Some("DCTDecode") { "jpeg" } else { "raw" };
        let result = call("pdf.compressRecode", &json!({"kind": kind, "dataBase64": data, "width": width, "height": height, "components": components, "longSide": long_side, "quality": quality}), cancel)?;
        if !result.is_null() {
            let jpeg = result.get("jpegBase64").and_then(Value::as_str).ok_or("invalid recoded PDF image")?;
            let length = STANDARD.decode(jpeg).map_err(|_| "invalid recoded PDF image")?.len() as u64;
            if length < image["size"].as_u64().ok_or("invalid PDF stream size")? {
                replacements.push(json!({"ref": reference, "jpegBase64": jpeg, "width": result["width"], "height": result["height"]}));
            }
        }
        progress((index + 1) as f64, pictures.len() as f64);
    }
    let result = call("pdf.compressReplace", &json!({"bytesBase64": source, "replacements": replacements}), cancel)?;
    let encoded = result.as_str().ok_or("invalid compressed PDF")?;
    STANDARD.decode(encoded).map_err(|_| "invalid compressed PDF".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eligibility_keeps_masks_and_predictors_untouched() {
        let base = json!({"subtype":"Image","mask":false,"imageMask":false,"hasMask":false,"hasDecode":false,"bits":8,"width":400,"height":300,"size":49152,"components":3,"filter":"FlateDecode","predictor":1});
        assert!(eligible(&base));
        let mut mask = base.clone();
        mask["mask"] = Value::Bool(true);
        assert!(!eligible(&mask));
        let mut predicted = base.clone();
        predicted["predictor"] = Value::from(12);
        assert!(!eligible(&predicted));
        predicted["filter"] = Value::String("DCTDecode".into());
        assert!(eligible(&predicted));
    }
}
