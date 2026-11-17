//! The Electron build's localStorage, carried into the system webviews.
//!
//! The Electron build kept it in Chromium's LevelDB under the userData
//! folder: `Local Storage/leveldb` for the editors and the shell, and the
//! `ieum-diagram` partition's own store for the diagram app and the draw.io
//! frame inside it. The system webviews keep theirs somewhere else, per
//! origin, and the origins are not the same: an editor was
//! `genoffice-app://<host>`, the shell's pages were `file://` pages, and the
//! native scheme of a host is `app_scheme::native_origin`. Nothing carries the
//! data over by itself.
//!
//! [`prepare`] runs once before the first webview exists. It reads a copy of
//! the two stores (the originals stay as Electron left them), keeps the live
//! entries of the known origins, and writes them to
//! `<userData>/localstorage-import-v1.json`. A read that fails is logged and
//! writes nothing, so the next launch tries again; stores that are not there
//! are an empty import. [`init_scripts`] gives a webview the script that
//! applies its own origin's entries at document start: only the keys the
//! origin does not have yet, and only once, so a key the user removed in the
//! new app is not brought back and a value written there is not overwritten.
//! The webview's own storage is never written to from outside.

mod leveldb;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use main_core::packages::electron_utils::renderer_scheme::{RENDERER_HOSTS, RENDERER_SCHEME};
use serde_json::{Value, json};

use crate::app_scheme::{SCHEME_PREFIX, SHELL_HOST};
use crate::native::url::Flavor;

const CACHE_FILE: &str = "localstorage-import-v1.json";
const CACHE_VERSION: u64 = 1;
/// Set in an origin's localStorage once its import has been applied
const FLAG_KEY: &str = "__seuoffice_localstorage_import_v1";
const DIAGRAM_HOST: &str = "diagram";
const DRAWIO_SCHEME: &str = "drawio-app";
const DRAWIO_HOST: &str = "editor";
/// `^` in a storage key is followed by `0` and the site of the top-level page
const EMBEDDED_IN_APP: &str = "0genoffice-app";

/// Chromium's marks in front of a stored string: one byte per character
/// (Latin-1), or two (UTF-16LE).
const ENCODING_UTF16: u8 = 0;
const ENCODING_LATIN1: u8 = 1;

/// native origin -> key -> value
type Origins = BTreeMap<String, BTreeMap<String, String>>;

static IMPORT: OnceLock<Origins> = OnceLock::new();

/// The script of one webview: `all_frames` for one that has to run in the
/// frames of other origins (draw.io inside the diagram page) too.
pub(crate) struct InitScript {
    pub source: String,
    pub all_frames: bool,
}

/// Reads the Electron build's stores unless that was done before. Never fails:
/// what cannot be read is logged and the webviews start without an import.
pub(crate) fn prepare(user_data: &Path) {
    let origins = load_or_import(user_data, Flavor::CURRENT).unwrap_or_else(|error| {
        eprintln!("shell: localStorage import skipped: {error}");
        Origins::new()
    });
    let _ = IMPORT.set(origins);
}

/// The scripts for the webview that loads `page_url` (the Electron form,
/// `genoffice-app://<host>/...`): its own origin's, and for the diagram page
/// also the draw.io origin's for the frames inside it.
pub(crate) fn init_scripts(page_url: &str) -> Vec<InitScript> {
    IMPORT
        .get()
        .map(|origins| scripts_for(origins, page_url, Flavor::CURRENT))
        .unwrap_or_default()
}

fn load_or_import(user_data: &Path, flavor: Flavor) -> Result<Origins, String> {
    let cache = user_data.join(CACHE_FILE);
    if let Some(origins) = read_cache(&cache) {
        return Ok(origins);
    }
    let origins = import(user_data, flavor)?;
    if let Err(error) = write_cache(&cache, &origins) {
        eprintln!(
            "shell: cannot write {}: {error}; the import runs again next launch",
            cache.display()
        );
    }
    Ok(origins)
}

