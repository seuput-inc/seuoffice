//! Exclusive namespace publication and its unforgeable receipt.
use std::{
    ffi::OsString,
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
mod rescue_unix;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use rescue_unix::capture_rescue_entry;
#[cfg(unix)]
pub use rescue_unix::{
    PreparedRescueCopy, RescueCopy, RescueCopyError, RescueDestination, RescueOpen,
};

#[cfg(windows)]
mod windows_stage;
#[cfg(windows)]
pub use windows_stage::{
    CreatedStage, OwnedPreparedCommit, OwnedStage, OwnedWrittenCommit, OwnedWrittenStage,
    PendingStage, PendingWrittenStage, PrivateDirectory, RenameUnavailableStage,
    WrittenRenameUnavailableStage, open_output_parent, open_pinned_file,
};

/// Captures an output directory without requesting permission to list it.
/// Relative names used later remain anchored to this actual directory.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn open_output_parent(path: &Path) -> io::Result<File> {
    use std::os::{fd::FromRawFd, unix::ffi::OsStrExt};
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "output parent contains NUL"))?;
    #[cfg(target_os = "macos")]
    let flags = libc::O_SEARCH | libc::O_CLOEXEC;
    #[cfg(target_os = "linux")]
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: path is a live NUL-terminated string and open returns an owned fd.
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn atomic_stage_name(target_name: &str) -> io::Result<String> {
    if target_name.is_empty()
        || target_name == "."
        || target_name == ".."
        || target_name.contains('/')
        || target_name.contains('\0')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid output name",
        ));
    }
    let mut end = target_name.len().min(200);
    while !target_name.is_char_boundary(end) {
        end -= 1;
    }
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{}.{suffix}.tmp", &target_name[..end]))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationMode {
    Exclusive,
    Replace,
    Direct,
}

/// Location of the namespace entry retained by a failed quarantine attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingStageLocation {
    Public,
    Quarantine,
}
struct Committed {
    parent: File,
    source: File,
    name: OsString,
    path: PathBuf,
    mode: PublicationMode,
    read: ReadDisposition,
    read_error: Option<(io::ErrorKind, Option<i32>, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadDisposition {
    Readable,
    WriteOnly,
    RequiresAccessCheck,
}

/// The receipt identifies a real completed publication, not immutable contents.
#[derive(Clone)]
pub struct FileCommitReceipt(Arc<Committed>);
/// Bytes read from one exact committed object. They are a read observation,
/// not an assertion that concurrent writers cannot change that object.
pub struct CommittedRead {
    receipt: FileCommitReceipt,
    bytes: Vec<u8>,
}
impl CommittedRead {
    pub fn receipt(&self) -> &FileCommitReceipt {
        &self.receipt
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_parts(self) -> (FileCommitReceipt, Vec<u8>) {
        (self.receipt, self.bytes)
    }
}
impl FileCommitReceipt {
    pub fn path(&self) -> &Path {
        &self.0.path
    }
    pub fn mode(&self) -> PublicationMode {
        self.0.mode
    }
    /// Clone the captured output parent, not whatever now occupies its path.
    pub fn captured_parent(&self) -> io::Result<File> {
        self.0.parent.try_clone()
    }
    pub fn read_disposition(&self) -> ReadDisposition {
        self.0.read
    }
    /// Metadata of the retained object at this instant, not a byte-snapshot
    /// version or a pathname compare-and-swap. Run outside admission gates.
    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.0.source.metadata()
    }
    /// Reads the retained object, never the pathname. Other writers may change
    /// bytes concurrently; this is not an immutable snapshot or a path CAS.
    pub fn read_bounded(
        &self,
        max_bytes: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> io::Result<CommittedRead> {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "committed-object read cancelled",
            ));
        }
        if let Some((kind, raw, message)) = &self.0.read_error {
            return Err(raw.map_or_else(
                || io::Error::new(*kind, message.clone()),
                io::Error::from_raw_os_error,
            ));
        }
        let readable = open_readable(&self.0.source)?;
        let mut bytes = Vec::new();
        let mut offset = 0u64;
        let mut chunk = [0u8; 8192];
        loop {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "committed-object read cancelled",
                ));
            }
            let available = max_bytes.saturating_sub(bytes.len());
            if available == 0 {
                return if read_at(&readable, &mut chunk[..1], offset)? == 0 {
                    Ok(CommittedRead {
                        receipt: self.clone(),
                        bytes,
                    })
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "committed object exceeds read limit",
                    ))
                };
            }
            let size = available.min(chunk.len());
            let count = read_at(&readable, &mut chunk[..size], offset)?;
            if count == 0 {
                return Ok(CommittedRead {
                    receipt: self.clone(),
                    bytes,
                });
            }
            bytes
                .try_reserve(count)
                .map_err(|error| io::Error::other(error.to_string()))?;
            bytes.extend_from_slice(&chunk[..count]);
            offset = offset.checked_add(count as u64).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "committed-object offset overflow",
                )
            })?;
        }
    }
    pub fn same_commit(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    /// Compares held object identities without consulting a pathname. This is
    /// metadata I/O and must run outside native admission gates.
    pub fn same_object(&self, other: &File) -> io::Result<bool> {
        same_handles(&self.0.source, other)
    }
    /// Revalidate the path against the actual directory handle used for publication.
    pub fn matches_path(&self, path: &Path) -> io::Result<bool> {
        if path != self.0.path || path.file_name() != Some(self.0.name.as_os_str()) {
            return Ok(false);
        }
        let Some(parent) = path.parent() else {
            return Ok(false);
        };
        Ok(same_parent(&self.0.parent, parent)? && same_file(&self.0.source, path)?)
    }
}

