//! Staged cache/index replacement with an exact, retryable inverse plan.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use main_core::packages::electron_utils::atomic_write::temp_path_beside;
use main_core::shell::projects::write_guard::rename_no_replace;
use sha2::{Digest, Sha256};

fn path_text(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| "The Drive cache path is not valid Unicode".into())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Fingerprint {
    device: u64,
    file: u64,
    pub(super) hash: String,
}

#[cfg(unix)]
fn file_id(file: &fs::File) -> Result<(u64, u64), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().map_err(|error| error.to_string())?;
    Ok((meta.dev(), meta.ino()))
}
#[cfg(windows)]
fn file_id(file: &fs::File) -> Result<(u64, u64), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}
fn fingerprint(file: &mut fs::File) -> Result<Fingerprint, String> {
    use std::io::{Read, Seek, SeekFrom};
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .file_type()
        .is_file()
    {
        return Err("A Drive transaction path changed type".into());
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let (device, id) = file_id(&file)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if file_id(&file)? != (device, id) {
        return Err("A Drive transaction file changed while reading".into());
    }
    let digest = hasher.finalize();
    let mut hash = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        hash.push(HEX[(byte >> 4) as usize] as char);
        hash.push(HEX[(byte & 15) as usize] as char);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    Ok(Fingerprint {
        device,
        file: id,
        hash,
    })
}

pub(super) fn digest(path: &Path) -> Result<Option<Fingerprint>, String> {
    capture_selected(path).map(|(fingerprint, _)| fingerprint)
}

pub(super) fn capture_selected(
    path: &Path,
) -> Result<(Option<Fingerprint>, Option<fs::File>), String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, None)),
        Err(error) => return Err(error.to_string()),
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if file
            .metadata()
            .map_err(|error| error.to_string())?
            .file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err("A Drive transaction path is a reparse point".into());
        }
    }
    let identity = fingerprint(&mut file)?;
    Ok((Some(identity), Some(file)))
}
fn reserve_name(path: &Path) -> Result<PathBuf, String> {
    let text = path_text(path)?;
    for _ in 0..16 {
        let candidate = PathBuf::from(temp_path_beside(text));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("A unique Drive transaction path could not be reserved".into())
}
fn stage(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let text = path_text(path)?;
    for _ in 0..16 {
        let candidate = PathBuf::from(temp_path_beside(text));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
                    drop(file);
                    let _ = fs::remove_file(&candidate);
                    return Err(error.to_string());
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("A unique Drive staging path could not be created".into())
}

#[cfg(unix)]
fn prepare_content_publication(
    from: &Path,
    to: &Path,
) -> Result<file_commit::PreparedCommit, String> {
    use std::os::unix::ffi::OsStrExt;
    let from_parent = fs::File::open(from.parent().ok_or("The staging path has no parent")?)
        .map_err(|error| error.to_string())?;
    let to_parent = fs::File::open(to.parent().ok_or("The destination path has no parent")?)
        .map_err(|error| error.to_string())?;
    let source = OpenOptions::new()
        .read(true)
        .open(from)
        .map_err(|error| error.to_string())?;
    let from_name = std::ffi::CString::new(
        from.file_name()
            .ok_or("The staging path has no name")?
            .as_bytes(),
    )
    .map_err(|error| error.to_string())?;
    let to_name = std::ffi::CString::new(
        to.file_name()
            .ok_or("The destination path has no name")?
            .as_bytes(),
    )
    .map_err(|error| error.to_string())?;
    file_commit::prepare_commit_between(
        &from_parent,
        &source,
        &from_name,
        &to_parent,
        &to_name,
        to,
        file_commit::PublicationMode::Exclusive,
    )
    .map_err(|error| error.to_string())
}

#[cfg(windows)]
fn prepare_content_publication(
    from: &Path,
    to: &Path,
) -> Result<file_commit::PreparedCommit, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    };
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(to.parent().ok_or("The destination path has no parent")?)
        .map_err(|error| error.to_string())?;
    let source = OpenOptions::new()
        .read(true)
        .write(true)
        .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
        .open(from)
        .map_err(|error| error.to_string())?;
    let name = to
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("The destination name is not valid Unicode")?;
    file_commit::prepare_commit(
        &source,
        &parent,
        name,
        to,
        file_commit::PublicationMode::Exclusive,
    )
    .map_err(|error| error.to_string())
}

struct Inverse {
    from: PathBuf,
    to: PathBuf,
    expected: Fingerprint,
    from_private: bool,
    held: Option<PathBuf>,
}

/// Paths and inverse operations remain owned until rollback or cleanup succeeds.
pub(in crate::drive) struct Transaction {
    old: PathBuf,
    destination: PathBuf,
    index: PathBuf,
    quarantine: PathBuf,
    old_digest: Option<Fingerprint>,
    destination_digest: Option<Fingerprint>,
    index_digest: Option<Fingerprint>,
    staged_content: PathBuf,
    staged_index: PathBuf,
    content_digest: Fingerprint,
    staged_index_digest: Fingerprint,
    content_publication: Option<file_commit::PreparedCommit>,
    backups: Vec<(PathBuf, Fingerprint)>,
    inverse: Vec<Inverse>,
}

