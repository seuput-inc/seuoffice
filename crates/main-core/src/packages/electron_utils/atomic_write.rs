//! `packages/electron-utils/src/atomic-write.ts`.
//!
//! The TypeScript's async functions are one synchronous function each here;
//! the order of the calls on the file system is the same. The retry sleeps
//! block the caller, as `writeJsonAtomic`'s do.

pub mod owned;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::time::Duration;

use jscompat::node_path::host::{basename, dirname, join};
use jscompat::{JsonValue, js_regex, json_stringify_pretty, random_uuid};

use crate::node_fs::{NodeError, code_of, node_error, node_error2};

/// Transient Windows codes: antivirus/indexer briefly locks the rename target.
const RETRYABLE_RENAME_CODES: &[&str] = &["EPERM", "EACCES", "EBUSY"];
const RENAME_RETRY_DELAYS_MS: [u64; 4] = [50, 100, 200, 400];

/// Codes a refused fsync may surface with inside cloud-sync folders, under AV
/// locks or on filesystems without fsync.
const TOLERATED_SYNC_CODES: &[&str] = &["EPERM", "EACCES", "EBUSY", "EINVAL", "ENOSYS", "ENOTSUP"];

/// `error.code ?? ''` of a Node error.
fn error_code(error: &NodeError) -> &'static str {
    error.code.unwrap_or("")
}

fn is_retryable_rename(error: &NodeError) -> bool {
    RETRYABLE_RENAME_CODES.contains(&error_code(error))
}

/// The code of a refused open or fsync. node_fs has no name for `ENOSYS` and
/// `ENOTSUP`, which the tolerated set holds.
fn sync_code(e: &io::Error) -> &'static str {
    if let Some(code) = code_of(e) {
        return code;
    }
    #[cfg(unix)]
    match e.raw_os_error() {
        Some(libc::ENOSYS) => return "ENOSYS",
        Some(libc::ENOTSUP) => return "ENOTSUP",
        _ => {}
    }
    ""
}

fn sleep_sync(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// Most bytes of the target's name a temp name carries. The temp name adds
/// `.` + 12 hex + `.tmp` (17 bytes) and Linux and macOS cap a file name
/// at 255 bytes.
const TEMP_NAME_MAX_BYTES: usize = 200;

/// `name` cut to at most `max_bytes` of UTF-8, never inside a character
fn name_head(name: &str, max_bytes: usize) -> &str {
    if name.len() <= max_bytes {
        return name;
    }
    let mut bytes = 0;
    for (at, ch) in name.char_indices() {
        bytes += ch.len_utf8();
        if bytes > max_bytes {
            return &name[..at];
        }
    }
    name
}

/// random bytes in a staging temp's name (written as twice as many hex digits)
const TEMP_RANDOM_BYTES: usize = 6;
const TEMP_EXT: &str = "tmp";
/// what the rescue copy carries where a staging temp has its random part
const RESCUE_MARK: &str = "unsaved";

/// Whether a file name is one a save can put beside a document: the staging
/// temp or the rescue copy.
pub fn is_save_temp_name(name: &str) -> bool {
    // `^.+\.(?:[0-9a-f]{TEMP_RANDOM_BYTES * 2}|RESCUE_MARK)\.TEMP_EXT$`, flag i
    js_regex!(r"^.+\.(?:[0-9a-f]{12}|unsaved)\.tmp$", "i").test(name)
}

/// a fresh `<name>.<12 hex>.tmp` path in the target's folder (a long name is cut short).
pub fn temp_path_beside(file_path: &str) -> String {
    // the first 12 digits of a UUID are 6 random bytes written as hex
    let random: String = random_uuid()
        .chars()
        .filter(|c| *c != '-')
        .take(TEMP_RANDOM_BYTES * 2)
        .collect();
    join(&[
        &dirname(file_path),
        &format!(
            "{}.{random}.{TEMP_EXT}",
            name_head(&basename(file_path, None), TEMP_NAME_MAX_BYTES)
        ),
    ])
}

/// Where a failed save parks the new bytes: beside the target, under ONE
/// stable name.
pub fn rescue_path_for(file_path: &str) -> String {
    join(&[
        &dirname(file_path),
        &format!(
            "{}.{RESCUE_MARK}.{TEMP_EXT}",
            name_head(&basename(file_path, None), TEMP_NAME_MAX_BYTES)
        ),
    ])
}

/// Park the new bytes beside the target, replacing the copy an earlier failed
/// attempt left. If the copy does not land, the staging temp still holds the
/// bytes.
fn keep_rescue_copy(file_path: &str, tmp: &str) {
    if crate::node_fs::copy_file(tmp, &rescue_path_for(file_path), false).is_err() {
        return;
    }
    let _ = fs::remove_file(tmp);
}

/// The save landed: an earlier attempt's rescue copy is stale.
fn clear_rescue_copy(file_path: &str) {
    let _ = fs::remove_file(rescue_path_for(file_path));
}

pub fn sync_file_best_effort(path: &str) -> Result<(), NodeError> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) => {
            if TOLERATED_SYNC_CODES.contains(&sync_code(&e)) {
                return Ok(());
            }
            return Err(node_error(&e, "open", path));
        }
    };
    match file.sync_all() {
        Ok(()) => Ok(()),
        Err(e) if TOLERATED_SYNC_CODES.contains(&sync_code(&e)) => Ok(()),
        Err(e) => Err(node_error(&e, "fsync", path)),
    }
}