/// Compares two held objects without consulting names. This does not compare
/// versions or make two independent reads an atomic snapshot.
pub fn same_opened_object(left: &File, right: &File) -> io::Result<bool> {
    same_handles(left, right)
}

fn prepare(
    parent: &File,
    source: &File,
    name: &std::ffi::OsStr,
    path: &Path,
    mode: PublicationMode,
) -> io::Result<Arc<Committed>> {
    if path.file_name() != Some(name) || !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "commit path does not match the destination name",
        ));
    }
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "commit path has no parent"))?;
    if !same_parent(parent, directory)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "commit directory changed",
        ));
    }
    Ok(Arc::new(Committed {
        parent: parent.try_clone()?,
        source: retain_source(source)?,
        name: name.to_owned(),
        path: path.to_owned(),
        mode,
        read: source_read_disposition(source)?,
        read_error: None,
    }))
}
/// Prepared off-gate; opening truncates the actual selected path and must be
/// called only from the caller's admission. The returned descriptor is one-shot.
pub struct PreparedDirectWrite {
    parent: File,
    path: PathBuf,
    name: OsString,
}
pub struct DirectWrite {
    parent: File,
    file: File,
    path: PathBuf,
    name: OsString,
    read: ReadDisposition,
    read_error: Option<(io::ErrorKind, Option<i32>, String)>,
}

pub fn prepare_direct(parent: &File, path: &Path) -> io::Result<PreparedDirectWrite> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "commit path has no name"))?;
    if !path.is_absolute()
        || !same_parent(
            parent,
            path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "commit path has no parent")
            })?,
        )?
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "commit directory changed",
        ));
    }
    Ok(PreparedDirectWrite {
        parent: parent.try_clone()?,
        path: path.to_owned(),
        name: name.to_owned(),
    })
}

impl PreparedDirectWrite {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn open(self) -> io::Result<DirectWrite> {
        let (file, read, read_error) = match open_relative(&self.parent, &self.name, true) {
            Ok(file) => (file, readable_disposition(), None),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                let saved = (error.kind(), error.raw_os_error(), error.to_string());
                (
                    open_relative(&self.parent, &self.name, false)?,
                    ReadDisposition::WriteOnly,
                    Some(saved),
                )
            }
            Err(error) => return Err(error),
        };
        Ok(DirectWrite {
            parent: self.parent,
            file,
            path: self.path,
            name: self.name,
            read,
            read_error,
        })
    }
}

#[cfg(unix)]
fn open_relative(parent: &File, name: &std::ffi::OsStr, readable: bool) -> io::Result<File> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid output name"))?;
    let access = if readable {
        libc::O_RDWR
    } else {
        libc::O_WRONLY
    };
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            access | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
            0o666,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Opens one regular child relative to an already captured parent, without
/// following a final symlink. Separate opens are not an atomic pair snapshot.
#[cfg(unix)]
pub fn open_pinned_file(parent: &File, name: &str) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid pinned file name",
        ));
    }
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid pinned file name"))?;
    // SAFETY: the captured parent and validated child name remain live.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pinned child is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_relative(parent: &File, name: &std::ffi::OsStr, readable: bool) -> io::Result<File> {
    use std::{
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        ptr,
    };
    use windows_sys::Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_NON_DIRECTORY_FILE, FILE_OVERWRITE_IF, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    };
    use windows_sys::Win32::{
        Foundation::{
            GENERIC_READ, GENERIC_WRITE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError,
            UNICODE_STRING,
        },
        Storage::FileSystem::{
            FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            SYNCHRONIZE,
        },
        System::IO::IO_STATUS_BLOCK,
    };
    let mut units: Vec<u16> = name.encode_wide().collect();
    let bytes = units
        .len()
        .checked_mul(2)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output name too long"))?;
    if units.is_empty() || units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid output name",
        ));
    }
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: units.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle().cast(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle = ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    let access = GENERIC_WRITE | SYNCHRONIZE | if readable { GENERIC_READ } else { 0 };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status_block,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OVERWRITE_IF,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}

pub enum DirectWriteOutcome {
    Committed(FileCommitReceipt),
    WrittenWithoutReceipt(io::Error),
}

