//! One immutable activation batch, retaining every captured participant between steps.
use shell_contract::{ipc::NativeRequestOwner, runtime::CancelToken};
use std::sync::Arc;
pub(super) mod driver;

#[derive(Clone)]
pub enum TabActivationAuthority {
    Interactive(NativeRequestOwner),
    NativeInvocation {
        document: shell_contract::ipc::NativeInvocationDocument,
        owner: NativeRequestOwner,
    },
    NativeOperation {
        operation: shell_contract::ipc::NativeInvocationOperation,
        owner: NativeRequestOwner,
    },
    StartupRecovery {
        startup: shell_contract::ipc::NativeInvocationOperation,
        shell: NativeRequestOwner,
        child: shell_contract::ipc::NativeInvocationDocument,
        owner: NativeRequestOwner,
    },
    NativeCommand {
        operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        owner: NativeRequestOwner,
    },
    NativeCommandTarget {
        document: shell_contract::ipc::NativeInvocationDocument,
        expected: shell_contract::ipc::NativeEditorCommand,
        owner: NativeRequestOwner,
    },
    StartupCommand {
        startup: shell_contract::ipc::NativeInvocationOperation,
        shell: NativeRequestOwner,
        child: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        owner: NativeRequestOwner,
    },
    StartupCommandTarget {
        startup: shell_contract::ipc::NativeInvocationOperation,
        shell: NativeRequestOwner,
        document: shell_contract::ipc::NativeInvocationDocument,
        expected: shell_contract::ipc::NativeEditorCommand,
        owner: NativeRequestOwner,
    },
    CommandRecovery {
        command: shell_contract::ipc::NativeInvocationOperation,
        child: shell_contract::ipc::NativeInvocationDocument,
        owner: NativeRequestOwner,
    },
    ExistingDocumentSave {
        path: shell_contract::ipc::NativeSavedPath,
        owner: NativeRequestOwner,
        additional: Vec<CancelToken>,
    },
    SavedDocument {
        document: shell_contract::ipc::NativeSavedDocument,
        owner: NativeRequestOwner,
        additional: Vec<CancelToken>,
    },
    NativeSaveReload {
        receipt: Arc<shell_contract::hosts::NativeSaveReloadReceipt>,
        owner: NativeRequestOwner,
    },
}

impl TabActivationAuthority {
    pub fn owner(&self) -> &NativeRequestOwner {
        match self {
            Self::Interactive(owner)
            | Self::NativeInvocation { owner, .. }
            | Self::NativeOperation { owner, .. }
            | Self::StartupRecovery { owner, .. }
            | Self::NativeCommand { owner, .. }
            | Self::NativeCommandTarget { owner, .. }
            | Self::StartupCommand { owner, .. }
            | Self::StartupCommandTarget { owner, .. }
            | Self::CommandRecovery { owner, .. }
            | Self::ExistingDocumentSave { owner, .. }
            | Self::SavedDocument { owner, .. }
            | Self::NativeSaveReload { owner, .. } => owner,
        }
    }
}

#[derive(Clone)]
pub struct StartupRecoveryConstraint {
    pub startup: shell_contract::ipc::NativeInvocationOperation,
    pub shell: NativeRequestOwner,
    pub participants: Vec<NativeRequestOwner>,
    pub cancel: CancelToken,
    pub pending: CancelToken,
}
use shell_contract::ipc::PlacementSuperseded;
pub enum TabUiStepOutcome {
    Applied,
    AppliedWithClosePermit(shell_contract::ipc::NativeBrowserClosePermit),
    Superseded(PlacementSuperseded),
    BrowserSuperseded(shell_contract::ipc::NativeBrowserSuperseded),
    CommittedCleanupPending {
        error: String,
        retry: TabUiStepRetry,
    },
}

pub type TabUiStepDone = Box<dyn FnOnce(Result<TabUiStepOutcome, String>)>;
pub type TabUiStepRetry = std::rc::Rc<dyn Fn(TabUiStepDone)>;

pub type TabUiDone = Box<dyn FnOnce(Result<(), String>)>;
pub type TabUiRun = Box<
    dyn FnOnce(
        TabActivationAuthority,
        Arc<[NativeRequestOwner]>,
        CancelToken,
        CancelToken,
        Option<shell_contract::ipc::NativeBrowserClosePermit>,
        TabUiStepDone,
    ),
>;

/// Constructed by the actual native adapter (or an explicit behavioral test host).
/// Its runner captures native handles before posting, admits the full participant
/// set, drops every gate, executes once on UI and returns its actual outcome on core.
pub struct PreparedTabUiStep(TabUiRun);

