//! Windows project-staging directory handles.
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr;

use crate::node_fs::{NodeError, namespaced, node_error, node_error_named};
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_READ_DATA,
    FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FileDispositionInfo, GetFileInformationByHandle,
    OPEN_EXISTING, SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

fn wide_path(path: &str) -> Result<Vec<u16>, NodeError> {
    if path.contains('\0') {
        return Err(node_error_named("EINVAL", "open", path));
    }
    Ok(namespaced(path)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect())
}

fn information(file: &File, path: &str) -> Result<BY_HANDLE_FILE_INFORMATION, NodeError> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns a live Windows handle and info is valid writable storage.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
        return Err(node_error(&io::Error::last_os_error(), "fstat", path));
    }
    Ok(info)
}

fn open_dir(path: &str, access: u32, share_delete: bool) -> Result<File, NodeError> {
    let wide = wide_path(path)?;
    // SAFETY: wide is a NUL-terminated path; no optional pointers are supplied.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | if share_delete { FILE_SHARE_DELETE } else { 0 },
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(node_error(&io::Error::last_os_error(), "open", path));
    }
    // SAFETY: CreateFileW returned a fresh owned handle, transferred once.
    let file = unsafe { File::from_raw_handle(handle.cast()) };
    let info = information(&file, path)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(node_error_named("ELOOP", "open", path));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(node_error_named("ENOTDIR", "open", path));
    }
    Ok(file)
}

pub(super) fn open_existing_dir(path: &str) -> Result<File, NodeError> {
    open_dir(path, FILE_READ_ATTRIBUTES, true)
}

pub(super) fn open_create_parent(path: &str) -> Result<File, NodeError> {
    open_dir(path, FILE_READ_ATTRIBUTES | FILE_TRAVERSE, false)
}

fn open_child(
    parent: &File,
    name: &str,
    display_path: &str,
    access: u32,
    share: u32,
    disposition: u32,
    directory: Option<bool>,
) -> Result<File, NodeError> {
    let syscall = if directory == Some(true) {
        "mkdir"
    } else {
        "open"
    };
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.chars().any(|ch| matches!(ch, '\0' | '/' | '\\' | ':'))
    {
        return Err(node_error_named("EINVAL", syscall, display_path));
    }
    let mut units: Vec<u16> = name.encode_utf16().collect();
    if units.len() > usize::from(u16::MAX) / 2 {
        return Err(node_error_named("ENAMETOOLONG", syscall, display_path));
    }
    let bytes = (units.len() * 2) as u16;
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
    let kind = match directory {
        Some(true) => FILE_DIRECTORY_FILE,
        Some(false) => FILE_NON_DIRECTORY_FILE,
        None => 0,
    };
    let attributes_file = if directory == Some(true) {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    // SAFETY: parent, name and outputs remain live; a null descriptor requests kernel inheritance.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            ptr::null(),
            attributes_file,
            share,
            disposition,
            kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: RtlNtStatusToDosError accepts any NTSTATUS.
        let error = unsafe { RtlNtStatusToDosError(status) };
        return Err(node_error(
            &io::Error::from_raw_os_error(error as i32),
            syscall,
            display_path,
        ));
    }
    // SAFETY: successful NtCreateFile returns a fresh owned handle.
    let file = unsafe { File::from_raw_handle(handle.cast()) };
    Ok(file)
}

pub(super) fn create_final_directory(
    parent: &File,
    name: &str,
    display_path: &str,
) -> Result<File, NodeError> {
    open_child(
        parent,
        name,
        display_path,
        FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        FILE_CREATE,
        Some(true),
    )
}

pub(super) fn create_metadata_directory(
    project: &File,
    project_path: &str,
) -> Result<File, NodeError> {
    open_data_directory(
        project,
        crate::packages::project_store::project_folder_name("dataDir"),
        project_path,
        true,
    )?
    .ok_or_else(|| node_error_named("ENOENT", "mkdir", project_path))
}

pub(super) fn open_data_directory(
    project: &File,
    data_name: &str,
    root: &str,
    create: bool,
) -> Result<Option<File>, NodeError> {
    if !super::super::folder_files::is_data_dir_name(data_name) {
        return Err(node_error_named("EINVAL", "open", root));
    }
    open_named_directory(project, data_name, root, create)
}

pub(super) fn open_chats_directory(
    data: &File,
    root: &str,
    create: bool,
) -> Result<Option<File>, NodeError> {
    open_named_directory(
        data,
        crate::packages::project_store::project_folder_name("chatsDir"),
        root,
        create,
    )
}

fn open_named_directory(
    project: &File,
    data_name: &str,
    root: &str,
    create: bool,
) -> Result<Option<File>, NodeError> {
    let open_existing = || {
        open_child(
            project,
            data_name,
            root,
            FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            None,
        )
    };
    let result = match open_existing() {
        Err(error) if error.code == Some("ENOENT") && create => {
            match open_child(
                project,
                data_name,
                root,
                FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_CREATE,
                Some(true),
            ) {
                Err(error) if error.code == Some("EEXIST") => open_existing(),
                other => other,
            }
        }
        other => other,
    };
    match result {
        Ok(file) => {
            let attributes = information(&file, root)?.dwFileAttributes;
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(node_error_named("ELOOP", "open", root));
            }
            if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Err(node_error_named("ENOTDIR", "open", root));
            }
            Ok(Some(file))
        }
        Err(error) if !create && error.code == Some("ENOENT") => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DataTarget {
    Missing,
    File { links: u32 },
    Directory,
    Reparse,
}

