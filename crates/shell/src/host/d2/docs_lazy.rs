//! A single owner for the non-Send lazy media registry.

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};

use main_core::docs::byte_handoff::{ByteHandoffs, MediaOwner, MediaPermit};
use main_core::docs::doc_format::Materialized;
use main_core::docs::lazy_media::{LazyMedia, LazyMediaBody, OpenedLazyDocx};

use super::docs_lazy_zip::Library;

struct SavedBytes(shell_contract::ipc::NativeSavedInput);
impl main_core::docs::lazy_media::RetainedZipBytes for SavedBytes {
    fn bytes(&self) -> &[u8] { self.0.bytes() }
}

struct DecryptedBytes(std::sync::Arc<Vec<u8>>);
impl main_core::docs::lazy_media::RetainedZipBytes for DecryptedBytes {
    fn bytes(&self) -> &[u8] { &self.0 }
}

type Command = Box<dyn FnOnce(&mut HashMap<MediaOwner, LazyMedia>, &mut Library, &mut ByteHandoffs<Vec<u8>>) + Send>;

pub(super) struct LazyHost {
    commands: Sender<Command>,
}

impl LazyHost {
    pub(super) fn new() -> Result<Self, String> {
        let (send, receive) = mpsc::channel::<Command>();
        std::thread::Builder::new().name("docs-lazy-media".into()).spawn(move || {
            let mut media = HashMap::new();
            let mut library = Library::default();
            let mut handoffs = ByteHandoffs::new();
            loop {
                handoffs.expire(main_core::packages::pdf_structure::files::system_now_ms());
                match receive.recv_timeout(std::time::Duration::from_secs(1)) {
                    Ok(command) => command(&mut media, &mut library, &mut handoffs),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }).map_err(|error| error.to_string())?;
        Ok(Self { commands: send })
    }

    fn call<T: Send + 'static>(
        &self,
        run: impl FnOnce(&mut HashMap<MediaOwner, LazyMedia>, &mut Library, &mut ByteHandoffs<Vec<u8>>) -> T + Send + 'static,
    ) -> Result<T, String> {
        let (send, receive) = mpsc::sync_channel(1);
        self.commands.send(Box::new(move |media, library, handoffs| {
            let _ = send.send(run(media, library, handoffs));
        })).map_err(|_| "The lazy-media worker has stopped".to_owned())?;
        receive.recv().map_err(|_| "The lazy-media worker failed".to_owned())
    }

    fn session<T: Send + 'static>(
        &self, permit: MediaPermit,
        run: impl FnOnce(&mut LazyMedia, &mut Library, &mut ByteHandoffs<Vec<u8>>, MediaOwner) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        if !permit.is_active() { return Err("The document media session is closed.".to_owned()); }
        self.call(move |sessions, library, handoffs| {
            let owner = permit.owner();
            if !permit.is_active() { return Err("The document media session is closed.".to_owned()); }
            let observed = permit.clone();
            library.set_cancelled(Some(std::sync::Arc::new(move || !observed.is_active())));
            let result = run(sessions.entry(owner).or_default(), library, handoffs, owner);
            library.set_cancelled(None);
            if !permit.is_active() {
                sessions.remove(&owner);
                handoffs.revoke_owner(owner);
                return Err("The document media session is closed.".to_owned());
            }
            result
        })?
    }
    fn session_cancelled<T: Send + 'static>(
        &self, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken,
        run: impl FnOnce(&mut LazyMedia, &mut Library, &mut ByteHandoffs<Vec<u8>>, MediaOwner) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let observed = permit.clone();
        self.session(permit, move |media, library, handoffs, owner| {
            let cancelled = std::sync::Arc::new(move || cancel.is_cancelled() || !observed.is_active());
            if cancelled() { return Err("Document operation cancelled".into()); }
            library.set_cancelled(Some(cancelled.clone()));
            let result = run(media, library, handoffs, owner);
            library.set_cancelled(None);
            if cancelled() { return Err("Document operation cancelled".into()); }
            result
        })
    }


    pub(super) fn open(&self, path: &str, file: Option<std::sync::Arc<std::fs::File>>, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<Option<OpenedLazyDocx>, String> {
        let path = path.to_owned();
        self.session_cancelled(permit, cancel, move |media, library, _, owner| media.open_lazy_docx(library, &path, file, f64::from(owner.view)).map_err(|error| error.message))
    }

    pub(super) fn open_saved(&self, path: &str, input: shell_contract::ipc::NativeSavedInput, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<Option<OpenedLazyDocx>, String> {
        let path = path.to_owned();
        let source: std::sync::Arc<dyn main_core::docs::lazy_media::RetainedZipBytes> = std::sync::Arc::new(SavedBytes(input));
        self.session_cancelled(permit, cancel, move |media, library, _, owner|
            media.open_lazy_retained(library, &path, source, f64::from(owner.view)).map_err(|error| error.message))
    }

    pub(super) fn adopt_saved(&self, bytes: &[u8], path: &str, input: shell_contract::ipc::NativeSavedInput, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<(), String> {
        let bytes = bytes.to_vec();
        let path = path.to_owned();
        let source: std::sync::Arc<dyn main_core::docs::lazy_media::RetainedZipBytes> = std::sync::Arc::new(SavedBytes(input));
        self.session_cancelled(permit, cancel, move |media, library, _, owner|
            media.adopt_lazy_retained(library, &bytes, &path, source, f64::from(owner.view)).map_err(|error| error.message))
    }

    pub(super) fn adopt_decrypted(&self, bytes: &[u8], path: &str, plain: std::sync::Arc<Vec<u8>>, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<(), String> {
        let bytes = bytes.to_vec();
        let path = path.to_owned();
        let source: std::sync::Arc<dyn main_core::docs::lazy_media::RetainedZipBytes> = std::sync::Arc::new(DecryptedBytes(plain));
        self.session_cancelled(permit, cancel, move |media, library, _, owner|
            media.adopt_lazy_retained(library, &bytes, &path, source, f64::from(owner.view)).map_err(|error| error.message))
    }

    pub(super) fn adopt(&self, bytes: &[u8], path: &str, file: Option<std::sync::Arc<std::fs::File>>, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<(), String> {
        let bytes = bytes.to_vec();
        let path = path.to_owned();
        self.session_cancelled(permit, cancel, move |media, library, _, owner| media.adopt_lazy_media_hashes(library, &bytes, &path, file, f64::from(owner.view)).map_err(|error| error.message))
    }

    pub(super) fn materialize(&self, bytes: Vec<u8>, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<Materialized, String> {
        self.session_cancelled(permit, cancel, move |media, library, _, _| media.materialize_lazy_docx(library, &bytes)
            .map(|result| Materialized { bytes: result.bytes, hashes: result.hashes })
            .map_err(|error| error.message))
    }

    pub(super) fn point(&self, hashes: &[String], path: &str, permit: MediaPermit, plain: Option<Vec<u8>>) -> Result<(), String> {
        let hashes = hashes.to_vec();
        let path = path.to_owned();
        if !permit.is_active() { return Err("The document media session is closed.".to_owned()); }
        self.commands.send(Box::new(move |sessions, _, handoffs| {
            let owner = permit.owner();
            if !permit.is_active() { return; }
            sessions.entry(owner).or_default().point_lazy_media_at(&hashes, &path, f64::from(owner.view), plain.as_deref());
            if !permit.is_active() { sessions.remove(&owner); handoffs.revoke_owner(owner); }
        })).map_err(|_| "The lazy-media worker has stopped".to_owned())
    }

    pub(super) fn moved(&self, old: &str, new: &str) -> Result<(), String> {
        let old = old.to_owned();
        let new = new.to_owned();
        self.commands.send(Box::new(move |sessions, _, _| {
            for media in sessions.values_mut() { media.move_lazy_media_source(&old, &new); }
        })).map_err(|_| "The lazy-media worker has stopped".to_owned())
    }

    pub(super) fn forget(&self, permit: MediaPermit) -> Result<(), String> {
        permit.revoke();
        let owner = permit.owner();
        self.commands.send(Box::new(move |sessions, _, handoffs| {
            sessions.remove(&owner);
            handoffs.revoke_owner(owner);
        })).map_err(|_| "The lazy-media worker has stopped".to_owned())
    }

    pub(super) fn read(&self, url: &str, permit: MediaPermit, cancel: shell_contract::runtime::CancelToken) -> Result<Option<LazyMediaBody>, String> {
        let url = url.to_owned();
        self.session_cancelled(permit, cancel, move |media, library, _, owner| media.read_lazy_media_for(library, &url, owner).map_err(|error| error.message))
    }

    pub(super) fn handoff(&self, bytes: Vec<u8>, permit: MediaPermit) -> Result<String, String> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|error| error.to_string())?;
        random[6] = (random[6] & 0x0f) | 0x40;
        random[8] = (random[8] & 0x3f) | 0x80;
        let hex: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let token = format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
        self.session(permit, move |_, _, handoffs, owner| handoffs.hand_off_bytes_for(bytes, token,
            main_core::packages::pdf_structure::files::system_now_ms(), owner))
    }

    pub(super) fn response_for(&self, permit: MediaPermit, url: &str, fonts_dir: &str, cancel: shell_contract::runtime::CancelToken) -> Result<main_core::docs::lazy_media::MediaAnswer, String> {
        let url = url.to_owned();
        let fonts_dir = fonts_dir.to_owned();
        let check = permit.clone();
        self.session_cancelled(permit, cancel, move |media, library, handoffs, _| {
            media.answer_for(library, handoffs, &url, &fonts_dir, &check).map_err(|error| error.message)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    #[test]
    fn lazy_media_survives_materialization_and_source_move() {
        let dir = std::env::temp_dir().join(format!("docs-lazy-{}", jscompat::random_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.docx");
        let second = dir.join("second.docx");
        let image = vec![42u8; 1024 * 1024];
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("word/document.xml", options).unwrap();
        zip.write_all(b"<document/>").unwrap();
        zip.start_file("word/media/image.png", options).unwrap();
        zip.write_all(&image).unwrap();
        std::fs::write(&first, zip.finish().unwrap().into_inner()).unwrap();

        let host = LazyHost::new().unwrap();
        let permit = MediaPermit::mint(7).unwrap();
        let other = MediaPermit::mint(8).unwrap();
        let opened = host.open(first.to_str().unwrap(), None, permit.clone(), shell_contract::runtime::CancelToken::default()).unwrap().unwrap();
        let url = docx_engine::lazy_media::lazy_media_url(&opened.hash, "word/media/image.png");
        let cancelled = shell_contract::runtime::CancelToken::default();
        cancelled.cancel();
        assert!(host.read(&url, permit.clone(), cancelled).is_err());
        assert_eq!(host.read(&url, permit.clone(), shell_contract::runtime::CancelToken::default()).unwrap().unwrap().body, image);
        assert!(host.read(&url, other, shell_contract::runtime::CancelToken::default()).unwrap().is_none());
        let full = host.materialize(opened.bytes.clone(), permit.clone(), shell_contract::runtime::CancelToken::default()).unwrap();
        assert_eq!(full.hashes, vec![opened.hash.clone()]);
        assert!(full.bytes.len() > opened.bytes.len());
        std::fs::rename(&first, &second).unwrap();
        host.moved(first.to_str().unwrap(), second.to_str().unwrap()).unwrap();
        assert_eq!(host.read(&url, permit.clone(), shell_contract::runtime::CancelToken::default()).unwrap().unwrap().body, image);
        host.forget(permit.clone()).unwrap();
        assert!(host.read(&url, permit, shell_contract::runtime::CancelToken::default()).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn retained_archive_survives_path_replacement_without_moving_cursor() {
        use std::io::{Seek, SeekFrom};
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("docs-pinned-{}", jscompat::random_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.docx");
        let image = vec![37u8; 1024 * 1024];
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("word/document.xml", options).unwrap();
        zip.write_all(b"<document/>").unwrap();
        zip.start_file("word/media/image.png", options).unwrap();
        zip.write_all(&image).unwrap();
        std::fs::write(&path, zip.finish().unwrap().into_inner()).unwrap();
        let file = Arc::new(std::fs::File::open(&path).unwrap());
        (&*file).seek(SeekFrom::Start(11)).unwrap();
        let host = LazyHost::new().unwrap();
        let permit = MediaPermit::mint(9).unwrap();
        let opened = host.open(path.to_str().unwrap(), Some(file.clone()), permit.clone(), shell_contract::runtime::CancelToken::default()).unwrap().unwrap();
        std::fs::rename(&path, dir.join("old.docx")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        host.adopt(&opened.bytes, path.to_str().unwrap(), Some(file.clone()), permit.clone(), shell_contract::runtime::CancelToken::default()).unwrap();
        let url = docx_engine::lazy_media::lazy_media_url(&opened.hash, "word/media/image.png");
        assert_eq!(host.read(&url, permit.clone(), shell_contract::runtime::CancelToken::default()).unwrap().unwrap().body, image);
        assert_eq!((&*file).stream_position().unwrap(), 11);
        let materialized = host.materialize(opened.bytes, permit.clone(), shell_contract::runtime::CancelToken::default()).unwrap();
        assert!(materialized.bytes.len() > image.len());
        host.forget(permit).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn handoff_is_delivered_once() {
        let host = LazyHost::new().unwrap();
        let permit = MediaPermit::mint(7).unwrap();
        let other = MediaPermit::mint(8).unwrap();
        let url = host.handoff(b"document".to_vec(), permit.clone()).unwrap();
        assert_eq!(host.response_for(other, &url, "", shell_contract::runtime::CancelToken::default()).unwrap().status, 404);
        let first = host.response_for(permit.clone(), &url, "", shell_contract::runtime::CancelToken::default()).unwrap();
        assert_eq!(first.status, 200);
        assert_eq!(first.body, b"document");
        assert_eq!(host.response_for(permit, &url, "", shell_contract::runtime::CancelToken::default()).unwrap().status, 404);
    }

    #[test]
    fn revoked_inflight_session_cannot_publish_a_handoff() {
        let host = std::sync::Arc::new(LazyHost::new().unwrap());
        let permit = MediaPermit::mint(7).unwrap();
        let owner = permit.owner();
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker_host = host.clone();
        let worker_permit = permit.clone();
        let worker = std::thread::spawn(move || worker_host.session(worker_permit, move |_, _, handoffs, owner| {
            started_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            handoffs.hand_off_bytes_for(b"secret".to_vec(), "inflight".into(), main_core::packages::pdf_structure::files::system_now_ms(), owner)
        }));
        started_rx.recv().unwrap();
        permit.revoke();
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        let url = format!("{}://handoff/inflight", main_core::docs::lazy_media_inline::LAZY_MEDIA_SCHEME);
        assert!(host.call(move |_, _, handoffs| handoffs.take_handoff_for(&url, owner)).unwrap().is_none());
        assert!(host.handoff(b"late".to_vec(), permit).is_err());
    }
}
