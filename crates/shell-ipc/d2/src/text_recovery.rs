//! The recovery of the text editors, HTML and Markdown
//! (`apps/html/src/main/recovery.ts`, `apps/markdown/src/main/recovery.ts`:
//! one module the two apps keep in step), shared by [`crate::html`] and
//! [`crate::markdown`]. [`ui`] holds the adapters from the shared hosts to
//! the `main-core` dialog traits that both editors (and this module) call.
//!
//! The document text lives in the renderer. While it has unsaved changes the
//! renderer sends the exact text a save would write; this module keeps it as
//! the document's copy in `<userData>/<kind>-autosave` through the shared
//! store. See the TypeScript's header for the rules this keeps.
//!
//! What differs from the TypeScript:
//! - The store's process-wide state (the shutting-down latch, delete epochs,
//!   the "Don't Save" block) is the TypeScript's `Symbol.for` global. Here it
//!   is the one [`SharedRecoveryHost`] the shell keeps in its hosts, shared
//!   with every editor and the shell's own recovery code.
//! - `writeCopiesNow` resolves a promise; here it calls `done` (the core
//!   thread never waits for a renderer).
//! - The crash (`render-process-gone`) is reported by the shell through
//!   [`TextRecovery::view_crashed`]; the editor calls [`TextRecovery::forget`]
//!   from its own destroyed hook, before it drops the view's dirty flag.

mod owned;
mod rename_owned;
pub(crate) use owned::RecoverySeedSpec;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jscompat::node_path::host::basename;
use main_core::packages::electron_utils::recovery_prompt::{
    RecoveryOpenResult, ResolveRecoveryOnOpenOptions, resolve_recovery_on_open,
};
use main_core::packages::electron_utils::recovery_store::{
    RecoveryDeleteReason, RecoveryFileIdentity, RecoveryIdentity, RecoveryStore,
    RecoveryStoreOptions, RecoveryUntitledIdentity, RecoveryWriteOptions, RecoveryWriteResult,
    SharedRecoveryState, create_recovery_store, new_untitled_recovery_id,
};
use main_core::shell::renderer_disposal::Failure;
use shell_contract::hosts::{ContentsHost, DialogsHost};
use shell_contract::ipc::Target;
use shell_contract::runtime::{PathName, Services};
use shell_contract::{IpcValue, ViewId};

use crate::kit::{self, Entry, IntoIpc, obj};
/// how long `write_copies_now` waits for one renderer to send its copy
pub(crate) const RECOVERY_FLUSH_TIMEOUT: Duration = Duration::from_millis(3_000);

/// The store's process-wide state, one per process in the shell's hosts
pub(crate) type SharedRecoveryHost = Arc<Mutex<SharedRecoveryState>>;

/// The hosts every editor test needs besides its fakes: the shell inserts
/// the shared recovery state, not the fakes
#[cfg(test)]
pub(crate) fn test_hosts(hosts: &mut shell_contract::Hosts) {
    hosts.insert::<SharedRecoveryHost>(Arc::new(Mutex::new(SharedRecoveryState::new("test"))));
}

/// a host call about one view's file
pub(crate) type PathHook = Box<dyn Fn(ViewId, &str)>;

/// What the editor tells the recovery about its views (`TextRecoveryHost`).
/// The closures read the editor's own per-view state; they must not call back
/// into the recovery.
pub(crate) struct TextRecoveryHost {
    pub kind: &'static str,
    /// the format an untitled document's copy is written in ('.md', '.html')
    pub untitled_ext: &'static str,
    /// main → renderer channel: "send your copy now"
    pub flush_channel: &'static str,
    pub lang: Box<dyn Fn() -> String>,
    /// `isHeadlessMode()`: a hidden export window never asks and never restores
    pub is_headless: Box<dyn Fn() -> bool>,
    /// the document's save target; None while untitled
    pub save_path: Box<dyn Fn(ViewId) -> Option<String>>,
    /// the file the view opens (its pending open); None for an untitled view
    pub open_path: Box<dyn Fn(ViewId) -> Option<String>>,
    /// the main side's unsaved-changes flag for the view
    pub is_dirty: Box<dyn Fn(ViewId) -> bool>,
    /// the restored text replaces the file's: record the disk state the save guard compares with
    pub restored_file: PathHook,
    /// last-resort title of an untitled document's copy
    pub untitled_fallback_title: Box<dyn Fn() -> String>,
    /// `Date.now()`, the time a copy records
    pub now_ms: Box<dyn Fn() -> f64>,
}

/// what a copy write answered (`TextRecoveryWriteResult`)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteResult {
    Written,
    Superseded,
    Unchanged,
    Skipped,
}

impl WriteResult {
    fn as_str(self) -> &'static str {
        match self {
            WriteResult::Written => "written",
            WriteResult::Superseded => "superseded",
            WriteResult::Unchanged => "unchanged",
            WriteResult::Skipped => "skipped",
        }
    }
}

/// a restored copy's text (`RecoveredText`)
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RecoveredText {
    pub text: String,
    pub original_path: Option<String>,
}

impl IntoIpc for RecoveredText {
    fn into_ipc(self) -> IpcValue {
        match self.original_path {
            Some(path) => obj([
                ("text", self.text.into_ipc()),
                ("originalPath", path.into_ipc()),
            ]),
            None => obj([("text", self.text.into_ipc())]),
        }
    }
}

struct OwnedOpenSeed {
    receipt: Rc<RefCell<Option<shell_contract::ipc::RecoverySeedReceipt>>>,
    token: String,
    lease: shell_contract::runtime::RendererGeneration,
    loss_subscription: Option<shell_contract::runtime::CancelSubscription>,
    recovered: Option<Option<RecoveredText>>,
}
type Waiter = Box<dyn FnOnce(Result<(), Failure>)>;

