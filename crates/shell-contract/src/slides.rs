//! The slides deck a view shows, and the design-template memory the slides AI
//! keeps for each deck. D1 provides [`SlidesDocuments`], A provides
//! [`SlidesTemplateSelectionHooks`]; both go through
//! [`crate::provided::Provided`], so neither batch crate depends on the other.

use crate::ids::ViewId;

/// A slides session: `id` stays fixed for the session's life, `path` is ""
/// before the first save
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlidesDocument {
    pub id: u64,
    pub path: String,
}

/// D1: `sessions.get(event.sender.id)` of the slides session state
pub trait SlidesDocuments {
    fn document(&self, view: ViewId) -> Option<SlidesDocument>;
}

pub type OwnedRecentMove = Box<dyn FnOnce(String, String, Box<dyn FnOnce(Result<(), String>) + Send>)>;

pub trait SlidesRecentFiles {
    fn prepare_moved_owned(
        &self, origin: &crate::ipc::NativeRequestOwner, destination: &crate::ipc::NativeRequestOwner,
        cancel: &crate::runtime::CancelToken, pending: &crate::runtime::CancelToken,
        effects: &crate::hosts::OwnedEffectsHost,
    ) -> Result<OwnedRecentMove, String>;
    fn prepare_moved_many_owned(
        &self, origin: &crate::ipc::NativeRequestOwner, destination: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken, pending: &crate::runtime::CancelToken,
        effects: &crate::hosts::OwnedEffectsHost,
    ) -> Result<OwnedRecentMove, String>;
    fn prepare_moved_many_owned_extra(
        &self, origin: &crate::ipc::NativeRequestOwner, destination: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner], cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken, additional: &[crate::runtime::CancelToken],
        effects: &crate::hosts::OwnedEffectsHost,
    ) -> Result<OwnedRecentMove, String>;
    fn prepare_moved_recovery(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, snapshot: &crate::ipc::RecoveryAuthoritySnapshot, receiver: &crate::ipc::NativeRequestOwner,
        destination: &crate::ipc::NativeRequestOwner, participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken, pending: &crate::runtime::CancelToken,
        effects: &crate::hosts::OwnedEffectsHost,
    ) -> Result<OwnedRecentMove, String>;
    fn prepare_moved_existing_document(
        &self, binding: &crate::ipc::NativeExistingDocumentBinding, participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken, pending: &crate::runtime::CancelToken, additional: &[crate::runtime::CancelToken], effects: &crate::hosts::OwnedEffectsHost,
    ) -> Result<OwnedRecentMove, String>;
}


/// Captured on core, invoked with immutable paths; completion runs on the IO worker.
pub type OwnedTemplateMove = Box<dyn FnOnce(String,String,Box<dyn FnOnce(Result<(),String>) + Send>)>;
/// A: the hooks the slides save and rename paths call (template-selection-ipc);
/// when none is provided, the callers do nothing
pub trait SlidesTemplateSelectionHooks {
    /// `followTemplateSelection(session, fromPath, toPath)`
    fn follow(&self, doc: &SlidesDocument, from_path: &str, to_path: &str);
    /// `inheritTemplateSelection(previous, replacement)`
    fn inherit(&self, previous: Option<&SlidesDocument>, replacement: &SlidesDocument);
    /// `moveSlidesTemplateSelection(oldPath, newPath)`: the shell's
    /// after-file-moved step for a .pptx (W's rename path)
    fn moved(&self, old_path: &str, new_path: &str);
    fn prepare_moved_owned(
        &self,origin:&crate::ipc::NativeRequestOwner,destination:&crate::ipc::NativeRequestOwner,
        cancel:&crate::runtime::CancelToken,pending:&crate::runtime::CancelToken,
        effects:&crate::hosts::OwnedEffectsHost,
    )->Result<OwnedTemplateMove,String>;
    fn prepare_moved_many_owned(
        &self,origin:&crate::ipc::NativeRequestOwner,destination:&crate::ipc::NativeRequestOwner,
        participants:&[crate::ipc::NativeRequestOwner],
        cancel:&crate::runtime::CancelToken,pending:&crate::runtime::CancelToken,
        effects:&crate::hosts::OwnedEffectsHost,
    )->Result<OwnedTemplateMove,String>;
    fn prepare_moved_many_owned_extra(
        &self, origin: &crate::ipc::NativeRequestOwner, destination: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner], cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken, additional: &[crate::runtime::CancelToken],
        effects: &crate::hosts::OwnedEffectsHost,
    ) -> Result<OwnedTemplateMove, String>;
    fn prepare_moved_recovery(
        &self, receipt: &crate::ipc::RecoverySeedReceipt, snapshot: &crate::ipc::RecoveryAuthoritySnapshot, receiver: &crate::ipc::NativeRequestOwner,
        destination: &crate::ipc::NativeRequestOwner, participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken, pending: &crate::runtime::CancelToken,
        effects: &crate::hosts::OwnedEffectsHost,
    ) -> Result<OwnedTemplateMove, String>;
    fn prepare_moved_existing_document(
        &self, binding: &crate::ipc::NativeExistingDocumentBinding, participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken, pending: &crate::runtime::CancelToken, additional: &[crate::runtime::CancelToken], effects: &crate::hosts::OwnedEffectsHost,
    ) -> Result<OwnedTemplateMove, String>;
}