fn import(user_data: &Path, flavor: Flavor) -> Result<Origins, String> {
    let stores = [
        user_data.join("Local Storage").join("leveldb"),
        user_data
            .join("Partitions")
            .join("ieum-diagram")
            .join("Local Storage")
            .join("leveldb"),
    ];
    let mut origins = Origins::new();
    for store in stores {
        if !store.join("CURRENT").is_file() {
            continue;
        }
        let live = read_copy(&store).map_err(|error| format!("{}: {error}", store.display()))?;
        absorb(&mut origins, &live, flavor);
    }
    Ok(origins)
}

/// The live entries of a LevelDB folder, read from a temporary copy
fn read_copy(store: &Path) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, String> {
    let scratch = ScratchDir::create()?;
    for entry in fs::read_dir(store).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let wanted = entry.file_type().is_ok_and(|kind| kind.is_file())
            && is_store_file(&name.to_string_lossy());
        if wanted {
            fs::copy(entry.path(), scratch.0.join(&name))
                .map_err(|error| format!("{}: {error}", name.to_string_lossy()))?;
        }
    }
    leveldb::read_live(&scratch.0)
}

/// What the reader needs; not the lock or the info logs
fn is_store_file(name: &str) -> bool {
    name == "CURRENT"
        || name.starts_with("MANIFEST-")
        || [".log", ".ldb", ".sst"]
            .iter()
            .any(|extension| name.ends_with(extension))
}

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn create() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "seuoffice-ls-import-{}",
            jscompat::random_uuid()
        ));
        fs::create_dir_all(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(ScratchDir(path))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Adds the entries of `live` that belong to a known origin. A later store
/// wins over an earlier one; an entry of the draw.io frame under a
/// top-level partition only fills keys the first-party entries did not set.
fn absorb(origins: &mut Origins, live: &BTreeMap<Vec<u8>, Vec<u8>>, flavor: Flavor) {
    let mut partitioned = Vec::new();
    for (db_key, db_value) in live {
        let Some((storage_key, key, value)) = decode_entry(db_key, db_value) else {
            continue;
        };
        let Some((origin, first_party)) = map_origin(storage_key, flavor) else {
            continue;
        };
        if first_party {
            origins.entry(origin).or_default().insert(key, value);
        } else {
            partitioned.push((origin, key, value));
        }
    }
    for (origin, key, value) in partitioned {
        origins
            .entry(origin)
            .or_default()
            .entry(key)
            .or_insert(value);
    }
}

/// `(storage key, key, value)` of a localStorage record. The schema's
/// metadata (`VERSION`, `META:...`, `METAACCESS:...`) is not a record, and a
/// record is `_` + storage key + NUL + the encoded key.
fn decode_entry<'a>(db_key: &'a [u8], db_value: &[u8]) -> Option<(&'a str, String, String)> {
    let rest = db_key.strip_prefix(b"_")?;
    let nul = rest.iter().position(|&byte| byte == 0)?;
    let storage_key = std::str::from_utf8(&rest[..nul]).ok()?;
    let key = decode_string(&rest[nul + 1..])?;
    let value = decode_string(db_value)?;
    Some((storage_key, key, value))
}

/// An encoding byte, then the characters. A lone UTF-16 surrogate, which
/// JavaScript strings can hold and Rust strings cannot, becomes U+FFFD.
fn decode_string(bytes: &[u8]) -> Option<String> {
    let (&encoding, body) = bytes.split_first()?;
    match encoding {
        ENCODING_LATIN1 => Some(body.iter().map(|&byte| char::from(byte)).collect()),
        ENCODING_UTF16 => {
            if !body.len().is_multiple_of(2) {
                return None;
            }
            let units = body
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
            Some(
                char::decode_utf16(units)
                    .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
                    .collect(),
            )
        }
        _ => None,
    }
}