/// Original serialized bytes consumed by the real successful descriptor write.
/// This is a written snapshot, not a claim about later filesystem contents.
pub struct CommittedWriteSnapshot {
    receipt: FileCommitReceipt,
    bytes: Vec<u8>,
}
impl CommittedWriteSnapshot {
    pub fn receipt(&self) -> &FileCommitReceipt {
        &self.receipt
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_parts(self) -> (FileCommitReceipt, Vec<u8>) {
        (self.receipt, self.bytes)
    }
}
pub enum DirectOwnedWriteOutcome {
    Committed(CommittedWriteSnapshot),
    WrittenWithoutReceipt { error: io::Error, bytes: Vec<u8> },
}

/// A completed write or a failed write retaining its original descriptor and progress.
#[must_use]
pub enum DirectOwnedWriteAttempt {
    Completed(DirectOwnedWriteOutcome),
    Partial {
        error: io::Error,
        partial: PartialDirectWrite,
    },
}

/// An interrupted descriptor write retains its exact bytes and progress.
pub struct PartialDirectWrite {
    direct: DirectWrite,
    bytes: Vec<u8>,
    written: usize,
}
/// Retains an abandoned partial-write descriptor without permitting a fresh
/// snapshot to be minted from its advanced offset.
pub struct AbandonedDirectWrite {
    _direct: DirectWrite,
}
impl std::fmt::Debug for PartialDirectWrite {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PartialDirectWrite")
            .field("written", &self.written)
            .field("length", &self.bytes.len())
            .finish()
    }
}
impl PartialDirectWrite {
    pub fn written(&self) -> usize {
        self.written
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Relinquish resumable snapshot evidence while retaining the descriptor.
    pub fn into_parts(self) -> (AbandonedDirectWrite, Vec<u8>, usize) {
        (
            AbandonedDirectWrite {
                _direct: self.direct,
            },
            self.bytes,
            self.written,
        )
    }
    pub fn resume(mut self) -> DirectOwnedWriteAttempt {
        use std::io::Write;
        while self.written < self.bytes.len() {
            match self.direct.file.write(&self.bytes[self.written..]) {
                Ok(0) => {
                    return DirectOwnedWriteAttempt::Partial {
                        error: io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write the saved output",
                        ),
                        partial: self,
                    };
                }
                Ok(count) => self.written += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return DirectOwnedWriteAttempt::Partial {
                        error,
                        partial: self,
                    };
                }
            }
        }
        match self.direct.finish() {
            DirectWriteOutcome::Committed(receipt) => DirectOwnedWriteAttempt::Completed(
                DirectOwnedWriteOutcome::Committed(CommittedWriteSnapshot {
                    receipt,
                    bytes: self.bytes,
                }),
            ),
            DirectWriteOutcome::WrittenWithoutReceipt(error) => {
                DirectOwnedWriteAttempt::Completed(DirectOwnedWriteOutcome::WrittenWithoutReceipt {
                    error,
                    bytes: self.bytes,
                })
            }
        }
    }
}

impl DirectWrite {
    pub fn write_owned(self, bytes: Vec<u8>) -> DirectOwnedWriteAttempt {
        PartialDirectWrite {
            direct: self,
            bytes,
            written: 0,
        }
        .resume()
    }
    /// A write error is distinct from a completed write whose proof failed.
    pub fn write_all(mut self, bytes: &[u8]) -> io::Result<DirectWriteOutcome> {
        use std::io::Write;
        self.file.write_all(bytes)?;
        Ok(self.finish())
    }
    fn finish(self) -> DirectWriteOutcome {
        let source = match retain_source(&self.file) {
            Ok(source) => source,
            Err(error) => return DirectWriteOutcome::WrittenWithoutReceipt(error),
        };
        let receipt = FileCommitReceipt(Arc::new(Committed {
            parent: self.parent,
            source,
            name: self.name,
            path: self.path,
            mode: PublicationMode::Direct,
            read: self.read,
            read_error: self.read_error,
        }));
        match receipt.matches_path(receipt.path()) {
            Ok(true) => DirectWriteOutcome::Committed(receipt),
            Ok(false) => DirectWriteOutcome::WrittenWithoutReceipt(io::Error::new(
                io::ErrorKind::InvalidData,
                "written file changed before acknowledgement",
            )),
            Err(error) => DirectWriteOutcome::WrittenWithoutReceipt(error),
        }
    }
}

#[cfg(unix)]
fn source_read_disposition(source: &File) -> io::Result<ReadDisposition> {
    use std::os::fd::AsRawFd;
    let flags = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(if flags & libc::O_ACCMODE == libc::O_WRONLY {
        ReadDisposition::WriteOnly
    } else {
        ReadDisposition::Readable
    })
}
#[cfg(windows)]
fn source_read_disposition(_: &File) -> io::Result<ReadDisposition> {
    Ok(ReadDisposition::RequiresAccessCheck)
}
#[cfg(unix)]
fn readable_disposition() -> ReadDisposition {
    ReadDisposition::Readable
}
#[cfg(windows)]
fn readable_disposition() -> ReadDisposition {
    ReadDisposition::RequiresAccessCheck
}

