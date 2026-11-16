//! The Markdown editor (`apps/markdown/src/main/markdown-main.ts`, `seed-main.ts`, `note-append.ts`).
//!
//! The document state and the flows other areas call are the HTML editor's
//! [`TextDocs`] (the two apps keep the same shape); this module adds the
//! Markdown editor's own state: the wiki (index, fragments), the seed, the
//! note appends, and its save, image and export handlers ([`save`]). What is
//! not in Rust sits behind [`MarkdownEngine`] (odt-engine, the hidden print
//! window, the network) and [`MarkdownShell`] (the shell's hooks).

#[cfg(any(test, feature = "probe"))]
mod fake;
mod save;
#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jscompat::node_path::host::{dirname, resolve};
use jscompat::serde::Serialize;
use main_core::markdown::asset_lifecycle::{
    discard_pending_owned_assets, rename_owned_asset_document,
};
use main_core::markdown::image_host::{FetchFailure, FetchInit, FetchResponse};
use main_core::markdown::wiki_create::{CreateNoteOptions, create_wiki_note};
use main_core::markdown::wiki_fragment::WikiFragments;
use main_core::markdown::wiki_heads::wiki_heads;
use main_core::markdown::wiki_index::{WikiIndex, node_vault_fs};
use main_core::markdown::wiki_notes::list_wiki_notes;
use main_core::markdown::wiki_resolve::{ResolveOptions, node_fs, resolve_wiki_links};
use main_core::packages::electron_utils::print_window::{JsError, RendererAnswer, RendererCall};
use main_core::packages::electron_utils::safe_remote_url::{Fetcher, Resolver, Response};
use shell_contract::hosts::OsShellHost;
use shell_contract::ipc::{RegisterError, Target};
use shell_contract::runtime::Services;
use shell_contract::{Batch, DocumentEditors, IpcValue, Provided, ViewId};

use crate::html::odt_export::OdtConversion;
use crate::html::text_doc::{DocChannels, DocHooks, TextDocs, text_document_editors};
use crate::kit::{self, Entry, IntoIpc, ViewMap, field, obj};
use crate::text_recovery::ui::now_ms;

/// `printMarkdownPdf`'s hidden window (`new BrowserWindow({ show: false,
/// webPreferences: { sandbox: true, javascript: false } })`). Its renderer
/// calls are awaited in the TypeScript, so here they answer later.
pub trait MarkdownPrintWindow: Send {
    /// carries out `win.loadFile(path)` or `webContents.printToPDF(options)`
    /// and calls `answer` once, from any thread, with what it came to
    fn call(&mut self, call: RendererCall, answer: Box<dyn FnOnce(RendererAnswer) + Send>);
    /// `win.destroy()`
    fn destroy(&mut self) -> Result<(), JsError>;
}

