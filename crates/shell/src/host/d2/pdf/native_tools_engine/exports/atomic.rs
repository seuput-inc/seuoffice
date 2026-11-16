//! Shell export atomic-write contract: never fall back to an in-place overwrite.
use shell_contract::runtime::CancelToken;
use shell_ipc_d2::pdf::tools::JobFailure;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn cancelled(cancel: &CancelToken) -> Result<(), JobFailure> {
    if cancel.is_cancelled() {
        Err(JobFailure::new("cancelled", None))
    } else {
        Ok(())
    }
}
fn sync(path: &Path, tolerate_all: bool) -> Result<(), JobFailure> {
    let failure = |error: std::io::Error, syscall: &str| -> Result<(), JobFailure> {
        let code = main_core::node_fs::code_of(&error);
        if tolerate_all
            || matches!(
                code,
                Some("EPERM" | "EACCES" | "EBUSY" | "EINVAL" | "ENOSYS")
            )
        {
            return Ok(());
        }
        Err(JobFailure::new(
            "failed",
            Some(&main_core::node_fs::node_error(&error, syscall, &path.to_string_lossy()).message),
        ))
    };
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return failure(error, "open"),
    };
    if let Err(error) = file.sync_all() {
        failure(error, "fsync")?;
    }
    Ok(())
}
pub(super) fn write(path: &str, bytes: &[u8], cancel: &CancelToken) -> Result<(), JobFailure> {
    cancelled(cancel)?;
    let temp = Temporary(PathBuf::from(
        main_core::packages::electron_utils::atomic_write::temp_path_beside(path),
    ));
    let mut file = fs::File::create(&temp.0).map_err(|e| {
        JobFailure::new(
            "failed",
            Some(&main_core::node_fs::node_error(&e, "open", &temp.0.to_string_lossy()).message),
        )
    })?;
    file.write_all(bytes).map_err(|e| {
        JobFailure::new(
            "failed",
            Some(&main_core::node_fs::node_error(&e, "write", &temp.0.to_string_lossy()).message),
        )
    })?;
    drop(file);
    sync(&temp.0, false)?;
    for attempt in 0..=4 {
        cancelled(cancel)?;
        match fs::rename(&temp.0, path) {
            Ok(()) => {
                #[cfg(not(windows))]
                sync(
                    Path::new(path)
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new(".")),
                    true,
                )?;
                return Ok(());
            }
            Err(error) => {
                if attempt == 4
                    || !matches!(
                        main_core::node_fs::code_of(&error),
                        Some("EPERM" | "EACCES" | "EBUSY")
                    )
                {
                    return Err(JobFailure::new(
                        "failed",
                        Some(
                            &main_core::node_fs::node_error2(
                                &error,
                                "rename",
                                &temp.0.to_string_lossy(),
                                path,
                            )
                            .message,
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50 << attempt));
            }
        }
    }
    Err(JobFailure::new("failed", Some("atomic rename failed")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_before_creating_temporary_file() {
        let token = CancelToken::default();
        token.cancel();
        assert_eq!(
            write("/missing/export", b"data", &token).unwrap_err().error,
            "cancelled"
        );
    }
}
