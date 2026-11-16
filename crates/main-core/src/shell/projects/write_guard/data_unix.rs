use super::*;
use std::ffi::CString;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

enum Parent<'a> {
    Owned(fs::File),
    Borrowed(&'a fs::File),
}

impl std::ops::Deref for Parent<'_> {
    type Target = fs::File;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(file) => file,
            Self::Borrowed(file) => file,
        }
    }
}

pub(super) struct Snapshot<'a> {
    root: Parent<'a>,
    root_path: String,
    data_name: CString,
    data_path: String,
    file_name: CString,
    target_path: String,
    directory: Option<Metadata>,
    target: Option<Metadata>,
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

fn same_content(a: &Metadata, b: &Metadata) -> bool {
    same_file(a, b)
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
        && a.nlink() == b.nlink()
}

impl<'a> Snapshot<'a> {
    pub(super) fn capture(
        root: &str,
        data_name: &str,
        rel_name: &str,
    ) -> Result<(Self, Option<String>), WriteGuardError> {
        Self::capture_at(
            Parent::Owned(open_stage_dir(root)?),
            root,
            data_name,
            rel_name,
        )
    }

    pub(super) fn capture_chats(
        scope: &'a ChatsDirectory,
        name: &str,
    ) -> Result<(Self, Option<String>), WriteGuardError> {
        Self::capture_at(
            Parent::Borrowed(&scope.data_handle),
            &scope.data_path,
            crate::packages::project_store::project_folder_name("chatsDir"),
            name,
        )
    }

    fn capture_at(
        parent: Parent<'a>,
        root: &str,
        data_name: &str,
        rel_name: &str,
    ) -> Result<(Self, Option<String>), WriteGuardError> {
        let mut snapshot = Self {
            root: parent,
            root_path: root.to_owned(),
            data_name: CString::new(data_name)
                .map_err(|_| ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, data_name))?,
            data_path: join(&[root, data_name]),
            file_name: CString::new(rel_name)
                .map_err(|_| ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, rel_name))?,
            target_path: join(&[root, data_name, rel_name]),
            directory: None,
            target: None,
        };
        let (directory, target) = snapshot.open_current()?;
        snapshot.directory = directory
            .as_ref()
            .map(|file| file.metadata())
            .transpose()
            .map_err(|error| node_error(&error, "fstat", &snapshot.data_path))?;
        let Some(mut target) = target else {
            return Ok((snapshot, None));
        };
        let mut bytes = Vec::new();
        target
            .read_to_end(&mut bytes)
            .map_err(|error| node_error(&error, "read", &snapshot.target_path))?;
        let after = target
            .metadata()
            .map_err(|error| node_error(&error, "fstat", &snapshot.target_path))?;
        snapshot.target = Some(after);
        Ok((snapshot, Some(String::from_utf8_lossy(&bytes).into_owned())))
    }

    fn open_current(&self) -> Result<(Option<fs::File>, Option<fs::File>), WriteGuardError> {
        let mut dir_info = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: root/name are live and fstatat initializes dir_info on success.
        if unsafe {
            libc::fstatat(
                self.root.as_raw_fd(),
                self.data_name.as_ptr(),
                dir_info.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok((None, None));
            }
            return Err(node_error(&error, "lstat", &self.data_path).into());
        }
        // SAFETY: fstatat succeeded.
        let dir_info = unsafe { dir_info.assume_init() };
        if dir_info.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(ProjectWriteRefused::new(
                if dir_info.st_mode & libc::S_IFMT == libc::S_IFLNK {
                    ProjectWriteRefusal::Link
                } else {
                    ProjectWriteRefusal::NotAFolder
                },
                &self.data_path,
            )
            .into());
        }
        // SAFETY: the pinned root descriptor and literal data child name are live.
        let fd = unsafe {
            libc::openat(
                self.root.as_raw_fd(),
                self.data_name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ENOENT) => Ok((None, None)),
                Some(libc::ELOOP) => {
                    Err(ProjectWriteRefused::new(ProjectWriteRefusal::Link, &self.data_path).into())
                }
                _ => Err(node_error(&error, "open", &self.data_path).into()),
            };
        }
        // SAFETY: fd came from successful openat and is newly owned.
        let directory = unsafe { fs::File::from_raw_fd(fd) };
        let held_dir = directory
            .metadata()
            .map_err(|error| node_error(&error, "fstat", &self.data_path))?;
        if held_dir.dev() != stat_dev(&dir_info) || held_dir.ino() != dir_info.st_ino {
            return Err(NodeError {
                code: Some("EBUSY"),
                message: "project data folder changed".into(),
            }
            .into());
        }
        let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the descriptor/name are live and fstatat initializes info on success.
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                self.file_name.as_ptr(),
                info.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok((Some(directory), None));
            }
            return Err(node_error(&error, "lstat", &self.target_path).into());
        }
        // SAFETY: fstatat succeeded.
        let info = unsafe { info.assume_init() };
        if info.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(ProjectWriteRefused::new(
                if info.st_mode & libc::S_IFMT == libc::S_IFLNK {
                    ProjectWriteRefusal::Link
                } else {
                    ProjectWriteRefusal::NotAFolder
                },
                &self.target_path,
            )
            .into());
        }
        // SAFETY: the pinned parent/name are live; nonblocking avoids hanging on a concurrently substituted FIFO.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                self.file_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(
                node_error(&std::io::Error::last_os_error(), "open", &self.target_path).into(),
            );
        }
        // SAFETY: fd came from successful openat and is newly owned.
        let target = unsafe { fs::File::from_raw_fd(fd) };
        let held = target
            .metadata()
            .map_err(|error| node_error(&error, "fstat", &self.target_path))?;
        if !held.is_file() || held.dev() != stat_dev(&info) || held.ino() != info.st_ino {
            return Err(NodeError {
                code: Some("EBUSY"),
                message: "project data target changed".into(),
            }
            .into());
        }
        Ok((Some(directory), Some(target)))
    }

    pub(super) fn matches(&self) -> Result<bool, WriteGuardError> {
        if !stage_handle_matches(&self.root, &self.root_path)? {
            return Ok(false);
        }
        let (directory, target) = self.open_current()?;
        if let Some(expected) = &self.directory {
            let Some(current) = directory else {
                return Ok(false);
            };
            let current = current
                .metadata()
                .map_err(|error| node_error(&error, "fstat", &self.data_path))?;
            if !same_file(expected, &current) {
                return Ok(false);
            }
        }
        match (&self.target, target) {
            (None, None) => Ok(true),
            (Some(expected), Some(current)) => {
                let current = current
                    .metadata()
                    .map_err(|error| node_error(&error, "fstat", &self.target_path))?;
                Ok(same_content(expected, &current))
            }
            _ => Ok(false),
        }
    }
}
