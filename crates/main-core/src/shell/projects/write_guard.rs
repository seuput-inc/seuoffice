//! `apps/shell/src/main/projects/write-guard.ts`: the single write guard for
//! project folders (spec 3, invariants 3 and 9).
//!
//! The app writes into a person's folder in two places only: under the
//! project's data folder (`dataDirName`), and a new document the AI made
//! inside the project folder (never under the data folder, by any name it has
//! had). Every such write goes through this type, which checks each time that
//! the project root is still its canonical path, that the target stays below
//! it (no `..`, no drive or stream names), and that no folder on the way nor
//! the target itself is a link or junction. JSON and Markdown are written to a
//! temporary file and renamed. The data folder and its subfolders are created
//! only by an actual write.
//!
//! A policy refusal is `ProjectWriteRefused`; a file system failure is the
//! Node error of the call that failed (`code` and `message` as Node gives
//! them). Neither is swallowed.
//!
//! The TypeScript awaits `node:fs/promises` and queues the writes to one file
//! so they run one at a time in arrival order; here every call runs to its
//! end before the next one starts, which is that order, so there is no queue.
//! A caller that shares the guard between threads runs the writes to one file
//! one at a time itself.

use std::fs::{self, Metadata, OpenOptions};
use std::io::Write;

use jscompat::js_regex;
use jscompat::node_path::host::{self, join};
use jscompat::node_path::{posix, win32};
use serde::{Deserialize, Serialize};

use super::folder_files::{data_dir_name, is_data_dir_name, link_count};
use super::paths::{is_plain_segment, path_api, relative_inside, same_path};
use crate::node_fs::{NodeError, namespaced, node_error, node_error_fd, node_error2};
use crate::packages::electron_utils::atomic_write::temp_path_beside;
use crate::packages::project_store::nfc;
use crate::shell::atomic_write::atomic_write_file;
use crate::shell::rename_validation::is_valid_rename_name;


mod new_document;
#[cfg(unix)]
mod cleanup;
#[cfg(unix)]
pub use cleanup::ProjectCleanup;
fn run_authorized<T>(
    authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    action: impl FnOnce() -> Result<T, NodeError>,
) -> Result<T, NodeError> {
    let mut action = Some(action);
    let mut outcome = None;
    let guarded = authorize(&mut || {
        let action = action.take().ok_or_else(|| NodeError { code: None, message: "project publish effect repeated".into() })?;
        let result = action();
        let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
        outcome = Some(result);
        status
    });
    match (guarded, outcome) {
        (Ok(()), Some(result)) => result,
        (Err(_), Some(Err(actual))) => Err(actual),
        (Err(error), _) => Err(error),
        (Ok(()), None) => Err(NodeError { code: None, message: "project owner guard did not run".into() }),
    }
}
#[cfg(windows)]
mod staging_windows;
#[cfg(unix)]
mod data_unix;
#[cfg(any(unix, windows))]
mod scoped;
#[cfg(any(unix, windows))]
pub use scoped::ChatsDirectory;

#[cfg(windows)]
fn stage_authorized_data(
    project: &fs::File, data: &fs::File, root: &str, data_path: &str, name: &str, bytes: &[u8],
    authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
) -> Result<staging_windows::OwnedMetadataStage, NodeError> {
    let mut staged = None;
    let result = run_authorized(authorize, || {
        if !staging_windows::handle_matches_path(project, root)?
            || !staging_windows::handle_matches_path(data, data_path)? {
            return Err(NodeError { code: None, message: "project data folder changed".into() });
        }
        staged = Some(staging_windows::create_project_data_stage(data, root, name)?);
        Ok(())
    });
    if let Err(mut error) = result {
        if let Some(stage) = staged {
            if let Err(cleanup) = stage.cancel() {
                error.message = format!("{}; unpublished data cleanup failed: {}", error.message, cleanup.message);
            }
        }
        return Err(error);
    }
    let mut stage = staged.ok_or_else(|| NodeError { code: None, message: "project data staging did not run".into() })?;
    if let Err(mut error) = staging_windows::write_project_data_stage(&mut stage, bytes) {
        if let Err(cleanup) = stage.cancel() {
            error.message = format!("{}; unpublished data cleanup failed: {}", error.message, cleanup.message);
        }
        return Err(error);
    }
    Ok(stage)
}

#[cfg(unix)]
fn open_stage_dir(path: &str) -> Result<fs::File, NodeError> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(namespaced(path)).map_err(|error| node_error(&error, "open", path))
}

#[cfg(unix)]
fn create_private_stage_dir(path: &str, parent: &fs::File, name: &str) -> Result<fs::File, NodeError> {
    use std::os::unix::fs::MetadataExt;
    let file = create_stage_child(parent, name, path, 0o700)?;
    let metadata = file.metadata().map_err(|error| node_error(&error, "fstat", path))?;
    // SAFETY: geteuid has no arguments or side effects.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(NodeError { code: None, message: format!("private staging permissions or owner changed: {path}") });
    }
    Ok(file)
}

#[cfg(unix)]
fn create_stage_child(parent: &fs::File, name: &str, path: &str, mode: libc::mode_t) -> Result<fs::File, NodeError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name).map_err(|_| NodeError { code: None, message: "staging name contains NUL".into() })?;
    // SAFETY: the parent descriptor and C string remain live throughout both calls.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) } != 0 {
        return Err(node_error(&std::io::Error::last_os_error(), "mkdir", path));
    }
    // SAFETY: openat returns a new owned descriptor on success.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 { return Err(node_error(&std::io::Error::last_os_error(), "open", path)); }
    // SAFETY: fd was returned by openat and has not been transferred.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn create_project_stage_dir(path: &str, _parent: &fs::File, private: &fs::File) -> Result<fs::File, NodeError> {
    create_stage_child(private, "project", path, 0o777)
}

#[cfg(unix)]
fn write_staged_project(staged: &fs::File, path: &str, bytes: &[u8],
    authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
) -> Result<(), NodeError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let (data, mut file) = run_authorized(authorize, || {
        let data = create_stage_child(staged, crate::packages::project_store::project_folder_name("dataDir"), path, 0o777)?;
        let filename = std::ffi::CString::new(crate::packages::project_store::project_folder_name("projectFile")).expect("static project filename");
        // SAFETY: data/name are live and openat creates a newly owned descriptor.
        let fd = unsafe { libc::openat(data.as_raw_fd(), filename.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o666) };
        if fd < 0 { return Err(node_error(&std::io::Error::last_os_error(), "open", path)); }
        // SAFETY: fd came from successful openat and has not been transferred.
        let file = unsafe { fs::File::from_raw_fd(fd) };
        Ok((data, file))
    })?;
    file.write_all(bytes).map_err(|error| node_error(&error, "write", path))?;
    if let Err(error) = file.sync_all() {
        let tolerated = matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES | libc::EBUSY | libc::EINVAL | libc::ENOSYS));
        if !tolerated {
            return Err(node_error(&error, "fsync", path));
        }
    }
    let _ = data.sync_all();
    let _ = staged.sync_all();
    Ok(())
}

#[cfg(unix)]
fn stage_handle_matches(handle: &fs::File, path: &str) -> Result<bool, NodeError> {
    use std::os::unix::fs::MetadataExt;
    if nfc(&realpath(path)?) != nfc(path) { return Ok(false); }
    let held = handle.metadata().map_err(|error| node_error(&error, "fstat", path))?;
    let current = fs::symlink_metadata(namespaced(path)).map_err(|error| node_error(&error, "lstat", path))?;
    Ok(!current.file_type().is_symlink() && held.dev() == current.dev() && held.ino() == current.ino())
}

