//! OS video thumbnails, matching Electron's nativeImage thumbnail backend.
use shell_contract::runtime::Thread;
use std::path::Path;
use std::sync::Arc;

#[cfg(target_os = "macos")]
#[path = "video_thumbnail/macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "video_thumbnail/windows.rs"]
mod platform;

/// Call only on a worker. An absent/unreadable/unsupported input has no poster.
pub(crate) fn video_poster(path: &Path, ui: &Arc<dyn Thread>) -> Result<Option<Vec<u8>>, String> {
    if ui.is_current() {
        return Err("Video thumbnail cannot run on the UI thread".into());
    }
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => (),
        Ok(_) => return Ok(None),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::InvalidInput
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(format!("Inspecting video thumbnail input: {error}")),
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        platform::video_poster(path, ui)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = path;
        Err("Video thumbnail service is unavailable on this platform".into())
    }
}