pub(super) fn inspect_data_target(
    data_dir: &File,
    name: &str,
    root: &str,
) -> Result<DataTarget, NodeError> {
    if !super::super::paths::is_plain_segment(name) || name.contains('/') || name.contains('\\') {
        return Err(node_error_named("EINVAL", "lstat", root));
    }
    let file = match open_child(
        data_dir,
        name,
        root,
        FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        None,
    ) {
        Ok(file) => file,
        Err(error) if error.code == Some("ENOENT") => return Ok(DataTarget::Missing),
        Err(error) => return Err(error),
    };
    let info = information(&file, root)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(DataTarget::Reparse);
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Ok(DataTarget::Directory);
    }
    Ok(DataTarget::File {
        links: info.nNumberOfLinks,
    })
}

pub(super) struct DataReadFile {
    file: File,
    observed: Option<BY_HANDLE_FILE_INFORMATION>,
    attempted: bool,
    display_path: String,
}
pub(super) enum DataReadTarget {
    Missing,
    Directory,
    Reparse,
    File(DataReadFile),
}

pub(super) fn open_data_read_target(
    data_dir: &File,
    name: &str,
    root: &str,
) -> Result<DataReadTarget, NodeError> {
    match inspect_data_target(data_dir, name, root)? {
        DataTarget::Missing => return Ok(DataReadTarget::Missing),
        DataTarget::Directory => return Ok(DataReadTarget::Directory),
        DataTarget::Reparse => return Ok(DataReadTarget::Reparse),
        DataTarget::File { .. } => {}
    }
    let file = match open_child(
        data_dir,
        name,
        root,
        FILE_READ_ATTRIBUTES | FILE_READ_DATA,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        None,
    ) {
        Ok(file) => file,
        Err(error) if error.code == Some("ENOENT") => return Ok(DataReadTarget::Missing),
        Err(error) => return Err(error),
    };
    let info = information(&file, root)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(DataReadTarget::Reparse);
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Ok(DataReadTarget::Directory);
    }
    Ok(DataReadTarget::File(DataReadFile {
        file,
        observed: None,
        attempted: false,
        display_path: root.to_owned(),
    }))
}

fn same_version(left: &BY_HANDLE_FILE_INFORMATION, right: &BY_HANDLE_FILE_INFORMATION) -> bool {
    same_identity(left, right)
        && left.nFileSizeHigh == right.nFileSizeHigh
        && left.nFileSizeLow == right.nFileSizeLow
        && left.ftLastWriteTime.dwLowDateTime == right.ftLastWriteTime.dwLowDateTime
        && left.ftLastWriteTime.dwHighDateTime == right.ftLastWriteTime.dwHighDateTime
        && left.nNumberOfLinks == right.nNumberOfLinks
}

/// Read only on a worker, not inside the owner gate.
pub(super) fn read_data_target(target: &mut DataReadFile) -> Result<String, NodeError> {
    if target.attempted {
        return Err(NodeError {
            code: None,
            message: "project data witness was already read".into(),
        });
    }
    target.attempted = true;
    let mut bytes = Vec::new();
    target
        .file
        .read_to_end(&mut bytes)
        .map_err(|error| crate::node_fs::node_error_fd(&error, "read"))?;
    let after = information(&target.file, &target.display_path)?;
    target.observed = Some(after);
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn current_identity(
    data_dir: &File,
    name: &str,
    root: &str,
) -> Result<Option<BY_HANDLE_FILE_INFORMATION>, NodeError> {
    let file = match open_child(
        data_dir,
        name,
        root,
        FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        None,
    ) {
        Ok(file) => file,
        Err(error) if error.code == Some("ENOENT") => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(information(&file, root)?))
}

pub(super) fn data_read_target_matches(
    data_dir: &File,
    name: &str,
    root: &str,
    target: &DataReadTarget,
) -> Result<bool, NodeError> {
    let current = current_identity(data_dir, name, root)?;
    match target {
        DataReadTarget::Missing => Ok(current.is_none()),
        DataReadTarget::Directory | DataReadTarget::Reparse => Ok(false),
        DataReadTarget::File(file) => {
            let expected = file.observed.as_ref().ok_or_else(|| NodeError {
                code: None,
                message: "project data witness was not read".into(),
            })?;
            Ok(current.as_ref().is_some_and(|info| {
                info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY)
                    == 0
                    && same_version(expected, info)
            }))
        }
    }
}

pub(super) struct DataDeleteFile {
    file: File,
    identity: BY_HANDLE_FILE_INFORMATION,
    deleted: bool,
    display_path: String,
}
pub(super) enum DataDeleteTarget {
    Missing,
    Directory,
    Reparse,
    File(DataDeleteFile),
}

