//! The windows and views the shell has (the stage 3 shell contract, sections
//! 1 and 2): which Tauri webview is which `ViewId`, in which window, loaded
//! with which preload. Every message from a renderer is attributed through
//! this table, by the label of the webview that sent it: a page cannot name
//! its own sender.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::native::{
    BrowserId, BrowserState, DocumentContext, DocumentIdentity, DocumentRevocation, ReloadIdentity,
    ReloadReservation, SchemeRequestContext, WindowClose, WindowCloseOutcome,
    WindowCloseSubscription, WindowClosingSubscription, WindowEffect,
};
use main_core::docs::byte_handoff::{MediaPermit, MediaViewLease};
use shell_contract::Surface;
use shell_contract::ids::{ViewId, WindowId};
use shell_contract::ipc::Sender;
use shell_contract::runtime::{RendererGeneration, Thread, Views};
use tauri::Manager;

use crate::ipc::outlet::TauriOutlet;

struct Record {
    sender: Sender,
    /// the Tauri webview label
    label: String,
    reserved: NativeReservedViewState,
    docs_media: Option<MediaViewLease>,
    docs_browser: Option<(BrowserId, BrowserState)>,
    docs_asset: Option<crate::host::document_assets::AssetSession>,
    slides_media: Option<SlidesViewLease>,
    slides_browser: Option<(BrowserId, BrowserState)>,
    asset_view: Option<AssetViewLease>,
    asset_browser: Option<(BrowserId, BrowserState)>,
    native_origin: Option<BoundNativeOrigin>,
    renderer: RendererGeneration,
    creation: RendererGeneration,
    native_generation_used: bool,
    native_browser: Option<(BrowserId, BrowserState)>,
    native_reload: Option<ReservedNativeReload>,
    closure_proof: Option<crate::host::window::NativeClosureProof>,
    teardown: Arc<ViewRemovalDelivery>,
    browser_owner: Arc<Mutex<Option<shell_contract::ipc::NativeBrowserOwner>>>,
}

/// Captured from the exact record before the native transition begins.
pub(crate) struct PreparedBrowserOwner {
    slot: Arc<Mutex<Option<shell_contract::ipc::NativeBrowserOwner>>>,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
}

/// The slot captured with the authenticated document, before entering owner gates.
pub(crate) struct CapturedBrowserOwner {
    slot: Arc<Mutex<Option<shell_contract::ipc::NativeBrowserOwner>>>,
    owner: shell_contract::ipc::NativeBrowserOwner,
}

impl CapturedBrowserOwner {
    /// Call only inside this owner's reservation transition.
    pub(crate) fn matches(
        &self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
    ) -> bool {
        transition.matches_owner(&self.owner)
            && self
                .slot
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(|owner| owner.same_owner(&self.owner)))
                .unwrap_or(false)
    }
}

/// The native reservation serializes the validated slot until this guard commits.
pub(crate) struct AdmittedBrowserOwner<'a> {
    prepared: &'a PreparedBrowserOwner,
    _transition: &'a shell_contract::ipc::NativeBrowserTransition<'a>,
}

impl PreparedBrowserOwner {
    pub(crate) fn validate<'a>(
        &'a self,
        transition: &'a shell_contract::ipc::NativeBrowserTransition<'a>,
    ) -> Result<AdmittedBrowserOwner<'a>, String> {
        if !transition.matches(&self.reservation) {
            return Err("The browser reservation changed".into());
        }
        let current = self.slot.lock().unwrap_or_else(|error| error.into_inner());
        match current.as_ref() {
            None if transition.matches_owner(&self.owner) => {}
            Some(previous)
                if transition.matches_owner(previous) && transition.proposes_owner(&self.owner) => {
            }
            _ => {
                return Err(
                    "The browser owner transition does not match the captured owner".into(),
                );
            }
        }
        drop(current);
        Ok(AdmittedBrowserOwner {
            prepared: self,
            _transition: transition,
        })
    }
}

impl AdmittedBrowserOwner<'_> {
    pub(crate) fn commit(self) {
        *self
            .prepared
            .slot
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(self.prepared.owner.clone());
    }
}

struct ReservedNativeReload {
    identity: ReloadIdentity,
    renderer: RendererGeneration,
}

/// Captured with a verified origin; never resolved again by view label while
/// holding a renderer or native-context effect gate.
#[derive(Clone)]
pub(crate) struct NativeReservedViewState(Arc<Mutex<ReservedViewState>>);

struct ReservedViewState {
    zoom: f64,
    dropped: Vec<PathBuf>,
}

impl NativeReservedViewState {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(ReservedViewState {
            zoom: 1.0,
            dropped: Vec::new(),
        })))
    }

    pub(crate) fn zoom(&self) -> f64 {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .zoom
    }

    pub(crate) fn set_zoom(&self, factor: f64) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .zoom = factor;
    }

    pub(crate) fn dropped(&self) -> Vec<PathBuf> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .dropped
            .clone()
    }
}

/// Native request evidence plus the exact revocable document capability.
#[derive(Clone)]
pub struct DocsMediaAuthorization {
    pub view: ViewId,
    pub permit: MediaPermit,
    browser: BrowserState,
    document: DocumentIdentity,
}

impl DocsMediaAuthorization {
    pub fn is_current(&self) -> bool {
        self.permit.is_active() && self.browser.document().as_ref() == Some(&self.document)
    }
}

/// Identity of one Slides view lifetime, independent of its numeric ViewId.
#[derive(Clone)]
pub struct SlidesViewLease(RendererGeneration);

impl SlidesViewLease {
    pub(crate) fn new() -> Self {
        Self(RendererGeneration::new())
    }
    pub(crate) fn revoke(&self) {
        self.0.revoke();
    }
    pub fn is_active(&self) -> bool {
        self.0.is_active()
    }
    pub fn same_lifetime(&self, other: &Self) -> bool {
        self.0.same_generation(&other.0)
    }
    pub fn loss_token(&self) -> shell_contract::runtime::CancelToken {
        self.0.loss_token()
    }
    pub fn with_lease_active_pair<T>(
        leases: [Option<&Self>; 2],
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        RendererGeneration::with_active_all(leases.map(|lease| lease.map(|lease| &lease.0)), action)
    }
}

#[derive(Clone)]
pub struct SlidesMediaAuthorization {
    pub view: ViewId,
    lease: SlidesViewLease,
    browser_id: BrowserId,
    browser: BrowserState,
    document: DocumentIdentity,
}

impl SlidesMediaAuthorization {
    pub fn lease(&self) -> SlidesViewLease {
        self.lease.clone()
    }

    /// The authorization of a slides view's registered document: its lease, and the browser
    /// and committed document `token` belongs to. It is current only while that document is
    /// still the browser's committed one.
    pub(crate) fn of_document(view: ViewId, lease: SlidesViewLease, token: &DocumentContext) -> Self {
        Self {
            view,
            lease,
            browser_id: token.identity().browser_id(),
            browser: token.browser().clone(),
            document: token.identity().clone(),
        }
    }

    pub fn is_current(&self) -> bool {
        self.lease.is_active() && self.browser.document().as_ref() == Some(&self.document)
    }

    /// Compare the captured document identity, including after it becomes stale.
    /// This does not establish authorization or freshness.
    pub fn same_document_identity(&self, other: &Self) -> bool {
        self.view == other.view
            && self.lease.same_lifetime(&other.lease)
            && self.browser_id == other.browser_id
            && self.browser.is_same_browser(&other.browser)
            && self.document == other.document
    }

    pub fn same_document(&self, other: &Self) -> bool {
        self.is_current() && other.is_current() && self.same_document_identity(other)
    }

    pub fn matches_native_snapshot(
        &self,
        browser_id: Option<BrowserId>,
        browser: &BrowserState,
        document: Option<&DocumentIdentity>,
    ) -> bool {
        self.is_current()
            && browser_id == Some(self.browser_id)
            && self.browser.is_same_browser(browser)
            && document == Some(&self.document)
    }
}

#[derive(Clone)]
pub struct AssetViewLease(Arc<AssetViewState>);

struct AssetViewState {
    live: Mutex<bool>,
    loss: shell_contract::runtime::CancelToken,
}

impl AssetViewLease {
    fn new() -> Self {
        Self(Arc::new(AssetViewState {
            live: Mutex::new(true),
            loss: shell_contract::runtime::CancelToken::default(),
        }))
    }
    fn is_live(&self) -> bool {
        self.0.live.lock().is_ok_and(|live| *live)
    }
    fn revoke(&self) {
        {
            let mut live = self
                .0
                .live
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *live = false;
        }
        self.0.loss.cancel();
    }

    fn with_live_pair<T>(&self, other: &Self, action: impl FnOnce() -> T) -> Option<T> {
        if Arc::ptr_eq(&self.0, &other.0) {
            let live = self.0.live.lock().ok()?;
            return (*live).then(action);
        }
        let (first, second) = if Arc::as_ptr(&self.0) < Arc::as_ptr(&other.0) {
            (self, other)
        } else {
            (other, self)
        };
        let first_live = first.0.live.lock().ok()?;
        let second_live = second.0.live.lock().ok()?;
        if !*first_live || !*second_live {
            return None;
        }
        Some(action())
    }
}

struct BoundNativeOrigin {
    context: DocumentContext,
    renderer: RendererGeneration,
    lost: Arc<AtomicBool>,
    _loss_subscription: DocumentRevocation,
}

#[derive(Clone)]
pub struct NativeAssetAuthorization {
    pub view: ViewId,
    pub surface: Surface,
    lease: AssetViewLease,
    browser_id: BrowserId,
    browser: BrowserState,
    document: DocumentContext,
}

pub(crate) struct CapturedNativeOwner {
    pub origin: DocumentContext,
    pub renderer: RendererGeneration,
    pub surface: Surface,
    pub asset: Option<NativeAssetAuthorization>,
    pub slides: Option<SlidesViewLease>,
    pub docs: Option<MediaViewLease>,
    pub browser_owner: Option<shell_contract::ipc::NativeBrowserOwner>,
}

/// A candidate webview is accepted only when its native creation reservation
/// is the one held by the captured table record.
pub(crate) struct CapturedClosingView {
    pub(crate) webview: tauri::Webview,
    pub(crate) creation: RendererGeneration,
    pub(crate) proof: crate::host::window::NativeClosureProof,
    pub(crate) owner: shell_contract::ipc::NativeBrowserOwner,
    pub(crate) parent: WindowId,
    pub(crate) parent_lifecycle: Arc<NativeWindowLifecycle>,
}

pub(crate) struct AssetLeaseGatePlan<'a> {
    leases: Vec<&'a AssetViewLease>,
    guards: Vec<std::sync::MutexGuard<'a, bool>>,
}