#[cfg(unix)]
fn open_readable(source: &File) -> io::Result<File> {
    source.try_clone()
}
#[cfg(windows)]
fn open_readable(source: &File) -> io::Result<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, ReOpenFile,
        },
    };
    let handle = unsafe {
        ReOpenFile(
            source.as_raw_handle().cast(),
            FILE_READ_DATA | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}
#[cfg(unix)]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buffer, offset)
}
#[cfg(windows)]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buffer, offset)
}

#[cfg(unix)]
fn retain_source(source: &File) -> io::Result<File> {
    source.try_clone()
}

#[cfg(windows)]
fn retain_source(source: &File) -> io::Result<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, ReOpenFile,
        },
    };
    // The project stage requests READ_ATTRIBUTES|WRITE_DATA|WRITE_ATTRIBUTES|DELETE
    // plus SYNCHRONIZE, with share=0. Windows documents attribute access as
    // exempt from sharing checks. ReOpenFile targets that same object, while
    // the new share-all handle must permit the original handle's access.
    // Hosted Windows proof is required for the actual filesystem/flag behavior.
    let handle = unsafe {
        ReOpenFile(
            source.as_raw_handle().cast(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}

#[cfg(unix)]
fn same_parent(parent: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = parent.metadata()?;
    let current = std::fs::metadata(path)?;
    Ok(held.dev() == current.dev() && held.ino() == current.ino())
}

#[cfg(unix)]
fn same_handles(left: &File, right: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let a = left.metadata()?;
    let b = right.metadata()?;
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

#[cfg(windows)]
fn same_handles(left: &File, right: &File) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut a = BY_HANDLE_FILE_INFORMATION::default();
    let mut b = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(left.as_raw_handle().cast(), &mut a) } == 0
        || unsafe { GetFileInformationByHandle(right.as_raw_handle().cast(), &mut b) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(a.dwVolumeSerialNumber == b.dwVolumeSerialNumber
        && a.nFileIndexHigh == b.nFileIndexHigh
        && a.nFileIndexLow == b.nFileIndexLow)
}

#[cfg(unix)]
fn same_file(source: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = source.metadata()?;
    let current = std::fs::metadata(path)?;
    Ok(held.dev() == current.dev() && held.ino() == current.ino())
}

/// A directory created exclusively by this process for a linear transaction.
/// Its 0700 permissions do not defend against hostile processes of the same
/// user. The transaction must not concurrently mutate its owned names.
#[cfg(unix)]
#[derive(Clone)]
pub struct PrivateDirectory(Arc<PrivateDirectoryInner>);

#[cfg(unix)]
struct PrivateDirectoryInner {
    parent: File,
    directory: File,
    name: std::ffi::CString,
}

#[cfg(unix)]
pub struct OwnedStage {
    directory: PrivateDirectory,
    public_parent: File,
    public_name: std::ffi::CString,
    name: std::ffi::CString,
    file: File,
}

/// A real created file whose public name has not yet been verified in quarantine.
/// On failure the caller must retain this value until retry or inverse succeeds.
#[cfg(unix)]
pub struct PendingStage {
    directory: PrivateDirectory,
    public_parent: File,
    name: std::ffi::CString,
    quarantine_name: std::ffi::CString,
    file: File,
    location: PendingStageLocation,
}

#[cfg(unix)]
pub struct PendingWrittenStage {
    stage: PendingStage,
    bytes: Vec<u8>,
}

#[cfg(unix)]
impl PendingWrittenStage {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Discard snapshot evidence while retaining both the actual stage and bytes.
    pub fn into_parts(self) -> (PendingStage, Vec<u8>) {
        (self.stage, self.bytes)
    }
    pub fn quarantine(self) -> Result<OwnedWrittenStage, (io::Error, Self)> {
        match self.stage.quarantine() {
            Ok(stage) => Ok(OwnedWrittenStage {
                stage,
                bytes: self.bytes,
            }),
            Err((error, stage)) => Err((
                error,
                Self {
                    stage,
                    bytes: self.bytes,
                },
            )),
        }
    }
}
#[cfg(unix)]
fn random_private_name(prefix: &str) -> io::Result<std::ffi::CString> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(std::ffi::CString::new(format!(
        "{prefix}{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
    .expect("generated ASCII name"))
}

#[cfg(unix)]
fn rename_exclusive(
    from: &File,
    from_name: &std::ffi::CStr,
    to: &File,
    to_name: &std::ffi::CStr,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            from_name.as_ptr(),
            to.as_raw_fd(),
            to_name.as_ptr(),
            libc::RENAME_EXCL,
        ) as i64
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from.as_raw_fd(),
            from_name.as_ptr(),
            to.as_raw_fd(),
            to_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exclusive rename is unavailable",
    ));
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn named_object_matches(directory: &File, name: &std::ffi::CStr, file: &File) -> io::Result<bool> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    let expected = file.metadata()?;
    let mut actual = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the directory and name are held and fstatat initializes stat on success.
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            actual.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let actual = unsafe { actual.assume_init() };
    Ok(stat_dev(&actual) == expected.dev() && actual.st_ino == expected.ino())
}

