//! Handle-relative Windows staging. A failed DELETE acquisition is a distinct
//! state: the original write-only stage remains available for fallback/rescue.
use crate::{CommittedWriteSnapshot, FileCommitReceipt, PublicationMode};
use std::{
    fs::File,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle},
    path::Path,
    ptr,
    sync::{Arc, Mutex},
};
use windows_sys::Wdk::{
    Foundation::OBJECT_ATTRIBUTES,
    Storage::FileSystem::{
        FILE_ACCESS_INFORMATION, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE,
        FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, FileAccessInformation,
        NtCreateFile, NtQueryInformationFile,
    },
};
use windows_sys::Win32::{
    Foundation::{
        GENERIC_READ, GENERIC_WRITE, LocalFree, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError,
        UNICODE_STRING,
    },
    Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
    Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_DISPOSITION_INFO,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
        FileDispositionInfo, SYNCHRONIZE, SetFileInformationByHandle,
    },
    System::IO::IO_STATUS_BLOCK,
};

fn random_name(prefix: &str) -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(format!(
        "{prefix}{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

/// Open a directory for traversal, without requesting list-directory access.
pub fn open_output_parent(path: &Path) -> io::Result<File> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_TRAVERSE, OPEN_EXISTING,
        },
    };
    let absolute = std::path::absolute(path)?;
    let source: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    if source.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "output parent contains NUL",
        ));
    }
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    let device: Vec<u16> = r"\\.\".encode_utf16().collect();
    let mut wide = if source.starts_with(&verbatim) || source.starts_with(&device) {
        source
    } else if source.starts_with(&[b'\\' as u16, b'\\' as u16]) {
        let mut result: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
        result.extend_from_slice(&source[2..]);
        result
    } else {
        let mut result = verbatim;
        result.extend_from_slice(&source);
        result
    };
    wide.push(0);
    // SAFETY: wide is a live terminated path and CreateFileW returns an owned handle.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_TRAVERSE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}
fn child(
    parent: &File,
    name: &str,
    access: u32,
    share: u32,
    directory: bool,
    private: bool,
    disposition: u32,
) -> io::Result<File> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.chars().any(|ch| matches!(ch, '\0' | '/' | '\\' | ':'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid staging name",
        ));
    }
    let mut units: Vec<u16> = name.encode_utf16().collect();
    let bytes = units
        .len()
        .checked_mul(2)
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "staging name too long"))?;
    let object_name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: units.as_mut_ptr(),
    };
    let mut descriptor = ptr::null_mut();
    if private {
        let sddl: Vec<u16> = "D:P(A;;FA;;;OW)".encode_utf16().chain(Some(0)).collect();
        // SAFETY: the SDDL and descriptor output remain live during conversion.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
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
    // SAFETY: all handles, strings and structures remain live until NtCreateFile returns.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            // CreateFileW adds FILE_READ_ATTRIBUTES itself; here only what is asked for is
            // granted, and comparing two handles by identity reads attributes through both
            access | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
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
                | if directory {
                    FILE_DIRECTORY_FILE
                } else {
                    FILE_NON_DIRECTORY_FILE
                },
            ptr::null(),
            0,
        )
    };
    if !descriptor.is_null() {
        unsafe {
            LocalFree(descriptor);
        }
    }
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    // SAFETY: successful NtCreateFile returns a uniquely owned handle.
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}

