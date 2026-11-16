//! Windows and views (the stage 3 shell contract, section 1): Electron's
//! `BrowserWindow` is a Tauri window and a `webContents` is a webview. A
//! window made by [`Windows::open_window`] has its first view as a webview
//! window; further views (the shell window's tabs were `WebContentsView`s)
//! are children of it ([`Windows::add_view`]).
//!
//! Every view carries a [`Bridge`]: the preload of its surface as the
//! webview's initialization script, and an entry in the [`ViewTable`] that
//! attributes its messages. A view with no bridge is for a page that is not
//! ours; it can message nothing and its navigation is not restricted.
//!
//! Callable from any thread; from the core thread a call waits for the UI
//! thread, from the UI thread it runs at once.

use std::sync::{Arc, OnceLock};

use shell_contract::hosts::MainWindow;
use shell_contract::ids::{ViewId, WindowId};
use shell_contract::runtime::RendererGeneration;
use shell_contract::{Hosts, MainWindowHost, Surface};
use tauri::webview::{NewWindowResponse, WebviewBuilder};
use tauri::{
    AppHandle, LogicalPosition, LogicalSize, Manager, Webview, WebviewUrl, WebviewWindowBuilder,
    WindowBuilder,
};

use crate::native::{
    self, BridgeMode, BrowserCreation, DocumentContext, Native, NativeEvent, NativeEventKind,
    NativeSpec, NavigationStart, RequestPolicy, ViewEvent, WebviewBuilderExt,
    WebviewWindowBuilderExt, WindowBuilderExt, WindowCreation,
};

use super::{HostContext, window_open};
use crate::app_scheme;
use crate::preload;
use crate::trace::{self, trace};
use crate::views::ViewTable;

#[derive(Clone)]
pub(crate) struct NativeClosureProof {
    close: native::BrowserClose,
    reservation: shell_contract::ipc::NativeClosureReservation,
    owner: shell_contract::ipc::NativeBrowserOwner,
    state: Arc<std::sync::Mutex<NativeClosureProofState>>,
    teardown: Arc<OnceLock<Arc<crate::views::ViewRemovalDelivery>>>,
}

struct NativeClosureProofState {
    issuer: Option<shell_contract::ipc::NativeClosureIssuer>,
    receipt: Option<shell_contract::ipc::NativeClosureReceipt>,
}

impl NativeClosureProof {
    pub(crate) fn new(
        close: native::BrowserClose,
        view: ViewId,
        created: shell_contract::runtime::RendererGeneration,
    ) -> Self {
        let (issuer, reservation, owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, created);
        Self {
            close,
            reservation,
            owner,
            teardown: Arc::new(OnceLock::new()),
            state: Arc::new(std::sync::Mutex::new(NativeClosureProofState {
                issuer: Some(issuer),
                receipt: None,
            })),
        }
    }

    pub(crate) fn reservation(&self) -> &shell_contract::ipc::NativeClosureReservation {
        &self.reservation
    }
    pub(crate) fn owner(&self) -> &shell_contract::ipc::NativeBrowserOwner {
        &self.owner
    }
    pub(crate) fn close(&self) -> native::BrowserClose {
        self.close.clone()
    }

    pub(crate) fn bind_teardown(
        &self,
        teardown: Arc<crate::views::ViewRemovalDelivery>,
    ) -> Result<(), String> {
        self.teardown
            .set(teardown)
            .map_err(|_| "The native view teardown is already bound".into())
    }

    pub(crate) fn teardown(&self) -> Option<Arc<crate::views::ViewRemovalDelivery>> {
        self.teardown.get().cloned()
    }

    pub(crate) fn dispatch_close(
        &self,
        mut permit: shell_contract::ipc::NativeBrowserClosePermit,
    ) -> Result<
        shell_contract::ipc::NativeBrowserClosePermit,
        (String, shell_contract::ipc::NativeBrowserClosePermit),
    > {
        if !permit.reservation().same_reservation(&self.reservation) {
            return Err((
                "The native close permit belongs to another browser".into(),
                permit,
            ));
        }
        if let Err(error) = permit.begin_dispatch() {
            return Err((error, permit));
        }
        match self.close.request_close() {
            Ok(()) => Ok(permit),
            Err(error) => {
                let result = permit.dispatch_failed();
                let error = match result {
                    Ok(()) => error,
                    Err(reset) => format!("{error}; close retry state failed: {reset}"),
                };
                Err((error, permit))
            }
        }
    }

    pub(crate) fn when_closed(
        &self,
        callback: impl FnOnce(shell_contract::ipc::NativeClosureReceipt) + Send + 'static,
    ) -> Result<native::BrowserCloseSubscription, String> {
        let state = self.state.clone();
        self.close.when_closed(move |outcome| {
            let receipt = {
                let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                if let Some(receipt) = &state.receipt {
                    receipt.clone()
                } else {
                    let Some(issuer) = state.issuer.take() else {
                        return;
                    };
                    let receipt = match outcome {
                        native::BrowserCloseOutcome::Retired => issuer.closed(),
                        native::BrowserCloseOutcome::NotCreated => issuer.not_created(),
                    };
                    state.receipt = Some(receipt.clone());
                    receipt
                }
            };
            callback(receipt);
        })
    }