#[cfg(unix)]
fn stat_dev(stat: &libc::stat) -> u64 {
    // macOS declares st_dev as a signed 32-bit dev_t; std's MetadataExt::dev
    // sign-extends it the same way. Linux declares it as u64 already.
    #[cfg(target_os = "linux")]
    {
        stat.st_dev
    }
    #[cfg(not(target_os = "linux"))]
    {
        stat.st_dev as u64
    }
}

#[cfg(unix)]
impl PrivateDirectory {
    pub fn create(parent: &File) -> io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        for _ in 0..16 {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
            let name = std::ffi::CString::new(format!(
                ".seuoffice-stage-{}",
                random
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ))
            .expect("generated ASCII name");
            // SAFETY: parent and name remain valid for both descriptor-relative calls.
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(error);
            }
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                unsafe {
                    libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
                }
                return Err(error);
            }
            let directory = unsafe { File::from_raw_fd(fd) };
            let parent = match parent.try_clone() {
                Ok(parent) => parent,
                Err(error) => {
                    unsafe {
                        libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
                    }
                    return Err(error);
                }
            };
            return Ok(Self(Arc::new(PrivateDirectoryInner {
                parent,
                directory,
                name,
            })));
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "private staging names exhausted",
        ))
    }

    /// Create in the original parent so the OS applies its normal mode and ACL.
    pub fn create_stage(&self, mode: u32) -> io::Result<PendingStage> {
        self.create_stage_with(mode, || {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
            Ok(format!(
                ".seuoffice-new-{}.tmp",
                random
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ))
        })
    }

    /// Atomic-write-compatible `<target>.<12 hex>.tmp` in the original parent.
    pub fn create_stage_for(&self, target_name: &str, mode: u32) -> io::Result<PendingStage> {
        self.create_stage_with(mode, || atomic_stage_name(target_name))
    }

    fn create_stage_with(
        &self,
        mode: u32,
        mut next_name: impl FnMut() -> io::Result<String>,
    ) -> io::Result<PendingStage> {
        let public_parent = self.0.parent.try_clone()?;
        let quarantine_name = random_private_name("entry-")?;
        use std::os::fd::{AsRawFd, FromRawFd};
        for _ in 0..16 {
            let name = std::ffi::CString::new(next_name()?)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid staging name"))?;
            // SAFETY: the parent handle and generated name remain live for openat.
            let fd = unsafe {
                libc::openat(
                    self.0.parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    mode as libc::c_uint,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(error);
            }
            return Ok(PendingStage {
                directory: self.clone(),
                public_parent,
                name,
                quarantine_name,
                file: unsafe { File::from_raw_fd(fd) },
                location: PendingStageLocation::Public,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "temporary staging names exhausted",
        ))
    }

    /// Capture a selected public name and its independently retained object.
    /// The name is not trusted until quarantine verifies the moved object.
    pub fn quarantine_public(
        &self,
        parent: &File,
        name: &std::ffi::CStr,
        file: File,
    ) -> io::Result<PendingStage> {
        let bytes = name.to_bytes();
        if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid public source name",
            ));
        }
        Ok(PendingStage {
            directory: self.clone(),
            public_parent: parent.try_clone()?,
            name: name.to_owned(),
            quarantine_name: random_private_name("entry-")?,
            file,
            location: PendingStageLocation::Public,
        })
    }

    /// Reacquire the exact committed object through its retained parent/name.
    /// The subsequent quarantine still checks the actual moved object and
    /// retains an inverse if a different public object occupied that name.
    pub fn quarantine_receipt(&self, receipt: &FileCommitReceipt) -> io::Result<PendingStage> {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(receipt.0.name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid committed name"))?;
        self.quarantine_public(&receipt.0.parent, &name, receipt.0.source.try_clone()?)
    }

    /// Removes only an empty private directory. Errors remain visible to the caller.
    pub fn remove(&self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        if unsafe {
            libc::unlinkat(
                self.0.parent.as_raw_fd(),
                self.0.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
impl PendingStage {
    pub fn directory(&self) -> PrivateDirectory {
        self.directory.clone()
    }
    pub fn location(&self) -> PendingStageLocation {
        self.location
    }
    pub fn is_quarantined(&self) -> bool {
        self.location == PendingStageLocation::Quarantine
    }

    pub fn write_owned(
        mut self,
        bytes: Vec<u8>,
    ) -> Result<PendingWrittenStage, (io::Error, Self, Vec<u8>)> {
        use std::io::{Seek, SeekFrom, Write};
        let result = self
            .file
            .set_len(0)
            .and_then(|()| self.file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| self.file.write_all(&bytes));
        match result {
            Ok(()) => Ok(PendingWrittenStage { stage: self, bytes }),
            Err(error) => Err((error, self, bytes)),
        }
    }

    /// Move first, then inspect the moved namespace entry against the actual
    /// creation descriptor. A wrong entry stays owned for a NOREPLACE inverse.
    pub fn quarantine(mut self) -> Result<OwnedStage, (io::Error, Self)> {
        if self.location == PendingStageLocation::Public {
            if let Err(error) = rename_exclusive(
                &self.public_parent,
                &self.name,
                &self.directory.0.directory,
                &self.quarantine_name,
            ) {
                return Err((error, self));
            }
            self.location = PendingStageLocation::Quarantine;
        }
        match named_object_matches(
            &self.directory.0.directory,
            &self.quarantine_name,
            &self.file,
        ) {
            Ok(true) => Ok(OwnedStage {
                directory: self.directory,
                public_parent: self.public_parent,
                public_name: self.name,
                name: self.quarantine_name,
                file: self.file,
            }),

            Ok(false) => Err((
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "temporary stage was replaced before quarantine",
                ),
                self,
            )),
            Err(error) => Err((error, self)),
        }
    }

    /// Restore the exact moved entry without replacing a new public occupant.
    pub fn restore(mut self) -> Result<(), (io::Error, Self)> {
        if self.location == PendingStageLocation::Public {
            return Err((
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stage has not entered quarantine",
                ),
                self,
            ));
        }
        if let Err(error) = rename_exclusive(
            &self.directory.0.directory,
            &self.quarantine_name,
            &self.public_parent,
            &self.name,
        ) {
            return Err((error, self));
        }
        self.location = PendingStageLocation::Public;
        Ok(())
    }
}

#[cfg(unix)]
impl OwnedStage {
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
    pub fn directory(&self) -> PrivateDirectory {
        self.directory.clone()
    }
    /// Retain the actual original output directory, not its current pathname.
    pub fn original_parent(&self) -> io::Result<File> {
        self.public_parent.try_clone()
    }

    pub fn restore(self) -> Result<(), (io::Error, Self)> {
        if let Err(error) = rename_exclusive(
            &self.directory.0.directory,
            &self.name,
            &self.public_parent,
            &self.public_name,
        ) {
            return Err((error, self));
        }
        Ok(())
    }

    /// Remove an unpublished stage while the transaction still owns its name.
    pub fn remove(self) -> Result<(), (io::Error, Self)> {
        use std::os::fd::AsRawFd;
        if unsafe {
            libc::unlinkat(
                self.directory.0.directory.as_raw_fd(),
                self.name.as_ptr(),
                0,
            )
        } != 0
        {
            return Err((io::Error::last_os_error(), self));
        }
        Ok(())
    }
}

/// A sealed staging object and the exact owned payload successfully written to it.
#[cfg(unix)]
pub struct OwnedWrittenStage {
    stage: OwnedStage,
    bytes: Vec<u8>,
}
#[cfg(unix)]
pub struct OwnedWrittenCommit {
    plan: OwnedPreparedCommit,
    bytes: Vec<u8>,
}

#[cfg(unix)]
impl OwnedStage {
    pub fn write_owned(
        mut self,
        bytes: Vec<u8>,
    ) -> Result<OwnedWrittenStage, (io::Error, Self, Vec<u8>)> {
        use std::io::{Seek, SeekFrom, Write};
        let result = self
            .file
            .set_len(0)
            .and_then(|()| self.file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| self.file.write_all(&bytes));
        match result {
            Ok(()) => Ok(OwnedWrittenStage { stage: self, bytes }),
            Err(error) => Err((error, self, bytes)),
        }
    }
}

#[cfg(unix)]
impl OwnedWrittenStage {
    /// Flush the actual staged object without reopening its public name.
    pub fn sync_all(&self) -> io::Result<()> {
        self.stage.file.sync_all()
    }
    /// Give up snapshot evidence while retaining the exact stage for rescue.
    pub fn into_parts(self) -> (OwnedStage, Vec<u8>) {
        (self.stage, self.bytes)
    }
    pub fn prepare(
        self,
        parent: &File,
        name: &std::ffi::CStr,
        path: &Path,
        mode: PublicationMode,
    ) -> Result<OwnedWrittenCommit, (io::Error, Self)> {
        match self.stage.prepare(parent, name, path, mode) {
            Ok(plan) => Ok(OwnedWrittenCommit {
                plan,
                bytes: self.bytes,
            }),
            Err((error, stage)) => Err((
                error,
                Self {
                    stage,
                    bytes: self.bytes,
                },
            )),
        }
    }
}

#[cfg(unix)]
impl OwnedWrittenCommit {
    pub fn path(&self) -> &Path {
        &self.plan.plan.receipt.path
    }
    pub fn mode(&self) -> PublicationMode {
        self.plan.plan.receipt.mode
    }
    pub fn into_parts(self) -> (OwnedStage, Vec<u8>) {
        (self.plan.into_stage(), self.bytes)
    }
    /// On failure the exact commit, with its frozen destination and payload, is
    /// returned boxed so a retry or rescue keeps every right; only the failure
    /// path allocates.
    pub fn publish(self) -> Result<CommittedWriteSnapshot, Box<(io::Error, Self)>> {
        match self.plan.publish() {
            Ok(receipt) => Ok(CommittedWriteSnapshot {
                receipt,
                bytes: self.bytes,
            }),
            Err((error, plan)) => Err(Box::new((
                error,
                Self {
                    plan,
                    bytes: self.bytes,
                },
            ))),
        }
    }
}

#[cfg(unix)]
pub struct OwnedPreparedCommit {
    stage: OwnedStage,
    plan: PreparedCommit,
}

#[cfg(unix)]
impl OwnedStage {
    pub fn prepare(
        self,
        parent: &File,
        name: &std::ffi::CStr,
        path: &Path,
        mode: PublicationMode,
    ) -> Result<OwnedPreparedCommit, (io::Error, Self)> {
        match prepare_commit_between(
            &self.directory.0.directory,
            &self.file,
            &self.name,
            parent,
            name,
            path,
            mode,
        ) {
            Ok(plan) => Ok(OwnedPreparedCommit { stage: self, plan }),
            Err(error) => Err((error, self)),
        }
    }
}

#[cfg(unix)]
impl OwnedPreparedCommit {
    pub fn stage(&mut self) -> &mut OwnedStage {
        &mut self.stage
    }
    pub fn into_stage(self) -> OwnedStage {
        self.stage
    }

    pub fn publish(self) -> Result<FileCommitReceipt, (io::Error, Self)> {
        use std::os::fd::AsRawFd;
        let result = match self.plan.receipt.mode {
            PublicationMode::Replace => {
                // SAFETY: both directory handles and names are held by this plan.
                if unsafe {
                    libc::renameat(
                        self.plan.from_parent.as_raw_fd(),
                        self.plan.from.as_ptr(),
                        self.plan.receipt.parent.as_raw_fd(),
                        self.plan.to.as_ptr(),
                    )
                } == 0
                {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            }
            PublicationMode::Exclusive => rename_exclusive(
                &self.plan.from_parent,
                &self.plan.from,
                &self.plan.receipt.parent,
                &self.plan.to,
            ),
            PublicationMode::Direct => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct publication requires prepare_direct",
            )),
        };
        if let Err(error) = result {
            return Err((error, self));
        }
        Ok(FileCommitReceipt(self.plan.receipt))
    }
}

/// A prepared publication captures handles and verifies the destination off the
/// admission gate; `publish` performs only the actual namespace operation.
pub struct PreparedCommit {
    receipt: Arc<Committed>,
    #[cfg(unix)]
    from_parent: File,
    #[cfg(unix)]
    from: std::ffi::CString,
    #[cfg(unix)]
    to: std::ffi::CString,
    #[cfg(windows)]
    leaf: File,
}

#[cfg(unix)]
pub fn prepare_commit(
    parent: &File,
    source: &File,
    from: &std::ffi::CStr,
    to: &std::ffi::CStr,
    path: &Path,
    mode: PublicationMode,
) -> io::Result<PreparedCommit> {
    prepare_commit_between(parent, source, from, parent, to, path, mode)
}

#[cfg(unix)]
pub fn prepare_commit_between(
    from_parent: &File,
    source: &File,
    from: &std::ffi::CStr,
    to_parent: &File,
    to: &std::ffi::CStr,
    path: &Path,
    mode: PublicationMode,
) -> io::Result<PreparedCommit> {
    if mode == PublicationMode::Direct {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "direct publication requires prepare_direct",
        ));
    }
    use std::os::unix::ffi::OsStrExt;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "commit path has no name"))?;
    if name.as_bytes() != to.to_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "commit name changed",
        ));
    }
    Ok(PreparedCommit {
        receipt: prepare(to_parent, source, name, path, mode)?,
        from_parent: from_parent.try_clone()?,
        from: from.to_owned(),
        to: to.to_owned(),
    })
}

