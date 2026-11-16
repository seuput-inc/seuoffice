//! The JSZip behind the agent window's deck check (`templateRedactionOf` of
//! `agent-skills.ts`): `assertZipInflatesWithinLimits(bytes, PPTX_ZIP_LIMITS)`
//! then `JSZip.loadAsync(bytes)`, and `entry.async('string')` of an entry of the
//! archive last opened.

use std::io::{Cursor, Read};
use std::sync::{Mutex, PoisonError};

use main_core::shell::agent::agent_skills::DeckEntry;
use pptx_engine::zip::PPTX_ZIP_LIMITS;
use zip::ZipArchive;

/// The archive of the deck last opened
#[derive(Default)]
pub struct Deck {
    archive: Mutex<Option<ZipArchive<Cursor<Vec<u8>>>>>,
}

impl Deck {
    /// The entries of the archive, in the order of its central directory
    /// (`Object.values(zip.files)`). A deck the zip gate refuses is not read: JSZip does not
    /// return from some of them.
    pub fn open(&self, bytes: &[u8]) -> Result<Vec<DeckEntry>, String> {
        let mut slot = self.archive.lock().unwrap_or_else(PoisonError::into_inner);
        *slot = None;
        ooxml_pkg::assert_zip_inflates_within_limits(bytes, &PPTX_ZIP_LIMITS)?;
        let mut archive =
            ZipArchive::new(Cursor::new(bytes.to_vec())).map_err(|error| error.to_string())?;
        let mut entries = Vec::with_capacity(archive.len());
        for index in 0..archive.len() {
            let entry = archive
                .by_index_raw(index)
                .map_err(|error| error.to_string())?;
            entries.push(DeckEntry {
                name: entry.name().to_owned(),
                dir: entry.is_dir(),
            });
        }
        *slot = Some(archive);
        Ok(entries)
    }

    /// `entry.async('string')` of the entry at `index`: its bytes as UTF-8 text
    pub fn text(&self, index: usize) -> Result<String, String> {
        let mut slot = self.archive.lock().unwrap_or_else(PoisonError::into_inner);
        let archive = slot.as_mut().ok_or("no deck is open")?;
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn deck_bytes(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, body) in parts {
            if name.ends_with('/') {
                writer.add_directory(*name, options).unwrap();
            } else {
                writer.start_file(*name, options).unwrap();
                writer.write_all(body.as_bytes()).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn entries_come_in_archive_order_and_their_text_is_read_by_index() {
        let deck = Deck::default();
        let bytes = deck_bytes(&[
            ("ppt/", ""),
            ("ppt/slides/slide1.xml", "<a>one</a>"),
            ("[Content_Types].xml", "<types/>"),
        ]);
        let entries = deck.open(&bytes).unwrap();
        assert_eq!(
            entries,
            vec![
                DeckEntry {
                    name: "ppt/".into(),
                    dir: true
                },
                DeckEntry {
                    name: "ppt/slides/slide1.xml".into(),
                    dir: false
                },
                DeckEntry {
                    name: "[Content_Types].xml".into(),
                    dir: false
                },
            ]
        );
        assert_eq!(deck.text(1).unwrap(), "<a>one</a>");
        assert_eq!(deck.text(2).unwrap(), "<types/>");
        assert!(deck.text(9).is_err());
    }

    #[test]
    fn a_file_that_is_not_a_zip_is_refused_and_leaves_no_archive() {
        let deck = Deck::default();
        deck.open(&deck_bytes(&[("a.xml", "<a/>")])).unwrap();
        assert!(deck.open(b"not a zip").is_err());
        assert!(deck.text(0).is_err());
    }
}