impl<'a> AssetLeaseGatePlan<'a> {
    pub(crate) fn new(assets: &[&'a NativeAssetAuthorization]) -> Self {
        let mut leases: Vec<_> = assets.iter().map(|asset| &asset.lease).collect();
        leases.sort_unstable_by_key(|lease| Arc::as_ptr(&lease.0));
        leases.dedup_by(|left, right| Arc::ptr_eq(&left.0, &right.0));
        let guards = Vec::with_capacity(leases.len());
        Self { leases, guards }
    }

    pub(crate) fn with_lease_live_fenced<T>(self, action: impl FnOnce() -> T) -> Option<T> {
        let mut guards = self.guards;
        for &lease in &self.leases {
            guards.push(lease.0.live.lock().ok()?);
        }
        if guards.iter().any(|live| !**live) {
            return None;
        }
        Some(action())
    }
}

pub(crate) struct SlidesViewLeaseGatePlan<'a>(
    shell_contract::runtime::RendererGenerationGatePlan<'a>,
);
impl<'a> SlidesViewLeaseGatePlan<'a> {
    pub(crate) fn new(leases: &[&'a SlidesViewLease]) -> Self {
        Self(
            shell_contract::runtime::RendererGenerationGatePlan::from_generations(
                leases.iter().map(|lease| &lease.0).collect(),
            ),
        )
    }
    pub(crate) fn with_active<T>(&mut self, action: impl FnOnce() -> T) -> Option<T> {
        self.0.with_active(action)
    }
}

impl NativeAssetAuthorization {
    pub fn is_current(&self) -> bool {
        self.lease.is_live() && self.document.is_registered()
    }

    /// Fires when the captured view lifetime is revoked; subscribe atomically via `on_cancel`.
    pub fn loss_token(&self) -> shell_contract::runtime::CancelToken {
        self.lease.0.loss.clone()
    }

    /// Compare the captured view lease even after it has been revoked.
    pub fn same_lease(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.lease.0, &other.lease.0)
    }

    /// Hold the view lifetime through a short admission or final commit.
    pub fn with_live<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        self.document
            .with_live(|| {
                let live = self.lease.0.live.lock().ok()?;
                if !*live {
                    return None;
                }
                Some(action())
            })
            .flatten()
    }

    /// Caller holds the exact matching native-context fence. Never reacquires it.
    pub fn with_lease_live_fenced<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let live = self.lease.0.live.lock().ok()?;
        if !*live {
            return None;
        }
        Some(action())
    }

    /// Both matching native origins are already fenced by the caller.
    pub fn with_lease_live_pair_fenced<T>(
        &self,
        other: &Self,
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        self.lease.with_live_pair(&other.lease, action)
    }

    /// Compares the captured request origin, not the browser's current frame.
    pub fn matches_native_context(&self, context: &DocumentContext) -> bool {
        self.document == *context
            && self.browser_id == context.identity().browser_id()
            && self.browser.is_same_browser(context.browser())
    }
}

pub(crate) struct NativeWindowLifecycle {
    id: WindowId,
    state: Mutex<NativeWindowLifecycleState>,
    window: std::sync::OnceLock<tauri::Window>,
    terminal_receipt: std::sync::OnceLock<WindowClose>,
    terminal_subscription: std::sync::OnceLock<WindowCloseSubscription>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeWindowTerminal {
    Destroyed,
    NotCreated,
}

struct NativeWindowLifecycleState {
    revision: u64,
    destroyed: bool,
    mutating: bool,
    valid: bool,
    terminal: Option<NativeWindowTerminal>,
    terminal_waiters: Vec<Box<dyn FnOnce(NativeWindowTerminal) + Send>>,
}

impl NativeWindowLifecycleState {
    fn advance(&mut self) {
        match self.revision.checked_add(1) {
            Some(revision) => self.revision = revision,
            None => self.valid = false,
        }
    }
}

struct NativeWindowGuard<'a> {
    state: std::sync::MutexGuard<'a, NativeWindowLifecycleState>,
    poisoned: bool,
}

impl shell_ipc_w::tabs::window_snapshot::NativeWindowGuard for NativeWindowGuard<'_> {
    fn revision(&self) -> u64 {
        self.state.revision
    }
    fn stable(&self) -> bool {
        !self.poisoned && self.state.valid && !self.state.mutating
    }
    fn destroyed(&self) -> bool {
        self.state.destroyed
    }
}

struct NativeTitleMutation<'a>(&'a NativeWindowLifecycle);

impl Drop for NativeTitleMutation<'_> {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.mutating = false;
        state.advance();
    }
}

impl NativeWindowLifecycle {
    fn new(id: WindowId) -> Arc<Self> {
        Arc::new(Self {
            id,
            state: Mutex::new(NativeWindowLifecycleState {
                revision: 0,
                destroyed: false,
                mutating: false,
                valid: true,
                terminal: None,
                terminal_waiters: Vec::new(),
            }),
            window: std::sync::OnceLock::new(),
            terminal_receipt: std::sync::OnceLock::new(),
            terminal_subscription: std::sync::OnceLock::new(),
        })
    }

    pub(crate) fn attach(self: &Arc<Self>, window: tauri::Window) -> Result<(), String> {
        self.window
            .set(window.clone())
            .map_err(|_| "The native window is already bound".to_owned())?;
        let lifecycle = Arc::downgrade(self);
        window.on_window_event(move |event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                let Some(lifecycle) = lifecycle.upgrade() else {
                    return;
                };
                let mut state = lifecycle
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state.destroyed = true;
                state.advance();
            }
        });
        Ok(())
    }
    fn finish_terminal(&self, outcome: NativeWindowTerminal) {
        let waiters = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.terminal.is_some() {
                return;
            }
            state.terminal = Some(outcome);
            state.destroyed = true;
            state.advance();
            std::mem::take(&mut state.terminal_waiters)
        };
        for waiter in waiters {
            waiter(outcome);
        }
    }

    pub(crate) fn bind_terminal(self: &Arc<Self>, receipt: WindowClose) -> Result<(), String> {
        let lifecycle = self.clone();
        let subscription = receipt.when_closed(move |outcome| {
            let terminal = match outcome {
                WindowCloseOutcome::NotCreated => NativeWindowTerminal::NotCreated,
                WindowCloseOutcome::Destroyed => NativeWindowTerminal::Destroyed,
            };
            lifecycle.finish_terminal(terminal);
        })?;
        self.terminal_receipt
            .set(receipt)
            .map_err(|_| "The native window terminal receipt is already bound".to_owned())?;
        self.terminal_subscription
            .set(subscription)
            .map_err(|_| "The native window terminal subscription is already bound".to_owned())
    }

    pub(crate) fn admit_native_effect(&self) -> Result<WindowEffect, String> {
        self.terminal_receipt
            .get()
            .ok_or("The original native window has no closure reservation")?
            .admit_effect()
            .ok_or_else(|| "The original native window is closing".into())
    }

    /// The shell accepted a close of this window: admit no further native effect and cancel
    /// the work parented to it. `CloseRequested` is cancellable, so only an accepted destroy
    /// reaches this.
    pub(crate) fn begin_close(&self) {
        if let Some(receipt) = self.terminal_receipt.get() {
            receipt.begin_close();
        }
    }

    pub(crate) fn when_closing(
        &self,
        callback: impl FnOnce() + Send + 'static,
    ) -> Result<WindowClosingSubscription, String> {
        self.terminal_receipt
            .get()
            .ok_or("The original native window has no closure reservation")?
            .when_closing(callback)
    }

    pub(crate) fn when_terminal(
        &self,
        done: Box<dyn FnOnce(NativeWindowTerminal) + Send>,
    ) -> Result<(), String> {
        let mut done = Some(done);
        let terminal = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "The native window state is unavailable")?;
            if state.terminal.is_none() {
                state.terminal_waiters.push(done.take().unwrap());
            }
            state.terminal
        };
        if let (Some(terminal), Some(done)) = (terminal, done) {
            done(terminal);
        }
        Ok(())
    }
    pub(crate) fn belongs_to(&self, id: WindowId) -> bool {
        self.id == id
    }

    pub(crate) fn window_handle(&self) -> Result<tauri::Window, String> {
        self.window
            .get()
            .cloned()
            .ok_or_else(|| "The original native window is unavailable".into())
    }

    pub(crate) fn set_title(&self, title: &str) -> Result<(), String> {
        let window = self
            .window
            .get()
            .ok_or("The original native window is unavailable")?;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "The native window state is unavailable")?;
            if !state.valid || state.destroyed || state.mutating {
                return Err("The native window is unavailable".into());
            }
            state.mutating = true;
            state.advance();
            if !state.valid {
                state.mutating = false;
                return Err("The native window revision is exhausted".into());
            }
        }
        let _mutation = NativeTitleMutation(self);
        window.set_title(title).map_err(|error| error.to_string())
    }

    fn observe_now(
        self: &Arc<Self>,
    ) -> Result<shell_ipc_w::tabs::window_snapshot::WindowObservation, String> {
        let (revision, destroyed) = {
            let state = self
                .state
                .lock()
                .map_err(|_| "The native window state is unavailable")?;
            if !state.valid || state.mutating {
                return Err("The native window title is changing".into());
            }
            (state.revision, state.destroyed)
        };
        let title = if destroyed {
            String::new()
        } else {
            self.window
                .get()
                .ok_or("The original native window is unavailable")?
                .title()
                .map_err(|error| error.to_string())?
        };
        let state = self
            .state
            .lock()
            .map_err(|_| "The native window state is unavailable")?;
        if !state.valid
            || state.mutating
            || state.revision != revision
            || state.destroyed != destroyed
        {
            return Err("The native window changed during observation".into());
        }
        let reservation =
            shell_ipc_w::tabs::window_snapshot::WindowReservation::new(self.id, self.clone());
        Ok(shell_ipc_w::tabs::window_snapshot::WindowObservation::new(
            reservation,
            revision,
            destroyed,
            title,
        ))
    }
}

impl shell_ipc_w::tabs::window_snapshot::NativeWindowProof for NativeWindowLifecycle {
    fn lock(&self) -> Box<dyn shell_ipc_w::tabs::window_snapshot::NativeWindowGuard + '_> {
        match self.state.lock() {
            Ok(state) => Box::new(NativeWindowGuard {
                state,
                poisoned: false,
            }),
            Err(error) => Box::new(NativeWindowGuard {
                state: error.into_inner(),
                poisoned: true,
            }),
        }
    }

    fn set_title(&self, title: &str) -> Result<(), String> {
        NativeWindowLifecycle::set_title(self, title)
    }

    fn observe(
        self: Arc<Self>,
        done: Box<
            dyn FnOnce(Result<shell_ipc_w::tabs::window_snapshot::WindowObservation, String>)
                + Send,
        >,
    ) {
        let Some(window) = self.window.get() else {
            return done(Err("The original native window is unavailable".into()));
        };
        let app = window.app_handle().clone();
        let completion = Arc::new(Mutex::new(Some(done)));
        let posted = completion.clone();
        if let Err(error) = app.run_on_main_thread(move || {
            let result = self.observe_now();
            if let Some(done) = posted
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                done(result);
            }
        }) && let Some(done) = completion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            done(Err(format!(
                "The native window observation could not be scheduled: {error}"
            )));
        }
    }
}

struct WindowRecord {
    label: String,
    lifecycle: Arc<NativeWindowLifecycle>,
}

#[derive(Default)]
struct Inner {
    views: BTreeMap<ViewId, Record>,
    by_label: HashMap<String, ViewId>,
    /// window id to its Tauri window label
    windows: BTreeMap<WindowId, WindowRecord>,
    /// the view of the shell window (Electron's `shellWindow`), when one was named
    main: Option<ViewId>,
}

type Listener = Arc<dyn Fn(ViewId) + Send + Sync>;
type RendererListener = Arc<dyn Fn(ViewId, RendererGeneration) + Send + Sync>;
struct NativeTerminalListener {
    active: Arc<AtomicBool>,
    callback: Box<
        dyn Fn(shell_contract::runtime::NativeViewTerminal) -> Result<(), String> + Send + Sync,
    >,
}