fn rename_with_retry(tmp: &str, file_path: &str) -> Result<(), NodeError> {
    rename_with_retry_authorized(tmp, file_path, &mut |effect| { effect(); Ok(()) })?
}

pub use crate::node_fs::FileAdmission as WriteAdmission;
use crate::node_fs::admitted_file_effect as admitted;

fn rename_with_retry_authorized(
    tmp: &str,
    file_path: &str,
    admit: &mut WriteAdmission<'_>,
) -> Result<Result<(), NodeError>, NodeError> {
    retry_rename(&mut || {
        Ok(admitted(admit, || fs::rename(tmp, file_path))?.map_err(|error| node_error2(&error, "rename", tmp, file_path)))
    })
}

fn retry_rename(rename: &mut dyn FnMut() -> Result<Result<(), NodeError>, NodeError>) -> Result<Result<(), NodeError>, NodeError> {
    let mut attempt = 0;
    loop {
        match rename()? {
            Ok(()) => return Ok(Ok(())),
            Err(error) => {
                if !is_retryable_rename(&error) || attempt >= RENAME_RETRY_DELAYS_MS.len() {
                    return Ok(Err(error));
                }
                sleep_sync(RENAME_RETRY_DELAYS_MS[attempt]);
                attempt += 1;
            }
        }
    }
}

/// `writeFile(path, data)`
fn write_file(path: &str, data: &[u8]) -> Result<(), NodeError> {
    let mut file = fs::File::create(path).map_err(|e| node_error(&e, "open", path))?;
    file.write_all(data)
        .map_err(|e| node_error(&e, "write", path))
}

/// `copyFile(from, to)`
fn copy_file(from: &str, to: &str) -> Result<(), NodeError> {
    crate::node_fs::copy_file(from, to, false)
}

/// Same-dir temp file, best-effort fsync, then rename. Rename-over-existing fails
/// transiently on Windows (Defender/indexer locks: retry) and permanently on some
/// network mounts; either way the save falls back to an in-place write rather
/// than failing.
pub fn atomic_write_file(file_path: &str, data: &[u8]) -> Result<(), NodeError> {
    atomic_write_file_authorized(file_path, data, &mut |effect| { effect(); Ok(()) })
}

/// Native replacement and fallback-open admission are separate from bulk IO.
/// An admitted in-place descriptor may finish after revocation, as the original
/// path-based writer did; this is not a compare-and-swap filesystem guarantee.
pub fn atomic_write_file_authorized(
    file_path: &str,
    data: &[u8],
    admit: &mut WriteAdmission<'_>,
) -> Result<(), NodeError> {
    let mut backend = PathByteWriter { path: file_path, tmp: temp_path_beside(file_path), admit };
    atomic_write_file_with_backend(data, &mut backend)
}

pub trait AtomicByteWriteBackend {
    fn stage(&mut self, data: &[u8]) -> Result<(), NodeError>;
    fn rename(&mut self) -> Result<Result<(), NodeError>, NodeError>;
    fn write_fallback(&mut self, data: &[u8]) -> Result<Result<(), NodeError>, NodeError>;
    fn copy_stage_to_rescue(&mut self) -> Result<Result<(), NodeError>, NodeError>;
    fn clear_rescue(&mut self) -> Result<(), NodeError>;
    fn remove_stage(&mut self) -> Result<(), NodeError>;
}

pub trait AtomicJsonWriteBackend {
    fn ensure_parent(&mut self) -> Result<(), NodeError>;
    fn stage(&mut self, json: &str) -> Result<Result<(), NodeError>, NodeError>;
    fn rename(&mut self) -> Result<Result<(), NodeError>, NodeError>;
    fn open_fallback(&mut self) -> Result<Result<fs::File, NodeError>, NodeError>;
    fn remove_stage(&mut self) -> Result<(), NodeError>;
    fn write_error(&self, error: &io::Error) -> NodeError;
}

