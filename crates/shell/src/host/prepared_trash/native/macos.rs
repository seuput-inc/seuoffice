use std::ffi::CString;
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

fn fail(message: &str) -> io::Error {
    io::Error::other(message)
}
fn same(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino() && a.file_type() == b.file_type()
}
fn open_at(directory: &File, name: &CString, flags: i32) -> io::Result<File> {
    // SAFETY: name/descriptor are live; successful openat returns a new owned fd.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is newly owned and transferred once.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) struct Witness {
    directories: Vec<File>,
    names: Vec<CString>,
    target: File,
    original: Metadata,
}
impl Witness {
    pub(super) fn capture(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(fail("trash source must be absolute"));
        }
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => names.push(
                    CString::new(name.as_bytes()).map_err(|_| fail("trash path contains NUL"))?,
                ),
                _ => return Err(fail("trash path contains traversal components")),
            }
        }
        let target_name = names
            .last()
            .ok_or_else(|| fail("trash source has no filename"))?;
        let mut directories = vec![
            File::options()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/")?,
        ];
        for name in &names[..names.len() - 1] {
            let parent = directories
                .last()
                .ok_or_else(|| fail("missing trash ancestor"))?;
            directories.push(open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY)?);
        }
        let parent = directories
            .last()
            .ok_or_else(|| fail("missing trash parent"))?;
        let target = open_at(parent, target_name, libc::O_EVTONLY)?;
        let original = target.metadata()?;
        if !original.is_file() || original.file_type().is_symlink() {
            return Err(fail(
                "chat trash refuses directories, links and special files",
            ));
        }
        Ok(Self {
            directories,
            names,
            target,
            original,
        })
    }
    pub(super) fn validate(&self) -> io::Result<()> {
        for (index, name) in self.names.iter().enumerate() {
            let is_target = index + 1 == self.names.len();
            let expected = if is_target {
                self.target.metadata()?
            } else {
                self.directories[index + 1].metadata()?
            };
            let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: directory/name are retained and fstatat initializes named.
            if unsafe {
                libc::fstatat(
                    self.directories[index].as_raw_fd(),
                    name.as_ptr(),
                    named.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fstatat succeeded.
            let named = unsafe { named.assume_init() };
            let kind = if is_target {
                libc::S_IFREG
            } else {
                libc::S_IFDIR
            };
            if named.st_mode & libc::S_IFMT != kind
                || named.st_dev as u64 != expected.dev()
                || named.st_ino != expected.ino()
            {
                return Err(fail("trash root, data directory or target was replaced"));
            }
            if is_target
                && (!same(&expected, &self.original)
                    || expected.len() != self.original.len()
                    || expected.mtime() != self.original.mtime()
                    || expected.mtime_nsec() != self.original.mtime_nsec()
                    || expected.ctime() != self.original.ctime()
                    || expected.ctime_nsec() != self.original.ctime_nsec())
            {
                return Err(fail("trash target changed after preparation"));
            }
        }
        Ok(())
    }
}
