//! The native surface between the shell and Tauri 2's public APIs on the OS webviews
//! (WKWebView on macOS, WebView2 on Windows).
//!
//! Three Rust-minted ids ([`BrowserId`], [`NavigationId`], [`DocumentId`]) and one registry
//! ([`Native`]) bind a webview to the shell. A document proves itself with a per-document
//! secret nonce its bridge registers once; Rust mints the document identity at navigation
//! commit and revokes it at navigation admission, before the next cross-document navigation
//! starts. WK and WebView2 objects are never identity.
//!
//! The pure core (`ids`, `browser`, `document`, `navigation`, `events`, `lifecycle`, `eval`,
//! `scheme`, `page`, `policy`, `ingress`, `defer`, `url`) has no `cfg` and no OS types. Only
//! `platform/*`, `ext` and `window` touch Tauri webview and OS APIs; `menu` and `locale`
//! (S7) sit on top of them.
//!
//! Do not call `on_navigation` or `on_page_load` on a builder that went through
//! `.native(...)`: both are single-slot in Tauri, and the native hooks own them. The shell's
//! navigation allow lists are a [`RequestPolicy`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, WindowEvent};

pub mod browser;
pub mod defer;
pub mod document;
pub mod eval;
pub mod events;
pub mod ext;
pub mod ids;
pub mod ingress;
pub mod lifecycle;
pub mod locale;
pub mod menu;
pub mod navigation;
pub mod page;
pub mod platform;
pub mod policy;
pub mod scheme;
pub mod url;
pub mod window;

pub use browser::{BridgeMode, BrowserIdentity, BrowserState, RegisterError, WebviewControl};
pub use document::{
    DocumentContext, DocumentGatePlan, DocumentIdentity, DocumentReady, DocumentRevocation,
};
pub use eval::DocumentEvaluation;
pub use events::{EventSink, NativeEvent, NativeEventKind, ViewEvent};
pub use ext::{
    NativeSpec, WebviewBuilderExt, WebviewExt, WebviewWindowBuilderExt,
    WindowBuilderExt,
};
pub use ids::{BrowserId, CreationId};
pub use ingress::{ChannelQueue, ReplyFrame};
pub use lifecycle::{
    BrowserClose, BrowserCloseOutcome, BrowserCloseSubscription, BrowserCreation,
    BrowserCreationOutcome, WindowClose, WindowCloseOutcome,
    WindowCloseSubscription, WindowClosingSubscription, WindowCreation, WindowEffect,
};
pub use navigation::{
    NavigationIdentity, NavigationStart, ObservedDocument, ObservedDocumentSubscription,
    ObservedNavigation, ReloadIdentity, ReloadReservation,
};
pub use page::{
    CaptureSpec, Done, EditCommand, Fence, HeaderFooter, KeyChord, Modifiers, PageError, PageOps,
    PdfSpec, QuietSpec, Rect, Viewport,
};
pub use policy::{Blocks, PolicyRequest, RequestPolicy, Storage};
pub use scheme::{RequestCancelSubscription, ResponseGate, SchemeRequestContext};
pub use window::{WindowExt, WindowHandle};

use lifecycle::WindowSubmission;

struct BrowserEntry {
    identity: BrowserIdentity,
    /// Retained: dropping it would silence the registry's own cleanup.
    _closed: BrowserCloseSubscription,
}

struct WindowEntry {
    close: WindowClose,
    creation: CreationId,
    /// Holds the reservation: dropping it unclaimed would report `NotCreated`.
    _submission: WindowSubmission,
    _closed: WindowCloseSubscription,
}

#[derive(Default)]
struct Registry {
    browsers: Mutex<HashMap<Box<str>, BrowserEntry>>,
    windows: Mutex<HashMap<Box<str>, WindowEntry>>,
}

/// Managed state: the registry of native browsers and windows, keyed by label for routing
/// only. Cloning shares it.
#[derive(Clone, Default)]
pub struct Native(Arc<Registry>);

/// Register the managed [`Native`] state. The window and page hooks are single-slot in
/// Tauri's `Builder`, so `main.rs` keeps them and forwards to
/// [`Native::on_window_event`]; per-webview hooks are installed by `.native(...)`.
pub fn install(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    builder.manage(Native::default())
}

/// Authenticate a request by its document nonce header. Replaces `redeem_native_context`.
pub fn redeem(
    webview: &tauri::Webview,
    headers: &tauri::http::HeaderMap,
) -> Option<DocumentContext> {
    use tauri::Manager;
    let native = webview.try_state::<Native>()?;
    ingress::redeem(&native, webview.label(), headers)
}

impl Native {
    /// Connect the deferred-completion queue to the UI thread. Call once from `setup`, on the
    /// UI thread.
    pub fn bind_ui(app: &AppHandle) -> bool {
        let poster_app = app.clone();
        let ui_thread = std::thread::current().id();
        defer::install_ui(defer::UiPoster {
            post: Box::new(move |job| {
                if let Err(error) = poster_app.run_on_main_thread(job) {
                    eprintln!(
                        "shell: a native completion could not be posted to the UI thread: {error}"
                    );
                }
            }),
            is_ui_thread: Box::new(move || std::thread::current().id() == ui_thread),
        })
    }

    /// The open browser behind a webview label.
    pub fn browser(&self, label: &str) -> Option<BrowserIdentity> {
        let browsers = self
            .0
            .browsers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        browsers
            .get(label)
            .map(|entry| entry.identity.clone())
            .filter(BrowserIdentity::is_open)
    }

    /// The registered, live document of the browser behind `label`.
    pub fn current_context(&self, label: &str) -> Option<DocumentContext> {
        self.browser(label)?.registered_context()
    }