#[derive(Default)]
struct ViewState {
    /// the copy identity while the document has no save path
    untitled: Option<RecoveryIdentity>,
    /// the text of this tab's last copy; None when there is none to compare with
    last_text: Option<String>,
    /// this tab wrote (or loaded) the copy of its current identity
    owns: bool,
    /// the renderer process died: the copy outlives the view
    crashed: bool,
    /// the per-file prompt is open: no copy is written until it is answered
    resolving: bool,
    owned_open: Option<OwnedOpenSeed>,
    /// the title the shell gave an untitled copy, or the editor's provisional title
    title: Option<String>,
    /// first Save As suggestion of a restored document whose original is missing
    save_as_default_path: Option<String>,
    /// a copy left under the path the file had before a rename
    renamed_from: Option<RecoveryIdentity>,
    /// the identity this tab's last copy was written under
    written_as: Option<RecoveryIdentity>,
    flush_waiters: HashMap<String, Waiter>,
}

impl ViewState {
    fn drop_copy(&mut self) {
        self.last_text = None;
        self.owns = false;
        self.written_as = None;
    }
}

/// the shell's title of an untitled view's tab
pub(crate) type TitleSource = Box<dyn Fn(ViewId) -> Option<String>>;

/// `createTextRecovery(host)`
pub(crate) struct TextRecovery {
    host: TextRecoveryHost,
    services: Services,
    dialogs: DialogsHost,
    contents: ContentsHost,
    shared: SharedRecoveryHost,
    views: RefCell<HashMap<ViewId, ViewState>>,
    owned_renames: RefCell<HashMap<ViewId, rename_owned::OwnedRename>>,
    title_source: RefCell<Option<TitleSource>>,
}

fn file_identity(kind: &str, file_path: &str) -> RecoveryIdentity {
    RecoveryIdentity::File(RecoveryFileIdentity {
        kind: kind.to_owned(),
        file_path: file_path.to_owned(),
    })
}

/// The hosts the recovery takes, read once when the editor registers
pub(crate) struct RecoveryHosts {
    pub dialogs: DialogsHost,
    pub contents: ContentsHost,
    pub shared: SharedRecoveryHost,
}

impl RecoveryHosts {
    pub(crate) fn need(services: &Services) -> Result<Self, shell_contract::ipc::RegisterError> {
        let by = shell_contract::Batch::D2;
        Ok(RecoveryHosts {
            dialogs: services.hosts.need::<DialogsHost>(by)?,
            contents: services.hosts.need::<ContentsHost>(by)?,
            shared: services.hosts.need::<SharedRecoveryHost>(by)?,
        })
    }
}

impl TextRecovery {
    pub(crate) fn new(
        services: &Services,
        host: TextRecoveryHost,
        hosts: RecoveryHosts,
    ) -> Rc<Self> {
        Rc::new(TextRecovery {
            dialogs: hosts.dialogs,
            contents: hosts.contents,
            shared: hosts.shared,
            services: services.clone(),
            host,
            views: RefCell::default(),
            owned_renames: RefCell::default(),
            title_source: RefCell::default(),
        })
    }

