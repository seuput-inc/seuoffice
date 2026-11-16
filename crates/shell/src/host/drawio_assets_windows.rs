use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, Prefix};
use std::ptr;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
    FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType, OPEN_EXISTING, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

fn verify(file: &File, directory: bool) -> io::Result<()> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file and output storage are live for both calls.
    if unsafe { GetFileType(file.as_raw_handle().cast()) } != FILE_TYPE_DISK {
        return Err(super::invalid("asset is not a disk object"));
    }
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(super::invalid("asset path contains a reparse point"));
    }
    if (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory {
        return Err(super::invalid("asset object has the wrong type"));
    }
    Ok(())
}

fn child(parent: &File, name: &OsStr, directory: bool) -> io::Result<File> {
    let mut units: Vec<u16> = name.encode_wide().collect();
    if units.is_empty()
        || units.iter().any(|c| matches!(*c, 0 | 47 | 58 | 92))
        || units == [46]
        || units == [46, 46]
    {
        return Err(super::invalid("invalid asset component"));
    }
    let len = units
        .len()
        .checked_mul(2)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| super::invalid("asset component too long"))?;
    let mut object_name = UNICODE_STRING {
        Length: len,
        MaximumLength: len,
        Buffer: units.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle().cast(),
        ObjectName: &mut object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null_mut(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle = ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    let access = FILE_READ_ATTRIBUTES
        | SYNCHRONIZE
        | if directory {
            FILE_TRAVERSE
        } else {
            FILE_READ_DATA
        };
    let options = FILE_OPEN_REPARSE_POINT
        | FILE_SYNCHRONOUS_IO_NONALERT
        | if directory {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        };
    // SAFETY: parent, attributes, name and output storage remain live.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status_block,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            options,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: every NTSTATUS can be translated to a Win32 error.
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    // SAFETY: NtCreateFile returned a new owned handle.
    let file = unsafe { File::from_raw_handle(handle.cast()) };
    verify(&file, directory)?;
    Ok(file)
}

pub(super) fn open(root: &Path, relative: &[&OsStr]) -> io::Result<File> {
    let mut components = root.components();
    let prefix = match components.next() {
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::UNC(_, _)) =>
        {
            prefix.as_os_str()
        }
        _ => return Err(super::invalid("unsupported asset root prefix")),
    };
    if components.next() != Some(Component::RootDir) {
        return Err(super::invalid("asset root must be absolute"));
    }
    let mut volume = OsString::from(prefix);
    volume.push("\\");
    let mut wide: Vec<u16> = volume.encode_wide().collect();
    wide.push(0);
    // SAFETY: volume is NUL-terminated and optional pointers are null.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a new owned handle.
    let mut dir = unsafe { File::from_raw_handle(handle.cast()) };
    verify(&dir, true)?;
    for component in components {
        match component {
            Component::Normal(name) => dir = child(&dir, name, true)?,
            _ => return Err(super::invalid("invalid asset root component")),
        }
    }
    let (name, parents) = relative
        .split_last()
        .ok_or_else(|| super::invalid("empty asset path"))?;
    for component in parents {
        dir = child(&dir, component, true)?;
    }
    child(&dir, name, false)
}
