use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx,
};

fn fail(message: &str) -> io::Error {
    io::Error::other(message)
}
struct Information {
    metadata: BY_HANDLE_FILE_INFORMATION,
    id: FILE_ID_INFO,
}
fn information(file: &File) -> io::Result<Information> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns a live handle and info is writable storage of the API type.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(io::Error::other)?;
    let mut id = FILE_ID_INFO::default();
    // SAFETY: id is writable storage of the exact requested FileIdInfo size.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(io::Error::other)?;
    Ok(Information { metadata: info, id })
}
fn same(a: &Information, b: &Information) -> bool {
    // ReFS identifiers must not be truncated to the legacy 64-bit file index.
    a.id.VolumeSerialNumber == b.id.VolumeSerialNumber
        && a.id.FileId == b.id.FileId
        && a.metadata.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0
            == b.metadata.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0
}
fn open(path: &Path, directory: bool) -> io::Result<(File, Information)> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(
            FILE_SHARE_READ.0
                | FILE_SHARE_WRITE.0
                | if directory { 0 } else { FILE_SHARE_DELETE.0 },
        )
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)?;
    let info = information(&file)?;
    if info.metadata.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(fail("trash refuses reparse points"));
    }
    if (info.metadata.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0) != directory {
        return Err(fail("chat trash target or ancestor changed kind"));
    }
    if !directory && !file.metadata()?.is_file() {
        return Err(fail("chat trash refuses special files"));
    }
    Ok((file, info))
}

pub(super) struct Witness {
    paths: Vec<PathBuf>,
    _handles: Vec<File>,
    captured: Vec<Information>,
}
impl Witness {
    pub(super) fn capture(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(fail("trash source must be absolute"));
        }
        let mut current = PathBuf::new();
        let mut paths = Vec::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => {
                    if !matches!(
                        prefix.kind(),
                        Prefix::Disk(_)
                            | Prefix::UNC(_, _)
                            | Prefix::VerbatimDisk(_)
                            | Prefix::VerbatimUNC(_, _)
                    ) {
                        return Err(fail("trash refuses device namespace paths"));
                    }
                    current.push(prefix.as_os_str());
                }
                Component::RootDir => {
                    current.push(component.as_os_str());
                    paths.push(current.clone());
                }
                Component::Normal(name) => {
                    if name
                        .encode_wide()
                        .any(|unit| unit == 0 || unit == u16::from(b':'))
                    {
                        return Err(fail("trash refuses streams and NUL"));
                    }
                    current.push(name);
                    paths.push(current.clone());
                }
                _ => return Err(fail("trash path contains traversal components")),
            }
        }
        if paths.len() < 2 {
            return Err(fail("trash source has no filename"));
        }
        let mut handles = Vec::with_capacity(paths.len());
        let mut captured = Vec::with_capacity(paths.len());
        for (index, path) in paths.iter().enumerate() {
            let (file, info) = open(path, index + 1 != paths.len())?;
            handles.push(file);
            captured.push(info);
        }
        Ok(Self {
            paths,
            _handles: handles,
            captured,
        })
    }
    pub(super) fn validate(&self) -> io::Result<()> {
        for (index, path) in self.paths.iter().enumerate() {
            let directory = index + 1 != self.paths.len();
            let (_named, current) = open(path, directory)?;
            let retained = &self.captured[index];
            if !same(&current, retained) {
                return Err(fail("trash root, data directory or target was replaced"));
            }
            if !directory
                && (current.metadata.nFileSizeHigh != retained.metadata.nFileSizeHigh
                    || current.metadata.nFileSizeLow != retained.metadata.nFileSizeLow
                    || current.metadata.ftLastWriteTime.dwHighDateTime
                        != retained.metadata.ftLastWriteTime.dwHighDateTime
                    || current.metadata.ftLastWriteTime.dwLowDateTime
                        != retained.metadata.ftLastWriteTime.dwLowDateTime)
            {
                return Err(fail("trash target changed after preparation"));
            }
        }
        Ok(())
    }
}