pub fn atomic_write_file_with_backend(data: &[u8], backend: &mut dyn AtomicByteWriteBackend) -> Result<(), NodeError> {
    let mut keep_tmp = false;
    let result = (|| {
        backend.stage(data)?;
        if retry_rename(&mut || backend.rename())?.is_ok() {
            backend.clear_rescue()?;
            return Ok(());
        }
        let written = backend.write_fallback(data)?;
        if let Err(error) = written {
            match backend.copy_stage_to_rescue()? {
                Ok(()) => backend.remove_stage()?,
                Err(_) => keep_tmp = true,
            }
            return Err(error);
        }
        backend.remove_stage()?;
        backend.clear_rescue()?;
        Ok(())
    })();
    if result.is_err() && !keep_tmp {
        backend.remove_stage()?;
    }
    result
}

struct PathByteWriter<'p, 'a, 'f> {
    path: &'p str,
    tmp: String,
    admit: &'a mut WriteAdmission<'f>,
}

impl AtomicByteWriteBackend for PathByteWriter<'_, '_, '_> {
    fn stage(&mut self, data: &[u8]) -> Result<(), NodeError> {
        write_file(&self.tmp, data).and_then(|()| sync_file_best_effort(&self.tmp))
    }
    fn rename(&mut self) -> Result<Result<(), NodeError>, NodeError> {
        Ok(admitted(self.admit, || fs::rename(&self.tmp, self.path))?.map_err(|error| node_error2(&error, "rename", &self.tmp, self.path)))
    }
    fn write_fallback(&mut self, data: &[u8]) -> Result<Result<(), NodeError>, NodeError> {
        let opened = admitted(self.admit, || fs::File::create(self.path))?;
        Ok(match opened {
            Ok(mut file) => file.write_all(data).map_err(|error| node_error(&error, "write", self.path)),
            Err(error) => Err(node_error(&error, "open", self.path)),
        })
    }
    fn copy_stage_to_rescue(&mut self) -> Result<Result<(), NodeError>, NodeError> {
        crate::node_fs::copy_file_authorized(&self.tmp, &rescue_path_for(self.path), false, self.admit)
    }
    fn clear_rescue(&mut self) -> Result<(), NodeError> {
        admitted(self.admit, || clear_rescue_copy(self.path))
    }
    fn remove_stage(&mut self) -> Result<(), NodeError> {
        let _ = fs::remove_file(&self.tmp);
        Ok(())
    }
}

struct PathJsonWriter<'p, 'a, 'f>(PathByteWriter<'p, 'a, 'f>);

impl AtomicJsonWriteBackend for PathJsonWriter<'_, '_, '_> {
    fn ensure_parent(&mut self) -> Result<(), NodeError> {
        let dir = dirname(self.0.path);
        admitted(self.0.admit, || fs::create_dir_all(&dir))?.map_err(|error| node_error(&error, "mkdir", &dir))
    }
    fn stage(&mut self, json: &str) -> Result<Result<(), NodeError>, NodeError> {
        Ok((|| {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&self.0.tmp)
                .map_err(|error| node_error(&error, "open", &self.0.tmp))?;
            file.write_all(json.as_bytes()).map_err(|error| node_error(&error, "write", &self.0.tmp))?;
            drop(file);
            sync_file_best_effort(&self.0.tmp)
        })())
    }
    fn rename(&mut self) -> Result<Result<(), NodeError>, NodeError> { self.0.rename() }
    fn open_fallback(&mut self) -> Result<Result<fs::File, NodeError>, NodeError> {
        Ok(admitted(self.0.admit, || fs::File::create(self.0.path))?.map_err(|error| node_error(&error, "open", self.0.path)))
    }
    fn remove_stage(&mut self) -> Result<(), NodeError> { self.0.remove_stage() }
    fn write_error(&self, error: &io::Error) -> NodeError { node_error(error, "write", self.0.path) }
}

/// Stage and sync bytes before delegating only the final replacement.
///
/// The caller owns the authorization fence around the rename. A refused or
/// failed replacement removes the temporary file and leaves the target alone.
pub fn atomic_write_guarded<E: From<NodeError>>(
    file_path: &str,
    data: &[u8],
    commit: impl FnOnce(&str, &str) -> Result<(), E>,
) -> Result<(), E> {
    let tmp = temp_path_beside(file_path);
    let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)
        .map_err(|error| E::from(node_error(&error, "open", &tmp)))?;
    let staged = file.write_all(data).map_err(|error| node_error(&error, "write", &tmp));
    drop(file);
    let staged = staged.and_then(|()| sync_file_best_effort(&tmp));
    if let Err(error) = staged {
        let _ = fs::remove_file(&tmp);
        return Err(error.into());
    }
    let result = commit(&tmp, file_path);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    } else {
        clear_rescue_copy(file_path);
    }
    result
}

