//! Native Drive cache refresh lineage for an existing-document save.
use std::path::Path;
use std::sync::Arc;
use super::{NativeRequestOwner, SavedPublication};
use crate::runtime::CancelToken;

/// A preflight snapshot of the Drive service's exact committed state.
/// Admission locks only Drive state; the caller supplies the already-prepared
/// native ownership admission as `effect`.
pub trait NativeDriveReopenSnapshot {
    fn participants(&self) -> &[NativeRequestOwner];
    fn controls(&self) -> &[CancelToken];
    /// Prepare loss observers for an accepted exact PDF predecessor retirement.
    /// Pending and additional cancellation roles remain literal even when they
    /// alias the dispatcher-selected primary cancellation token.
    fn accepted_reload_controls(&self, accepted: &crate::hosts::AcceptedPdfReload) -> Result<Vec<CancelToken>,String>;
    fn with_committed_state(
        &self, moved: bool, source_commit: &file_commit::FileCommitReceipt,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
}

/// The Drive service supplies the only production implementation. Each effect
/// is admitted against the original selected destination and the exact captured
/// participants and lifetimes; callers cannot substitute a path.
pub trait NativeDriveMoveReceipt: std::any::Any + Send + Sync {
    fn publication(&self) -> &SavedPublication;
    fn moved_path(&self) -> &Path;
    fn fallback_path(&self) -> &Path;
    fn preflight_reopen(&self) -> Result<Box<dyn NativeDriveReopenSnapshot>, String>;
    fn read_committed_reopen(&self, moved: bool, max_bytes: usize, controls: &[CancelToken])
        -> std::io::Result<file_commit::CommittedRead>;
    fn with_moved_effect(&self, participants:&[NativeRequestOwner], additional:&[&CancelToken], effect:&mut dyn FnMut(&Path)->Result<(),String>) -> Result<(),String>;
    fn with_fallback_effect(&self, participants:&[NativeRequestOwner], additional:&[&CancelToken], effect:&mut dyn FnMut(&Path)->Result<(),String>) -> Result<(),String>;
    /// A committed move remains committed even when durability or cleanup fails.
    fn cleanup_warning(&self) -> Option<String>;
    fn retry_cleanup(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>);
}

/// Exact, still-owned inverse plan after a partial transaction. Retrying never
/// widens the original destination or exposes its staged paths to callers.
pub trait NativeDrivePendingReceipt: Send + Sync {
    fn publication(&self) -> &SavedPublication;
    fn original_error(&self) -> &str;
    fn cleanup_error(&self) -> Option<String>;
    fn retry(&self, done: Box<dyn FnOnce(NativeDrivePendingOutcome) + Send>);
}

/// A failed inverse retains the same owned transaction for another retry.
pub enum NativeDrivePendingOutcome {
    Restored { original_error: String },
    StillPending { receipt: Arc<dyn NativeDrivePendingReceipt>, error: String },
}

/// No Drive cache relocation, or a service-authenticated committed relocation.
pub enum NativeDriveReopen {
    Unchanged(SavedPublication),
    Moved(Arc<dyn NativeDriveMoveReceipt>),
    CleanupPending(Arc<dyn NativeDrivePendingReceipt>),
}
