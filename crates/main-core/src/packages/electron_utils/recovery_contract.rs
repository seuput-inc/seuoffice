//! `packages/electron-utils/src/recovery-contract.ts`: what every editor main
//! promises the shell about crash recovery, and the table the shell looks
//! editors up in.
//!
//! The TypeScript keeps the table on `globalThis` behind a `Symbol.for` key
//! so bundled copies share it; here the table is an explicit
//! [`RecoveryRegistry`] the caller owns. `C` is the editor's webContents type.
//! `A` is the opening authority captured before target creation. The platform
//! supplies its concrete type; the registry never constructs it.
//! `writeCopiesNow` answers through a completion callback.

use std::rc::Rc;

use super::recovery_store::{RecoveryEditorKind, RecoveryIdentity};

/// `RecoveryOpenFileRequest.decision`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOpenDecision {
    /// load the copy's bytes; the original path stays the tab's path
    Restore,
    /// load the file as saved, without asking
    Original,
}

/// open a file tab whose recovery question is already answered
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryOpenFileRequest {
    pub file_path: String,
    pub decision: RecoveryOpenDecision,
}

/// open a copy as an untitled tab
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryOpenUntitledRequest {
    /// the copy to load: an untitled copy, or a file copy whose original is missing
    pub copy: RecoveryIdentity,
    /// tab title to show
    pub title: String,
    /// first Save As suggestion, set when the original file could not be found
    pub save_as_default_path: Option<String>,
}

/// Completes the original authorized read/adopt/mark/seed preparation. A target
/// must not navigate or become a ready loaded view before this succeeds.
pub type RecoveryOpenDone = Box<dyn FnOnce(Result<(), crate::shell::renderer_disposal::Failure>)>;

/// `RecoveryEditorContract`
pub trait RecoveryEditorContract<C, A> {
    fn kind(&self) -> &str;
    /// The OS is ending the session. Answer once every copy has settled, or
    /// report the failure that prevented a copy from being written.
    fn write_copies_now(
        &self,
        done: Box<dyn FnOnce(Result<(), crate::shell::renderer_disposal::Failure>)>,
    );
    /// load `request.file_path` into `contents` (a tab the shell created)
    fn open_file_with_decision(
        &self,
        contents: &C,
        request: &RecoveryOpenFileRequest,
        authority: &A,
        done: RecoveryOpenDone,
    );
    /// Load a copy into `contents` as an untitled tab.
    fn open_untitled_copy(
        &self,
        contents: &C,
        request: &RecoveryOpenUntitledRequest,
        authority: &A,
        done: RecoveryOpenDone,
    );
    /// Whether the document in this tab cannot get a safe copy.
    fn is_unprotectable(&self, contents: &C) -> bool;
}

/// `registry()`: a `Map` from kind to contract, in the order of first registration.
pub struct RecoveryRegistry<C, A> {
    table: Vec<(RecoveryEditorKind, Rc<dyn RecoveryEditorContract<C, A>>)>,
}

impl<C, A> Default for RecoveryRegistry<C, A> {
    fn default() -> Self {
        RecoveryRegistry { table: Vec::new() }
    }
}

/// The function `registerRecoveryEditor` returns: hand it to
/// [`RecoveryRegistry::unregister`].
pub struct RecoveryRegistration<C, A> {
    contract: Rc<dyn RecoveryEditorContract<C, A>>,
}

impl<C, A> RecoveryRegistry<C, A> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an editor's contract (a later registration of the same kind
    /// replaces the earlier one and keeps its place in the order).
    pub fn register_recovery_editor(
        &mut self,
        contract: Rc<dyn RecoveryEditorContract<C, A>>,
    ) -> RecoveryRegistration<C, A> {
        let kind = contract.kind().to_owned();
        match self.table.iter_mut().find(|(k, _)| *k == kind) {
            Some(entry) => entry.1 = Rc::clone(&contract),
            None => self.table.push((kind, Rc::clone(&contract))),
        }
        RecoveryRegistration { contract }
    }

    /// The unregister function: removes the contract unless a newer
    /// registration has replaced it meanwhile.
    pub fn unregister(&mut self, registration: &RecoveryRegistration<C, A>) {
        let kind = registration.contract.kind();
        let current = self.table.iter().position(|(k, _)| k == kind);
        if let Some(at) = current
            && Rc::ptr_eq(&self.table[at].1, &registration.contract)
        {
            self.table.remove(at);
        }
    }

    pub fn recovery_editor(&self, kind: &str) -> Option<Rc<dyn RecoveryEditorContract<C, A>>> {
        self.table
            .iter()
            .find(|(k, _)| k == kind)
            .map(|(_, contract)| Rc::clone(contract))
    }

    pub fn recovery_editors(&self) -> Vec<Rc<dyn RecoveryEditorContract<C, A>>> {
        self.table
            .iter()
            .map(|(_, contract)| Rc::clone(contract))
            .collect()
    }
}
