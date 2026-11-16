//! Extension traits over Tauri's builders and webviews: how the shell asks for a native
//! browser and reaches the operations behind it. Replaces the CEF runtime's `*CefExt` traits.
//!
//! Building a browser:
//!
//! ```ignore
//! let spec = NativeSpec::new().bridge(BridgeMode::Invoke).native_events(sink);
//! let (builder, browser) = WebviewWindowBuilder::new(app, label, url).native(&native, label, spec)?;
//! let window = builder.build()?;
//! native.attach(window.as_ref(), &browser)?;   // UI thread, after build() returned Ok
//! ```
//!
//! A builder that went through `.native(...)` owns its `on_navigation` and `on_page_load`
//! hooks (both are single-slot in Tauri). Navigation allow lists are a [`RequestPolicy`].

use std::sync::Arc;

use serde_json::Value;
use tauri::webview::WebviewBuilder;
use tauri::window::WindowBuilder;
use tauri::{Manager, Url, Webview, WebviewWindow, WebviewWindowBuilder, Wry};

use super::Native;
use super::browser::{BridgeMode, BrowserIdentity, BrowserState, Config, WebviewControl};
use super::document::DocumentContext;
use super::events::EventSink;
use super::lifecycle::{BrowserClose, BrowserCreation, WindowCreation};
use super::navigation::{NavigationStart, ReloadAdmission, ReloadReservation};
use super::page::PageOps;
use super::platform::{self, Current, PageDriver, tauri_public};
use super::policy::{RequestPolicy, Storage, Store, scratch_root, store_for};

/// What one native browser is built with.
#[derive(Default)]
pub struct NativeSpec {
    sink: Option<EventSink>,
    creation: Option<BrowserCreation>,
    bridge: BridgeMode,
    start: NavigationStart,
    policy: Option<RequestPolicy>,
}

impl NativeSpec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Receive this browser's events on the UI thread, in order.
    pub fn native_events(mut self, sink: EventSink) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Use the caller's creation reservation (its receipt reports `NotCreated` if the build
    /// fails); without one the browser reserves its own.
    pub fn native_creation(mut self, creation: BrowserCreation) -> Self {
        self.creation = Some(creation);
        self
    }

    pub fn bridge(mut self, mode: BridgeMode) -> Self {
        self.bridge = mode;
        self
    }

    pub fn navigation_start(mut self, start: NavigationStart) -> Self {
        self.start = start;
        self
    }

    pub fn request_policy(mut self, policy: RequestPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    pub(crate) fn into_parts(self) -> (Config, Option<BrowserCreation>) {
        let config = Config {
            sink: self.sink,
            start: self.start,
            bridge: self.bridge,
            policy: self.policy,
        };
        (config, self.creation)
    }
}

/// Install the native hooks on a builder (`on_navigation`, `on_page_load`) and the store the
/// policy asks for (a scratch run gets stores of its own). The navigation hook also keeps the
/// creation reservation alive: the last drop of an unclaimed one proves the build failed.
macro_rules! install_hooks {
    ($builder:expr, $identity:expr, $submission:expr) => {{
        let navigating = $identity.clone();
        let loading = $identity.clone();
        let reservation = $submission;
        let builder = $builder
            .on_navigation(move |url| {
                let _reservation = &reservation;
                tauri_public::on_navigation(&navigating, url)
            })
            .on_page_load(move |_, payload| {
                tauri_public::on_page_load(&loading, payload.event(), payload.url());
            });
        let storage = $identity
            .policy()
            .map_or(Storage::Default, RequestPolicy::storage);
        match store_for(storage, scratch_root().as_deref()) {
            Some(Store::Identifier(id)) => builder.data_store_identifier(id),
            Some(Store::Directory(directory)) => builder.data_directory(directory),
            None => builder,
        }
    }};
}

/// `WebviewBuilder` (a child view inside a window).
pub trait WebviewBuilderExt: Sized {
    fn native(
        self,
        native: &Native,
        label: &str,
        spec: NativeSpec,
    ) -> Result<(Self, BrowserIdentity), String>;
}