pub(super) fn open_data_delete_target(
    data_dir: &File,
    name: &str,
    root: &str,
) -> Result<DataDeleteTarget, NodeError> {
    match inspect_data_target(data_dir, name, root)? {
        DataTarget::Missing => return Ok(DataDeleteTarget::Missing),
        DataTarget::Directory => return Ok(DataDeleteTarget::Directory),
        DataTarget::Reparse => return Ok(DataDeleteTarget::Reparse),
        DataTarget::File { .. } => {}
    }
    let file = match open_child(
        data_dir,
        name,
        root,
        FILE_READ_ATTRIBUTES | DELETE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        FILE_OPEN,
        None,
    ) {
        Ok(file) => file,
        Err(error) if error.code == Some("ENOENT") => return Ok(DataDeleteTarget::Missing),
        Err(error) => return Err(error),
    };
    let identity = information(&file, root)?;
    if identity.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(DataDeleteTarget::Reparse);
    }
    if identity.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Ok(DataDeleteTarget::Directory);
    }
    Ok(DataDeleteTarget::File(DataDeleteFile {
        file,
        identity,
        deleted: false,
        display_path: root.to_owned(),
    }))
}

pub(super) fn data_delete_target_matches(
    data_dir: &File,
    name: &str,
    root: &str,
    target: &DataDeleteTarget,
) -> Result<bool, NodeError> {
    let current = current_identity(data_dir, name, root)?;
    match target {
        DataDeleteTarget::Missing => Ok(current.is_none()),
        DataDeleteTarget::Directory | DataDeleteTarget::Reparse => Ok(false),
        DataDeleteTarget::File(file) => Ok(!file.deleted
            && current.as_ref().is_some_and(|info| {
                info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY)
                    == 0
                    && same_identity(&file.identity, info)
            })),
    }
}

pub(super) fn delete_data_target(target: &mut DataDeleteFile) -> Result<(), NodeError> {
    if target.deleted {
        return Err(NodeError {
            code: None,
            message: "project data was already deleted".into(),
        });
    }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the held handle is the captured target and has DELETE access.
    if unsafe {
        SetFileInformationByHandle(
            target.file.as_raw_handle().cast(),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(node_error(
            &io::Error::last_os_error(),
            "unlink",
            &target.display_path,
        ));
    }
    target.deleted = true;
    Ok(())
}

pub(super) struct DataAppendFile {
    file: File,
    display_path: String,
}
pub(super) enum DataAppendTarget {
    Missing,
    Directory,
    Reparse,
    HardLink,
    File(DataAppendFile),
}
pub(super) enum DataAppendWrite {
    Written(usize),
    HardLink,
}

pub(super) fn open_data_append_target(
    directory: &File,
    name: &str,
    root: &str,
    create: bool,
) -> Result<DataAppendTarget, NodeError> {
    let classification = inspect_data_target(directory, name, root)?;
    match classification {
        DataTarget::Missing if !create => return Ok(DataAppendTarget::Missing),
        DataTarget::Directory => return Ok(DataAppendTarget::Directory),
        DataTarget::Reparse => return Ok(DataAppendTarget::Reparse),
        DataTarget::File { links } if links > 1 => return Ok(DataAppendTarget::HardLink),
        _ => {}
    }
    let open = |disposition| {
        open_child(
            directory,
            name,
            root,
            FILE_READ_ATTRIBUTES | FILE_APPEND_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            disposition,
            None,
        )
    };
    let file = match open(if classification == DataTarget::Missing {
        FILE_CREATE
    } else {
        FILE_OPEN
    }) {
        Ok(file) => file,
        Err(error) if error.code == Some("EEXIST") && create => open(FILE_OPEN)?,
        Err(error) if error.code == Some("ENOENT") => return Ok(DataAppendTarget::Missing),
        Err(error) => return Err(error),
    };
    let info = information(&file, root)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(DataAppendTarget::Reparse);
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Ok(DataAppendTarget::Directory);
    }
    if info.nNumberOfLinks > 1 {
        return Ok(DataAppendTarget::HardLink);
    }
    Ok(DataAppendTarget::File(DataAppendFile {
        file,
        display_path: root.to_owned(),
    }))
}

/// One kernel append write. The caller owns authorization and partial-write progress.
pub(super) fn append_data_target(
    target: &mut DataAppendFile,
    bytes: &[u8],
) -> Result<DataAppendWrite, NodeError> {
    if information(&target.file, &target.display_path)?.nNumberOfLinks > 1 {
        return Ok(DataAppendWrite::HardLink);
    }
    target
        .file
        .write(bytes)
        .map(DataAppendWrite::Written)
        .map_err(|error| crate::node_fs::node_error_fd(&error, "write"))
}

