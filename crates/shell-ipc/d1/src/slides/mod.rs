//! The `slides` channels of batch D1: a port of the `ipcMain` handlers of
//! `apps/slides/src/main` (`slides-main.ts`, `attachments-ipc.ts`,
//! `presenter-show.ts`, `generate-handoff-ipc.ts`, `template-bytes.ts`,
//! `template-cover.ts`, `seed-deck.ts`).
//!
//! Layout:
//!
//! - [`engine`]: the deck engine ([`engine::SlidesEngine`]) and the shell
//!   hooks ([`engine::SlidesShell`]) the handlers lean on.
//! - [`state`]: the session model: one [`state::SlideSession`] per deck
//!   (main-core's `Session` plus the `slides-main.ts` fields), attached to
//!   views through slots, and the module-level state of the TypeScript.
//! - [`session`]: helpers every edit handler shares (session lookup,
//!   journaled transactions, history and deck broadcast flush, slide
//!   updates, object builders).
//! - `lifecycle`, `export`, `generate`, `files`, `clipboard`, `presenter`,
//!   `edits`: the channels.

pub mod clipboard;
pub mod dialog;
pub mod edit_ops;
pub mod edits;
pub mod engine;
pub mod export;
#[cfg(any(test, feature = "probe"))]
pub mod fake;
pub mod files;
#[cfg(any(test, feature = "probe"))]
pub mod fixtures;
pub mod generate;
pub mod i18n;
pub mod inserts;
pub mod lifecycle;
pub mod master;
mod owned_close;
mod owned_save;
mod owned_seed;
pub use owned_seed::{PreparedSlidesSeed, SlidesSeedRollback};
mod open_authority;
pub mod presenter;
pub mod reads;
mod recent;
pub mod recovery;
mod saved_load;
pub mod session;
mod shared_open;
pub mod state;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use shell_contract::editors::{DocumentEditors, SaveMode, Then};
use shell_contract::hosts::{ClipboardHost, ContentsHost, DialogsHost, OsShellHost};
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::ipc::{CallContext, RegisterError, Responder};
use shell_contract::provided::Provided;
use shell_contract::runtime::CancelToken;
use shell_contract::runtime::Services;
use shell_contract::slides::{SlidesDocument, SlidesDocuments, SlidesTemplateSelectionHooks};
use shell_contract::{Batch, IpcError, IpcValue, ViewId, WindowId};

use crate::host::{LocaleHost, RecoveryStateHost, Shared, on_view_gone};
use crate::route::{self, Route};
use crate::value::{Args, string, thrown};
use engine::{SlidesEngineHost, SlidesShellHost};
use main_core::packages::electron_utils::generate_handoff::HandoffWebContents;
use main_core::slides::seed_queue::SlidesSeedOptions;
use state::{App, AppState};

type Reply = Result<IpcValue, IpcError>;