/// The native origin an Electron storage key belongs to, and whether the key
/// is first-party. Chromium writes a storage key as the origin (older
/// versions) or the origin and a slash, followed by `^` and the top-level
/// site when the frame is embedded.
fn map_origin(storage_key: &str, flavor: Flavor) -> Option<(String, bool)> {
    let (origin, partition) = match storage_key.split_once('^') {
        Some((origin, partition)) => (origin, Some(partition)),
        None => (storage_key, None),
    };
    let origin = origin.strip_suffix('/').unwrap_or(origin);
    let (scheme, host) = origin.split_once("://")?;
    let native = match (scheme, host) {
        ("file", "") => app_origin(flavor, SHELL_HOST),
        (DRAWIO_SCHEME, DRAWIO_HOST) => drawio_origin(flavor),
        (RENDERER_SCHEME, host) if RENDERER_HOSTS.contains(&host) => app_origin(flavor, host),
        _ => return None,
    };
    match partition {
        None => Some((native, true)),
        // the draw.io frame inside the diagram page: `0` and the site of the top-level page
        Some(top) if scheme == DRAWIO_SCHEME && top.starts_with(EMBEDDED_IN_APP) => {
            Some((native, false))
        }
        Some(_) => None,
    }
}

/// `app_scheme::native_origin` for either platform's shape
fn app_origin(flavor: Flavor, host: &str) -> String {
    match flavor {
        Flavor::Scheme => format!("{SCHEME_PREFIX}{host}://localhost"),
        Flavor::LocalhostHttp => format!("http://{SCHEME_PREFIX}{host}.localhost"),
    }
}

/// The draw.io origin as the webview sees it (`native::url` translates the
/// logical `drawio-app://editor`)
fn drawio_origin(flavor: Flavor) -> String {
    match flavor {
        Flavor::Scheme => format!("{DRAWIO_SCHEME}://{DRAWIO_HOST}"),
        Flavor::LocalhostHttp => format!("http://{DRAWIO_SCHEME}.localhost"),
    }
}

fn read_cache(path: &Path) -> Option<Origins> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            eprintln!("shell: cannot read {}: {error}", path.display());
            return None;
        }
    };
    match parse_cache(&text) {
        Ok(origins) => Some(origins),
        Err(error) => {
            eprintln!("shell: ignoring {}: {error}", path.display());
            None
        }
    }
}

fn parse_cache(text: &str) -> Result<Origins, String> {
    let root: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    if root.get("version").and_then(Value::as_u64) != Some(CACHE_VERSION) {
        return Err("an unknown version".to_owned());
    }
    let listed = root
        .get("origins")
        .and_then(Value::as_object)
        .ok_or("no origins")?;
    let mut origins = Origins::new();
    for (origin, entries) in listed {
        let entries = entries.as_object().ok_or("an origin that is not an object")?;
        let mut map = BTreeMap::new();
        for (key, value) in entries {
            let value = value.as_str().ok_or("a value that is not a string")?;
            map.insert(key.clone(), value.to_owned());
        }
        origins.insert(origin.clone(), map);
    }
    Ok(origins)
}

/// Written beside the target and renamed over it, so a crash leaves no half file
fn write_cache(path: &Path, origins: &Origins) -> Result<(), String> {
    let bytes = serde_json::to_vec(&json!({ "version": CACHE_VERSION, "origins": origins }))
        .map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("json.tmp");
    let written = fs::File::create(&temporary).and_then(|mut file| {
        file.write_all(&bytes)?;
        file.sync_all()
    });
    written
        .and_then(|()| fs::rename(&temporary, path))
        .map_err(|error| {
            let _ = fs::remove_file(&temporary);
            error.to_string()
        })
}

fn scripts_for(origins: &Origins, page_url: &str, flavor: Flavor) -> Vec<InitScript> {
    let Some(host) = renderer_host(page_url) else {
        return Vec::new();
    };
    let mut scripts = Vec::new();
    let mut add = |origin: String, all_frames: bool| {
        let source = origins
            .get(&origin)
            .and_then(|entries| script(&origin, entries));
        if let Some(source) = source {
            scripts.push(InitScript { source, all_frames });
        }
    };
    add(app_origin(flavor, &host), false);
    if host == DIAGRAM_HOST {
        add(drawio_origin(flavor), true);
    }
    scripts
}