    fn with_store<R>(&self, f: impl FnOnce(&RecoveryStore<'_>) -> R) -> R {
        let user_data = self.services.paths.path(PathName::UserData);
        let user_data_dir = || user_data.to_string_lossy().into_owned();
        let now_ms = || (self.host.now_ms)();
        let random_uuid = jscompat::random_uuid;
        // the core thread is the only one that writes copies: the lock is never contended
        let shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let store = create_recovery_store(
            RecoveryStoreOptions {
                user_data_dir: &user_data_dir,
                now_ms: &now_ms,
                random_uuid: &random_uuid,
            },
            &shared,
        );
        f(&store)
    }

    /// recovery never breaks the editor: a store failure is logged and ignored
    fn quietly<E: std::fmt::Debug>(&self, what: &str, result: Result<impl Sized, E>) {
        if let Err(error) = result {
            eprintln!("[{}] recovery {what} failed: {error:?}", self.host.kind);
        }
    }

    fn is_destroyed(&self, view: ViewId) -> bool {
        self.services.views.sender_of(view).is_none()
    }

    fn identity_for(&self, view: ViewId, state: &mut ViewState) -> RecoveryIdentity {
        if let Some(file_path) = (self.host.save_path)(view).filter(|p| !p.is_empty()) {
            return file_identity(self.host.kind, &file_path);
        }
        state
            .untitled
            .get_or_insert_with(|| {
                RecoveryIdentity::Untitled(RecoveryUntitledIdentity {
                    kind: self.host.kind.to_owned(),
                    untitled_id: new_untitled_recovery_id(&jscompat::random_uuid),
                    ext: self.host.untitled_ext.to_owned(),
                })
            })
            .clone()
    }

    fn copy_title(&self, view: ViewId, state: &ViewState, identity: &RecoveryIdentity) -> String {
        if let RecoveryIdentity::File(f) = identity {
            return basename(&f.file_path, None);
        }
        let from_shell = self.title_source.borrow().as_ref().and_then(|s| s(view));
        from_shell
            .filter(|t| !t.is_empty())
            .or_else(|| state.title.clone().filter(|t| !t.is_empty()))
            .unwrap_or_else(|| (self.host.untitled_fallback_title)())
    }

    fn settle_flush(&self, view: ViewId, token: &str, result: Result<(), Failure>) {
        let waiter = self
            .views
            .borrow_mut()
            .get_mut(&view)
            .and_then(|state| state.flush_waiters.remove(token));
        if let Some(waiter) = waiter {
            waiter(result);
        }
    }

    fn fail_flushes(&self, view: ViewId, message: &str) {
        let waiters = self
            .views
            .borrow_mut()
            .get_mut(&view)
            .map(|state| std::mem::take(&mut state.flush_waiters))
            .unwrap_or_default();
        for (_, waiter) in waiters {
            waiter(Err(Failure(message.to_owned())));
        }
    }

    /// the copy `identity` is (or was) this view's: its save or open path, or what it last wrote
    fn shows_copy(&self, view: ViewId, state: &ViewState, identity: &RecoveryIdentity) -> bool {
        if state.written_as.as_ref() == Some(identity) || state.untitled.as_ref() == Some(identity)
        {
            return true;
        }
        match identity {
            RecoveryIdentity::Untitled(_) => false,
            RecoveryIdentity::File(f) => {
                (self.host.save_path)(view).as_deref() == Some(f.file_path.as_str())
                    || (self.host.open_path)(view).as_deref() == Some(f.file_path.as_str())
            }
        }
    }

    /// Deletes the copy; `view`'s own state (when still tracked) and every
    /// pane showing the same copy forget their last copy
    fn delete_for(&self, view: ViewId, identity: &RecoveryIdentity, reason: RecoveryDeleteReason) {
        self.quietly(
            "delete",
            self.with_store(|store| store.delete_copy(identity, reason)),
        );
        let mut shared = false;
        for (id, other) in self.views.borrow_mut().iter_mut() {
            if *id == view {
                other.drop_copy();
                continue;
            }
            if !self.shows_copy(*id, other, identity) {
                continue;
            }
            other.drop_copy();
            shared = true;
        }
        // "Don't Save" blocks the copy until the close is cancelled; another
        // pane's edits were not thrown away, so its copies go on
        if reason == RecoveryDeleteReason::DontSave && shared {
            self.quietly(
                "resume",
                self.with_store(|store| store.clear_discarded(identity)),
            );
        }
    }

    /// another live view shows the same file and already settled its copy question
    fn sibling_owns_path(&self, view: ViewId, file_path: &str) -> bool {
        let others: Vec<(ViewId, bool)> = self
            .views
            .borrow()
            .iter()
            .filter(|(id, _)| **id != view)
            .map(|(id, s)| (*id, s.resolving))
            .collect();
        others.into_iter().any(|(id, resolving)| {
            !self.is_destroyed(id)
                && !resolving
                && ((self.host.save_path)(id).as_deref() == Some(file_path)
                    || (self.host.open_path)(id).as_deref() == Some(file_path))
        })
    }

    /// start tracking a view (once, when it is created)
    pub(crate) fn track(&self, view: ViewId) {
        self.views.borrow_mut().entry(view).or_default();
    }

    /// the renderer process died: its copy outlives the view
    pub(crate) fn view_crashed(&self, view: ViewId) {
        if let Some(state) = self.views.borrow_mut().get_mut(&view) {
            state.crashed = true;
        }
        self.fail_owned_rename(view, "recovery renderer crashed");
        self.fail_owned_open_after_loss(view, "recovery renderer crashed");
        self.fail_flushes(view, "recovery renderer crashed");
    }

    /// The view is being destroyed: delete the copy it wrote when it closes
    /// clean, keep it otherwise. Call before the editor drops the view's dirty flag.
    pub(crate) fn forget(&self, view: ViewId) {
        self.fail_owned_rename(view, "recovery view destroyed");
        self.fail_owned_open_after_loss(view, "recovery view destroyed");
        let Some(mut state) = self.views.borrow_mut().remove(&view) else {
            return;
        };
        for (_, resolve) in std::mem::take(&mut state.flush_waiters) {
            resolve(Err(Failure("recovery view destroyed".into())));
        }
        if state.crashed || (self.host.is_dirty)(view) || !state.owns {
            return;
        }
        let identity = self.identity_for(view, &mut state);
        self.delete_for(view, &identity, RecoveryDeleteReason::ClosedClean);
    }

    /// the renderer's copy (None: nothing to write, it only answers a flush request)
    #[cfg(test)]
    pub(crate) fn write_copy(&self, view: ViewId, text: Option<&str>) -> WriteResult {
        if !self.views.borrow().contains_key(&view) {
            return WriteResult::Skipped;
        }
        self.write_tracked(view, text)
            .unwrap_or(WriteResult::Skipped)
    }

    fn write_tracked(&self, view: ViewId, text: Option<&str>) -> Result<WriteResult, Failure> {
        let (identity, title) = {
            let mut views = self.views.borrow_mut();
            let Some(state) = views.get_mut(&view) else {
                return Ok(WriteResult::Skipped);
            };
            let Some(text) = text else {
                return Ok(WriteResult::Skipped);
            };
            if state.resolving || state.crashed || !(self.host.is_dirty)(view) {
                return Ok(WriteResult::Skipped);
            }
            if state.last_text.as_deref() == Some(text) {
                return Ok(WriteResult::Unchanged);
            }
            let identity = self.identity_for(view, state);
            let title = self.copy_title(view, state, &identity);
            (identity, title)
        };
        let text = text.unwrap_or_default();
        let written = self.with_store(|store| {
            store.write_copy(
                &identity,
                text.as_bytes(),
                &RecoveryWriteOptions {
                    title: Some(title),
                    ..RecoveryWriteOptions::default()
                },
            )
        });
        let written =
            written.map_err(|error| Failure(format!("recovery write failed: {error:?}")))?;
        if written == RecoveryWriteResult::Superseded {
            return Ok(WriteResult::Superseded);
        }
        let old = self.views.borrow_mut().get_mut(&view).and_then(|state| {
            state.last_text = Some(text.to_owned());
            state.owns = true;
            state.written_as = Some(identity.clone());
            state.renamed_from.take()
        });
        // the copy under the pre-rename path is replaced by this one
        if let Some(old) = old {
            self.quietly(
                "delete",
                self.with_store(|store| store.delete_copy(&old, RecoveryDeleteReason::ClosedClean)),
            );
        }
        Ok(WriteResult::Written)
    }

    /// the identity the view's copy has before a save; pass it to `saved` afterwards
    pub(crate) fn identity_of(&self, view: ViewId) -> Option<RecoveryIdentity> {
        let mut views = self.views.borrow_mut();
        let state = views.get_mut(&view)?;
        Some(self.identity_for(view, state))
    }

    /// a save succeeded: delete the copy of the identity the document had when the save began
    pub(crate) fn saved(&self, view: ViewId, before: Option<&RecoveryIdentity>) {
        if !self.views.borrow().contains_key(&view) {
            return;
        }
        match before {
            Some(before) => self.delete_for(view, before, RecoveryDeleteReason::Saved),
            None => {
                if let Some(state) = self.views.borrow_mut().get_mut(&view) {
                    state.last_text = None;
                    state.owns = false;
                }
            }
        }
        // the document has a path now; a restored missing original is saved
        if (self.host.save_path)(view).is_some_and(|p| !p.is_empty())
            && let Some(state) = self.views.borrow_mut().get_mut(&view)
        {
            state.untitled = None;
            state.save_as_default_path = None;
        }
    }

    /// "Don't Save" / a close without saving
    pub(crate) fn dont_save(&self, view: ViewId) {
        let Some(identity) = self.identity_of(view) else {
            return;
        };
        self.delete_for(view, &identity, RecoveryDeleteReason::DontSave);
    }

    /// a save to `file_path` given from outside (MCP) failed and the old target is back
    pub(crate) fn save_to_path_failed(&self, view: ViewId, file_path: &str) {
        let Some(own) = self.identity_of(view) else {
            return;
        };
        let target = file_identity(self.host.kind, file_path);
        // the failed save pointed at the document's own path: that copy is its own
        if own == target {
            return;
        }
        let wrote_target = self
            .views
            .borrow()
            .get(&view)
            .is_some_and(|s| s.written_as.as_ref() == Some(&target));
        if !wrote_target {
            return;
        }
        self.delete_for(view, &target, RecoveryDeleteReason::DontSave);
        // a cleanup, not the user's "Don't Save": that path takes copies as usual
        self.quietly(
            "resume",
            self.with_store(|store| store.clear_discarded(&target)),
        );
    }

    /// the file was renamed on disk while open
    pub(crate) fn renamed(&self, view: ViewId, old_path: &str) {
        {
            let mut views = self.views.borrow_mut();
            let Some(state) = views.get_mut(&view).filter(|s| s.owns) else {
                return;
            };
            // the next copy lands under the new path; the old one goes once it has
            state.renamed_from = Some(file_identity(self.host.kind, old_path));
            state.last_text = None;
            state.owns = false;
        }
        if !self.is_destroyed(view) {
            self.services
                .events
                .emit(Target::View(view), self.host.flush_channel, Vec::new());
        }
    }

    /// The renderer's open question: the text to load instead of the file, or
    /// None.
    pub(crate) fn resolve_open(&self, view: ViewId) -> Option<RecoveredText> {
        self.views.borrow_mut().entry(view).or_default();
        if (self.host.is_headless)() {
            return None;
        }
        let file_path = (self.host.open_path)(view).filter(|p| !p.is_empty())?;
        let identity = file_identity(self.host.kind, &file_path);
        if self.sibling_owns_path(view, &file_path) {
            return None;
        }
        self.set_resolving(view, true);
        let parent = self.contents.window_of(view).map(ui::Parent);
        let lang = (self.host.lang)();
        let boxes = ui::Boxes(self.dialogs.clone());
        let result = self.with_store(|store| {
            resolve_recovery_on_open(&ResolveRecoveryOnOpenOptions {
                store,
                identity: &identity,
                dialog: &boxes,
                parent: parent.as_ref(),
                lang: &lang,
                document_name: None,
            })
        });
        self.set_resolving(view, false);
        match result {
            Ok(RecoveryOpenResult::Restore { bytes, .. }) => {
                self.set_owns(view);
                (self.host.restored_file)(view, &file_path);
                Some(RecoveredText {
                    text: String::from_utf8_lossy(&bytes).into_owned(),
                    original_path: None,
                })
            }
            Ok(RecoveryOpenResult::Original) => None,
            Err(error) => {
                eprintln!("[{}] recovery open failed: {error:?}", self.host.kind);
                None
            }
        }
    }

    fn set_owns(&self, view: ViewId) {
        if let Some(state) = self.views.borrow_mut().get_mut(&view) {
            state.owns = true;
        }
    }

    fn set_resolving(&self, view: ViewId, on: bool) {
        if let Some(state) = self.views.borrow_mut().get_mut(&view) {
            state.resolving = on;
        }
    }

    /// the default Save As path for an untitled view, when recovery has one
    pub(crate) fn save_as_default_path(&self, view: ViewId) -> Option<String> {
        let views = self.views.borrow();
        let state = views.get(&view)?;
        if let Some(path) = state.save_as_default_path.clone().filter(|p| !p.is_empty()) {
            return Some(path);
        }
        // a copy of a missing original reopened without a suggestion: its old path
        match &state.untitled {
            Some(RecoveryIdentity::File(f)) => Some(f.file_path.clone()),
            _ => None,
        }
    }

    /// remember an untitled document's own title (the editor's provisional name)
    pub(crate) fn set_title(&self, view: ViewId, title: &str) {
        if let Some(state) = self.views.borrow_mut().get_mut(&view) {
            state.title = Some(title.to_owned());
        }
    }

    /// where the title of an untitled copy comes from first (the shell's tab title)
    pub(crate) fn set_title_source(&self, source: Option<TitleSource>) {
        *self.title_source.borrow_mut() = source;
    }

    /// Ask each dirty renderer for a fresh copy and wait for its matching reply.
    pub(crate) fn write_copies_now(
        self: &Rc<Self>,
        done: impl FnOnce(Result<(), Failure>) + 'static,
    ) {
        let targets: Vec<ViewId> = self
            .views
            .borrow()
            .iter()
            .filter(|(id, _)| (self.host.is_dirty)(**id))
            .map(|(id, _)| *id)
            .collect();
        if targets.is_empty() {
            done(Ok(()));
            return;
        }
        let pending = Rc::new(Cell::new(targets.len()));
        let first_error = Rc::new(RefCell::new(None::<Failure>));
        let done = Rc::new(RefCell::new(Some(Box::new(done) as Waiter)));
        for view in targets {
            if self.is_destroyed(view) || self.views.borrow().get(&view).is_some_and(|s| s.crashed)
            {
                if first_error.borrow().is_none() {
                    *first_error.borrow_mut() =
                        Some(Failure("recovery renderer unavailable".into()));
                }
                pending.set(pending.get() - 1);
                continue;
            }
            let token = kit::unique().to_string();
            let weak = Rc::downgrade(self);
            let timeout_token = token.clone();
            let timer = kit::after(&self.services, RECOVERY_FLUSH_TIMEOUT, move || {
                if let Some(recovery) = weak.upgrade() {
                    recovery.settle_flush(
                        view,
                        &timeout_token,
                        Err(Failure("recovery flush timed out".into())),
                    );
                }
            });
            let pending = Rc::clone(&pending);
            let first_error = Rc::clone(&first_error);
            let done = Rc::clone(&done);
            self.views
                .borrow_mut()
                .get_mut(&view)
                .expect("tracked view")
                .flush_waiters
                .insert(
                    token.clone(),
                    Box::new(move |result| {
                        kit::clear(timer);
                        if let Err(error) = result
                            && first_error.borrow().is_none()
                        {
                            *first_error.borrow_mut() = Some(error);
                        }
                        pending.set(pending.get() - 1);
                        if pending.get() == 0 {
                            let result = first_error.borrow_mut().take().map_or(Ok(()), Err);
                            let callback = done.borrow_mut().take();
                            if let Some(callback) = callback {
                                callback(result);
                            }
                        }
                    }),
                );
            self.services.events.emit(
                Target::View(view),
                self.host.flush_channel,
                vec![token.into_ipc()],
            );
        }
        if pending.get() == 0 {
            let result = first_error.borrow_mut().take().map_or(Ok(()), Err);
            let callback = done.borrow_mut().take();
            if let Some(callback) = callback {
                callback(result);
            }
        }
    }

    /// `contract.isUnprotectable`: every copy here is plain text the user could have saved
    pub(crate) fn is_unprotectable(&self, _view: ViewId) -> bool {
        false
    }

    /// `kind` of the contract the shell registers
    pub(crate) fn kind(&self) -> &'static str {
        self.host.kind
    }