pub(super) fn data_append_target_matches(
    directory: &File,
    name: &str,
    root: &str,
    target: &DataAppendFile,
) -> Result<bool, NodeError> {
    let held = information(&target.file, root)?;
    Ok(current_identity(directory, name, root)?
        .as_ref()
        .is_some_and(|current| {
            current.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
                == 0
                && same_identity(&held, current)
        }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StageState {
    Created,
    WriteFailed,
    Ready,
    Published,
    Cancelled,
}

pub(super) struct OwnedMetadataStage {
    directory: File,
    file: File,
    target_name: String,
    display_path: String,
    state: StageState,
}

impl OwnedMetadataStage {
    fn remove_unpublished(&self) -> Result<(), NodeError> {
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: this handle belongs to the temporary object, never the published target.
        if unsafe {
            SetFileInformationByHandle(
                self.file.as_raw_handle().cast(),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(node_error(
                &io::Error::last_os_error(),
                "unlink",
                &self.display_path,
            ));
        }
        Ok(())
    }

    pub(super) fn cancel(mut self) -> Result<(), NodeError> {
        if matches!(self.state, StageState::Published | StageState::Cancelled) {
            return Err(NodeError {
                code: None,
                message: "project metadata was already published".into(),
            });
        }
        self.remove_unpublished()?;
        self.state = StageState::Cancelled;
        Ok(())
    }
}

impl Drop for OwnedMetadataStage {
    fn drop(&mut self) {
        if !matches!(self.state, StageState::Published | StageState::Cancelled) {
            if let Err(error) = self.remove_unpublished() {
                eprintln!(
                    "cannot remove unpublished project metadata: {}",
                    error.message
                );
            }
        }
    }
}

/// `dir_path` is where the stage is made (the folder `data_dir` is the handle
/// of): a failure to make it is the `open` of the temporary file beside the
/// target, as the original reports it.
pub(super) fn create_project_data_stage(
    data_dir: &File,
    dir_path: &str,
    project_path: &str,
    rel_name: &str,
) -> Result<OwnedMetadataStage, NodeError> {
    if !super::super::paths::is_plain_segment(rel_name)
        || rel_name.contains('/')
        || rel_name.contains('\\')
    {
        return Err(node_error_named("EINVAL", "open", project_path));
    }
    let directory = data_dir
        .try_clone()
        .map_err(|error| node_error(&error, "open", project_path))?;
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).map_err(|error| NodeError {
        code: None,
        message: error.to_string(),
    })?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut end = rel_name.len().min(200);
    while !rel_name.is_char_boundary(end) {
        end -= 1;
    }
    let name = format!("{}.{}.tmp", &rel_name[..end], suffix);
    let temp_path = jscompat::node_path::host::join(&[dir_path, &name]);
    let file = open_child(
        data_dir,
        &name,
        &temp_path,
        FILE_READ_ATTRIBUTES | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | DELETE,
        0,
        FILE_CREATE,
        Some(false),
    )?;
    Ok(OwnedMetadataStage {
        directory,
        file,
        target_name: rel_name.to_owned(),
        display_path: project_path.to_owned(),
        state: StageState::Created,
    })
}

pub(super) fn write_project_data_stage(
    stage: &mut OwnedMetadataStage,
    bytes: &[u8],
) -> Result<(), NodeError> {
    if stage.state != StageState::Created {
        return Err(NodeError {
            code: None,
            message: "project data stage is not writable".into(),
        });
    }
    stage.state = StageState::WriteFailed;
    if let Err(error) = stage.file.write_all(bytes) {
        return Err(node_error(&error, "write", &stage.display_path));
    }
    if let Err(error) = stage.file.sync_all() {
        if !matches!(
            crate::node_fs::code_of(&error),
            Some("EPERM" | "EACCES" | "EBUSY" | "EINVAL" | "ENOSYS" | "ENOTSUP")
        ) {
            return Err(node_error(&error, "fsync", &stage.display_path));
        }
    }
    stage.state = StageState::Ready;
    Ok(())
}

pub(super) fn publish_project_metadata(stage: &mut OwnedMetadataStage) -> Result<(), NodeError> {
    if stage.target_name != crate::packages::project_store::project_folder_name("projectFile") {
        return Err(node_error_named("EINVAL", "rename", &stage.display_path));
    }
    publish_project_data_exclusive(stage)
}

#[cfg(test)]
pub(super) fn publish_project_metadata_replace(
    stage: &mut OwnedMetadataStage,
) -> Result<(), NodeError> {
    if stage.target_name != crate::packages::project_store::project_folder_name("projectFile") {
        return Err(node_error_named("EINVAL", "rename", &stage.display_path));
    }
    publish_project_data_replace(stage)
}

pub(super) fn publish_project_data_exclusive(
    stage: &mut OwnedMetadataStage,
) -> Result<(), NodeError> {
    if stage.state != StageState::Ready {
        return Err(NodeError {
            code: None,
            message: "project data stage is not ready".into(),
        });
    }
    rename_owned(
        &stage.file,
        &stage.directory,
        &stage.target_name,
        &stage.display_path,
        false,
    )?;
    stage.state = StageState::Published;
    Ok(())
}
pub(super) fn prepare_project_data_exclusive_receipt(
    stage: &OwnedMetadataStage,
    path: &std::path::Path,
) -> Result<file_commit::PreparedCommit, NodeError> {
    if stage.state != StageState::Ready {
        return Err(NodeError {
            code: None,
            message: "project data stage is not ready".into(),
        });
    }
    file_commit::prepare_commit(
        &stage.file,
        &stage.directory,
        &stage.target_name,
        path,
        file_commit::PublicationMode::Exclusive,
    )
    .map_err(|error| node_error(&error, "rename", &stage.display_path))
}

pub(super) fn publish_project_data_exclusive_receipt(
    stage: &mut OwnedMetadataStage,
    plan: file_commit::PreparedCommit,
) -> Result<file_commit::FileCommitReceipt, NodeError> {
    if stage.state != StageState::Ready {
        return Err(NodeError {
            code: None,
            message: "project data stage is not ready".into(),
        });
    }
    let receipt = plan
        .publish()
        .map_err(|error| node_error(&error, "rename", &stage.display_path))?;
    stage.state = StageState::Published;
    Ok(receipt)
}

pub(super) fn publish_project_data_replace(
    stage: &mut OwnedMetadataStage,
) -> Result<(), NodeError> {
    if stage.state != StageState::Ready {
        return Err(NodeError {
            code: None,
            message: "project data stage is not ready".into(),
        });
    }
    rename_owned(
        &stage.file,
        &stage.directory,
        &stage.target_name,
        &stage.display_path,
        true,
    )?;
    stage.state = StageState::Published;
    Ok(())
}

fn rename_owned(
    leaf: &File,
    final_parent: &File,
    final_name: &str,
    display_path: &str,
    replace: bool,
) -> Result<(), NodeError> {
    if final_name.is_empty()
        || final_name == "."
        || final_name == ".."
        || final_name
            .chars()
            .any(|ch| matches!(ch, '\0' | '/' | '\\' | ':'))
    {
        return Err(node_error_named("EINVAL", "rename", display_path));
    }
    let name_bytes = final_name
        .encode_utf16()
        .count()
        .checked_mul(2)
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| node_error_named("ENAMETOOLONG", "rename", display_path))?;
    std::mem::size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes as usize)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| node_error_named("ENAMETOOLONG", "rename", display_path))?;
    file_commit::rename_handle(leaf, final_parent, final_name, replace)
        .map_err(|error| node_error(&error, "rename", display_path))
}

