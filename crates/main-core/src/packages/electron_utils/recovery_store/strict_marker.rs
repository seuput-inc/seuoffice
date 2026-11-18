//! Strict publication of an already-owned recovery journal stage.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;

use file_commit::{FileCommitReceipt, OwnedPreparedCommit, OwnedStage, PrivateDirectory, PublicationMode};

/// The stage and its private directory remain owned on every error.
pub struct StrictMarkerStage {
    pub parent: File,
    pub path: PathBuf,
    pub name: String,
    pub stage: OwnedStage,
}

pub struct StrictMarkerPrepared {
    parent: File,
    directory: PrivateDirectory,
    commit: OwnedPreparedCommit,
}

/// Publication succeeded. The receipt remains available even if directory sync fails.
pub struct StrictMarkerPublished {
    parent: File,
    directory: PrivateDirectory,
    receipt: FileCommitReceipt,
}

pub struct StrictMarkerDurable {
    directory: PrivateDirectory,
    receipt: FileCommitReceipt,
}

impl StrictMarkerDurable {
    pub fn receipt(&self) -> &FileCommitReceipt { &self.receipt }
    pub fn directory(&self) -> &PrivateDirectory { &self.directory }
}

impl StrictMarkerStage {
    pub fn prepare_json<T: serde::Serialize>(self, value: &T) -> Result<StrictMarkerPrepared, (io::Error, Self)> {
        let bytes = match serde_json::to_vec(value) {
            Ok(bytes) => bytes,
            Err(error) => return Err((io::Error::new(io::ErrorKind::InvalidData, error), self)),
        };
        self.prepare(&bytes)
    }

    /// Stage bytes and sync the exact descriptor before preparing the atomic rename.
    /// Run on a worker, outside native authority gates.
    pub fn prepare(mut self, bytes: &[u8]) -> Result<StrictMarkerPrepared, (io::Error, Self)> {
        if self.name.is_empty() || self.name == "." || self.name == ".."
            || self.name.chars().any(|c| matches!(c, '\0' | '/' | '\\'))
            || self.path.file_name().and_then(|name| name.to_str()) != Some(self.name.as_str())
        {
            return Err((io::Error::new(io::ErrorKind::InvalidInput, "invalid recovery marker name"), self));
        }
        let write = self.stage.file_mut().set_len(0)
            .and_then(|()| self.stage.file_mut().seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| self.stage.file_mut().write_all(bytes))
            .and_then(|()| self.stage.file_mut().sync_all());
        if let Err(error) = write { return Err((error, self)); }
        let Self { parent, path, name, stage } = self;
        let directory = stage.directory();
        #[cfg(unix)]
        let prepared = match std::ffi::CString::new(name.as_bytes()) {
            Ok(name) => stage.prepare(&parent, &name, &path, PublicationMode::Exclusive),
            Err(_) => return Err((io::Error::new(io::ErrorKind::InvalidInput, "invalid recovery marker name"), Self { parent, path, name, stage })),
        };
        #[cfg(windows)]
        let prepared = stage.prepare(&parent, &name, &path, PublicationMode::Exclusive);
        match prepared {
            Ok(commit) => Ok(StrictMarkerPrepared { parent, directory, commit }),
            Err((error, stage)) => Err((error, Self { parent, path, name, stage })),
        }
    }
}

impl StrictMarkerPrepared {
    /// Call only inside the exact bounded native namespace admission.
    pub fn publish(self) -> Result<StrictMarkerPublished, (io::Error, Self)> {
        let Self { parent, directory, commit } = self;
        match commit.publish() {
            Ok(receipt) => Ok(StrictMarkerPublished { parent, directory, receipt }),
            Err((error, commit)) => Err((error, Self { parent, directory, commit })),
        }
    }
}

impl StrictMarkerPublished {
    pub fn receipt(&self) -> &FileCommitReceipt { &self.receipt }

    /// Run on a worker. Unsupported directory sync is an error, not a durable receipt.
    pub fn sync_parent(self) -> Result<StrictMarkerDurable, (io::Error, Self)> {
        if let Err(error) = self.parent.sync_all() { return Err((error, self)); }
        Ok(StrictMarkerDurable { directory: self.directory, receipt: self.receipt })
    }
}
