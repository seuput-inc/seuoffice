use crate::node_fs::{NodeError, namespaced, node_error, node_error_named, node_error2};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::Arc;

type R<T> = Result<T, NodeError>;

#[derive(Debug)]
pub(super) struct Directory {
    pub(super) file: File,
    pub(super) path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Version {
    device: u64,
    inode: u64,
    size: u64,
    seconds: i64,
    nanos: i64,
    changed: i64,
    changed_nanos: i64,
}

pub(super) fn version(file: &File, path: &str) -> R<Version> {
    let m = file.metadata().map_err(|e| node_error(&e, "fstat", path))?;
    Ok(Version {
        device: m.dev(),
        inode: m.ino(),
        size: m.len(),
        seconds: m.mtime(),
        nanos: m.mtime_nsec(),
        changed: m.ctime(),
        changed_nanos: m.ctime_nsec(),
    })
}
impl Version {
    pub(super) fn same_object(&self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

/// An exact source object opened beneath the captured document directory.
#[derive(Clone, Debug)]
pub(super) struct PinnedSource {
    file: Arc<File>,
    version: Version,
    path: String,
}

pub(super) fn pin_source(root: &str, path: &str) -> R<PinnedSource> {
    use std::path::{Component, Path};
    let relative = Path::new(path)
        .strip_prefix(Path::new(root))
        .map_err(|_| NodeError {
            code: None,
            message: "asset source is outside its document directory".into(),
        })?;
    let mut components = relative.components().peekable();
    let mut root_components = Path::new(root).components();
    if root_components.next() != Some(Component::RootDir) {
        return Err(node_error_named("EINVAL", "open", root));
    }
    let mut directory = Directory::open("/", false)?;
    for component in root_components {
        let Component::Normal(name) = component else {
            return Err(node_error_named("EINVAL", "open", root));
        };
        let name = name
            .to_str()
            .ok_or_else(|| node_error_named("EINVAL", "open", root))?;
        directory = directory
            .child_directory(name, false, false)?
            .ok_or_else(|| node_error_named("ENOENT", "open", root))?;
    }
    loop {
        let component = components
            .next()
            .ok_or_else(|| node_error_named("EINVAL", "open", path))?;
        let Component::Normal(name) = component else {
            return Err(node_error_named("EINVAL", "open", path));
        };
        let name = name
            .to_str()
            .ok_or_else(|| node_error_named("EINVAL", "open", path))?;
        if components.peek().is_none() {
            let file = directory
                .read(name)?
                .ok_or_else(|| node_error_named("ENOENT", "open", path))?;
            let version = version(&file, path)?;
            return Ok(PinnedSource {
                file: Arc::new(file),
                version,
                path: path.to_owned(),
            });
        }
        directory = directory
            .child_directory(name, false, false)?
            .ok_or_else(|| node_error_named("ENOENT", "open", path))?;
    }
}

impl PinnedSource {
    pub(super) fn validate(&self) -> R<()> {
        if version(&self.file, &self.path)? != self.version {
            return Err(NodeError {
                code: None,
                message: "asset source changed after preparation".into(),
            });
        }
        Ok(())
    }

    pub(super) fn permissions(&self) -> R<std::fs::Permissions> {
        self.file
            .metadata()
            .map(|metadata| metadata.permissions())
            .map_err(|error| node_error(&error, "fstat", &self.path))
    }

    pub(super) fn copy_to(&self, destination: &mut File) -> R<()> {
        use std::io::Write;
        use std::os::unix::fs::FileExt;
        self.validate()?;
        let mut offset = 0;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = self
                .file
                .read_at(&mut buffer, offset)
                .map_err(|error| node_error(&error, "read", &self.path))?;
            if read == 0 {
                break;
            }
            destination
                .write_all(&buffer[..read])
                .map_err(|error| node_error(&error, "write", &self.path))?;
            offset += read as u64;
        }
        self.validate()
    }
}

fn segment(name: &str, path: &str) -> R<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(node_error_named("EINVAL", "open", path));
    }
    CString::new(name).map_err(|_| node_error_named("EINVAL", "open", path))
}
fn opened(fd: i32, path: &str) -> R<File> {
    if fd < 0 {
        return Err(node_error(&io::Error::last_os_error(), "open", path));
    }
    // SAFETY: successful openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn regular(file: File, path: &str) -> R<File> {
    let metadata = file.metadata().map_err(|e| node_error(&e, "fstat", path))?;
    if !metadata.is_file() {
        return Err(node_error_named(
            if metadata.is_dir() {
                "EISDIR"
            } else {
                "EINVAL"
            },
            "open",
            path,
        ));
    }
    Ok(file)
}