#[cfg(unix)]
impl PreparedCommit {
    pub fn publish(self) -> io::Result<FileCommitReceipt> {
        use std::os::fd::AsRawFd;
        let from = self.from_parent.as_raw_fd();
        let to = self.receipt.parent.as_raw_fd();
        let result = match self.receipt.mode {
            PublicationMode::Replace => unsafe {
                libc::renameat(from, self.from.as_ptr(), to, self.to.as_ptr()) as i64
            },
            #[cfg(target_os = "macos")]
            PublicationMode::Exclusive => unsafe {
                libc::renameatx_np(
                    from,
                    self.from.as_ptr(),
                    to,
                    self.to.as_ptr(),
                    libc::RENAME_EXCL,
                ) as i64
            },
            #[cfg(target_os = "linux")]
            PublicationMode::Exclusive => unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    from,
                    self.from.as_ptr(),
                    to,
                    self.to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            },
            PublicationMode::Direct => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "direct publication requires prepare_direct",
                ));
            }
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileCommitReceipt(self.receipt))
    }
}

#[cfg(windows)]
fn same_parent(parent: &File, path: &Path) -> io::Result<bool> {
    let current = open_output_parent(path)?;
    same_handles(parent, &current)
}

#[cfg(windows)]
fn same_file(source: &File, path: &Path) -> io::Result<bool> {
    use std::{
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            GetFileInformationByHandle, OPEN_EXISTING,
        },
    };
    let canonical = std::fs::canonicalize(path)?;
    let wide: Vec<u16> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let current = unsafe { File::from_raw_handle(handle.cast()) };
    let mut a = BY_HANDLE_FILE_INFORMATION::default();
    let mut b = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(source.as_raw_handle().cast(), &mut a) } == 0
        || unsafe { GetFileInformationByHandle(current.as_raw_handle().cast(), &mut b) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(a.dwVolumeSerialNumber == b.dwVolumeSerialNumber
        && a.nFileIndexHigh == b.nFileIndexHigh
        && a.nFileIndexLow == b.nFileIndexLow)
}