thread_local! {
    /// the apps registered on this (core) thread, for deferred jobs
    static APPS: RefCell<Vec<App>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` with the app registered at `index`, on the core thread
pub(crate) fn with_app<R>(index: usize, f: impl FnOnce(&App) -> R) -> Option<R> {
    let app = APPS.with(|apps| apps.borrow().get(index).cloned())?;
    Some(f(&app))
}

/// Posts `job` to the core thread: it finds the app again there, runs, and
/// queues what the sessions scheduled meanwhile. Callable from any thread.
pub(crate) fn post_to(services: &Services, index: usize, job: impl FnOnce(&App) + Send + 'static) {
    services.core.post(Box::new(move || {
        with_app(index, |app| {
            job(app);
            app.pump();
        });
    }));
}
/// The slides views registered on the core thread.
#[derive(Clone)]
pub struct SlidesViews(App);

/// Ordinary presentation actor prepared before its native browser is submitted.
pub struct PreparedNativeSlides {
    app_index: usize,
    view: ViewId,
    owner: shell_contract::ipc::NativeBrowserOwner,
    created: shell_contract::runtime::RendererGeneration,
    path: Option<String>,
    committed: bool,
}
impl PreparedNativeSlides {
    pub fn commit(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if self.committed || !transition.matches_owner(&self.owner) {
            return Err("The presentation browser transition does not match".into());
        }
        with_app(self.app_index, |app| {
            let mut state = app.state.borrow_mut();
            if state.native_actor_owners.contains_key(&self.view)
                || state.native_actor_owners.len() == state.native_actor_owners.capacity()
                || (self.path.is_some()
                    && state.pending_by_view.len() == state.pending_by_view.capacity())
            {
                return Err("The reserved presentation actor is unavailable".into());
            }
            next()?;
            state
                .native_actor_owners
                .insert(self.view, (self.owner.clone(), self.created.clone()));
            if let Some(path) = self.path.take() {
                state.pending_by_view.insert(self.view, path);
            }
            state.active_view = Some(self.view);
            self.committed = true;
            Ok(())
        })
        .ok_or_else(|| {
            "The original presentation actor is unavailable on this core thread".to_owned()
        })?
    }
}

impl SlidesViews {
    pub fn prepare_native_tab(
        &self,
        view: ViewId,
        created: &shell_contract::runtime::RendererGeneration,
        path: Option<&str>,
        owner: shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<PreparedNativeSlides, String> {
        if !owner.reservation().matches(view, created) {
            return Err("The presentation reservation does not match".into());
        }
        let path = path.filter(|value| !value.is_empty()).map(str::to_owned);
        {
            let mut state = self.0.state.borrow_mut();
            if state.native_actor_owners.contains_key(&view) {
                return Err("The presentation actor already exists".into());
            }
            state
                .native_actor_owners
                .try_reserve(1)
                .map_err(|_| "The presentation actor could not be reserved")?;
            if path.is_some() {
                state
                    .pending_by_view
                    .try_reserve(1)
                    .map_err(|_| "The presentation path could not be reserved")?;
            }
        }
        Ok(PreparedNativeSlides {
            app_index: self.0.registry_index,
            view,
            owner,
            created: created.clone(),
            path,
            committed: false,
        })
    }

    /// The exact close of a native presentation actor: its owner entry, attached session
    /// and queued seed are released once, and a second call finds no actor.
    pub fn retire_native_closed(
        &self,
        view: ViewId,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        let (owner, created) = self
            .0
            .state
            .borrow()
            .native_actor_owners
            .get(&view)
            .cloned()
            .ok_or("The presentation actor is not registered")?;
        if !proof.matches(owner.reservation(), view, &created) {
            return Err("The native close receipt does not match the presentation actor".into());
        }
        proof.with_terminal_owner(&owner, |transition| {
            if !transition.matches(owner.reservation(), &owner) {
                return Err("The presentation terminal owner changed".into());
            }
            Ok(())
        })?;
        saved_load::release_closed_actor(&self.0, view);
        Ok(())
    }
}

pub fn views() -> Option<SlidesViews> {
    APPS.with(|apps| apps.borrow().last().cloned().map(SlidesViews))
}

struct SeedContents {
    view: ViewId,
    alive: bool,
}

impl HandoffWebContents for SeedContents {
    fn id(&self) -> f64 {
        f64::from(self.view.0)
    }

    fn is_destroyed(&self) -> bool {
        !self.alive
    }

    fn once_destroyed(&mut self) {
        // The batch's shared view-destroyed listener already forgets every seed.
    }
}

impl SlidesViews {
    pub fn register_tab(&self, view: ViewId) {
        if self.0.services.views.sender_of(view).is_some() {
            self.set_active_view(view);
        }
    }

    pub fn queue_open(&self, view: ViewId, path: &str) {
        self.0.queue_open(view, path);
    }

    pub fn is_dirty(&self, view: ViewId) -> Result<bool, IpcError> {
        self.0.is_view_dirty(view)
    }

    pub fn has_session(&self, view: ViewId) -> bool {
        self.0.services.views.sender_of(view).is_some() && self.0.state.borrow().has_session(view)
    }

    pub fn deck_model(&self, view: ViewId) -> Result<jscompat::JsonValue, IpcError> {
        let deck = self
            .0
            .state
            .borrow()
            .session(view)
            .map(|session| session.deck())
            .ok_or_else(|| thrown("the deck session is gone"))?;
        self.0
            .engine
            .deck_model(deck)
            .map_err(|error| thrown(error.0))
    }

    pub fn deck_withholds(&self, view: ViewId) -> Result<bool, IpcError> {
        use jscompat::JsonValue;
        use main_core::shell::mcp::slides_bridge::element_withholds;
        let model = self.deck_model(view)?;
        let JsonValue::Object(deck) = model else {
            return Err(thrown("the deck model is invalid"));
        };
        let slides_value = deck.get("slides");
        let Some(JsonValue::Array(slides)) = slides_value else {
            return Err(thrown("the deck model has no slides"));
        };
        Ok(slides.iter().any(|slide| {
            let JsonValue::Object(slide) = slide else {
                return false;
            };
            match slide.get("elements") {
                Some(JsonValue::Array(elements)) => elements.iter().any(element_withholds),
                _ => false,
            }
        }))
    }

    pub fn attached_views(&self, view: ViewId) -> Vec<ViewId> {
        let state = self.0.state.borrow();
        state
            .slot_of(view)
            .map_or_else(Vec::new, |slot| state.attached_ids(slot))
    }

    pub fn slide_comments(
        &self,
        view: ViewId,
        path: &str,
    ) -> Result<jscompat::JsonValue, IpcError> {
        let deck = self
            .0
            .state
            .borrow()
            .session(view)
            .map(|session| session.deck())
            .ok_or_else(|| thrown("the deck session is gone"))?;
        let index = self
            .0
            .engine
            .slide_paths(deck)
            .map_err(|error| thrown(error.0))?
            .iter()
            .position(|item| item == path)
            .ok_or_else(|| thrown("the slide is no longer open"))?;
        let comments = self
            .0
            .engine
            .slide_comments(deck, index)
            .map_err(|error| thrown(error.0))?;
        crate::value::to_json(&comments).ok_or_else(|| thrown("the slide comments are not JSON"))
    }
    pub fn apply_transaction(
        &self,
        owner: &engine::RenderOwner,
        request: &IpcValue,
    ) -> Result<IpcValue, IpcError> {
        if !self.has_session(owner.view) {
            return Err(thrown("the deck session is gone"));
        }
        let result = edit_ops::apply_txn_view(&self.0, owner, request);
        self.0.pump();
        result
    }

    /// `runTxn(wcId, req)` of the MCP slides control: applies one transaction to the deck the
    /// view is attached to, under the capability that view's renderer bound at attach time (an
    /// MCP call has no renderer of its own). A single-window session gets no scheduled deck
    /// broadcast, so the fresh render is pushed to its tab here. The render tree is not part of
    /// the answer; it carries every picture's bytes.
    pub fn apply_mcp_transaction(
        &self,
        view: ViewId,
        request: &jscompat::JsonValue,
    ) -> Result<jscompat::JsonValue, IpcError> {
        let owner = self
            .0
            .state
            .borrow()
            .owner_of(view)
            .cloned()
            .ok_or_else(|| thrown("the deck session is gone"))?;
        let result = edit_ops::apply_txn_view(&self.0, &owner, &crate::value::from_json(request));
        let result = result.and_then(|result| {
            let applied = crate::value::truthy(crate::value::field(&result, "applied"));
            let IpcValue::Object(entries) = result else {
                return Err(thrown("the transaction answer is invalid"));
            };
            let (render, summary): (Vec<_>, Vec<_>) =
                entries.into_iter().partition(|(key, _)| key == "slides");
            if applied && let Some((_, slides)) = render.into_iter().next() {
                let owners = {
                    let state = self.0.state.borrow();
                    state
                        .slot_of(view)
                        .map_or_else(Vec::new, |slot| state.attached_owners(slot))
                };
                if owners.len() < 2 {
                    let deck = self
                        .0
                        .state
                        .borrow()
                        .session(view)
                        .map(|session| session.deck())
                        .ok_or_else(|| thrown("the deck session is gone"))?;
                    let size = self.0.engine.size(deck).map_err(|error| thrown(error.0))?;
                    let payload =
                        crate::value::obj([("slides", slides), ("size", self.0.size_value(size))]);
                    for owner in owners {
                        self.0
                            .services
                            .events
                            .emit_to_owner_checked(
                                &owner,
                                "slides:deck-changed",
                                vec![payload.clone()],
                            )
                            .map_err(thrown)?;
                    }
                }
            }
            crate::value::to_json(&IpcValue::Object(summary))
                .ok_or_else(|| thrown("the transaction answer is not JSON"))
        });
        self.0.pump();
        result
    }

    pub fn save_to_path(&self, view: ViewId, path: &str) -> Result<(), IpcError> {
        if !self.has_session(view) {
            return Err(thrown("the deck session is gone"));
        }
        self.0.save_session_deck_to(view, path)
    }

    pub fn set_active_view(&self, view: ViewId) {
        self.0.state.borrow_mut().active_view = Some(view);
    }

    pub fn active_view(&self) -> Option<ViewId> {
        self.0.state.borrow().active_view
    }

    pub fn recovery_identity(
        &self,
        view: ViewId,
    ) -> Option<main_core::packages::electron_utils::recovery_store::RecoveryIdentity> {
        let mut state = self.0.state.borrow_mut();
        let slot = state.slot_of(view)?;
        let session = state.slot_mut(slot)?;
        Some(self.0.recovery_identity_of(session))
    }

    pub fn discard_recovery(&self, view: ViewId) {
        self.0.discard_recovery(view);
    }

    pub fn queue_seed(&self, view: ViewId, bytes: &[u8], suggested_name: &str) {
        let mut contents = SeedContents {
            view,
            alive: self.0.services.views.sender_of(view).is_some(),
        };
        self.0.state.borrow_mut().seeds.queue_slides_seed_for(
            &mut contents,
            bytes.to_vec(),
            SlidesSeedOptions {
                ask_path_on_first_save: Some(true),
                suggested_name: Some(suggested_name.to_owned()),
                ..SlidesSeedOptions::default()
            },
        );
    }
}

/// The audience window of a presenter show closed (`win.on('closed')`): the
/// shell calls this from the window's close
pub fn audience_closed(view: ViewId) {
    let app = APPS.with(|apps| apps.borrow().last().cloned());
    if let Some(app) = app {
        app.audience_closed(view);
    }
}

type NowFn = fn(&App, &CallContext, Args) -> Reply;
type LaterFn = fn(&App, &CallContext, Args, Responder);
type OnFn = fn(&App, &CallContext, Args) -> Result<(), IpcError>;

/// `ipcMain.handle(channel, fn)` that answers at once
pub(crate) fn now(app: &App, channel: &'static str, f: NowFn) -> Route {
    let app = app.clone();
    route::invoke(channel, move |cx, args| {
        let reply = f(&app, cx, args);
        app.pump();
        reply
    })
}
/// Routes that render or mutate a deck must authenticate before invoking their handler.
pub(crate) fn now_render(app: &App, channel: &'static str, f: NowFn) -> Route {
    let app = app.clone();
    route::invoke(channel, move |cx, args| {
        session::render_owner(cx)?;
        let reply = f(&app, cx, args);
        app.pump();
        reply
    })
}

/// `ipcMain.handle(channel, async fn)`
pub(crate) fn later(app: &App, channel: &'static str, f: LaterFn) -> Route {
    let app = app.clone();
    route::invoke_later(channel, move |cx, args, responder| {
        f(&app, cx, args, responder);
        app.pump();
    })
}

/// `ipcMain.on(channel, fn)`
pub(crate) fn on(app: &App, channel: &'static str, f: OnFn) -> Route {
    let app = app.clone();
    route::send(channel, move |cx, args| {
        let result = f(&app, cx, args);
        app.pump();
        result
    })
}

/// What the other areas call into (tabs, menus, MCP): the exported
/// functions of `slides-main.ts`
struct SlidesEditors {
    app: App,
}

impl DocumentEditors for SlidesEditors {
    fn saved_surface(&self) -> shell_contract::Surface {
        shell_contract::Surface::Slides
    }

    fn owns(&self, view: ViewId) -> bool {
        self.app.state.borrow().has_session(view)
    }

    fn document_path(&self, view: ViewId) -> Option<PathBuf> {
        let state = self.app.state.borrow();
        let session = state.session(view)?;
        (!session.path().is_empty()).then(|| PathBuf::from(session.path()))
    }

    fn capture_document_owner(
        &self,
        view: ViewId,
        native: NativeRequestOwner,
    ) -> Result<NativeRequestOwner, IpcError> {
        self.app.capture_save_owner(view, native)
    }

    fn prepare_saved_document(
        &self,
        target: shell_contract::ipc::NativeSavedTarget,
    ) -> Result<Box<dyn shell_contract::editors::PreparedSavedDocument>, IpcError> {
        saved_load::prepare(&self.app, target)
    }

    fn load_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        then: Then<shell_contract::editors::SavedDocumentLoadOutcome>,
    ) {
        saved_load::load(&self.app, document, then);
        self.app.pump();
    }

    /// `requestRendererSave`: the renderer runs its full save flow and
    /// reports on `slides:close-save-result`. `saveAs` is the menu's
    /// `slides:menu` message: the renderer's Save As ends in `slides:save-as`,
    /// whose answer resumes the request (`true` saved; `false` cancelled, a
    /// copy format, or failed: the contract's "user cancelled or it failed",
    /// so a handler error is `false` here, not an `Err`).
    fn request_save(&self, view: ViewId, mode: SaveMode, then: Then<bool>) {
        match mode {
            SaveMode::Save => self
                .app
                .request_renderer_save(view, Box::new(move |ok| then(Ok(ok)))),
            SaveMode::SaveAs => self
                .app
                .request_save_as(view, Box::new(move |ok| then(Ok(ok)))),
        }
        self.app.pump();
    }

    fn request_save_owned(
        &self,
        view: ViewId,
        mode: SaveMode,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<bool>,
    ) {
        self.app
            .request_save_owned(view, mode, origin, cancel, None, then);
        self.app.pump();
    }

    fn request_save_native(
        &self,
        view: ViewId,
        mode: SaveMode,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<bool>,
    ) {
        self.app
            .request_save_native(view, mode, binding, cancel, pending, then);
        self.app.pump();
    }

    fn request_close_owned(
        &self,
        view: ViewId,
        parent: Option<WindowId>,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<bool>,
    ) {
        self.app
            .request_close_owned(view, parent, origin, cancel, then);
        self.app.pump();
    }

    fn request_close(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>) {
        self.app.request_close(view, parent, then);
        self.app.pump();
    }

    /// The slides main process has no text export of its own (the AI reads
    /// through `slides:apply-txn` and the session ops): the caller sees an
    /// error as a missing function would give it
    fn read_text(&self, _view: ViewId, then: Then<String>) {
        then(Err(thrown("slides has no read-text export")))
    }

    /// `saveSessionDeckTo(session, filePath)`
    fn save_to_path(&self, view: ViewId, path: PathBuf, then: Then<()>) {
        let result = self.app.save_session_deck_to(view, &path.to_string_lossy());
        self.app.pump();
        then(result)
    }

    fn save_to_path_owned(
        &self,
        view: ViewId,
        path: PathBuf,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<()>,
    ) {
        self.app
            .save_to_path_owned(view, &path.to_string_lossy(), origin, cancel, then);
        self.app.pump();
    }

    fn save_to_path_native(
        &self,
        view: ViewId,
        path: PathBuf,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<()>,
    ) {
        self.app
            .save_to_path_native(view, &path, binding, cancel, pending, then);
        self.app.pump();
    }

    /// `slidesFileRenamed(wc, oldPath, newPath)`
    fn file_renamed(&self, view: ViewId, old_path: &Path, new_path: &Path) {
        self.app.file_renamed(
            view,
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
        );
    }

    fn file_renamed_owned(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        then: Then<()>,
    ) {
        self.app.rename_path_owned(
            view,
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
            origin,
            destination,
            cancel,
            pending,
            then,
        );
        self.app.pump();
    }
    fn file_renamed_many_owned_extra(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        self.app.rename_path_many_owned_extra(
            view,
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
            origin,
            destination,
            participants,
            cancel,
            pending,
            additional,
            then,
        );
        self.app.pump();
    }

    fn file_renamed_existing_document(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        authority: &shell_contract::ipc::NativeSaveRevisionAuthority,
        expected_path: &Path,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<Option<shell_contract::ipc::NativeSaveRevisionReceipt>>,
    ) {
        self.app.rename_path_revision(
            view,
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
            authority,
            expected_path,
            destination,
            participants,
            cancel,
            pending,
            additional,
            Box::new(move |result| then(result.map(|()| None))),
        );
        self.app.pump();
    }

    fn file_renamed_recovery(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        then: Then<()>,
    ) {
        self.app.rename_path_recovery(
            view,
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
            receipt,
            receiver,
            destination,
            participants,
            cancel,
            pending,
            then,
        );
        self.app.pump();
    }

    /// The File menu's `slides:menu` command: `pdf`, `png` and `jpg` are the
    /// menu's export-pdf, export-images and export-images-jpg
    fn export_request(&self, view: ViewId, format: &str) {
        let command = match format {
            "png" => "export-images".to_owned(),
            "jpg" => "export-images-jpg".to_owned(),
            other => format!("export-{other}"),
        };
        self.app.emit(view, "slides:menu", vec![string(command)]);
    }
}

/// `sessions.get(view)` for the batches that ask which deck a view shows
struct SlidesDocs {
    app: App,
}

impl SlidesDocuments for SlidesDocs {
    fn document(&self, view: ViewId) -> Option<SlidesDocument> {
        let state = self.app.state.borrow();
        state.session(view).map(|s| self.app.document_of_session(s))
    }
}

impl shell_contract::slides::SlidesRecentFiles for SlidesDocs {
    fn prepare_moved_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_contract::slides::OwnedRecentMove, String> {
        self.app
            .prepare_recent_move(origin, destination, &[], cancel, pending, &[], effects)
    }
    fn prepare_moved_many_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_contract::slides::OwnedRecentMove, String> {
        self.app.prepare_recent_move(
            origin,
            destination,
            participants,
            cancel,
            pending,
            &[],
            effects,
        )
    }
    fn prepare_moved_many_owned_extra(
        &self,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_contract::slides::OwnedRecentMove, String> {
        self.app.prepare_recent_move(
            origin,
            destination,
            participants,
            cancel,
            pending,
            additional,
            effects,
        )
    }
    fn prepare_moved_recovery(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        snapshot: &shell_contract::ipc::RecoveryAuthoritySnapshot,
        receiver: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_contract::slides::OwnedRecentMove, String> {
        self.app.prepare_recent_move_recovery(
            receipt,
            snapshot,
            receiver,
            destination,
            participants,
            cancel,
            pending,
            effects,
        )
    }
    fn prepare_moved_existing_document(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<shell_contract::slides::OwnedRecentMove, String> {
        self.app.prepare_recent_move_existing(
            binding,
            participants,
            cancel,
            pending,
            additional,
            effects,
        )
    }
}

impl App {
    /// The session as the other batches name it
    pub fn document_of_session(&self, s: &state::SlideSession) -> SlidesDocument {
        SlidesDocument {
            id: s.id,
            path: s.path().to_owned(),
        }
    }

    /// `followTemplateSelection(session, fromPath, toPath)`: every hook
    /// provided, none when absent
    pub fn follow_template(&self, s: &state::SlideSession, from: &str, to: &str) {
        let doc = self.document_of_session(s);
        for hooks in self.provided.all::<dyn SlidesTemplateSelectionHooks>() {
            hooks.follow(&doc, from, to);
        }
    }

    /// `inheritTemplateSelection(previous, replacement)`
    pub fn inherit_template(
        &self,
        previous: Option<&SlidesDocument>,
        replacement: &state::SlideSession,
    ) {
        let doc = self.document_of_session(replacement);
        for hooks in self.provided.all::<dyn SlidesTemplateSelectionHooks>() {
            hooks.inherit(previous, &doc);
        }
    }
}

impl App {
    /// A view is gone: the work of `trackSlidesWebContents`'s
    /// `once('destroyed')` and of the presenter show
    fn view_gone(&self, view: ViewId) {
        if self.state.borrow().native_actor_owners.contains_key(&view) {
            // The actor itself is retired by its closure receipt. Its attachment to the shared
            // presentation must not outlive the view meanwhile: a later export or save of the
            // same deck would try to capture an owner for a view that is gone and be refused.
            eprintln!(
                "[slides] view {}: gone, detaching it from its presentation",
                view.0
            );
            self.state.borrow_mut().detach_view(view);
            return;
        }
        saved_load::abandon(self, view);
        let pending = self.state.borrow_mut().pending_recovery_seeds.remove(&view);
        if let Some(pending) = pending {
            (pending.done)(Err(main_core::shell::renderer_disposal::Failure(
                "The presentation view was closed".into(),
            )));
        }
        recovery::abandon_recovery(self, view);
        self.close_narration_microphone(view);
        self.release_recovery(view);
        self.close_presenter_show(view);
        self.audience_closed(view);
        let waiter = {
            let mut state = self.state.borrow_mut();
            state.detach_view(view);
            if state.active_view == Some(view) {
                state.active_view = None;
            }
            state.pending_by_view.remove(&view);
            state.recovery_open_decisions.remove(&view);
            state.last_slide_paste.remove(&view);
            state.auto_save_pref.remove(&view);
            state.seeds.forget(f64::from(view.0));
            owned_seed::forget_seed_on_view_gone(&mut state, view);
            state.headless_targets.remove(&view);
            let at = state.close_waiters.iter().rposition(|w| w.0 == view);
            at.map(|at| state.close_waiters.remove(at).2)
        };
        if let Some(waiter) = waiter {
            waiter(false);
        }
        // a Save As the menu asked for ends with its view
        let save_as = self.state.borrow_mut().save_as_waiters.remove(&view);
        if let Some((_, waiter)) = save_as {
            waiter(false);
        }
        self.pump();
    }

    /// Queues the path a view opens when its renderer asks (`pendingByWc`)
    pub fn queue_open(&self, view: ViewId, path: &str) {
        self.state
            .borrow_mut()
            .pending_by_view
            .insert(view, path.to_owned());
    }
}

pub fn routes(services: &Services, provided: &Rc<Provided>) -> Result<Vec<Route>, RegisterError> {
    let hosts = &services.hosts;
    let engine = hosts.need::<SlidesEngineHost>(Batch::D1)?;
    let shell = hosts.need::<SlidesShellHost>(Batch::D1)?;
    let dialogs = hosts.need::<DialogsHost>(Batch::D1)?;
    let os_shell = hosts.need::<OsShellHost>(Batch::D1)?;
    let clipboard = hosts.need::<ClipboardHost>(Batch::D1)?;
    let contents = hosts.need::<ContentsHost>(Batch::D1)?;
    let locale = hosts.need::<LocaleHost>(Batch::D1)?;
    let shared = Shared::need(hosts)?;
    let recovery_state = hosts.need::<RecoveryStateHost>(Batch::D1)?;
    let registry_index = APPS.with(|apps| apps.borrow().len());
    let app = App {
        state: Rc::new(RefCell::new(AppState::default())),
        registry_index,
        services: services.clone(),
        engine,
        shell,
        dialogs,
        os_shell,
        clipboard,
        contents,
        locale,
        shared,
        recovery_state,
        text_cache: Arc::new(Mutex::default()),
        provided: provided.clone(),
        backoff: Rc::default(),
    };
    APPS.with(|apps| apps.borrow_mut().push(app.clone()));
    provided.add::<dyn DocumentEditors>(Rc::new(SlidesEditors { app: app.clone() }));
    provided
        .add::<dyn main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract<
                ViewId,
                shell_contract::ipc::RecoveryOpenTarget,
            >>(Rc::new(SlidesViews(app.clone())));
    provided.add::<dyn SlidesDocuments>(Rc::new(SlidesDocs { app: app.clone() }));
    provided.add::<dyn shell_contract::slides::SlidesRecentFiles>(Rc::new(SlidesDocs {
        app: app.clone(),
    }));
    let gone = app.clone();
    on_view_gone(services, move |view| gone.view_gone(view));
    app.schedule_recovery_tick();

    let mut routes = vec![
        // lifecycle
        later(&app, "slides:open", lifecycle::open),
        later(&app, "slides:open-path", lifecycle::open_path),
        later(
            &app,
            "slides:consume-pending-open",
            lifecycle::consume_pending_open,
        ),
        now(
            &app,
            "slides:recovery-initialized",
            recovery::recovery_initialized,
        ),
        now(&app, "slides:new-blank", lifecycle::new_blank),
        later(&app, "slides:recent", lifecycle::recent),
        later(&app, "slides:save", owned_save::save),
        later(&app, "slides:save-as", owned_save::save_as),
        later(
            &app,
            "slides:save-into-project",
            owned_save::save_into_project,
        ),
        now(&app, "slides:is-dirty", lifecycle::is_dirty),
        on(&app, "slides:autosave-pref", lifecycle::autosave_pref),
        on(
            &app,
            "slides:close-save-result",
            lifecycle::close_save_result,
        ),
        now(&app, "slides:undo", lifecycle::undo),
        now(&app, "slides:redo", lifecycle::redo),
        now(
            &app,
            "slides:history-batch-begin",
            lifecycle::history_batch_begin,
        ),
        now(
            &app,
            "slides:history-batch-end",
            lifecycle::history_batch_end,
        ),
        now(
            &app,
            "slides:get-render-slides",
            lifecycle::get_render_slides,
        ),
        // export and pickers
        now(&app, "slides:pick-export-dir", export::pick_export_dir),
        now(
            &app,
            "slides:pick-export-pdf-path",
            export::pick_export_pdf_path,
        ),
        later(&app, "slides:export-images", export::export_images),
        now(
            &app,
            "slides:ask-withheld-export",
            export::ask_withheld_export,
        ),
        later(&app, "slides:export-pdf", export::export_pdf),
        now(&app, "slides:save-picture", export::save_picture),
        now(
            &app,
            "slides:consume-headless-export",
            export::consume_headless_export,
        ),
        on(
            &app,
            "slides:headless-export-done",
            export::headless_export_done,
        ),
        // fonts, templates, hand-off
        now(
            &app,
            "slides:font-install-local",
            export::font_install_local,
        ),
        now(
            &app,
            "slides:private-font-faces",
            export::private_font_faces,
        ),
        now(&app, "slides:private-font-data", export::private_font_data),
        now(&app, "slides:template-bytes", export::template_bytes),
        later(
            &app,
            "slides:template-cover-render",
            export::template_cover_render,
        ),
        now(&app, "slides:handoff-take", export::handoff_take),
        now(
            &app,
            "slides:handoff-run-started",
            export::handoff_run_started,
        ),
        // narration microphone
        later(&app, "slides:microphone-access", export::microphone_access),
        on(
            &app,
            "slides:microphone-release",
            export::microphone_release,
        ),
        // generated pages
        later(
            &app,
            "slides:local-page-generate",
            export::local_page_generate,
        ),
        now(
            &app,
            "slides:land-generated-pages",
            generate::land_generated_pages,
        ),
        // attachments
        now(&app, "slides:files-pick", files::files_pick),
        now(&app, "slides:files-add", files::files_add),
        now(
            &app,
            "slides:files-add-pasted-image",
            files::files_add_pasted_image,
        ),
        later(&app, "slides:files-read", files::files_read),
        now(&app, "slides:files-read-image", files::files_read_image),
        // presenter and show
        later(&app, "slides:presenter-start", presenter::presenter_start),
        on(&app, "slides:presenter-sync", presenter::presenter_sync),
        on(&app, "slides:presenter-ink", presenter::presenter_ink),
        later(&app, "slides:presenter-swap", presenter::presenter_swap),
        now(&app, "slides:presenter-end", presenter::presenter_end),
        now(&app, "slides:audience-ready", presenter::audience_ready),
        on(&app, "slides:audience-nav", presenter::audience_nav),
        later(&app, "slides:show-fullscreen", presenter::show_fullscreen),
        // clipboards
        now(
            &app,
            "slides:has-slide-clipboard",
            clipboard::has_slide_clipboard,
        ),
        now(&app, "slides:copy-slides", clipboard::copy_slides),
        now(&app, "slides:paste-slide", clipboard::paste_slide),
        now(&app, "slides:repaste-slide", clipboard::repaste_slide),
        now(
            &app,
            "slides:clipboard-external",
            clipboard::clipboard_external,
        ),
        now(&app, "slides:clipboard-probe", clipboard::clipboard_probe),
        now(&app, "slides:copy-elements", clipboard::copy_elements),
        now(
            &app,
            "slides:copy-elements-image",
            clipboard::copy_elements_image,
        ),
        now(&app, "slides:paste-elements", clipboard::paste_elements),
        now(
            &app,
            "slides:duplicate-elements",
            clipboard::duplicate_elements,
        ),
        now(&app, "slides:native-clipboard", clipboard::native_clipboard),
    ];
    routes.extend(edits::routes(&app));
    routes.extend(edit_ops::routes(&app));
    routes.extend(reads::routes(&app));
    routes.extend(master::routes(&app));
    routes.extend(inserts::routes(&app));
    Ok(routes)
}

/// This app's fake hosts for [`crate::testkit::Kit`] and `ipc-probe`; `case`
/// is the case's `hosts.d1` value (Null in unit tests)
#[cfg(any(test, feature = "probe"))]
pub fn insert_fakes(hosts: &mut shell_contract::Hosts, case: &serde_json::Value) {
    fake::insert(hosts, case);
}

/// The shell's `createSlidesWindow(openPath)` and `open-file` after the page
/// loaded: opens `path` in `view` at the editor's default width and tells the
/// renderer with `slides:opened`. A deck that cannot be opened is ignored, as
/// the TypeScript's `catch` does. Called by the shell on the core thread.
pub fn open_file_in_window(owner: engine::RenderOwner, path: &str) {
    let app = APPS.with(|apps| apps.borrow().last().cloned());
    if let Some(app) = app {
        if let Ok(result) = app.open_and_build(&owner, path, 1280.0, None) {
            // the history notice is queued for the end of the task: it goes out while
            // the TypeScript awaits the recent list, before the open's answer is sent
            app.pump();
            let index = app.registry_index;
            post_to(&app.services.clone(), index, move |app| {
                if let Err(error) =
                    app.services
                        .events
                        .emit_to_owner_checked(&owner, "slides:opened", vec![result])
                {
                    eprintln!("[slides] scoped opened notification failed: {error}");
                }
                app.pump();
            });
        }
        app.pump();
    }
}

/// This app's triggers for [`crate::probe_trigger`]: `name` without its
/// `d1/` prefix; false when it is not one of them. Named after the TypeScript
/// the shell runs:
///
/// - `slides:requestSlidesClose` `[withParent]`: `requestSlidesClose(contents, parent)`
/// - `slides:slidesMenu` `[command]`: the click of the File or Edit menu item that
///   sends `slides:menu` `command` (`save-as`, `export-pdf`, `export-images`,
///   `export-images-jpg`)
/// - `slides:openFileInWindow` `[path]`: the open of `createSlidesWindow(path)`
#[cfg(feature = "probe")]
pub fn probe_trigger(
    name: &str,
    view: shell_contract::ViewId,
    owner: Option<engine::RenderOwner>,
    args: &serde_json::Value,
    _services: &Services,
    _provided: &std::rc::Rc<shell_contract::provided::Provided>,
    then: shell_contract::editors::Then<shell_contract::IpcValue>,
) -> bool {
    use shell_contract::editors::{DocumentEditors, SaveMode};
    let Some(what) = name.strip_prefix("slides:") else {
        return false;
    };
    let Some(app) = APPS.with(|apps| apps.borrow().last().cloned()) else {
        return false;
    };
    let text = args
        .get(0)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    let editors = SlidesEditors { app: app.clone() };
    match what {
        "requestSlidesClose" => {
            let parent = args
                .get(0)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                .then(|| app.shell.host_window_for(view))
                .flatten();
            editors.request_close(
                view,
                parent,
                Box::new(move |close| then(close.map(shell_contract::IpcValue::Bool))),
            );
        }
        "slidesMenu" => {
            match text.as_str() {
                "save-as" => editors.request_save(view, SaveMode::SaveAs, Box::new(|_| {})),
                "export-pdf" => editors.export_request(view, "pdf"),
                "export-images" => editors.export_request(view, "png"),
                "export-images-jpg" => editors.export_request(view, "jpg"),
                _ => return false,
            }
            app.pump();
            then(Ok(shell_contract::IpcValue::Undefined));
        }
        // `createSlidesView(openPath)`: the tab opens the path when its renderer mounts
        "createSlidesView" => {
            if !text.is_empty() && std::path::Path::new(&text).exists() {
                app.queue_open(view, &text);
            }
            then(Ok(shell_contract::IpcValue::Undefined));
        }
        "openFileInWindow" => {
            let Some(owner) = owner.filter(|owner| owner.view == view) else {
                then(Err(thrown("missing captured presentation window owner")));
                return true;
            };
            open_file_in_window(owner, &text);
            then(Ok(shell_contract::IpcValue::Undefined));
        }
        _ => return false,
    }
    true
}
