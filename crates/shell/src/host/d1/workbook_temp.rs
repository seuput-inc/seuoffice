//! Private per-operation workbook staging directory.

use shell_ipc_d1::sheets::engine::{EngineResult, engine_error};
use std::path::{Path, PathBuf};

pub(super) struct WorkDir(PathBuf);

impl WorkDir {
    pub fn create(prefix: &str) -> EngineResult<Self> {
        let path = std::env::temp_dir().join(format!("{prefix}{}", jscompat::random_uuid()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|error| {
            engine_error(
                main_core::node_fs::node_error(&error, "mkdir", &path.to_string_lossy()).message,
            )
        })?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
