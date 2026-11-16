use super::*;

pub struct ChatsDirectory {
    pub(super) guard: ProjectWriteGuard,
    pub(super) root_handle: fs::File,
    pub(super) data_handle: fs::File,
    pub(super) directory_handle: fs::File,
    pub(super) root_path: String,
    pub(super) data_name: String,
    pub(super) data_path: String,
    pub(super) directory_path: String,
}

fn authorized<T>(
    authorize: &mut Authorize<'_>,
    work: impl FnOnce() -> Result<T, WriteGuardError>,
) -> Result<T, WriteGuardError> {
    let mut actual = None;
    let result = run_authorized(authorize, || match work() {
        Ok(value) => Ok(value),
        Err(error) => {
            let status = NodeError {
                code: error.code(),
                message: error.message(),
            };
            actual = Some(error);
            Err(status)
        }
    });
    result.map_err(|error| actual.unwrap_or(WriteGuardError::Fs(error)))
}

#[cfg(unix)]
fn open_child(
    parent: &fs::File,
    name: &str,
    path: &str,
    create: bool,
) -> Result<Option<fs::File>, WriteGuardError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::MetadataExt;
    let name = std::ffi::CString::new(name)
        .map_err(|_| ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, path))?;
    if create {
        // SAFETY: the pinned parent/name are live and only this child is created.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(node_error(&error, "mkdir", path).into());
            }
        }
    }
    let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the parent/name are live; fstatat initializes info on success.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        if !create && error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        return Err(node_error(&error, "lstat", path).into());
    }
    // SAFETY: fstatat succeeded.
    let info = unsafe { info.assume_init() };
    if info.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(ProjectWriteRefused::new(
            if info.st_mode & libc::S_IFMT == libc::S_IFLNK {
                ProjectWriteRefusal::Link
            } else {
                ProjectWriteRefusal::NotAFolder
            },
            path,
        )
        .into());
    }
    // SAFETY: the pinned parent/name are live; openat returns a newly owned descriptor.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(node_error(&std::io::Error::last_os_error(), "open", path).into());
    }
    // SAFETY: fd came from successful openat and has not been transferred.
    let directory = unsafe { fs::File::from_raw_fd(fd) };
    let held = directory
        .metadata()
        .map_err(|error| node_error(&error, "fstat", path))?;
    if held.dev() != stat_dev(&info) || held.ino() != info.st_ino {
        return Err(NodeError {
            code: None,
            message: "project data directory changed".into(),
        }
        .into());
    }
    Ok(Some(directory))
}

#[cfg(windows)]
fn directory_error(error: NodeError, path: &str) -> WriteGuardError {
    match error.code {
        Some("ELOOP") => ProjectWriteRefused::new(ProjectWriteRefusal::Link, path).into(),
        Some("ENOTDIR") => ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, path).into(),
        _ => error.into(),
    }
}