#[cfg(unix)]
fn publish_staged_directory(
    private: &fs::File, staged: &fs::File, parent: &fs::File, name: &str, from: &str, to: &str,
) -> Result<(), NodeError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let source = c"project";
    let target = std::ffi::CString::new(name).map_err(|_| NodeError { code: None, message: "project name contains NUL".into() })?;
    let held = staged.metadata().map_err(|error| node_error(&error, "fstat", from))?;
    let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: descriptors and source string are live; fstatat initializes the output on success.
    if unsafe { libc::fstatat(private.as_raw_fd(), source.as_ptr(), current.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(node_error(&std::io::Error::last_os_error(), "lstat", from));
    }
    // SAFETY: fstatat succeeded and initialized current.
    let current = unsafe { current.assume_init() };
    if current.st_dev as u64 != held.dev() || current.st_ino as u64 != held.ino() || (current.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        return Err(NodeError { code: None, message: "staged project identity changed".into() });
    }
    #[cfg(target_os = "macos")]
    // SAFETY: both directory descriptors and C strings are live.
    let result = unsafe { libc::renameatx_np(private.as_raw_fd(), source.as_ptr(), parent.as_raw_fd(), target.as_ptr(), libc::RENAME_EXCL) } as i64;
    #[cfg(target_os = "linux")]
    // SAFETY: both directory descriptors and C strings are live.
    let result = unsafe { libc::syscall(libc::SYS_renameat2, private.as_raw_fd(), source.as_ptr(), parent.as_raw_fd(), target.as_ptr(), libc::RENAME_NOREPLACE) } as i64;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result = return Err(NodeError { code: None, message: "exclusive directory publication is unavailable on this platform".into() });
    if result == 0 { Ok(()) } else { Err(node_error2(&std::io::Error::last_os_error(), "rename", from, to)) }
}


/// `ProjectWriteRefusal`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectWriteRefusal {
    /// The relative path is empty, absolute, or holds `..`, `.`, a stream or control characters
    BadPath,
    /// The target is not below the project folder (or, for a new document, inside the data folder)
    Outside,
    /// A folder on the way, or the target, is a link or junction
    Link,
    /// Something that should be a folder is a file, or the target is a folder
    NotAFolder,
    /// The project folder no longer resolves to the path it was registered under
    RootMoved,
    /// A new document or new project folder would replace an existing entry
    Exists,
    /// A new document's folder does not exist (new documents never create folders)
    MissingFolder,
    /// A hard-linked file would be appended to through its other name
    HardLink,
}

impl ProjectWriteRefusal {
    /// the text the message and `reason` carry
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadPath => "bad-path",
            Self::Outside => "outside",
            Self::Link => "link",
            Self::NotAFolder => "not-a-folder",
            Self::RootMoved => "root-moved",
            Self::Exists => "exists",
            Self::MissingFolder => "missing-folder",
            Self::HardLink => "hard-link",
        }
    }
}

/// `ProjectWriteRefused`: an `Error` named `ProjectWriteRefused`, with no `code`
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectWriteRefused {
    pub reason: ProjectWriteRefusal,
    pub detail: String,
}

impl ProjectWriteRefused {
    /// `new ProjectWriteRefused(reason, detail)`
    pub fn new(reason: ProjectWriteRefusal, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }

    /// `error.name`
    pub const NAME: &'static str = "ProjectWriteRefused";

    /// `error.message`
    pub fn message(&self) -> String {
        format!(
            "project folder write refused ({}): {}",
            self.reason.as_str(),
            self.detail
        )
    }
}

impl std::fmt::Display for ProjectWriteRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ProjectWriteRefused {}

/// What a write of the guard throws: a refusal, or the error of the file
/// system call (an `Error` without a `code` is a [`NodeError`] with none).
/// `error instanceof ProjectWriteRefused` is a match on `Refused`.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteGuardError {
    Refused(ProjectWriteRefused),
    Fs(NodeError),
    #[cfg(unix)]
    Cleanup { original: Box<WriteGuardError>, cleanup: ProjectCleanup },
}

impl WriteGuardError {
    /// `error.message`
    pub fn message(&self) -> String {
        match self {
            Self::Refused(refused) => refused.message(),
            Self::Fs(error) => error.message.clone(),
            #[cfg(unix)]
            Self::Cleanup { original, .. } => format!("{}; project staging cleanup is pending", original.message()),
        }
    }

    /// `error.code`; `None` for a refusal
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Refused(_) => None,
            Self::Fs(error) => error.code,
            #[cfg(unix)]
            Self::Cleanup { original, .. } => original.code(),
        }
    }
}

impl From<ProjectWriteRefused> for WriteGuardError {
    fn from(refused: ProjectWriteRefused) -> Self {
        Self::Refused(refused)
    }
}

impl From<NodeError> for WriteGuardError {
    fn from(error: NodeError) -> Self {
        Self::Fs(error)
    }
}

/// The `change` of `updateData`: the current text (`None` when absent) to the
/// new text, `None` to leave the file alone, or what the change fails with
pub type DataChange<'r, E> = &'r mut dyn FnMut(Option<&str>) -> Result<Option<String>, E>;

/// `DataUpdateResult`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataUpdateResult {
    /// False when the change returned null or undefined: nothing was written or created
    pub written: bool,
    /// The text now in the file (the previous text when nothing was written; `None` (`null`) when absent)
    pub text: Option<String>,
}

// ---------- the file system, as Node's promises API answers ----------

/// `lstat(path)`
fn lstat(path: &str) -> Result<Metadata, NodeError> {
    fs::symlink_metadata(namespaced(path)).map_err(|e| node_error(&e, "lstat", path))
}

/// `lstatOrNull(path)`: only a missing entry is `None`; any other failure is thrown
fn lstat_or_null(path: &str) -> Result<Option<Metadata>, NodeError> {
    match lstat(path) {
        Ok(info) => Ok(Some(info)),
        Err(error) if error.code == Some("ENOENT") => Ok(None),
        Err(error) => Err(error),
    }
}

/// `realpath(path)`
fn realpath(path: &str) -> Result<String, NodeError> {
    let real = fs::canonicalize(namespaced(path)).map_err(|e| node_error(&e, "realpath", path))?;
    let text = real.to_string_lossy().into_owned();
    // the verbatim form std gives on Windows is not what Node's realpath answers
    #[cfg(windows)]
    if let Some(plain) = text.strip_prefix(r"\\?\")
        && !plain.starts_with("UNC\\")
    {
        return Ok(plain.to_owned());
    }
    Ok(text)
}

/// `mkdir(path)`
fn mkdir(path: &str) -> Result<(), NodeError> {
    fs::create_dir(namespaced(path)).map_err(|e| node_error(&e, "mkdir", path))
}

/// `unlink(path)`
fn unlink(path: &str) -> Result<(), NodeError> {
    fs::remove_file(namespaced(path)).map_err(|e| node_error(&e, "unlink", path))
}

/// `rename(from, to)`
fn rename(from: &str, to: &str) -> Result<(), NodeError> {
    fs::rename(namespaced(from), namespaced(to)).map_err(|e| node_error2(&e, "rename", from, to))
}

/// Atomically publish a staged path without replacing an existing destination.
pub fn rename_no_replace(from: &str, to: &str) -> Result<(), NodeError> {
    rename_no_replace_native(from, to).map_err(|error| node_error2(&error, "rename", from, to))
}

#[cfg(target_os = "macos")]
fn rename_no_replace_native(from: &str, to: &str) -> std::io::Result<()> {
    let from = std::ffi::CString::new(from).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let to = std::ffi::CString::new(to).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: both C strings remain live and are NUL-terminated.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(target_os = "linux")]
fn rename_no_replace_native(from: &str, to: &str) -> std::io::Result<()> {
    let from = std::ffi::CString::new(from).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let to = std::ffi::CString::new(to).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: both C strings remain live and are NUL-terminated.
    let result = unsafe {
        libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), libc::RENAME_NOREPLACE)
    };
    if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(windows)]
fn rename_no_replace_native(from: &str, to: &str) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let wide = |path: &str| -> std::io::Result<Vec<u16>> {
        if path.contains('\0') {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"));
        }
        Ok(std::ffi::OsStr::new(path).encode_wide().chain(std::iter::once(0)).collect())
    };
    let from = wide(&namespaced(from))?;
    let to = wide(&namespaced(to))?;
    // SAFETY: both UTF-16 buffers remain live and are NUL-terminated.
    let result = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if result != 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn rename_no_replace_native(_from: &str, _to: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "exclusive rename is unavailable on this platform"))
}

