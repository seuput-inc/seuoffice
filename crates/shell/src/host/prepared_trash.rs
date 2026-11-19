//! A prepared recycle operation never uses a quarantine path as its restore path.
//!
//! Linux writes the original pathname into freedesktop metadata before admitting
//! the namespace change. Other desktop transports have a different admission
//! boundary; they must not be represented as an inode-conditional rename.
//!
//! The consumer authenticates the exact captured origin, project, root, data
//! folder and filename inside `Authorize`; OS witnesses do not authenticate a
//! renderer. See the original `apps/shell/src/main/projects/chat-store.ts`
//! `deleteChat`, which serializes a chat and awaits `deps.trash` without an
//! abort signal. Native admission preserves that already-accepted operation.
//! Before admission owner loss refuses all source effects. After admission
//! Finder/IFileOperation may finish despite owner loss and return their actual
//! error. This does not authorize a second path, retry, or a later operation.
//!
//! The native desktop APIs retain their original external-writer path race.
//! Nofollow witnesses reject observed replacements, links and reparse points;
//! they cannot turn an OS pathname API into conditional-inode CAS. Linux uses
//! no-replace, handle-relative moves with conditional recovery, not a claim
//! that unrelated external processes are serialized by the application gate.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::PreparedTrash;
#[cfg(any(target_os = "macos", windows))]
mod native;
#[cfg(any(target_os = "macos", windows))]
pub(crate) use native::PreparedTrash;

pub(crate) type Authorize<'a> = dyn FnMut(
        &mut dyn FnMut() -> Result<(), main_core::node_fs::NodeError>,
    ) -> Result<(), main_core::node_fs::NodeError>
    + 'a;

#[cfg(test)]
mod fixtures;