impl ProjectWriteGuard {
    pub fn chats_directory(
        &self,
        root: &str,
        data_dir: Option<&str>,
        create: bool,
        authorize: &mut Authorize<'_>,
    ) -> Result<Option<ChatsDirectory>, WriteGuardError> {
        self.assert_root(root)?;
        let data_name = data_dir
            .map(str::to_owned)
            .unwrap_or_else(|| data_dir_name(root));
        if !is_data_dir_name(&data_name) {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, data_name).into());
        }
        let data_path = join(&[root, &data_name]);
        let directory_path = join(&[
            &data_path,
            crate::packages::project_store::project_folder_name("chatsDir"),
        ]);
        #[cfg(unix)]
        let root_handle = open_stage_dir(root)?;
        #[cfg(windows)]
        let root_handle = staging_windows::open_existing_dir(root)?;
        let open_data = || -> Result<Option<fs::File>, WriteGuardError> {
            #[cfg(unix)]
            {
                if !stage_handle_matches(&root_handle, root)? {
                    return Err(
                        ProjectWriteRefused::new(ProjectWriteRefusal::RootMoved, root).into(),
                    );
                }
                open_child(&root_handle, &data_name, &data_path, create)
            }
            #[cfg(windows)]
            {
                if !staging_windows::handle_matches_path(&root_handle, root)? {
                    return Err(
                        ProjectWriteRefused::new(ProjectWriteRefusal::RootMoved, root).into(),
                    );
                }
                staging_windows::open_data_directory(&root_handle, &data_name, root, create)
                    .map_err(|error| directory_error(error, &data_path))
            }
        };
        let data = if create {
            authorized(authorize, open_data)?
        } else {
            open_data()?
        };
        let Some(data_handle) = data else {
            return Ok(None);
        };
        let open_chats = || -> Result<Option<fs::File>, WriteGuardError> {
            #[cfg(unix)]
            {
                if !stage_handle_matches(&root_handle, root)?
                    || !stage_handle_matches(&data_handle, &data_path)?
                {
                    return Err(
                        ProjectWriteRefused::new(ProjectWriteRefusal::RootMoved, root).into(),
                    );
                }
                open_child(
                    &data_handle,
                    crate::packages::project_store::project_folder_name("chatsDir"),
                    &directory_path,
                    create,
                )
            }
            #[cfg(windows)]
            {
                if !staging_windows::handle_matches_path(&root_handle, root)?
                    || !staging_windows::handle_matches_path(&data_handle, &data_path)?
                {
                    return Err(
                        ProjectWriteRefused::new(ProjectWriteRefusal::RootMoved, root).into(),
                    );
                }
                staging_windows::open_chats_directory(&data_handle, root, create)
                    .map_err(|error| directory_error(error, &directory_path))
            }
        };
        let directory = if create {
            authorized(authorize, open_chats)?
        } else {
            open_chats()?
        };
        Ok(directory.map(|directory_handle| ChatsDirectory {
            guard: ProjectWriteGuard::new(&self.platform),
            root_handle,
            data_handle,
            directory_handle,
            root_path: root.to_owned(),
            data_name,
            data_path,
            directory_path,
        }))
    }
}