#[derive(Default)]
struct NativeTerminalState {
    listeners: Vec<Arc<NativeTerminalListener>>,
    pending: std::collections::VecDeque<NativeTerminalDelivery>,
    draining: bool,
}
struct NativeTerminalDelivery {
    event: shell_contract::runtime::NativeViewTerminal,
    listeners: Vec<Arc<NativeTerminalListener>>,
    next: usize,
}
struct NativeTerminalDrain(Arc<Mutex<NativeTerminalState>>);
impl Drop for NativeTerminalDrain {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.lock() {
            state.draining = false;
        }
    }
}
fn drain_native_terminal_queue(
    state: &Arc<Mutex<NativeTerminalState>>,
    core: &Arc<dyn Thread>,
) -> Result<usize, String> {
    if !core.is_current() {
        return Err("Native terminal delivery requires the core thread".into());
    }
    {
        let mut state = state
            .lock()
            .map_err(|_| "Native terminal queue is unavailable")?;
        if state.draining {
            return Err("Native terminal delivery is already running".into());
        }
        state.draining = true;
    }
    let _drain = NativeTerminalDrain(state.clone());
    let mut delivered = 0;
    loop {
        let next = {
            let mut state = state
                .lock()
                .map_err(|_| "Native terminal queue is unavailable")?;
            let Some(front) = state.pending.front() else {
                break;
            };
            if front.next == front.listeners.len() {
                state.pending.pop_front();
                delivered += 1;
                continue;
            }
            (front.listeners[front.next].clone(), front.event.clone())
        };
        if next.0.active.load(Ordering::Acquire) {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (next.0.callback)(next.1)
            })) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    return Err(
                        "Native terminal listener panicked; delivery remains pending".into(),
                    );
                }
            }
        }
        let mut state = state
            .lock()
            .map_err(|_| "Native terminal queue is unavailable")?;
        let front = state
            .pending
            .front_mut()
            .ok_or("Native terminal delivery was lost")?;
        front.next += 1;
    }
    Ok(delivered)
}
struct NativeTerminalWake {
    state: Arc<Mutex<NativeTerminalState>>,
    started: bool,
}
impl Drop for NativeTerminalWake {
    fn drop(&mut self) {
        if !self.started
            && self
                .state
                .lock()
                .is_ok_and(|state| !state.pending.is_empty())
        {
            eprintln!("native terminal core wake was dropped; delivery remains pending");
        }
    }
}
fn wake_native_terminal_queue(state: Arc<Mutex<NativeTerminalState>>, core: Arc<dyn Thread>) {
    let wake = NativeTerminalWake {
        state: state.clone(),
        started: false,
    };
    let callback_core = core.clone();
    core.post(Box::new(move || {
        let mut wake = wake;
        wake.started = true;
        if let Err(error) = drain_native_terminal_queue(&state, &callback_core) {
            eprintln!("{error}");
        }
    }));
}

pub(crate) enum TerminalViewRemoval {
    Removed(Arc<ViewRemovalDelivery>),
    Absent,
    Replaced,
}

type RemovalDone = Box<dyn FnOnce(Result<(), String>) + Send>;

#[derive(Default)]
struct ViewRemovalDeliveryState {
    ready: bool,
    listeners: Vec<Listener>,
    running: bool,
    complete: bool,
    indeterminate: bool,
    waiters: Vec<RemovalDone>,
}

pub(crate) struct ViewRemovalDelivery {
    view: ViewId,
    core: Arc<dyn Thread>,
    state: Mutex<ViewRemovalDeliveryState>,
}

impl ViewRemovalDelivery {
    pub(crate) fn start(self: &Arc<Self>, done: RemovalDone) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.complete {
            drop(state);
            done(Ok(()));
            return;
        }
        if state.indeterminate {
            drop(state);
            done(Err(
                "View destruction notification has an indeterminate outcome".into(),
            ));
            return;
        }
        state.waiters.push(done);
        if !state.ready {
            return;
        }
        if state.running {
            return;
        }
        state.running = true;
        drop(state);
        let guard = ViewRemovalWake {
            delivery: self.clone(),
            pending: true,
        };
        let delivery = self.clone();
        self.core.post(Box::new(move || {
            let mut guard = guard;
            guard.pending = false;
            if !delivery.core.is_current() {
                delivery.finish(
                    Err("View destruction notification requires the core thread".into()),
                    false,
                );
                return;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::ipc::view_destroyed(delivery.view);
                crate::host::menu::document_view_destroyed(delivery.view);
                let listeners = delivery
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .listeners
                    .clone();
                for listener in &listeners {
                    listener(delivery.view);
                }
            }));
            match result {
                Ok(()) => delivery.finish(Ok(()), false),
                Err(_) => {
                    delivery.finish(Err("View destruction notification panicked".into()), true)
                }
            }
        }));
    }

    fn publish(self: &Arc<Self>, listeners: Vec<Listener>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.listeners = listeners;
        state.ready = true;
        drop(state);
        self.start(Box::new(|result| {
            if let Err(error) = result {
                eprintln!("{error}");
            }
        }));
    }

    fn finish(&self, result: Result<(), String>, indeterminate: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.running = false;
        state.complete = result.is_ok();
        state.indeterminate = indeterminate;
        let waiters = std::mem::take(&mut state.waiters);
        drop(state);
        for done in waiters {
            done(result.clone());
        }
    }
}

struct ViewRemovalWake {
    delivery: Arc<ViewRemovalDelivery>,
    pending: bool,
}
impl Drop for ViewRemovalWake {
    fn drop(&mut self) {
        if self.pending {
            self.delivery.finish(
                Err("View destruction notification was discarded".into()),
                false,
            );
        }
    }
}

static NEXT_VIEW_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_view_id(next: &AtomicU64) -> Result<ViewId, String> {
    let id = next
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
            if id <= u64::from(u32::MAX) {
                Some(id + 1)
            } else {
                None
            }
        })
        .map_err(|_| "The native view IDs are exhausted".to_owned())?;
    Ok(ViewId(id as u32))
}

pub(crate) struct ViewRegistration {
    view: ViewId,
    creation: RendererGeneration,
    table: Arc<()>,
}

impl ViewRegistration {
    pub(crate) fn view(&self) -> ViewId {
        self.view
    }
}

pub struct ViewTable {
    inner: Mutex<Inner>,
    identity: Arc<()>,
    next_window: AtomicU32,
    listeners: Mutex<Vec<Listener>>,
    renderer_listeners: Mutex<Vec<RendererListener>>,
    native_terminals: Arc<Mutex<NativeTerminalState>>,
    outlet: Arc<TauriOutlet>,
    core: Arc<dyn Thread>,
}

impl ViewTable {
    pub fn new(outlet: Arc<TauriOutlet>, core: Arc<dyn Thread>) -> Self {
        ViewTable {
            inner: Mutex::default(),
            identity: Arc::new(()),
            // Electron's ids start at 1
            next_window: AtomicU32::new(1),
            listeners: Mutex::default(),
            renderer_listeners: Mutex::default(),
            native_terminals: Arc::default(),
            outlet,
            core,
        }
    }