impl PreparedTabUiStep {
    pub fn new(run: TabUiRun) -> Self {
        Self(run)
    }
    pub fn run(
        self,
        authority: TabActivationAuthority,
        participants: Arc<[NativeRequestOwner]>,
        cancel: CancelToken,
        pending: CancelToken,
        close: Option<shell_contract::ipc::NativeBrowserClosePermit>,
        done: TabUiStepDone,
    ) {
        (self.0)(authority, participants, cancel, pending, close, done)
    }
}

/// One slot per immutable TabActivationEffect. Native slots contain an actual
/// prepared UI operation; core-only state/timer/publication slots contain None.
/// The shell renderer is itself a captured participant, not a late label lookup.
pub struct PreparedTabUiBatch {
    pub shell: NativeRequestOwner,
    pub participants: Vec<NativeRequestOwner>,
    pub steps: Vec<Option<PreparedTabUiStep>>,
}

pub type RecoveryCleanup = Arc<
    dyn Fn(Box<dyn FnOnce(Result<shell_contract::ipc::NativeBrowserRetirement, String>) + Send>)
        + Send
        + Sync,
>;

/// The exact in-flight recovery attempt, retained from the sealed opener entry.
pub trait RecoveryAttempt {
    fn rollback_store(&self) -> super::host::EditorSeedRollbackStore;
    fn bind_target(&self, target: &shell_contract::ipc::RecoveryOpenTarget) -> Result<(), String>;
    /// Keep an unbound producer result without running its cleanup.
    fn retain_unbound_cleanup(&self, cleanup: Box<dyn FnOnce()>);
    /// Retain the original failure until native retirement is acknowledged.
    /// Invoke completion before releasing the drain.
    fn fail(
        &self,
        error: String,
        cleanup: Option<RecoveryCleanup>,
        on_cleanup_error: std::rc::Rc<dyn Fn(String)>,
        done: TabUiDone,
    );
    /// Only before the native constructor or actor preparation was admitted.
    fn fail_before_creation(&self, error: String, done: TabUiDone);
    /// Complete only after the original retirement and its receipt rollback settle.
    fn fail_after_retirement(
        &self,
        error: String,
        retirement: shell_contract::ipc::NativeBrowserRetirement,
        on_cleanup_error: std::rc::Rc<dyn Fn(String)>,
        done: Box<dyn FnOnce()>,
    );
    fn retry_cleanup(&self);
    /// Transfer the already adopted document, not an activation-success claim.
    fn adopted(&self) -> Result<(), String>;
}

pub struct OwnedLoadedTabView {
    pub loaded: main_core::shell::tab_manager::LoadedTabView<shell_contract::ViewId>,
    pub owner: NativeRequestOwner,
    pub document: shell_contract::ipc::NativeInvocationDocument,
    pub discard: RecoveryCleanup,
    pub closure: super::host::ViewClosureBinding,
    pub attempt: std::rc::Rc<dyn RecoveryAttempt>,
}

pub struct TabActivationPublicationSnapshot {
    pub changed: super::host::TabsChanged,
    pub tab_views: Vec<(String, shell_contract::ViewId)>,
    pub split: Option<main_core::shell::tab_manager::SplitViewState>,
    pub settings_path: String,
    pub session_strip: serde_json::Value,
    pub session_detached: Vec<serde_json::Value>,
    pub split_memory_revision: SplitMemoryRevision,
}

impl TabActivationPublicationSnapshot {
    pub fn changed_args(&self) -> Vec<shell_contract::IpcValue> {
        vec![shell_contract::IpcValue::Array(
            self.changed
                .tabs
                .iter()
                .map(|tab| super::wire::to_ipc(&tab.to_value()))
                .collect(),
        )]
    }
    pub fn split_args(&self) -> Vec<shell_contract::IpcValue> {
        vec![
            self.split
                .as_ref()
                .map_or(shell_contract::IpcValue::Null, |split| {
                    super::wire::to_ipc(&split.to_value())
                }),
        ]
    }
}

pub struct PreparedTabActivationPublication {
    pub changed: Box<dyn FnOnce(TabUiStepDone)>,
    pub split: Box<dyn FnOnce(bool, TabUiStepDone)>,
    pub persist_split: Box<dyn FnOnce(TabUiStepDone)>,
}

pub(super) struct SplitMemoryClock(Arc<std::sync::Mutex<Option<u64>>>);

