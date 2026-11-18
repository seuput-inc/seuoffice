use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};

fn child(parent: &File, name: &OsStr, directory: bool) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| super::invalid("NUL in asset component"))?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        | if directory { libc::O_DIRECTORY } else { libc::O_NONBLOCK };
    // SAFETY: parent and the NUL-terminated single component remain live.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn open(root: &Path, relative: &[&OsStr]) -> io::Result<File> {
    let mut dir = OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    let mut root_components = root.components();
    if root_components.next() != Some(Component::RootDir) {
        return Err(super::invalid("asset root must be absolute"));
    }
    for component in root_components {
        match component {
            Component::Normal(name) => dir = child(&dir, name, true)?,
            _ => return Err(super::invalid("invalid asset root component")),
        }
    }
    let (name, parents) = relative.split_last().ok_or_else(|| super::invalid("empty asset path"))?;
    for component in parents {
        dir = child(&dir, component, true)?;
    }
    child(&dir, name, false)
}