    /// `<kind>:recovery-open` and `<kind>:recovery-write`
    pub(crate) fn entries(
        self: &Rc<Self>,
        open_channel: &'static str,
        write_channel: &'static str,
        applied_channel: &'static str,
    ) -> [Entry; 3] {
        let open = Rc::clone(self);
        let write = Rc::clone(self);
        let applied = Rc::clone(self);
        [
            kit::invoke(open_channel, move |cx, _| {
                if let Some(result) = open.consume_owned_open(cx) {
                    return result;
                }
                Ok(open.resolve_open(cx.sender.view).into_ipc())
            }),
            kit::invoke_later(write_channel, move |cx, args, responder| {
                let view = cx.sender.view;
                if let Some(token) = args
                    .str(1)
                    .filter(|token| Self::is_owned_rename_token(token))
                {
                    return write.handle_owned_rename(
                        cx,
                        token.to_owned(),
                        args.str(0).map(str::to_owned),
                        args.str(2).map(str::to_owned),
                        responder,
                    );
                }
                let result = if let Some(message) = args.str(2) {
                    Err(Failure(message.to_owned()))
                } else {
                    write.write_tracked(view, args.str(0))
                };
                if let Some(token) = args.str(1) {
                    let outcome = match &result {
                        Ok(WriteResult::Written | WriteResult::Unchanged) => Ok(()),
                        Ok(_) => Err(Failure("recovery copy was not written".into())),
                        Err(error) => Err(error.clone()),
                    };
                    write.settle_flush(view, token, outcome);
                }
                responder.reply(
                    result
                        .map(|value| value.as_str().into_ipc())
                        .map_err(|error| kit::error(error.0)),
                )
            }),
            kit::invoke(applied_channel, move |cx, args| {
                let value = args.get(0);
                let token = kit::as_str(kit::field(value, "token"))
                    .ok_or_else(|| kit::error("the recovery acknowledgment has no token"))?;
                let result = match kit::field(value, "ok") {
                    IpcValue::Bool(true) => Ok(()),
                    IpcValue::Bool(false) => Err(kit::as_str(kit::field(value, "error"))
                        .unwrap_or("the renderer could not apply the recovery")
                        .to_owned()),
                    _ => return Err(kit::error("the recovery acknowledgment has no result")),
                };
                applied.settle_owned_open(cx, token, result)?;
                Ok(IpcValue::Undefined)
            }),
        ]
    }
}

