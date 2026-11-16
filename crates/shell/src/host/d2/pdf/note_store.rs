//! Adapter between the editor's note-link model and the structure result store.

use jscompat::JsMap;
use main_core::packages::pdf_structure::files::SystemEnv;
use main_core::packages::pdf_structure::note_links::MatchedBy as StoreMatch;
use main_core::packages::pdf_structure::store::{ResultStore, ResultStoreOptions};
use main_core::pdf::note_link::{FoundNoteLink, MatchedBy, NoteLink, NoteLinks};
use sha2::{Digest, Sha256};
use std::io::Read;

fn with_store<T>(
    user_data: &str,
    f: impl FnOnce(&mut ResultStore) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = crate::host::d2::RESULT_STORE_LOCK
        .lock()
        .map_err(|e| e.to_string())?;
    let mut store = ResultStore::new(
        user_data,
        ResultStoreOptions::default(),
        Box::new(SystemEnv),
    )
    .map_err(|e| e.message().to_owned())?;
    f(&mut store)
}

pub(super) fn read_note_links(user_data: &str) -> Result<NoteLinks, String> {
    with_store(user_data, |store| {
        let stored = store.read_note_links();
        let mut links = JsMap::new();
        for (key, link) in stored.links.entries() {
            links.set(
                key.to_owned(),
                NoteLink {
                    note_path: link.note_path.clone(),
                    doc_hash: link.doc_hash.clone(),
                },
            );
        }
        Ok(NoteLinks { links })
    })
}

pub(super) fn find_note_link(
    user_data: &str,
    pdf_path: &str,
    doc_hash: &str,
) -> Result<Option<FoundNoteLink>, String> {
    with_store(user_data, |store| {
        Ok(store
            .find_note_link(pdf_path, doc_hash)
            .map(|found| FoundNoteLink {
                note_path: found.link.note_path,
                matched_by: match found.matched_by {
                    StoreMatch::Path => MatchedBy::Path,
                    StoreMatch::DocHash => MatchedBy::DocHash,
                },
            }))
    })
}

pub(super) fn set_note_link(
    user_data: &str,
    pdf_path: &str,
    link: &NoteLink,
) -> Result<(), String> {
    with_store(user_data, |store| {
        store
            .set_note_link(pdf_path, &link.note_path, &link.doc_hash)
            .map_err(|e| e.message().to_owned())
    })
}

pub(super) fn update_note_link_doc_hash(
    user_data: &str,
    pdf_path: &str,
    doc_hash: &str,
) -> Result<bool, String> {
    with_store(user_data, |store| {
        store
            .update_note_link_doc_hash(pdf_path, doc_hash)
            .map_err(|e| e.message().to_owned())
    })
}

pub(super) fn update_note_link_doc_hash_guarded(
    user_data: &str,
    pdf_path: &str,
    doc_hash: &str,
    admit: &mut main_core::packages::pdf_structure::files::StoreAdmission<'_>,
) -> Result<bool, String> {
    with_store(user_data, |store| {
        store
            .update_note_link_doc_hash_guarded(pdf_path, doc_hash, admit)
            .map_err(|e| e.message().to_owned())
    })
}

pub(super) fn document_hash_of_file(path: &str) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn note_links_survive_save_and_hash_changes() {
        let root = std::env::temp_dir().join(format!(
            "pdf-note-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let note = root.join("note.md");
        let pdf = root.join("file.pdf");
        std::fs::write(&note, "note").unwrap();
        std::fs::write(&pdf, b"abc").unwrap();
        let root = root.to_str().unwrap();
        let pdf = pdf.to_str().unwrap();
        let link = NoteLink {
            note_path: note.to_string_lossy().into_owned(),
            doc_hash: document_hash_of_file(pdf).unwrap(),
        };
        assert_eq!(
            link.doc_hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        set_note_link(root, pdf, &link).unwrap();
        assert_eq!(
            find_note_link(root, pdf, &link.doc_hash)
                .unwrap()
                .unwrap()
                .matched_by,
            MatchedBy::Path
        );
        assert!(update_note_link_doc_hash(root, pdf, "newhash").unwrap());
        let found = read_note_links(root).unwrap();
        assert_eq!(found.links.entries()[0].1.doc_hash, "newhash");
        std::fs::remove_dir_all(root).unwrap();
    }
}
