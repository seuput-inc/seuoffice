//! The texts the main process shows itself: the editor's one dictionary
//! (`renderer/i18n/app/{en,ko}.ts`, the keys `i18n-main.ts` reads) and the
//! attachment texts of `ai-attachments.ts`.

use main_core::packages::i18n::Lang;

/// `tm(key)` of `i18n-main.ts`
pub fn tm(lang: Lang, key: &str) -> String {
    let (en, ko) = match key {
        "untitled" => ("Untitled Diagram", "제목 없는 다이어그램"),
        "failUnsupported" => (
            "This is not a diagram file that can be opened.",
            "열 수 있는 다이어그램 파일이 아닙니다.",
        ),
        "failNoOriginal" => (
            "This picture has no diagram inside, so it cannot be opened as a diagram.",
            "이 그림에는 다이어그램 원본이 들어 있지 않아 다이어그램으로 열 수 없습니다.",
        ),
        "failCorruptImage" => (
            "The picture file is damaged and cannot be read.",
            "그림 파일이 손상되어 읽을 수 없습니다.",
        ),
        "failNotXml" => (
            "The file is not in the diagram format (XML). It may be damaged.",
            "파일 내용이 다이어그램 형식(XML)이 아닙니다. 손상되었을 수 있습니다.",
        ),
        "failDecompress" => (
            "A compressed page in the file cannot be unpacked. The file may be damaged.",
            "파일의 압축된 쪽을 풀 수 없습니다. 파일이 손상되었을 수 있습니다.",
        ),
        "failNoDiagram" => (
            "The file has no diagram pages.",
            "파일에 다이어그램 쪽이 없습니다.",
        ),
        "failUnreadable" => ("The file cannot be read.", "파일을 읽을 수 없습니다."),
        "failImport" => (
            "This file cannot be imported: {reason}",
            "이 파일을 가져올 수 없습니다: {reason}",
        ),
        "importVisioUnreadable" => (
            "The file is damaged or is not a Visio drawing.",
            "파일이 손상되었거나 Visio 도면이 아닙니다.",
        ),
        "dlgSaveTitle" => ("Save Diagram", "다이어그램 저장"),
        "dlgExportTitle" => ("Export Diagram", "다이어그램 내보내기"),
        "filterDrawio" => ("draw.io diagram (.drawio)", "draw.io 다이어그램 (.drawio)"),
        "filterDio" => ("draw.io diagram (.dio)", "draw.io 다이어그램 (.dio)"),
        "filterDrawioSvg" => (
            "SVG with diagram (.drawio.svg)",
            "다이어그램이 든 SVG (.drawio.svg)",
        ),
        "filterDrawioPng" => (
            "PNG with diagram (.drawio.png)",
            "다이어그램이 든 PNG (.drawio.png)",
        ),
        "filterPng" => ("PNG image", "PNG 그림"),
        "filterSvg" => ("SVG image", "SVG 그림"),
        "filterPdf" => ("PDF document", "PDF 문서"),
        "closeUnsavedMsg" => (
            "This diagram has unsaved changes.",
            "이 다이어그램에 저장하지 않은 변경 사항이 있습니다.",
        ),
        "closeUnsavedDetail" => (
            "Do you want to save them before closing?",
            "닫기 전에 저장하시겠습니까?",
        ),
        "btnSave" => ("Save", "저장"),
        "btnDontSave" => ("Don't Save", "저장 안 함"),
        "btnCancel" => ("Cancel", "취소"),
        "btnReplace" => ("Replace", "바꾸기"),
        "exportReplaceDetail" => ("Do you want to replace them?", "바꾸시겠습니까?"),
        _ => return key.to_owned(),
    };
    match lang {
        Lang::En => en,
        Lang::Ko => ko,
    }
    .to_owned()
}