    pub(crate) fn watch(
        &self,
        callback: Box<dyn FnOnce(shell_contract::ipc::NativeClosureReceipt) + Send>,
    ) -> Result<(), String> {
        let retained = Arc::new(std::sync::Mutex::new((false, None)));
        let callback_retained = retained.clone();
        let subscription = self.when_closed(move |receipt| {
            let subscription = {
                let mut state = callback_retained
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state.0 = true;
                state.1.take()
            };
            drop(subscription);
            callback(receipt);
        })?;
        let mut subscription = Some(subscription);
        {
            let mut state = retained.lock().unwrap_or_else(|error| error.into_inner());
            if !state.0 {
                state.1 = subscription.take();
            }
        }
        drop(subscription);
        Ok(())
    }
}

impl shell_ipc_w::tabs::host::NativeBrowserCloseProducer for NativeClosureProof {
    fn watch(
        &self,
        done: Box<dyn FnOnce(shell_contract::ipc::NativeClosureReceipt) + Send>,
    ) -> Result<(), String> {
        NativeClosureProof::watch(self, done)
    }

    fn dispatch_close(
        &self,
        permit: shell_contract::ipc::NativeBrowserClosePermit,
    ) -> Result<
        shell_contract::ipc::NativeBrowserClosePermit,
        (String, shell_contract::ipc::NativeBrowserClosePermit),
    > {
        NativeClosureProof::dispatch_close(self, permit)
    }
}

pub(crate) struct CreatedChild {
    pub(crate) view: ViewId,
    pub(crate) webview: Webview,
    pub(crate) close: native::BrowserClose,
    pub(crate) proof: Option<NativeClosureProof>,
}
pub(crate) struct PreparedDeferredWindow {
    pub(crate) window: WindowId,
    pub(crate) view: ViewId,
    pub(crate) created: RendererGeneration,
    pub(crate) proof: NativeClosureProof,
    pub(crate) window_lifecycle: Arc<crate::views::NativeWindowLifecycle>,
    surface: Surface,
    submission: BrowserCreation,
    window_submission: WindowCreation,
}

pub(crate) struct ChildCreationError {
    pub(crate) message: String,
    pub(crate) close: Option<native::BrowserClose>,
    pub(crate) proof: Option<NativeClosureProof>,
}

impl From<String> for ChildCreationError {
    fn from(message: String) -> Self {
        Self {
            message,
            close: None,
            proof: None,
        }
    }
}

type FrameCallback = Arc<dyn Fn(ViewId, &NativeEvent) + Send + Sync>;
type NavigationFilter = Arc<dyn Fn(&tauri::Url) -> bool + Send + Sync>;

/// A native event of `view`: a main frame whose web content process failed goes to the
/// crash watch always; the rest is a trace line
fn frame_event(view: ViewId, label: &str, event: &NativeEvent) {
    if matches!(event.kind, NativeEventKind::WebviewProcessFailed { .. }) {
        super::crash::renderer_terminated(view);
    }
    if trace::enabled() {
        frame_line(label, event);
    }
}

fn view_event(view: ViewId, event: &NativeEvent) {
    let event = match &event.kind {
        NativeEventKind::View(ViewEvent::Focus) => {
            Some(shell_ipc_w::projects::PageEvent::Focus(view))
        }
        NativeEventKind::View(ViewEvent::Blur) => {
            Some(shell_ipc_w::projects::PageEvent::Blur(view))
        }
        NativeEventKind::NavigationStarted {
            same_document: false,
            ..
        } => Some(shell_ipc_w::projects::PageEvent::Navigated(view)),
        _ => None,
    };
    if let Some(event) = event {
        super::w_projects::platform::page_event(event);
    }
}

/// The events worth a line: where a page went and whether it loaded
fn frame_line(label: &str, event: &NativeEvent) {
    let what = match &event.kind {
        NativeEventKind::NavigationStarted { url, .. } => format!("navigation started {url}"),
        NativeEventKind::DocumentCommitted { url, .. } => format!("document committed {url}"),
        NativeEventKind::NavigationFailed { url, error, .. } => {
            format!("NAVIGATION FAILED {url} {error}")
        }
        NativeEventKind::LoadFinished { ok, .. } => format!("load finished ok={ok}"),
        NativeEventKind::WebviewProcessFailed { kind } => {
            format!("WEBVIEW PROCESS FAILED {kind:?}")
        }
        _ => return,
    };
    trace!("frame", "{label} {what}");
}

/// One sink per browser: the shell's own handling first, then the caller's callback.
fn event_sink(view: ViewId, label: String, on_frame: Option<FrameCallback>) -> native::EventSink {
    Arc::new(move |event| {
        crate::native::menu::note_view_event(event);
        frame_event(view, &label, event);
        view_event(view, event);
        if let Some(callback) = &on_frame {
            callback(view, event);
        }
    })
}