impl SplitMemoryClock {
    pub(super) fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(Some(0))))
    }
    pub(super) fn advance(&self) -> Option<SplitMemoryRevision> {
        let mut current = self.0.lock().ok()?;
        let next = (*current).and_then(|current| current.checked_add(1));
        *current = next;
        Some(SplitMemoryRevision {
            clock: self.0.clone(),
            expected: next?,
        })
    }
    pub(super) fn capture(&self) -> Option<SplitMemoryRevision> {
        Some(SplitMemoryRevision {
            clock: self.0.clone(),
            expected: (*self.0.lock().ok()?)?,
        })
    }

    pub(super) fn reserve_advance(&self) -> Option<ReservedSplitMemoryRevision> {
        let previous = (*self.0.lock().ok()?)?;
        let expected = previous.checked_add(1)?;
        Some(ReservedSplitMemoryRevision {
            previous,
            revision: SplitMemoryRevision {
                clock: self.0.clone(),
                expected,
            },
        })
    }
}

/// Latest split-state ordering, not document authority. Native adapters first
/// admit all captured owners, then use this only for the final prepared rename.
#[derive(Clone)]
pub struct SplitMemoryRevision {
    clock: Arc<std::sync::Mutex<Option<u64>>>,
    expected: u64,
}

impl SplitMemoryRevision {
    pub fn with_current_commit<R>(&self, rename: impl FnOnce() -> R) -> Option<R> {
        let current = self.clock.lock().ok()?;
        if *current != Some(self.expected) {
            return None;
        }
        Some(rename())
    }
}

pub(super) struct ReservedSplitMemoryRevision {
    previous: u64,
    revision: SplitMemoryRevision,
}

impl ReservedSplitMemoryRevision {
    pub(super) fn revision(&self) -> SplitMemoryRevision {
        self.revision.clone()
    }

    pub(super) fn commit<R>(
        &self,
        change: impl FnOnce() -> Result<R, String>,
    ) -> Result<R, String> {
        let mut current = self
            .revision
            .clock
            .lock()
            .map_err(|_| "The split memory revision is unavailable")?;
        if *current != Some(self.previous) {
            return Err("The split state changed before removal was committed".into());
        }
        let result = change()?;
        *current = Some(self.revision.expected);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::SplitMemoryClock;
    use std::cell::Cell;

    #[test]
    fn newer_split_rejects_an_older_prepared_final_rename() {
        let clock = SplitMemoryClock::new();
        let old = clock.advance().unwrap();
        let current = clock.advance().unwrap();
        let writes = Cell::new(0);
        assert!(
            old.with_current_commit(|| writes.set(writes.get() + 1))
                .is_none()
        );
        assert!(
            current
                .with_current_commit(|| writes.set(writes.get() + 1))
                .is_some()
        );
        assert_eq!(writes.get(), 1);
    }

    #[test]
    fn reserved_split_revision_does_not_mutate_on_stale_or_failed_reducer() {
        let clock = SplitMemoryClock::new();
        let reserved = clock.reserve_advance().unwrap();
        let before = clock.capture().unwrap();
        assert!(reserved.revision().with_current_commit(|| ()).is_none());
        assert!(
            reserved
                .commit(|| Err::<(), _>("reducer refused".into()))
                .is_err()
        );
        assert!(before.with_current_commit(|| ()).is_some());
        let newer = clock.advance().unwrap();
        let called = Cell::new(false);
        assert!(
            reserved
                .commit(|| {
                    called.set(true);
                    Ok(())
                })
                .is_err()
        );
        assert!(!called.get());
        assert!(newer.with_current_commit(|| ()).is_some());
    }
}

/// Retains the exact captured actor and native browser between failed attempts.
pub trait DocsDisposalOperation {
    fn run(self: std::rc::Rc<Self>, done: Box<dyn FnOnce(Result<DocsDisposal, String>)>);
}
/// Native producer acknowledgement of the exact captured document's completed
/// disposal. It retains identity only; it cannot authorize a different operation.

pub struct DocsDisposal {
    owner: NativeRequestOwner,
    generation: shell_contract::runtime::RendererGeneration,
}

impl DocsDisposal {
    pub fn closed(owner: NativeRequestOwner) -> Result<Self, String> {
        let generation = owner
            .context
            .renderer()
            .cloned()
            .ok_or("The disposed document had no captured renderer.")?;
        Ok(Self { owner, generation })
    }
    pub fn view(&self) -> shell_contract::ViewId {
        self.owner.view
    }
    pub fn generation(&self) -> &shell_contract::runtime::RendererGeneration {
        &self.generation
    }
}