/// The host of an Electron-form renderer URL the shell serves
fn renderer_host(page_url: &str) -> Option<String> {
    let url = tauri::Url::parse(page_url).ok()?;
    let host = url.host_str()?;
    let known = host == SHELL_HOST || RENDERER_HOSTS.contains(&host);
    (url.scheme() == RENDERER_SCHEME && known).then(|| host.to_owned())
}

/// `window.localStorage` entries are `[key, value]` pairs, not an object, so
/// a key such as `__proto__` is just a key.
const SCRIPT: &str = r#"(function () {
  try {
    if (location.protocol !== __PROTOCOL__ || location.host !== __HOST__) return;
    var storage = window.localStorage;
    var flag = __FLAG__;
    if (storage.getItem(flag) !== null) return;
    var entries = __ENTRIES__;
    for (var i = 0; i < entries.length; i++) {
      if (storage.getItem(entries[i][0]) === null) {
        storage.setItem(entries[i][0], entries[i][1]);
      }
    }
    storage.setItem(flag, "1");
  } catch (error) {}
})();
"#;

/// The script that applies `entries` in a document of exactly `origin`
/// (protocol and host as `location` has them)
fn script(origin: &str, entries: &BTreeMap<String, String>) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let (scheme, host) = origin.split_once("://")?;
    let pairs: Vec<Value> = entries.iter().map(|(key, value)| json!([key, value])).collect();
    // the entries go in last: nothing in them is looked at for the placeholders
    Some(
        SCRIPT
            .replace("__PROTOCOL__", &js_literal(&Value::from(format!("{scheme}:"))))
            .replace("__HOST__", &js_literal(&Value::from(host)))
            .replace("__FLAG__", &js_literal(&Value::from(FLAG_KEY)))
            .replace("__ENTRIES__", &js_literal(&Value::Array(pairs))),
    )
}