impl ChatsDirectory {
    pub fn path(&self, name: &str) -> Result<String, ProjectWriteRefused> {
        if data_segments(name)?.len() != 1 {
            return Err(ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, name));
        }
        Ok(join(&[&self.directory_path, name]))
    }

    pub(super) fn matches(&self) -> Result<bool, NodeError> {
        #[cfg(unix)]
        {
            Ok(stage_handle_matches(&self.root_handle, &self.root_path)?
                && stage_handle_matches(&self.data_handle, &self.data_path)?
                && stage_handle_matches(&self.directory_handle, &self.directory_path)?)
        }
        #[cfg(windows)]
        {
            Ok(
                staging_windows::handle_matches_path(&self.root_handle, &self.root_path)?
                    && staging_windows::handle_matches_path(&self.data_handle, &self.data_path)?
                    && staging_windows::handle_matches_path(
                        &self.directory_handle,
                        &self.directory_path,
                    )?,
            )
        }
    }

    pub fn write_authorized(
        &self,
        name: &str,
        bytes: &[u8],
        authorize: &mut Authorize<'_>,
    ) -> Result<String, WriteGuardError> {
        self.path(name)?;
        self.guard.write_data_authorized_inner(
            &self.root_path,
            name,
            bytes,
            Some(&self.data_name),
            false,
            Some(self),
            authorize,
        )
    }

    pub fn update_authorized<E: From<WriteGuardError>>(
        &self,
        name: &str,
        change: DataChange<'_, E>,
        authorize: &mut Authorize<'_>,
    ) -> Result<(DataUpdateResult, Vec<String>), E> {
        self.path(name).map_err(|error| E::from(error.into()))?;
        if !self.matches().map_err(|error| E::from(error.into()))? {
            return Err(E::from(
                ProjectWriteRefused::new(ProjectWriteRefusal::RootMoved, &self.root_path).into(),
            ));
        }
        self.guard.update_data_authorized_inner(
            &self.root_path,
            name,
            change,
            Some(&self.data_name),
            Some(self),
            authorize,
        )
    }

    #[cfg(unix)]
    pub fn append_authorized(
        &self,
        name: &str,
        text: &str,
        authorize: &mut Authorize<'_>,
    ) -> Result<(), WriteGuardError> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::MetadataExt;
        let path = self.path(name)?;
        let filename = std::ffi::CString::new(name)
            .map_err(|_| ProjectWriteRefused::new(ProjectWriteRefusal::BadPath, name))?;
        let file = authorized(authorize, || {
            if !self.matches()? {
                return Err(ProjectWriteRefused::new(
                    ProjectWriteRefusal::RootMoved,
                    &self.root_path,
                )
                .into());
            }
            let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: the pinned directory/name are live; fstatat initializes info on success.
            if unsafe {
                libc::fstatat(
                    self.directory_handle.as_raw_fd(),
                    filename.as_ptr(),
                    info.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
            {
                // SAFETY: fstatat succeeded.
                let info = unsafe { info.assume_init() };
                if info.st_mode & libc::S_IFMT != libc::S_IFREG {
                    return Err(ProjectWriteRefused::new(
                        if info.st_mode & libc::S_IFMT == libc::S_IFLNK {
                            ProjectWriteRefusal::Link
                        } else {
                            ProjectWriteRefusal::NotAFolder
                        },
                        &path,
                    )
                    .into());
                }
                if info.st_nlink > 1 {
                    return Err(
                        ProjectWriteRefused::new(ProjectWriteRefusal::HardLink, &path).into(),
                    );
                }
            } else {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ENOENT) {
                    return Err(node_error(&error, "lstat", &path).into());
                }
            }
            // SAFETY: the pinned directory/name are live and openat returns a new owned descriptor.
            let fd = unsafe {
                libc::openat(
                    self.directory_handle.as_raw_fd(),
                    filename.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_APPEND
                        | libc::O_CREAT
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK
                        | libc::O_CLOEXEC,
                    0o666,
                )
            };
            if fd < 0 {
                return Err(node_error(&std::io::Error::last_os_error(), "open", &path).into());
            }
            // SAFETY: fd came from successful openat and has not been transferred.
            let file = unsafe { fs::File::from_raw_fd(fd) };
            let info = file
                .metadata()
                .map_err(|error| node_error(&error, "fstat", &path))?;
            if !info.is_file() {
                return Err(
                    ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, &path).into(),
                );
            }
            if info.nlink() > 1 {
                return Err(ProjectWriteRefused::new(ProjectWriteRefusal::HardLink, &path).into());
            }
            Ok(file)
        })?;
        let bytes = text.as_bytes();
        let mut offset = 0;
        while offset < bytes.len() {
            let result = authorized(authorize, || {
                if !self.matches()? {
                    return Err(ProjectWriteRefused::new(
                        ProjectWriteRefusal::RootMoved,
                        &self.root_path,
                    )
                    .into());
                }
                let held = file
                    .metadata()
                    .map_err(|error| node_error(&error, "fstat", &path))?;
                if held.nlink() > 1 {
                    return Err(
                        ProjectWriteRefused::new(ProjectWriteRefusal::HardLink, &path).into(),
                    );
                }
                let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
                // SAFETY: the pinned directory/name are live; fstatat initializes info on success.
                if unsafe {
                    libc::fstatat(
                        self.directory_handle.as_raw_fd(),
                        filename.as_ptr(),
                        info.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } != 0
                {
                    return Err(node_error(&std::io::Error::last_os_error(), "lstat", &path).into());
                }
                // SAFETY: fstatat succeeded.
                let info = unsafe { info.assume_init() };
                if info.st_mode & libc::S_IFMT != libc::S_IFREG
                    || held.dev() != stat_dev(&info)
                    || held.ino() != info.st_ino
                {
                    return Err(NodeError {
                        code: None,
                        message: "chat append target changed".into(),
                    }
                    .into());
                }
                let remaining = &bytes[offset..];
                // SAFETY: file is open for append; remaining is a live readable slice.
                let written = unsafe {
                    libc::write(file.as_raw_fd(), remaining.as_ptr().cast(), remaining.len())
                };
                if written < 0 {
                    return Err(node_error(&std::io::Error::last_os_error(), "write", &path).into());
                }
                Ok(written as usize)
            });
            match result {
                Ok(0) => {
                    return Err(NodeError {
                        code: None,
                        message: "chat append made no progress".into(),
                    }
                    .into());
                }
                Ok(written) => offset += written,
                Err(WriteGuardError::Fs(error)) if error.code == Some("EINTR") => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn append_authorized(
        &self,
        name: &str,
        text: &str,
        authorize: &mut Authorize<'_>,
    ) -> Result<(), WriteGuardError> {
        let path = self.path(name)?;
        let mut file = authorized(authorize, || {
            if !self.matches()? {
                return Err(ProjectWriteRefused::new(
                    ProjectWriteRefusal::RootMoved,
                    &self.root_path,
                )
                .into());
            }
            match staging_windows::open_data_append_target(
                &self.directory_handle,
                name,
                &self.root_path,
                true,
            )? {
                staging_windows::DataAppendTarget::File(file) => Ok(file),
                staging_windows::DataAppendTarget::HardLink => {
                    Err(ProjectWriteRefused::new(ProjectWriteRefusal::HardLink, &path).into())
                }
                staging_windows::DataAppendTarget::Directory => {
                    Err(ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, &path).into())
                }
                staging_windows::DataAppendTarget::Reparse => {
                    Err(ProjectWriteRefused::new(ProjectWriteRefusal::Link, &path).into())
                }
                staging_windows::DataAppendTarget::Missing => Err(NodeError {
                    code: Some("ENOENT"),
                    message: "chat append target was not created".into(),
                }
                .into()),
            }
        })?;
        let bytes = text.as_bytes();
        let mut offset = 0;
        while offset < bytes.len() {
            let written = authorized(authorize, || {
                if !self.matches()?
                    || !staging_windows::data_append_target_matches(
                        &self.directory_handle,
                        name,
                        &self.root_path,
                        &file,
                    )?
                {
                    return Err(NodeError {
                        code: None,
                        message: "chat append target changed".into(),
                    }
                    .into());
                }
                match staging_windows::append_data_target(&mut file, &bytes[offset..])? {
                    staging_windows::DataAppendWrite::Written(written) => Ok(written),
                    staging_windows::DataAppendWrite::HardLink => {
                        Err(ProjectWriteRefused::new(ProjectWriteRefusal::HardLink, &path).into())
                    }
                }
            })?;
            if written == 0 {
                return Err(NodeError {
                    code: None,
                    message: "chat append made no progress".into(),
                }
                .into());
            }
            offset += written;
        }
        Ok(())
    }

    pub fn read(&self, name: &str) -> Result<Option<String>, WriteGuardError> {
        self.path(name)?;
        if !self.matches()? {
            return Err(
                ProjectWriteRefused::new(ProjectWriteRefusal::RootMoved, &self.root_path).into(),
            );
        }
        #[cfg(unix)]
        {
            Ok(data_unix::Snapshot::capture_chats(self, name)?.1)
        }
        #[cfg(windows)]
        {
            match staging_windows::open_data_read_target(
                &self.directory_handle,
                name,
                &self.root_path,
            )? {
                staging_windows::DataReadTarget::Missing => Ok(None),
                staging_windows::DataReadTarget::File(mut file) => {
                    Ok(Some(staging_windows::read_data_target(&mut file)?))
                }
                staging_windows::DataReadTarget::Directory => {
                    Err(ProjectWriteRefused::new(ProjectWriteRefusal::NotAFolder, name).into())
                }
                staging_windows::DataReadTarget::Reparse => {
                    Err(ProjectWriteRefused::new(ProjectWriteRefusal::Link, name).into())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (std::path::PathBuf, ChatsDirectory) {
        let serial = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "project-chats-scope-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let scope = ProjectWriteGuard::new(std::env::consts::OS)
            .chats_directory(root.to_str().unwrap(), None, true, &mut |effect| effect())
            .unwrap()
            .unwrap();
        (root, scope)
    }

    #[test]
    fn scoped_updates_stay_in_the_exact_chats_directory() {
        let (root, scope) = fixture();
        assert!(scope.path("../project.json").is_err());
        assert!(scope.path("other/chat.json").is_err());
        let retained = scope
            .write_authorized("chat.json", b"before", &mut |effect| effect())
            .unwrap();
        let (updated, more) = scope
            .update_authorized::<WriteGuardError>(
                "chat.json",
                &mut |current| {
                    assert_eq!(current, Some("before"));
                    Ok(Some("after".into()))
                },
                &mut |effect| effect(),
            )
            .unwrap();
        assert!(updated.written);
        assert_eq!(scope.read("chat.json").unwrap().as_deref(), Some("after"));
        assert!(!root.join(".seuoffice/project.json").exists());
        drop((scope, retained, more));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revoked_append_does_not_create_the_message_file() {
        let (root, scope) = fixture();
        assert!(
            scope
                .append_authorized("chat.jsonl", "message\n", &mut |_| {
                    Err(NodeError {
                        code: None,
                        message: "owner revoked".into(),
                    })
                })
                .is_err()
        );
        assert!(!std::path::Path::new(&scope.path("chat.jsonl").unwrap()).exists());
        drop(scope);
        fs::remove_dir_all(root).unwrap();
    }
}
