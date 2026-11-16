use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use super::Authorize;
use main_core::node_fs::NodeError;

fn fail(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| fail("trash path contains NUL"))
}
fn stat(file: &File) -> io::Result<libc::stat> {
    let mut value = std::mem::MaybeUninit::uninit();
    // SAFETY: file owns the descriptor and fstat initializes value on success.
    if unsafe { libc::fstat(file.as_raw_fd(), value.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstat succeeded.
    Ok(unsafe { value.assume_init() })
}
fn at_stat(dir: &File, name: &CString) -> io::Result<Option<libc::stat>> {
    let mut value = std::mem::MaybeUninit::uninit();
    // SAFETY: the descriptor/name are live and fstatat initializes value on success.
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: fstatat succeeded.
    Ok(Some(unsafe { value.assume_init() }))
}
fn same(a: &libc::stat, b: &libc::stat) -> bool {
    a.st_dev == b.st_dev
        && a.st_ino == b.st_ino
        && a.st_mode & libc::S_IFMT == b.st_mode & libc::S_IFMT
}
fn unchanged(a: &libc::stat, b: &libc::stat) -> bool {
    same(a, b)
        && a.st_size == b.st_size
        && a.st_mtime == b.st_mtime
        && a.st_mtime_nsec == b.st_mtime_nsec
        && a.st_ctime == b.st_ctime
        && a.st_ctime_nsec == b.st_ctime_nsec
}
fn open_at(dir: &File, name: &CString, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    // SAFETY: arguments are live; successful openat returns a new owned descriptor.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is newly owned and transferred exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn move_new(from: &File, old: &CString, to: &File, new: &CString) -> io::Result<()> {
    // SAFETY: all descriptors/names remain live. NOREPLACE never overwrites a collision.
    if unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            old.as_ptr(),
            to.as_raw_fd(),
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn unlink_same(dir: &File, name: &CString, file: &File) -> io::Result<()> {
    if let Some(named) = at_stat(dir, name)? {
        if !same(&named, &stat(file)?) {
            return Err(fail(
                "trash cleanup entry was replaced; retained rather than deleting another file",
            ));
        }
        // SAFETY: this unlinks only a direct nondirectory child; no recursive cleanup.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Every ancestor is retained, not just a canonicalized pathname. Opening and
/// directory creation happen before the native owner callback.
struct Directory {
    path: PathBuf,
    handles: Vec<File>,
    names: Vec<CString>,
}
impl Directory {
    fn open(path: &Path, create: bool) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(fail("trash requires an absolute path"));
        }
        let mut handles = vec![
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/")?,
        ];
        let mut names = Vec::new();
        for component in path.components() {
            let name = match component {
                Component::RootDir => continue,
                Component::Normal(name) => c_name(name)?,
                _ => return Err(fail("trash path must not contain traversal components")),
            };
            let parent = handles
                .last()
                .ok_or_else(|| fail("missing trash directory handle"))?;
            let next = match open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
                Ok(file) => file,
                Err(error) if create && error.raw_os_error() == Some(libc::ENOENT) => {
                    // SAFETY: mkdirat creates a direct child relative to a pinned directory.
                    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
                    if result != 0
                        && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                    {
                        return Err(io::Error::last_os_error());
                    }
                    open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?
                }
                Err(error) => return Err(error),
            };
            names.push(name);
            handles.push(next);
        }
        Ok(Self {
            path: path.to_owned(),
            handles,
            names,
        })
    }
    fn file(&self) -> &File {
        &self.handles[self.handles.len() - 1]
    }
    fn validate(&self) -> io::Result<()> {
        for (index, name) in self.names.iter().enumerate() {
            let named = at_stat(&self.handles[index], name)?
                .ok_or_else(|| fail("trash ancestor disappeared"))?;
            if !same(&named, &stat(&self.handles[index + 1])?) {
                return Err(fail("trash ancestor changed"));
            }
        }
        Ok(())
    }
    fn private(path: &Path) -> io::Result<Self> {
        let directory = Self::open(path, true)?;
        let metadata = stat(directory.file())?;
        // SAFETY: getuid has no pointer arguments or preconditions.
        if metadata.st_uid != unsafe { libc::getuid() } || metadata.st_mode & 0o022 != 0 {
            return Err(fail(
                "trash directory is not owned and write-protected for this user",
            ));
        }
        Ok(directory)
    }
}
use std::os::unix::fs::OpenOptionsExt;

struct Source {
    directory: Directory,
    name: CString,
    file: File,
    original: libc::stat,
}
impl Source {
    fn validate(&self) -> io::Result<()> {
        self.directory.validate()?;
        let named = at_stat(self.directory.file(), &self.name)?
            .ok_or_else(|| fail("trash source disappeared"))?;
        if !unchanged(&named, &self.original) || !unchanged(&stat(&self.file)?, &self.original) {
            return Err(fail("trash source changed after preparation"));
        }
        Ok(())
    }
}
struct Destination {
    files: Directory,
    info: Directory,
    name: CString,
    info_name: CString,
    metadata: File,
    copy: Option<(CString, File)>,
    committed: bool,
}
impl Destination {
    fn cleanup(&mut self) -> io::Result<()> {
        if self.committed {
            return Ok(());
        }
        let copy_error = self
            .copy
            .as_ref()
            .and_then(|(name, file)| unlink_same(self.files.file(), name, file).err());
        let metadata_error = unlink_same(self.info.file(), &self.info_name, &self.metadata).err();
        match (copy_error, metadata_error) {
            (None, None) => Ok(()),
            (Some(a), Some(b)) => Err(fail(format!("copy cleanup: {a}; metadata cleanup: {b}"))),
            (Some(error), None) | (None, Some(error)) => Err(error),
        }
    }
    fn validate(&self) -> io::Result<()> {
        self.files.validate()?;
        self.info.validate()?;
        let metadata = at_stat(self.info.file(), &self.info_name)?
            .ok_or_else(|| fail("trash metadata disappeared"))?;
        if !same(&metadata, &stat(&self.metadata)?) {
            return Err(fail("trash metadata was replaced"));
        }
        if let Some((name, file)) = &self.copy {
            let named = at_stat(self.files.file(), name)?
                .ok_or_else(|| fail("prepared trash copy disappeared"))?;
            if !same(&named, &stat(file)?) {
                return Err(fail("prepared trash copy was replaced"));
            }
        }
        Ok(())
    }
}
impl Drop for Destination {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!(
                "prepared trash cleanup retained artifacts in {}: {error}",
                self.files.path.display()
            );
        }
    }
}

