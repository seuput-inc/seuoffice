//! The mind map attachment reader over the shell's shared file parser.

use std::path::Path;

use main_core::packages::file_parse::{attachable, diagram};
use shell_ipc_d1::mindmap::{FileParse, ParsedText};

pub(super) struct Parser;

impl FileParse for Parser {
    fn attachable_exts(&self) -> Vec<String> {
        attachable::attachable_exts().into_iter().map(str::to_owned).collect()
    }

    fn attachable_image_exts(&self) -> Vec<String> {
        attachable::ATTACHABLE_IMAGE_EXTS.iter().map(|ext| (*ext).to_owned()).collect()
    }

    fn attachable_max_bytes_default(&self) -> u64 {
        attachable::ATTACHABLE_MAX_BYTES as u64
    }

    fn attachable_max_bytes(&self, ext: &str) -> u64 {
        attachable::attachable_max_bytes(ext) as u64
    }

    fn attachable_image_max_bytes(&self) -> u64 {
        attachable::ATTACHABLE_IMAGE_MAX_BYTES as u64
    }

    fn diagram_picture_ext_of(&self, path: &str) -> Option<String> {
        super::super::w_files::diagram_picture_ext(Path::new(path)).map(str::to_owned)
    }

    fn diagram_picture_ext_of_name(&self, path: &str) -> Option<String> {
        diagram::diagram_picture_ext_of_name(path).map(str::to_owned)
    }

    fn parse_file_to_text(&self, path: &str) -> ParsedText {
        match super::super::w_files::parsed_value(path) {
            Ok(value) => match value.get("ok").and_then(|v| v.as_bool()) {
                Some(ok) => ParsedText {
                    ok,
                    kind: value.get("kind").and_then(|v| v.as_str()).map(str::to_owned),
                    text: value.get("text").and_then(|v| v.as_str()).map(str::to_owned),
                    damaged: value.get("damaged").and_then(|v| v.as_bool()).unwrap_or(false),
                    error: value.get("error").and_then(|v| v.as_str()).map(str::to_owned),
                },
                None => ParsedText { error: Some("file parser returned no ok flag".into()), ..ParsedText::default() },
            },
            Err(error) => ParsedText { error: Some(error), ..ParsedText::default() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_limits_preserve_unlimited_document_formats() {
        let parser = Parser;
        assert_eq!(parser.attachable_max_bytes(".PNG"), 5 * 1024 * 1024);
        assert_eq!(parser.attachable_max_bytes("txt"), 50 * 1024 * 1024);
        assert_eq!(parser.attachable_max_bytes("docx"), u64::MAX);
        assert_eq!(parser.diagram_picture_ext_of_name("sample.DRAWIO.SVG").as_deref(), Some("drawio.svg"));
    }
}