/// `readFile(path, 'utf8')`: ill-formed bytes decode as U+FFFD, a byte order
/// mark stays
fn read_utf8(path: &str) -> Result<String, NodeError> {
    use std::io::Read;
    let mut file = fs::File::open(namespaced(path)).map_err(|e| node_error(&e, "open", path))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| node_error_fd(&e, "read"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `writeFile(path, data, { flag: 'wx' })`
fn write_new(path: &str, data: &[u8]) -> Result<(), NodeError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(namespaced(path))
        .map_err(|e| node_error(&e, "open", path))?;
    file.write_all(data).map_err(|e| node_error_fd(&e, "write"))
}

/// `appendFile(path, text, { encoding: 'utf8' })`
fn append_text(path: &str, text: &str) -> Result<(), NodeError> {
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(namespaced(path))
        .map_err(|e| node_error(&e, "open", path))?;
    file.write_all(text.as_bytes())
        .map_err(|e| node_error_fd(&e, "write"))
}

/// `looksAbsolute(path)`
fn looks_absolute(path: &str) -> bool {
    host::is_absolute(path)
        || win32::is_absolute(path)
        || posix::is_absolute(path)
        || js_regex!(r"^[A-Za-z]:").test(path)
}

/// `relPath.split(/[\\/]/)`
fn split_segments(path: &str) -> Vec<&str> {
    js_regex!(r"[\\/]").split(path)
}

/// `dataSegments(relPath)`: the segments of a path relative to `.seuoffice/`,
/// or a refusal
fn data_segments(rel_path: &str) -> Result<Vec<&str>, ProjectWriteRefused> {
    if rel_path.is_empty() || looks_absolute(rel_path) {
        return Err(ProjectWriteRefused::new(
            ProjectWriteRefusal::BadPath,
            rel_path,
        ));
    }
    let segments = split_segments(rel_path);
    if !segments.iter().all(|segment| is_plain_segment(segment)) {
        return Err(ProjectWriteRefused::new(
            ProjectWriteRefusal::BadPath,
            rel_path,
        ));
    }
    Ok(segments)
}

/// `ProjectWriteGuard`
#[derive(Debug, Clone)]
pub struct ProjectWriteGuard {
    /// `NodeJS.Platform` (`process.platform` where the TypeScript leaves it out)
    platform: String,
}

#[cfg(unix)]
pub struct PreparedProjectFolder {
    parent_path: String,
    private_path: String,
    staged_path: String,
    target_path: String,
    target_name: String,
    parent_handle: fs::File,
    private_handle: fs::File,
    staged_handle: fs::File,
}

pub struct PublishedProjectFolder {
    pub path: String,
    pub retained_private_dir: Option<String>,
}

#[cfg(unix)]
impl PreparedProjectFolder {
    /// Publish only the fully prepared directory. The caller holds its native
    /// request authority while invoking the supplied effect.
    pub fn publish(
        self,
        authorize: impl FnOnce(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<PublishedProjectFolder, WriteGuardError> {
        let mut called = false;
        let mut published = false;
        let mut effect = || {
            if called {
                return Err(NodeError { code: None, message: "project publish effect repeated".into() });
            }
            called = true;
            if !stage_handle_matches(&self.parent_handle, &self.parent_path)? {
                return Err(NodeError { code: None, message: "project destination parent changed".into() });
            }
            publish_staged_directory(&self.private_handle, &self.staged_handle, &self.parent_handle,
                &self.target_name, &self.staged_path, &self.target_path)?;
            published = true;
            Ok(())
        };
        let result = authorize(&mut effect);
        drop(effect);
        match result {
            Ok(()) if published => Ok(PublishedProjectFolder { path: self.target_path, retained_private_dir: Some(self.private_path) }),
            Ok(()) => Err(NodeError { code: None, message: format!("project owner guard did not publish; private staging retained at {}", self.private_path) }.into()),
            Err(error) if published => Err(NodeError {
                code: error.code,
                message: format!("{}; project folder was already published at {}", error.message, self.target_path),
            }.into()),
            Err(error) if error.code == Some("EEXIST") || error.code == Some("ENOTEMPTY") =>
                Err(ProjectWriteRefused::new(ProjectWriteRefusal::Exists, format!("{}; private staging retained at {}", self.target_path, self.private_path)).into()),
            Err(error) => Err(NodeError {
                code: error.code,
                message: format!("{}; unpublished private staging retained at {}", error.message, self.private_path),
            }.into()),
        }
    }
}

impl ProjectWriteGuard {
    pub fn new(platform: &str) -> Self {
        Self {
            platform: platform.to_owned(),
        }
    }

    pub fn resolve_create_parent(&self, parent: &str) -> Result<String, WriteGuardError> {
        if !looks_absolute(parent) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, parent).into());
        }
        let resolved = nfc(&realpath(parent)?);
        if !lstat(&resolved)?.is_dir() {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, resolved).into());
        }
        Ok(resolved)
    }

    #[cfg(any(unix, windows))]
    pub fn write_data_authorized(
        &self, root: &str, rel_name: &str, bytes: &[u8], data_dir: Option<&str>,
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<String, WriteGuardError> {
        self.write_data_authorized_inner(root, rel_name, bytes, data_dir, false, None, authorize)
    }

    #[cfg(any(unix, windows))]
    pub fn create_data_authorized(
        &self, root: &str, rel_name: &str, bytes: &[u8], data_dir: Option<&str>,
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<Option<String>, WriteGuardError> {
        let retained = self.write_data_authorized_inner(root, rel_name, bytes, data_dir, true, None, authorize)?;
        Ok((!retained.is_empty()).then_some(retained))
    }

    #[cfg(any(unix, windows))]
    pub fn update_data_authorized<E: From<WriteGuardError>>(
        &self, root: &str, rel_name: &str,
        change: &mut dyn FnMut(Option<&str>) -> Result<Option<String>, E>,
        data_dir: Option<&str>,
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<(DataUpdateResult, Vec<String>), E> {
        self.update_data_authorized_inner(root, rel_name, change, data_dir, None, authorize)
    }

    #[cfg(any(unix, windows))]
    fn update_data_authorized_inner<E: From<WriteGuardError>>(
        &self, root: &str, rel_name: &str,
        change: &mut dyn FnMut(Option<&str>) -> Result<Option<String>, E>,
        data_dir: Option<&str>, scope: Option<&ChatsDirectory>,
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<(DataUpdateResult, Vec<String>), E> {
        if data_segments(rel_name).map_err(|error| E::from(error.into()))?.len() != 1 {
            return Err(E::from(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, rel_name).into()));
        }
        self.assert_root(root).map_err(E::from)?;
        let data_name = data_dir.map(str::to_owned).unwrap_or_else(|| data_dir_name(root));
        if !is_data_dir_name(&data_name) {
            return Err(E::from(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, data_name).into()));
        }
        #[cfg(unix)]
        let (snapshot, current) = match scope {
            Some(scope) => data_unix::Snapshot::capture_chats(scope, rel_name),
            None => data_unix::Snapshot::capture(root, &data_name, rel_name),
        }.map_err(E::from)?;
        #[cfg(unix)]
        let mut check = || {
            if let Some(scope) = scope {
                if !scope.matches()? { return Ok(false); }
            }
            snapshot.matches()
        };
        #[cfg(windows)]
        let lift = |error: NodeError| E::from(WriteGuardError::Fs(error));
        #[cfg(windows)]
        let owned_project = if scope.is_none() { Some(staging_windows::open_existing_dir(root).map_err(&lift)?) } else { None };
        #[cfg(windows)]
        let project = match scope { Some(scope) => &scope.root_handle, None => owned_project.as_ref().expect("owned project was opened") };
        #[cfg(windows)]
        let owned_data = if scope.is_none() { staging_windows::open_data_directory(project, &data_name, root, false).map_err(&lift)? } else { None };
        #[cfg(windows)]
        let data = match scope { Some(scope) => Some(&scope.directory_handle), None => owned_data.as_ref() };
        #[cfg(windows)]
        let mut snapshot = match &data {
            Some(data) => staging_windows::open_data_read_target(data, rel_name, root).map_err(&lift)?,
            None => staging_windows::DataReadTarget::Missing,
        };
        #[cfg(windows)]
        let current = match &mut snapshot {
            staging_windows::DataReadTarget::Missing => None,
            staging_windows::DataReadTarget::File(file) => Some(staging_windows::read_data_target(file).map_err(&lift)?),
            staging_windows::DataReadTarget::Directory => return Err(E::from(ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, rel_name).into())),
            staging_windows::DataReadTarget::Reparse => return Err(E::from(ProjectWriteRefused::new(ProjectWriteRefusal::Link, rel_name).into())),
        };
        #[cfg(windows)]
        let mut check = || -> Result<bool, WriteGuardError> {
            if let Some(scope) = scope {
                if !scope.matches()? { return Ok(false); }
                return Ok(staging_windows::data_read_target_matches(&scope.directory_handle, rel_name, root, &snapshot)?);
            }
            if !staging_windows::handle_matches_path(project, root)? { return Ok(false); }
            let current = staging_windows::open_data_directory(project, &data_name, root, false)?;
            if let Some(held) = data {
                if !staging_windows::handle_matches_path(held, &join(&[root, &data_name]))? { return Ok(false); }
            }
            match current {
                Some(current) => Ok(staging_windows::data_read_target_matches(&current, rel_name, root, &snapshot)?),
                None => Ok(matches!(snapshot, staging_windows::DataReadTarget::Missing)),
            }
        };
        let Some(next) = change(current.as_deref())? else {
            return Ok((DataUpdateResult { written: false, text: current }, Vec::new()));
        };
        let mut witness_error = None;
        let result = self.write_data_authorized_inner(root, rel_name, next.as_bytes(), Some(&data_name), false, scope, &mut |effect| {
            authorize(&mut || {
                match check() {
                    Ok(true) => effect(),
                    result => {
                        let error = match result {
                            Err(error) => error,
                            _ => WriteGuardError::Fs(NodeError { code: None, message: "project data changed before update".into() }),
                        };
                        let answer = NodeError { code: error.code(), message: error.message() };
                        witness_error = Some(error);
                        Err(answer)
                    }
                }
            })
        });
        if let Some(mut error) = witness_error {
            if let Err(actual) = result {
                match &mut error {
                    WriteGuardError::Refused(refused) => refused.detail = format!("{}; {}", refused.detail, actual.message()),
                    WriteGuardError::Fs(node) => node.message = actual.message(),
                    #[cfg(unix)]
                    WriteGuardError::Cleanup { original, .. } => {
                        if let WriteGuardError::Fs(node) = original.as_mut() { node.message = actual.message(); }
                    }
                }
            }
            return Err(E::from(error));
        }
        let retained = result.map_err(E::from)?;
        Ok((DataUpdateResult { written: true, text: Some(next) }, if retained.is_empty() { Vec::new() } else { vec![retained] }))
    }

    #[cfg(windows)]
    pub fn remove_data_authorized(
        &self, root: &str, rel_name: &str, data_dir: Option<&str>,
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<(), WriteGuardError> {
        if data_segments(rel_name)?.len() != 1 {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, rel_name).into());
        }
        self.assert_root(root)?;
        let data_name = data_dir.map(str::to_owned).unwrap_or_else(|| data_dir_name(root));
        if !is_data_dir_name(&data_name) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, data_name).into());
        }
        let project = staging_windows::open_existing_dir(root)?;
        let Some(data) = staging_windows::open_data_directory(&project, &data_name, root, false)? else { return Ok(()); };
        let mut target = staging_windows::open_data_delete_target(&data, rel_name, root)?;
        match &target {
            staging_windows::DataDeleteTarget::Directory => return Err(ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, rel_name).into()),
            staging_windows::DataDeleteTarget::Reparse => return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Link, rel_name).into()),
            _ => {}
        }
        run_authorized(authorize, || {
            if !staging_windows::handle_matches_path(&project, root)?
                || !staging_windows::handle_matches_path(&data, &join(&[root, &data_name]))?
                || !staging_windows::data_delete_target_matches(&data, rel_name, root, &target)? {
                return Err(NodeError { code: None, message: "project data changed before removal".into() });
            }
            match &mut target {
                staging_windows::DataDeleteTarget::File(file) => staging_windows::delete_data_target(file),
                staging_windows::DataDeleteTarget::Missing => Ok(()),
                _ => unreachable!("refused targets were rejected"),
            }
        })?;
        Ok(())
    }

    #[cfg(unix)]
    pub fn remove_data_authorized(
        &self, root: &str, rel_name: &str, data_dir: Option<&str>,
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<(), WriteGuardError> {
        use std::os::fd::{AsRawFd, FromRawFd};
        if data_segments(rel_name)?.len() != 1 {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, rel_name).into());
        }
        self.assert_root(root)?;
        let root_handle = open_stage_dir(root)?;
        let data_name = data_dir.map(str::to_owned).unwrap_or_else(|| data_dir_name(root));
        if !is_data_dir_name(&data_name) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, data_name).into());
        }
        let data_path = join(&[root, &data_name]);
        let target = join(&[&data_path, rel_name]);
        let data_c = std::ffi::CString::new(data_name).map_err(|_| NodeError { code: None, message: "data folder contains NUL".into() })?;
        let file_c = std::ffi::CString::new(rel_name).map_err(|_| NodeError { code: None, message: "data filename contains NUL".into() })?;
        let mut refusal = None;
        let result = run_authorized(authorize, || {
            if !stage_handle_matches(&root_handle, root)? {
                return Err(NodeError { code: None, message: "project root changed".into() });
            }
            let mut directory_info = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: root/name are live; fstatat initializes directory_info on success.
            if unsafe { libc::fstatat(root_handle.as_raw_fd(), data_c.as_ptr(), directory_info.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENOENT) { return Ok(()); }
                return Err(node_error(&error, "lstat", &data_path));
            }
            // SAFETY: fstatat succeeded.
            let directory_info = unsafe { directory_info.assume_init() };
            if directory_info.st_mode & libc::S_IFMT != libc::S_IFDIR {
                refusal = Some(if directory_info.st_mode & libc::S_IFMT == libc::S_IFLNK { ProjectWriteRefusal::Link } else { ProjectWriteRefusal::NotAFolder });
                return Err(NodeError { code: None, message: "project data folder refused".into() });
            }
            // SAFETY: root_handle and the child name are live; openat creates an owned descriptor.
            let fd = unsafe { libc::openat(root_handle.as_raw_fd(), data_c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENOENT) { return Ok(()); }
                return Err(node_error(&error, "open", &data_path));
            }
            // SAFETY: fd came from successful openat and has not been transferred.
            let data = unsafe { fs::File::from_raw_fd(fd) };
            let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: data and file_c are live; fstatat initializes info on success.
            if unsafe { libc::fstatat(data.as_raw_fd(), file_c.as_ptr(), info.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENOENT) { return Ok(()); }
                return Err(node_error(&error, "lstat", &target));
            }
            // SAFETY: fstatat succeeded.
            let info = unsafe { info.assume_init() };
            if info.st_mode & libc::S_IFMT != libc::S_IFREG {
                refusal = Some(if info.st_mode & libc::S_IFMT == libc::S_IFLNK { ProjectWriteRefusal::Link } else { ProjectWriteRefusal::NotAFolder });
                return Err(NodeError { code: None, message: "project data target refused".into() });
            }
            // SAFETY: unlinkat names a direct child of the pinned, no-follow data directory.
            if unsafe { libc::unlinkat(data.as_raw_fd(), file_c.as_ptr(), 0) } != 0 {
                return Err(node_error(&std::io::Error::last_os_error(), "unlink", &target));
            }
            Ok(())
        });
        if let Some(reason) = refusal { return Err(ProjectWriteRefused::new(reason, target).into()); }
        result.map_err(Into::into)
    }

    #[cfg(unix)]
    fn write_data_authorized_inner(
        &self, root: &str, rel_name: &str, bytes: &[u8], data_dir: Option<&str>, exclusive: bool, scope: Option<&ChatsDirectory>,
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<String, WriteGuardError> {
        let mut scoped_authorize = |effect: &mut dyn FnMut() -> Result<(), NodeError>| {
            authorize(&mut || {
                if let Some(scope) = scope {
                    if !scope.matches()? { return Err(NodeError { code: None, message: "chat directory binding changed".into() }); }
                }
                effect()
            })
        };
        let authorize = &mut scoped_authorize;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::MetadataExt;
        if data_segments(rel_name)?.len() != 1 {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, rel_name).into());
        }
        self.assert_root(root)?;
        let owned_root = if scope.is_none() { Some(open_stage_dir(root)?) } else { None };
        let root_handle = match scope { Some(scope) => &scope.root_handle, None => owned_root.as_ref().expect("project root was opened") };
        let data_name = data_dir.map(str::to_owned).unwrap_or_else(|| data_dir_name(root));
        if !is_data_dir_name(&data_name) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, data_name).into());
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|error| NodeError { code: None, message: error.to_string() })?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let private_name = format!(".seuoffice-stage-{suffix}");
        let private_path = join(&[root, &private_name]);
        let private = run_authorized(authorize, || {
            if !stage_handle_matches(&root_handle, root)? {
                return Err(NodeError { code: None, message: "project root changed".into() });
            }
            create_private_stage_dir(&private_path, &root_handle, &private_name)
        }).map_err(|error| NodeError {
            code: error.code, message: format!("{}; private staging may remain at {}", error.message, private_path),
        })?;
        let retained = |error: NodeError| NodeError {
            code: error.code, message: format!("{}; private staging retained at {}", error.message, private_path),
        };
        let payload_name = c"payload";
        let mut payload = run_authorized(authorize, || {
            if !stage_handle_matches(root_handle, root)? {
                return Err(NodeError { code: None, message: "project root changed".into() });
            }
            // SAFETY: private is an open directory and payload_name is a live C string.
            let fd = unsafe { libc::openat(private.as_raw_fd(), payload_name.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o666) };
            if fd < 0 { return Err(node_error(&std::io::Error::last_os_error(), "open", &private_path)); }
            // SAFETY: fd was returned by openat and has not been transferred.
            Ok(unsafe { fs::File::from_raw_fd(fd) })
        }).map_err(&retained)?;
        payload.write_all(bytes).map_err(|error| retained(node_error(&error, "write", &private_path)))?;
        if let Err(error) = payload.sync_all() {
            if !matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES | libc::EBUSY | libc::EINVAL | libc::ENOSYS)) {
                return Err(retained(node_error(&error, "fsync", &private_path)).into());
            }
        }
        let data_c = std::ffi::CString::new(data_name.as_str()).map_err(|_| NodeError { code: None, message: "data folder contains NUL".into() })?;
        let file_c = std::ffi::CString::new(rel_name).map_err(|_| NodeError { code: None, message: "data filename contains NUL".into() })?;
        let inferred_data_path;
        let data_path = match scope {
            Some(scope) => scope.directory_path.as_str(),
            None => { inferred_data_path = join(&[root, &data_name]); inferred_data_path.as_str() }
        };
        let mut directory_refusal = None;
        let owned_data = if scope.is_none() { Some({
        let data = run_authorized(authorize, || {
            if !stage_handle_matches(&root_handle, root)? {
                return Err(NodeError { code: None, message: "project root changed".into() });
            }
            // SAFETY: root_handle and data_c are live and mkdirat creates only a direct child.
            if unsafe { libc::mkdirat(root_handle.as_raw_fd(), data_c.as_ptr(), 0o777) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) { return Err(node_error(&error, "mkdir", &data_path)); }
            }
            let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: root/name are live; fstatat initializes info on success.
            if unsafe { libc::fstatat(root_handle.as_raw_fd(), data_c.as_ptr(), info.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                return Err(node_error(&std::io::Error::last_os_error(), "lstat", &data_path));
            }
            // SAFETY: fstatat succeeded.
            let info = unsafe { info.assume_init() };
            if info.st_mode & libc::S_IFMT != libc::S_IFDIR {
                directory_refusal = Some(if info.st_mode & libc::S_IFMT == libc::S_IFLNK { ProjectWriteRefusal::Link } else { ProjectWriteRefusal::NotAFolder });
                return Err(NodeError { code: None, message: "project data folder refused".into() });
            }
            // SAFETY: root_handle and data_c are live; openat returns a new owned directory descriptor.
            let fd = unsafe { libc::openat(root_handle.as_raw_fd(), data_c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
            if fd < 0 { return Err(node_error(&std::io::Error::last_os_error(), "open", &data_path)); }
            // SAFETY: fd was returned by openat and has not been transferred.
            Ok(unsafe { fs::File::from_raw_fd(fd) })
        });
        let data = match data {
            Ok(data) => data,
            Err(error) => {
                if let Some(reason) = directory_refusal {
                    return Err(ProjectWriteRefused::new(reason, format!("{}; private staging retained at {}", data_path, private_path)).into());
                }
                return Err(retained(error).into());
            }
        };
        data
        }) } else { None };
        let data = match scope { Some(scope) => &scope.directory_handle, None => owned_data.as_ref().expect("data directory was opened") };
        let mut attempt = 0;
        loop {
            let mut published = false;
            let mut refusal = None;
            let result = run_authorized(authorize, || {
                if !stage_handle_matches(&root_handle, root)? {
                    return Err(NodeError { code: None, message: "project root changed".into() });
                }
                if scope.is_none() {
                    let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
                    // SAFETY: root_handle and data_c are live; fstatat initializes current on success.
                    if unsafe { libc::fstatat(root_handle.as_raw_fd(), data_c.as_ptr(), current.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                        return Err(node_error(&std::io::Error::last_os_error(), "lstat", &data_path));
                    }
                    // SAFETY: fstatat succeeded.
                    let current = unsafe { current.assume_init() };
                    let held = data.metadata().map_err(|error| node_error(&error, "fstat", &data_path))?;
                    if current.st_dev as u64 != held.dev() || current.st_ino as u64 != held.ino() || current.st_mode & libc::S_IFMT != libc::S_IFDIR {
                        return Err(NodeError { code: None, message: "project data folder changed".into() });
                    }
                }
                let target = join(&[&data_path, rel_name]);
                let mut existing = std::mem::MaybeUninit::<libc::stat>::uninit();
                // SAFETY: data and file_c are live; fstatat initializes existing on success.
                if unsafe { libc::fstatat(data.as_raw_fd(), file_c.as_ptr(), existing.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } == 0 {
                    if exclusive {
                        return Err(NodeError { code: Some("EEXIST"), message: format!("project data target already exists: {target}") });
                    }
                    // SAFETY: fstatat succeeded.
                    let existing = unsafe { existing.assume_init() };
                    if existing.st_mode & libc::S_IFMT != libc::S_IFREG {
                        refusal = Some(if existing.st_mode & libc::S_IFMT == libc::S_IFLNK {
                            ProjectWriteRefusal::Link
                        } else {
                            ProjectWriteRefusal::NotAFolder
                        });
                        return Err(NodeError { code: None, message: "project data target refused".into() });
                    }
                } else {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ENOENT) { return Err(node_error(&error, "lstat", &target)); }
                }
                // SAFETY: the descriptors and C strings remain live for each rename operation.
                let renamed = if exclusive {
                    #[cfg(target_os = "macos")]
                    { (unsafe { libc::renameatx_np(private.as_raw_fd(), payload_name.as_ptr(), data.as_raw_fd(), file_c.as_ptr(), libc::RENAME_EXCL) }) as i64 }
                    #[cfg(target_os = "linux")]
                    { (unsafe { libc::syscall(libc::SYS_renameat2, private.as_raw_fd(), payload_name.as_ptr(), data.as_raw_fd(), file_c.as_ptr(), libc::RENAME_NOREPLACE) }) as i64 }
                    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                    { return Err(NodeError { code: None, message: "exclusive data publication is unavailable on this platform".into() }); }
                } else {
                    (unsafe { libc::renameat(private.as_raw_fd(), payload_name.as_ptr(), data.as_raw_fd(), file_c.as_ptr()) }) as i64
                };
                if renamed != 0 {
                    return Err(node_error2(&std::io::Error::last_os_error(), "rename", &private_path, &target));
                }
                published = true;
                Ok(())
            });
            match result {
                Ok(()) => return Ok(private_path),
                Err(error) if published => return Err(NodeError {
                    code: error.code,
                    message: format!("{}; project data was already published; private staging retained at {}", error.message, private_path),
                }.into()),
                Err(_) if refusal.is_some() => return Err(ProjectWriteRefused::new(
                    refusal.expect("refusal was checked"),
                    format!("{}; private staging retained at {}", join(&[&data_path, rel_name]), private_path),
                ).into()),
                Err(error) if matches!(error.code, Some("EPERM" | "EACCES" | "EBUSY")) && attempt < 4 => {
                    std::thread::sleep(std::time::Duration::from_millis(50 << attempt));
                    attempt += 1;
                }
                Err(error) => return Err(NodeError {
                    code: error.code,
                    message: format!("{}; private staging retained at {}", error.message, private_path),
                }.into()),
            }
        }
    }

    #[cfg(windows)]
    fn write_data_authorized_inner(
        &self, root: &str, rel_name: &str, bytes: &[u8], data_dir: Option<&str>, exclusive: bool, scope: Option<&ChatsDirectory>,
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<String, WriteGuardError> {
        let mut scoped_authorize = |effect: &mut dyn FnMut() -> Result<(), NodeError>| {
            authorize(&mut || {
                if let Some(scope) = scope {
                    if !scope.matches()? { return Err(NodeError { code: None, message: "chat directory binding changed".into() }); }
                }
                effect()
            })
        };
        let authorize = &mut scoped_authorize;
        if data_segments(rel_name)?.len() != 1 {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, rel_name).into());
        }
        self.assert_root(root)?;
        let data_name = data_dir.map(str::to_owned).unwrap_or_else(|| data_dir_name(root));
        if !is_data_dir_name(&data_name) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, data_name).into());
        }
        let owned_project = if scope.is_none() { Some(staging_windows::open_existing_dir(root)?) } else { None };
        let project = match scope { Some(scope) => &scope.root_handle, None => owned_project.as_ref().expect("project root was opened") };
        let inferred_data_path;
        let data_path = match scope {
            Some(scope) => scope.directory_path.as_str(),
            None => { inferred_data_path = join(&[root, &data_name]); inferred_data_path.as_str() }
        };
        let target = join(&[&data_path, rel_name]);
        let owned_data = if scope.is_none() { Some(run_authorized(authorize, || {
            if !staging_windows::handle_matches_path(&project, root)? {
                return Err(NodeError { code: None, message: "project root changed".into() });
            }
            staging_windows::open_data_directory(&project, &data_name, root, true)?
                .ok_or_else(|| NodeError { code: None, message: "project data folder was not created".into() })
        }).map_err(|error| match error.code {
            Some("ELOOP") => WriteGuardError::Refused(ProjectWriteRefused::new(ProjectWriteRefusal::Link, data_path)),
            Some("ENOTDIR") => WriteGuardError::Refused(ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, data_path)),
            _ => WriteGuardError::Fs(error),
        })?) } else { None };
        let data = match scope { Some(scope) => &scope.directory_handle, None => owned_data.as_ref().expect("data directory was opened") };
        let mut stage = stage_authorized_data(&project, &data, root, &data_path, rel_name, bytes, authorize)?;
        let mut attempt = 0;
        loop {
            let mut published = false;
            let mut refusal = None;
            let result = run_authorized(authorize, || {
                if !staging_windows::handle_matches_path(&project, root)?
                    || !staging_windows::handle_matches_path(&data, &data_path)? {
                    return Err(NodeError { code: None, message: "project data folder changed".into() });
                }
                let current = staging_windows::inspect_data_target(&data, rel_name, root)?;
                if exclusive && current != staging_windows::DataTarget::Missing {
                    return Err(NodeError { code: Some("EEXIST"), message: format!("project data target already exists: {target}") });
                }
                match current {
                    staging_windows::DataTarget::Missing | staging_windows::DataTarget::File { .. } => {}
                    staging_windows::DataTarget::Directory => refusal = Some(ProjectWriteRefusal::NotAFolder),
                    staging_windows::DataTarget::Reparse => refusal = Some(ProjectWriteRefusal::Link),
                }
                if refusal.is_some() {
                    return Err(NodeError { code: None, message: "project data target refused".into() });
                }
                if exclusive {
                    staging_windows::publish_project_data_exclusive(&mut stage)?;
                } else {
                    staging_windows::publish_project_data_replace(&mut stage)?;
                }
                published = true;
                Ok(())
            });
            match result {
                Ok(()) => return Ok(String::new()),
                Err(error) if published => return Err(NodeError {
                    code: error.code, message: format!("{}; project data was already published", error.message),
                }.into()),
                Err(error) if refusal.is_none()
                    && matches!(error.code, Some("EPERM" | "EACCES" | "EBUSY")) && attempt < 4 => {
                    std::thread::sleep(std::time::Duration::from_millis(50 << attempt));
                    attempt += 1;
                }
                Err(error) => {
                    if let Err(cleanup) = stage.cancel() {
                        return Err(NodeError { code: error.code, message: format!(
                            "{}; unpublished data cleanup failed: {}", error.message, cleanup.message
                        ) }.into());
                    }
                    if let Some(reason) = refusal {
                        return Err(ProjectWriteRefused::new(reason, target).into());
                    }
                    return Err(error.into());
                }
            }
        }
    }

    #[cfg(unix)]
    pub fn create_project_authorized(
        &self, parent: &str, name: &str, project_file: &[u8],
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<PublishedProjectFolder, WriteGuardError> {
        self.stage_project_folder_authorized(parent, name, project_file, authorize)?.publish(|effect| authorize(effect))
    }

    #[cfg(windows)]
    pub fn create_project_authorized(
        &self, parent: &str, name: &str, project_file: &[u8],
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<PublishedProjectFolder, WriteGuardError> {
        if !is_valid_rename_name(name) || !is_plain_segment(name) || !looks_absolute(parent) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, name).into());
        }
        let parent_path = nfc(&realpath(parent)?);
        if !same_path(&parent_path, parent, &self.platform) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::RootMoved, parent).into());
        }
        let parent_handle = staging_windows::open_create_parent(&parent_path)?;
        let target_path = join(&[&parent_path, name]);
        let mut created = false;
        let project = match run_authorized(authorize, || {
            if !staging_windows::handle_matches_path(&parent_handle, &parent_path)? {
                return Err(NodeError { code: None, message: "project destination parent changed".into() });
            }
            let project = staging_windows::create_final_directory(&parent_handle, name, &target_path)?;
            created = true;
            Ok(project)
        }) {
            Ok(project) => project,
            Err(error) if !created && error.code == Some("EEXIST") =>
                return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Exists, target_path).into()),
            Err(error) if created => return Err(NodeError {
                code: error.code, message: format!("{}; project folder was already published at {}", error.message, target_path),
            }.into()),
            Err(error) => return Err(error.into()),
        };
        let after_create = |error: NodeError| WriteGuardError::Fs(NodeError {
            code: error.code, message: format!("{}; project folder was already published at {}", error.message, target_path),
        });
        let data = run_authorized(authorize, || {
            if !staging_windows::handle_matches_path(&project, &target_path)? {
                return Err(NodeError { code: None, message: "project folder changed".into() });
            }
            staging_windows::create_metadata_directory(&project, &target_path)
        }).map_err(&after_create)?;
        let data_path = join(&[&target_path, crate::packages::project_store::project_folder_name("dataDir")]);
        let mut stage = stage_authorized_data(
            &project, &data, &target_path, &data_path,
            crate::packages::project_store::project_folder_name("projectFile"), project_file, authorize,
        ).map_err(&after_create)?;
        let mut metadata_published = false;
        let result = run_authorized(authorize, || {
            if !staging_windows::handle_matches_path(&project, &target_path)?
                || !staging_windows::handle_matches_path(&data, &data_path)? {
                return Err(NodeError { code: None, message: "project folder changed".into() });
            }
            if staging_windows::inspect_data_target(
                &data, crate::packages::project_store::project_folder_name("projectFile"), &target_path,
            )? != staging_windows::DataTarget::Missing {
                return Err(NodeError { code: Some("EEXIST"), message: "project metadata already exists".into() });
            }
            staging_windows::publish_project_metadata(&mut stage)?;
            metadata_published = true;
            Ok(())
        });
        if let Err(error) = result {
            if metadata_published {
                return Err(after_create(NodeError {
                    code: error.code,
                    message: format!("{}; project metadata was already published", error.message),
                }));
            }
            if let Err(cleanup) = stage.cancel() {
                return Err(after_create(NodeError {
                    code: error.code,
                    message: format!("{}; unpublished metadata cleanup failed: {}", error.message, cleanup.message),
                }));
            }
            return Err(after_create(error));
        }
        Ok(PublishedProjectFolder { path: target_path, retained_private_dir: None })
    }

    #[cfg(unix)]
    pub fn stage_project_folder(
        &self, parent: &str, name: &str, project_file: &[u8],
    ) -> Result<PreparedProjectFolder, WriteGuardError> {
        self.stage_project_folder_authorized(parent, name, project_file, &mut |effect| effect())
    }

    #[cfg(unix)]
    fn stage_project_folder_authorized(
        &self, parent: &str, name: &str, project_file: &[u8],
        authorize: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError>,
    ) -> Result<PreparedProjectFolder, WriteGuardError> {
        if !is_valid_rename_name(name) || !is_plain_segment(name) || !looks_absolute(parent) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, name).into());
        }
        let parent_path = nfc(&realpath(parent)?);
        if !same_path(&parent_path, parent, &self.platform) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::RootMoved, parent).into());
        }
        let parent_handle = open_stage_dir(&parent_path)?;
        let target_path = join(&[&parent_path, name]);
        if lstat_or_null(&target_path)?.is_some() {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Exists, target_path).into());
        }
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| NodeError { code: None, message: error.to_string() })?;
        let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let private_name = format!(".seuoffice-stage-{suffix}");
        let private_path = join(&[&parent_path, &private_name]);
        let private_handle = run_authorized(authorize, || {
            if !stage_handle_matches(&parent_handle, &parent_path)? {
                return Err(NodeError { code: None, message: "project destination parent changed".into() });
            }
            create_private_stage_dir(&private_path, &parent_handle, &private_name)
        }).map_err(|error| NodeError {
            code: error.code,
            message: format!("{}; private staging may remain at {}", error.message, private_path),
        })?;
        let staged_path = join(&[&private_path, "project"]);
        let mut stage_authorize = |effect: &mut dyn FnMut() -> Result<(), NodeError>| {
            authorize(&mut || {
                if !stage_handle_matches(&parent_handle, &parent_path)? {
                    return Err(NodeError { code: None, message: "project destination parent changed".into() });
                }
                effect()
            })
        };
        let staged = (|| -> Result<fs::File, WriteGuardError> {
            let staged_handle = run_authorized(&mut stage_authorize, || create_project_stage_dir(&staged_path, &parent_handle, &private_handle))?;
            write_staged_project(&staged_handle, &staged_path, project_file, &mut stage_authorize)?;
            Ok(staged_handle)
        })();
        let staged_handle = staged.map_err(|error| NodeError {
            code: error.code(),
            message: format!("{}; private staging retained at {}", error.message(), private_path),
        })?;
        Ok(PreparedProjectFolder {
            parent_path, private_path, staged_path, target_path, target_name: name.to_owned(),
            parent_handle, private_handle, staged_handle,
        })
    }

    /// `locate(root, relPath, dataDir)`: the data folder and the file
    /// `relPath` names in it. `dataDir` is the name an operation of several
    /// writes resolved once (`dataDirName`), so all its writes land in one
    /// folder; only the data folder's own names are taken.
    fn locate(
        &self,
        root: &str,
        rel_path: &str,
        data_dir: Option<&str>,
    ) -> Result<(String, String), ProjectWriteRefused> {
        let name = match data_dir {
            Some(name) => name.to_owned(),
            None => data_dir_name(root),
        };
        if !is_data_dir_name(&name) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, name));
        }
        let dir = join(&[root, &name]);
        let mut parts: Vec<&str> = vec![&dir];
        parts.extend(data_segments(rel_path)?);
        let target = join(&parts);
        Ok((dir, target))
    }

    /// `dataPath(root, relPath, dataDir)`: the absolute path of a file in
    /// `root`'s data folder; checks the name only, touches nothing
    pub fn data_path(
        &self,
        root: &str,
        rel_path: &str,
        data_dir: Option<&str>,
    ) -> Result<String, ProjectWriteRefused> {
        self.locate(root, rel_path, data_dir)
            .map(|(_, target)| target)
    }

    /// `writeData(root, relPath, data, dataDir)`: replace a file in the data
    /// folder (temporary file, then rename)
    pub fn write_data(
        &self,
        root: &str,
        rel_path: &str,
        data: &[u8],
        data_dir: Option<&str>,
    ) -> Result<(), WriteGuardError> {
        let (dir, target) = self.locate(root, rel_path, data_dir)?;
        self.prepare_data(root, &dir, rel_path, true)?;
        atomic_write_file(&target, data)?;
        Ok(())
    }

    /// `appendData(root, relPath, text, dataDir)`: append to a file in the
    /// data folder (chat message lines)
    pub fn append_data(
        &self,
        root: &str,
        rel_path: &str,
        text: &str,
        data_dir: Option<&str>,
    ) -> Result<(), WriteGuardError> {
        let (dir, target) = self.locate(root, rel_path, data_dir)?;
        let existing = self.prepare_data(root, &dir, rel_path, true)?;
        if existing.is_some_and(|info| link_count(&target, &info) > 1) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::HardLink, target).into());
        }
        append_text(&target, text)?;
        Ok(())
    }

    /// `updateData(root, relPath, change, dataDir)`: read, change and write
    /// one file in the data folder without another write to that file in
    /// between. `change` gets the current text (`None` when absent) and
    /// returns the new text, or `None` (null/undefined) to leave the file
    /// (and the folder) alone. What `change` fails with comes back as it is.
    pub fn update_data<E: From<WriteGuardError>>(
        &self,
        root: &str,
        rel_path: &str,
        change: DataChange<'_, E>,
        data_dir: Option<&str>,
    ) -> Result<DataUpdateResult, E> {
        fn lift<E: From<WriteGuardError>>(error: impl Into<WriteGuardError>) -> E {
            E::from(error.into())
        }
        let (dir, target) = self.locate(root, rel_path, data_dir).map_err(lift::<E>)?;
        let existing = self
            .prepare_data(root, &dir, rel_path, false)
            .map_err(lift::<E>)?;
        let current = if existing.is_some() {
            Some(read_utf8(&target).map_err(lift::<E>)?)
        } else {
            None
        };
        let Some(next) = change(current.as_deref())? else {
            return Ok(DataUpdateResult {
                written: false,
                text: current,
            });
        };
        self.prepare_data(root, &dir, rel_path, true)
            .map_err(lift::<E>)?;
        atomic_write_file(&target, next.as_bytes()).map_err(lift::<E>)?;
        Ok(DataUpdateResult {
            written: true,
            text: Some(next),
        })
    }

    /// `removeData(root, relPath, dataDir)`: delete one file in the data
    /// folder (old memory backups); a missing file is fine
    pub fn remove_data(
        &self,
        root: &str,
        rel_path: &str,
        data_dir: Option<&str>,
    ) -> Result<(), WriteGuardError> {
        let (dir, target) = self.locate(root, rel_path, data_dir)?;
        let existing = self.prepare_data(root, &dir, rel_path, false)?;
        if existing.is_some() {
            unlink(&target)?;
        }
        Ok(())
    }

    /// `writeNewDocument(root, target, data)`: write a new document the AI
    /// made (spec 6): an absolute path inside the project folder, not under
    /// `.seuoffice/`, in a folder that already exists, never over an existing
    /// entry. The bytes go to a temporary file beside it first.
    pub fn write_new_document(
        &self,
        root: &str,
        target: &str,
        data: &[u8],
    ) -> Result<(), WriteGuardError> {
        self.write_new_document_authorized(root, target, data, std::sync::Arc::new(|effect| Ok(effect())))
    }

    /// `createProjectFolder(parent, name)`: make the folder of a new project
    /// (spec 5.1): one new folder `name` in an existing `parent`. Never
    /// reuses or replaces an existing entry. Returns the folder's path under
    /// the parent's canonical path.
    pub fn create_project_folder(
        &self,
        parent: &str,
        name: &str,
    ) -> Result<String, WriteGuardError> {
        if !is_valid_rename_name(name) || !is_plain_segment(name) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, name).into());
        }
        if !looks_absolute(parent) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, parent).into());
        }
        let real_parent = nfc(&realpath(parent)?);
        let target = join(&[&real_parent, name]);
        if lstat_or_null(&target)?.is_some() {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Exists, target).into());
        }
        match mkdir(&target) {
            Ok(()) => {}
            Err(error) if error.code == Some("EEXIST") => {
                return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Exists, target).into());
            }
            Err(error) => return Err(error.into()),
        }
        Ok(target)
    }

    /// `assertRoot(root)`: the root must still be a folder at exactly its
    /// registered (canonical) path
    fn assert_root(&self, root: &str) -> Result<(), WriteGuardError> {
        if !looks_absolute(root) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, root).into());
        }
        let real = realpath(root)?;
        if !same_path(&real, root, &self.platform) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::RootMoved, root).into());
        }
        let info = lstat(root)?;
        if !info.is_dir() {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, root).into());
        }
        Ok(())
    }

    /// `prepareData(root, dataDir, relPath, create)`: checks the way from the
    /// root through `dataDir` to a data file. With `create`, missing folders
    /// below the root are made one level at a time; without it, a missing
    /// folder means the file is absent. The target's stats, or `None` (`null`)
    /// when it is absent.
    fn prepare_data(
        &self,
        root: &str,
        data_dir: &str,
        rel_path: &str,
        create: bool,
    ) -> Result<Option<Metadata>, WriteGuardError> {
        let segments = data_segments(rel_path)?;
        self.assert_root(root)?;
        let (last, folders_below) = segments.split_last().expect("a split has a segment");
        let mut dir = data_dir.to_owned();
        let mut folders = vec![dir.clone()];
        for segment in folders_below {
            dir = join(&[&dir, segment]);
            folders.push(dir.clone());
        }
        for folder in &folders {
            let info = match lstat_or_null(folder)? {
                Some(info) => info,
                None => {
                    if !create {
                        return Ok(None);
                    }
                    match mkdir(folder) {
                        Ok(()) => {}
                        Err(error) if error.code == Some("EEXIST") => {}
                        Err(error) => return Err(error.into()),
                    }
                    lstat(folder)?
                }
            };
            if info.file_type().is_symlink() {
                return Err(
                    ProjectWriteRefused::new(ProjectWriteRefusal::Link, folder.as_str()).into(),
                );
            }
            if !info.is_dir() {
                return Err(ProjectWriteRefused::new(
                    ProjectWriteRefusal::NotAFolder,
                    folder.as_str(),
                )
                .into());
            }
        }
        if !same_path(&realpath(&dir)?, &dir, &self.platform) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Link, dir).into());
        }
        let target = join(&[&dir, last]);
        let Some(info) = lstat_or_null(&target)? else {
            return Ok(None);
        };
        if info.file_type().is_symlink() {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::Link, target).into());
        }
        if !info.is_file() {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, target).into());
        }
        Ok(Some(info))
    }
}

