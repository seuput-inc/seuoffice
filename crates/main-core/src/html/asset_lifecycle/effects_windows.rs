use crate::node_fs::{NodeError, namespaced, node_error, node_error_named};
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    INVALID_HANDLE_VALUE, LocalFree, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CALLBACK_STREAM_SWITCH, COPY_FILE_FAIL_IF_EXISTS, CopyFileExW,
    CreateFileW, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_RENAME_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_DATA,
    FileDispositionInfo, FileRenameInfo, GetFileInformationByHandle, GetFinalPathNameByHandleW,
    OPEN_EXISTING, PROGRESS_CANCEL, PROGRESS_CONTINUE, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

type R<T> = Result<T, NodeError>;

#[derive(Debug)]
pub(super) struct Directory {
    pub(super) file: File,
    pub(super) path: String,
}

fn wide(path: &str) -> R<Vec<u16>> {
    if path.contains('\0') {
        return Err(node_error_named("EINVAL", "open", path));
    }
    Ok(namespaced(path)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect())
}

pub(super) fn information(file: &File, path: &str) -> R<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file and the writable output remain live.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
        return Err(node_error(&io::Error::last_os_error(), "fstat", path));
    }
    Ok(info)
}

pub(super) fn same_identity(
    a: &BY_HANDLE_FILE_INFORMATION,
    b: &BY_HANDLE_FILE_INFORMATION,
) -> bool {
    a.dwVolumeSerialNumber == b.dwVolumeSerialNumber
        && a.nFileIndexHigh == b.nFileIndexHigh
        && a.nFileIndexLow == b.nFileIndexLow
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Version {
    volume: u32,
    high: u32,
    low: u32,
    size_high: u32,
    size_low: u32,
    time_high: u32,
    time_low: u32,
}

pub(super) fn version(file: &File, path: &str) -> R<Version> {
    let info = information(file, path)?;
    Ok(Version {
        volume: info.dwVolumeSerialNumber,
        high: info.nFileIndexHigh,
        low: info.nFileIndexLow,
        size_high: info.nFileSizeHigh,
        size_low: info.nFileSizeLow,
        time_high: info.ftLastWriteTime.dwHighDateTime,
        time_low: info.ftLastWriteTime.dwLowDateTime,
    })
}

impl Version {
    pub(super) fn same_object(&self, other: &Self) -> bool {
        self.volume == other.volume && self.high == other.high && self.low == other.low
    }
}

pub(super) struct PinnedSource {
    file: Arc<File>,
    version: Version,
    root_input: String,
    source_input: String,
    root: PathBuf,
    source: PathBuf,
    _ancestors: Vec<Directory>,
}

fn canonical(path: &str) -> R<PathBuf> {
    std::fs::canonicalize(namespaced(path)).map_err(|error| node_error(&error, "realpath", path))
}

fn path_text(path: &Path, display: &str) -> R<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| node_error_named("EINVAL", "realpath", display))
}

fn open_pinned_source(path: &Path, display: &str) -> R<File> {
    let name = wide(&path_text(path, display)?)?;
    // SAFETY: the name is NUL terminated and all optional pointers are null.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_READ_DATA | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(node_error(&io::Error::last_os_error(), "open", display));
    }
    // SAFETY: CreateFileW returned a newly owned handle.
    real_file(unsafe { File::from_raw_handle(handle.cast()) }, display)
}

pub(super) fn pin_source(root: &str, path: &str) -> R<PinnedSource> {
    let root_path = canonical(root)?;
    let source_path = canonical(path)?;
    if !source_path.starts_with(&root_path) || source_path == root_path {
        return Err(NodeError {
            code: None,
            message: "asset source is outside its root".into(),
        });
    }
    let parent = source_path
        .parent()
        .ok_or_else(|| node_error_named("EINVAL", "open", path))?;
    let mut ancestors = Vec::new();
    for ancestor in parent.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let name = path_text(ancestor, path)?;
        ancestors.push(Directory::open(&name, false)?);
    }
    let file = open_pinned_source(&source_path, path)?;
    let version = version(&file, path)?;
    let pinned = PinnedSource {
        file: Arc::new(file),
        version,
        root_input: root.into(),
        source_input: path.into(),
        root: root_path,
        source: source_path,
        _ancestors: ancestors,
    };
    pinned.validate()?;
    Ok(pinned)
}