/// What a view with a bridge may load: the shell's own pages, and what the caller allows.
/// Without a bridge the caller's policy stands alone.
fn view_policy(
    bridge: Bridge,
    base: Option<RequestPolicy>,
    navigation: Option<NavigationFilter>,
) -> Option<RequestPolicy> {
    if bridge.surface().is_none() {
        return base;
    }
    let javascript = base.as_ref().is_none_or(RequestPolicy::javascript);
    let storage = base.as_ref().map(RequestPolicy::storage);
    let blocks = base.as_ref().map(RequestPolicy::blocks);
    let policy = RequestPolicy::new(move |request| {
        if !base.as_ref().is_none_or(|base| base.allows(request)) {
            return false;
        }
        if !request.main_frame {
            return true;
        }
        tauri::Url::parse(request.url).is_ok_and(|url| {
            app_scheme::is_own_page(&url) && navigation.as_ref().is_none_or(|allow| allow(&url))
        })
    })
    .with_javascript(javascript);
    let policy = match blocks {
        Some(blocks) => policy.with_blocks(blocks),
        None => policy,
    };
    Some(match storage {
        Some(storage) => policy.with_storage(storage),
        None => policy,
    })
}

/// `Native::attach` runs on the UI thread, after the build returned.
fn attach_native(
    app: &AppHandle,
    native: &Native,
    webview: &Webview,
    browser: &native::BrowserIdentity,
) -> Result<(), String> {
    if native::defer::is_ui_thread() {
        return native.attach(webview, browser);
    }
    let (native, webview, browser) = (native.clone(), webview.clone(), browser.clone());
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = send.send(native.attach(&webview, &browser));
    })
    .map_err(|error| error.to_string())?;
    receive.recv().map_err(|error| error.to_string())?
}

/// How a view talks to the shell
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bridge {
    /// no bridge: a page that is not ours
    #[allow(dead_code)]
    // for the first page that is not ours (a sign-in window); batch W's views
    None,
    /// the preload of this surface
    Surface(Surface),
    /// `--measure-dialog`'s page: the bridge, as the shell surface
    Measure,
}

impl Bridge {
    fn surface(self) -> Option<Surface> {
        match self {
            Bridge::None => None,
            Bridge::Surface(surface) => Some(surface),
            Bridge::Measure => Some(Surface::Shell),
        }
    }

    // This mode comes from native surface construction, never page data.
    fn bridge_mode(self) -> BridgeMode {
        match self {
            Bridge::None => BridgeMode::None,
            Bridge::Surface(_) | Bridge::Measure => BridgeMode::Preload,
        }
    }

    /// The initialization script; with the trace on, the bridge is told to log its stages
    fn script(self) -> Option<String> {
        let source = match self {
            Bridge::None => return None,
            Bridge::Surface(surface) => preload::source(surface),
            Bridge::Measure => preload::MEASURE,
        };
        Some(if trace::enabled() {
            format!("window.__SEUOFFICE_TRACE__ = true;\n{source}")
        } else {
            source.to_owned()
        })
    }
}

pub struct ViewSpec {
    /// A page of a renderer bundle, named as the handlers name it
    /// (`genoffice-app://docs/index.html?mode=tab`); [`app_scheme::webview_url`] translates
    pub url: String,
    pub bridge: Bridge,
}

pub struct WindowSpec {
    pub title: String,
    /// logical pixels
    pub size: (f64, f64),
    pub min_size: Option<(f64, f64)>,
    pub visible: bool,
    /// the OS title bar and borders (Electron's `frame`)
    pub decorations: bool,
    pub transparent: bool,
    pub resizable: bool,
    /// a child window of this one (Electron's `parent`): above it, and gone with it
    pub parent: Option<WindowId>,
    /// takes the focus when it shows (false: Electron's `showInactive`)
    pub focus: bool,
    /// this window's first view is the shell window's (what [`MainWindow`] answers)
    pub main: bool,
    pub view: ViewSpec,
}

pub struct WindowOptions {
    pub center: bool,
    pub always_on_top: bool,
    pub minimizable: bool,
    pub maximizable: bool,
    pub fullscreenable: bool,
    pub shadow: bool,
    pub background_throttling: Option<tauri::utils::config::BackgroundThrottlingPolicy>,
    pub request_policy: Option<RequestPolicy>,
    pub on_frame: Option<FrameCallback>,
    pub on_open: Option<OpenCallback>,
    pub on_navigation: Option<NavigationCallback>,
    pub on_closed: Option<Box<dyn FnOnce(WindowId) + Send>>,
}