    pub fn new_window_id(&self) -> WindowId {
        WindowId(self.next_window.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) fn allocate_view(&self) -> Result<ViewRegistration, String> {
        Ok(ViewRegistration {
            view: allocate_view_id(&NEXT_VIEW_ID)?,
            creation: RendererGeneration::new(),
            table: self.identity.clone(),
        })
    }

    pub(crate) fn set_closure_proof(
        &self,
        view: ViewId,
        created: &RendererGeneration,
        proof: crate::host::window::NativeClosureProof,
    ) -> Result<(), String> {
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "The view table is unavailable")?;
            let record = inner
                .views
                .get_mut(&view)
                .ok_or("The view is unavailable")?;
            if !record.renderer.same_generation(created)
                || !proof.reservation().matches(view, created)
                || record.closure_proof.is_some()
            {
                return Err("The view closure reservation does not match".into());
            }
            proof.bind_teardown(record.teardown.clone())?;
            record.closure_proof = Some(proof.clone());
        }
        let terminals = self.native_terminals.clone();
        let core = self.core.clone();
        let reservation = proof.reservation().clone();
        proof.watch(Box::new(move |receipt| {
            let Some(owner) = receipt.terminal_owner() else {
                eprintln!("native view terminal owner is unavailable; actor retirement withheld");
                return;
            };
            let event = shell_contract::runtime::NativeViewTerminal {
                view,
                reservation,
                owner,
                receipt,
            };
            {
                let mut state = terminals.lock().unwrap_or_else(|error| error.into_inner());
                let listeners = state.listeners.clone();
                state.pending.push_back(NativeTerminalDelivery {
                    event,
                    listeners,
                    next: 0,
                });
            }
            wake_native_terminal_queue(terminals, core);
        }))?;
        Ok(())
    }

    pub(crate) fn closure_proof(
        &self,
        view: ViewId,
        created: &RendererGeneration,
    ) -> Option<crate::host::window::NativeClosureProof> {
        let inner = self.inner.lock().ok()?;
        let record = inner.views.get(&view)?;
        if !record.renderer.same_generation(created) {
            return None;
        }
        record.closure_proof.clone()
    }

    pub(crate) fn add_window(&self, window: WindowId, label: String) -> Arc<NativeWindowLifecycle> {
        let lifecycle = NativeWindowLifecycle::new(window);
        self.inner.lock().unwrap().windows.insert(
            window,
            WindowRecord {
                label,
                lifecycle: lifecycle.clone(),
            },
        );
        lifecycle
    }

    pub(crate) fn window_lifecycle(&self, window: WindowId) -> Option<Arc<NativeWindowLifecycle>> {
        self.inner
            .lock()
            .ok()?
            .windows
            .get(&window)
            .map(|record| record.lifecycle.clone())
    }

    /// Consume the only registration right for this newly allocated physical webview.
    pub(crate) fn register_view(
        &self,
        registration: ViewRegistration,
        window: WindowId,
        surface: Surface,
    ) -> Result<(ViewId, RendererGeneration), String> {
        if !Arc::ptr_eq(&registration.table, &self.identity) {
            return Err("The native view registration belongs to another table".into());
        }
        let ViewRegistration { view, creation, .. } = registration;
        let label = format!("view-{}", view.0);
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The view table is unavailable")?;
        if inner.views.contains_key(&view) || inner.by_label.contains_key(&label) {
            return Err("The native view registration was already consumed".into());
        }
        self.outlet.expect(view);
        inner.by_label.insert(label.clone(), view);
        inner.views.insert(
            view,
            Record {
                sender: Sender {
                    view,
                    window,
                    surface,
                },
                label,
                reserved: NativeReservedViewState::new(),
                docs_media: None,
                docs_browser: None,
                docs_asset: None,
                slides_media: None,
                slides_browser: None,
                asset_view: None,
                asset_browser: None,
                native_origin: None,
                renderer: creation.clone(),
                creation: creation.clone(),
                native_generation_used: false,
                native_browser: None,
                native_reload: None,
                closure_proof: None,
                browser_owner: Arc::new(Mutex::new(None)),
                teardown: Arc::new(ViewRemovalDelivery {
                    view,
                    core: self.core.clone(),
                    state: Mutex::default(),
                }),
            },
        );
        Ok((view, creation))
    }

    pub(crate) fn validate_reserved_view(
        &self,
        view: ViewId,
        created: &RendererGeneration,
        window: WindowId,
        surface: Surface,
        lifecycle: &Arc<NativeWindowLifecycle>,
    ) -> Result<(), String> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| "The view table is unavailable")?;
        let record = inner
            .views
            .get(&view)
            .ok_or("The captured native view is unavailable")?;
        if !record.creation.same_generation(created)
            || !record.renderer.same_generation(created)
            || !created.is_active()
            || record.sender.window != window
            || record.sender.surface != surface
            || !inner
                .windows
                .get(&window)
                .is_some_and(|current| Arc::ptr_eq(&current.lifecycle, lifecycle))
        {
            return Err("The captured native view or parent changed".into());
        }
        Ok(())
    }

    /// The shell window's view: what [`shell_contract::hosts::MainWindow`] answers
    pub fn mark_main(&self, view: ViewId) {
        self.inner.lock().unwrap().main = Some(view);
    }
    pub fn marked_main(&self) -> Option<Sender> {
        let inner = self.inner.lock().unwrap();
        inner
            .main
            .and_then(|view| inner.views.get(&view))
            .map(|record| record.sender)
    }

    /// The shell window's view if it is alive; else the first view of the shell surface
    pub fn main(&self) -> Option<Sender> {
        let inner = self.inner.lock().unwrap();
        inner
            .main
            .and_then(|view| inner.views.get(&view))
            .or_else(|| {
                inner
                    .views
                    .values()
                    .find(|r| r.sender.surface == Surface::Shell)
            })
            .map(|r| r.sender)
    }

    /// Replaces only the captured renderer; stale notifications cannot retire a successor.
    #[cfg(test)]
    pub fn replace_renderer(
        &self,
        view: ViewId,
        expected: &RendererGeneration,
    ) -> Option<RendererGeneration> {
        let next = RendererGeneration::new();
        {
            let inner = self.inner.lock().ok()?;
            if !inner
                .views
                .get(&view)
                .is_some_and(|record| record.renderer.same_generation(expected))
                || !expected.is_active()
            {
                return None;
            }
        }
        let loss = expected.revoke_deferred();
        let replaced = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            match inner.views.get_mut(&view) {
                Some(record) if record.renderer.same_generation(expected) => {
                    let origin = record.native_origin.take();
                    Some((
                        std::mem::replace(&mut record.renderer, next.clone()),
                        origin,
                    ))
                }
                _ => None,
            }
        };
        loss.cancel();
        let (previous, origin) = replaced?;
        drop(origin);
        self.notify_renderer_replaced(view, previous);
        Some(next)
    }

    fn notify_renderer_replaced(&self, view: ViewId, previous: RendererGeneration) {
        let listeners = self
            .renderer_listeners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        self.core.post(Box::new(move || {
            for listener in &listeners {
                listener(view, previous.clone());
            }
        }));
    }

    pub fn set_docs_media_lease(&self, view: ViewId, lease: MediaViewLease) -> Result<(), String> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => {
                lease.revoke();
                return Err("The view table is unavailable.".to_owned());
            }
        };
        let Some(record) = inner.views.get_mut(&view) else {
            lease.revoke();
            return Err("The document view is no longer available.".to_owned());
        };
        if record.sender.surface != Surface::Docs
            || lease.view() != view.0
            || record.docs_media.is_some()
            || lease.current().is_none()
        {
            lease.revoke();
            return Err("The document view does not admit this media lease.".to_owned());
        }
        record.docs_media = Some(lease);
        Ok(())
    }

    pub(crate) fn set_docs_asset_session(
        &self,
        view: ViewId,
        session: crate::host::document_assets::AssetSession,
    ) -> Result<(), String> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => {
                let _ = crate::host::document_assets::revoke_session(session);
                return Err("The view table is unavailable.".to_owned());
            }
        };
        let valid = inner.views.get(&view).is_some_and(|record| {
            record.sender.surface == Surface::Docs
                && record.docs_asset.is_none()
                && session.view() == view
                && session.kind() == crate::host::document_assets::AssetKind::Docs
        });
        if !valid {
            drop(inner);
            let _ = crate::host::document_assets::revoke_session(session);
            return Err("The document view does not admit this asset session.".to_owned());
        }
        if let Some(record) = inner.views.get_mut(&view) {
            record.docs_asset = Some(session);
        }
        Ok(())
    }

    pub fn bind_docs_media_browser(
        &self,
        view: ViewId,
        lease: &MediaViewLease,
        browser_id: BrowserId,
        browser: BrowserState,
    ) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The view table is unavailable.".to_owned())?;
        let record = inner
            .views
            .get_mut(&view)
            .ok_or("The document view is no longer available.")?;
        if record.sender.surface != Surface::Docs
            || !record
                .docs_media
                .as_ref()
                .is_some_and(|existing| existing.same_view(lease))
            || lease.current().is_none()
        {
            return Err("The document media lease is no longer current.".to_owned());
        }
        if let Some((old_id, old_browser)) = &record.docs_browser {
            if *old_id != browser_id || !old_browser.is_same_browser(&browser) {
                return Err("The document browser lifetime has changed.".to_owned());
            }
        } else {
            record.docs_browser = Some((browser_id, browser));
        }
        Ok(())
    }

    pub fn prepare_slides_media(&self, view: ViewId) -> Result<SlidesViewLease, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The view table is unavailable.".to_owned())?;
        let record = inner
            .views
            .get_mut(&view)
            .ok_or("The presentation view is no longer available.")?;
        if record.sender.surface != Surface::Slides || record.slides_media.is_some() {
            return Err("The presentation view does not admit a media lease.".to_owned());
        }
        let lease = SlidesViewLease::new();
        record.slides_media = Some(lease.clone());
        Ok(lease)
    }

    pub fn prepare_native_assets(&self, view: ViewId) -> Result<AssetViewLease, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The view table is unavailable.".to_owned())?;
        let record = inner
            .views
            .get_mut(&view)
            .ok_or("The asset view is no longer available.")?;
        if !matches!(
            record.sender.surface,
            Surface::Html | Surface::Markdown | Surface::Pdf
        ) || record.asset_view.is_some()
        {
            return Err("The view does not admit an asset lease.".to_owned());
        }
        let lease = AssetViewLease::new();
        record.asset_view = Some(lease.clone());
        Ok(lease)
    }

    pub fn bind_native_assets_browser(
        &self,
        view: ViewId,
        lease: &AssetViewLease,
        browser_id: BrowserId,
        browser: BrowserState,
    ) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The view table is unavailable.".to_owned())?;
        let record = inner
            .views
            .get_mut(&view)
            .ok_or("The asset view is no longer available.")?;
        if !matches!(
            record.sender.surface,
            Surface::Html | Surface::Markdown | Surface::Pdf
        ) || !lease.is_live()
            || !record
                .asset_view
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(&current.0, &lease.0))
        {
            return Err("The asset view lease is no longer current.".to_owned());
        }
        if record.native_origin.as_ref().is_some_and(|bound| {
            bound.context.identity().browser_id() != browser_id
                || !bound.context.browser().is_same_browser(&browser)
        }) {
            return Err("The native context belongs to a different browser.".to_owned());
        }
        if let Some((old_id, old_browser)) = &record.asset_browser {
            if *old_id != browser_id || !old_browser.is_same_browser(&browser) {
                return Err("The asset browser lifetime has changed.".to_owned());
            }
        } else {
            record.asset_browser = Some((browser_id, browser));
        }
        Ok(())
    }

    /// Only the fresh native registration callback may map a document to a
    /// generation. The reserved creation epoch names this view incarnation;
    /// the first document keeps that epoch for constructor handoffs.
    pub(crate) fn reserve_native_reload(
        &self,
        view: ViewId,
        old_renderer: &RendererGeneration,
        origin: &DocumentContext,
        reservation: &ReloadReservation,
    ) -> Result<RendererGeneration, String> {
        if !reservation.belongs_to(origin.browser())
            || !origin.is_registered()
            || !old_renderer.is_active()
        {
            return Err("Original native reload owner is no longer available.".into());
        }
        let next = RendererGeneration::new();
        let mut inner = self.inner.lock().map_err(|_| "View table was poisoned.")?;
        let record = inner
            .views
            .get_mut(&view)
            .ok_or("Original reload view is unavailable.")?;
        if !record.renderer.same_generation(old_renderer)
            || record.native_reload.is_some()
            || !record.native_origin.as_ref().is_some_and(|bound| {
                bound.context == *origin && !bound.lost.load(Ordering::Acquire)
            })
        {
            return Err("Original native reload binding changed.".into());
        }
        record.native_reload = Some(ReservedNativeReload {
            identity: reservation.identity(),
            renderer: next.clone(),
        });
        Ok(next)
    }

    pub(crate) fn cancel_native_reload(&self, view: ViewId, reserved: &RendererGeneration) {
        let pending = {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            let Some(record) = inner.views.get_mut(&view) else {
                return;
            };
            if record
                .native_reload
                .as_ref()
                .is_some_and(|pending| pending.renderer.same_generation(reserved))
            {
                record.native_reload.take()
            } else {
                None
            }
        };
        if let Some(pending) = pending {
            pending.renderer.revoke();
        }
    }

    pub(crate) fn bind_verified_native_origin(
        &self,
        view: ViewId,
        renderer: &RendererGeneration,
        origin: &DocumentContext,
    ) -> bool {
        if !origin.is_registered() {
            return false;
        }
        let lost = Arc::new(AtomicBool::new(false));
        let on_loss = lost.clone();
        let Some(subscription) = origin.on_revoke(move || {
            on_loss.store(true, Ordering::Release);
        }) else {
            return false;
        };
        let (captured, needs_replacement, asset, has_reserved) = {
            let Ok(inner) = self.inner.lock() else {
                return false;
            };
            let Some(record) = inner.views.get(&view) else {
                return false;
            };
            (
                record.renderer.clone(),
                record.native_generation_used,
                record.asset_view.clone(),
                record.native_reload.is_some(),
            )
        };
        let fallback = (needs_replacement && !has_reserved).then(RendererGeneration::new);
        let mut notifications = [None, None];
        let (accepted, retired) = origin
            .with_live(|| {
                let _asset_guard = match &asset {
                    Some(asset) => match asset.0.live.lock() {
                        Ok(guard) if *guard => Some(guard),
                        _ => return None,
                    },
                    None => None,
                };
                RendererGeneration::with_active_all_retiring(
                    [Some(&captured)],
                    [Some(&captured), None],
                    &mut notifications,
                    |retirement| {
                        let Ok(mut inner) = self.inner.lock() else {
                            return (false, None);
                        };
                        let Some(record) = inner.views.get_mut(&view) else {
                            return (false, None);
                        };
                        if !record.renderer.same_generation(&captured)
                            || record.native_generation_used != needs_replacement
                        {
                            return (false, None);
                        }
                        let wrong_browser = |binding: &(BrowserId, BrowserState)| {
                            binding.0 != origin.identity().browser_id()
                                || !binding.1.is_same_browser(origin.browser())
                        };
                        if record.asset_browser.as_ref().is_some_and(wrong_browser)
                            || record.native_browser.as_ref().is_some_and(wrong_browser)
                            || !record.creation.same_generation(renderer)
                            || match (&record.asset_view, &asset) {
                                (None, None) => false,
                                (Some(current), Some(expected)) => {
                                    !Arc::ptr_eq(&current.0, &expected.0)
                                }
                                _ => true,
                            }
                            || lost.load(Ordering::Acquire)
                        {
                            return (false, None);
                        }
                        if let Some(bound) = &record.native_origin {
                            if bound.context == *origin {
                                return (
                                    bound.renderer.is_active()
                                        && !bound.lost.load(Ordering::Acquire),
                                    None,
                                );
                            }
                            if !bound.context.is_revoked() {
                                return (false, None);
                            }
                        }
                        let reserved = match record.native_reload.as_ref() {
                            Some(pending) if pending.identity.matches_registered(origin) => {
                                Some(pending.renderer.clone())
                            }
                            Some(_) => return (false, None),
                            None => None,
                        };
                        let retired = if record.native_generation_used {
                            let Some(next) = reserved.or(fallback) else {
                                return (false, None);
                            };
                            if retirement.retire(&captured).is_err() {
                                return (false, None);
                            }
                            let previous = std::mem::replace(&mut record.renderer, next);
                            Some(previous)
                        } else {
                            if !record.renderer.same_generation(renderer) || !renderer.is_active() {
                                return (false, None);
                            }
                            None
                        };
                        record.native_generation_used = true;
                        record.native_reload = None;
                        if record.native_browser.is_none() {
                            record.native_browser =
                                Some((origin.identity().browser_id(), origin.browser().clone()));
                        }
                        record.native_origin = Some(BoundNativeOrigin {
                            context: origin.clone(),
                            renderer: record.renderer.clone(),
                            lost,
                            _loss_subscription: subscription,
                        });
                        (true, retired)
                    },
                )
            })
            .flatten()
            .unwrap_or((false, None));
        for loss in notifications.into_iter().flatten() {
            self.core.post(Box::new(move || loss.cancel()));
        }
        if let Some(previous) = retired {
            self.notify_renderer_replaced(view, previous);
        }
        accepted
    }

    /// Resolve only the context captured by the focused native menu target.
    pub(crate) fn capture_native_menu_target(
        &self,
        origin: &DocumentContext,
    ) -> Option<(ViewId, RendererGeneration)> {
        let target = {
            let inner = self.inner.lock().ok()?;
            let mut records = inner.views.values().filter(|record| {
                record.native_origin.as_ref().is_some_and(|bound| {
                    bound.context == *origin && !bound.lost.load(Ordering::Acquire)
                })
            });
            let record = records.next()?;
            if records.next().is_some() {
                return None;
            }
            (
                record.sender.view,
                record.native_origin.as_ref()?.renderer.clone(),
            )
        };
        (origin.is_live() && target.1.is_active()).then_some(target)
    }

    /// Resolve only a previously staged, exact context and creation-time
    /// renderer generation. The caller must gate each effect against both.
    pub(crate) fn authorize_verified_native_origin(
        &self,
        label: &str,
        origin: &DocumentContext,
    ) -> Option<(Sender, RendererGeneration, NativeReservedViewState)> {
        let (sender, renderer, reserved) = {
            let inner = self.inner.lock().ok()?;
            let record = inner.views.get(inner.by_label.get(label)?)?;
            let bound = record.native_origin.as_ref()?;
            if record.label != label
                || bound.context != *origin
                || bound.lost.load(Ordering::Acquire)
                || !bound.renderer.same_generation(&record.renderer)
            {
                return None;
            }
            (
                record.sender,
                bound.renderer.clone(),
                record.reserved.clone(),
            )
        };
        (renderer.is_active() && origin.is_registered()).then_some((sender, renderer, reserved))
    }

    /// The caller must admit `created` in the same plan as the actual transition.
    pub(crate) fn prepare_browser_owner(
        &self,
        view: ViewId,
        created: &RendererGeneration,
        owner: &shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<PreparedBrowserOwner, String> {
        let inner = self.inner.lock().map_err(|_| "view table is unavailable")?;
        let record = inner
            .views
            .get(&view)
            .ok_or("The native view is unavailable")?;
        if !record.creation.same_generation(created)
            || !owner.reservation().matches(view, created)
            || !record
                .closure_proof
                .as_ref()
                .is_some_and(|proof| proof.reservation().same_reservation(owner.reservation()))
        {
            return Err("The browser reservation does not match the captured view".into());
        }
        Ok(PreparedBrowserOwner {
            slot: record.browser_owner.clone(),
            reservation: owner.reservation().clone(),
            owner: owner.clone(),
        })
    }

    /// Capture the slot from the record bound to the original authenticated
    /// document. The caller must separately admit that document and renderer.
    pub(crate) fn capture_browser_owner(
        &self,
        view: ViewId,
        origin: &DocumentContext,
        renderer: &RendererGeneration,
        owner: &shell_contract::ipc::NativeBrowserOwner,
    ) -> Option<CapturedBrowserOwner> {
        let inner = self.inner.lock().ok()?;
        let record = inner.views.get(&view)?;
        let bound = record.native_origin.as_ref()?;
        if bound.context != *origin
            || bound.lost.load(Ordering::Acquire)
            || !bound.renderer.same_generation(renderer)
            || !record.renderer.same_generation(renderer)
            || !record
                .closure_proof
                .as_ref()
                .is_some_and(|proof| proof.reservation().same_reservation(owner.reservation()))
            || !owner.reservation().matches(view, &record.creation)
        {
            return None;
        }
        Some(CapturedBrowserOwner {
            slot: record.browser_owner.clone(),
            owner: owner.clone(),
        })
    }

    /// Capture an already bound native target and its independently owned
    /// surface lease. The caller retains and fences these original owners.
    pub(crate) fn capture_native_owner(
        &self,
        view: ViewId,
        expected: &RendererGeneration,
    ) -> Option<CapturedNativeOwner> {
        let captured = {
            let inner = self.inner.lock().ok()?;
            let record = inner.views.get(&view)?;
            let bound = record.native_origin.as_ref()?;
            if bound.lost.load(Ordering::Acquire)
                || !bound.renderer.same_generation(expected)
                || !record.renderer.same_generation(expected)
            {
                return None;
            }
            let asset = if matches!(
                record.sender.surface,
                Surface::Html | Surface::Markdown | Surface::Pdf
            ) {
                let (browser_id, browser) = record.asset_browser.as_ref()?;
                if *browser_id != bound.context.identity().browser_id()
                    || !browser.is_same_browser(bound.context.browser())
                {
                    return None;
                }
                Some(NativeAssetAuthorization {
                    view,
                    surface: record.sender.surface,
                    lease: record.asset_view.as_ref()?.clone(),
                    browser_id: *browser_id,
                    browser: browser.clone(),
                    document: bound.context.clone(),
                })
            } else {
                None
            };
            CapturedNativeOwner {
                origin: bound.context.clone(),
                renderer: bound.renderer.clone(),
                surface: record.sender.surface,
                asset,
                slides: record.slides_media.clone(),
                docs: record.docs_media.clone(),
                browser_owner: record
                    .browser_owner
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone(),
            }
        };
        if !captured.origin.is_registered()
            || !captured.renderer.is_active()
            || captured
                .asset
                .as_ref()
                .is_some_and(|asset| !asset.is_current())
            || (captured.surface == Surface::Slides
                && !captured
                    .slides
                    .as_ref()
                    .is_some_and(SlidesViewLease::is_active))
            || (captured.surface == Surface::Docs
                && captured
                    .docs
                    .as_ref()
                    .is_none_or(|lease| lease.current().is_none()))
        {
            return None;
        }
        Some(captured)
    }

    /// Resolve an authenticated ingress token only against its prior exact
    /// binding. Neither label nor a current-document snapshot proves origin.
    pub(crate) fn authorize_verified_native_asset_origin(
        &self,
        label: &str,
        origin: &DocumentContext,
    ) -> Option<(Sender, NativeAssetAuthorization, RendererGeneration)> {
        let (sender, asset, renderer) = {
            let inner = self.inner.lock().ok()?;
            let record = inner.views.get(inner.by_label.get(label)?)?;
            let bound = record.native_origin.as_ref()?;
            let (browser_id, browser) = record.asset_browser.as_ref()?;
            if record.label != label
                || bound.context != *origin
                || bound.lost.load(Ordering::Acquire)
                || !bound.renderer.same_generation(&record.renderer)
                || *browser_id != origin.identity().browser_id()
                || !browser.is_same_browser(origin.browser())
            {
                return None;
            }
            let lease = record.asset_view.as_ref()?.clone();
            let asset = NativeAssetAuthorization {
                view: record.sender.view,
                surface: record.sender.surface,
                lease,
                browser_id: *browser_id,
                browser: browser.clone(),
                document: origin.clone(),
            };
            (record.sender, asset, bound.renderer.clone())
        };
        (renderer.is_active() && asset.is_current()).then_some((sender, asset, renderer))
    }

    pub fn bind_slides_media_browser(
        &self,
        view: ViewId,
        lease: &SlidesViewLease,
        browser_id: BrowserId,
        browser: BrowserState,
    ) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The view table is unavailable.".to_owned())?;
        let record = inner
            .views
            .get_mut(&view)
            .ok_or("The presentation view is no longer available.")?;
        if record.sender.surface != Surface::Slides
            || !lease.is_active()
            || !record
                .slides_media
                .as_ref()
                .is_some_and(|current| current.same_lifetime(lease))
        {
            return Err("The presentation media lease is no longer current.".to_owned());
        }
        if let Some((old_id, old_browser)) = &record.slides_browser {
            if *old_id != browser_id || !old_browser.is_same_browser(&browser) {
                return Err("The presentation browser lifetime has changed.".to_owned());
            }
        } else {
            record.slides_browser = Some((browser_id, browser));
        }
        Ok(())
    }

    pub fn authorize_slides_media(
        &self,
        label: &str,
        context: &SchemeRequestContext,
    ) -> Option<SlidesMediaAuthorization> {
        let browser_id = context.browser_id()?;
        let browser = context.browser_state()?;
        let document = context.document()?;
        let inner = self.inner.lock().ok()?;
        let view = *inner.by_label.get(label)?;
        let record = inner.views.get(&view)?;
        if record.sender.surface != Surface::Slides || record.label != label {
            return None;
        }
        let (expected_id, expected_browser) = record.slides_browser.as_ref()?;
        if *expected_id != browser_id
            || !expected_browser.is_same_browser(browser)
            || expected_browser.document().as_ref() != Some(document)
        {
            return None;
        }
        let lease = record.slides_media.as_ref()?.clone();
        if !lease.is_active() {
            return None;
        }
        Some(SlidesMediaAuthorization {
            view,
            lease,
            browser_id,
            browser: expected_browser.clone(),
            document: document.clone(),
        })
    }

    pub fn authorize_docs_media(
        &self,
        label: &str,
        context: &SchemeRequestContext,
    ) -> Option<DocsMediaAuthorization> {
        let browser_id = context.browser_id()?;
        let browser = context.browser_state()?;
        let document = context.document()?;
        let inner = self.inner.lock().ok()?;
        let view = *inner.by_label.get(label)?;
        let record = inner.views.get(&view)?;
        if record.sender.surface != Surface::Docs || record.label != label {
            return None;
        }
        let (expected_id, expected_browser) = record.docs_browser.as_ref()?;
        if *expected_id != browser_id
            || !expected_browser.is_same_browser(browser)
            || expected_browser.document().as_ref() != Some(document)
        {
            return None;
        }
        let permit = record.docs_media.as_ref()?.current()?;
        Some(DocsMediaAuthorization {
            view,
            permit,
            browser: expected_browser.clone(),
            document: document.clone(),
        })
    }

    pub fn label_of_view(&self, view: ViewId) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        inner.views.get(&view).map(|r| r.label.clone())
    }

    pub fn label_of_window(&self, window: WindowId) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .windows
            .get(&window)
            .map(|record| record.label.clone())
    }

    pub fn move_view(&self, view: ViewId, window: WindowId) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.windows.contains_key(&window) {
            return Err(format!("no window {}", window.0));
        }
        let record = inner
            .views
            .get_mut(&view)
            .ok_or_else(|| format!("no view {}", view.0))?;
        record.sender.window = window;
        Ok(())
    }

    pub(crate) fn move_view_between_captured(
        &self,
        view: ViewId,
        expected: &RendererGeneration,
        source: WindowId,
        source_lifecycle: &Arc<NativeWindowLifecycle>,
        target: WindowId,
        target_lifecycle: &Arc<NativeWindowLifecycle>,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().map_err(|_| "view table is unavailable")?;
        if !inner
            .windows
            .get(&source)
            .is_some_and(|record| Arc::ptr_eq(&record.lifecycle, source_lifecycle))
            || !inner
                .windows
                .get(&target)
                .is_some_and(|record| Arc::ptr_eq(&record.lifecycle, target_lifecycle))
        {
            return Err("A captured window was replaced".into());
        }
        let record = inner
            .views
            .get_mut(&view)
            .ok_or_else(|| format!("no view {}", view.0))?;
        if !record.renderer.same_generation(expected)
            || !expected.is_active()
            || record.sender.window != source
        {
            return Err("The captured view or parent changed".into());
        }
        record.sender.window = target;
        Ok(())
    }

    pub(crate) fn capture_closing_view(
        &self,
        view: ViewId,
        webview: tauri::Webview,
        owner: &shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<CapturedClosingView, String> {
        use crate::native::WebviewExt;
        let close = webview
            .native_browser_close()
            .map_err(|error| error.to_string())?;
        let inner = self.inner.lock().map_err(|_| "view table is unavailable")?;
        let record = inner
            .views
            .get(&view)
            .ok_or("The captured view is unavailable")?;
        let proof = record
            .closure_proof
            .as_ref()
            .ok_or("The browser reservation is unavailable")?;
        if !record
            .creation
            .same_generation(owner.reservation().created())
            || !owner.reservation().matches(view, &record.creation)
            || !proof.reservation().same_reservation(owner.reservation())
            || !proof.close().same_reservation(&close)
            || !record
                .browser_owner
                .lock()
                .map_err(|_| "browser owner is unavailable")?
                .as_ref()
                .is_some_and(|bound| bound.same_owner(owner))
        {
            return Err("The captured webview does not belong to this browser".into());
        }
        let parent = record.sender.window;
        let parent_lifecycle = inner
            .windows
            .get(&parent)
            .ok_or("The captured parent window is unavailable")?
            .lifecycle
            .clone();
        Ok(CapturedClosingView {
            webview,
            creation: record.creation.clone(),
            proof: proof.clone(),
            owner: owner.clone(),
            parent,
            parent_lifecycle,
        })
    }

    /// Move only the browser captured by a close permit, even if its renderer
    /// has already retired. The caller holds `with_closing` for this owner.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn move_view_between_closing(
        &self,
        view: ViewId,
        creation: &RendererGeneration,
        owner: &shell_contract::ipc::NativeBrowserOwner,
        source: WindowId,
        source_lifecycle: &Arc<NativeWindowLifecycle>,
        target: WindowId,
        target_lifecycle: &Arc<NativeWindowLifecycle>,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().map_err(|_| "view table is unavailable")?;
        if !inner
            .windows
            .get(&source)
            .is_some_and(|record| Arc::ptr_eq(&record.lifecycle, source_lifecycle))
            || !inner
                .windows
                .get(&target)
                .is_some_and(|record| Arc::ptr_eq(&record.lifecycle, target_lifecycle))
        {
            return Err("A captured window was replaced".into());
        }
        let record = inner
            .views
            .get_mut(&view)
            .ok_or_else(|| format!("no view {}", view.0))?;
        if !record.creation.same_generation(creation)
            || record.sender.window != source
            || !transition.matches_owner(owner)
            || !record
                .closure_proof
                .as_ref()
                .is_some_and(|proof| transition.matches(proof.reservation()))
            || !record
                .browser_owner
                .lock()
                .map_err(|_| "browser owner is unavailable")?
                .as_ref()
                .is_some_and(|bound| bound.same_owner(owner))
        {
            return Err("The captured view, browser or parent changed".into());
        }
        record.sender.window = target;
        Ok(())
    }

    pub(crate) fn view_window_captured(
        &self,
        view: ViewId,
        expected: &RendererGeneration,
    ) -> Result<(WindowId, Arc<NativeWindowLifecycle>), String> {
        let inner = self.inner.lock().map_err(|_| "view table is unavailable")?;
        let record = inner
            .views
            .get(&view)
            .ok_or_else(|| format!("no view {}", view.0))?;
        if !record.renderer.same_generation(expected) || !expected.is_active() {
            return Err("The captured view was replaced".into());
        }
        let window = record.sender.window;
        let lifecycle = inner
            .windows
            .get(&window)
            .ok_or("The parent window is unavailable")?
            .lifecycle
            .clone();
        Ok((window, lifecycle))
    }

    pub fn window_of_window_label(&self, label: &str) -> Option<WindowId> {
        let inner = self.inner.lock().unwrap();
        inner
            .windows
            .iter()
            .find(|(_, record)| record.label == label)
            .map(|(w, _)| *w)
    }

    pub fn views_of_window(&self, window: WindowId) -> Vec<ViewId> {
        let inner = self.inner.lock().unwrap();
        inner
            .views
            .values()
            .filter(|r| r.sender.window == window)
            .map(|r| r.sender.view)
            .collect()
    }

    pub fn zoom(&self, view: ViewId) -> f64 {
        let inner = self.inner.lock().unwrap();
        inner.views.get(&view).map_or(1.0, |r| r.reserved.zoom())
    }

    pub fn set_zoom(&self, view: ViewId, factor: f64) {
        if let Some(r) = self.inner.lock().unwrap().views.get_mut(&view) {
            r.reserved.set_zoom(factor);
        }
    }

    pub fn set_dropped(&self, label: &str, paths: Vec<PathBuf>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(view) = inner.by_label.get(label).copied()
            && let Some(r) = inner.views.get_mut(&view)
        {
            r.reserved
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .dropped = paths;
        }
    }

    /// The view is gone: forget it, close its stream, and on the core thread
    /// cancel its pending calls and tell the listeners of [`Views::on_destroyed`]
    pub fn remove_view(&self, view: ViewId) {
        self.remove_view_matching(view, None);
    }

    pub(crate) fn remove_view_if_generation(
        &self,
        view: ViewId,
        expected: &RendererGeneration,
    ) -> bool {
        self.remove_view_matching(view, Some(expected))
    }

    pub(crate) fn remove_view_after_terminal(
        &self,
        view: ViewId,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        owner: &shell_contract::ipc::NativeBrowserOwner,
        receipt: &shell_contract::ipc::NativeClosureReceipt,
    ) -> Result<TerminalViewRemoval, String> {
        if !receipt.matches(reservation, view, reservation.created()) {
            return Err("The native terminal receipt does not match the captured view".into());
        }
        let (replaced, removed) = receipt.with_terminal_owner(owner, |transition| {
            if !transition.matches(reservation, owner) {
                return Err("The native terminal owner does not match the captured view".into());
            }
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "The view table is unavailable")?;
            let Some(record) = inner.views.get(&view) else {
                return Ok((false, None));
            };
            if !record.creation.same_generation(reservation.created())
                || !record.closure_proof.as_ref().is_some_and(|proof| {
                    proof.reservation().same_reservation(reservation)
                        && proof.owner().same_owner(owner)
                })
                || record
                    .browser_owner
                    .lock()
                    .map_err(|_| "The browser owner is unavailable")?
                    .as_ref()
                    .is_some_and(|current| !current.same_owner(owner))
            {
                return Ok((true, None));
            }
            let removed = inner
                .views
                .remove(&view)
                .ok_or("The captured native view disappeared")?;
            if inner.by_label.get(&removed.label) == Some(&view) {
                inner.by_label.remove(&removed.label);
            }
            if inner.main == Some(view) {
                inner.main = None;
            }
            Ok((false, Some(removed)))
        })?;
        if let Some(removed) = removed {
            removed.renderer.revoke_deferred().cancel();
            let delivery = removed.teardown.clone();
            self.finish_removed_view(view, removed);
            return Ok(TerminalViewRemoval::Removed(delivery));
        }
        Ok(if replaced {
            TerminalViewRemoval::Replaced
        } else {
            TerminalViewRemoval::Absent
        })
    }

    fn remove_view_matching(&self, view: ViewId, expected: Option<&RendererGeneration>) -> bool {
        let captured = {
            let inner = self.inner.lock().unwrap();
            let Some(record) = inner.views.get(&view) else {
                return false;
            };
            if expected.is_some_and(|expected| !record.renderer.same_generation(expected)) {
                return false;
            }
            record.renderer.clone()
        };
        let renderer_loss = captured.revoke_deferred();
        let removed = {
            let mut inner = self.inner.lock().unwrap();
            if !inner
                .views
                .get(&view)
                .is_some_and(|record| record.renderer.same_generation(&captured))
            {
                drop(inner);
                renderer_loss.cancel();
                return false;
            }
            let removed = inner.views.remove(&view);
            if let Some(r) = &removed
                && inner.by_label.get(&r.label) == Some(&view)
            {
                inner.by_label.remove(&r.label);
            }
            if inner.main == Some(view) {
                inner.main = None;
            }
            removed
        };
        renderer_loss.cancel();
        let Some(removed) = removed else {
            return false;
        };
        self.finish_removed_view(view, removed);
        true
    }

    fn finish_removed_view(&self, view: ViewId, removed: Record) {
        if let Some(lease) = &removed.docs_media {
            lease.revoke();
        }
        if let Some(lease) = &removed.slides_media {
            lease.revoke();
            crate::host::d1::slides_media::revoke_view(view, lease);
            crate::host::d1::slides_fonts::revoke_view(view, lease);
        }
        if let Some(lease) = &removed.asset_view {
            lease.revoke();
        }
        if let Some(session) = removed.docs_asset
            && let Err(error) = crate::host::document_assets::revoke_session(session)
        {
            eprintln!("[docs] could not revoke document asset session: {error}");
        }
        if let Err(error) = crate::host::document_assets::revoke(view) {
            eprintln!("[assets] could not revoke document asset grants: {error}");
        }
        crate::engine_host::owner_destroyed(view);
        crate::host::renderer_disposal::forget(view);
        self.outlet.forget(view);
        let listeners = self.listeners.lock().unwrap().clone();
        removed.teardown.publish(listeners);
    }

    pub(crate) fn remove_window_captured(
        &self,
        window: WindowId,
        expected: &Arc<NativeWindowLifecycle>,
    ) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "The view table is unavailable")?;
        let record = inner
            .windows
            .get(&window)
            .ok_or("The captured window is unavailable")?;
        if !Arc::ptr_eq(&record.lifecycle, expected) {
            return Err("The captured window was replaced".into());
        }
        if inner
            .views
            .values()
            .any(|record| record.sender.window == window)
        {
            return Err("The captured window still owns views".into());
        }
        inner.windows.remove(&window);
        Ok(())
    }

    /// The window is gone with every view in it
    pub fn remove_window(&self, window: WindowId) {
        for view in self.views_of_window(window) {
            self.remove_view(view);
        }
        self.inner.lock().unwrap().windows.remove(&window);
    }
}