impl PinnedSource {
    pub(super) fn validate(&self) -> R<()> {
        for ancestor in &self._ancestors {
            if !ancestor.matches_path(&ancestor.path, false)? {
                return Err(NodeError {
                    code: None,
                    message: "asset source ancestor changed during copy".into(),
                });
            }
        }
        let current = open_pinned_source(&self.source, &self.source_input)?;
        if version(&current, &self.source_input)? != self.version {
            return Err(NodeError {
                code: None,
                message: "asset source identity changed during copy".into(),
            });
        }
        if canonical(&self.root_input)? != self.root
            || canonical(&self.source_input)? != self.source
            || version(&self.file, &self.source_input)? != self.version
        {
            return Err(NodeError {
                code: None,
                message: "asset source changed during copy".into(),
            });
        }
        Ok(())
    }

    pub(super) fn copy_to(&self, destination: &Directory, name: &str) -> R<()> {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.chars().any(|ch| matches!(ch, '\0' | '/' | '\\' | ':'))
        {
            return Err(node_error_named("EINVAL", "copyfile", &destination.path));
        }
        self.validate()?;
        if !destination.matches_path(&destination.path, false)? {
            return Err(NodeError {
                code: None,
                message: "asset staging directory changed".into(),
            });
        }
        let target = jscompat::node_path::host::join(&[&destination.path, name]);
        let source_name = wide(&path_text(&self.source, &self.source_input)?)?;
        let destination_name = wide(&target)?;
        let mut context = CopyContext {
            source: self,
            destination,
            name,
            error: None,
            saw_stream: false,
        };
        // SAFETY: the UTF-16 names and callback context remain live until CopyFileExW returns.
        let copied = unsafe {
            CopyFileExW(
                source_name.as_ptr(),
                destination_name.as_ptr(),
                Some(copy_progress),
                (&mut context as *mut CopyContext<'_>).cast(),
                ptr::null_mut(),
                COPY_FILE_FAIL_IF_EXISTS,
            )
        };
        if let Some(error) = context.error {
            return Err(error);
        }
        if copied == 0 {
            return Err(node_error(&io::Error::last_os_error(), "copyfile", &target));
        }
        if !context.saw_stream {
            return Err(NodeError {
                code: None,
                message: "asset copy did not attest its stream".into(),
            });
        }
        self.validate()
    }
}

struct CopyContext<'a> {
    source: &'a PinnedSource,
    destination: &'a Directory,
    name: &'a str,
    error: Option<NodeError>,
    saw_stream: bool,
}