impl WebviewBuilderExt for WebviewBuilder<Wry> {
    fn native(
        self,
        native: &Native,
        label: &str,
        spec: NativeSpec,
    ) -> Result<(Self, BrowserIdentity), String> {
        let (identity, submission) = native.register(label, spec)?;
        let builder = install_hooks!(self, identity, submission);
        Ok((builder, identity))
    }
}

/// `WebviewWindowBuilder` (a window with one view).
pub trait WebviewWindowBuilderExt: Sized {
    fn native(
        self,
        native: &Native,
        label: &str,
        spec: NativeSpec,
    ) -> Result<(Self, BrowserIdentity), String>;
}

impl<M: Manager<Wry>> WebviewWindowBuilderExt for WebviewWindowBuilder<'_, Wry, M> {
    fn native(
        self,
        native: &Native,
        label: &str,
        spec: NativeSpec,
    ) -> Result<(Self, BrowserIdentity), String> {
        let (identity, submission) = native.register(label, spec)?;
        let builder = install_hooks!(self, identity, submission);
        Ok((builder, identity))
    }
}

/// `WindowBuilder` (a bare window): reserve the window receipt. Report the build result with
/// `Native::window_created` or `Native::window_failed`.
pub trait WindowBuilderExt: Sized {
    fn native_window_creation(
        self,
        native: &Native,
        label: &str,
        creation: WindowCreation,
    ) -> Result<Self, String>;
}

impl<M: Manager<Wry>> WindowBuilderExt for WindowBuilder<'_, Wry, M> {
    fn native_window_creation(
        self,
        native: &Native,
        label: &str,
        creation: WindowCreation,
    ) -> Result<Self, String> {
        native.reserve_window(label, creation)?;
        Ok(self)
    }
}

/// The webview behind the Tauri `Webview` as the native layer drives it.
pub(crate) struct TauriControl {
    webview: Webview,
}

impl WebviewControl for TauriControl {
    fn navigate(&self, url: &Url) -> Result<(), String> {
        self.webview
            .navigate(url.clone())
            .map_err(|error| error.to_string())
    }

    fn reload(&self) -> Result<(), String> {
        self.webview.reload().map_err(|error| error.to_string())
    }

    /// `Webview::close` removes the label from Tauri's registry before it returns; that is
    /// the `Retired` receipt's condition.
    fn close(&self) -> Result<(), String> {
        self.webview.close().map_err(|error| error.to_string())?;
        if self
            .webview
            .app_handle()
            .get_webview(self.webview.label())
            .is_some()
        {
            return Err("The webview is still registered after close.".into());
        }
        Ok(())
    }

    fn evaluate(
        &self,
        script: String,
        done: Box<dyn FnOnce(Result<Value, String>) + Send>,
    ) -> Result<(), String> {
        Current::evaluate(
            &self.webview,
            script,
            true,
            Box::new(move |result| done(result.map_err(|error| error.to_string()))),
        );
        Ok(())
    }

    fn run(&self, script: String) -> Result<(), String> {
        self.webview.eval(script).map_err(|error| error.to_string())
    }
}

/// Claim the creation reservation, bind the close request and attach the OS observers on the
/// UI thread. See [`Native::attach`].
pub(crate) fn attach(webview: &Webview, browser: &BrowserIdentity) -> Result<(), String> {
    let core = browser.core.clone();
    core.claim_creation()?;
    let control: Arc<dyn WebviewControl> = Arc::new(TauriControl {
        webview: webview.clone(),
    });
    let closer = control.clone();
    let retiring = core.clone();
    core.close_receipt().bind_request(Arc::new(move || {
        closer.close()?;
        retiring.retire();
        Ok(())
    }))?;
    let window_label = webview.window().label().to_owned();
    let identity = browser.clone();
    webview
        .with_webview(move |platform_webview| {
            match platform::attach(&platform_webview, &identity) {
                Ok(()) => identity.core.mark_attached(control, &window_label),
                Err(error) => {
                    eprintln!("shell: native observers could not be attached: {error}");
                    identity.core.attach_failed(&error);
                }
            }
        })
        .map_err(|error| {
            let error = error.to_string();
            core.attach_failed(&error);
            error
        })
}

