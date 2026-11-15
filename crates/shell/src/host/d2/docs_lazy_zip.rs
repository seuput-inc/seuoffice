//! Adapter between the document lazy-media state and the DOCX ZIP splicer.

use std::fs::File;
use std::rc::Rc;
use std::sync::Arc;

fn check(cancelled: &Option<Arc<dyn Fn() -> bool + Send + Sync>>) -> Result<(), String> {
    if cancelled.as_ref().is_some_and(|cancelled| cancelled()) { Err("Document operation cancelled".into()) } else { Ok(()) }
}

use docx_engine::save::parts::zip_splice::{self, ByteSource, ZipArchive};
use jscompat::JsMap;
use main_core::docs::lazy_media::{RetainedZipBytes, SharedZip, ZipEntry, ZipFile, ZipLibrary, ZipRead, ZipSourceOf};
use main_core::packages::electron_utils::print_window::JsError;

#[derive(Clone)]
struct FileSource { file: Arc<File>, size: u64, cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>> }
impl ByteSource for FileSource {
    fn size(&self) -> u64 { self.size }
    fn check_cancelled(&self) -> Result<(), String> {
        if self.cancelled.as_ref().is_some_and(|cancelled| cancelled()) { Err("Document operation cancelled".into()) } else { Ok(()) }
    }
    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        self.check_cancelled()?;
        let size = usize::try_from(length).map_err(|_| "ZIP range is too large".to_owned())?;
        let mut bytes = vec![0; size];
        let mut done = 0usize;
        while done < size {
            self.check_cancelled()?;
            let position = offset.checked_add(done as u64).ok_or("ZIP offset overflow")?;
            let end = done.saturating_add(64 * 1024).min(size);
            let read = main_core::docs::lazy_media::read_file_at(&self.file, position, &mut bytes[done..end])
                .map_err(|error| error.to_string())?;
            if read == 0 { break; }
            done += read;
            self.check_cancelled()?;
        }
        bytes.truncate(done);
        Ok(bytes)
    }
}

impl ZipRead for FileSource {
    fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, JsError> {
        ByteSource::read(self, offset, length).map_err(JsError::plain)
    }
}

#[derive(Clone)]
enum OpenSource {
    File(FileSource),
    Retained { source: Arc<dyn RetainedZipBytes>, cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>> },
}

impl ByteSource for OpenSource {
    fn size(&self) -> u64 {
        match self { Self::File(file) => file.size(), Self::Retained { source, .. } => source.bytes().len() as u64 }
    }
    fn check_cancelled(&self) -> Result<(), String> {
        match self { Self::File(file) => file.check_cancelled(), Self::Retained { cancelled, .. } => check(cancelled) }
    }
    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        self.check_cancelled()?;
        match self {
            Self::File(file) => ByteSource::read(file, offset, length),
            Self::Retained { source, .. } => {
                let bytes = source.bytes();
                let start = offset.min(bytes.len() as u64) as usize;
                let end = offset.saturating_add(length).min(bytes.len() as u64).max(start as u64) as usize;
                Ok(bytes[start..end].to_vec())
            }
        }
    }
}

impl ZipRead for OpenSource {
    fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, JsError> {
        ByteSource::read(self, offset, length).map_err(JsError::plain)
    }
}

struct SharedSource(SharedZip, Option<Arc<dyn Fn() -> bool + Send + Sync>>);
impl ByteSource for SharedSource {
    fn size(&self) -> u64 { self.0.borrow().size }
    fn check_cancelled(&self) -> Result<(), String> {
        if self.1.as_ref().is_some_and(|cancelled| cancelled()) { Err("Document operation cancelled".into()) } else { Ok(()) }
    }
    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let mut result = Vec::new();
        let mut done = 0u64;
        while done < length {
            self.check_cancelled()?;
            let count = (length - done).min(64 * 1024);
            let position = offset.checked_add(done).ok_or("ZIP offset overflow")?;
            let part = self.0.borrow_mut().reader.read(position, count).map_err(|error| error.message)?;
            self.check_cancelled()?;
            done += part.len() as u64;
            result.extend_from_slice(&part);
            if part.len() < count as usize { break; }
        }
        Ok(result)
    }
}

fn entry(entry: &zip_splice::ZipEntry) -> ZipEntry {
    ZipEntry {
        name: entry.name.clone(), method: entry.method, csize: u64::from(entry.csize),
        uncompressed_size: u64::from(entry.usize), data_offset: entry.data_offset,
    }
}

#[derive(Default)]
pub(super) struct Library {
    opened: Option<OpenSource>,
    cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl Library {
    pub(super) fn set_cancelled(&mut self, cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>) {
        self.cancelled = cancelled;
    }
}

impl ZipLibrary for Library {
    fn check_cancelled(&self) -> Result<(), JsError> {
        if self.cancelled.as_ref().is_some_and(|cancelled| cancelled()) {
            Err(JsError::plain("Document operation cancelled"))
        } else { Ok(()) }
    }
    fn open_zip_file(&mut self, path: &str, file: Option<Arc<File>>) -> Result<ZipFile, JsError> {
        let file = match file {
            Some(file) => file,
            None => Arc::new(File::open(path).map_err(|error| JsError::plain(error.to_string()))?),
        };
        let size = file.metadata().map_err(|error| JsError::plain(error.to_string()))?.len();
        let source = OpenSource::File(FileSource { file, size, cancelled: self.cancelled.clone() });
        let entries = zip_splice::read_zip_entries(&source, &ooxml_pkg::DEFAULT_ZIP_LIMITS)
            .map_err(JsError::plain)?;
        let mut table = JsMap::new();
        for item in &entries {
            source.check_cancelled().map_err(JsError::plain)?;
            table.set(item.name.clone(), entry(item));
        }
        self.opened = Some(source.clone());
        Ok(ZipFile { size, entries: Rc::new(table), reader: Box::new(source) })
    }