impl shell_contract::runtime::NativeViewRetirements for ViewTable {
    fn on_native_terminal(
        &self,
        listener: Box<
            dyn Fn(shell_contract::runtime::NativeViewTerminal) -> Result<(), String> + Send + Sync,
        >,
    ) -> shell_contract::runtime::NativeViewRetirementSubscription {
        let subscription = shell_contract::runtime::NativeViewRetirementSubscription::new();
        let mut state = self
            .native_terminals
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state
            .listeners
            .retain(|entry| entry.active.load(Ordering::Acquire));
        state.listeners.push(Arc::new(NativeTerminalListener {
            active: subscription.state(),
            callback: listener,
        }));
        subscription
    }
    fn authenticated_browser_owner(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
    ) -> Option<shell_contract::ipc::NativeBrowserOwner> {
        let browser =
            crate::ipc::native_owner::authenticated_browser_owner(&owner.context)?.clone();
        let loss = owner.loss_token()?;
        crate::ipc::native_owner::with_many_owner_effect_extra(
            owner,
            &[],
            &loss,
            &loss,
            &[],
            || {
                browser
                    .reservation()
                    .with_owner(&browser, |_| Ok(browser.clone()))
                    .ok()
            },
        )
        .flatten()
    }
    fn drain_native_terminals(&self) -> Result<usize, String> {
        drain_native_terminal_queue(&self.native_terminals, &self.core)
    }
    fn pending_native_terminals(&self) -> Result<usize, String> {
        self.native_terminals
            .lock()
            .map(|state| state.pending.len())
            .map_err(|_| "Native terminal queue is unavailable".into())
    }
    fn wake_native_terminals(&self) {
        wake_native_terminal_queue(self.native_terminals.clone(), self.core.clone());
    }
}