fn run_on_ui(webview: &Webview, job: impl FnOnce() + Send + 'static) -> Result<(), String> {
    webview
        .app_handle()
        .run_on_main_thread(job)
        .map_err(|error| error.to_string())
}

/// A webview together with its native browser, for work that must run on the UI thread.
pub struct NativeWebview {
    pub identity: BrowserState,
}

/// Native operations on an existing webview.
pub trait WebviewExt {
    fn as_webview(&self) -> &Webview;

    /// The open native browser behind this webview.
    fn native_browser(&self) -> Result<BrowserIdentity, String> {
        let webview = self.as_webview();
        let native = webview
            .try_state::<Native>()
            .ok_or("The native module is not installed.")?;
        native.browser(webview.label()).ok_or_else(|| {
            format!(
                "No open native browser for the webview {}.",
                webview.label()
            )
        })
    }

    fn native_browser_close(&self) -> Result<BrowserClose, String> {
        Ok(self.native_browser()?.close_receipt())
    }

    /// Typed page operations (replaces the CDP surface).
    fn page(&self) -> Result<PageOps, String> {
        self.native_browser()?;
        Ok(PageOps::new(self.as_webview().clone()))
    }

    /// Run `job` on the UI thread with the webview and its browser. Replaces
    /// `with_cef_webview`.
    fn with_native<F: FnOnce(&NativeWebview) + Send + 'static>(
        &self,
        job: F,
    ) -> Result<(), String> {
        self.with_native_async(move |native| {
            if let Ok(native) = native {
                job(native);
            }
        })
    }

    /// Like `with_native`, but a browser that retired first is reported to the closure.
    fn with_native_async<F: FnOnce(Result<&NativeWebview, String>) + Send + 'static>(
        &self,
        job: F,
    ) -> Result<(), String> {
        let identity = self.native_browser();
        run_on_ui(self.as_webview(), move || match identity {
            Ok(identity) => job(Ok(&NativeWebview { identity })),
            Err(error) => job(Err(error)),
        })
    }

    /// Dispatch an approved reload on the UI thread; `done` completes once with the
    /// registered successor or the reason the transaction failed.
    fn reload_reserved(
        &self,
        reservation: &ReloadReservation,
        admission: ReloadAdmission,
        done: impl FnOnce(Result<DocumentContext, String>) + Send + 'static,
    ) -> Result<(), String> {
        let browser = self.native_browser()?;
        let reservation = reservation.clone();
        run_on_ui(self.as_webview(), move || {
            reservation.dispatch(&browser, admission, Box::new(done));
        })
    }

    /// Show or hide the view; `done` reports `Err` when the browser retired first.
    fn set_visible_checked(
        &self,
        visible: bool,
        done: impl FnOnce(Result<(), String>) + Send + 'static,
    ) -> Result<(), String> {
        let webview = self.as_webview().clone();
        let browser = self.native_browser()?;
        run_on_ui(self.as_webview(), move || {
            let changed = if visible {
                webview.show()
            } else {
                webview.hide()
            };
            let result = changed.map_err(|error| error.to_string()).and_then(|()| {
                browser
                    .with_live_browser(|| ())
                    .ok_or_else(|| "The browser retired before the visibility change.".to_owned())
            });
            done(result);
        })
    }
}

impl WebviewExt for Webview {
    fn as_webview(&self) -> &Webview {
        self
    }
}

impl WebviewExt for WebviewWindow {
    fn as_webview(&self) -> &Webview {
        AsRef::<Webview>::as_ref(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_carries_its_choices_into_the_browser_config() {
        let (config, creation) = NativeSpec::new()
            .bridge(BridgeMode::Preload)
            .navigation_start(NavigationStart::Explicit)
            .request_policy(RequestPolicy::allow_all().with_javascript(false))
            .into_parts();
        assert_eq!(config.bridge, BridgeMode::Preload);
        assert_eq!(config.start, NavigationStart::Explicit);
        assert!(config.policy.is_some_and(|policy| !policy.javascript()));
        assert!(config.sink.is_none() && creation.is_none());
        let (config, _) = NativeSpec::new().into_parts();
        assert_eq!(
            config.bridge,
            BridgeMode::None,
            "no bridge unless asked for"
        );
    }
}