/// The app-created private quarantine is single-writer within this operation.
/// Hostile same-UID modification of app-private staging is outside this scope.
#[cfg(test)]
#[derive(Debug)]
pub(super) struct Quarantine {
    original: Directory,
    original_name: String,
    private: Directory,
    private_name: String,
    object: Option<File>,
}

/// A failed post-rename acquisition must retain the inverse capability if
/// the public name was occupied before the immediate restore could complete.
#[cfg(test)]
#[derive(Debug)]
pub(super) struct QuarantineMoveError {
    pub(super) error: NodeError,
    pub(super) retained: Option<Quarantine>,
}

/// A second descriptor of the same directory object.
pub(super) fn duplicate(directory: &Directory) -> R<Directory> {
    Ok(Directory {
        file: directory
            .file
            .try_clone()
            .map_err(|error| node_error(&error, "dup", &directory.path))?,
        path: directory.path.clone(),
    })
}

#[cfg(test)]
fn move_exclusive(
    source: &Directory,
    source_name: &str,
    target: &Directory,
    target_name: &str,
) -> R<()> {
    let from = segment(source_name, &source.path)?;
    let to = segment(target_name, &target.path)?;
    // SAFETY: both retained directory descriptors and component names remain live.
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            source.file.as_raw_fd(),
            from.as_ptr(),
            target.file.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    } as i64;
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source.file.as_raw_fd(),
            from.as_ptr(),
            target.file.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } as i64;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(NodeError {
        code: None,
        message: "exclusive asset quarantine is unavailable on this platform".into(),
    });
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if result != 0 {
        return Err(node_error2(
            &io::Error::last_os_error(),
            "rename",
            &source.path,
            &target.path,
        ));
    }
    Ok(())
}

#[cfg(test)]
fn identity_handle(directory: &Directory, name: &str) -> R<File> {
    let component = segment(name, &directory.path)?;
    #[cfg(target_os = "linux")]
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(target_os = "macos")]
    let flags = libc::O_EVTONLY | libc::O_SYMLINK | libc::O_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: the private directory and NUL-terminated component are retained.
    let fd = unsafe { libc::openat(directory.file.as_raw_fd(), component.as_ptr(), flags) };
    opened(fd, &directory.path)
}

#[cfg(test)]
impl Quarantine {
    pub(super) fn move_from(
        original: &Directory,
        name: &str,
        private: &Directory,
        private_name: &str,
    ) -> Result<Self, Box<QuarantineMoveError>> {
        let fail = |error| {
            Box::new(QuarantineMoveError {
                error,
                retained: None,
            })
        };
        let original = duplicate(original).map_err(fail)?;
        let private = duplicate(private).map_err(fail)?;
        move_exclusive(&original, name, &private, private_name).map_err(fail)?;
        let mut receipt = Self {
            original,
            original_name: name.to_owned(),
            private,
            private_name: private_name.to_owned(),
            object: None,
        };
        match identity_handle(&receipt.private, private_name) {
            Ok(object) => {
                receipt.object = Some(object);
                Ok(receipt)
            }
            Err(error) => match receipt.restore() {
                Ok(()) => Err(fail(error)),
                Err(failure) => {
                    let (retained, restore) = *failure;
                    Err(Box::new(QuarantineMoveError {
                        error: NodeError {
                            code: restore.code,
                            message: format!(
                                "{}; quarantine retained at {}: {}",
                                error.message, retained.private.path, restore.message
                            ),
                        },
                        retained: Some(retained),
                    }))
                }
            },
        }
    }

