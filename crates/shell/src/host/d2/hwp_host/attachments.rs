use serde_json::{Value, json};
use shell_ipc_d2::hwp::{ParsedFile, ParsedFileKind};

pub(super) fn parse(path: &str) -> ParsedFile {
    match crate::host::w_files::parsed_value(path).and_then(convert) {
        Ok(parsed) => parsed,
        Err(error) => ParsedFile {
            ok: false,
            kind: ParsedFileKind::Text,
            text: None,
            error: Some(error),
            damaged: false,
        },
    }
}

fn convert(value: Value) -> Result<ParsedFile, String> {
    let string = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let kind = match value.get("kind").and_then(Value::as_str) {
        Some("text") => ParsedFileKind::Text,
        Some("image") => ParsedFileKind::Image,
        Some("unsupported") => ParsedFileKind::Unsupported,
        _ => return Err("file parser returned an invalid kind".into()),
    };
    Ok(ParsedFile {
        ok: value
            .get("ok")
            .and_then(Value::as_bool)
            .ok_or("file parser returned no ok flag")?,
        kind,
        text: string("text"),
        error: string("error"),
        damaged: value.get("damaged").and_then(Value::as_bool) == Some(true),
    })
}

pub(super) fn diagram_picture_has_no_original(file_name: String, bytes: Vec<u8>) -> bool {
    seu_host::call(
        "fileparse.readDiagramText",
        &json!({
            "fileName": file_name,
            "bytesBase64": jscompat::base64(&bytes),
        }),
    )
    .ok()
    .and_then(|answer| {
        answer
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
    .as_deref()
        == Some("no_original")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_failure_keeps_damage_and_message() {
        let result =
            convert(json!({"ok": false, "kind": "text", "error": "damaged", "damaged": true}))
                .unwrap();
        assert!(!result.ok);
        assert!(result.damaged);
        assert_eq!(result.error.as_deref(), Some("damaged"));
    }

    #[test]
    fn malformed_host_result_is_an_error() {
        assert!(convert(json!({"ok": true})).is_err());
        assert!(convert(json!({"kind": "text"})).is_err());
    }
}