/// Owned, Send preparation. Dropping it before admission never removes the
/// original source. Directory inputs remain directories and are refused.
pub(crate) struct PreparedTrash {
    source: Source,
    destination: Destination,
}
impl PreparedTrash {
    pub(crate) fn prepare(path: &Path) -> Result<Self, String> {
        Self::prepare_with(path, None)
            .map_err(|error| format!("prepare trash {}: {error}", path.display()))
    }
    fn prepare_with(path: &Path, selected: Option<&Path>) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| fail("trash source has no parent"))?;
        let name = c_name(
            path.file_name()
                .ok_or_else(|| fail("trash source has no name"))?,
        )?;
        let directory = Directory::open(parent, false)?;
        let file = open_at(directory.file(), &name, libc::O_PATH, 0)?;
        let original = stat(&file)?;
        if original.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(fail(
                "chat trash refuses directories, links and special files",
            ));
        }
        let source = Source {
            directory,
            name,
            file,
            original,
        };
        let trash = match selected {
            Some(path) => path.to_owned(),
            None => select_trash(path)?,
        };
        let _root = Directory::private(&trash)?;
        let files = Directory::private(&trash.join("files"))?;
        let info = Directory::private(&trash.join("info"))?;
        let basename = path
            .file_name()
            .ok_or_else(|| fail("trash source has no filename"))?;
        let mut suffix = 0u64;
        let (name, info_name, metadata) = loop {
            let mut candidate = basename.to_os_string();
            if suffix > 0 {
                candidate.push(format!(".{}", suffix + 1));
            }
            suffix = suffix
                .checked_add(1)
                .ok_or_else(|| fail("trash names exhausted"))?;
            let name = c_name(&candidate)?;
            let mut info_candidate = candidate;
            info_candidate.push(".trashinfo");
            let info_name = c_name(&info_candidate)?;
            if at_stat(files.file(), &name)?.is_some() {
                continue;
            }
            match open_at(
                info.file(),
                &info_name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            ) {
                Ok(metadata) => break (name, info_name, metadata),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        let mut destination = Destination {
            files,
            info,
            name,
            info_name,
            metadata,
            copy: None,
            committed: false,
        };
        write!(
            destination.metadata,
            "[Trash Info]\nPath={}\nDeletionDate={}\n",
            encoded_path(path),
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S")
        )?;
        destination.metadata.sync_all()?;
        if stat(destination.files.file())?.st_dev != source.original.st_dev {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|error| fail(error.to_string()))?;
            let mut temporary = Vec::with_capacity(49);
            temporary.extend_from_slice(b".seuoffice-trash-");
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for byte in random {
                temporary.push(HEX[usize::from(byte >> 4)]);
                temporary.push(HEX[usize::from(byte & 15)]);
            }
            let name =
                CString::new(temporary).map_err(|_| fail("private trash name contains NUL"))?;
            let copy = open_at(
                destination.files.file(),
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )?;
            destination.copy = Some((name, copy));
            let mut reader = File::open(format!("/proc/self/fd/{}", source.file.as_raw_fd()))?;
            let output = &mut destination
                .copy
                .as_mut()
                .ok_or_else(|| fail("missing prepared copy"))?
                .1;
            io::copy(&mut reader, output)?;
            output.set_permissions(fs::Permissions::from_mode(source.original.st_mode & 0o7777))?;
            output.sync_all()?;
            source.validate()?;
        }
        Ok(Self {
            source,
            destination,
        })
    }
    pub(crate) fn execute_authorized(
        mut self,
        authorize: &mut Authorize<'_>,
    ) -> Result<(), String> {
        let mut called = false;
        let mut actual = None;
        let guarded = authorize(&mut || {
            if called {
                return Err(NodeError {
                    code: None,
                    message: "trash admission was repeated".into(),
                });
            }
            called = true;
            let result = self
                .commit()
                .map_err(|error| main_core::node_fs::node_error_fd(&error, "trash"));
            let status = result.clone();
            actual = Some(result);
            status
        });
        let outcome = match (guarded, actual) {
            (_, Some(Err(error))) | (Err(error), _) => Err(error.message),
            (Ok(()), Some(Ok(()))) => Ok(()),
            (Ok(()), None) => Err("native owner did not admit trash".into()),
        };
        if let Err(error) = outcome {
            let location = if self.destination.committed {
                format!(
                    "; source already moved to trash at {}",
                    self.destination
                        .files
                        .path
                        .join(OsStr::from_bytes(self.destination.name.as_bytes()))
                        .display()
                )
            } else {
                String::new()
            };
            let cleanup = self
                .destination
                .cleanup()
                .err()
                .map(|failure| format!("; cleanup retained artifacts: {failure}"))
                .unwrap_or_default();
            return Err(format!("{error}{location}{cleanup}"));
        }
        Ok(())
    }
    fn commit(&mut self) -> io::Result<()> {
        self.source.validate()?;
        self.destination.validate()?;
        let source_dir = self.source.directory.file();
        let destination_dir = self.destination.files.file();
        if let Some((temporary, _)) = &self.destination.copy {
            // A cross-device source is first detached by a same-directory,
            // no-replace rename. Never unlink an unpinned original pathname.
            move_new(source_dir, &self.source.name, source_dir, temporary)?;
            let moved = at_stat(source_dir, temporary);
            let result = (|| {
                let moved = moved
                    .as_ref()
                    .map_err(|error| fail(format!("inspect detached source: {error}")))?
                    .as_ref()
                    .ok_or_else(|| fail("detached trash source disappeared"))?;
                if !same(moved, &self.source.original)
                    || moved.st_size != self.source.original.st_size
                    || moved.st_mtime != self.source.original.st_mtime
                    || moved.st_mtime_nsec != self.source.original.st_mtime_nsec
                {
                    return Err(fail("trash source changed during detachment"));
                }
                move_new(
                    destination_dir,
                    temporary,
                    destination_dir,
                    &self.destination.name,
                )?;
                self.destination.committed = true;
                unlink_same(source_dir, temporary, &self.source.file)
            })();
            if let Err(error) = result {
                let recover = (|| {
                    let current = at_stat(source_dir, temporary)?
                        .ok_or_else(|| fail("detached source disappeared before recovery"))?;
                    let expected = moved
                        .as_ref()
                        .ok()
                        .and_then(Option::as_ref)
                        .unwrap_or(&self.source.original);
                    if !same(expected, &current) {
                        return Err(fail("detached source changed before recovery"));
                    }
                    move_new(source_dir, temporary, source_dir, &self.source.name)
                })();
                if let Err(restore) = recover {
                    return Err(fail(format!(
                        "{error}; conditional restore failed: {restore}; detached source retained at {}",
                        self.source
                            .directory
                            .path
                            .join(OsStr::from_bytes(temporary.as_bytes()))
                            .display()
                    )));
                }
                if self.destination.committed {
                    // The original is back. Cleanup outside the authority gate
                    // removes only our captured destination copy and metadata.
                    self.destination.committed = false;
                    let final_name = self.destination.name.clone();
                    if let Some((name, _)) = &mut self.destination.copy {
                        *name = final_name;
                    }
                }
                return Err(error);
            }
        } else {
            move_new(
                source_dir,
                &self.source.name,
                destination_dir,
                &self.destination.name,
            )?;
            self.destination.committed = true;
            let moved = at_stat(destination_dir, &self.destination.name)?
                .ok_or_else(|| fail("moved trash source disappeared"))?;
            if !same(&moved, &self.source.original) {
                match move_new(
                    destination_dir,
                    &self.destination.name,
                    source_dir,
                    &self.source.name,
                ) {
                    Ok(()) => {
                        self.destination.committed = false;
                        return Err(fail(
                            "source was replaced during trash move; replacement restored",
                        ));
                    }
                    Err(error) => {
                        return Err(fail(format!(
                            "source was replaced during trash move; moved entry retained with restore metadata: {error}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

fn encoded_path(path: &Path) -> String {
    let mut encoded = String::new();
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(byte) {
            encoded.push(*byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}
fn mount_path(field: &[u8]) -> io::Result<PathBuf> {
    let mut bytes = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] == b'\\' {
            let digits = field
                .get(index + 1..index + 4)
                .ok_or_else(|| fail("invalid mountinfo escape"))?;
            if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                return Err(fail("invalid mountinfo octal escape"));
            }
            let value = ((digits[0] - b'0') as u16) * 64
                + ((digits[1] - b'0') as u16) * 8
                + (digits[2] - b'0') as u16;
            bytes.push(u8::try_from(value).map_err(|_| fail("mountinfo escape out of range"))?);
            index += 4;
        } else {
            bytes.push(field[index]);
            index += 1;
        }
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}
fn select_trash(source: &Path) -> io::Result<PathBuf> {
    let home = if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|path| !path.is_empty())
    {
        PathBuf::from(data).join("Trash")
    } else {
        PathBuf::from(
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .ok_or_else(|| fail("neither XDG_DATA_HOME nor HOME is set"))?,
        )
        .join(".local/share/Trash")
    };
    if !home.is_absolute() {
        return Err(fail("trash home must be absolute"));
    }
    let mounts = fs::read("/proc/self/mountinfo")?;
    let mut roots = Vec::new();
    for line in mounts
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let field = line
            .split(|byte| *byte == b' ')
            .nth(4)
            .ok_or_else(|| fail("malformed mountinfo record"))?;
        roots.push(mount_path(field)?);
    }
    roots.sort_by_key(|path| std::cmp::Reverse(path.as_os_str().len()));
    let containing = |path: &Path| {
        roots
            .iter()
            .find(|root| path.starts_with(root))
            .cloned()
            .ok_or_else(|| fail("mount point unavailable"))
    };
    let root = containing(source)?;
    if root == containing(&home)? {
        return Ok(home);
    }
    // SAFETY: getuid has no arguments or preconditions.
    let uid = unsafe { libc::getuid() };
    let common = root.join(".Trash");
    match fs::symlink_metadata(&common) {
        Ok(metadata)
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.mode() & 0o1000 != 0 =>
        {
            let candidate = common.join(uid.to_string());
            match Directory::private(&candidate) {
                Ok(_) => return Ok(candidate),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
                Err(error) => return Err(error),
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let candidate = root.join(format!(".Trash-{uid}"));
    match Directory::private(&candidate) {
        Ok(_) => Ok(candidate),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(home),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[path = "linux_tests.rs"]
mod tests;
