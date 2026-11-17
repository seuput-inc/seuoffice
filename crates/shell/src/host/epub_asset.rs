//! The `epub-asset://<token>/<entry path>` scheme: a book's pictures, styles, fonts and
//! chapters for the reader's chapter frame.
//!
//! Rules follow apps/epub/src/main/asset-protocol.ts. The token is random and names one view;
//! it is the only way to reach that view's book and stops working when the view closes. Served:
//! - only entries of that book's zip (`..` stops at the zip root; nothing on disk and nothing
//!   remote is ever reached);
//! - obfuscated fonts revealed, for drawing only;
//! - with a content type from a fixed table, `nosniff`, and a CSP that blocks scripts and every
//!   outside connection, should an entry ever be loaded as a document.
//!
//! Refused: script entries, any method but GET and HEAD, unknown tokens, missing entries. The
//! scheme boundary in `asset_protocol` additionally answers a token only to the document of the
//! view that owns it (`EpubEditors::asset_book`).
//!
//! The parsed book is not `Send`, and opening one inflates the whole archive through the zip
//! gate, so one worker thread owns a few opened books and every request is answered there.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, LazyLock, Weak};

use epub_engine::open::{EPUB_ZIP_LIMITS, EpubOpenResult, open_epub};
use epub_engine::primitives::normalize_zip_path;
use epub_engine::types::EpubBook;
use main_core::docs::lazy_media::MediaAnswer;
use shell_ipc_d2::epub::hosts::Bytes;

const SCHEME: &str = "epub-asset";

/// The policy every response carries. A document served from here (an SVG or a chapter opened
/// directly) runs no script and reaches nothing outside the book; `sandbox` gives it an opaque
/// origin besides. The book is named in both shapes of its scheme, `epub-asset:` and WebView2's
/// `http://epub-asset.localhost`.
pub(crate) const CSP: &str = "default-src 'none'; \
    img-src epub-asset: http://epub-asset.localhost data:; \
    style-src epub-asset: http://epub-asset.localhost 'unsafe-inline'; \
    font-src epub-asset: http://epub-asset.localhost data:; \
    media-src epub-asset: http://epub-asset.localhost; \
    script-src 'none'; connect-src 'none'; object-src 'none'; frame-src 'none'; \
    form-action 'none'; base-uri epub-asset: http://epub-asset.localhost; sandbox";

/// Content types by extension. Anything else is served as opaque bytes.
const MIME_BY_EXT: &[(&str, &str)] = &[
    ("xhtml", "application/xhtml+xml"),
    ("xht", "application/xhtml+xml"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("css", "text/css"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("avif", "image/avif"),
    ("ico", "image/x-icon"),
    ("otf", "font/otf"),
    ("ttf", "font/ttf"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("mp3", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("ogg", "audio/ogg"),
    ("oga", "audio/ogg"),
    ("opus", "audio/opus"),
    ("wav", "audio/wav"),
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("webm", "video/webm"),
    ("ogv", "video/ogg"),
    ("xml", "application/xml"),
    ("opf", "application/oebps-package+xml"),
    ("ncx", "application/x-dtbncx+xml"),
    ("smil", "application/smil+xml"),
    ("txt", "text/plain"),
];

const SCRIPT_EXTS: [&str; 3] = ["js", "mjs", "cjs"];

/// `/(?:java|ecma)script/i`
fn is_script_media_type(media_type: &str) -> bool {
    let lower = media_type.to_ascii_lowercase();
    lower.contains("javascript") || lower.contains("ecmascript")
}

/// `extOf`: the lowercase text after the last dot of the file name, `""` without a dot.
fn extension_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map_or_else(String::new, |(_, ext)| ext.to_lowercase())
}

/// `assetContentType`: the content type an entry is served with; `None` for a script, which is
/// never served.
fn content_type(path: &str, manifest_media_type: Option<&str>) -> Option<&'static str> {
    let ext = extension_of(path);
    if SCRIPT_EXTS.contains(&ext.as_str()) || manifest_media_type.is_some_and(is_script_media_type)
    {
        return None;
    }
    if let Some((_, mime)) = MIME_BY_EXT.iter().find(|(known, _)| *known == ext) {
        return Some(*mime);
    }
    // the manifest names a type the table knows under another extension: trust only those
    if let Some(declared) = manifest_media_type
        && let Some((_, mime)) = MIME_BY_EXT.iter().find(|(_, known)| *known == declared)
    {
        return Some(*mime);
    }
    Some("application/octet-stream")
}

/// A refusal: the status and the policy, nothing else. `Access-Control-Allow-Origin` is added
/// to every answer of every scheme by `asset_protocol`.
pub(crate) fn refusal(status: u16) -> MediaAnswer {
    MediaAnswer {
        status,
        headers: vec![("Content-Security-Policy".to_owned(), CSP.to_owned())],
        body: Vec::new(),
    }
}

/// The view token and the zip entry path an address names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) token: String,
    pub(crate) path: String,
}

/// `parseAssetUrl`. `Err` is the status the handler answered: 400 for an address that is not
/// this scheme's or is not decodable, 404 for a malformed token or an empty path.
pub(crate) fn parse(uri: &str) -> Result<Target, u16> {
    // `http://epub-asset.localhost/<token>/<path>` is the Windows shape of
    // `epub-asset://<token>/<path>`; the handler only ever sees its own scheme, so both shapes
    // are read on every OS
    let wire = tauri::Url::parse(uri).map_err(|_| 400u16)?;
    let url = crate::native::url::logical_url_for(crate::native::url::Flavor::LocalhostHttp, &wire)
        .unwrap_or(wire);
    if url.scheme() != SCHEME {
        return Err(400);
    }
    let token = url.host_str().unwrap_or("").to_ascii_lowercase();
    if !(16..=64).contains(&token.len())
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(404);
    }
    let decoded = jscompat::decode_uri_component(url.path()).map_err(|_| 400u16)?;
    // `..` stops at the zip root; a backslash is never a separator inside a zip
    let path = normalize_zip_path(&decoded);
    if path.is_empty() {
        return Err(404);
    }
    Ok(Target { token, path })
}