impl Views for ViewTable {
    fn sender_of(&self, view: ViewId) -> Option<Sender> {
        self.inner
            .lock()
            .unwrap()
            .views
            .get(&view)
            .map(|r| r.sender)
    }

    fn all(&self) -> Vec<Sender> {
        self.inner
            .lock()
            .unwrap()
            .views
            .values()
            .map(|r| r.sender)
            .collect()
    }

    fn on_destroyed(&self, listener: Box<dyn Fn(ViewId) + Send + Sync>) {
        self.listeners.lock().unwrap().push(Arc::from(listener));
    }

    fn renderer_generation(&self, view: ViewId) -> Option<RendererGeneration> {
        let inner = self.inner.lock().ok()?;
        let generation = &inner.views.get(&view)?.renderer;
        generation.is_active().then(|| generation.clone())
    }

    fn on_renderer_replaced(
        &self,
        listener: Box<dyn Fn(ViewId, RendererGeneration) + Send + Sync>,
    ) {
        self.renderer_listeners
            .lock()
            .unwrap()
            .push(Arc::from(listener));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_snapshot_rejects_title_mutation_and_stale_revision() {
        use shell_ipc_w::tabs::window_snapshot::{
            WindowObservation, WindowReservation, with_window_observations,
        };
        let lifecycle = NativeWindowLifecycle::new(WindowId(7));
        let reservation = WindowReservation::new(WindowId(7), lifecycle.clone());
        let captured = WindowObservation::new(reservation, 0, false, "old".into());
        assert!(with_window_observations(std::slice::from_ref(&captured), || ()).is_ok());
        {
            let mut state = lifecycle.state.lock().unwrap();
            state.mutating = true;
            state.advance();
        }
        assert!(with_window_observations(std::slice::from_ref(&captured), || ()).is_err());
        {
            let mut state = lifecycle.state.lock().unwrap();
            state.mutating = false;
            state.advance();
        }
        assert!(with_window_observations(&[captured], || ()).is_err());
    }

    #[test]
    fn unsubmitted_window_terminal_replays_without_locking_callback() {
        let lifecycle = NativeWindowLifecycle::new(WindowId(8));
        let submission = crate::native::WindowCreation::default();
        lifecycle.bind_terminal(submission.receipt()).unwrap();
        drop(submission);
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = called.clone();
        let original = lifecycle.clone();
        lifecycle
            .when_terminal(Box::new(move |terminal| {
                assert_eq!(terminal, NativeWindowTerminal::NotCreated);
                assert!(original.state.try_lock().is_ok());
                seen.store(true, Ordering::Release);
            }))
            .unwrap();
        assert!(called.load(Ordering::Acquire));
    }

    struct Inline;

    impl Thread for Inline {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            job();
        }

        fn is_current(&self) -> bool {
            true
        }
    }