/// Renames the open file to `name` in the directory `parent`. `SetFileInformationByHandle`
/// cannot do this: it rewrites the name as a path from the current directory and keeps the
/// root handle, which the kernel refuses as an invalid parameter.
#[cfg(windows)]
pub fn rename_handle(leaf: &File, parent: &File, name: &str, replace: bool) -> io::Result<()> {
    use std::{os::windows::io::AsRawHandle, ptr};
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
    };
    use windows_sys::Win32::{Foundation::RtlNtStatusToDosError, System::IO::IO_STATUS_BLOCK};
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.chars().any(|c| matches!(c, '\0' | '/' | '\\' | ':'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid destination name",
        ));
    }
    let name: Vec<u16> = name.encode_utf16().collect();
    let bytes = name
        .len()
        .checked_mul(2)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination name too long"))?;
    let offset = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let size = std::mem::size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(bytes as usize)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination name too long"))?;
    let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: storage is aligned, zeroed and covers the header plus the name; both handles
    // and the status block outlive the call.
    let status = unsafe {
        (*info).Anonymous.ReplaceIfExists = replace;
        (*info).RootDirectory = parent.as_raw_handle().cast();
        (*info).FileNameLength = bytes;
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            storage.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
            name.len(),
        );
        NtSetInformationFile(
            leaf.as_raw_handle().cast(),
            &mut status_block,
            info.cast(),
            size,
            FileRenameInformation,
        )
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub fn prepare_commit(
    leaf: &File,
    parent: &File,
    name: &str,
    path: &Path,
    mode: PublicationMode,
) -> io::Result<PreparedCommit> {
    if mode == PublicationMode::Direct {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "direct publication requires prepare_direct",
        ));
    }
    Ok(PreparedCommit {
        receipt: prepare(parent, leaf, std::ffi::OsStr::new(name), path, mode)?,
        leaf: leaf.try_clone()?,
    })
}