pub fn open_refusal(lang: Lang, reason: &str) -> Result<String, String> {
    let key = match reason {
        "unsupported" => "failUnsupported",
        "no_original" => "failNoOriginal",
        "corrupt_image" => "failCorruptImage",
        "not_xml" => "failNotXml",
        "decompress_failed" => "failDecompress",
        "no_diagram" => "failNoDiagram",
        "unreadable" => "failUnreadable",
        "visio_unreadable" => {
            return Ok(
                tm(lang, "failImport").replace("{reason}", &tm(lang, "importVisioUnreadable"))
            );
        }
        _ => return Err(format!("Unknown diagram open failure: {reason}")),
    };
    Ok(tm(lang, key))
}

#[cfg(test)]
mod open_refusal_tests {
    use super::*;

    #[test]
    fn translates_original_reasons_and_visio_import_message() {
        for reason in [
            "unsupported",
            "no_original",
            "corrupt_image",
            "not_xml",
            "decompress_failed",
            "no_diagram",
            "unreadable",
        ] {
            assert_ne!(
                open_refusal(Lang::En, reason).unwrap(),
                open_refusal(Lang::Ko, reason).unwrap()
            );
        }
        assert_eq!(
            open_refusal(Lang::En, "not_xml").unwrap(),
            "The file is not in the diagram format (XML). It may be damaged."
        );
        assert_eq!(
            open_refusal(Lang::Ko, "visio_unreadable").unwrap(),
            "이 파일을 가져올 수 없습니다: 파일이 손상되었거나 Visio 도면이 아닙니다."
        );
        assert!(open_refusal(Lang::En, "unknown").is_err());
    }
}

/// `tm('exportReplaceMsg', { count })`
pub fn export_replace_message(lang: Lang, count: usize) -> String {
    match lang {
        Lang::En => format!("Files with these names that already exist: {count}."),
        Lang::Ko => format!("같은 이름의 파일이 {count}개 있습니다."),
    }
}

/// The keys of `ATTACH_STRINGS`
#[derive(Clone, Copy)]
pub enum Attach {
    DlgAddAttachment,
    FilterSupported,
    FilterAll,
    ErrNotFile,
    ErrImageTooLarge,
    ErrUnreadable,
    ErrFileTooLarge,
    ErrParseFailed,
    ErrImageNoText,
    ErrNotImage,
}

/// `tm(key)` of `ai-attachments.ts`
pub fn attach(lang: Lang, key: Attach) -> &'static str {
    let (en, ko) = match key {
        Attach::DlgAddAttachment => ("Add Attachments", "첨부 파일 추가"),
        Attach::FilterSupported => ("Supported Files", "지원되는 파일"),
        Attach::FilterAll => ("All Files", "모든 파일"),
        Attach::ErrNotFile => ("not a file", "파일이 아닙니다"),
        Attach::ErrImageTooLarge => (
            "image exceeds the 5MB limit",
            "이미지가 5MB 제한을 초과했습니다",
        ),
        Attach::ErrUnreadable => ("cannot be read", "읽을 수 없습니다"),
        Attach::ErrFileTooLarge => (
            "File exceeds the size limit",
            "파일이 크기 제한을 초과했습니다",
        ),
        Attach::ErrParseFailed => ("Failed to parse file", "파일을 분석하지 못했습니다"),
        Attach::ErrImageNoText => (
            "Image attachments have no text; the image is sent along with the user message",
            "이미지 첨부 파일은 텍스트를 제공하지 않으며, 이미지 형태로 사용자 메시지와 함께 전송되므로 이미지를 직접 확인하면 됩니다",
        ),
        Attach::ErrNotImage => (
            "not a supported image type",
            "지원되지 않는 이미지 형식입니다",
        ),
    };
    match lang {
        Lang::En => en,
        Lang::Ko => ko,
    }
}

/// `tm('errUnsupportedExt', { ext })`
pub fn unsupported_ext(lang: Lang, ext: &str) -> String {
    match lang {
        Lang::En => format!(".{ext} files are not supported"),
        Lang::Ko => format!(".{ext} 형식은 지원되지 않습니다"),
    }
}

/// `tm('errTooLarge', { mb })`
pub fn too_large(lang: Lang, mb: f64) -> String {
    let mb = jscompat::number_to_string(mb);
    match lang {
        Lang::En => format!("exceeds the {mb}MB limit"),
        Lang::Ko => format!("{mb}MB 제한을 초과했습니다"),
    }
}
