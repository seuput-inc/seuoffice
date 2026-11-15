//! `apps/shell/src/main/atomic-write.ts`: the shell's own writer, not the
//! electron-utils helper of the same file name. That one falls back to an
//! in-place write; this one leaves the destination untouched on failure.
//!
//! The TypeScript's async functions are one synchronous function each; the
//! order of the calls on the file system is the same and the retry sleeps
//! block the caller.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::time::Duration;

use jscompat::node_path::host::dirname;

use crate::node_fs::{NodeError, code_of, node_error, node_error2};
use crate::packages::electron_utils::atomic_write::temp_path_beside;

pub use crate::packages::electron_utils::atomic_write::atomic_copy_file;

const RETRYABLE_RENAME_CODES: &[&str] = &["EPERM", "EACCES", "EBUSY"];
// cloud-sync folders, AV locks and fsync-less filesystems refuse the flush after a
// successful write; that only weakens power-loss durability, so the save proceeds
const TOLERATED_SYNC_CODES: &[&str] = &["EPERM", "EACCES", "EBUSY", "EINVAL", "ENOSYS"];
const RENAME_RETRIES: u32 = 4;

/// `error.code ?? ''` of a refused open or fsync. node_fs has no name for
/// `ENOSYS`, which the tolerated set holds.
fn sync_code(e: &io::Error) -> &'static str {
    if let Some(code) = code_of(e) {
        return code;
    }
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ENOSYS) {
        return "ENOSYS";
    }
    ""
}

/// `atomicWriteFile(filePath, data)`
pub fn atomic_write_file(file_path: &str, data: &[u8]) -> Result<(), NodeError> {
    atomic_write_file_authorized(file_path, data, &mut |rename| Ok(rename()))
}

/// Staging, durability flushes and retry delays run outside admission.
pub fn atomic_write_file_authorized(
    file_path: &str,
    data: &[u8],
    admit: &mut dyn FnMut(&mut dyn FnMut() -> io::Result<()>) -> Result<io::Result<()>, NodeError>,
) -> Result<(), NodeError> {
    via_temp(
        file_path,
        &mut |tmp| {
            // writeFile(tmp, data)
            let mut file = fs::File::create(tmp).map_err(|e| node_error(&e, "open", tmp))?;
            file.write_all(data)
                .map_err(|e| node_error(&e, "write", tmp))
        },
        admit,
    )
}

/// `viaTemp(filePath, fill)`
fn via_temp(
    file_path: &str,
    fill: &mut dyn FnMut(&str) -> Result<(), NodeError>,
    admit: &mut dyn FnMut(&mut dyn FnMut() -> io::Result<()>) -> Result<io::Result<()>, NodeError>,
) -> Result<(), NodeError> {
    let tmp = temp_path_beside(file_path);
    let staged = (|| -> Result<(), NodeError> {
        fill(&tmp)?;
        fsync_path(&tmp, false)?;
        let mut attempt: u32 = 0;
        loop {
            match admit(&mut || fs::rename(&tmp, file_path))? {
                Ok(()) => {
                    // the rename is only durable once the directory entry is flushed; opening
                    // a directory fails on Windows, where NTFS journals the rename itself
                    if cfg!(not(windows)) {
                        fsync_path(&dirname(file_path), true)?;
                    }
                    return Ok(());
                }
                Err(e) => {
                    let error = node_error2(&e, "rename", &tmp, file_path);
                    let code = error.code.unwrap_or("");
                    if !RETRYABLE_RENAME_CODES.contains(&code) || attempt >= RENAME_RETRIES {
                        return Err(error);
                    }
                    std::thread::sleep(Duration::from_millis(50 * 2u64.pow(attempt)));
                    attempt += 1;
                }
            }
        }
    })();
    if staged.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    staged
}

/// `fsyncPath(path, tolerateAll)`
fn fsync_path(path: &str, tolerate_all: bool) -> Result<(), NodeError> {
    let tolerated = |e: &io::Error| tolerate_all || TOLERATED_SYNC_CODES.contains(&sync_code(e));
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(e) => {
            if tolerated(&e) {
                return Ok(());
            }
            return Err(node_error(&e, "open", path));
        }
    };
    match file.sync_all() {
        Ok(()) => Ok(()),
        Err(e) if tolerated(&e) => Ok(()),
        Err(e) => Err(node_error(&e, "fsync", path)),
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("atomic-admission-{}", jscompat::random_uuid()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn target(&self) -> String {
            self.0.join("document.txt").to_string_lossy().into_owned()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn refused_commit_keeps_destination_and_removes_staged_bytes() {
        let fixture = Fixture::new();
        let target = fixture.target();
        fs::write(&target, b"before").unwrap();
        let error = atomic_write_file_authorized(&target, b"after", &mut |_| {
            assert_eq!(fs::read(&target).unwrap(), b"before");
            assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
            Err(NodeError {
                code: None,
                message: "owner was revoked".into(),
            })
        })
        .unwrap_err();
        assert!(error.message.contains("owner was revoked"));
        assert_eq!(fs::read(&target).unwrap(), b"before");
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn admitted_commit_preserves_original_overwrite_behavior() {
        let fixture = Fixture::new();
        let target = fixture.target();
        fs::write(&target, b"before").unwrap();
        let mut commits = 0;
        atomic_write_file_authorized(&target, b"after", &mut |rename| {
            commits += 1;
            assert_eq!(fs::read(&target).unwrap(), b"before");
            Ok(rename())
        })
        .unwrap();
        assert_eq!(commits, 1);
        assert_eq!(fs::read(&target).unwrap(), b"after");
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn authority_is_checked_again_after_a_retryable_rename_failure() {
        let fixture = Fixture::new();
        let target = fixture.target();
        fs::write(&target, b"before").unwrap();
        let mut attempts = 0;
        let error = atomic_write_file_authorized(&target, b"after", &mut |_| {
            attempts += 1;
            if attempts == 1 {
                Ok(Err(io::Error::from_raw_os_error(libc::EACCES)))
            } else {
                Err(NodeError {
                    code: None,
                    message: "owner was revoked before retry".into(),
                })
            }
        })
        .unwrap_err();
        assert_eq!(attempts, 2);
        assert_eq!(error.message, "owner was revoked before retry");
        assert_eq!(fs::read(&target).unwrap(), b"before");
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }
}