fn same_identity(left: &BY_HANDLE_FILE_INFORMATION, right: &BY_HANDLE_FILE_INFORMATION) -> bool {
    left.dwVolumeSerialNumber == right.dwVolumeSerialNumber
        && left.nFileIndexHigh == right.nFileIndexHigh
        && left.nFileIndexLow == right.nFileIndexLow
}

pub(super) fn handle_matches_path(handle: &File, path: &str) -> Result<bool, NodeError> {
    if !super::super::paths::same_path(&super::realpath(path)?, path, "win32") {
        return Ok(false);
    }
    let found = open_existing_dir(path)?;
    if !same_identity(&information(handle, path)?, &information(&found, path)?) {
        return Ok(false);
    }
    Ok(super::super::paths::same_path(
        &super::realpath(path)?,
        path,
        "win32",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hosted expectations, not observations:
    // - Restricted parent: CreateDirectoryW(NULL security) is the outcome oracle;
    //   helper success and Node error code must agree without adding READ_CONTROL.
    // - Inheritance: compare direct control and helper DACL, mandatory label and
    //   available audit SACL; unavailable audit inspection is not a passing result.
    // - Removal: normal/hardlinked files preserve other names; reparse points and
    //   directories remain refused. Parent DELETE_CHILD-only ACL needs its own oracle.
    // - Sharing: a live share-all writer must permit the same read as the native
    //   control. A read witness observes state; it does not freeze other writers.
    // - Namespace changes: test owner-gated serialized effects, not global inode CAS.

    struct RestoreDacl {
        file: File,
        descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
        acl: *mut windows_sys::Win32::Security::ACL,
        protected: bool,
    }
    impl RestoreDacl {
        fn capture(path: &str) -> Self {
            use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
            use windows_sys::Win32::Security::{
                DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, SE_DACL_PROTECTED,
            };
            use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC};
            let file =
                open_dir(path, FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC, true).unwrap();
            let mut descriptor = ptr::null_mut();
            let mut acl = ptr::null_mut();
            // SAFETY: live file handle and valid output storage.
            assert_eq!(
                unsafe {
                    GetSecurityInfo(
                        file.as_raw_handle().cast(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &mut acl,
                        ptr::null_mut(),
                        &mut descriptor,
                    )
                },
                0
            );
            let mut control = 0;
            let mut revision = 0;
            // SAFETY: descriptor is allocated by GetSecurityInfo.
            assert_ne!(
                unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
                0
            );
            Self {
                file,
                descriptor,
                acl,
                protected: control & SE_DACL_PROTECTED != 0,
            }
        }
        fn restrict(&self, mask: u32) {
            use windows_sys::Win32::Security::Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SE_FILE_OBJECT,
                SetSecurityInfo,
            };
            use windows_sys::Win32::Security::{
                DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
                PROTECTED_DACL_SECURITY_INFORMATION,
            };
            let sddl: Vec<u16> = format!("D:P(A;;0x{mask:08x};;;OW)")
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut descriptor = ptr::null_mut();
            // SAFETY: NUL-terminated SDDL and writable descriptor output.
            assert_ne!(
                unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        sddl.as_ptr(),
                        1,
                        &mut descriptor,
                        ptr::null_mut(),
                    )
                },
                0
            );
            let mut acl = ptr::null_mut();
            let mut present = 0;
            let mut defaulted = 0;
            // SAFETY: descriptor is live and output pointers are valid.
            assert_ne!(
                unsafe {
                    GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted)
                },
                0
            );
            // SAFETY: pre-acquired WRITE_DAC handle is live; ACL belongs to descriptor.
            let status = unsafe {
                SetSecurityInfo(
                    self.file.as_raw_handle().cast(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    acl,
                    ptr::null(),
                )
            };
            // SAFETY: descriptor was allocated by the SDDL conversion API.
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(descriptor);
            }
            assert_eq!(status, 0);
        }
    }
    impl Drop for RestoreDacl {
        fn drop(&mut self) {
            use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
            use windows_sys::Win32::Security::{
                DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
                UNPROTECTED_DACL_SECURITY_INFORMATION,
            };
            let protection = if self.protected {
                PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                UNPROTECTED_DACL_SECURITY_INFORMATION
            };
            // SAFETY: saved descriptor and WRITE_DAC handle remain live through restoration.
            let status = unsafe {
                SetSecurityInfo(
                    self.file.as_raw_handle().cast(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | protection,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    self.acl,
                    ptr::null(),
                )
            };
            if status != 0 {
                eprintln!("synthetic ACL restoration failed: {status}");
            }
            // SAFETY: descriptor was allocated by GetSecurityInfo.
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.descriptor);
            }
        }
    }

    #[test]
    fn restricted_parent_matches_native_directory_creation() {
        use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
        let unique = format!(
            "seu-acl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        std::fs::create_dir(&base).unwrap();
        for mask in [0x00000004u32, 0x00000084, 0x001000a4, 0x00000080] {
            let parent = base.join(format!("{mask:x}"));
            std::fs::create_dir(&parent).unwrap();
            let restore = RestoreDacl::capture(parent.to_str().unwrap());
            restore.restrict(mask);
            let control_path = parent.join("control");
            let wide = wide_path(control_path.to_str().unwrap()).unwrap();
            // SAFETY: path is NUL-terminated; null security asks for native inheritance.
            let control = if unsafe { CreateDirectoryW(wide.as_ptr(), ptr::null()) } != 0 {
                Ok(())
            } else {
                Err(node_error(
                    &io::Error::last_os_error(),
                    "mkdir",
                    control_path.to_str().unwrap(),
                )
                .code)
            };
            let actual_path = parent.join("actual");
            let actual = open_create_parent(parent.to_str().unwrap())
                .and_then(|handle| {
                    create_final_directory(&handle, "actual", actual_path.to_str().unwrap())
                })
                .map(drop)
                .map_err(|error| error.code);
            drop(restore);
            assert_eq!(
                actual, control,
                "restricted-parent differential for {mask:#x}"
            );
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn read_witness_matches_native_read_with_an_open_share_all_writer() {
        use std::os::windows::fs::OpenOptionsExt;
        let unique = format!(
            "seu-sharing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        std::fs::create_dir(&base).unwrap();
        let path = base.join("memory.md");
        std::fs::write(&path, b"current").unwrap();
        let directory = open_create_parent(base.to_str().unwrap()).unwrap();
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&path)
            .unwrap();
        let control = std::fs::read(&path).unwrap();
        let mut witness =
            open_data_read_target(&directory, "memory.md", base.to_str().unwrap()).unwrap();
        let DataReadTarget::File(ref mut file) = witness else {
            panic!("expected readable file")
        };
        assert_eq!(read_data_target(file).unwrap().as_bytes(), control);
        assert!(
            data_read_target_matches(&directory, "memory.md", base.to_str().unwrap(), &witness)
                .unwrap()
        );
        drop(writer);
        drop(witness);
        drop(directory);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn binding_rejects_an_ancestor_reparse_alias_to_the_same_object() {
        let unique = format!(
            "seu-binding-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        let real = base.join("real");
        let directory = real.join("data");
        std::fs::create_dir_all(&directory).unwrap();
        let handle = open_existing_dir(directory.to_str().unwrap()).unwrap();
        assert!(handle_matches_path(&handle, directory.to_str().unwrap()).unwrap());
        let alias = base.join("alias");
        std::os::windows::fs::symlink_dir(&real, &alias).unwrap();
        let aliased_directory = alias.join("data");
        let aliased_handle = open_existing_dir(aliased_directory.to_str().unwrap()).unwrap();
        assert!(same_identity(
            &information(&handle, "").unwrap(),
            &information(&aliased_handle, "").unwrap()
        ));
        assert!(!handle_matches_path(&handle, aliased_directory.to_str().unwrap()).unwrap());
        drop(aliased_handle);
        drop(handle);
        std::fs::remove_dir(alias).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    fn stage_project_data(
        data_dir: &File,
        root: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<OwnedMetadataStage, NodeError> {
        let mut stage = create_project_data_stage(data_dir, root, root, name)?;
        write_project_data_stage(&mut stage, bytes)?;
        Ok(stage)
    }

    fn stage_project_metadata(
        data_dir: &File,
        root: &str,
        bytes: &[u8],
    ) -> Result<OwnedMetadataStage, NodeError> {
        stage_project_data(
            data_dir,
            root,
            crate::packages::project_store::project_folder_name("projectFile"),
            bytes,
        )
    }

    fn dacl(path: &str) -> Option<Vec<u8>> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION};
        use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
        let file = open_dir(path, FILE_READ_ATTRIBUTES | READ_CONTROL, true).unwrap();
        let mut descriptor = ptr::null_mut();
        let mut acl: *mut ACL = ptr::null_mut();
        // SAFETY: file and output pointers are valid for the duration of the call.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut acl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, 0);
        let result = if acl.is_null() {
            None
        } else {
            // SAFETY: descriptor owns the ACL, whose AclSize is its allocated byte size.
            Some(unsafe {
                std::slice::from_raw_parts(acl.cast::<u8>(), usize::from((*acl).AclSize)).to_vec()
            })
        };
        // SAFETY: GetSecurityInfo returns a LocalAlloc-owned descriptor.
        unsafe {
            LocalFree(descriptor);
        }
        result
    }

    #[test]
    fn identity_includes_volume_and_both_index_halves() {
        let mut a = BY_HANDLE_FILE_INFORMATION::default();
        a.dwVolumeSerialNumber = 7;
        a.nFileIndexHigh = 2;
        a.nFileIndexLow = 3;
        let mut b = a;
        assert!(same_identity(&a, &b));
        b.dwVolumeSerialNumber += 1;
        assert!(!same_identity(&a, &b));
        b = a;
        b.nFileIndexHigh += 1;
        assert!(!same_identity(&a, &b));
        b = a;
        b.nFileIndexLow += 1;
        assert!(!same_identity(&a, &b));
    }

    #[test]
    fn embedded_nul_is_not_truncated() {
        assert_eq!(
            wide_path("C:\\safe\0other").unwrap_err().code,
            Some("EINVAL")
        );
    }

    #[test]
    fn direct_creation_and_owned_metadata_publication() {
        let unique = format!(
            "seu-stage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        std::fs::create_dir(&base).unwrap();
        let parent = open_create_parent(base.to_str().unwrap()).unwrap();
        let control = base.join("control");
        std::fs::create_dir(&control).unwrap();
        let expected_acl = dacl(control.to_str().unwrap());
        let path = base.join("project-\u{ac00}");
        let project =
            create_final_directory(&parent, "project-\u{ac00}", path.to_str().unwrap()).unwrap();
        assert_eq!(dacl(path.to_str().unwrap()), expected_acl);
        assert_eq!(
            create_final_directory(&parent, "project-\u{ac00}", path.to_str().unwrap())
                .unwrap_err()
                .code,
            Some("EEXIST")
        );
        assert!(std::fs::rename(&path, base.join("other")).is_err());
        assert!(handle_matches_path(&project, path.to_str().unwrap()).unwrap());
        let data_name = crate::packages::project_store::project_folder_name("dataDir");
        assert!(
            open_data_directory(&project, data_name, path.to_str().unwrap(), false)
                .unwrap()
                .is_none()
        );
        assert!(open_data_directory(&project, "../bad", path.to_str().unwrap(), false).is_err());
        let data = create_metadata_directory(&project, path.to_str().unwrap()).unwrap();
        let data_path = path.join(crate::packages::project_store::project_folder_name(
            "dataDir",
        ));
        assert_eq!(
            inspect_data_target(&data, "missing.md", path.to_str().unwrap()).unwrap(),
            DataTarget::Missing
        );
        std::fs::create_dir(data_path.join("folder")).unwrap();
        assert_eq!(
            inspect_data_target(&data, "folder", path.to_str().unwrap()).unwrap(),
            DataTarget::Directory
        );
        std::fs::remove_dir(data_path.join("folder")).unwrap();
        let mut partial =
            create_project_data_stage(&data, data_path.to_str().unwrap(), path.to_str().unwrap(), "partial.md").unwrap();
        assert!(publish_project_data_replace(&mut partial).is_err());
        write_project_data_stage(&mut partial, b"one").unwrap();
        assert!(write_project_data_stage(&mut partial, b"two").is_err());
        partial.cancel().unwrap();
        stage_project_metadata(&data, path.to_str().unwrap(), b"discarded")
            .unwrap()
            .cancel()
            .unwrap();
        assert_eq!(std::fs::read_dir(&data_path).unwrap().count(), 0);
        let mut stage = stage_project_metadata(&data, path.to_str().unwrap(), b"first").unwrap();
        publish_project_metadata(&mut stage).unwrap();
        assert!(publish_project_metadata(&mut stage).is_err());
        drop(stage);
        let metadata_path = data_path.join(crate::packages::project_store::project_folder_name(
            "projectFile",
        ));
        assert_eq!(std::fs::read(&metadata_path).unwrap(), b"first");
        assert_eq!(
            inspect_data_target(
                &data,
                crate::packages::project_store::project_folder_name("projectFile"),
                path.to_str().unwrap()
            )
            .unwrap(),
            DataTarget::File { links: 1 }
        );
        let mut witness = open_data_read_target(
            &data,
            crate::packages::project_store::project_folder_name("projectFile"),
            path.to_str().unwrap(),
        )
        .unwrap();
        let DataReadTarget::File(ref mut file) = witness else {
            panic!("expected readable file")
        };
        assert_eq!(read_data_target(file).unwrap(), "first");
        assert!(read_data_target(file).is_err());
        assert!(
            data_read_target_matches(
                &data,
                crate::packages::project_store::project_folder_name("projectFile"),
                path.to_str().unwrap(),
                &witness
            )
            .unwrap()
        );
        let hardlink = data_path.join("hardlink.json");
        std::fs::hard_link(&metadata_path, &hardlink).unwrap();
        assert_eq!(
            inspect_data_target(&data, "hardlink.json", path.to_str().unwrap()).unwrap(),
            DataTarget::File { links: 2 }
        );
        std::fs::remove_file(&hardlink).unwrap();
        let symlink = data_path.join("link.json");
        std::os::windows::fs::symlink_file(&metadata_path, &symlink).unwrap();
        assert_eq!(
            inspect_data_target(&data, "link.json", path.to_str().unwrap()).unwrap(),
            DataTarget::Reparse
        );
        std::fs::remove_file(symlink).unwrap();
        let mut duplicate =
            stage_project_metadata(&data, path.to_str().unwrap(), b"second").unwrap();
        assert!(publish_project_metadata(&mut duplicate).is_err());
        duplicate.cancel().unwrap();
        assert_eq!(std::fs::read(&metadata_path).unwrap(), b"first");
        let mut replacement =
            stage_project_metadata(&data, path.to_str().unwrap(), b"replacement").unwrap();
        publish_project_metadata_replace(&mut replacement).unwrap();
        assert!(publish_project_metadata_replace(&mut replacement).is_err());
        drop(replacement);
        assert_eq!(std::fs::read(&metadata_path).unwrap(), b"replacement");
        assert!(
            !data_read_target_matches(
                &data,
                crate::packages::project_store::project_folder_name("projectFile"),
                path.to_str().unwrap(),
                &witness
            )
            .unwrap()
        );
        drop(witness);
        let delete_path = data_path.join("delete.md");
        std::fs::write(&delete_path, b"delete").unwrap();
        let survivor = data_path.join("survivor.md");
        std::fs::hard_link(&delete_path, &survivor).unwrap();
        let mut delete =
            open_data_delete_target(&data, "delete.md", path.to_str().unwrap()).unwrap();
        assert!(
            data_delete_target_matches(&data, "delete.md", path.to_str().unwrap(), &delete)
                .unwrap()
        );
        let DataDeleteTarget::File(ref mut file) = delete else {
            panic!("expected deletable file")
        };
        delete_data_target(file).unwrap();
        assert!(delete_data_target(file).is_err());
        drop(delete);
        assert!(!delete_path.exists());
        assert_eq!(std::fs::read(&survivor).unwrap(), b"delete");
        let private =
            stage_project_data(&data, path.to_str().unwrap(), "private.md", b"secret").unwrap();
        let temp = std::fs::read_dir(&data_path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("private.md.")
            })
            .unwrap();
        assert!(std::fs::read(&temp).is_err());
        assert!(!data_path.join("private.md").exists());
        private.cancel().unwrap();
        for name in ["instructions.md", "memory.md", "memory.20250101.bak.md"] {
            let mut initial =
                stage_project_data(&data, path.to_str().unwrap(), name, b"one").unwrap();
            assert!(publish_project_metadata(&mut initial).is_err());
            publish_project_data_replace(&mut initial).unwrap();
            drop(initial);
            let mut updated =
                stage_project_data(&data, path.to_str().unwrap(), name, b"two").unwrap();
            publish_project_data_replace(&mut updated).unwrap();
            drop(updated);
            assert_eq!(std::fs::read(data_path.join(name)).unwrap(), b"two");
        }
        for bad in ["../x", "a\\b", "x:stream", "trailing.", "name "] {
            assert!(stage_project_data(&data, path.to_str().unwrap(), bad, b"x").is_err());
        }
        drop(data);
        drop(project);
        drop(parent);
        std::fs::remove_dir_all(base).unwrap();
    }
}

#[cfg(test)]
mod chats_append_tests {
    use super::*;

    #[test]
    fn scoped_chats_append_checks_each_write_for_hardlinks() {
        let unique = format!(
            "seu-chat-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir(&root).unwrap();
        let data = open_create_parent(root.to_str().unwrap()).unwrap();
        assert!(
            open_chats_directory(&data, root.to_str().unwrap(), false)
                .unwrap()
                .is_none()
        );
        let chats = open_chats_directory(&data, root.to_str().unwrap(), true)
            .unwrap()
            .unwrap();
        let chat_path = root.join(crate::packages::project_store::project_folder_name(
            "chatsDir",
        ));
        let mut target =
            open_data_append_target(&chats, "chat.jsonl", root.to_str().unwrap(), true).unwrap();
        let DataAppendTarget::File(ref mut file) = target else {
            panic!("expected append target")
        };
        assert!(matches!(
            append_data_target(file, b"one\n").unwrap(),
            DataAppendWrite::Written(4)
        ));
        let link = chat_path.join("linked.jsonl");
        std::fs::hard_link(chat_path.join("chat.jsonl"), &link).unwrap();
        assert!(matches!(
            append_data_target(file, b"two\n").unwrap(),
            DataAppendWrite::HardLink
        ));
        drop(target);
        assert!(matches!(
            open_data_append_target(&chats, "chat.jsonl", root.to_str().unwrap(), false).unwrap(),
            DataAppendTarget::HardLink
        ));
        assert_eq!(std::fs::read(link).unwrap(), b"one\n");
        drop(chats);
        drop(data);
        std::fs::remove_dir_all(root).unwrap();
    }
}