impl Transaction {
    pub(super) fn prepare(
        old: PathBuf,
        expected_old: Option<Fingerprint>,
        destination: PathBuf,
        index: PathBuf,
        content: &[u8],
        index_bytes: &[u8],
    ) -> Result<Self, String> {
        let old_digest = digest(&old)?;
        if old_digest != expected_old {
            return Err("The Drive cache file changed during download".into());
        }
        let destination_digest = if old == destination {
            old_digest.clone()
        } else {
            digest(&destination)?
        };
        let index_digest = digest(&index)?;
        let parent = index.parent().ok_or("The Drive index has no parent")?;
        let quarantine = reserve_name(&parent.join(".drive-transaction"))?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&quarantine)
            .map_err(|error| error.to_string())?;
        let staged_content = stage(&quarantine.join("content"), content)?;
        let staged_index = match stage(&quarantine.join("index"), index_bytes) {
            Ok(path) => path,
            Err(error) => {
                let _ = fs::remove_file(&staged_content);
                let _ = fs::remove_dir(&quarantine);
                return Err(error);
            }
        };
        let content_digest =
            digest(&staged_content)?.ok_or("The staged Drive content disappeared")?;
        let staged_index_digest =
            digest(&staged_index)?.ok_or("The staged Drive index disappeared")?;
        let content_publication = prepare_content_publication(&staged_content, &destination)?;
        Ok(Self {
            old,
            destination,
            index,
            quarantine,
            old_digest,
            destination_digest,
            index_digest,
            staged_content,
            staged_index,
            content_digest,
            staged_index_digest,
            content_publication: Some(content_publication),
            backups: Vec::new(),
            inverse: Vec::new(),
        })
    }

    fn move_exact(
        &mut self,
        from: &Path,
        to: &Path,
        expected: &Fingerprint,
        from_private: bool,
    ) -> Result<(), String> {
        if from_private && digest(from)?.as_ref() != Some(expected) {
            return Err("A Drive staging source changed".into());
        }
        rename_no_replace(path_text(from)?, path_text(to)?).map_err(|error| error.to_string())?;
        self.inverse.push(Inverse {
            from: to.to_owned(),
            to: from.to_owned(),
            expected: expected.clone(),
            from_private: !from_private,
            held: None,
        });
        let actual = digest(to)?;
        if actual.as_ref() != Some(expected) {
            if let Some(actual) = actual {
                self.inverse.last_mut().unwrap().expected = actual;
            }
            return Err("A Drive transaction source changed during the move".into());
        }
        Ok(())
    }

    fn backup(&mut self, path: &Path, expected: &Fingerprint) -> Result<(), String> {
        let backup = reserve_name(&self.quarantine.join("backup"))?;
        self.move_exact(path, &backup, expected, false)?;
        self.backups.push((backup, expected.clone()));
        Ok(())
    }

    /// Called once inside the original destination's canonical admission.
    pub(super) fn commit(&mut self) -> Result<file_commit::FileCommitReceipt, String> {
        if digest(&self.old)? != self.old_digest
            || digest(&self.destination)? != self.destination_digest
            || digest(&self.index)? != self.index_digest
        {
            return Err("A Drive cache file or index changed while the refresh was staged".into());
        }
        if let Some(expected) = self.old_digest.clone() {
            let path = self.old.clone();
            self.backup(&path, &expected)?;
        }
        if self.old != self.destination {
            if let Some(expected) = self.destination_digest.clone() {
                let path = self.destination.clone();
                self.backup(&path, &expected)?;
            }
        }
        let publication = self
            .content_publication
            .take()
            .ok_or("The Drive content publication was already used")?;
        let receipt = publication.publish().map_err(|error| error.to_string())?;
        self.inverse.push(Inverse {
            from: self.destination.clone(),
            to: self.staged_content.clone(),
            expected: self.content_digest.clone(),
            from_private: false,
            held: None,
        });
        if let Some(expected) = self.index_digest.clone() {
            let path = self.index.clone();
            self.backup(&path, &expected)?;
        }
        let (from, to, expected) = (
            self.staged_index.clone(),
            self.index.clone(),
            self.staged_index_digest.clone(),
        );
        self.move_exact(&from, &to, &expected, true)?;
        Ok(receipt)
    }

    /// Undo only moves whose identity is still the one installed by this plan.
    pub(in crate::drive) fn rollback(&mut self) -> Result<(), String> {
        while let Some(step) = self.inverse.last_mut() {
            if step.from_private {
                rename_no_replace(path_text(&step.from)?, path_text(&step.to)?)
                    .map_err(|error| error.to_string())?;
                self.inverse.pop();
                continue;
            }
            if let Some(held) = step.held.as_ref() {
                rename_no_replace(path_text(held)?, path_text(&step.from)?)
                    .map_err(|error| error.to_string())?;
                step.held = None;
                return Err("A Drive rollback source was replaced".into());
            }
            let held = reserve_name(&self.quarantine.join("rollback"))?;
            rename_no_replace(path_text(&step.from)?, path_text(&held)?)
                .map_err(|error| error.to_string())?;
            step.held = Some(held.clone());
            if digest(&held)?.as_ref() != Some(&step.expected) {
                rename_no_replace(path_text(&held)?, path_text(&step.from)?)
                    .map_err(|error| error.to_string())?;
                step.held = None;
                return Err("A Drive rollback source was replaced".into());
            }
            rename_no_replace(path_text(&held)?, path_text(&step.to)?)
                .map_err(|error| error.to_string())?;
            self.inverse.pop();
        }
        self.cleanup()
    }

    /// Cleanup is separately retryable after both committed identities exist.
    pub(in crate::drive) fn cleanup(&mut self) -> Result<(), String> {
        for path in self
            .backups
            .iter()
            .map(|(path, _)| path)
            .chain([&self.staged_content, &self.staged_index])
        {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        match fs::remove_dir(&self.quarantine) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        self.backups.clear();
        Ok(())
    }
}