unsafe extern "system" fn copy_progress(
    _total: i64,
    _transferred: i64,
    _size: i64,
    _stream_transferred: i64,
    _stream: u32,
    reason: u32,
    source: windows_sys::Win32::Foundation::HANDLE,
    destination: windows_sys::Win32::Foundation::HANDLE,
    data: *const std::ffi::c_void,
) -> u32 {
    if reason != CALLBACK_STREAM_SWITCH {
        return PROGRESS_CONTINUE;
    }
    // SAFETY: CopyFileExW receives this exclusive context for its synchronous call.
    let context = unsafe { &mut *data.cast_mut().cast::<CopyContext<'_>>() };
    let result = (|| -> R<()> {
        let mut source_info = BY_HANDLE_FILE_INFORMATION::default();
        let mut destination_info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: callback handles are valid for this invocation and outputs are writable.
        if unsafe { GetFileInformationByHandle(source, &mut source_info) } == 0 {
            return Err(node_error(
                &io::Error::last_os_error(),
                "fstat",
                &context.source.source_input,
            ));
        }
        // SAFETY: callback handles are valid for this invocation and outputs are writable.
        if unsafe { GetFileInformationByHandle(destination, &mut destination_info) } == 0 {
            return Err(node_error(
                &io::Error::last_os_error(),
                "fstat",
                &context.destination.path,
            ));
        }
        if !same_identity(
            &information(&context.source.file, &context.source.source_input)?,
            &source_info,
        ) {
            return Err(NodeError {
                code: None,
                message: "asset copy source identity changed".into(),
            });
        }
        if !context
            .destination
            .matches_path(&context.destination.path, false)?
        {
            return Err(NodeError {
                code: None,
                message: "asset staging directory changed".into(),
            });
        }
        let target = jscompat::node_path::host::join(&[&context.destination.path, context.name]);
        let expected = child(
            &context.destination.file,
            context.name,
            &target,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            false,
            false,
        )?;
        if !same_identity(&information(&expected, &target)?, &destination_info) {
            return Err(NodeError {
                code: None,
                message: "asset copy destination identity changed".into(),
            });
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            context.saw_stream = true;
            PROGRESS_CONTINUE
        }
        Err(error) => {
            context.error = Some(error);
            PROGRESS_CANCEL
        }
    }
}

fn actual_path(file: &File, display: &str) -> R<String> {
    // SAFETY: the handle is live; a zero buffer queries the required size.
    let size =
        unsafe { GetFinalPathNameByHandleW(file.as_raw_handle().cast(), ptr::null_mut(), 0, 0) };
    if size == 0 {
        return Err(node_error(&io::Error::last_os_error(), "realpath", display));
    }
    let mut units = vec![0u16; size as usize + 1];
    // SAFETY: units has the requested writable capacity.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle().cast(),
            units.as_mut_ptr(),
            units.len() as u32,
            0,
        )
    };
    if length == 0 || length as usize >= units.len() {
        return Err(node_error(&io::Error::last_os_error(), "realpath", display));
    }
    Ok(String::from_utf16_lossy(&units[..length as usize]))
}

fn child(
    parent: &File,
    name: &str,
    path: &str,
    access: u32,
    share: u32,
    disposition: u32,
    directory: bool,
    private: bool,
) -> R<File> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.chars().any(|ch| matches!(ch, '\0' | '/' | '\\' | ':'))
    {
        return Err(node_error_named("EINVAL", "open", path));
    }
    let mut units: Vec<u16> = name.encode_utf16().collect();
    let bytes = units
        .len()
        .checked_mul(2)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| node_error_named("ENAMETOOLONG", "open", path))?;
    let object_name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: units.as_mut_ptr(),
    };
    let mut descriptor = ptr::null_mut();
    if private {
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        let sddl: Vec<u16> = "D:P(A;;FA;;;OW)"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: sddl is NUL terminated; descriptor is valid writable output.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(node_error(&io::Error::last_os_error(), "mkdir", path));
        }
    }
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle().cast(),
        ObjectName: &object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: descriptor.cast(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle = ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: all descriptor/name/output storage remains live through NtCreateFile.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            ptr::null(),
            if directory {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
            share,
            disposition,
            FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | if directory { FILE_DIRECTORY_FILE } else { 0 },
            ptr::null(),
            0,
        )
    };
    if !descriptor.is_null() {
        // SAFETY: the SDDL conversion allocated descriptor with LocalAlloc.
        unsafe {
            LocalFree(descriptor);
        }
    }
    if status < 0 {
        // SAFETY: any NTSTATUS may be converted to its Win32 error.
        return Err(node_error(
            &io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32),
            if directory && disposition == FILE_CREATE {
                "mkdir"
            } else {
                "open"
            },
            path,
        ));
    }
    // SAFETY: successful NtCreateFile returns a new owned handle.
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}

fn real_file(file: File, path: &str) -> R<File> {
    let info = information(&file, path)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(node_error_named("ELOOP", "open", path));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(node_error_named("EISDIR", "open", path));
    }
    Ok(file)
}