pub type OpenCallback = Arc<dyn Fn(ViewId, &DocumentContext, &str) + Send + Sync>;
pub type NavigationCallback = Arc<dyn Fn(&tauri::Url) -> bool + Send + Sync>;

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            center: false,
            always_on_top: false,
            minimizable: true,
            maximizable: true,
            fullscreenable: true,
            shadow: true,
            background_throttling: None,
            request_policy: None,
            on_frame: None,
            on_open: None,
            on_navigation: None,
            on_closed: None,
        }
    }
}

#[cfg(target_os = "macos")]
fn disable_native_fullscreen(app: &AppHandle, window: &tauri::WebviewWindow) -> Result<(), String> {
    fn apply(window: &tauri::WebviewWindow) -> Result<(), String> {
        use crate::native::WindowExt;
        use objc2::rc::Retained;
        use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};

        let handle = window
            .as_ref()
            .window()
            .native_window_handle()
            .map_err(|error| error.to_string())?;
        handle
            .with_window(|_| {
                let _main = objc2::MainThreadMarker::new()
                    .ok_or_else(|| "not on the main thread".to_owned())?;
                let pointer = window.ns_window().map_err(|error| error.to_string())?;
                // SAFETY: the creation-bound native window remains admitted for this operation.
                let native = unsafe { Retained::retain(pointer.cast::<NSWindow>()) }
                    .ok_or_else(|| "window has no native NSWindow".to_owned())?;
                let mut behavior = native.collectionBehavior();
                behavior.remove(
                    NSWindowCollectionBehavior::FullScreenPrimary
                        | NSWindowCollectionBehavior::FullScreenAuxiliary,
                );
                behavior.insert(NSWindowCollectionBehavior::FullScreenNone);
                native.setCollectionBehavior(behavior);
                Ok(())
            })
            .map_err(|error| error.to_string())?
    }

    if objc2::MainThreadMarker::new().is_some() {
        return apply(window);
    }
    let window = window.clone();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = send.send(apply(&window));
    })
    .map_err(|error| error.to_string())?;
    receive.recv().map_err(|error| error.to_string())?
}

#[cfg(not(target_os = "macos"))]
fn disable_native_fullscreen(_: &AppHandle, _: &tauri::WebviewWindow) -> Result<(), String> {
    Ok(())
}

type ClosedCallback = Box<dyn FnOnce(WindowId) + Send>;
static CLOSED_CALLBACKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<WindowId, ClosedCallback>>,
> = std::sync::LazyLock::new(Default::default);

pub(crate) fn window_destroyed(window: WindowId) {
    let callback = CLOSED_CALLBACKS.lock().unwrap().remove(&window);
    if let Some(callback) = callback {
        callback(window);
    }
}

/// A child view's place in its window, in logical pixels
#[allow(dead_code)] // the tab strip's views, with `add_view`: the WindowsHost that replaces its stub
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub struct Windows {
    app: AppHandle,
    views: Arc<ViewTable>,
}

impl Windows {
    pub fn new(app: AppHandle, views: Arc<ViewTable>) -> Self {
        Windows { app, views }
    }

    fn url_of(url: &str) -> Result<WebviewUrl, String> {
        if url == "about:blank" {
            return tauri::Url::parse(url)
                .map(WebviewUrl::External)
                .map_err(|error| error.to_string());
        }
        app_scheme::webview_url(url)
            .map(WebviewUrl::External)
            .ok_or_else(|| format!("'{url}' is not a renderer page"))
    }

    pub(crate) fn open_empty_window_handle(
        &self,
        title: &str,
        size: (f64, f64),
        min_size: (f64, f64),
        origin: Option<(f64, f64)>,
        visible: bool,
    ) -> Result<
        (
            WindowId,
            tauri::Window,
            Arc<crate::views::NativeWindowLifecycle>,
        ),
        String,
    > {
        let native = self.native()?;
        let creation = WindowCreation::default();
        let id = self.views.new_window_id();
        let label = format!("window-{}", id.0);
        let mut builder = WindowBuilder::new(&self.app, &label)
            .title(title)
            .inner_size(size.0, size.1)
            .min_inner_size(min_size.0, min_size.1)
            .visible(visible);
        if let Some((x, y)) = origin {
            builder = builder.position(x, y);
        }
        let lifecycle = self.views.add_window(id, label.clone());
        if let Err(error) = lifecycle.bind_terminal(creation.receipt()) {
            self.views.remove_window(id);
            return Err(error);
        }
        let builder = match builder.native_window_creation(&native, &label, creation) {
            Ok(builder) => builder,
            Err(error) => {
                self.views.remove_window(id);
                return Err(error);
            }
        };
        match builder.build() {
            Ok(window) => {
                if let Err(error) = native.window_created(&window) {
                    let _ = window.destroy();
                    self.views.remove_window(id);
                    return Err(error);
                }
                if let Err(error) = lifecycle.attach(window.clone()) {
                    let _ = window.destroy();
                    self.views.remove_window(id);
                    return Err(error);
                }
                Ok((id, window, lifecycle))
            }
            Err(error) => {
                native.window_failed(&label);
                self.views.remove_window(id);
                Err(format!("opening a window: {error}"))
            }
        }
    }