#[cfg(test)]
mod guarded_publish_tests {
    use super::*;

    #[test]
    fn exclusive_publish_preserves_an_existing_empty_directory() {
        let parent = std::env::temp_dir().join(format!("project-guard-publish-{}", std::process::id()));
        fs::create_dir_all(&parent).unwrap();
        let staged = parent.join("staged");
        let target = parent.join("target");
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("marker"), b"staged").unwrap();
        fs::create_dir(&target).unwrap();
        assert!(rename_no_replace(staged.to_str().unwrap(), target.to_str().unwrap()).is_err());
        assert_eq!(fs::read(staged.join("marker")).unwrap(), b"staged");
        assert!(fs::read_dir(&target).unwrap().next().is_none());
        fs::remove_file(staged.join("marker")).unwrap();
        fs::remove_dir(&staged).unwrap();
        fs::remove_dir(&target).unwrap();
        fs::remove_dir(&parent).unwrap();
    }

    #[test]
    fn exclusive_publish_moves_the_complete_staged_directory() {
        let parent = std::env::temp_dir().join(format!("project-guard-success-{}", std::process::id()));
        fs::create_dir_all(&parent).unwrap();
        let staged = parent.join("staged");
        let target = parent.join("target");
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("marker"), b"staged").unwrap();
        rename_no_replace(staged.to_str().unwrap(), target.to_str().unwrap()).unwrap();
        assert!(!staged.exists());
        assert_eq!(fs::read(target.join("marker")).unwrap(), b"staged");
        fs::remove_file(target.join("marker")).unwrap();
        fs::remove_dir(&target).unwrap();
        fs::remove_dir(&parent).unwrap();
    }

    #[test]
    fn exclusive_publish_does_not_replace_a_competing_file() {
        let parent = std::env::temp_dir().join(format!("project-guard-file-{}", std::process::id()));
        fs::create_dir_all(&parent).unwrap();
        let staged = parent.join("staged");
        let target = parent.join("target");
        fs::create_dir(&staged).unwrap();
        fs::write(&target, b"other writer").unwrap();
        assert!(rename_no_replace(staged.to_str().unwrap(), target.to_str().unwrap()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"other writer");
        assert!(staged.is_dir());
        fs::remove_dir(&staged).unwrap();
        fs::remove_file(&target).unwrap();
        fs::remove_dir(&parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn revoked_owner_leaves_project_unpublished() {
        let parent = std::env::temp_dir().join(format!("project-stage-revoked-{}", std::process::id()));
        fs::create_dir_all(&parent).unwrap();
        let parent = parent.canonicalize().unwrap();
        let guard = ProjectWriteGuard::new(std::env::consts::OS);
        let stage = guard.stage_project_folder(parent.to_str().unwrap(), "created", b"project").unwrap();
        let private = stage.private_path.clone();
        let result = stage.publish(|_| Err(NodeError { code: None, message: "owner revoked".into() }));
        assert!(result.is_err());
        assert!(!parent.join("created").exists());
        assert_eq!(fs::read(std::path::Path::new(&private).join("project/.seuoffice/project.json")).unwrap(), b"project");
        fs::remove_file(std::path::Path::new(&private).join("project/.seuoffice/project.json")).unwrap();
        fs::remove_dir(std::path::Path::new(&private).join("project/.seuoffice")).unwrap();
        fs::remove_dir(std::path::Path::new(&private).join("project")).unwrap();
        fs::remove_dir(&private).unwrap();
        fs::remove_dir(&parent).unwrap();
    }
}