impl Directory {
    pub(super) fn open(path: &str, follow_final_alias: bool) -> R<Self> {
        let name = wide(path)?;
        // SAFETY: name is NUL terminated; no optional security/template pointers are supplied.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS
                    | if follow_final_alias {
                        0
                    } else {
                        FILE_FLAG_OPEN_REPARSE_POINT
                    },
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(node_error(&io::Error::last_os_error(), "open", path));
        }
        // SAFETY: CreateFileW returned a newly owned handle.
        let file = unsafe { File::from_raw_handle(handle.cast()) };
        let info = information(&file, path)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(node_error_named("ENOTDIR", "open", path));
        }
        if !follow_final_alias && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(node_error_named("ELOOP", "open", path));
        }
        Ok(Self {
            path: actual_path(&file, path)?,
            file,
        })
    }

    pub(super) fn matches_path(&self, path: &str, follow_final_alias: bool) -> R<bool> {
        let current = Self::open(path, follow_final_alias)?;
        Ok(same_identity(
            &information(&self.file, path)?,
            &information(&current.file, path)?,
        ))
    }

    pub(super) fn child_directory(
        &self,
        name: &str,
        create: bool,
        private: bool,
    ) -> R<Option<Self>> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        let access = FILE_READ_ATTRIBUTES | FILE_TRAVERSE | if private { DELETE } else { 0 };
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
        let result = if create {
            match child(
                &self.file,
                name,
                &path,
                access,
                share,
                FILE_CREATE,
                true,
                private,
            ) {
                Err(error) if error.code == Some("EEXIST") && !private => child(
                    &self.file, name, &path, access, share, FILE_OPEN, false, false,
                ),
                other => other,
            }
        } else {
            child(
                &self.file, name, &path, access, share, FILE_OPEN, false, false,
            )
        };
        let file = match result {
            Ok(file) => file,
            Err(error) if !create && error.code == Some("ENOENT") => return Ok(None),
            Err(error) => return Err(error),
        };
        let info = information(&file, &path)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(node_error_named("ELOOP", "open", &path));
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(node_error_named("ENOTDIR", "open", &path));
        }
        Ok(Some(Self {
            path: actual_path(&file, &path)?,
            file,
        }))
    }

    pub(super) fn read(&self, name: &str) -> R<Option<File>> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        match child(
            &self.file,
            name,
            &path,
            FILE_READ_DATA | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            false,
            false,
        ) {
            Ok(file) => Ok(Some(real_file(file, &path)?)),
            Err(error) if error.code == Some("ENOENT") => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn create(&self, name: &str) -> R<File> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        real_file(
            child(
                &self.file,
                name,
                &path,
                FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | DELETE,
                0,
                FILE_CREATE,
                false,
                false,
            )?,
            &path,
        )
    }

    pub(super) fn open_copy(&self, name: &str) -> R<File> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        real_file(
            child(
                &self.file,
                name,
                &path,
                FILE_READ_DATA | FILE_READ_ATTRIBUTES | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                false,
                false,
            )?,
            &path,
        )
    }

    pub(super) fn quarantine(
        &self,
        name: &str,
        expected: &Version,
        private: &Directory,
        private_name: &str,
    ) -> R<Quarantine> {
        let original = Directory {
            file: self
                .file
                .try_clone()
                .map_err(|error| node_error(&error, "dup", &self.path))?,
            path: self.path.clone(),
        };
        let staging = Directory {
            file: private
                .file
                .try_clone()
                .map_err(|error| node_error(&error, "dup", &private.path))?,
            path: private.path.clone(),
        };
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        let file = real_file(
            child(
                &self.file,
                name,
                &path,
                FILE_READ_ATTRIBUTES | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN,
                false,
                false,
            )?,
            &path,
        )?;
        if &version(&file, &path)? != expected {
            return Err(NodeError {
                code: None,
                message: "asset object changed before quarantine".into(),
            });
        }
        if !self.matches_path(&self.path, false)? || !private.matches_path(&private.path, false)? {
            return Err(NodeError {
                code: None,
                message: "asset quarantine directory changed".into(),
            });
        }
        let receipt = Quarantine {
            original,
            original_name: name.to_owned(),
            staging,
            private_name: private_name.to_owned(),
            file,
            identity: expected.clone(),
        };
        receipt
            .staging
            .publish(&receipt.file, &receipt.private_name, false)?;
        Ok(receipt)
    }

    pub(super) fn fallback(&self, name: &str) -> R<File> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        let file = real_file(
            child(
                &self.file,
                name,
                &path,
                FILE_WRITE_DATA | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN_IF,
                false,
                false,
            )?,
            &path,
        )?;
        file.set_len(0)
            .map_err(|error| node_error(&error, "open", &path))?;
        Ok(file)
    }

    pub(super) fn matches(&self, name: &str, expected: &Version, content: bool) -> R<bool> {
        let Some(current) = self.read(name)? else {
            return Ok(false);
        };
        let current = version(&current, &self.path)?;
        Ok(if content {
            expected == &current
        } else {
            expected.same_object(&current)
        })
    }

    pub(super) fn publish(&self, file: &File, name: &str, replace: bool) -> R<()> {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.chars().any(|ch| matches!(ch, '\0' | '/' | '\\' | ':'))
        {
            return Err(node_error_named("EINVAL", "rename", &self.path));
        }
        let name: Vec<u16> = name.encode_utf16().collect();
        let bytes = name
            .len()
            .checked_mul(2)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| node_error_named("ENAMETOOLONG", "rename", &self.path))?;
        let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let size = std::mem::size_of::<FILE_RENAME_INFO>()
            .checked_add(bytes as usize)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| node_error_named("ENAMETOOLONG", "rename", &self.path))?;
        let mut memory = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        let info = memory.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: memory is aligned and covers the header plus UTF-16 name.
        unsafe {
            (*info).Anonymous.ReplaceIfExists = replace;
            (*info).RootDirectory = self.file.as_raw_handle().cast();
            (*info).FileNameLength = bytes;
            ptr::copy_nonoverlapping(
                name.as_ptr(),
                memory.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
                name.len(),
            );
            if SetFileInformationByHandle(
                file.as_raw_handle().cast(),
                FileRenameInfo,
                info.cast(),
                size,
            ) == 0
            {
                return Err(node_error(
                    &io::Error::last_os_error(),
                    "rename",
                    &self.path,
                ));
            }
        }
        Ok(())
    }

    pub(super) fn remove(&self, name: &str, expected: Option<&Version>) -> R<()> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        let file = real_file(
            child(
                &self.file,
                name,
                &path,
                FILE_READ_ATTRIBUTES | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN,
                false,
                false,
            )?,
            &path,
        )?;
        if let Some(expected) = expected {
            if &version(&file, &path)? != expected {
                return Err(NodeError {
                    code: None,
                    message: "asset object changed before removal".into(),
                });
            }
        }
        remove_handle(&file, &path)
    }
}

