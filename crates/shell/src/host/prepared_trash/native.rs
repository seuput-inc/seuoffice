//! Finder and IFileOperation accept original-path operations, not inode CAS.
//! The original chat caller awaits trash without cancellation after invocation.
//! Admission is one-shot; subsequent owner loss cannot cancel an accepted OS
//! operation. No lock is held across OS transport or user/desktop interaction.

use std::path::{Path, PathBuf};
use main_core::node_fs::NodeError;
use super::Authorize;

#[cfg(target_os = "macos")]
#[path = "native/macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "native/windows.rs"]
mod platform;

pub(crate) struct PreparedTrash { path: PathBuf, witness: platform::Witness }

/// This is neither reusable authority nor a promise about native rename
/// atomicity. Owning the prepared object fixes the only path it can authorize.
struct Admission(PreparedTrash);
impl PreparedTrash {
    pub(crate) fn prepare(path: &Path) -> Result<Self, String> {
        let witness = platform::Witness::capture(path).map_err(|error| format!("prepare trash {}: {error}", path.display()))?;
        Ok(Self { path: path.to_owned(), witness })
    }
    fn admit(self, authorize: &mut Authorize<'_>) -> Result<Admission, String> {
        let mut pending = Some(self);
        let mut accepted = None;
        let guarded = authorize(&mut || {
            let prepared = pending.take().ok_or_else(|| NodeError { code: None, message: "trash admission repeated".into() })?;
            prepared.witness.validate().map_err(|error| NodeError { code: None, message: error.to_string() })?;
            accepted = Some(Admission(prepared));
            Ok(())
        });
        guarded.map_err(|error| error.message)?;
        accepted.ok_or_else(|| "native owner did not admit trash".into())
    }
    pub(crate) fn execute_authorized(self, authorize: &mut Authorize<'_>) -> Result<(), String> {
        self.admit(authorize)?.start(|path| trash::delete(path).map_err(|error| error.to_string()))
    }
}
impl Admission {
    fn start(self, transport: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
        let PreparedTrash { path, witness } = self.0;
        // An external native writer can still replace a pathname after this
        // check. Finder/IFileOperation provide no atomic conditional-inode API.
        witness.validate().map_err(|error| format!("trash witness changed before native transport: {error}"))?;
        let result = transport(&path);
        // Keep every captured root/data/target handle through the OS result.
        drop(witness);
        result
    }
}

#[cfg(test)]
#[path = "native/tests.rs"]
mod tests;
