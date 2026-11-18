//! EPUB book opening and entry reads through the Rust engine.

use epub_engine::open::{EPUB_ZIP_LIMITS, EpubOpenResult, open_epub};
use epub_engine::translate::keys::epub_document_hash;
use shell_ipc_d2::epub::hosts::{Bytes, DecodedText, OpenedBook};

pub(super) fn open(bytes: Bytes) -> Result<OpenedBook, String> {
    let book = match open_epub(bytes.as_slice(), &EPUB_ZIP_LIMITS) {
        EpubOpenResult::Opened(book) => book,
        EpubOpenResult::Refused { reason, .. } => return Err(reason.as_str().to_owned()),
    };
    let info = &book.info;
    let summary = serde_json::json!({
        "opfPath": info.opf_path,
        "version": info.version,
        "metadata": info.metadata,
        "manifest": info.manifest,
        "spine": info.spine.items,
        "pageProgressionDirection": info.spine.page_progression_direction,
        "toc": info.toc,
        "fixedLayout": info.fixed_layout,
        "verticalWriting": info.vertical_writing,
    });
    Ok(OpenedBook { summary, doc_hash: epub_document_hash(bytes.as_slice()) })
}

pub(super) fn read_text(bytes: Bytes, path: &str) -> Option<DecodedText> {
    let EpubOpenResult::Opened(book) = open_epub(bytes.as_slice(), &EPUB_ZIP_LIMITS) else {
        return None;
    };
    book.read_text(path).ok().flatten().map(|decoded| DecodedText {
        text: decoded.text,
        valid: decoded.valid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn damaged_archive_is_refused_and_has_no_readable_entry() {
        let bytes = Arc::new(b"not a zip".to_vec());
        assert!(open(bytes.clone()).is_err());
        assert!(read_text(bytes, "chapter.xhtml").is_none());
    }
}