    fn native(&self) -> Result<Native, String> {
        self.app
            .try_state::<Native>()
            .map(|state| state.inner().clone())
            .ok_or_else(|| "The native module is not installed.".to_owned())
    }

    pub fn reparent_view(&self, view: ViewId, window: WindowId) -> Result<(), String> {
        let webview = self
            .webview(view)
            .ok_or_else(|| format!("no view {}", view.0))?;
        let label = self
            .views
            .label_of_window(window)
            .ok_or_else(|| format!("no window {}", window.0))?;
        let target = self
            .app
            .get_window(&label)
            .ok_or_else(|| format!("window {} is gone", window.0))?;
        let previous = webview.window();
        if let Err(error) = webview.reparent(&target) {
            return match webview.reparent(&previous) {
                Ok(()) => Err(format!("reparenting view: {error}")),
                Err(rollback) => Err(format!(
                    "reparenting view: {error}; restoring parent: {rollback}"
                )),
            };
        }
        if let Err(error) = self.views.move_view(view, window) {
            return match webview.reparent(&previous) {
                Ok(()) => Err(error),
                Err(rollback) => Err(format!("{error}; restoring parent: {rollback}")),
            };
        }
        Ok(())
    }

    /// A new window with its first view
    pub fn open_window(&self, spec: WindowSpec) -> Result<(WindowId, ViewId), String> {
        self.open_window_with_options(spec, WindowOptions::default())
    }

    pub fn open_window_with_options(
        &self,
        spec: WindowSpec,
        options: WindowOptions,
    ) -> Result<(WindowId, ViewId), String> {
        self.open_window_native(spec, options, false, None)
    }

