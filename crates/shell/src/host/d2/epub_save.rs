//! EPUB archive save through the Rust engine.

use epub_engine::open::EPUB_ZIP_LIMITS;
use epub_engine::ranges::RangeReplacement;
use epub_engine::save::{EpubSaveOptions, EpubSaveResult, SaveOriginal, save_epub};
use shell_ipc_d2::epub::hosts::{Bytes, Replacement, SavedBytes};

pub(super) fn save(
    bytes: Bytes,
    edits: &[(String, Vec<Replacement>)],
    now: f64,
) -> Result<SavedBytes, String> {
    let edits: Vec<_> = edits
        .iter()
        .map(|(path, replacements)| {
            (
                path.clone(),
                replacements
                    .iter()
                    .map(|replacement| RangeReplacement {
                        start: replacement.start as f64,
                        end: replacement.end as f64,
                        text: replacement.text.encode_utf16().collect(),
                    })
                    .collect(),
            )
        })
        .collect();
    let options = EpubSaveOptions {
        limits: &EPUB_ZIP_LIMITS,
        now,
    };
    match save_epub(SaveOriginal::Bytes(bytes.as_slice()), &edits, &options)
        .map_err(|error| error.message)?
    {
        EpubSaveResult::Unchanged => Ok(SavedBytes::Unchanged),
        EpubSaveResult::Saved { bytes, .. } => Ok(SavedBytes::Changed(bytes)),
        EpubSaveResult::Refused { reason, detail, .. } => Ok(SavedBytes::Refused {
            reason: reason.as_str().to_owned(),
            detail,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::sync::Arc;

    fn book() -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (path, body) in [
            ("mimetype", "application/epub+zip"),
            (
                "META-INF/container.xml",
                r#"<?xml version="1.0"?><container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="book.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            ),
            (
                "book.opf",
                r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Book</dc:title><dc:identifier id="id">book</dc:identifier><dc:language>en</dc:language></metadata><manifest><item id="c" href="chapter.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c"/></spine></package>"#,
            ),
            (
                "chapter.xhtml",
                r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p>Hello</p></body></html>"#,
            ),
        ] {
            zip.start_file(path, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn chapter_edit_survives_save() {
        let original = Arc::new(book());
        let chapter = super::super::epub_book::read_text(original.clone(), "chapter.xhtml")
            .unwrap()
            .text;
        let start = chapter.find("Hello").unwrap();
        let edits = vec![(
            "chapter.xhtml".to_owned(),
            vec![Replacement {
                start,
                end: start + 5,
                text: "World".to_owned(),
            }],
        )];
        let SavedBytes::Changed(saved) = save(original, &edits, 0.0).unwrap() else {
            panic!("chapter edit was not saved");
        };
        let saved = super::super::epub_book::read_text(Arc::new(saved), "chapter.xhtml").unwrap();
        assert!(saved.text.contains("<p>World</p>"));
    }

    #[test]
    fn damaged_book_is_refused_without_output_bytes() {
        let saved = save(Arc::new(b"not a zip".to_vec()), &[], 0.0).unwrap();
        assert!(matches!(saved, SavedBytes::Refused { .. }));
    }
}