    fn register(table: &ViewTable, window: WindowId) -> (ViewId, RendererGeneration) {
        table
            .register_view(table.allocate_view().unwrap(), window, Surface::Docs)
            .unwrap()
    }

    #[test]
    fn view_id_allocation_exhausts_without_wrapping() {
        let next = AtomicU64::new(u64::from(u32::MAX));
        assert_eq!(allocate_view_id(&next).unwrap(), ViewId(u32::MAX));
        assert!(allocate_view_id(&next).is_err());
        assert!(allocate_view_id(&next).is_err());
    }

    #[test]
    fn dropped_view_removal_wake_reports_failure_and_can_retry() {
        struct Dropped;
        impl Thread for Dropped {
            fn post(&self, _: Box<dyn FnOnce() + Send>) {}
            fn is_current(&self) -> bool {
                false
            }
        }
        let delivery = Arc::new(ViewRemovalDelivery {
            view: ViewId(1),
            core: Arc::new(Dropped),
            state: Mutex::new(ViewRemovalDeliveryState {
                ready: true,
                ..Default::default()
            }),
        });
        let errors = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..2 {
            let errors = errors.clone();
            delivery.start(Box::new(move |result| {
                if result.is_err() {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        assert_eq!(errors.load(Ordering::Relaxed), 2);
        assert!(!delivery.state.lock().unwrap().complete);
    }

    #[test]
    fn legacy_detach_and_native_proof_join_the_same_teardown() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let (view, created) = register(&table, WindowId(1));
        let proof = crate::host::window::NativeClosureProof::new(
            crate::native::BrowserClose::default(),
            view,
            created.clone(),
        );
        table
            .set_closure_proof(view, &created, proof.clone())
            .unwrap();
        let notifications = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = notifications.clone();
        table.on_destroyed(Box::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
        }));
        let completions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first = completions.clone();
        let delivery = proof.teardown().unwrap();
        delivery.start(Box::new(move |result| {
            assert!(result.is_ok());
            first.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(completions.load(Ordering::Relaxed), 0);
        table.remove_view(view);
        let second = completions.clone();
        delivery.start(Box::new(move |result| {
            assert!(result.is_ok());
            second.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(completions.load(Ordering::Relaxed), 2);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn terminal_removal_distinguishes_replaced_and_missing_records() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let ticket = table.allocate_view().unwrap();
        let view = ticket.view();
        let window = WindowId(181);
        table.add_window(window, "terminal-window".into());
        let (_, created) = table.register_view(ticket, window, Surface::Docs).unwrap();
        let actual = crate::host::window::NativeClosureProof::new(
            crate::native::BrowserClose::default(),
            view,
            created.clone(),
        );
        table.set_closure_proof(view, &created, actual).unwrap();
        let (issuer, reservation, owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, created);
        let receipt = issuer.closed();
        assert!(matches!(
            table
                .remove_view_after_terminal(view, &reservation, &owner, &receipt)
                .unwrap(),
            TerminalViewRemoval::Replaced
        ));
        assert!(table.sender_of(view).is_some());
        let absent = table.allocate_view().unwrap().view();
        let (issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(
            absent,
            RendererGeneration::new(),
        );
        assert!(matches!(
            table
                .remove_view_after_terminal(absent, &reservation, &owner, &issuer.closed())
                .unwrap(),
            TerminalViewRemoval::Absent
        ));
    }

    #[test]
    fn successor_terminal_cannot_remove_the_original_owners_view() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let ticket = table.allocate_view().unwrap();
        let view = ticket.view();
        let window = WindowId(183);
        table.add_window(window, "successor-window".into());
        let (_, created) = table.register_view(ticket, window, Surface::Docs).unwrap();
        let (issuer, reservation, owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, created);
        reservation.with_initial_owner(&owner, |_| Ok(())).unwrap();
        reservation
            .prepare_replacement(&owner)
            .unwrap()
            .commit(|_, _| Ok(()))
            .unwrap();
        assert!(
            table
                .remove_view_after_terminal(view, &reservation, &owner, &issuer.closed())
                .is_err()
        );
        assert!(table.sender_of(view).is_some());
    }

    #[test]
    fn native_terminal_delivery_retains_event_after_dropped_wake() {
        struct Dropped;
        impl Thread for Dropped {
            fn post(&self, _: Box<dyn FnOnce() + Send>) {}
            fn is_current(&self) -> bool {
                false
            }
        }
        let (issuer, reservation, owner) = shell_contract::ipc::NativeClosureIssuer::new_with_owner(
            ViewId(42),
            RendererGeneration::new(),
        );
        let event = shell_contract::runtime::NativeViewTerminal {
            view: ViewId(42),
            reservation,
            owner,
            receipt: issuer.not_created(),
        };
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let listener = Arc::new(NativeTerminalListener {
            active: Arc::new(AtomicBool::new(true)),
            callback: Box::new(move |_| {
                seen.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }),
        });
        let state = Arc::new(Mutex::new(NativeTerminalState::default()));
        state
            .lock()
            .unwrap()
            .pending
            .push_back(NativeTerminalDelivery {
                event,
                listeners: vec![listener],
                next: 0,
            });
        wake_native_terminal_queue(state.clone(), Arc::new(Dropped));
        assert_eq!(state.lock().unwrap().pending.len(), 1);
        let core: Arc<dyn Thread> = Arc::new(Inline);
        assert_eq!(drain_native_terminal_queue(&state, &core).unwrap(), 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(state.lock().unwrap().pending.is_empty());
    }

    #[derive(Default)]
    struct Queued(Mutex<Vec<Box<dyn FnOnce() + Send>>>);

    impl Thread for Queued {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            self.0.lock().unwrap().push(job);
        }
        fn is_current(&self) -> bool {
            false
        }
    }

    #[test]
    fn moved_view_survives_old_window_close() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let old = WindowId(1);
        let new = WindowId(2);
        let old_lifecycle = table.add_window(old, "old".into());
        let new_lifecycle = table.add_window(new, "new".into());
        let (view, generation) = register(&table, old);
        table
            .move_view_between_captured(view, &generation, old, &old_lifecycle, new, &new_lifecycle)
            .unwrap();
        table.remove_window(old);
        assert_eq!(table.sender_of(view).map(|sender| sender.window), Some(new));
        assert_eq!(table.views_of_window(new), vec![view]);
    }

    #[test]
    fn browser_owner_is_published_only_after_exact_adoption() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let window = WindowId(91);
        table.add_window(window, "owner-window".into());
        let (view, created) = register(&table, window);
        let submission = crate::native::BrowserCreation::default();
        let proof = crate::host::window::NativeClosureProof::new(
            submission.receipt(),
            view,
            created.clone(),
        );
        table
            .set_closure_proof(view, &created, proof.clone())
            .unwrap();
        let prepared = table
            .prepare_browser_owner(view, &created, proof.owner())
            .unwrap();
        assert!(prepared.slot.lock().unwrap().is_none());
        assert!(
            proof
                .reservation()
                .with_initial_owner(proof.owner(), |transition| {
                    let _guard = prepared.validate(transition)?;
                    Err::<(), _>("actor rejected".into())
                })
                .is_err()
        );
        assert!(prepared.slot.lock().unwrap().is_none());
        proof
            .reservation()
            .with_initial_owner(proof.owner(), |transition| {
                let guard = prepared.validate(transition)?;
                assert!(table.sender_of(view).is_some());
                guard.commit();
                Ok(())
            })
            .unwrap();
        assert!(
            prepared
                .slot
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .same_owner(proof.owner())
        );
        let rejected = proof
            .reservation()
            .prepare_replacement(proof.owner())
            .unwrap();
        let rejected_owner = table
            .prepare_browser_owner(view, &created, rejected.successor())
            .unwrap();
        assert!(
            rejected
                .commit(|transition, _| {
                    let _guard = rejected_owner.validate(transition)?;
                    Err::<(), _>("replacement rejected".into())
                })
                .is_err()
        );
        assert!(
            prepared
                .slot
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .same_owner(proof.owner())
        );
        let replacement = proof
            .reservation()
            .prepare_replacement(proof.owner())
            .unwrap();
        let successor = replacement.successor().clone();
        let prepared_next = table
            .prepare_browser_owner(view, &created, &successor)
            .unwrap();
        replacement
            .commit(|transition, _| {
                let guard = prepared_next.validate(transition)?;
                guard.commit();
                Ok(())
            })
            .unwrap();
        assert!(
            prepared
                .slot
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .same_owner(&successor)
        );
        let foreign = crate::host::window::NativeClosureProof::new(
            submission.receipt(),
            view,
            created.clone(),
        );
        assert!(
            table
                .prepare_browser_owner(view, &created, foreign.owner())
                .is_err()
        );
        let stale_replacement = proof.reservation().prepare_replacement(&successor).unwrap();
        let stale_owner = table
            .prepare_browser_owner(view, &created, stale_replacement.successor())
            .unwrap();
        table.remove_view(view);
        assert!(
            created
                .with_active(|| stale_replacement.commit(|transition, _| {
                    stale_owner.validate(transition)?.commit();
                    Ok(())
                }))
                .is_none()
        );
    }

    #[test]
    fn closing_move_uses_creation_after_renderer_replacement() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let source = WindowId(41);
        let target = WindowId(42);
        let source_lifecycle = table.add_window(source, "source".into());
        let target_lifecycle = table.add_window(target, "target".into());
        let (view, creation) = register(&table, source);
        let submission = crate::native::BrowserCreation::default();
        let proof = crate::host::window::NativeClosureProof::new(
            submission.receipt(),
            view,
            creation.clone(),
        );
        table
            .set_closure_proof(view, &creation, proof.clone())
            .unwrap();
        let prepared = table
            .prepare_browser_owner(view, &creation, proof.owner())
            .unwrap();
        proof
            .reservation()
            .with_initial_owner(proof.owner(), |transition| {
                prepared.validate(transition)?.commit();
                Ok(())
            })
            .unwrap();
        table.replace_renderer(view, &creation).unwrap();
        let shell_contract::ipc::NativeBrowserCloseClaim::Won { mut permit, .. } = proof
            .reservation()
            .claim_close(proof.owner(), |_| Ok(()))
            .unwrap()
        else {
            panic!("close must be admitted")
        };
        let replaced_target = table.add_window(target, "replacement".into());
        assert!(
            permit
                .with_closing(|transition| table.move_view_between_closing(
                    view,
                    &creation,
                    proof.owner(),
                    source,
                    &source_lifecycle,
                    target,
                    &target_lifecycle,
                    transition,
                ))
                .is_err()
        );
        assert_eq!(table.sender_of(view).unwrap().window, source);
        permit
            .with_closing(|transition| {
                table.move_view_between_closing(
                    view,
                    &creation,
                    proof.owner(),
                    source,
                    &source_lifecycle,
                    target,
                    &replaced_target,
                    transition,
                )
            })
            .unwrap();
        assert_eq!(table.sender_of(view).unwrap().window, target);
    }

    #[test]
    fn captured_move_rejects_replaced_renderer() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let old_window = WindowId(1);
        let new_window = WindowId(2);
        let old_lifecycle = table.add_window(old_window, "old".into());
        let new_lifecycle = table.add_window(new_window, "new".into());
        let (view, old) = register(&table, old_window);
        let current = table.replace_renderer(view, &old).unwrap();
        assert!(
            table
                .move_view_between_captured(
                    view,
                    &old,
                    old_window,
                    &old_lifecycle,
                    new_window,
                    &new_lifecycle,
                )
                .is_err()
        );
        assert_eq!(table.sender_of(view).unwrap().window, old_window);
        table
            .move_view_between_captured(
                view,
                &current,
                old_window,
                &old_lifecycle,
                new_window,
                &new_lifecycle,
            )
            .unwrap();
        assert_eq!(table.sender_of(view).unwrap().window, new_window);
    }

    #[test]
    fn renderer_replacement_is_fenced_and_keeps_the_view() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let (view, _) = register(&table, WindowId(1));
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        table.on_renderer_replaced(Box::new(move |id, generation| {
            captured.lock().unwrap().push((id, generation))
        }));
        let old = table.renderer_generation(view).unwrap();
        let next = table.replace_renderer(view, &old).unwrap();
        assert!(!old.is_active());
        assert!(next.is_active());
        assert!(table.replace_renderer(view, &old).is_none());
        assert!(
            table
                .renderer_generation(view)
                .unwrap()
                .same_generation(&next)
        );
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, view);
        assert!(events[0].1.same_generation(&old));
        assert!(table.sender_of(view).is_some());
    }

    #[test]
    fn registration_ticket_is_bound_to_its_table_and_never_reused() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let other = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let ticket = table.allocate_view().unwrap();
        let abandoned = ticket.view();
        assert!(
            other
                .register_view(ticket, WindowId(1), Surface::Docs)
                .is_err()
        );
        assert!(table.sender_of(abandoned).is_none());
        assert!(other.sender_of(abandoned).is_none());
        let (view, _) = register(&table, WindowId(1));
        assert_ne!(view, abandoned);
    }

    #[test]
    fn stale_child_cannot_resurrect_a_removed_view() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let window = WindowId(1);
        let lifecycle = table.add_window(window, "window".into());
        let (view, old) = register(&table, window);
        table.remove_view(view);
        let (next, current) = register(&table, window);
        assert_ne!(view, next);
        assert!(
            table
                .validate_reserved_view(view, &old, window, Surface::Docs, &lifecycle)
                .is_err()
        );
        assert!(
            table
                .validate_reserved_view(next, &current, window, Surface::Docs, &lifecycle)
                .is_ok()
        );
    }