#[cfg(windows)]
impl PreparedCommit {
    pub fn publish(self) -> io::Result<FileCommitReceipt> {
        let name = self.receipt.name.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "non-Unicode destination name")
        })?;
        rename_handle(
            &self.leaf,
            &self.receipt.parent,
            name,
            self.receipt.mode == PublicationMode::Replace,
        )?;
        Ok(FileCommitReceipt(self.receipt))
    }
}

#[cfg(test)]
mod direct_write_tests {
    use super::*;

    #[test]
    fn failed_owned_write_retains_bytes_and_descriptor_for_retry() {
        let path = std::env::current_exe().unwrap();
        let file = File::open(&path).unwrap();
        let direct = DirectWrite {
            parent: file.try_clone().unwrap(),
            file,
            name: path.file_name().unwrap().to_owned(),
            path,
            read: ReadDisposition::Readable,
            read_error: None,
        };
        let bytes = b"retained output".to_vec();
        let pointer = bytes.as_ptr();
        let partial = match direct.write_owned(bytes) {
            DirectOwnedWriteAttempt::Partial { partial, .. } => partial,
            DirectOwnedWriteAttempt::Completed(_) => {
                panic!("read-only descriptor accepted a write")
            }
        };
        assert_eq!(partial.written(), 0);
        assert_eq!(partial.bytes(), b"retained output");
        assert_eq!(partial.bytes().as_ptr(), pointer);
        let partial = match partial.resume() {
            DirectOwnedWriteAttempt::Partial { partial, .. } => partial,
            DirectOwnedWriteAttempt::Completed(_) => panic!("retry unexpectedly completed"),
        };
        assert_eq!(partial.written(), 0);
        let (_descriptor, bytes, written) = partial.into_parts();
        assert_eq!(bytes.as_ptr(), pointer);
        assert_eq!(bytes, b"retained output");
        assert_eq!(written, 0);
    }
}
