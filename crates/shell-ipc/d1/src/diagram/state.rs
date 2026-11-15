//! The per-view state of the diagram editor: the module-level `Map`s and
//! `Set`s of `diagram-main.ts`, `recovery-main.ts`, `mcp-queries.ts` and
//! `headless-main.ts`, keyed by the view (`webContents.id`). Several maps of
//! one view are fields of one [`ViewState`]; a waiting promise is a stored
//! continuation.

use std::collections::{HashMap, HashSet, VecDeque};

use main_core::packages::electron_utils::recovery_store::RecoveryIdentity;
use main_core::packages::electron_utils::save_conflict::SaveDiskState;
use shell_contract::{IpcValue, ViewId};

use super::formats::DiagramFormat;

/// `unopenedByWc`: the view's file is not seen open yet, or did not open
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unopened {
    Pending,
    Failed,
}

/// `importByWc`: a view made from another program's file
#[derive(Clone, Debug, PartialEq)]
pub struct Import {
    pub path: String,
    pub title: String,
}

/// A save target handed out by `save-target`, used once
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub path: String,
    pub format: DiagramFormat,
}

/// How a wait for the renderer ended
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitEnd {
    /// the renderer's `ok` (or the view's end: false)
    Answer(bool),
    /// nothing came in time
    TimedOut,
}

impl WaitEnd {
    pub fn ok(self) -> bool {
        self == WaitEnd::Answer(true)
    }
}

/// A `Promise<boolean>` resolver of `closeSaveWaiters`/`saveWaiters`, with the
/// generation its timeout checks
pub struct Waiter {
    pub generation: u64,
    pub resume: Box<dyn FnOnce(WaitEnd)>,
}

/// `copyNowWaiters`
pub struct CopyNow {
    pub generation: u64,
    pub finish: Box<dyn FnOnce(Result<(), main_core::shell::renderer_disposal::Failure>)>,
}

#[derive(Default)]
pub struct ViewState {
    /// `contentsByWc.has`: `open_view` ran for this view
    pub tracked: bool,
    /// `openPathByWc`: queued at tab creation, consumed after mount
    pub open_path: Option<String>,
    /// `allowedByWc`: file paths granted to the view
    pub allowed: Option<HashSet<String>>,
    /// `savePathByWc`
    pub save_path: Option<String>,
    pub save_revision: Option<u64>,
    pub unopened: Option<Unopened>,
    pub import: Option<Import>,
    /// `dirtyByWc.has`
    pub dirty: bool,
    pub close_save: Option<Waiter>,
    pub save: Option<Waiter>,
    pub copy_now: Option<CopyNow>,
    /// `targetsByWc`, by token
    pub targets: HashMap<String, Target>,
    /// `cleanPathByWc`
    pub clean_path: Option<String>,
    /// `seedsByWc`: the draw.io XML of the content queued for a new view
    pub seed: Option<String>,
    /// `docDiskStates`
    pub disk_states: HashMap<String, SaveDiskState>,
    pub document: Option<shell_contract::runtime::RendererGeneration>,
    pub owned_saves: HashMap<String, super::owned::PendingSave>,
    pub owned_renames: HashMap<String, shell_contract::editors::Then<()>>,
    pub owned_closes: HashMap<String, super::owned::close::PendingClose>,
    pub owned_publications: HashMap<String, super::owned::PendingPublication>,
    pub first_save_running: bool,
    pub first_save_queue: VecDeque<(shell_contract::ipc::CallContext, crate::value::Args, shell_contract::ipc::Responder)>,
    pub recovery_preparing: Option<super::recovery_open::Preparing>,
    pub recovery_seed: Option<super::recovery_open::Seed>,
    pub recovery_applying: Option<super::recovery_open::Applying>,
    pub(super) saved_load: Option<super::saved_load::SavedLoad>,
    pub(super) saved_actor_identity: Option<std::rc::Rc<()>>,
    pub(super) browser_owner: Option<shell_contract::ipc::NativeBrowserOwner>,
}

impl ViewState {
    pub fn advance_save_revision(&mut self) {
        self.save_revision = self.save_revision.and_then(|revision| revision.checked_add(1));
    }

    /// `mayWrite`: the view's file did not fail to open
    pub fn may_write(&self) -> bool {
        self.unopened.is_none()
    }
}

/// `ViewRecovery` of `recovery-main.ts`
#[derive(Default)]
pub struct RecoveryView {
    /// the copy an untitled view writes
    pub untitled_copy: Option<RecoveryIdentity>,
    /// this view's copies, by copy path
    pub owned: HashMap<String, RecoveryIdentity>,
    /// the first Save As suggestion of a restored diagram whose file is gone
    pub save_as_default: Option<String>,
    /// the tab title a restored untitled copy came with
    pub title: Option<String>,
    /// what the shell already decided for the file this view opens next
    pub decision: Option<Decision>,
    /// the renderer crashed: its copies outlive the view
    pub crashed: bool,
    /// "Don't Save" dropped the copies: none is taken until the hold ends
    /// (the generation of the timer that ends it)
    pub dropped: Option<u64>,
    /// the copy left for the file could not be settled
    pub blocked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Restore,
    Original,
}

/// How a one-page read settles: the table text, and the values when the
/// renderer sent a one-page table (`DiagramPageRead`)
#[derive(Clone, Debug, PartialEq)]
pub struct PageRead {
    pub text: String,
    pub pages: Option<IpcValue>,
    pub page: Option<IpcValue>,
}

pub type ReadDone = Box<dyn FnOnce(Result<PageRead, String>)>;
pub type ApplyDone = Box<dyn FnOnce(Result<IpcValue, String>)>;

/// A question to the renderer waiting for its answer on `read-result`
pub struct ReadWaiter {
    pub view: ViewId,
    pub done: ReadDone,
}

pub struct ApplyWaiter {
    pub view: ViewId,
    pub done: ApplyDone,
}

/// One `diagramApplyOps` call in its view's chain
pub struct ApplyJob {
    pub ops: IpcValue,
    pub done: ApplyDone,
}

/// `running` of `headless-main.ts`
pub struct HeadlessJob {
    pub generation: u64,
    pub renderer: Option<shell_contract::runtime::RendererGeneration>,
    pub job: IpcValue,
    pub authorize: Box<dyn Fn(&shell_contract::ipc::CallContext) -> bool>,
    pub done: Box<dyn FnOnce(IpcValue)>,
}

#[derive(Default)]
pub struct State {
    pub views: HashMap<ViewId, ViewState>,
    pub recovery: HashMap<ViewId, RecoveryView>,
    /// `readWaiters` by question id
    pub reads: HashMap<String, ReadWaiter>,
    /// `applyWaiters` by question id
    pub applies: HashMap<String, ApplyWaiter>,
    /// `applyChains`: applies queued per view; the first one runs
    pub apply_chains: HashMap<ViewId, VecDeque<ApplyJob>>,
    pub headless: HashMap<ViewId, HeadlessJob>,
    /// source of the generations that timers check
    pub generation: u64,
    /// `pastedImageSeq`
    pub pasted_seq: u32,
    /// `pastedDirPruned`
    pub pasted_pruned: bool,
}

impl State {
    pub fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    pub fn view(&mut self, view: ViewId) -> &mut ViewState {
        self.views.entry(view).or_insert_with(|| ViewState { save_revision: Some(0), ..ViewState::default() })
    }

    /// `stateOf(wcId)` of `recovery-main.ts`
    pub fn recovery_of(&mut self, view: ViewId) -> &mut RecoveryView {
        self.recovery.entry(view).or_default()
    }
}
