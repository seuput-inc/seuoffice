//! Handle-relative rescue copy for an actual retained private stage.
use std::{ffi::{CStr, CString}, fs::{File, Permissions}, io::{self, Seek, SeekFrom}, os::{fd::{AsRawFd, FromRawFd}, unix::fs::{MetadataExt, PermissionsExt}};
use crate::{OwnedStage, PendingStage, PrivateDirectory};

/// Capture the final entry itself, including a symlink, without requiring
/// read access to its contents. The descriptor is not a namespace lock.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn capture_rescue_entry(parent: &File, name: &CStr) -> io::Result<Option<File>> {
    let bytes = name.to_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid rescue name"));
    }
    #[cfg(target_os = "linux")]
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(target_os = "macos")]
    let flags = libc::O_EVTONLY | libc::O_SYMLINK | libc::O_CLOEXEC;
    // SAFETY: the captured directory and NUL-terminated child remain live.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound { Ok(None) } else { Err(error) };
    }
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

/// Prepared off-gate. Opening the rescue destination must be admitted by the
/// same captured save authority that admitted the failed direct write.
pub struct PreparedRescueCopy { parent: File, name: CString, source: File, mode: u32 }

/// The exact destination descriptor, including when opening or copying had a
/// partial effect. It can be quarantined before any cleanup by name.
pub struct RescueDestination { parent: File, name: CString, file: File }

pub struct RescueCopy { source: File, destination: RescueDestination, mode: u32 }

pub enum RescueOpen {
    SameObject(RescueDestination),
    Ready(RescueCopy),
    Failed { error: io::Error, destination: RescueDestination },
}

pub enum RescueCopyError {
    BeforeOpen { error: io::Error, prepared: PreparedRescueCopy },
    AfterOpen { error: io::Error, destination: RescueDestination },
}

impl PreparedRescueCopy {
    /// Capture the held stage object and parent without consulting a pathname.
    pub fn new(parent: &File, name: &CStr, stage: &mut OwnedStage) -> io::Result<Self> {
        let bytes = name.to_bytes();
        if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid rescue name"));
        }
        let source = stage.file_mut().try_clone()?;
        let mode = source.metadata()?.mode() & 0o7777;
        Ok(Self { parent: parent.try_clone()?, name: name.to_owned(), source, mode })
    }

    /// Open and truncate only after the caller's short admission. The held
    /// source and opened destination are compared before any truncation.
    pub fn open(self) -> Result<RescueOpen, RescueCopyError> {
        let fd = unsafe { libc::openat(self.parent.as_raw_fd(), self.name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC, self.mode as libc::mode_t) };
        if fd < 0 { return Err(RescueCopyError::BeforeOpen { error: io::Error::last_os_error(), prepared: self }); }
        let file = unsafe { File::from_raw_fd(fd) };
        let destination = RescueDestination { parent: self.parent, name: self.name, file };
        match crate::same_opened_object(&self.source, &destination.file) {
            Ok(true) => return Ok(RescueOpen::SameObject(destination)),
            Err(error) => return Ok(RescueOpen::Failed { error, destination }),
            Ok(false) => {}
        }
        if let Err(error) = destination.file.set_len(0) {
            return Ok(RescueOpen::Failed { error, destination });
        }
        Ok(RescueOpen::Ready(RescueCopy { source: self.source, destination, mode: self.mode }))
    }
}

impl RescueCopy {
    /// Copy off-gate, preserving the actual destination after any failure.
    pub fn copy(mut self) -> Result<RescueDestination, RescueCopyError> {
        let result = self.destination.file.set_permissions(Permissions::from_mode(self.mode))
            .and_then(|()| self.source.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| io::copy(&mut self.source, &mut self.destination.file).map(|_| ()));
        match result {
            Ok(()) => Ok(self.destination),
            Err(error) => Err(RescueCopyError::AfterOpen { error, destination: self.destination }),
        }
    }
}

impl RescueDestination {
    /// Allocate a distinct quarantine so rescue cleanup never shares the
    /// still-retained source stage's private directory.
    pub fn private_directory(&self) -> io::Result<PrivateDirectory> {
        PrivateDirectory::create(&self.parent)
    }
    /// Preserve the descriptor on a failed quarantine preparation. Quarantine
    /// itself checks the moved entry before allowing any private deletion.
    pub fn quarantine(self, private: &PrivateDirectory) -> Result<PendingStage, (io::Error, Self)> {
        let file = match self.file.try_clone() { Ok(file) => file, Err(error) => return Err((error, self)) };
        match private.quarantine_public(&self.parent, &self.name, file) {
            Ok(pending) => Ok(pending),
            Err(error) => Err((error, self)),
        }
    }
}