    fn open_zip_retained(&mut self, retained: Arc<dyn RetainedZipBytes>) -> Result<ZipFile, JsError> {
        let source = OpenSource::Retained { source: retained, cancelled: self.cancelled.clone() };
        let size = source.size();
        let entries = zip_splice::read_zip_entries(&source, &ooxml_pkg::DEFAULT_ZIP_LIMITS).map_err(JsError::plain)?;
        let mut table = JsMap::new();
        for item in &entries {
            source.check_cancelled().map_err(JsError::plain)?;
            table.set(item.name.clone(), entry(item));
        }
        self.opened = Some(source.clone());
        Ok(ZipFile { size, entries: Rc::new(table), reader: Box::new(source) })
    }

    fn close_zip_file(&mut self, _zip: ZipFile) {
        self.opened = None;
    }

    fn read_zip_entries(&mut self, bytes: &[u8]) -> Result<Vec<ZipEntry>, JsError> {
        let cancelled = self.cancelled.clone();
        let entries = zip_splice::read_zip_entries_checked(bytes, &ooxml_pkg::DEFAULT_ZIP_LIMITS, &|| check(&cancelled)).map_err(JsError::plain)?;
        let mut result = Vec::with_capacity(entries.len());
        for item in &entries {
            check(&cancelled).map_err(JsError::plain)?;
            result.push(entry(item));
        }
        Ok(result)
    }

    fn slim_docx(&mut self, _zip: &ZipFile, hash: &str, min_bytes: u64) -> Result<Option<Vec<u8>>, JsError> {
        let source = self.opened.clone().ok_or_else(|| JsError::plain("ZIP source is closed"))?;
        let source = zip_splice::ZipFile::open(source).map_err(JsError::plain)?;
        zip_splice::slim_docx(&source, hash, min_bytes as f64)
            .map(|result| result.map(|result| result.bytes)).map_err(JsError::plain)
    }

    fn lazy_media_hashes_in(&mut self, bytes: &[u8]) -> Result<Vec<String>, JsError> {
        let cancelled = self.cancelled.clone();
        zip_splice::lazy_media_hashes_in_checked(bytes, &|| check(&cancelled)).map_err(JsError::plain)
    }

    fn materialize_docx(&mut self, bytes: &[u8], get_zip: &mut ZipSourceOf<'_>) -> Result<Vec<u8>, JsError> {
        let cancelled = self.cancelled.clone();
        zip_splice::materialize_docx_checked(bytes, &mut |hash| {
            let Some(zip) = get_zip(self, hash).map_err(|error| error.message)? else { return Ok(None); };
            let archive = zip_splice::ZipFile::open(SharedSource(zip, cancelled.clone()))?;
            Ok(Some(Box::new(archive) as Box<dyn ZipArchive>))
        }, &|| check(&cancelled)).map_err(JsError::plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    struct Bytes(Vec<u8>);
    impl RetainedZipBytes for Bytes {
        fn bytes(&self) -> &[u8] { &self.0 }
    }

    #[test]
    fn retained_archive_serves_and_materializes_without_source_path() {
        let image = vec![37u8; 1024 * 1024];
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file("word/document.xml", options).unwrap();
        writer.write_all(b"<document/>").unwrap();
        writer.start_file("word/media/image.png", options).unwrap();
        writer.write_all(&image).unwrap();
        let source = Arc::new(Bytes(writer.finish().unwrap().into_inner()));
        let weak = Arc::downgrade(&source);
        let mut media = main_core::docs::lazy_media::LazyMedia::new();
        let mut library = Library::default();
        let opened = media.open_lazy_retained(&mut library, "/no-reopen/source.docx", source.clone(), 7.0).unwrap().unwrap();
        let url = docx_engine::lazy_media::lazy_media_url(&opened.hash, "word/media/image.png");
        assert_eq!(media.read_lazy_media(&mut library, &url).unwrap().unwrap().body, image);
        let restored = media.materialize_lazy_docx(&mut library, &opened.bytes).unwrap();
        assert_eq!(restored.hashes, vec![opened.hash]);
        assert!(restored.bytes.len() > opened.bytes.len());
        let mut recovered = main_core::docs::lazy_media::LazyMedia::new();
        recovered.adopt_lazy_retained(&mut library, &opened.bytes, "/no-reopen/source.docx", source, 8.0).unwrap();
        assert_eq!(recovered.read_lazy_media(&mut library, &url).unwrap().unwrap().body, image);
        recovered.forget_lazy_media_owner(8.0);
        media.forget_lazy_media_owner(7.0);
        assert!(weak.upgrade().is_none());
    }
}