    #[test]
    fn captured_reserved_state_obeys_renderer_generation() {
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), Arc::new(Inline));
        let (view, old) = register(&table, WindowId(1));
        let reserved = table
            .inner
            .lock()
            .unwrap()
            .views
            .get(&view)
            .unwrap()
            .reserved
            .clone();
        assert!(old.with_active(|| reserved.set_zoom(1.5)).is_some());
        let next = table.replace_renderer(view, &old).unwrap();
        assert!(old.with_active(|| reserved.set_zoom(3.0)).is_none());
        assert_eq!(reserved.zoom(), 1.5);
        assert!(next.with_active(|| reserved.set_zoom(2.0)).is_some());
        assert_eq!(table.zoom(view), 2.0);
        table.remove_view(view);
        assert!(next.with_active(|| reserved.set_zoom(4.0)).is_none());
        let (replacement, _) = register(&table, WindowId(1));
        assert_eq!(table.zoom(replacement), 1.0);
    }

    #[test]
    fn delayed_replacement_notification_identifies_only_old_waiters() {
        let queue = Arc::new(Queued::default());
        let table = ViewTable::new(Arc::new(TauriOutlet::default()), queue.clone());
        let (view, _) = register(&table, WindowId(1));
        let waiters = Arc::new(Mutex::new(Vec::<RendererGeneration>::new()));
        let pending = waiters.clone();
        table.on_renderer_replaced(Box::new(move |_, old| {
            pending
                .lock()
                .unwrap()
                .retain(|generation| !generation.same_generation(&old));
        }));
        let old = table.renderer_generation(view).unwrap();
        waiters.lock().unwrap().push(old.clone());
        let next = table.replace_renderer(view, &old).unwrap();
        waiters.lock().unwrap().push(next.clone());
        let jobs = std::mem::take(&mut *queue.0.lock().unwrap());
        for job in jobs {
            job();
        }
        let pending = waiters.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].same_generation(&next));
    }

    #[test]
    fn asset_revocation_serializes_with_a_commit_and_leaves_new_lifetime_live() {
        let old = AssetViewLease::new();
        let new = AssetViewLease::new();
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observer_value = observed.clone();
        let observer_lease = old.clone();
        let _subscription = old.0.loss.on_cancel(move || {
            observer_value.store(!observer_lease.is_live(), Ordering::SeqCst);
        });
        let commit = old.0.live.lock().unwrap();
        let revoked = old.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            revoked.revoke();
            done_tx.send(()).unwrap();
        });
        entered_rx.recv().unwrap();
        assert!(done_rx.try_recv().is_err());
        drop(commit);
        done_rx.recv().unwrap();
        worker.join().unwrap();
        assert!(!old.is_live());
        assert!(observed.load(Ordering::SeqCst));
        assert!(old.0.loss.is_cancelled());
        assert!(new.is_live());
    }

    #[test]
    fn asset_pair_fence_deduplicates_aliases_and_rejects_either_lost_lease() {
        let first = AssetViewLease::new();
        let second = AssetViewLease::new();
        assert_eq!(first.with_live_pair(&first.clone(), || 1), Some(1));
        assert_eq!(first.with_live_pair(&second, || 2), Some(2));
        assert_eq!(second.with_live_pair(&first, || 3), Some(3));
        first.revoke();
        assert!(
            first
                .with_live_pair(&second, || panic!("revoked lease admitted"))
                .is_none()
        );
        assert!(
            second
                .with_live_pair(&first, || panic!("revoked lease admitted"))
                .is_none()
        );
        assert!(second.is_live());
    }

    #[test]
    fn slides_lease_fence_deduplicates_and_revokes_before_notifying() {
        let first = SlidesViewLease::new();
        let second = SlidesViewLease::new();
        assert_eq!(
            SlidesViewLease::with_lease_active_pair([Some(&first), Some(&first)], || 1),
            Some(1)
        );
        assert_eq!(
            SlidesViewLease::with_lease_active_pair([Some(&second), Some(&first)], || 2),
            Some(2)
        );
        let observed = Arc::new(AtomicBool::new(false));
        let flag = observed.clone();
        let captured = first.clone();
        let _subscription = first
            .loss_token()
            .on_cancel(move || flag.store(!captured.is_active(), Ordering::SeqCst));
        first.revoke();
        assert!(observed.load(Ordering::SeqCst));
        assert!(
            SlidesViewLease::with_lease_active_pair([Some(&second), Some(&first)], || panic!(
                "stale slides admitted"
            ))
            .is_none()
        );
        assert!(second.is_active());
    }
}