    fn open_window_native(
        &self,
        spec: WindowSpec,
        options: WindowOptions,
        explicit_navigation: bool,
        reserved: Option<PreparedDeferredWindow>,
    ) -> Result<(WindowId, ViewId), String> {
        let url = Self::url_of(&spec.view.url)?;
        let native = self.native()?;
        let reserved_identity = reserved.as_ref().map(|entry| {
            (
                entry.window,
                entry.view,
                entry.created.clone(),
                entry.surface,
                entry.window_lifecycle.clone(),
            )
        });
        let (window, view, mut registration) = match &reserved_identity {
            Some((window, view, _, _, _)) => (*window, *view, None),
            None => {
                let registration = self.views.allocate_view()?;
                (
                    self.views.new_window_id(),
                    registration.view(),
                    Some(registration),
                )
            }
        };
        if let Some((_, _, created, surface, lifecycle)) = &reserved_identity {
            let bound = self.views.view_window_captured(view, created).ok();
            if spec.view.bridge.surface() != Some(*surface)
                || !bound.is_some_and(|(current, token)| {
                    current == window && Arc::ptr_eq(&token, lifecycle)
                })
            {
                return Err("The reserved native window no longer matches its view".into());
            }
        }
        let (mut browser_creation, window_creation) = match reserved {
            Some(entry) => (Some(entry.submission), Some(entry.window_submission)),
            None => (None, None),
        };
        let label = format!("view-{}", view.0);
        let generation = Arc::new(OnceLock::new());
        let mut builder = WebviewWindowBuilder::new(&self.app, &label, url)
            .title(spec.title)
            .inner_size(spec.size.0, spec.size.1)
            .visible(spec.visible && (options.fullscreenable || !cfg!(target_os = "macos")))
            .decorations(spec.decorations)
            .transparent(spec.transparent)
            .resizable(spec.resizable)
            .focused(spec.focus)
            .always_on_top(options.always_on_top)
            .minimizable(options.minimizable)
            .maximizable(options.maximizable)
            .shadow(options.shadow);
        if let Some(policy) = options.background_throttling {
            builder = builder.background_throttling(policy);
        }
        if options.center {
            builder = builder.center();
        }
        if let Some((w, h)) = spec.min_size {
            builder = builder.min_inner_size(w, h);
        }
        if let Some(parent) = spec.parent {
            let parent = self
                .views
                .label_of_window(parent)
                .and_then(|l| self.app.get_webview_window(&l))
                .ok_or_else(|| format!("no parent window {}", parent.0))?;
            builder = builder
                .parent(&parent)
                .map_err(|e| format!("parenting a window: {e}"))?;
        }
        trace!(
            "window",
            "open window {} view {} '{label}' {} (bridge {:?}, script {} bytes)",
            window.0,
            view.0,
            spec.view.url,
            spec.view.bridge,
            spec.view.bridge.script().map_or(0, |s| s.len())
        );
        if let Some(script) = spec.view.bridge.script() {
            builder = builder.initialization_script(script);
        }
        // The page asks for a popup; the OS opens nothing, the app's policy decides
        let asked_by = spec.view.bridge.surface();
        let popup_views = self.views.clone();
        let popup_generation = generation.clone();
        let popup_native = native.clone();
        let popup_label = label.clone();
        let on_open = options.on_open.clone();
        builder = builder.on_new_window(move |url, _features| {
            if let Some(origin) = popup_native.current_context(&popup_label) {
                if let Some(callback) = &on_open {
                    callback(view, &origin, url.as_str());
                } else if let Some(created) = popup_generation.get() {
                    let _ = window_open::requested(
                        asked_by,
                        &popup_views,
                        view,
                        created,
                        &origin,
                        url.as_str(),
                    );
                }
            }
            NewWindowResponse::Deny
        });
        // known before the page can run: its first message must find its sender
        if let Some(callback) = options.on_closed {
            CLOSED_CALLBACKS.lock().unwrap().insert(window, callback);
        }
        let window_lifecycle = if let Some((_, _, _, _, lifecycle)) = &reserved_identity {
            lifecycle.clone()
        } else {
            self.views.add_window(window, label.clone())
        };
        let window_creation = match window_creation {
            Some(creation) => creation,
            None => {
                let creation = WindowCreation::default();
                if let Err(error) = window_lifecycle.bind_terminal(creation.receipt()) {
                    CLOSED_CALLBACKS.lock().unwrap().remove(&window);
                    self.views.remove_window(window);
                    return Err(error);
                }
                creation
            }
        };
        if let Some(surface) = spec.view.bridge.surface() {
            let captured = if let Some(registration) = registration.take() {
                match self.views.register_view(registration, window, surface) {
                    Ok((_, created)) => created,
                    Err(error) => {
                        CLOSED_CALLBACKS.lock().unwrap().remove(&window);
                        self.views.remove_window(window);
                        return Err(error);
                    }
                }
            } else if let Some((_, _, created, _, _)) = &reserved_identity {
                created.clone()
            } else {
                return Err("The native view registration is unavailable".into());
            };
            if reserved_identity.is_none() {
                let creation = BrowserCreation::default();
                let proof = NativeClosureProof::new(creation.receipt(), view, captured.clone());
                let admitted = (|| {
                    self.views
                        .set_closure_proof(view, &captured, proof.clone())?;
                    let prepared =
                        self.views
                            .prepare_browser_owner(view, &captured, proof.owner())?;
                    proof
                        .reservation()
                        .with_initial_owner(proof.owner(), |transition| {
                            prepared.validate(transition)?.commit();
                            Ok(())
                        })
                })();
                if let Err(error) = admitted {
                    CLOSED_CALLBACKS.lock().unwrap().remove(&window);
                    self.views.remove_window(window);
                    return Err(error);
                }
                browser_creation = Some(creation);
            }
            let _ = generation.set(captured);
            if spec.main && surface == Surface::Shell {
                self.views.mark_main(view);
            }
        }
        let mut native_spec = NativeSpec::new()
            .bridge(spec.view.bridge.bridge_mode())
            .native_events(event_sink(view, label.clone(), options.on_frame));
        if explicit_navigation {
            native_spec = native_spec.navigation_start(NavigationStart::Explicit);
        }
        if let Some(creation) = browser_creation {
            native_spec = native_spec.native_creation(creation);
        }
        if let Some(policy) = view_policy(
            spec.view.bridge,
            options.request_policy,
            options.on_navigation,
        ) {
            native_spec = native_spec.request_policy(policy);
        }
        let abandon = |error: String| -> Result<(WindowId, ViewId), String> {
            CLOSED_CALLBACKS.lock().unwrap().remove(&window);
            if reserved_identity.is_none() {
                self.views.remove_window(window);
            }
            Err(error)
        };
        if let Err(error) = native.reserve_window(&label, window_creation) {
            return abandon(error);
        }
        let (builder, browser) = match builder.native(&native, &label, native_spec) {
            Ok(native_builder) => native_builder,
            Err(error) => {
                native.window_failed(&label);
                return abandon(error);
            }
        };
        match builder.build() {
            Ok(built) => {
                let configured = (|| {
                    native.window_created(&built.as_ref().window())?;
                    window_lifecycle.attach(built.as_ref().window())?;
                    if let Some(created) = generation.get() {
                        crate::ipc::bind_native_webview(built.as_ref(), view, created.clone())?;
                    }
                    attach_native(&self.app, &native, built.as_ref(), &browser)?;
                    if !options.fullscreenable {
                        disable_native_fullscreen(&self.app, &built)?;
                        if cfg!(target_os = "macos") && spec.visible {
                            built.show().map_err(|error| error.to_string())?;
                            if spec.focus {
                                built.set_focus().map_err(|error| error.to_string())?;
                            }
                        }
                    }
                    Ok::<(), String>(())
                })();
                if let Err(error) = configured {
                    CLOSED_CALLBACKS.lock().unwrap().remove(&window);
                    let _ = built.close();
                    if reserved_identity.is_none() {
                        self.views.remove_window(window);
                    }
                    return Err(error);
                }
                trace!("window", "window {} built", window.0);
                Ok((window, view))
            }
            Err(error) => {
                native.window_failed(&label);
                abandon(format!("opening a window: {error}"))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_reserved_view(
        &self,
        view: ViewId,
        created: &RendererGeneration,
        window: WindowId,
        window_lifecycle: &Arc<crate::views::NativeWindowLifecycle>,
        spec: ViewSpec,
        bounds: Bounds,
        options: WindowOptions,
        prepared_proof: Option<NativeClosureProof>,
        submission: Option<BrowserCreation>,
        on_frame: Option<FrameCallback>,
    ) -> Result<CreatedChild, ChildCreationError> {
        let url = Self::url_of(&spec.url)?;
        if !window_lifecycle.belongs_to(window) {
            return Err("The captured parent window does not match"
                .to_owned()
                .into());
        }
        let surface = spec
            .bridge
            .surface()
            .ok_or_else(|| "A reserved child must have a bridge".to_owned())?;
        self.views
            .validate_reserved_view(view, created, window, surface, window_lifecycle)?;
        let native = self.native()?;
        let parent = window_lifecycle.window_handle()?;
        let view_label = format!("view-{}", view.0);
        let generation = Arc::new(OnceLock::new());
        let mut builder = WebviewBuilder::new(&view_label, url);
        trace!(
            "window",
            "add view {} '{view_label}' to window {} {}", view.0, window.0, spec.url
        );
        if let Some(script) = spec.bridge.script() {
            builder = builder.initialization_script(script);
        }
        let asked_by = spec.bridge.surface();
        let popup_views = self.views.clone();
        let popup_generation = generation.clone();
        let popup_native = native.clone();
        let popup_label = view_label.clone();
        builder = builder.on_new_window(move |url, _features| {
            if let (Some(origin), Some(created)) = (
                popup_native.current_context(&popup_label),
                popup_generation.get(),
            ) {
                let _ = window_open::requested(
                    asked_by,
                    &popup_views,
                    view,
                    created,
                    &origin,
                    url.as_str(),
                );
            }
            NewWindowResponse::Deny
        });
        let _ = generation.set(created.clone());
        if let Some(proof) = &prepared_proof
            && !generation
                .get()
                .is_some_and(|created| proof.reservation().matches(view, created))
        {
            return Err("The native closure reservation does not match the view"
                .to_owned()
                .into());
        }
        let creation = match (submission, &prepared_proof) {
            (Some(creation), Some(proof)) => {
                if !proof.close().same_reservation(&creation.receipt()) {
                    return Err("The native closure proof belongs to another creation"
                        .to_owned()
                        .into());
                }
                creation
            }
            (Some(creation), None) => creation,
            (None, None) => BrowserCreation::default(),
            (None, Some(_)) => {
                return Err(
                    "A prepared native closure proof needs its creation reservation"
                        .to_owned()
                        .into(),
                );
            }
        };
        let close = creation.receipt();
        let proof = match prepared_proof {
            Some(proof) => Some(proof),
            None => {
                let proof = NativeClosureProof::new(close.clone(), view, created.clone());
                self.views
                    .set_closure_proof(view, created, proof.clone())
                    .map_err(ChildCreationError::from)?;
                Some(proof)
            }
        };
        let mut native_spec = NativeSpec::new()
            .bridge(spec.bridge.bridge_mode())
            .native_creation(creation)
            .native_events(event_sink(view, view_label.clone(), on_frame));
        if let Some(policy) =
            view_policy(spec.bridge, options.request_policy, options.on_navigation)
        {
            native_spec = native_spec.request_policy(policy);
        }
        let (builder, browser) = match builder.native(&native, &view_label, native_spec) {
            Ok(native_builder) => native_builder,
            Err(error) => {
                return Err(ChildCreationError {
                    message: error,
                    close: Some(close),
                    proof,
                });
            }
        };
        match parent.add_child(
            builder,
            LogicalPosition::new(bounds.x, bounds.y),
            LogicalSize::new(bounds.width, bounds.height),
        ) {
            Ok(built) => {
                if let Some(created) = generation.get()
                    && let Err(error) = crate::ipc::bind_native_webview(&built, view, created.clone())
                {
                    return Err(ChildCreationError {
                        message: error,
                        close: Some(close),
                        proof,
                    });
                }
                if let Err(error) = attach_native(&self.app, &native, &built, &browser) {
                    return Err(ChildCreationError {
                        message: error,
                        close: Some(close),
                        proof,
                    });
                }
                Ok(CreatedChild {
                    view,
                    webview: built,
                    close,
                    proof,
                })
            }
            Err(error) => Err(ChildCreationError {
                message: format!("adding a view: {error}"),
                close: Some(close),
                proof,
            }),
        }
    }

    pub(crate) fn reserve_deferred_window(
        &self,
        surface: Surface,
    ) -> Result<PreparedDeferredWindow, String> {
        let window_submission = WindowCreation::default();
        let submission = BrowserCreation::default();
        let window = self.views.new_window_id();
        let registration = self.views.allocate_view()?;
        let view = registration.view();
        let label = format!("view-{}", view.0);
        let window_lifecycle = self.views.add_window(window, label.clone());
        if let Err(error) = window_lifecycle.bind_terminal(window_submission.receipt()) {
            self.views.remove_window(window);
            return Err(error);
        }
        let (_, created) = match self.views.register_view(registration, window, surface) {
            Ok(registered) => registered,
            Err(error) => {
                self.views.remove_window(window);
                return Err(error);
            }
        };
        let proof = NativeClosureProof::new(submission.receipt(), view, created.clone());
        if let Err(error) = self.views.set_closure_proof(view, &created, proof.clone()) {
            self.views.remove_view_if_generation(view, &created);
            let _ = self.views.remove_window_captured(window, &window_lifecycle);
            return Err(error);
        }
        Ok(PreparedDeferredWindow {
            window,
            view,
            created,
            proof,
            window_lifecycle,
            surface,
            submission,
            window_submission,
        })
    }

    pub(crate) fn open_reserved_deferred_window(
        &self,
        mut spec: WindowSpec,
        options: WindowOptions,
        reservation: PreparedDeferredWindow,
    ) -> Result<(WindowId, ViewId), String> {
        spec.view.url = "about:blank".to_owned();
        spec.visible = false;
        self.open_window_native(spec, options, true, Some(reservation))
    }

    pub fn open_deferred_window(
        &self,
        mut spec: WindowSpec,
        options: WindowOptions,
    ) -> Result<(WindowId, ViewId), String> {
        spec.view.url = "about:blank".to_owned();
        spec.visible = false;
        self.open_window_native(spec, options, true, None)
    }

    pub fn load_view(&self, view: ViewId, url: &str) -> Result<(), String> {
        let url = app_scheme::webview_url(url)
            .ok_or_else(|| format!("'{url}' is not a renderer page"))?;
        self.webview(view)
            .ok_or_else(|| format!("no view {}", view.0))?
            .navigate(url)
            .map_err(|error| error.to_string())
    }

    /// `webContents.reload()`; `false` when the view is gone or the reload was refused
    pub fn reload(&self, view: ViewId) -> bool {
        let Some(webview) = self.webview(view) else {
            return false;
        };
        match webview.reload() {
            Ok(()) => true,
            Err(error) => {
                eprintln!("shell: reload view {}: {error}", view.0);
                false
            }
        }
    }

    /// The Tauri webview of a view
    pub fn webview(&self, view: ViewId) -> Option<Webview> {
        self.app.get_webview(&self.views.label_of_view(view)?)
    }

    #[allow(dead_code)] // the tab strip's views: the WindowsHost that replaces its stub
    pub fn close_view(&self, view: ViewId) -> Result<(), String> {
        let webview = self
            .webview(view)
            .ok_or_else(|| format!("no view {}", view.0))?;
        self.views.remove_view(view);
        webview.close().map_err(|e| e.to_string())
    }

    /// Shows a window, taking the focus or not (Electron's `show` and `showInactive`)
    #[allow(dead_code)] // U's update window and the WindowsHost that replaces its stub
    pub fn show(&self, window: WindowId, focus: bool) -> Result<(), String> {
        let label = self
            .views
            .label_of_window(window)
            .ok_or_else(|| format!("no window {}", window.0))?;
        let window = self
            .app
            .get_window(&label)
            .ok_or_else(|| format!("window {label} is gone"))?;
        window.show().map_err(|e| e.to_string())?;
        if focus {
            window.set_focus().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn close_window(&self, window: WindowId) -> Result<(), String> {
        let label = self
            .views
            .label_of_window(window)
            .ok_or_else(|| format!("no window {}", window.0))?;
        let window = self
            .app
            .get_window(&label)
            .ok_or_else(|| format!("window {label} is gone"))?;
        window.close().map_err(|e| e.to_string())
    }
}

/// The shell window, as the `MainWindow` host answers
struct ShellMain(Arc<ViewTable>);

impl MainWindow for ShellMain {
    fn view(&self) -> Option<ViewId> {
        self.0.main().map(|s| s.view)
    }

    fn window(&self) -> Option<WindowId> {
        self.0.main().map(|s| s.window)
    }
}

/// The windows of the process, for the seams that start where no `HostContext`
/// is at hand (`shell_tabs`: the crash prompt reloads a view)
static WINDOWS: std::sync::OnceLock<Arc<Windows>> = std::sync::OnceLock::new();

pub(crate) fn windows() -> Option<Arc<Windows>> {
    WINDOWS.get().cloned()
}

pub fn install(hosts: &mut Hosts, cx: &HostContext) {
    let _ = WINDOWS.set(cx.windows.clone());
    install_main_window(hosts, &cx.views);
}

pub fn install_main_window(hosts: &mut Hosts, views: &Arc<ViewTable>) {
    let main: MainWindowHost = Arc::new(ShellMain(views.clone()));
    hosts.insert(main);
}