/// The shared hosts in the shape the `main-core` dialog functions call
/// (`MessageBox`, `Dialog`, `PathProvider`), for this module and both editors
pub(crate) mod ui {
    use jscompat::{JsObject, JsonValue};
    use main_core::packages::electron_utils::default_save_dir::{PathProvider, Thrown};
    use main_core::packages::electron_utils::dialog_memory::Dialog;
    use main_core::packages::electron_utils::message_box::{
        MessageBox, MessageBoxOptions, ParentWindow,
    };
    use shell_contract::WindowId;
    use shell_contract::hosts::{DialogsHost, FileDialog, FileFilter};
    use shell_contract::runtime::{AppPaths, PathName};

    /// `Date.now()`
    pub(crate) fn now_ms() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_millis() as f64)
    }

    /// The window a box or dialog sits over. The shell keeps no destroyed
    /// windows in its answers, so a window it named is open.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Parent(pub WindowId);

    impl ParentWindow for Parent {
        fn is_destroyed(&self) -> bool {
            false
        }
    }

    /// Electron's `MessageBoxOptions` as the shared host takes them. `noLink`
    /// has no place there (a Windows-only look): it is dropped.
    pub(crate) fn message_box(options: &MessageBoxOptions) -> shell_contract::hosts::MessageBox {
        shell_contract::hosts::MessageBox {
            kind: Some(options.kind.to_owned()),
            title: options.title.clone(),
            message: options.message.clone(),
            detail: options.detail.clone(),
            buttons: options.buttons.clone(),
            default_id: Some(options.default_id as usize),
            cancel_id: options.cancel_id.map(|id| id as usize),
            checkbox_label: None,
        }
    }

    /// `dialog.showMessageBox` over the shared dialogs host
    pub(crate) struct Boxes(pub DialogsHost);

    impl MessageBox<Parent> for Boxes {
        fn show_message_box_over(&self, parent: &Parent, options: &MessageBoxOptions) -> f64 {
            self.0
                .message(Some(parent.0), &message_box(options))
                .response as f64
        }
        fn show_message_box(&self, options: &MessageBoxOptions) -> f64 {
            self.0.message(None, &message_box(options)).response as f64
        }
    }

    fn string_of(options: &JsObject<JsonValue>, key: &str) -> Option<String> {
        match options.get(key) {
            Some(JsonValue::String(s)) => Some(s.clone()),
            _ => None,
        }
    }

    fn strings(value: Option<&JsonValue>) -> Vec<String> {
        match value {
            Some(JsonValue::Array(items)) => items
                .iter()
                .filter_map(|item| match item {
                    JsonValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Electron's open/save dialog options object as the shared host takes them
    pub(crate) fn file_dialog(options: &JsObject<JsonValue>) -> FileDialog {
        let filters = match options.get("filters") {
            Some(JsonValue::Array(items)) => items
                .iter()
                .filter_map(|item| match item {
                    JsonValue::Object(f) => Some(FileFilter {
                        name: string_of(f, "name").unwrap_or_default(),
                        extensions: strings(f.get("extensions")),
                    }),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        FileDialog {
            title: string_of(options, "title"),
            default_path: string_of(options, "defaultPath").map(Into::into),
            button_label: string_of(options, "buttonLabel"),
            filters,
            properties: strings(options.get("properties")),
        }
    }

    /// `dialog.showOpenDialog`/`showSaveDialog` over the shared dialogs host,
    /// answering Electron's result objects
    pub(crate) struct Files(pub DialogsHost);

    impl Dialog for Files {
        type Window = Parent;

        fn show_open_dialog(
            &mut self,
            parent: Option<&Parent>,
            options: &JsObject<JsonValue>,
        ) -> JsonValue {
            let picked = self.0.open(parent.map(|p| p.0), &file_dialog(options));
            let mut result = JsObject::new();
            result.set("canceled", JsonValue::Bool(picked.is_none()));
            result.set(
                "filePaths",
                JsonValue::Array(
                    picked
                        .unwrap_or_default()
                        .iter()
                        .map(|p| JsonValue::String(p.to_string_lossy().into_owned()))
                        .collect(),
                ),
            );
            JsonValue::Object(result)
        }

        fn show_save_dialog(
            &mut self,
            parent: Option<&Parent>,
            options: &JsObject<JsonValue>,
        ) -> JsonValue {
            let picked = self.0.save(parent.map(|p| p.0), &file_dialog(options));
            let mut result = JsObject::new();
            result.set("canceled", JsonValue::Bool(picked.is_none()));
            result.set(
                "filePath",
                JsonValue::String(
                    picked.map_or_else(String::new, |p| p.to_string_lossy().into_owned()),
                ),
            );
            JsonValue::Object(result)
        }
    }

    /// `app.getPath(name)` for `main-core`'s `PathProvider`
    pub(crate) struct AppPathProvider<'a>(pub &'a dyn AppPaths);

    impl PathProvider for AppPathProvider<'_> {
        fn get_path(&self, name: &str) -> Result<String, Thrown> {
            let name = match name {
                "home" => PathName::Home,
                "appData" => PathName::AppData,
                "userData" => PathName::UserData,
                "temp" => PathName::Temp,
                "exe" => PathName::Exe,
                "desktop" => PathName::Desktop,
                "documents" => PathName::Documents,
                "downloads" => PathName::Downloads,
                "logs" => PathName::Logs,
                other => return Err(Thrown(format!("Failed to get '{other}' path"))),
            };
            Ok(self.0.path(name).to_string_lossy().into_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;
    use std::time::Duration;

    use main_core::packages::electron_utils::recovery_store::{
        RecoveryIdentity, resume_discarded_copies,
    };
    use shell_contract::runtime::PathName;
    use shell_contract::{Surface, ViewId};

    use super::{RecoveredText, TextRecovery, TextRecoveryHost, WriteResult};
    use crate::testing::{Event, HostCall, Rt};

    fn test_rt() -> Rt {
        Rt::new(super::test_hosts)
    }

    /// the editor state the recovery reads through its host
    #[derive(Default)]
    struct Editor {
        save: HashMap<ViewId, String>,
        open: HashMap<ViewId, String>,
        dirty: HashSet<ViewId>,
        headless: bool,
        restored: Vec<(ViewId, String)>,
    }

    fn recovery(rt: &Rt) -> (Rc<TextRecovery>, Rc<RefCell<Editor>>) {
        let ed: Rc<RefCell<Editor>> = Rc::default();
        let e = |f: fn(&Editor, ViewId) -> Option<String>| {
            let ed = Rc::clone(&ed);
            Box::new(move |v| f(&ed.borrow(), v))
        };
        let (d, h, r) = (Rc::clone(&ed), Rc::clone(&ed), Rc::clone(&ed));
        let host = TextRecoveryHost {
            kind: "markdown",
            untitled_ext: ".md",
            flush_channel: "markdown:recovery-flush-request",
            lang: Box::new(|| "en".into()),
            is_headless: Box::new(move || h.borrow().headless),
            save_path: e(|ed, v| ed.save.get(&v).cloned()),
            open_path: e(|ed, v| ed.open.get(&v).cloned()),
            is_dirty: Box::new(move |v| d.borrow().dirty.contains(&v)),
            restored_file: Box::new(move |v, p| r.borrow_mut().restored.push((v, p.into()))),
            untitled_fallback_title: Box::new(|| "Untitled".into()),
            now_ms: Box::new(super::ui::now_ms),
        };
        let hosts = super::RecoveryHosts::need(&rt.services).unwrap();
        (TextRecovery::new(&rt.services, host, hosts), ed)
    }

    fn copy_dir(rt: &Rt) -> std::path::PathBuf {
        rt.services
            .paths
            .path(PathName::UserData)
            .join("markdown-autosave")
    }

    fn copies(rt: &Rt) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(copy_dir(rt))
            .map(|d| {
                d.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".md"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn file_view(rt: &Rt, ed: &Rc<RefCell<Editor>>, id: u32, path: &str) -> ViewId {
        let v = rt.view(id, Surface::Markdown).view;
        ed.borrow_mut().save.insert(v, path.into());
        ed.borrow_mut().open.insert(v, path.into());
        v
    }

    #[test]
    fn a_dirty_view_writes_its_text_once_and_a_clean_close_deletes_it() {
        let rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let doc = rt.dir.join("a.md").to_string_lossy().into_owned();
        let v = file_view(&rt, &ed, 1, &doc);
        rec.track(v);
        assert_eq!(
            rec.write_copy(v, Some("x")),
            WriteResult::Skipped,
            "clean view"
        );
        ed.borrow_mut().dirty.insert(v);
        assert_eq!(rec.write_copy(v, None), WriteResult::Skipped);
        assert_eq!(rec.write_copy(v, Some("x")), WriteResult::Written);
        assert_eq!(rec.write_copy(v, Some("x")), WriteResult::Unchanged);
        assert_eq!(copies(&rt).len(), 1);
        // closing dirty keeps the copy
        rec.forget(v);
        assert_eq!(copies(&rt).len(), 1);
        // a clean close of a view that owns it deletes it
        rec.track(v);
        assert_eq!(rec.write_copy(v, Some("y")), WriteResult::Written);
        ed.borrow_mut().dirty.clear();
        rec.forget(v);
        assert!(copies(&rt).is_empty());
        assert_eq!(
            rec.write_copy(v, Some("z")),
            WriteResult::Skipped,
            "forgotten"
        );
    }

    #[test]
    fn an_untitled_copy_is_titled_by_the_shell_then_the_editor_then_the_fallback() {
        let rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let v = rt.view(1, Surface::Markdown).view;
        rec.track(v);
        ed.borrow_mut().dirty.insert(v);
        let title = |rt: &Rt| {
            let dir = copy_dir(rt).join("untitled");
            let meta = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .find(|e| e.file_name().to_string_lossy().ends_with(".meta.json"))
                .unwrap();
            let json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(meta.path()).unwrap()).unwrap();
            json["title"].as_str().unwrap().to_owned()
        };
        rec.write_copy(v, Some("1"));
        assert_eq!(title(&rt), "Untitled");
        rec.set_title(v, "Notes");
        rec.write_copy(v, Some("2"));
        assert_eq!(title(&rt), "Notes");
        rec.set_title_source(Some(Box::new(|_| Some("Tab".into()))));
        rec.write_copy(v, Some("3"));
        assert_eq!(title(&rt), "Tab");
        assert!(matches!(
            rec.identity_of(v),
            Some(RecoveryIdentity::Untitled(u)) if u.ext == ".md"
        ));
    }

    #[test]
    fn panes_on_one_file_share_the_copy_and_dont_save_resumes_the_other() {
        let rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let doc = rt.dir.join("s.md").to_string_lossy().into_owned();
        let a = file_view(&rt, &ed, 1, &doc);
        let b = file_view(&rt, &ed, 2, &doc);
        rec.track(a);
        rec.track(b);
        ed.borrow_mut().dirty.extend([a, b]);
        assert_eq!(rec.write_copy(a, Some("a")), WriteResult::Written);
        assert_eq!(rec.write_copy(b, Some("b")), WriteResult::Written);
        rec.dont_save(a);
        assert!(copies(&rt).is_empty());
        // b forgot its last copy and the block lifted: it writes the same text again
        assert_eq!(rec.write_copy(b, Some("b")), WriteResult::Written);
        // alone (a closed with its edits), "Don't Save" blocks the copy until
        // the close is cancelled
        rec.forget(a);
        rec.dont_save(b);
        assert_eq!(rec.write_copy(b, Some("c")), WriteResult::Superseded);
        resume_discarded_copies(
            &rt.services
                .hosts
                .get::<super::SharedRecoveryHost>()
                .unwrap()
                .lock()
                .unwrap(),
        );
        assert_eq!(rec.write_copy(b, Some("c")), WriteResult::Written);
    }

    #[test]
    fn a_save_deletes_the_copy_of_the_identity_before_it() {
        let rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let v = rt.view(1, Surface::Markdown).view;
        rec.track(v);
        ed.borrow_mut().dirty.insert(v);
        rec.write_copy(v, Some("draft"));
        let before = rec.identity_of(v);
        let doc = rt.dir.join("saved.md").to_string_lossy().into_owned();
        ed.borrow_mut().save.insert(v, doc.clone());
        rec.saved(v, before.as_ref());
        assert!(
            std::fs::read_dir(copy_dir(&rt).join("untitled"))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            matches!(rec.identity_of(v), Some(RecoveryIdentity::File(f)) if f.file_path == doc)
        );
    }

    #[test]
    fn a_rename_asks_for_a_copy_and_the_next_write_drops_the_old_one() {
        let mut rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let old = rt.dir.join("old.md").to_string_lossy().into_owned();
        let new = rt.dir.join("new.md").to_string_lossy().into_owned();
        let v = file_view(&rt, &ed, 1, &old);
        rec.track(v);
        ed.borrow_mut().dirty.insert(v);
        rec.write_copy(v, Some("t"));
        rt.events();
        ed.borrow_mut().save.insert(v, new.clone());
        rec.renamed(v, &old);
        assert_eq!(
            rt.events(),
            vec![Event {
                target: shell_contract::ipc::Target::View(v),
                channel: "markdown:recovery-flush-request",
                args: vec![]
            }]
        );
        assert_eq!(copies(&rt).len(), 1);
        assert_eq!(rec.write_copy(v, Some("t")), WriteResult::Written);
        let names = copies(&rt);
        assert_eq!(names.len(), 1, "{names:?}");
        rt.drain();
    }

    #[test]
    fn a_failed_save_to_another_path_drops_the_copy_written_under_it() {
        let rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let v = rt.view(1, Surface::Markdown).view;
        rec.track(v);
        ed.borrow_mut().dirty.insert(v);
        let target = rt.dir.join("mcp.md").to_string_lossy().into_owned();
        ed.borrow_mut().save.insert(v, target.clone());
        rec.write_copy(v, Some("t"));
        assert_eq!(copies(&rt).len(), 1);
        ed.borrow_mut().save.remove(&v);
        rec.save_to_path_failed(v, &target);
        assert!(copies(&rt).is_empty());
        // the target takes copies again
        ed.borrow_mut().save.insert(v, target);
        assert_eq!(rec.write_copy(v, Some("u")), WriteResult::Written);
    }

    #[test]
    fn opening_a_file_with_a_copy_asks_and_restores_or_discards() {
        let rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let doc = rt.dir.join("o.md");
        std::fs::write(&doc, "disk").unwrap();
        let doc = doc.to_string_lossy().into_owned();
        let v = file_view(&rt, &ed, 1, &doc);
        rec.track(v);
        assert_eq!(rec.resolve_open(v), None, "no copy, no question");
        ed.borrow_mut().dirty.insert(v);
        rec.write_copy(v, Some("copy"));
        // restore (button 0)
        rt.script().message.push_back(0);
        assert_eq!(
            rec.resolve_open(v),
            Some(RecoveredText {
                text: "copy".into(),
                original_path: None
            })
        );
        assert_eq!(ed.borrow().restored, vec![(v, doc.clone())]);
        assert!(
            rt.script()
                .calls
                .iter()
                .any(|c| matches!(c, HostCall::Message(Some(_), m) if m.buttons.len() == 2))
        );
        // discard (button 1) deletes it
        rt.script().message.push_back(1);
        assert_eq!(rec.resolve_open(v), None);
        assert!(copies(&rt).is_empty());
    }

    #[test]
    fn headless_and_a_sibling_view_never_ask() {
        let rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let doc = rt.dir.join("h.md").to_string_lossy().into_owned();
        let a = file_view(&rt, &ed, 1, &doc);
        let b = file_view(&rt, &ed, 2, &doc);
        rec.track(a);
        rec.track(b);
        ed.borrow_mut().dirty.insert(a);
        rec.write_copy(a, Some("c"));
        assert_eq!(rec.resolve_open(b), None, "a already shows the file");
        ed.borrow_mut().headless = true;
        rec.forget(a);
        assert_eq!(rec.resolve_open(b), None);
        assert!(rt.script().calls.is_empty());
    }

    #[test]
    fn write_copies_now_waits_for_each_dirty_view_or_its_timeout() {
        let mut rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let a = rt.view(1, Surface::Markdown).view;
        let b = rt.view(2, Surface::Markdown).view;
        let c = rt.view(3, Surface::Markdown).view;
        for v in [a, b, c] {
            rec.track(v);
        }
        ed.borrow_mut().dirty.extend([a, b]);
        let done = Rc::new(RefCell::new(None));
        let d = Rc::clone(&done);
        rec.write_copies_now(move |result| *d.borrow_mut() = Some(result));
        assert_eq!(rt.events().len(), 2, "only dirty views are asked");
        let token = rec
            .views
            .borrow()
            .get(&a)
            .unwrap()
            .flush_waiters
            .keys()
            .next()
            .unwrap()
            .clone();
        rec.settle_flush(a, "stale", Ok(()));
        assert!(done.borrow().is_none());
        assert_eq!(rec.write_tracked(a, Some("a")), Ok(WriteResult::Written));
        rec.settle_flush(a, &token, Ok(()));
        assert!(done.borrow().is_none());
        rt.advance(Duration::from_millis(2_999));
        assert!(done.borrow().is_none());
        rt.advance(Duration::from_millis(1));
        assert!(done.borrow().as_ref().unwrap().is_err(), "b timed out");
        let done = Rc::new(RefCell::new(None));
        let d = Rc::clone(&done);
        ed.borrow_mut().dirty.clear();
        rec.write_copies_now(move |result| *d.borrow_mut() = Some(result));
        assert_eq!(*done.borrow(), Some(Ok(())));
    }

    #[test]
    fn overlapping_flushes_settle_only_the_matching_request_and_destroy_fails_the_rest() {
        let rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let view = rt.view(1, Surface::Markdown).view;
        rec.track(view);
        ed.borrow_mut().dirty.insert(view);
        let first = Rc::new(RefCell::new(None));
        let result = Rc::clone(&first);
        rec.write_copies_now(move |value| *result.borrow_mut() = Some(value));
        let first_token = rec
            .views
            .borrow()
            .get(&view)
            .unwrap()
            .flush_waiters
            .keys()
            .next()
            .unwrap()
            .clone();
        let second = Rc::new(RefCell::new(None));
        let result = Rc::clone(&second);
        rec.write_copies_now(move |value| *result.borrow_mut() = Some(value));
        rec.settle_flush(view, &first_token, Ok(()));
        assert_eq!(*first.borrow(), Some(Ok(())));
        assert!(second.borrow().is_none());
        rec.forget(view);
        assert!(second.borrow().as_ref().unwrap().is_err());
    }

    #[test]
    fn a_crashed_view_keeps_its_copy() {
        let rt = test_rt();
        let (rec, ed) = recovery(&rt);
        let doc = rt.dir.join("c.md").to_string_lossy().into_owned();
        let v = file_view(&rt, &ed, 1, &doc);
        rec.track(v);
        ed.borrow_mut().dirty.insert(v);
        rec.write_copy(v, Some("c"));
        rec.view_crashed(v);
        ed.borrow_mut().dirty.clear();
        rec.forget(v);
        assert_eq!(copies(&rt).len(), 1);
    }
}