pub(super) struct Quarantine {
    original: Directory,
    original_name: String,
    staging: Directory,
    private_name: String,
    file: File,
    identity: Version,
}

impl Quarantine {
    fn current(&self) -> R<()> {
        if !self.staging.matches_path(&self.staging.path, false)? {
            return Err(NodeError {
                code: None,
                message: "asset quarantine directory changed".into(),
            });
        }
        let path = jscompat::node_path::host::join(&[&self.staging.path, &self.private_name]);
        let current = real_file(
            child(
                &self.staging.file,
                &self.private_name,
                &path,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                false,
                false,
            )?,
            &path,
        )?;
        if !self.identity.same_object(&version(&current, &path)?)
            || !self
                .identity
                .same_object(&version(&self.file, &self.staging.path)?)
        {
            return Err(NodeError {
                code: None,
                message: "quarantined asset identity changed".into(),
            });
        }
        Ok(())
    }

    pub(super) fn restore(self) -> Result<(), (Self, NodeError)> {
        let result = self.current().and_then(|()| {
            if !self.original.matches_path(&self.original.path, false)? {
                return Err(NodeError {
                    code: None,
                    message: "asset original directory changed".into(),
                });
            }
            self.original
                .publish(&self.file, &self.original_name, false)
        });
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err((self, error)),
        }
    }

    pub(super) fn dispose(self) -> Result<(), (Self, NodeError)> {
        let result = self
            .current()
            .and_then(|()| remove_handle(&self.file, &self.staging.path));
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err((self, error)),
        }
    }
}

pub(super) fn remove_handle(file: &File, path: &str) -> R<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: file owns the object to remove; disposition is valid input storage.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle().cast(),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(node_error(&io::Error::last_os_error(), "unlink", path));
    }
    Ok(())
}