/// What the Markdown handlers call that is not in Rust. Every method may run
/// on a worker.
pub trait MarkdownEngine: Send + Sync {
    /// a new hidden print window
    fn print_window(&self) -> Box<dyn MarkdownPrintWindow>;
    fn print_window_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn MarkdownPrintWindow>, String>;
    fn print_window_native(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
    ) -> Result<Box<dyn MarkdownPrintWindow>, String>;
    fn print_window_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn MarkdownPrintWindow>, String>;
    /// `docxToOdt(docx)` (packages/odt-engine)
    fn docx_to_odt(&self, docx: Vec<u8>) -> Result<OdtConversion, String>;
    fn docx_to_odt_cancellable(
        &self,
        docx: Vec<u8>,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<OdtConversion, String>;
    /// the DNS lookups `fetchRemoteImage` makes (Node's `dns.lookup`)
    fn resolver(&self) -> Box<dyn Resolver + Send>;
    /// the HTTP client `fetchRemoteImage` uses (Node's `fetch`/undici)
    fn fetcher(&self) -> Box<dyn Fetcher + Send>;
    /// Electron's `net.fetch(url)` that `saveImageFromUrl` uses, the app
    /// schemes (`md-asset://`) included; `Err` is what it throws
    fn net_fetch(
        &self,
        view: ViewId,
        url: &str,
        cancel: &shell_contract::runtime::CancelToken,
    ) -> Result<Box<dyn Response>, String>;
    /// the `fetch` `uploadImageToHost` sends its request with
    fn upload_fetch(&self, url: &str, init: &FetchInit<'_>) -> Result<FetchResponse, FetchFailure>;
}

pub type MarkdownEngineHost = Arc<dyn MarkdownEngine>;

/// The shell's side of the Markdown editor: its hooks and process state.
/// Called on the core thread.
pub trait MarkdownShell: Send + Sync {
    /// `isHeadlessMode()`
    fn is_headless(&self) -> bool;
    /// `getUiLang()`
    fn ui_lang(&self) -> String;
    /// `Date.now()` (recovery copies, image host keys)
    fn now_ms(&self) -> f64;
    /// `contents.reload()` (recovery's open decisions)
    fn reload(&self, view: ViewId);
    /// `isPathOpenElsewhere(path)`: the split-pane probe
    fn is_path_open_elsewhere(&self, path: &str) -> bool;
    /// `runtime.openGeneratedPath(path)`: true when a tab opened it
    fn open_generated_path(&self, path: &Path) -> bool;
    /// `runtime.newDocumentDir(webContentsId)`
    fn new_document_dir(&self, view: ViewId) -> Option<PathBuf>;
    /// `runtime.writeNewDocument(webContentsId, fileName, bytes)`
    fn write_new_document(
        &self,
        view: ViewId,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<PathBuf>, String>;
    fn write_new_document_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<PathBuf>, String>;
    /// [`Self::write_new_document_owned`] for a save a native operation owns.
    #[allow(clippy::too_many_arguments)]
    fn write_new_document_existing_document(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Option<PathBuf>, String>;
    /// Replace this view's document-scoped asset roots; both None revoke them.
    fn asset_paths_changed(
        &self,
        view: ViewId,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<(), String>;
    fn asset_paths_changed_owned(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        captured: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        effects: &shell_contract::hosts::OwnedEffectsHost,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<(), String>;
    fn prepare_native_save_asset_paths(
        &self,
        destination: &shell_contract::ipc::NativeRequestOwner,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<crate::html::RecoveryAssetStage, String>;
    fn prepare_saved_document_asset_paths(
        &self,
        destination: &shell_contract::ipc::NativeRequestOwner,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<crate::html::SavedAssetStage, String>;
    fn prepare_recovery_asset_paths(
        &self,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<Box<dyn FnMut() -> Result<(), String> + Send>, String>;
    fn prepare_rename_asset_paths_recovery(
        &self,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        tracked: bool,
        open: Option<&str>,
        save: Option<&str>,
    ) -> Result<crate::html::RecoveryAssetStage, String>;
    /// `fileSavedHook(contents, path)`
    fn file_saved(&self, view: ViewId, path: &Path);
    fn file_saved_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        path: PathBuf,
        then: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<PathBuf>, String>)>,
    );
    fn file_saved_native(
        &self,
        destination: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        additional: Vec<shell_contract::runtime::CancelToken>,
        effects: shell_contract::hosts::OwnedEffectsHost,
        then: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<PathBuf>, String>)>,
    );
    /// `docxExportedHook(path)`: a "convert and open in Docs" export landed
    fn docx_exported(&self, path: &Path);
    fn export_written_owned(
        &self,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        effects: shell_contract::hosts::OwnedEffectsHost,
        path: PathBuf,
        kind: &'static str,
        then: Box<dyn FnOnce(Result<(), String>)>,
    );
    /// `randomUUID()` of the page image exports (session ids, folder names)
    /// and of the Docs conversions (session folder, file names)
    fn random_uuid(&self) -> String {
        jscompat::random_uuid()
    }
}

pub type MarkdownShellHost = Arc<dyn MarkdownShell>;

/// The Markdown editor: its documents and the calls other areas make into it.
/// The calls both text editors share (view set-up, recovery contract) come
/// from [`text_document_editors!`].
pub struct MarkdownEditors {
    docs: Rc<TextDocs>,
    seeds: ViewMap<String>,
    owned_seeds: OwnedSeedMap,
    /// `queueMarkdownNoteAppendFor`'s pending texts by view: the note and its texts
    notes: ViewMap<(String, Vec<String>)>,
    services: Services,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SeedOutcome {
    Pending,
    Consumed,
    Retired,
    Replaced,
}

#[derive(Clone)]
struct OwnedSeedMap {
    map: Rc<RefCell<std::collections::HashMap<ViewId, OwnedSeed>>>,
    prepared: Rc<Cell<usize>>,
}

impl OwnedSeedMap {
    fn new(_services: &Services) -> Self {
        Self {
            map: Rc::default(),
            prepared: Rc::default(),
        }
    }
    fn get(&self, view: ViewId) -> Option<OwnedSeed> {
        self.map.borrow().get(&view).cloned()
    }
    fn insert(&self, view: ViewId, seed: OwnedSeed) -> Option<OwnedSeed> {
        self.map.borrow_mut().insert(view, seed)
    }
    fn remove(&self, view: ViewId) -> Option<OwnedSeed> {
        self.map.borrow_mut().remove(&view)
    }
}

#[derive(Clone)]
struct OwnedSeed {
    text: String,
    _source: shell_contract::ipc::NativeRequestOwner,
    created: shell_contract::runtime::RendererGeneration,
    effects: shell_contract::hosts::OwnedEffectsHost,
    identity: Rc<()>,
    outcome: Rc<Cell<SeedOutcome>>,
}

/// Core-thread rollback of one exact generated-seed insertion.
pub struct MarkdownSeedRollback {
    seeds: OwnedSeedMap,
    view: ViewId,
    created: shell_contract::runtime::RendererGeneration,
    reservation: shell_contract::ipc::NativeClosureReservation,
    browser: Option<shell_contract::ipc::NativeBrowserOwner>,
    committed: Cell<bool>,
    identity: Rc<()>,
    outcome: Rc<Cell<SeedOutcome>>,
    previous: RefCell<Option<OwnedSeed>>,
    legacy_seeds: ViewMap<String>,
    previous_legacy: RefCell<Option<String>>,
    close_proven: Cell<bool>,
    completed: Cell<bool>,
}

pub struct PreparedMarkdownSeed {
    seeds: OwnedSeedMap,
    legacy_seeds: ViewMap<String>,
    browser: shell_contract::ipc::NativeBrowserOwner,
    seed: Option<OwnedSeed>,
    rollback: Option<Rc<MarkdownSeedRollback>>,
}

impl Drop for PreparedMarkdownSeed {
    fn drop(&mut self) {
        if self.seed.is_some() {
            self.seeds.prepared.set(self.seeds.prepared.get() - 1);
        }
    }
}

impl PreparedMarkdownSeed {
    pub fn receipt(&self) -> Rc<MarkdownSeedRollback> {
        self.rollback
            .as_ref()
            .expect("the generated Markdown receipt was already committed")
            .clone()
    }

    pub fn commit(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<Rc<MarkdownSeedRollback>, String> {
        let seed = self
            .seed
            .as_ref()
            .ok_or("the generated Markdown seed was already committed")?;
        let rollback = self
            .rollback
            .as_ref()
            .ok_or("the generated Markdown receipt is unavailable")?;
        if !transition.matches(&rollback.reservation)
            || !(transition.matches_owner(&self.browser)
                || transition.proposes_owner(&self.browser))
            || !rollback.reservation.matches(rollback.view, &seed.created)
        {
            return Err("the generated Markdown seed belongs to another browser".into());
        }
        let mut entries = self
            .seeds
            .map
            .try_borrow_mut()
            .map_err(|_| "the generated Markdown seed map is busy")?;
        initialize()?;
        let seed = self
            .seed
            .take()
            .expect("the seed was validated before initialization");
        let rollback = self
            .rollback
            .take()
            .expect("the receipt was validated before initialization");
        *rollback.previous_legacy.borrow_mut() = self.legacy_seeds.remove(rollback.view);
        *rollback.previous.borrow_mut() = entries.insert(rollback.view, seed);
        rollback.committed.set(true);
        self.seeds.prepared.set(self.seeds.prepared.get() - 1);
        Ok(rollback)
    }
}

impl MarkdownSeedRollback {
    /// Whether this exact seed was consumed by its authenticated renderer.
    pub fn was_consumed(&self) -> bool {
        self.outcome.get() == SeedOutcome::Consumed
    }

    pub fn retire_superseded(
        &self,
        proof: &shell_contract::ipc::NativeBrowserSuperseded,
    ) -> Result<(), String> {
        let browser = self
            .browser
            .as_ref()
            .ok_or("the generated Markdown seed has no browser owner")?;
        if !self.committed.get() || !proof.matches(&self.reservation, browser) {
            return Err(
                "the browser supersession does not match the generated Markdown seed".into(),
            );
        }
        if self.completed.get() {
            return Ok(());
        }
        if self.outcome.get() == SeedOutcome::Pending
            && self
                .seeds
                .get(self.view)
                .is_some_and(|current| Rc::ptr_eq(&current.identity, &self.identity))
        {
            return Err("the generated Markdown seed is still current".into());
        }
        if let Some(previous) = self.previous.borrow_mut().take() {
            if previous.outcome.get() == SeedOutcome::Pending {
                previous.outcome.set(SeedOutcome::Replaced);
            }
        }
        self.previous_legacy.borrow_mut().take();
        if self.outcome.get() == SeedOutcome::Pending {
            self.outcome.set(SeedOutcome::Retired);
        }
        self.completed.set(true);
        Ok(())
    }

    /// Record native evidence without altering the displaced seed.
    pub fn retire_closed(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created) {
            return Err(
                "the native close receipt does not match the generated Markdown seed".into(),
            );
        }
        match self.outcome.get() {
            SeedOutcome::Consumed | SeedOutcome::Retired => return Ok(()),
            SeedOutcome::Replaced => return Err("the generated Markdown seed was replaced".into()),
            SeedOutcome::Pending => {}
        }
        match self.seeds.get(self.view) {
            Some(current) if Rc::ptr_eq(&current.identity, &self.identity) => {
                self.close_proven.set(true);
                Ok(())
            }
            Some(_) => Err("the generated Markdown seed was replaced".into()),
            None => Err("the generated Markdown seed is no longer available".into()),
        }
    }

    /// Retire an adopted target without restoring the displaced seed.
    pub fn retire_adopted(
        &self,
        proof: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<(), String> {
        if !proof.matches(&self.reservation, self.view, &self.created) {
            return Err(
                "the native close receipt does not match the generated Markdown seed".into(),
            );
        }
        match self.outcome.get() {
            SeedOutcome::Consumed | SeedOutcome::Retired => return Ok(()),
            SeedOutcome::Replaced => return Err("the generated Markdown seed was replaced".into()),
            SeedOutcome::Pending => {}
        }
        match self.seeds.get(self.view) {
            Some(current) if Rc::ptr_eq(&current.identity, &self.identity) => {
                let removed = self.seeds.remove(self.view);
                self.outcome.set(SeedOutcome::Retired);
                if let Some(previous) = self.previous.borrow_mut().take() {
                    if previous.outcome.get() == SeedOutcome::Pending {
                        previous.outcome.set(SeedOutcome::Replaced);
                    }
                }
                self.previous_legacy.borrow_mut().take();
                self.completed.set(true);
                drop(removed);
                Ok(())
            }
            Some(_) => Err("the generated Markdown seed was replaced".into()),
            None => Err("the generated Markdown seed is no longer available".into()),
        }
    }

    pub fn rollback(&self) -> Result<(), String> {
        if self.completed.get() {
            return Ok(());
        }
        match self.outcome.get() {
            SeedOutcome::Consumed | SeedOutcome::Retired => {
                self.completed.set(true);
                return Ok(());
            }
            SeedOutcome::Replaced => return Err("the generated Markdown seed was replaced".into()),
            SeedOutcome::Pending => {}
        }
        match self.seeds.get(self.view) {
            Some(current) if Rc::ptr_eq(&current.identity, &self.identity) => {
                if !current.created.is_active() && !self.close_proven.get() {
                    return Err(
                        "the generated Markdown seed lifetime ended without a close receipt".into(),
                    );
                }
                let removed = self.seeds.remove(self.view);
                if let Some(previous) = self.previous.borrow_mut().take() {
                    self.seeds.insert(self.view, previous);
                }
                if let Some(previous) = self.previous_legacy.borrow_mut().take() {
                    self.legacy_seeds.insert(self.view, previous);
                }
                self.outcome.set(SeedOutcome::Retired);
                drop(removed);
                self.completed.set(true);
                Ok(())
            }
            Some(_) => Err("the generated Markdown seed was replaced".into()),
            None => Err("the generated Markdown seed is no longer available".into()),
        }
    }
}

impl Drop for MarkdownSeedRollback {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.get_mut().take() {
            if previous.outcome.get() == SeedOutcome::Pending {
                previous.outcome.set(SeedOutcome::Replaced);
            }
        }
    }
}

text_document_editors!(MarkdownEditors, shell_contract::Surface::Markdown);

/// `samePath(a, b)`: case-folded where the file system usually is
fn same_path(a: &str, b: &str) -> bool {
    let key = |p: &str| {
        let full = resolve(&[p]);
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            full.to_lowercase()
        } else {
            full
        }
    };
    key(a) == key(b)
}

impl MarkdownEditors {
    /// `queueMarkdownSeedFor(contents, text)`: text a new untitled tab starts with
    pub fn queue_seed(&self, view: ViewId, text: &str) {
        if !self.docs.is_destroyed(view) {
            let previous = self.owned_seeds.remove(view);
            if let Some(seed) = &previous {
                seed.outcome.set(SeedOutcome::Replaced);
            }
            self.seeds.insert(view, text.to_owned());
            drop(previous);
        }
    }

    pub fn prepare_seed_in_transition(
        &self,
        view: ViewId,
        text: String,
        source: shell_contract::ipc::NativeRequestOwner,
        created: shell_contract::runtime::RendererGeneration,
        reservation: shell_contract::ipc::NativeClosureReservation,
        browser: shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<PreparedMarkdownSeed, String> {
        if !reservation.matches(view, &created)
            || !browser.reservation().same_reservation(&reservation)
        {
            return Err("the generated Markdown reservation does not match its target".into());
        }
        let effects = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("owned document effects are unavailable")?;
        let identity = Rc::new(());
        let outcome = Rc::new(Cell::new(SeedOutcome::Pending));
        let seed = OwnedSeed {
            text,
            _source: source,
            created: created.clone(),
            effects,
            identity: identity.clone(),
            outcome: outcome.clone(),
        };
        let count = self
            .owned_seeds
            .prepared
            .get()
            .checked_add(1)
            .ok_or("the generated Markdown seed capacity is exhausted")?;
        self.owned_seeds
            .map
            .borrow_mut()
            .try_reserve(count)
            .map_err(|error| error.to_string())?;
        let rollback = Rc::new(MarkdownSeedRollback {
            seeds: self.owned_seeds.clone(),
            view,
            created,
            reservation,
            browser: Some(browser.clone()),
            committed: Cell::new(false),
            identity,
            outcome,
            previous: RefCell::new(None),
            legacy_seeds: self.seeds.clone(),
            previous_legacy: RefCell::new(None),
            close_proven: Cell::new(false),
            completed: Cell::new(false),
        });
        self.owned_seeds.prepared.set(count);
        Ok(PreparedMarkdownSeed {
            seeds: self.owned_seeds.clone(),
            legacy_seeds: self.seeds.clone(),
            browser,
            seed: Some(seed),
            rollback: Some(rollback),
        })
    }

    /// Generated content belongs to the exact native view created for that request.
    pub fn queue_seed_owned(
        &self,
        view: ViewId,
        text: String,
        source: shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        created: shell_contract::runtime::RendererGeneration,
        reservation: shell_contract::ipc::NativeClosureReservation,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<MarkdownSeedRollback, String> {
        if !reservation.matches(view, &created) {
            return Err("the generated Markdown reservation does not match its target".into());
        }
        let effects = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
            .ok_or("owned document effects are unavailable")?;
        self.owned_seeds
            .map
            .borrow_mut()
            .try_reserve(self.owned_seeds.prepared.get().saturating_add(1))
            .map_err(|error| error.to_string())?;
        let identity = Rc::new(());
        let outcome = Rc::new(Cell::new(SeedOutcome::Pending));
        let seed = OwnedSeed {
            text,
            _source: source.clone(),
            created: created.clone(),
            effects: effects.clone(),
            identity: identity.clone(),
            outcome: outcome.clone(),
        };
        let mut seed = Some(seed);
        let mut previous = None;
        let mut previous_legacy = None;
        effects.with_created_many_effect(
            &source,
            participants,
            &created,
            &cancel,
            &pending,
            &mut || {
                let seed = seed
                    .take()
                    .ok_or("generated seed was admitted more than once")?;
                initialize()?;
                previous_legacy = self.seeds.remove(view);
                previous = self.owned_seeds.insert(view, seed);
                Ok(())
            },
        )?;
        Ok(MarkdownSeedRollback {
            seeds: self.owned_seeds.clone(),
            view,
            created,
            reservation,
            browser: None,
            committed: Cell::new(true),
            identity,
            outcome,
            previous: RefCell::new(previous),
            legacy_seeds: self.seeds.clone(),
            previous_legacy: RefCell::new(previous_legacy),
            close_proven: Cell::new(false),
            completed: Cell::new(false),
        })
    }

    /// `queueMarkdownNoteAppendFor(contents, notePath, text)`: text for the
    /// note the view shows (the PDF editor's "Add to note"); the renderer
    /// takes it after `markdown:note-append-ready`
    pub fn queue_note_append(&self, view: ViewId, note_path: &str, text: &str) {
        if self.docs.is_destroyed(view) {
            return;
        }
        let queued = match self.notes.remove(view) {
            Some((path, mut texts)) if same_path(&path, note_path) => {
                texts.push(text.to_owned());
                (path, texts)
            }
            _ => (note_path.to_owned(), vec![text.to_owned()]),
        };
        self.notes.insert(view, queued);
        self.services
            .events
            .emit(Target::View(view), "markdown:note-append-ready", Vec::new());
    }

    /// `exportMarkdownPdfHeadless(input, outPath, timeoutMs)` on the hidden
    /// view the shell made for it
    pub fn export_pdf_headless(
        &self,
        view: ViewId,
        input: &str,
        out_path: &str,
        timeout: Duration,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.docs
            .headless_export(view, input, out_path.into_ipc(), timeout, then)
    }

    pub fn export_pdf_headless_invocation(
        &self,
        view: ViewId,
        input: &str,
        out_path: &str,
        timeout: Duration,
        invocation: shell_contract::ipc::NativeInvocationTarget,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.docs.headless_export_invocation(
            view,
            input,
            out_path.into_ipc(),
            timeout,
            invocation,
            then,
        )
    }
}

const CHANNELS: DocChannels = DocChannels {
    app: "markdown",
    untitled_ext: ".md",
    read_file: "markdown:read-file",
    dirty_changed: "markdown:dirty-changed",
    close_save_result: "markdown:close-save-result",
    close_save_request: "markdown:close-save-request",
    save_request: "markdown:save-request",
    save_request_ack: "markdown:save-request-ack",
    read_text_request: "markdown:read-text-request",
    read_text_result: "markdown:read-text-result",
    file_renamed: "markdown:file-renamed",
    export_request: "markdown:export-request",
    recovery_flush_request: "markdown:recovery-flush-request",
    recovery_open: "markdown:recovery-open",
    recovery_write: "markdown:recovery-write",
    recovery_applied: "markdown:recovery-applied",
    consume_headless_export: "markdown:consume-headless-export",
    headless_export_done: "markdown:headless-export-done",
};

/// `MAX_NOTES` of `wiki-notes.ts`
const MAX_NOTES: f64 = 2000.0;

/// a serializable answer as the renderer receives it
pub(crate) fn to_ipc(value: &impl Serialize) -> IpcValue {
    serde_json::to_value(value).map_or(IpcValue::Null, |v| kit::from_json(&v))
}

/// The wiki index of `wiki-index.ts` (module state, one index per root):
/// its builds run on workers one at a time, as the TypeScript's promise
/// chain ran them
type SharedIndex = Arc<Mutex<WikiIndex>>;

pub(crate) fn entries(
    services: &Services,
    provided: &Rc<Provided>,
) -> Result<Vec<Entry>, RegisterError> {
    let shell = services.hosts.need::<MarkdownShellHost>(Batch::D2)?;
    let engine = services.hosts.need::<MarkdownEngineHost>(Batch::D2)?;
    let os = services.hosts.need::<OsShellHost>(Batch::D2)?;
    let (s1, s2, s3, s4, s5) = (
        shell.clone(),
        shell.clone(),
        shell.clone(),
        shell.clone(),
        shell.clone(),
    );
    let assets = shell.clone();
    let owned_assets = shell.clone();
    let native_assets = shell.clone();
    let saved_assets = shell.clone();
    let recovery_assets = shell.clone();
    let rename_recovery_assets = shell.clone();
    let docs = TextDocs::new(
        services,
        CHANNELS,
        DocHooks {
            lang: Box::new(move || s1.ui_lang()),
            is_headless: Box::new(move || s2.is_headless()),
            reload: Box::new(move |v| s3.reload(v)),
            now_ms: Box::new(move || s5.now_ms()),
            open_elsewhere: Box::new(move |p| s4.is_path_open_elsewhere(p)),
            discard_pending_assets: |path| discard_pending_owned_assets(path).errors,
            rename_assets: |old, new| rename_owned_asset_document(old, new).map_err(|e| e.message),
            asset_paths: Box::new(move |view, tracked, open, save| {
                assets.asset_paths_changed(view, tracked, open, save)
            }),
            asset_paths_owned: Box::new(move |owner, tracked, open, save| {
                owned_assets.asset_paths_changed_owned(
                    owner.origin().ok_or("the save has no renderer origin")?,
                    &owner.captured_participants(),
                    &owner.cancel(),
                    &owner.pending(),
                    &owner.callback_cancels(),
                    owner.effects(),
                    tracked,
                    open,
                    save,
                )
            }),
            native_save_asset_paths: Arc::new(move |binding, tracked, open, save| {
                native_assets.prepare_native_save_asset_paths(binding, tracked, open, save)
            }),
            saved_asset_paths: Arc::new(move |binding, open, save| {
                saved_assets.prepare_saved_document_asset_paths(binding, open, save)
            }),
            recovery_asset_paths: Box::new(move |target, tracked, open, save| {
                recovery_assets.prepare_recovery_asset_paths(target, tracked, open, save)
            }),
            rename_recovery_asset_paths: Arc::new(move |scope, tracked, open, save| {
                rename_recovery_assets.prepare_rename_asset_paths_recovery(
                    &scope.receipt,
                    &scope.receiver,
                    &scope.destination,
                    &scope.participants,
                    &scope.cancel,
                    &scope.pending,
                    tracked,
                    open,
                    save,
                )
            }),
            presenting: Box::new(|_| false),
        },
        ("Untitled", "제목 없음"),
    )?;
    let editors = Rc::new(MarkdownEditors {
        docs: Rc::clone(&docs),
        seeds: ViewMap::new(services),
        owned_seeds: OwnedSeedMap::new(services),
        notes: ViewMap::new(services),
        services: services.clone(),
    });
    provided.add::<dyn DocumentEditors>(editors.clone());
    provided
        .add::<dyn main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract<
                ViewId,
                shell_contract::ipc::RecoveryOpenTarget,
            >>(editors.clone());
    // the app's own calls (seed, note append, headless export, tracking) for the shell
    provided.add::<MarkdownEditors>(editors.clone());

    let index: SharedIndex = Arc::new(Mutex::new(WikiIndex::new()));
    let fragments: Rc<RefCell<WikiFragments>> = Rc::default();
    let mut out = docs.entries();

    let (e, ix, sv) = (Rc::clone(&editors), Arc::clone(&index), services.clone());
    out.push(kit::invoke_later(
        "markdown:consume-pending",
        move |cx, _, responder| {
            let responder = match e.docs.consume_saved(cx, responder) {
                Ok(()) => return,
                Err(responder) => responder,
            };
            let pending = e.docs.open_path(cx.sender.view);
            // the vault root is known from here on: start the index so the
            // backlinks panel and the graph usually find it already built
            if pending.is_some()
                && let Ok(root) = e.docs.default_save_dir()
            {
                let ix = Arc::clone(&ix);
                kit::on_worker(
                    &sv,
                    move || {
                        let mut fs = node_vault_fs(&now_ms);
                        lock(&ix).warm_wiki_index(&root, &mut fs)
                    },
                    |()| {},
                );
            }
            responder.reply(Ok(pending.into_ipc()))
        },
    ));
    let e = Rc::clone(&editors);
    out.push(kit::invoke(
        "markdown:saved-input-applied",
        move |cx, args| {
            let result = args.get(0);
            let text = |key: &str| {
                kit::as_str(kit::field(result, key))
                    .unwrap_or_default()
                    .to_owned()
            };
            e.docs
                .saved_input_applied(cx, &text("token"), &text("outcome"), &text("error"))?;
            Ok(IpcValue::Undefined)
        },
    ));
    let e = Rc::clone(&editors);
    out.push(kit::invoke("markdown:seed-take", move |cx, _| {
        let view = cx.sender.view;
        if e.docs.open_path(view).is_some() {
            return Ok(IpcValue::Null);
        }
        if let Some(seed) = e.owned_seeds.get(view) {
            let matches = cx
                .native
                .as_ref()
                .and_then(shell_contract::ipc::NativeContext::renderer)
                .is_some_and(|renderer| renderer.same_generation(&seed.created));
            if !matches {
                return Ok(IpcValue::Null);
            }
            let Some(native) = cx.native.clone() else {
                return Ok(IpcValue::Null);
            };
            let destination = shell_contract::ipc::NativeRequestOwner {
                view,
                context: native,
            };
            let mut text = None;
            let admitted = seed.effects.with_owned_effect(
                &destination,
                &destination,
                &cx.cancel,
                &cx.cancel,
                &mut || {
                    if !e
                        .owned_seeds
                        .get(view)
                        .is_some_and(|current| Rc::ptr_eq(&current.identity, &seed.identity))
                    {
                        return Err("the generated Markdown seed was replaced".into());
                    }
                    if let Some(current) = e.owned_seeds.remove(view) {
                        current.outcome.set(SeedOutcome::Consumed);
                        text = Some(current.text);
                    }
                    Ok(())
                },
            );
            if admitted.is_err() {
                return Ok(IpcValue::Null);
            }
            return Ok(text.into_ipc());
        }
        Ok(e.seeds.remove(view).into_ipc())
    }));
    let e = Rc::clone(&editors);
    out.push(kit::invoke("markdown:note-append-take", move |cx, _| {
        let view = cx.sender.view;
        let document = e.docs.save_path(view);
        let matches = match (e.notes.get(view), &document) {
            (Some((path, _)), Some(doc)) => same_path(&path, doc),
            _ => false,
        };
        if !matches {
            return Ok(IpcValue::Array(Vec::new()));
        }
        let texts = e.notes.remove(view).map(|(_, t)| t).unwrap_or_default();
        Ok(texts.into_ipc())
    }));

    let (e, sv) = (Rc::clone(&editors), services.clone());
    out.push(kit::invoke_later(
        "markdown:resolve-wiki-links",
        move |_, args, responder| {
            let request = args.get(0);
            let from_path = kit::as_str(field(request, "fromPath"))
                .unwrap_or("")
                .to_owned();
            let targets: Vec<String> = match field(request, "targets") {
                IpcValue::Array(items) => items
                    .iter()
                    .filter_map(|t| kit::as_str(t).map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            };
            let root = match e.docs.default_save_dir() {
                Ok(root) => root,
                Err(error) => return responder.reply(Err(error)),
            };
            kit::on_worker(
                &sv,
                move || {
                    let mut fs = node_fs(&now_ms);
                    let resolved = resolve_wiki_links(
                        &targets,
                        &ResolveOptions {
                            from_path: &from_path,
                            root: &root,
                        },
                        &mut fs,
                    );
                    // a Map arrives as an empty object: ship the shape the renderer asked for
                    IpcValue::Object(
                        resolved
                            .iter()
                            .map(|(target, link)| {
                                (
                                    target.to_owned(),
                                    obj([
                                        ("path", link.path.as_str().into_ipc()),
                                        ("fragment", link.fragment.as_str().into_ipc()),
                                        ("found", link.found.into_ipc()),
                                    ]),
                                )
                            })
                            .collect(),
                    )
                },
                move |resolved| responder.reply(Ok(resolved)),
            );
        },
    ));
    let fr = Rc::clone(&fragments);
    out.push(kit::invoke(
        "markdown:remember-wiki-fragment",
        move |_, args| {
            if let (Some(path), Some(fragment)) = (args.str(0), args.str(1)) {
                fr.borrow_mut().remember_wiki_fragment(path, fragment);
            }
            Ok(IpcValue::Undefined)
        },
    ));
    let fr = Rc::clone(&fragments);
    out.push(kit::invoke(
        "markdown:take-wiki-fragment",
        move |_, args| {
            Ok(match args.str(0) {
                Some(path) => fr.borrow_mut().take_wiki_fragment(path).into_ipc(),
                None => "".into_ipc(),
            })
        },
    ));
    let (e, sv) = (Rc::clone(&editors), services.clone());
    out.push(kit::invoke_later(
        "markdown:list-wiki-notes",
        move |_, _, responder| {
            let root = match e.docs.default_save_dir() {
                Ok(root) => root,
                Err(error) => return responder.reply(Err(error)),
            };
            kit::on_worker(
                &sv,
                move || to_ipc(&list_wiki_notes(&root, &mut node_fs(&now_ms), MAX_NOTES)),
                move |notes| responder.reply(Ok(notes)),
            );
        },
    ));
    let (e, sv) = (Rc::clone(&editors), services.clone());
    out.push(kit::invoke_later(
        "markdown:wiki-heads",
        move |_, args, responder| {
            let request = args.get(0);
            let target = kit::as_str(field(request, "target"))
                .unwrap_or("")
                .to_owned();
            let from_path = kit::as_str(field(request, "fromPath"))
                .unwrap_or("")
                .to_owned();
            let root = match e.docs.default_save_dir() {
                Ok(root) => root,
                Err(error) => return responder.reply(Err(error)),
            };
            kit::on_worker(
                &sv,
                move || {
                    to_ipc(&wiki_heads(
                        &target,
                        &from_path,
                        &root,
                        &mut node_fs(&now_ms),
                    ))
                },
                move |heads| responder.reply(Ok(heads)),
            );
        },
    ));
    let (e, sv) = (Rc::clone(&editors), services.clone());
    out.push(kit::invoke_later(
        "markdown:create-wiki-note",
        move |cx, args, responder| {
            let view = cx.sender.view;
            let target = kit::as_str(field(args.get(0), "target"))
                .unwrap_or("")
                .to_owned();
            if target.is_empty() {
                return responder.reply(Ok(obj([("ok", false.into_ipc())])));
            }
            // where a bare name lands follows the document this view holds: the
            // saved path wins (Save As moved it), the opened path is the fallback
            let from_path = e
                .docs
                .save_path(view)
                .or_else(|| e.docs.open_path(view))
                .unwrap_or_default();
            let root = match e.docs.default_save_dir() {
                Ok(root) => root,
                Err(error) => return responder.reply(Err(error)),
            };
            kit::on_worker(
                &sv,
                move || {
                    let from_dir = if from_path.is_empty() {
                        String::new()
                    } else {
                        dirname(&from_path)
                    };
                    let made = create_wiki_note(
                        &target,
                        &CreateNoteOptions {
                            root: &root,
                            from_dir: &from_dir,
                        },
                    );
                    match made {
                        Ok(Some(note)) => match to_ipc(&note) {
                            IpcValue::Object(mut fields) => {
                                fields.insert(0, ("ok".to_owned(), true.into_ipc()));
                                IpcValue::Object(fields)
                            }
                            other => other,
                        },
                        _ => obj([("ok", false.into_ipc())]),
                    }
                },
                move |made| responder.reply(Ok(made)),
            );
        },
    ));
    for (channel, op) in [
        ("markdown:wiki-backlinks", WikiOp::Backlinks),
        ("markdown:wiki-graph", WikiOp::Graph),
        ("markdown:wiki-tags", WikiOp::Tags),
    ] {
        let (e, ix, sv) = (Rc::clone(&editors), Arc::clone(&index), services.clone());
        out.push(kit::invoke_later(channel, move |_, args, responder| {
            let path = args.str(0).map(str::to_owned);
            if op == WikiOp::Backlinks && path.is_none() {
                return responder.reply(Ok(IpcValue::Array(Vec::new())));
            }
            let root = match e.docs.default_save_dir() {
                Ok(root) => root,
                Err(error) => return responder.reply(Err(error)),
            };
            let ix = Arc::clone(&ix);
            kit::on_worker(
                &sv,
                move || {
                    let mut fs = node_vault_fs(&now_ms);
                    let mut index = lock(&ix);
                    match op {
                        WikiOp::Backlinks => to_ipc(&index.wiki_backlinks(
                            &root,
                            path.as_deref().unwrap_or_default(),
                            &mut fs,
                        )),
                        WikiOp::Graph => to_ipc(&index.wiki_graph(&root, &mut fs)),
                        WikiOp::Tags => to_ipc(&index.wiki_tags(&root, &mut fs)),
                    }
                },
                move |answer| responder.reply(Ok(answer)),
            );
        }));
    }
    out.extend(save::entries(services, &editors, engine, os, shell)?);
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WikiOp {
    Backlinks,
    Graph,
    Tags,
}

/// the index, also after a build that panicked on another worker
fn lock(index: &SharedIndex) -> std::sync::MutexGuard<'_, WikiIndex> {
    index.lock().unwrap_or_else(|e| e.into_inner())
}

/// This app's host fakes, answering from `case` (`hosts.d2.markdown`): see [`fake::FakeMarkdown`]
#[cfg(any(test, feature = "probe"))]
pub(crate) fn fakes(hosts: &mut shell_contract::Hosts, case: &serde_json::Value) {
    let probe = hosts.get::<crate::html::fake::ProbeLog>().cloned();
    let os = hosts
        .get::<OsShellHost>()
        .expect("fake OS shell is installed")
        .clone();
    let fake = Arc::new(fake::FakeMarkdown::new(case, probe, os));
    hosts.insert::<MarkdownEngineHost>(fake.clone());
    hosts.insert::<MarkdownShellHost>(fake.clone());
    hosts.insert::<Arc<fake::FakeMarkdown>>(fake);
}

/// tools/ipc-diff's `trigger` steps for this editor (`d2/markdown:<what>`):
/// the shell's and the other areas' calls into it. The TypeScript side of
/// groups/d2/markdown.ts maps the same names to `markdown-main.ts`'s and
/// `note-append.ts`'s exports. False for a name this editor does not know
/// (`then` is dropped).
#[cfg(any(test, feature = "probe"))]
pub(crate) fn probe_trigger(
    what: &str,
    view: ViewId,
    args: &serde_json::Value,
    _services: &Services,
    provided: &Rc<Provided>,
    then: shell_contract::editors::Then<IpcValue>,
) -> bool {
    let Some(editors) = provided.all::<MarkdownEditors>().into_iter().next() else {
        return false;
    };
    crate::html::text_doc::probe_trigger_begun(what, view, args, then, |what, then| {
        run_trigger(&editors, what, view, args, then)
    })
}

#[cfg(any(test, feature = "probe"))]
fn run_trigger(
    editors: &MarkdownEditors,
    what: &str,
    view: ViewId,
    args: &serde_json::Value,
    then: shell_contract::editors::Then<IpcValue>,
) -> bool {
    let arg = |i: usize| args.get(i).and_then(serde_json::Value::as_str);
    match what {
        // `queueMarkdownSeedFor(contents, text)`
        "queue-seed" => {
            editors.queue_seed(view, arg(0).unwrap_or_default());
            then(Ok(IpcValue::Undefined))
        }
        // `queueMarkdownNoteAppendFor(contents, notePath, text)`
        "queue-note-append" => {
            editors.queue_note_append(view, arg(0).unwrap_or_default(), arg(1).unwrap_or_default());
            then(Ok(IpcValue::Undefined))
        }
        // `exportMarkdownPdfHeadless(input, outPath, timeoutMs)`
        "export-pdf-headless" => {
            let timeout = args
                .get(2)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(180_000);
            editors.export_pdf_headless(
                view,
                arg(0).unwrap_or_default(),
                arg(1).unwrap_or_default(),
                Duration::from_millis(timeout),
                Box::new(|r| then(r.map(|()| IpcValue::Undefined).map_err(kit::error))),
            )
        }
        _ => {
            return crate::html::text_doc::probe_trigger(&editors.docs, what, view, args, then);
        }
    }
    true
}