/// Opens one regular child by the retained parent; this is not a pair snapshot.
pub fn open_pinned_file(parent: &File, name: &str) -> io::Result<File> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    let file = child(
        parent,
        name,
        GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        false,
        false,
        FILE_OPEN,
    )?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pinned child is not a regular non-reparse file",
        ));
    }
    Ok(file)
}
fn delete_handle(file: &File) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the live handle and initialized disposition remain valid for the call.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle().cast(),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A second handle to the private directory, one that may delete it. The handle a transaction
/// keeps holds no DELETE: up to Windows 11 and Windows Server 2022 a rename opens its
/// destination directory without sharing delete ([MS-FSA] 2.1.5.15.12, note 185), so a held
/// DELETE refuses every move of a stage into the directory with a sharing violation.
fn open_for_removal(directory: &File) -> io::Result<File> {
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, ReOpenFile},
    };
    // SAFETY: the directory handle is live and ReOpenFile returns an owned handle.
    let handle = unsafe {
        ReOpenFile(
            directory.as_raw_handle().cast(),
            DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}

/// What a handle may do, for the line that explains a refused move. Reading it needs no
/// right on the handle.
fn granted_access(file: &File) -> String {
    let mut info = FILE_ACCESS_INFORMATION::default();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: the handle is live and both structures outlive the call.
    let status = unsafe {
        NtQueryInformationFile(
            file.as_raw_handle().cast(),
            &mut status_block,
            (&mut info as *mut FILE_ACCESS_INFORMATION).cast(),
            std::mem::size_of::<FILE_ACCESS_INFORMATION>() as u32,
            FileAccessInformation,
        )
    };
    if status < 0 {
        return format!("unknown (status {status:#010x})");
    }
    let has = |right: u32| info.AccessFlags & right != 0;
    format!(
        "{:#010x} (read attributes: {}, delete: {})",
        info.AccessFlags,
        has(FILE_READ_ATTRIBUTES),
        has(DELETE)
    )
}

struct DirectoryInner {
    parent: File,
    directory: File,
    removed: Mutex<bool>,
}
#[derive(Clone)]
pub struct PrivateDirectory(Arc<DirectoryInner>);
pub struct PendingStage {
    directory: PrivateDirectory,
    public_parent: File,
    public_name: String,
    file: File,
    leased: bool,
}
pub struct OwnedStage {
    directory: PrivateDirectory,
    public_parent: File,
    public_name: String,
    file: File,
    rename: File,
}
/// Staging succeeded with original write-only access, but obtaining DELETE on
/// that same object failed. The original stage and error remain available.
pub struct RenameUnavailableStage {
    directory: PrivateDirectory,
    public_parent: File,
    public_name: String,
    file: File,
    error: io::Error,
}
pub enum CreatedStage {
    Movable(PendingStage),
    RenameUnavailable(RenameUnavailableStage),
}
pub struct WrittenRenameUnavailableStage {
    stage: RenameUnavailableStage,
    bytes: Vec<u8>,
}
pub struct PendingWrittenStage {
    stage: PendingStage,
    bytes: Vec<u8>,
}

impl PrivateDirectory {
    pub fn create(parent: &File) -> io::Result<Self> {
        let held_parent = parent.try_clone()?;
        for _ in 0..16 {
            let name = random_name(".seuoffice-stage-")?;
            match child(
                parent,
                &name,
                GENERIC_READ | GENERIC_WRITE | FILE_TRAVERSE | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                true,
                true,
                FILE_CREATE,
            ) {
                Ok(directory) => {
                    return Ok(Self(Arc::new(DirectoryInner {
                        parent: held_parent,
                        directory,
                        removed: Mutex::new(false),
                    })));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "private staging names exhausted",
        ))
    }
    pub fn create_stage(&self, _mode: u32) -> io::Result<CreatedStage> {
        self.create_stage_with(|| random_name(".seuoffice-new-").map(|name| format!("{name}.tmp")))
    }
    pub fn create_stage_for(&self, target_name: &str, _mode: u32) -> io::Result<CreatedStage> {
        self.create_stage_with(|| crate::atomic_stage_name(target_name))
    }
    fn create_stage_with(
        &self,
        mut next_name: impl FnMut() -> io::Result<String>,
    ) -> io::Result<CreatedStage> {
        let parent = self.0.parent.try_clone()?;
        for _ in 0..16 {
            let name = next_name()?;
            match child(
                &parent,
                &name,
                GENERIC_WRITE | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                false,
                false,
                FILE_CREATE,
            ) {
                Ok(file) => {
                    return Ok(CreatedStage::Movable(PendingStage {
                        directory: self.clone(),
                        public_parent: parent,
                        public_name: name,
                        file,
                        leased: true,
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(rename_error) => match child(
                    &parent,
                    &name,
                    GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    false,
                    false,
                    FILE_CREATE,
                ) {
                    Ok(file) => {
                        return Ok(CreatedStage::RenameUnavailable(RenameUnavailableStage {
                            directory: self.clone(),
                            public_parent: parent,
                            public_name: name,
                            file,
                            error: rename_error,
                        }));
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                },
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "temporary staging names exhausted",
        ))
    }
    /// The source is validated under a same-link no-delete-sharing lease in
    /// quarantine, not by a racy pathname precheck.
    pub fn quarantine_public(
        &self,
        parent: &File,
        name: &str,
        file: File,
    ) -> Result<PendingStage, (io::Error, File)> {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.chars().any(|ch| matches!(ch, '\0' | '/' | '\\' | ':'))
        {
            return Err((
                io::Error::new(io::ErrorKind::InvalidInput, "invalid public source name"),
                file,
            ));
        }
        match parent.try_clone() {
            Ok(public_parent) => Ok(PendingStage {
                directory: self.clone(),
                public_parent,
                public_name: name.to_owned(),
                file,
                leased: false,
            }),
            Err(error) => Err((error, file)),
        }
    }

    /// Reacquire a committed object using its retained parent, name and handle.
    pub fn quarantine_receipt(&self, receipt: &FileCommitReceipt) -> io::Result<PendingStage> {
        let name = receipt.0.name.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "committed name is not Unicode")
        })?;
        let file = receipt.0.source.try_clone()?;
        self.quarantine_public(&receipt.0.parent, name, file)
            .map_err(|(error, _)| error)
    }
    /// Marks the directory for deletion, once; it goes when its last handle closes.
    pub fn remove(&self) -> io::Result<()> {
        let mut removed = self
            .0
            .removed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *removed {
            return Ok(());
        }
        delete_handle(&open_for_removal(&self.0.directory)?)?;
        *removed = true;
        Ok(())
    }
}
impl RenameUnavailableStage {
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
    pub fn directory(&self) -> PrivateDirectory {
        self.directory.clone()
    }
    pub fn error(&self) -> &io::Error {
        &self.error
    }
    pub fn retry(self) -> PendingStage {
        PendingStage {
            directory: self.directory,
            public_parent: self.public_parent,
            public_name: self.public_name,
            file: self.file,
            leased: false,
        }
    }
}

impl RenameUnavailableStage {
    pub fn write_owned(
        mut self,
        bytes: Vec<u8>,
    ) -> Result<WrittenRenameUnavailableStage, (io::Error, Self, Vec<u8>)> {
        use std::io::{Seek, SeekFrom, Write};
        let result = self
            .file
            .set_len(0)
            .and_then(|()| self.file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| self.file.write_all(&bytes));
        match result {
            Ok(()) => Ok(WrittenRenameUnavailableStage { stage: self, bytes }),
            Err(error) => Err((error, self, bytes)),
        }
    }
}
impl WrittenRenameUnavailableStage {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn rename_error(&self) -> &io::Error {
        self.stage.error()
    }
    pub fn into_parts(self) -> (RenameUnavailableStage, Vec<u8>) {
        (self.stage, self.bytes)
    }
    pub fn retry(self) -> PendingWrittenStage {
        PendingWrittenStage {
            stage: self.stage.retry(),
            bytes: self.bytes,
        }
    }
}
impl PendingWrittenStage {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_parts(self) -> (PendingStage, Vec<u8>) {
        (self.stage, self.bytes)
    }
    pub fn into_unavailable(self, error: io::Error) -> WrittenRenameUnavailableStage {
        WrittenRenameUnavailableStage {
            stage: self.stage.unavailable(error),
            bytes: self.bytes,
        }
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
impl PendingStage {
    pub fn directory(&self) -> PrivateDirectory {
        self.directory.clone()
    }
    pub fn location(&self) -> crate::PendingStageLocation {
        crate::PendingStageLocation::Public
    }
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
    pub fn unavailable(self, error: io::Error) -> RenameUnavailableStage {
        RenameUnavailableStage {
            directory: self.directory,
            public_parent: self.public_parent,
            public_name: self.public_name,
            file: self.file,
            error,
        }
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
    /// Says on stderr which step refused the move and what each handle may do. The OS's text
    /// alone ("Access is denied") is the same for a right missing on the stage, on its lease
    /// and on the private directory.
    fn note_refusal(&self, step: &str, error: &io::Error, lease: Option<&File>) {
        let granted = format!(
            "stage {}, lease {}, private directory {}",
            granted_access(&self.file),
            lease.map_or_else(|| "none".to_owned(), granted_access),
            granted_access(&self.directory.0.directory)
        );
        eprintln!(
            "[file-commit] {}: {step} failed: {error}; access granted: {granted}",
            self.public_name
        );
    }
    pub fn quarantine(self) -> Result<OwnedStage, (io::Error, Self)> {
        let name = match random_name("entry-") {
            Ok(name) => name,
            Err(error) => return Err((error, self)),
        };
        let lease = if self.leased {
            match self.file.try_clone() {
                Ok(lease) => lease,
                Err(error) => {
                    self.note_refusal("duplicating the stage handle", &error, None);
                    return Err((error, self));
                }
            }
        } else {
            match child(
                &self.public_parent,
                &self.public_name,
                DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                false,
                false,
                FILE_OPEN,
            ) {
                Ok(lease) => lease,
                Err(error) => {
                    self.note_refusal("opening the source for its move", &error, None);
                    return Err((error, self));
                }
            }
        };
        match crate::same_handles(&lease, &self.file) {
            Ok(true) => {}
            Ok(false) => {
                return Err((
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "leased source is not the captured object",
                    ),
                    self,
                ));
            }
            Err(error) => {
                self.note_refusal(
                    "reading the identity of the stage and its lease",
                    &error,
                    Some(&lease),
                );
                return Err((error, self));
            }
        }
        if let Err(error) = crate::rename_handle(&lease, &self.directory.0.directory, &name, false)
        {
            self.note_refusal(
                "renaming the stage into the private directory",
                &error,
                Some(&lease),
            );
            return Err((error, self));
        }
        Ok(OwnedStage {
            directory: self.directory,
            public_parent: self.public_parent,
            public_name: self.public_name,
            file: self.file,
            rename: lease,
        })
    }
}
impl OwnedStage {
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
    pub fn directory(&self) -> PrivateDirectory {
        self.directory.clone()
    }
    pub fn restore(self) -> Result<(), (io::Error, Self)> {
        if let Err(error) =
            crate::rename_handle(&self.rename, &self.public_parent, &self.public_name, false)
        {
            return Err((error, self));
        }
        Ok(())
    }
    pub fn remove(self) -> Result<(), (io::Error, Self)> {
        if let Err(error) = delete_handle(&self.rename) {
            return Err((error, self));
        }
        Ok(())
    }
    pub fn prepare(
        self,
        parent: &File,
        name: &str,
        path: &Path,
        mode: PublicationMode,
    ) -> Result<OwnedPreparedCommit, (io::Error, Self)> {
        match crate::prepare_commit(&self.rename, parent, name, path, mode) {
            Ok(plan) => Ok(OwnedPreparedCommit { stage: self, plan }),
            Err(error) => Err((error, self)),
        }
    }
}
pub struct OwnedPreparedCommit {
    stage: OwnedStage,
    plan: crate::PreparedCommit,
}
impl OwnedPreparedCommit {
    pub fn into_stage(self) -> OwnedStage {
        self.stage
    }
    pub fn publish(self) -> Result<FileCommitReceipt, (io::Error, Self)> {
        let name = match self.plan.receipt.name.to_str() {
            Some(name) => name,
            None => {
                return Err((
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid destination name"),
                    self,
                ));
            }
        };
        if let Err(error) = crate::rename_handle(
            &self.stage.rename,
            &self.plan.receipt.parent,
            name,
            self.plan.receipt.mode == PublicationMode::Replace,
        ) {
            return Err((error, self));
        }
        Ok(FileCommitReceipt(self.plan.receipt))
    }
}

pub struct OwnedWrittenStage {
    stage: OwnedStage,
    bytes: Vec<u8>,
}
pub struct OwnedWrittenCommit {
    plan: OwnedPreparedCommit,
    bytes: Vec<u8>,
}
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
impl OwnedWrittenStage {
    /// Flush the actual staged object without reopening its public name.
    pub fn sync_all(&self) -> io::Result<()> {
        self.stage.file.sync_all()
    }
    pub fn into_parts(self) -> (OwnedStage, Vec<u8>) {
        (self.stage, self.bytes)
    }
    pub fn prepare(
        self,
        parent: &File,
        name: &str,
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