/// copyFile stays in the kernel, so a multi-hundred-MB file is never read into RAM
pub fn atomic_copy_file(source: &str, file_path: &str) -> Result<(), NodeError> {
    atomic_copy_file_authorized(source, file_path, &mut |effect| { effect(); Ok(()) })
}

pub fn atomic_copy_file_authorized(source: &str, file_path: &str, admit: &mut WriteAdmission<'_>) -> Result<(), NodeError> {
    let tmp = temp_path_beside(file_path);
    if let Err(error) = copy_file(source, &tmp).and_then(|()| sync_file_best_effort(&tmp)) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    let renamed = match rename_with_retry_authorized(&tmp, file_path, admit) {
        Ok(result) => result,
        Err(error) => { let _ = fs::remove_file(&tmp); return Err(error); }
    };
    if renamed.is_ok() {
        admitted(admit, || clear_rescue_copy(file_path))?;
        return Ok(());
    }
    let copied = match crate::node_fs::copy_file_authorized(source, file_path, false, admit) {
        Ok(result) => result,
        Err(error) => { let _ = fs::remove_file(&tmp); return Err(error); }
    };
    if let Err(error) = copied {
        let rescued = match crate::node_fs::copy_file_authorized(&tmp, &rescue_path_for(file_path), false, admit) {
            Ok(result) => result,
            Err(error) => { let _ = fs::remove_file(&tmp); return Err(error); }
        };
        if rescued.is_ok() { let _ = fs::remove_file(&tmp); }
        return Err(error);
    }
    let _ = fs::remove_file(&tmp);
    admitted(admit, || clear_rescue_copy(file_path))?;
    Ok(())
}

/// Put a finished temp file (one a caller streamed to `temp_path_beside(file_path)`)
/// in place of `file_path`, with `atomic_write_file`'s flush, rename retry and
/// in-place fallback. The temp file is gone afterwards, whether this returns
/// `Ok` or `Err`, except when the in-place fallback itself fails.
pub fn atomic_move_file(temp_path: &str, file_path: &str) -> Result<(), NodeError> {
    atomic_move_file_authorized(temp_path, file_path, &mut |effect| { effect(); Ok(()) })
}

pub fn atomic_move_file_authorized(temp_path: &str, file_path: &str, admit: &mut WriteAdmission<'_>) -> Result<(), NodeError> {
    if let Err(error) = sync_file_best_effort(temp_path) {
        let _ = fs::remove_file(temp_path);
        return Err(error);
    }
    if rename_with_retry_authorized(temp_path, file_path, admit)?.is_ok() {
        return Ok(());
    }
    // fall through to the in-place write
    crate::node_fs::copy_file_authorized(temp_path, file_path, false, admit)??;
    let _ = fs::remove_file(temp_path);
    Ok(())
}

/// The same contract for output that is produced by a writer: `write` fills
/// the sibling temp path, and only a temp that `write` completed is synced
/// and renamed over the target. A writer that fails part-way leaves the
/// target untouched.
pub fn atomic_write_via(
    file_path: &str,
    write: &mut dyn FnMut(&str) -> Result<(), NodeError>,
) -> Result<(), NodeError> {
    let tmp = temp_path_beside(file_path);
    if let Err(error) = write(&tmp).and_then(|()| sync_file_best_effort(&tmp)) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    if rename_with_retry(&tmp, file_path).is_ok() {
        clear_rescue_copy(file_path);
        return Ok(());
    }
    // fall through to the in-place copy: keep the completed temp until it lands
    if let Err(error) = copy_file(&tmp, file_path) {
        keep_rescue_copy(file_path, &tmp);
        return Err(error);
    }
    let _ = fs::remove_file(&tmp);
    clear_rescue_copy(file_path);
    Ok(())
}

/// Atomic replacement for small userData JSON state files (recents, stars, AI
/// settings): `JSON.stringify(value, null, 2)`. Same retry + in-place
/// fallback as `atomic_write_file`; the retry sleeps block the caller.
pub fn write_json_atomic(file_path: &str, value: &JsonValue) -> Result<(), NodeError> {
    write_json_atomic_authorized(file_path, value, &mut |effect| { effect(); Ok(()) })
}

