use odt_engine::core::{OdtError, OdtOpenErrorCode, ReadOdtPackageOptions, read_odt_package};
use serde_json::{Value, json};

/// Hosted result consumed by `file_parse::parse::parse_file_to_text`.
pub(crate) fn read_odt(bytes: &[u8]) -> Value {
    match odt_engine::assemble::odt_to_text(bytes) {
        Ok(text) => json!({ "text": text }),
        Err(OdtError::Open { code, message }) => {
            let mut error =
                json!({ "name": "OdtOpenError", "code": code.as_str(), "message": message });
            if code == OdtOpenErrorCode::NotOdtText
                && let Ok(package) = read_odt_package(bytes, &ReadOdtPackageOptions::default())
                && let Some(media_type) = package.mimetype
            {
                error["mediaType"] = Value::String(media_type);
            }
            json!({ "error": error })
        }
        Err(OdtError::Thrown(message)) => {
            json!({ "error": { "name": "Error", "message": message } })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use odt_engine::assemble::zip::{ZipEntryInput, write_zip};
    use odt_engine::core::HostNow;
    use std::borrow::Cow;

    fn package(media_type: &str, content: &str) -> Vec<u8> {
        let entries = [
            ZipEntryInput::Data {
                name: "mimetype".into(),
                data: Cow::Borrowed(media_type.as_bytes()),
                store: true,
                dos_date_time: None,
            },
            ZipEntryInput::Data {
                name: "content.xml".into(),
                data: Cow::Borrowed(content.as_bytes()),
                store: false,
                dos_date_time: None,
            },
        ];
        write_zip(
            &entries,
            HostNow {
                ms: 0.0,
                offset_minutes: 0.0,
            },
        )
        .unwrap()
    }

    #[test]
    fn extracts_paragraph_and_excludes_deleted_text() {
        let bytes = package(
            "application/vnd.oasis.opendocument.text",
            r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:text><text:p>Hello world</text:p><text:tracked-changes><text:p>secret</text:p></text:tracked-changes></office:text></office:body></office:document-content>"#,
        );
        assert_eq!(read_odt(&bytes)["text"], "Hello world");
    }

    #[test]
    fn reports_actual_non_text_media_type() {
        let bytes = package("application/vnd.oasis.opendocument.spreadsheet", "");
        let result = read_odt(&bytes);
        assert_eq!(result["error"]["code"], "not-odt-text");
        assert_eq!(
            result["error"]["mediaType"],
            "application/vnd.oasis.opendocument.spreadsheet"
        );
    }

    #[test]
    fn rejects_corrupt_archive() {
        assert_eq!(read_odt(b"PK not a zip")["error"]["code"], "corrupt");
    }
}