/// The headers of a served entry.
fn headers(content_type: &str) -> Vec<(String, String)> {
    [
        ("Content-Type", content_type),
        ("Content-Security-Policy", CSP),
        ("X-Content-Type-Options", "nosniff"),
        ("Cache-Control", "no-store"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect()
}

/// `serveEpubAsset` after the lookup: one entry of an opened book.
fn answer(book: &EpubBook, head: bool, path: &str) -> MediaAnswer {
    if !book.has(path) {
        return refusal(404);
    }
    let declared = book
        .info
        .manifest
        .iter()
        .find(|item| item.path.as_deref() == Some(path))
        .map(|item| item.media_type.as_str());
    let Some(content_type) = content_type(path, declared) else {
        return refusal(403);
    };
    let bytes = match book.read_for_rendering(path) {
        Ok(Some(bytes)) => bytes,
        _ => return refusal(404),
    };
    let mut headers = headers(content_type);
    if head {
        return MediaAnswer {
            status: 200,
            headers,
            body: Vec::new(),
        };
    }
    headers.push(("Content-Length".to_owned(), bytes.len().to_string()));
    MediaAnswer {
        status: 200,
        headers,
        body: bytes,
    }
}

/// The most books kept open at once, the oldest dropped first.
const CACHE_CAPACITY: usize = 4;

struct Opened {
    /// Identity of the bytes the book was opened from, without keeping them alive: a closed
    /// view or a saved book drops its bytes, and the entry goes with them.
    source: Weak<Vec<u8>>,
    /// `None` for bytes that did not open: every request for them is a 404.
    book: Option<EpubBook>,
}

#[derive(Default)]
struct Cache {
    books: Vec<Opened>,
}

impl Cache {
    fn answer(&mut self, bytes: &Bytes, head: bool, path: &str) -> MediaAnswer {
        self.books.retain(|opened| opened.source.strong_count() > 0);
        let known = self
            .books
            .iter()
            .position(|opened| std::ptr::eq(opened.source.as_ptr(), Arc::as_ptr(bytes)));
        let at = known.unwrap_or_else(|| {
            let book = match open_epub(bytes.as_slice(), &EPUB_ZIP_LIMITS) {
                EpubOpenResult::Opened(book) => Some(*book),
                EpubOpenResult::Refused { .. } => None,
            };
            if self.books.len() == CACHE_CAPACITY {
                self.books.remove(0);
            }
            self.books.push(Opened {
                source: Arc::downgrade(bytes),
                book,
            });
            self.books.len() - 1
        });
        match &self.books[at].book {
            Some(book) => answer(book, head, path),
            None => refusal(404),
        }
    }
}

struct Job {
    bytes: Bytes,
    head: bool,
    path: String,
    reply: SyncSender<MediaAnswer>,
}

fn run(jobs: Receiver<Job>) {
    let mut cache = Cache::default();
    for job in jobs {
        let answered = catch_unwind(AssertUnwindSafe(|| {
            cache.answer(&job.bytes, job.head, &job.path)
        }));
        let _ = job.reply.send(answered.unwrap_or_else(|_| refusal(500)));
    }
}

/// The worker thread, started by the first request; `Err` says why it could not start.
static WORKER: LazyLock<Result<Sender<Job>, String>> = LazyLock::new(|| {
    let (jobs, queue) = mpsc::channel();
    std::thread::Builder::new()
        .name("seuoffice-epub-assets".to_owned())
        .spawn(move || run(queue))
        .map(|_| jobs)
        .map_err(|error| format!("The EPUB asset worker did not start: {error}"))
});

/// Answers a request for `path` of the book held in `bytes`; blocks until the worker has.
pub(crate) fn serve(bytes: Bytes, head: bool, path: String) -> Result<MediaAnswer, String> {
    let (reply, answered) = mpsc::sync_channel(1);
    WORKER
        .as_ref()
        .map_err(Clone::clone)?
        .send(Job {
            bytes,
            head,
            path,
            reply,
        })
        .map_err(|_| "The EPUB asset worker stopped.".to_owned())?;
    answered
        .recv()
        .map_err(|_| "The EPUB asset worker stopped.".to_owned())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use epub_engine::primitives::{IDPF_FONT_OBFUSCATION, obfuscate_font};

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const BOOK_ID: &str = "urn:uuid:12345678-90ab-cdef-1234-567890abcdef";

    fn font() -> Vec<u8> {
        (0..2048usize).map(|i| ((i * 7) % 256) as u8).collect()
    }

    /// A two-chapter book with a stylesheet, a picture, an obfuscated font and a script.
    fn book() -> Vec<u8> {
        let container = r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
        let opf = format!(
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="uid">{BOOK_ID}</dc:identifier><dc:title>A Small Book</dc:title><dc:language>en</dc:language><meta property="dcterms:modified">2026-01-01T00:00:00Z</meta></metadata><manifest><item id="one" href="text/one.xhtml" media-type="application/xhtml+xml"/><item id="css" href="styles/book.css" media-type="text/css"/><item id="font" href="fonts/a.otf" media-type="font/otf"/><item id="js" href="code/app.js" media-type="text/javascript"/><item id="pic" href="images/pic.png" media-type="image/png"/><item id="odd" href="data/odd.dat" media-type="application/javascript"/></manifest><spine><itemref idref="one"/></spine></package>"#
        );
        let encryption = format!(
            r#"<?xml version="1.0"?><encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container" xmlns:enc="http://www.w3.org/2001/04/xmlenc#"><enc:EncryptedData><enc:EncryptionMethod Algorithm="{IDPF_FONT_OBFUSCATION}"/><enc:CipherData><enc:CipherReference URI="OEBPS/fonts/a.otf"/></enc:CipherData></enc:EncryptedData></encryption>"#
        );
        let obfuscated = obfuscate_font(&font(), IDPF_FONT_OBFUSCATION, BOOK_ID).unwrap();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let files: [(&str, &[u8]); 10] = [
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", container.as_bytes()),
            ("META-INF/encryption.xml", encryption.as_bytes()),
            ("OEBPS/content.opf", opf.as_bytes()),
            ("OEBPS/styles/book.css", b"body { margin: 0 }\n"),
            (
                "OEBPS/text/one.xhtml",
                br#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title></head><body><p>x</p></body></html>"#,
            ),
            ("OEBPS/fonts/a.otf", &obfuscated),
            ("OEBPS/code/app.js", b"alert(1)\n"),
            ("OEBPS/images/pic.png", &[0x89, 0x50, 0x4e, 0x47, 1, 2, 3]),
            ("OEBPS/data/odd.dat", b"not script"),
        ];
        for (path, body) in files {
            zip.start_file(path, options).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn opened() -> EpubBook {
        match open_epub(&book(), &EPUB_ZIP_LIMITS) {
            EpubOpenResult::Opened(book) => *book,
            EpubOpenResult::Refused { reason, detail } => {
                panic!("the test book did not open: {} {detail}", reason.as_str())
            }
        }
    }

    fn header<'a>(answer: &'a MediaAnswer, name: &str) -> Option<&'a str> {
        answer
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn address(path: &str) -> String {
        format!("{SCHEME}://{TOKEN}/{path}")
    }

    #[test]
    fn an_address_names_the_views_token_and_an_entry() {
        assert_eq!(
            parse(&address("OEBPS/text/chapter%201.xhtml")),
            Ok(Target {
                token: TOKEN.to_owned(),
                path: "OEBPS/text/chapter 1.xhtml".to_owned()
            })
        );
        // the Windows shape names the same entry
        assert_eq!(
            parse(&format!(
                "http://epub-asset.localhost/{TOKEN}/OEBPS/a.css?x=1#y"
            )),
            Ok(Target {
                token: TOKEN.to_owned(),
                path: "OEBPS/a.css".to_owned()
            })
        );
    }

    #[test]
    fn dot_dot_stops_at_the_zip_root() {
        let expected = Ok(Target {
            token: TOKEN.to_owned(),
            path: "etc/passwd".to_owned(),
        });
        for path in [
            "OEBPS/%2E%2E/%2E%2E/%2E%2E/etc/passwd",
            "OEBPS/..%2F..%2F..%2Fetc/passwd",
            "../../../etc/passwd",
            "%2e%2e%2f%2e%2e%2fetc%2fpasswd",
            "./OEBPS/../etc//passwd",
        ] {
            assert_eq!(parse(&address(path)), expected, "{path}");
        }
        // a backslash is a name character in a zip, never a separator
        assert_eq!(
            parse(&address("OEBPS/..%5C..%5Cwin.ini")).map(|target| target.path),
            Ok("OEBPS/..\\..\\win.ini".to_owned())
        );
    }

    #[test]
    fn another_scheme_a_bad_token_or_a_bad_escape_is_refused() {
        assert_eq!(parse("file:///C:/Windows/win.ini"), Err(400));
        assert_eq!(parse("html-asset://local/etc/passwd"), Err(400));
        assert_eq!(parse("not a url"), Err(400));
        assert_eq!(parse(&address("OEBPS/%E0%A4%A")), Err(400));
        assert_eq!(parse("epub-asset://not-a-token/OEBPS/a.css"), Err(404));
        assert_eq!(parse("epub-asset://0123456789abcde/a.css"), Err(404));
        assert_eq!(
            parse(&format!("epub-asset://{}/a.css", "a".repeat(65))),
            Err(404)
        );
        assert_eq!(parse(&format!("epub-asset://{TOKEN}")), Err(404));
        assert_eq!(parse(&address("")), Err(404));
        assert_eq!(parse(&address("%2e%2e/%2e%2e")), Err(404));
    }

    #[test]
    fn a_token_is_read_in_lower_case() {
        let upper = format!("epub-asset://{}/a.css", TOKEN.to_uppercase());
        assert_eq!(parse(&upper).unwrap().token, TOKEN);
    }

    #[test]
    fn content_types_come_from_a_fixed_table_and_never_from_a_script_type() {
        assert_eq!(
            content_type("a/b.xhtml", None),
            Some("application/xhtml+xml")
        );
        assert_eq!(content_type("a/b.SVG", None), Some("image/svg+xml"));
        assert_eq!(content_type("a/b.woff2", None), Some("font/woff2"));
        assert_eq!(content_type("a/b.js", None), None);
        assert_eq!(content_type("a/b.MJS", None), None);
        assert_eq!(
            content_type("a/b.dat", Some("application/javascript")),
            None
        );
        assert_eq!(content_type("a/b.dat", Some("text/ECMAScript")), None);
        assert_eq!(content_type("a/b", Some("image/png")), Some("image/png"));
        assert_eq!(
            content_type("a/b", Some("text/html; evil")),
            Some("application/octet-stream")
        );
        assert_eq!(
            content_type("a/b.dat", None),
            Some("application/octet-stream")
        );
        // the extension wins over a manifest type the table knows
        assert_eq!(content_type("a/b.css", Some("image/png")), Some("text/css"));
        // only the file name carries the extension
        assert_eq!(
            content_type("a.js/b", None),
            Some("application/octet-stream")
        );
    }

    #[test]
    fn the_policy_blocks_scripts_and_every_outside_connection() {
        for directive in [
            "default-src 'none'",
            "script-src 'none'",
            "connect-src 'none'",
            "object-src 'none'",
            "frame-src 'none'",
            "form-action 'none'",
        ] {
            assert!(CSP.contains(directive), "{directive}");
        }
        assert!(CSP.ends_with("sandbox"));
        assert!(!CSP.contains("https"));
        assert!(
            !CSP.replace("http://epub-asset.localhost", "")
                .contains("http")
        );
        assert!(!CSP.contains('*'));
    }

    #[test]
    fn an_entry_is_served_with_its_type_the_policy_and_nosniff() {
        let served = answer(&opened(), false, "OEBPS/styles/book.css");
        assert_eq!(served.status, 200);
        assert_eq!(header(&served, "Content-Type"), Some("text/css"));
        assert_eq!(header(&served, "Content-Security-Policy"), Some(CSP));
        assert_eq!(header(&served, "X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(header(&served, "Cache-Control"), Some("no-store"));
        assert_eq!(served.body, b"body { margin: 0 }\n");
        assert_eq!(header(&served, "Content-Length"), Some("19"));
    }

    #[test]
    fn head_answers_the_headers_without_a_body() {
        let served = answer(&opened(), true, "OEBPS/images/pic.png");
        assert_eq!(served.status, 200);
        assert_eq!(header(&served, "Content-Type"), Some("image/png"));
        assert!(served.body.is_empty());
    }

    #[test]
    fn an_obfuscated_font_is_revealed_for_drawing() {
        let book = opened();
        let stored = book.read("OEBPS/fonts/a.otf").unwrap().unwrap();
        assert_ne!(stored, font());
        let served = answer(&book, false, "OEBPS/fonts/a.otf");
        assert_eq!(header(&served, "Content-Type"), Some("font/otf"));
        assert_eq!(served.body, font());
    }

    #[test]
    fn scripts_are_refused_by_extension_and_by_manifest_type() {
        let book = opened();
        for path in ["OEBPS/code/app.js", "OEBPS/data/odd.dat"] {
            let refused = answer(&book, false, path);
            assert_eq!(refused.status, 403, "{path}");
            assert!(refused.body.is_empty());
            assert_eq!(header(&refused, "Content-Security-Policy"), Some(CSP));
        }
    }

    #[test]
    fn an_entry_the_book_lacks_is_not_found_and_carries_the_policy() {
        let missing = answer(&opened(), false, "OEBPS/missing.png");
        assert_eq!(missing.status, 404);
        assert_eq!(header(&missing, "Content-Security-Policy"), Some(CSP));
        // a directory is no file entry
        assert_eq!(answer(&opened(), false, "OEBPS").status, 404);
    }

    #[test]
    fn a_path_that_climbs_out_never_reaches_a_file_outside_the_book() {
        let target = parse(&address("OEBPS/../../../../Windows/win.ini")).unwrap();
        assert_eq!(target.path, "Windows/win.ini");
        assert_eq!(answer(&opened(), false, &target.path).status, 404);
    }

    #[test]
    fn a_book_is_opened_once_and_forgotten_with_its_bytes() {
        let mut cache = Cache::default();
        let bytes: Bytes = Arc::new(book());
        assert_eq!(
            cache.answer(&bytes, false, "OEBPS/styles/book.css").status,
            200
        );
        assert_eq!(
            cache.answer(&bytes, false, "OEBPS/images/pic.png").status,
            200
        );
        assert_eq!(cache.books.len(), 1);
        let other: Bytes = Arc::new(book());
        assert_eq!(
            cache.answer(&other, false, "OEBPS/styles/book.css").status,
            200
        );
        assert_eq!(cache.books.len(), 2);
        drop(bytes);
        assert_eq!(
            cache.answer(&other, false, "OEBPS/styles/book.css").status,
            200
        );
        assert_eq!(cache.books.len(), 1);
    }

    #[test]
    fn the_cache_keeps_at_most_four_books_and_a_damaged_one_is_not_found() {
        let mut cache = Cache::default();
        let kept: Vec<Bytes> = (0..6).map(|_| Arc::new(book())).collect();
        for bytes in &kept {
            assert_eq!(
                cache.answer(bytes, false, "OEBPS/styles/book.css").status,
                200
            );
        }
        assert_eq!(cache.books.len(), CACHE_CAPACITY);
        let damaged: Bytes = Arc::new(b"not a zip".to_vec());
        let refused = cache.answer(&damaged, false, "OEBPS/styles/book.css");
        assert_eq!(refused.status, 404);
        assert_eq!(header(&refused, "Content-Security-Policy"), Some(CSP));
    }

    #[test]
    fn the_worker_answers_requests_from_any_thread() {
        let bytes: Bytes = Arc::new(book());
        let served = serve(bytes.clone(), false, "OEBPS/styles/book.css".to_owned()).unwrap();
        assert_eq!(served.status, 200);
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let bytes = bytes.clone();
                std::thread::spawn(move || serve(bytes, true, "OEBPS/images/pic.png".to_owned()))
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap().unwrap().status, 200);
        }
        let damaged: Bytes = Arc::new(b"not a zip".to_vec());
        assert_eq!(
            serve(damaged, false, "a.css".to_owned()).unwrap().status,
            404
        );
    }
}