pub fn write_json_atomic_authorized(file_path: &str, value: &JsonValue, admit: &mut WriteAdmission<'_>) -> Result<(), NodeError> {
    let mut backend = PathJsonWriter(PathByteWriter { path: file_path, tmp: temp_path_beside(file_path), admit });
    write_json_atomic_with_backend(value, &mut backend)
}

pub fn write_json_atomic_with_backend(value: &JsonValue, backend: &mut dyn AtomicJsonWriteBackend) -> Result<(), NodeError> {
    backend.ensure_parent()?;
    let json = json_stringify_pretty(value, 2);
    let published = match backend.stage(&json) {
        Ok(Ok(())) => retry_rename(&mut || backend.rename()),
        failed => failed,
    };
    match published {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) if is_retryable_rename(&error) => {
            let opened = match backend.open_fallback() {
                Ok(result) => result,
                Err(error) => { backend.remove_stage()?; return Err(error); }
            };
            let mut file = opened?;
            file.write_all(json.as_bytes()).map_err(|error| backend.write_error(&error))?;
            backend.remove_stage()?;
            Ok(())
        }
        Ok(Err(error)) | Err(error) => {
            backend.remove_stage()?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod guarded_tests {
    use super::*;

    #[test]
    fn refused_or_failed_replacement_keeps_target_and_removes_staging() {
        let dir = std::env::temp_dir().join(format!("guarded-write-{}", random_uuid()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("document.pdf");
        fs::write(&target, b"old").unwrap();
        let target = target.to_str().unwrap();
        for refusal in [true, false] {
            let result = atomic_write_guarded::<NodeError>(target, b"signed", |tmp, dest| {
                if refusal {
                    Err(node_error(&io::Error::new(io::ErrorKind::PermissionDenied, "revoked"), "rename", dest))
                } else {
                    fs::rename(tmp, dir.join("missing").join("target"))
                        .map_err(|error| node_error2(&error, "rename", tmp, dest))
                }
            });
            assert!(result.is_err());
            assert_eq!(fs::read(target).unwrap(), b"old");
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        }
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod authorized_tests {
    use super::*;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("owned-atomic-{}", jscompat::random_uuid()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn target(&self) -> String { self.0.join("document.bin").to_string_lossy().into_owned() }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn refused_byte_publication_never_enters_fallback_or_rescue() {
        let fixture = Fixture::new();
        let target = fixture.target();
        fs::write(&target, b"before").unwrap();
        let mut attempts = 0;
        let error = atomic_write_file_authorized(&target, b"after", &mut |_| {
            attempts += 1;
            Err(NodeError { code: Some("EACCES"), message: "request owner was revoked".into() })
        }).unwrap_err();
        assert_eq!(attempts, 1);
        assert_eq!(error.message, "request owner was revoked");
        assert_eq!(fs::read(&target).unwrap(), b"before");
        assert!(!std::path::Path::new(&rescue_path_for(&target)).exists());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn json_rename_admission_failure_is_not_a_retryable_native_failure() {
        let fixture = Fixture::new();
        let target = fixture.target();
        fs::write(&target, b"before").unwrap();
        let mut calls = 0;
        let error = write_json_atomic_authorized(&target, &JsonValue::Null, &mut |effect| {
            calls += 1;
            if calls == 1 { effect(); Ok(()) }
            else { Err(NodeError { code: Some("EACCES"), message: "no replacement owner".into() }) }
        }).unwrap_err();
        assert_eq!(calls, 2);
        assert_eq!(error.message, "no replacement owner");
        assert_eq!(fs::read(&target).unwrap(), b"before");
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn original_copy_protects_a_source_hardlinked_as_its_destination() {
        let fixture = Fixture::new();
        let source = fixture.0.join("source.bin");
        let target = fixture.target();
        fs::write(&source, b"same inode").unwrap();
        fs::hard_link(&source, &target).unwrap();
        crate::node_fs::copy_file_authorized(&source.to_string_lossy(), &target, false, &mut |effect| { effect(); Ok(()) }).unwrap().unwrap();
        assert_eq!(fs::read(source).unwrap(), b"same inode");
        assert_eq!(fs::read(target).unwrap(), b"same inode");
    }

    #[cfg(unix)]
    #[test]
    fn real_failed_target_write_preserves_the_original_rescue_bytes() {
        let fixture = Fixture::new();
        let target = fixture.target();
        fs::create_dir(&target).unwrap();
        let error = atomic_write_file_authorized(&target, b"not lost", &mut |effect| { effect(); Ok(()) }).unwrap_err();
        assert_eq!(error.code, Some("EISDIR"));
        assert_eq!(fs::read(rescue_path_for(&target)).unwrap(), b"not lost");
        assert!(std::path::Path::new(&target).is_dir());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
    }
}