    /// Reserve a browser for `label`. The returned submission must live as long as the
    /// builder hooks do: the last drop of an unclaimed reservation reports `NotCreated`.
    pub(crate) fn register(
        &self,
        label: &str,
        spec: NativeSpec,
    ) -> Result<(BrowserIdentity, lifecycle::BrowserSubmission), String> {
        let (config, creation) = spec.into_parts();
        let creation = match creation {
            Some(creation) => creation,
            None => BrowserCreation::new().ok_or("Native creation ids are exhausted.")?,
        };
        let receipt = creation.receipt();
        let submission = creation.into_submission();
        let core = browser::BrowserCore::new(label, submission.creation(), receipt, config)
            .ok_or("Native browser ids are exhausted.")?;
        let identity = core.identity();
        let native = self.clone();
        let key: Box<str> = label.into();
        let id = identity.id();
        let closed =
            identity.when_closed(move |outcome| native.forget_browser(&key, id, outcome))?;
        let mut browsers = self
            .0
            .browsers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if browsers
            .get(label)
            .is_some_and(|entry| entry.identity.is_open())
        {
            return Err(format!("A browser with the label {label} already exists."));
        }
        browsers.insert(
            label.into(),
            BrowserEntry {
                identity: identity.clone(),
                _closed: closed,
            },
        );
        Ok((identity, submission))
    }

    fn forget_browser(&self, label: &str, id: BrowserId, outcome: BrowserCloseOutcome) {
        let entry = {
            let mut browsers = self
                .0
                .browsers
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if browsers
                .get(label)
                .is_some_and(|entry| entry.identity.id() == id)
            {
                browsers.remove(label)
            } else {
                None
            }
        };
        if let Some(entry) = entry
            && outcome == BrowserCloseOutcome::NotCreated
        {
            entry
                .identity
                .core
                .retire_with(BrowserCloseOutcome::NotCreated);
        }
    }

    /// Reserve a window for `label`. Report the build result with [`Native::window_created`]
    /// or [`Native::window_failed`].
    pub fn reserve_window(
        &self,
        label: &str,
        creation: WindowCreation,
    ) -> Result<WindowClose, String> {
        let receipt = creation.receipt();
        let creation_id = creation.creation_id();
        let submission = creation.submit();
        let native = self.clone();
        let key: Box<str> = label.into();
        let closed = receipt.when_closed(move |_| native.forget_window(&key))?;
        let mut windows = self
            .0
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if windows
            .get(label)
            .is_some_and(|entry| entry.close.is_open())
        {
            return Err(format!("A window with the label {label} already exists."));
        }
        windows.insert(
            label.into(),
            WindowEntry {
                close: receipt.clone(),
                creation: creation_id,
                _submission: submission,
                _closed: closed,
            },
        );
        Ok(receipt)
    }

    fn forget_window(&self, label: &str) {
        self.0
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(label);
    }

    /// The window build returned `Ok`: the reservation now has a native window behind it.
    pub fn window_created(&self, window: &tauri::Window) -> Result<(), String> {
        let Some(close) = self.window_close(window.label()) else {
            return Ok(());
        };
        if close.claim() && close.allocated() {
            Ok(())
        } else {
            Err("The native window reservation was already used or is closing.".into())
        }
    }

    /// The window build failed: the reservation proves "not created".
    pub fn window_failed(&self, label: &str) {
        let entry = self
            .0
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(label);
        if let Some(entry) = entry {
            entry.close.not_created();
        }
    }

    pub fn window_close(&self, label: &str) -> Option<WindowClose> {
        let windows = self
            .0
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        windows.get(label).map(|entry| entry.close.clone())
    }

    /// Non-owning access to `window`; a window without a reservation gets a handle that is
    /// never retired.
    pub fn window_handle(&self, window: &tauri::Window) -> WindowHandle {
        let windows = self
            .0
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match windows.get(window.label()) {
            Some(entry) => WindowHandle::new(
                window.clone(),
                Some(entry.close.clone()),
                Some(entry.creation),
            ),
            None => WindowHandle::new(window.clone(), None, None),
        }
    }

    /// Forward from the application's `on_window_event`. `Focused` reaches the browser whose
    /// webview label is the window's (a single-view window); `Destroyed` retires every
    /// browser of the window, then completes the window receipt.
    pub fn on_window_event(&self, window: &tauri::Window, event: &WindowEvent) {
        match event {
            WindowEvent::Focused(focused) => {
                if let Some(browser) = self.browser(window.label()) {
                    browser.observe_view(if *focused {
                        ViewEvent::Focus
                    } else {
                        ViewEvent::Blur
                    });
                }
            }
            WindowEvent::Destroyed => {
                let label = window.label();
                let doomed: Vec<BrowserIdentity> = {
                    let browsers = self
                        .0
                        .browsers
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    browsers
                        .values()
                        .filter(|entry| entry.identity.core.window_label() == Some(label))
                        .map(|entry| entry.identity.clone())
                        .collect()
                };
                for browser in doomed {
                    browser.observe_retired();
                }
                if let Some(close) = self.window_close(label) {
                    close.destroyed();
                }
            }
            _ => {}
        }
    }

    /// Attach a built webview to its browser. Call once on the UI thread after `build()`
    /// returned `Ok`: it claims the creation reservation, binds the close request and, on the
    /// UI thread, attaches the OS observers; readiness (`Navigator::when_ready`) follows.
    pub fn attach(
        &self,
        webview: &tauri::Webview,
        browser: &BrowserIdentity,
    ) -> Result<(), String> {
        ext::attach(webview, browser)
    }
}
