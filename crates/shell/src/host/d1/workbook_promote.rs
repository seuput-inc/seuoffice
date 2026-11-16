//! `promoteFileAtomically` from packages/xlsx-gateway/src/gateway/xlsx-package-io.ts.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shell_ipc_d1::sheets::engine::{EngineResult, engine_error};

fn retryable(error: &io::Error) -> bool {
    matches!(
        main_core::node_fs::node_error(error, "rename", "").code,
        Some("EPERM" | "EACCES" | "EBUSY")
    )
}

fn sync_tolerated(error: &io::Error) -> bool {
    matches!(
        main_core::node_fs::node_error(error, "fsync", "").code,
        Some("EPERM" | "EACCES" | "EBUSY" | "EINVAL" | "ENOSYS")
    )
}

fn recovery_name(target: &Path, tag: &str) -> String {
    let name = target.file_stem().unwrap_or_default().to_string_lossy();
    let extension = target
        .extension()
        .map(|value| format!(".{}", value.to_string_lossy()))
        .unwrap_or_default();
    format!("{name}.recovered-{tag}{extension}")
}

fn preserve_backup(backup: &Path, target: &Path) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let recovered = target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(recovery_name(target, &now.to_string()));
    if std::fs::copy(backup, &recovered).is_err() {
        return backup.to_owned();
    }
    let _ = std::fs::remove_file(backup);
    recovered
}

pub(super) fn promote(temporary: &str, target: &str) -> EngineResult<()> {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(temporary)
    {
        Ok(file) => {
            if let Err(error) = file.sync_all()
                && !sync_tolerated(&error)
            {
                    return Err(engine_error(
                        main_core::node_fs::node_error(&error, "fsync", temporary).message,
                    ));
                }
        }
        Err(error) if sync_tolerated(&error) => {}
        Err(error) => {
            return Err(engine_error(
                main_core::node_fs::node_error(&error, "open", temporary).message,
            ));
        }
    }
    for attempt in 0..=4 {
        match std::fs::rename(temporary, target) {
            Ok(()) => return Ok(()),
            Err(error) if !retryable(&error) => {
                return Err(engine_error(
                    main_core::node_fs::node_error2(&error, "rename", temporary, target).message,
                ));
            }
            Err(_) if attempt < 4 => std::thread::sleep(Duration::from_millis(50 << attempt)),
            Err(_) => break,
        }
    }
    let before = match std::fs::metadata(target) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(engine_error(
                main_core::node_fs::node_error(&error, "stat", target).message,
            ));
        }
    };
    let backup = before.as_ref().map(|_| {
        std::env::temp_dir().join(recovery_name(Path::new(target), &jscompat::random_uuid()))
    });
    if let Some(backup) = &backup
        && let Err(error) = std::fs::copy(target, backup)
    {
            return Err(engine_error(if retryable(&error) {
                format!("The save target is locked by another program: {target}")
            } else {
                main_core::node_fs::node_error2(
                    &error,
                    "copyfile",
                    target,
                    &backup.to_string_lossy(),
                )
                .message
            }));
        }
    if let Err(error) = std::fs::copy(temporary, target) {
        if let (Some(backup), Some(before)) = (&backup, &before) {
            let untouched = std::fs::metadata(target).ok().is_some_and(|after| {
                after.len() == before.len()
                    && matches!((after.modified(), before.modified()), (Ok(a), Ok(b)) if a == b)
            });
            if !untouched && std::fs::copy(backup, target).is_err() {
                let survivor = preserve_backup(backup, Path::new(target));
                return Err(engine_error(format!(
                    "The save target {target} could not be restored after a failed save; the previous workbook contents were preserved at: {}",
                    survivor.display()
                )));
            }
            let _ = std::fs::remove_file(backup);
        }
        return Err(engine_error(if retryable(&error) {
            format!("The save target is locked by another program: {target}")
        } else {
            main_core::node_fs::node_error2(&error, "copyfile", temporary, target).message
        }));
    }
    if let Some(backup) = backup {
        let _ = std::fs::remove_file(backup);
    }
    let _ = std::fs::remove_file(temporary);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promotion_replaces_target_and_consumes_staged_file() {
        let dir =
            std::env::temp_dir().join(format!("workbook-promotion-{}", jscompat::random_uuid()));
        std::fs::create_dir(&dir).expect("create isolated directory");
        let target = dir.join("report.xlsx");
        let staged = dir.join("staged.xlsx");
        std::fs::write(&target, b"old workbook").expect("write target");
        std::fs::write(&staged, b"new workbook").expect("write staged workbook");
        promote(staged.to_str().unwrap(), target.to_str().unwrap()).expect("promote workbook");
        assert_eq!(std::fs::read(&target).unwrap(), b"new workbook");
        assert!(!staged.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