    pub(super) fn same_object(&self, file: &File) -> R<bool> {
        let object = self.object.as_ref().ok_or_else(|| NodeError {
            code: None,
            message: "quarantined object handle was not acquired".into(),
        })?;
        Ok(version(object, &self.private.path)?.same_object(&version(file, &self.original.path)?))
    }

    /// The receipt authorizes only this one no-replace inverse move.
    pub(super) fn restore(self) -> Result<(), Box<(Self, NodeError)>> {
        match move_exclusive(
            &self.private,
            &self.private_name,
            &self.original,
            &self.original_name,
        ) {
            Ok(()) => Ok(()),
            Err(error) => Err(Box::new((self, error))),
        }
    }
}

impl Directory {
    pub(super) fn open(path: &str, follow_final_alias: bool) -> R<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(
                libc::O_DIRECTORY
                    | libc::O_CLOEXEC
                    | if follow_final_alias {
                        0
                    } else {
                        libc::O_NOFOLLOW
                    },
            )
            .open(namespaced(path))
            .map_err(|e| node_error(&e, "open", path))?;
        Ok(Self {
            file,
            path: path.to_owned(),
        })
    }
    pub(super) fn matches_path(&self, path: &str, follow_alias: bool) -> R<bool> {
        let current = Self::open(path, follow_alias)?;
        Ok(version(&self.file, path)?.same_object(&version(&current.file, path)?))
    }
    pub(super) fn child_directory(
        &self,
        name: &str,
        create: bool,
        private: bool,
    ) -> R<Option<Self>> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        let name = segment(name, &path)?;
        if create {
            // SAFETY: the pinned directory and single component name remain live.
            if unsafe {
                libc::mkdirat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    if private { 0o700 } else { 0o777 },
                )
            } != 0
            {
                let error = io::Error::last_os_error();
                if private || error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(node_error(&error, "mkdir", &path));
                }
            }
        }
        // SAFETY: openat resolves only this component below the held directory.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 && !create && io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        let file = opened(fd, &path)?;
        if private {
            let m = file
                .metadata()
                .map_err(|e| node_error(&e, "fstat", &path))?;
            // SAFETY: geteuid has no arguments or side effects.
            if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
                return Err(NodeError {
                    code: None,
                    message: format!("private asset staging owner or permissions changed: {path}"),
                });
            }
            // Enforce 0700 on the captured newly-created directory, without
            // changing process umask or revisiting a mutable outer path.
            if m.mode() & 0o7777 != 0o700 {
                // SAFETY: file is the verified owned directory descriptor.
                if unsafe { libc::fchmod(file.as_raw_fd(), 0o700) } != 0 {
                    return Err(node_error(&io::Error::last_os_error(), "fchmod", &path));
                }
            }
        }
        Ok(Some(Self { file, path }))
    }
    pub(super) fn read(&self, name: &str) -> R<Option<File>> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        let name = segment(name, &path)?;
        // SAFETY: the directory and name remain live. NONBLOCK refuses blocking special-file reads.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        Ok(Some(regular(opened(fd, &path)?, &path)?))
    }
    pub(super) fn create(&self, name: &str) -> R<File> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        let name = segment(name, &path)?;
        // SAFETY: the private parent/name remain live; exclusive creation never follows a link.
        regular(
            opened(
                unsafe {
                    libc::openat(
                        self.file.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDWR
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_NOFOLLOW
                            | libc::O_CLOEXEC,
                        0o666,
                    )
                },
                &path,
            )?,
            &path,
        )
    }
    pub(super) fn open_rescue(&self, name: &str) -> R<File> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        let name = segment(name, &path)?;
        // SAFETY: the single non-link name is relative to the pinned parent.
        // CopyFile's same-inode check must precede truncation.
        regular(
            opened(
                unsafe {
                    libc::openat(
                        self.file.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_WRONLY
                            | libc::O_CREAT
                            | libc::O_NOFOLLOW
                            | libc::O_CLOEXEC
                            | libc::O_NONBLOCK,
                        0o666,
                    )
                },
                &path,
            )?,
            &path,
        )
    }
    pub(super) fn fallback(&self, name: &str) -> R<File> {
        let file = self.open_rescue(name)?;
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        file.set_len(0).map_err(|e| node_error(&e, "open", &path))?;
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
    pub(super) fn publish(
        &self,
        source: &Self,
        source_name: &str,
        file: &File,
        name: &str,
        replace: bool,
    ) -> R<()> {
        let from = segment(source_name, &source.path)?;
        let to = segment(name, &self.path)?;
        if !source.matches(source_name, &version(file, &source.path)?, false)? {
            return Err(NodeError {
                code: None,
                message: "private asset stage changed".into(),
            });
        }
        // SAFETY: both directory descriptors and the validated component names remain live.
        let result = if replace {
            (unsafe {
                libc::renameat(
                    source.file.as_raw_fd(),
                    from.as_ptr(),
                    self.file.as_raw_fd(),
                    to.as_ptr(),
                )
            }) as i64
        } else {
            #[cfg(target_os = "macos")]
            {
                (unsafe {
                    libc::renameatx_np(
                        source.file.as_raw_fd(),
                        from.as_ptr(),
                        self.file.as_raw_fd(),
                        to.as_ptr(),
                        libc::RENAME_EXCL,
                    )
                }) as i64
            }
            #[cfg(target_os = "linux")]
            {
                (unsafe {
                    libc::syscall(
                        libc::SYS_renameat2,
                        source.file.as_raw_fd(),
                        from.as_ptr(),
                        self.file.as_raw_fd(),
                        to.as_ptr(),
                        libc::RENAME_NOREPLACE,
                    )
                }) as i64
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                return Err(NodeError {
                    code: None,
                    message: "exclusive asset publication is unavailable on this platform".into(),
                });
            }
        };
        if result != 0 {
            return Err(node_error2(
                &io::Error::last_os_error(),
                "rename",
                &source.path,
                &self.path,
            ));
        }
        Ok(())
    }
    pub(super) fn remove(&self, name: &str, expected: Option<&Version>) -> R<()> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        if let Some(expected) = expected {
            if !self.matches(name, expected, true)? {
                return Err(NodeError {
                    code: None,
                    message: "asset object changed before removal".into(),
                });
            }
        } else if self.read(name)?.is_none() {
            return Err(node_error_named("ENOENT", "unlink", &path));
        }
        let name = segment(name, &path)?;
        // This is an admitted namespace unlink, not an inode-conditional operation against external writers.
        // SAFETY: the parent descriptor and validated component remain live.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(node_error(&io::Error::last_os_error(), "unlink", &path));
        }
        Ok(())
    }
    /// Removes the private staging directory `staged`, which this operation
    /// made as the entry `name` of this directory. The name must still lead,
    /// without following a link, to the object `staged` holds, and the
    /// kernel refuses anything but an empty directory. There is no
    /// inode-conditional unlink: a name swapped between the check and the
    /// call can only cost an empty directory that carries this operation's
    /// random name.
    pub(super) fn remove_private_child(&self, name: &str, staged: &Self) -> R<()> {
        let path = jscompat::node_path::host::join(&[&self.path, name]);
        let current = self
            .child_directory(name, false, false)?
            .ok_or_else(|| node_error_named("ENOENT", "rmdir", &path))?;
        let held = version(&staged.file, &staged.path)?;
        let named = version(&current.file, &path)?;
        if !held.same_object(&named) {
            return Err(NodeError {
                code: None,
                message: "private asset staging changed before removal".into(),
            });
        }
        let component = segment(name, &path)?;
        // SAFETY: the parent descriptor and the validated component remain
        // live; AT_REMOVEDIR unlinks only an empty directory and never follows a link.
        let removed = unsafe {
            libc::unlinkat(self.file.as_raw_fd(), component.as_ptr(), libc::AT_REMOVEDIR)
        };
        if removed != 0 {
            return Err(node_error(&io::Error::last_os_error(), "rmdir", &path));
        }
        Ok(())
    }
}