/// JSON as a JavaScript literal that is also safe inside an HTML script
/// element and a line-terminator-sensitive parser
fn js_literal(value: &Value) -> String {
    value
        .to_string()
        .replace("</", "<\\/")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use super::leveldb::fixture::{Batch, Fixture, TableEntry, TableSpec, TempDir, write_db};
    use super::*;

    const SLIDES_KEY: &str = "ai-slides-guides:/Users/a/발표.pptx";

    type Op = (Vec<u8>, Option<Vec<u8>>);

    fn latin1(text: &str) -> Vec<u8> {
        let mut out = vec![ENCODING_LATIN1];
        out.extend(text.chars().map(|c| u8::try_from(u32::from(c)).unwrap()));
        out
    }

    fn utf16(text: &str) -> Vec<u8> {
        let mut out = vec![ENCODING_UTF16];
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    fn db_key(storage_key: &str, key: Vec<u8>) -> Vec<u8> {
        let mut out = format!("_{storage_key}").into_bytes();
        out.push(0);
        out.extend(key);
        out
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// A record with a Latin-1 key and value
    fn put(storage_key: &str, key: &str, value: &str) -> Op {
        (db_key(storage_key, latin1(key)), Some(latin1(value)))
    }

    fn numbered(ops: Vec<Op>) -> Vec<TableEntry> {
        ops.into_iter()
            .enumerate()
            .map(|(i, (key, value))| (key, 1 + i as u64, value))
            .collect()
    }

    #[test]
    fn strings_decode_by_their_own_encoding_byte() {
        assert_eq!(decode_string(&latin1("aidocs.showAi")).unwrap(), "aidocs.showAi");
        assert_eq!(decode_string(&[1, 0xe9, 0xff]).unwrap(), "\u{e9}\u{ff}");
        assert_eq!(decode_string(&utf16(SLIDES_KEY)).unwrap(), SLIDES_KEY);
        assert_eq!(decode_string(&utf16("a\u{1f600}")).unwrap(), "a\u{1f600}");
        assert_eq!(decode_string(&[1]).unwrap(), "");
        assert_eq!(decode_string(&[0, 0x00, 0xd8]).unwrap(), "\u{fffd}");
        // odd UTF-16 length, an unknown encoding, nothing at all
        assert_eq!(decode_string(&[0, 0x41]), None);
        assert_eq!(decode_string(&[2, 0x41]), None);
        assert_eq!(decode_string(&[]), None);
    }

    #[test]
    fn a_record_splits_into_storage_key_key_and_value() {
        let record = db_key("genoffice-app://slides", utf16(SLIDES_KEY));
        let (storage_key, key, value) = decode_entry(&record, &latin1("[1,2]")).unwrap();
        assert_eq!(storage_key, "genoffice-app://slides");
        assert_eq!(key, SLIDES_KEY);
        assert_eq!(value, "[1,2]");

        let record = db_key("file://", latin1("home.sidebarWidth"));
        let (storage_key, key, value) = decode_entry(&record, &utf16("300")).unwrap();
        assert_eq!(storage_key, "file://");
        assert_eq!(key, "home.sidebarWidth");
        assert_eq!(value, "300");
    }

    #[test]
    fn schema_metadata_is_not_a_record() {
        let keys: [&[u8]; 4] = [
            b"VERSION",
            b"META:genoffice-app://slides",
            b"METAACCESS:genoffice-app://slides",
            // a record without its separator
            b"_genoffice-app://slides",
        ];
        for key in keys {
            assert_eq!(decode_entry(key, &latin1("1")), None, "{key:?}");
        }
    }

    #[test]
    fn electron_origins_map_to_the_native_origin_of_each_platform() {
        let cases = [
            (
                "genoffice-app://slides",
                "genoffice-app-slides://localhost",
                "http://genoffice-app-slides.localhost",
            ),
            (
                "genoffice-app://help/",
                "genoffice-app-help://localhost",
                "http://genoffice-app-help.localhost",
            ),
            (
                "file://",
                "genoffice-app-shell://localhost",
                "http://genoffice-app-shell.localhost",
            ),
            (
                "file:///",
                "genoffice-app-shell://localhost",
                "http://genoffice-app-shell.localhost",
            ),
            (
                "drawio-app://editor",
                "drawio-app://editor",
                "http://drawio-app.localhost",
            ),
        ];
        for (electron, mac, windows) in cases {
            let on_mac = map_origin(electron, Flavor::Scheme);
            assert_eq!(on_mac, Some((mac.to_owned(), true)), "{electron}");
            let on_windows = map_origin(electron, Flavor::LocalhostHttp);
            assert_eq!(on_windows, Some((windows.to_owned(), true)), "{electron}");
        }
    }

    #[test]
    fn every_renderer_host_maps_to_what_the_scheme_handler_serves() {
        for host in RENDERER_HOSTS {
            let electron = format!("genoffice-app://{host}");
            let mapped = map_origin(&electron, Flavor::CURRENT).map(|(origin, _)| origin);
            assert_eq!(mapped, Some(crate::app_scheme::native_origin(host)), "{host}");
        }
    }

    #[test]
    fn unknown_dev_and_foreign_origins_are_not_imported() {
        for storage_key in [
            "http://localhost:5173",
            "http://localhost:5199/",
            "https://example.com",
            "genoffice-app://unknown",
            "genoffice-app://shell",
            "drawio-app://other",
            "file://host",
            "",
            "genoffice-app://docs/^0https://evil.example",
            "drawio-app://editor/^0https://evil.example",
        ] {
            assert_eq!(map_origin(storage_key, Flavor::Scheme), None, "{storage_key}");
        }
        let embedded = map_origin("drawio-app://editor/^0genoffice-app://diagram", Flavor::Scheme);
        assert_eq!(embedded, Some(("drawio-app://editor".to_owned(), false)));
    }

    /// An Electron userData folder: the default store, whose newest values and
    /// deletions are only in its log, and the diagram partition's store.
    fn electron_profile(user_data: &Path) {
        let guides = (
            db_key("genoffice-app://slides", utf16(SLIDES_KEY)),
            Some(latin1("[1,2]")),
        );
        let dictionary = (
            db_key("genoffice-app://sheets", latin1("aisheets.spell-dictionary")),
            Some(utf16("[\"안녕\"]")),
        );
        let thumbs = "ai-slides-thumbs-width";
        let default_table = vec![
            (b"VERSION".to_vec(), Some(b"1".to_vec())),
            (b"META:genoffice-app://slides".to_vec(), Some(vec![8, 1])),
            (b"METAACCESS:genoffice-app://slides".to_vec(), Some(vec![8, 1])),
            guides,
            put("genoffice-app://slides", thumbs, "180"),
            put("genoffice-app://docs", "aidocs.showAi", "true"),
            put("file://", "home.sidebarWidth", "300"),
            put("genoffice-app://help/", "a", "b"),
            dictionary,
            put("https://example.com", "tracker", "1"),
            put("http://localhost:5173", "dev", "1"),
        ];
        let log = Batch {
            first_sequence: 100,
            ops: vec![
                put("genoffice-app://slides", thumbs, "240"),
                (db_key("genoffice-app://docs", latin1("aidocs.showAi")), None),
                put("genoffice-app://pdf", "pdfapp.autoSave", "false"),
            ],
        };
        let default_store = user_data.join("Local Storage").join("leveldb");
        let default_fixture = Fixture {
            tables: vec![TableSpec {
                number: 5,
                compressed: true,
                entries: numbered(default_table),
            }],
            batches: vec![log],
            ..Fixture::default()
        };
        write_db(&default_store, &default_fixture);

        let top = "drawio-app://editor/^0genoffice-app://diagram";
        let diagram_table = vec![
            put("genoffice-app://diagram", "diagramapp.showAi", "false"),
            put("drawio-app://editor", "scratchpad", "[1]"),
            put(top, "scratchpad", "[2]"),
            put(top, "libs", "lib"),
            put("drawio-app://editor/^0https://evil.example", "libs", "evil"),
        ];
        let diagram_store = user_data
            .join("Partitions")
            .join("ieum-diagram")
            .join("Local Storage")
            .join("leveldb");
        let diagram_fixture = Fixture {
            tables: vec![TableSpec {
                number: 5,
                compressed: false,
                entries: numbered(diagram_table),
            }],
            ..Fixture::default()
        };
        write_db(&diagram_store, &diagram_fixture);
    }

    fn expected_origins(flavor: Flavor) -> Origins {
        let app = |host: &str| app_origin(flavor, host);
        Origins::from([
            (
                app("slides"),
                map(&[(SLIDES_KEY, "[1,2]"), ("ai-slides-thumbs-width", "240")]),
            ),
            (app("sheets"), map(&[("aisheets.spell-dictionary", "[\"안녕\"]")])),
            (app("shell"), map(&[("home.sidebarWidth", "300")])),
            (app("help"), map(&[("a", "b")])),
            (app("pdf"), map(&[("pdfapp.autoSave", "false")])),
            (app("diagram"), map(&[("diagramapp.showAi", "false")])),
            (
                drawio_origin(flavor),
                map(&[("scratchpad", "[1]"), ("libs", "lib")]),
            ),
        ])
    }

    fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (name, fs::read(&path).unwrap())
            })
            .collect()
    }

    #[test]
    fn the_live_entries_of_both_stores_are_imported_per_origin() {
        for flavor in [Flavor::Scheme, Flavor::LocalhostHttp] {
            let user_data = TempDir::new();
            electron_profile(user_data.path());
            let store = user_data.path().join("Local Storage").join("leveldb");
            let before = snapshot(&store);
            let imported = import(user_data.path(), flavor).unwrap();
            assert_eq!(imported, expected_origins(flavor), "{flavor:?}");
            assert_eq!(snapshot(&store), before, "the Electron store is untouched");
        }
    }

    #[test]
    fn the_cache_is_written_once_and_then_wins_over_the_stores() {
        let user_data = TempDir::new();
        electron_profile(user_data.path());
        let first = load_or_import(user_data.path(), Flavor::Scheme).unwrap();
        assert_eq!(first, expected_origins(Flavor::Scheme));

        let cache = user_data.path().join(CACHE_FILE);
        let written: Value = serde_json::from_str(&fs::read_to_string(&cache).unwrap()).unwrap();
        assert_eq!(written["version"], 1);
        let slides = &written["origins"]["genoffice-app-slides://localhost"];
        assert_eq!(slides[SLIDES_KEY], "[1,2]");
        assert!(!user_data.path().join("localstorage-import-v1.json.tmp").exists());

        fs::remove_dir_all(user_data.path().join("Local Storage")).unwrap();
        fs::remove_dir_all(user_data.path().join("Partitions")).unwrap();
        let second = load_or_import(user_data.path(), Flavor::Scheme).unwrap();
        assert_eq!(second, first);
    }

    #[test]
    fn missing_stores_are_an_empty_import_that_is_cached() {
        let user_data = TempDir::new();
        let imported = load_or_import(user_data.path(), Flavor::Scheme).unwrap();
        assert!(imported.is_empty());
        let text = fs::read_to_string(user_data.path().join(CACHE_FILE)).unwrap();
        let written: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(written, json!({ "version": 1, "origins": {} }));
    }

    #[test]
    fn a_store_that_cannot_be_read_writes_no_cache() {
        let user_data = TempDir::new();
        electron_profile(user_data.path());
        let store = user_data.path().join("Local Storage").join("leveldb");
        fs::remove_file(store.join("000005.ldb")).unwrap();
        let error = load_or_import(user_data.path(), Flavor::Scheme).unwrap_err();
        assert!(error.contains("000005"), "{error}");
        assert!(!user_data.path().join(CACHE_FILE).exists());
    }

    #[test]
    fn a_damaged_cache_is_ignored_and_rebuilt() {
        let user_data = TempDir::new();
        electron_profile(user_data.path());
        let cache = user_data.path().join(CACHE_FILE);
        for damaged in ["{", r#"{"version":2,"origins":{}}"#, r#"{"version":1}"#] {
            fs::write(&cache, damaged).unwrap();
            let imported = load_or_import(user_data.path(), Flavor::Scheme).unwrap();
            assert_eq!(imported, expected_origins(Flavor::Scheme), "{damaged}");
        }
    }

    fn sample_origins(flavor: Flavor) -> Origins {
        Origins::from([
            (app_origin(flavor, "slides"), map(&[("k", "SLIDES-ONLY")])),
            (app_origin(flavor, "docs"), map(&[("k", "DOCS-ONLY")])),
            (app_origin(flavor, "diagram"), map(&[("k", "DIAGRAM-ONLY")])),
            (app_origin(flavor, "shell"), map(&[("k", "SHELL-ONLY")])),
            (drawio_origin(flavor), map(&[("k", "DRAWIO-ONLY")])),
        ])
    }

    #[test]
    fn a_page_gets_only_the_script_of_its_own_origin() {
        let origins = sample_origins(Flavor::Scheme);
        let page = "genoffice-app://slides/index.html?x=1";
        let scripts = scripts_for(&origins, page, Flavor::Scheme);
        assert_eq!(scripts.len(), 1);
        assert!(!scripts[0].all_frames);
        assert!(scripts[0].source.contains("SLIDES-ONLY"));
        for other in ["DOCS-ONLY", "DIAGRAM-ONLY", "SHELL-ONLY", "DRAWIO-ONLY"] {
            assert!(!scripts[0].source.contains(other), "{other}");
        }

        let page = "genoffice-app://shell/index.html";
        let shell = scripts_for(&origins, page, Flavor::Scheme);
        assert_eq!(shell.len(), 1);
        assert!(shell[0].source.contains("SHELL-ONLY"));
    }

    #[test]
    fn the_diagram_page_also_gets_the_draw_io_script_for_all_frames() {
        let origins = sample_origins(Flavor::Scheme);
        let page = "genoffice-app://diagram/index.html";
        let scripts = scripts_for(&origins, page, Flavor::Scheme);
        assert_eq!(scripts.len(), 2);
        assert!(!scripts[0].all_frames);
        assert!(scripts[0].source.contains("DIAGRAM-ONLY"));
        assert!(!scripts[0].source.contains("DRAWIO-ONLY"));
        assert!(scripts[1].all_frames);
        assert!(scripts[1].source.contains("DRAWIO-ONLY"));
        assert!(!scripts[1].source.contains("DIAGRAM-ONLY"));
        assert!(scripts[1].source.contains(r#"location.protocol !== "drawio-app:""#));
        assert!(scripts[1].source.contains(r#"location.host !== "editor""#));
    }

    #[test]
    fn pages_without_an_import_get_no_script() {
        let origins = sample_origins(Flavor::Scheme);
        for page in [
            "about:blank",
            "https://example.com/",
            "genoffice-app://unknown/index.html",
            "genoffice-app://pdf/index.html",
            "not a url",
        ] {
            assert!(scripts_for(&origins, page, Flavor::Scheme).is_empty(), "{page}");
        }
        let page = "genoffice-app://slides/index.html";
        assert!(scripts_for(&Origins::new(), page, Flavor::Scheme).is_empty());
    }

    #[test]
    fn the_script_gates_on_the_exact_origin_and_never_overwrites() {
        let page = "genoffice-app://slides/index.html";
        let cases = [
            (Flavor::Scheme, "genoffice-app-slides:", "localhost"),
            (Flavor::LocalhostHttp, "http:", "genoffice-app-slides.localhost"),
        ];
        for (flavor, protocol, host) in cases {
            let scripts = scripts_for(&sample_origins(flavor), page, flavor);
            let source = &scripts[0].source;
            let gate = format!(
                r#"if (location.protocol !== "{protocol}" || location.host !== "{host}") return;"#
            );
            let flag = format!(r#"var flag = "{FLAG_KEY}";"#);
            let gate_at = source.find(&gate).expect("the origin gate");
            let flag_at = source.find(&flag).unwrap();
            let skip_at = source
                .find("if (storage.getItem(flag) !== null) return;")
                .unwrap();
            let missing_at = source
                .find("if (storage.getItem(entries[i][0]) === null) {")
                .unwrap();
            let set_at = source
                .find("storage.setItem(entries[i][0], entries[i][1]);")
                .unwrap();
            let done_at = source.find(r#"storage.setItem(flag, "1");"#).unwrap();
            assert!(gate_at < flag_at && flag_at < skip_at && skip_at < missing_at);
            assert!(missing_at < set_at && set_at < done_at);
            assert_eq!(source.matches("setItem").count(), 2);
        }
    }

    #[test]
    fn entries_are_embedded_as_pairs_that_cannot_end_the_script() {
        let hostile = "</script><script>alert(1)</script> \u{2028} \u{2029} \" \\ __FLAG__";
        let origins = Origins::from([(
            app_origin(Flavor::Scheme, "slides"),
            map(&[("__proto__", hostile), ("한글", "값")]),
        )]);
        let page = "genoffice-app://slides/index.html";
        let scripts = scripts_for(&origins, page, Flavor::Scheme);
        let source = &scripts[0].source;
        assert!(!source.contains("</script>"));
        assert!(!source.contains('\u{2028}') && !source.contains('\u{2029}'));
        assert!(source.contains(r"<\/script><script>alert(1)<\/script> \u2028 \u2029"));
        assert!(source.contains(r#"["__proto__","#));
        assert!(source.contains(r#"["한글","값"]"#));
        // the payload is never scanned for the placeholders
        assert!(source.contains("__FLAG__"));
    }
}
