//! What the probes share: a context, the way a probe opens a window with the native module's
//! own builder extensions, and the blocking calls it makes on a window's page operations.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use super::log::{self, Timeline, record};
use crate::app_scheme;
use crate::host::HostContext;
use crate::host::window::Windows;
use crate::native::{
    BridgeMode, BrowserIdentity, EventSink, Native, NativeEvent, NativeEventKind, NativeSpec,
    PageError, PageOps, RequestPolicy, WebviewExt, WebviewWindowBuilderExt,
};

pub struct Ctx {
    pub app: AppHandle,
    pub windows: Arc<Windows>,
}

impl Ctx {
    pub fn new(context: &HostContext) -> Self {
        Self { app: context.app.clone(), windows: context.windows.clone() }
    }

    pub fn native(&self) -> Native {
        self.app.state::<Native>().inner().clone()
    }

    /// A file next to the record.
    pub fn artifact(&self, name: &str) -> PathBuf {
        log::out_dir().map_or_else(|| PathBuf::from(name), |dir| dir.join(name))
    }
}

pub fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "other"
    }
}

/// A page of the fixture folder as the shell's app scheme serves it, in the webview's own URL
/// shape (`genoffice-app-<host>://localhost/...` on macOS, `http://genoffice-app-<host>.localhost/...`
/// on Windows).
pub fn app_url(host: &str, page: &str) -> String {
    app_scheme::webview_url(&format!("genoffice-app://{host}/probe/{page}"))
        .map_or_else(|| format!("genoffice-app://{host}/probe/{page}"), |url| url.to_string())
}

/// The same for a scheme the probe serves itself.
pub fn scheme_url(scheme: &str, path: &str) -> String {
    if cfg!(windows) {
        format!("http://{scheme}.localhost/{path}")
    } else {
        format!("{scheme}://localhost/{path}")
    }
}

/// How a probe window is built.
#[derive(Clone)]
pub struct Direct {
    pub label: String,
    pub url: String,
    pub visible: bool,
    pub focused: bool,
    pub size: (f64, f64),
    pub position: Option<(f64, f64)>,
    pub bridge: BridgeMode,
    pub policy: Option<RequestPolicy>,
    /// Install an `on_new_window` hook that records the request and denies it.
    pub new_window: bool,
    /// Called with every event of the browser, on the UI thread, after it was recorded.
    pub hook: Option<Arc<dyn Fn(&NativeEvent) + Send + Sync>>,
}

impl Direct {
    pub fn new(label: &str, url: &str) -> Self {
        Self {
            label: label.to_owned(),
            url: url.to_owned(),
            visible: true,
            focused: false,
            size: (480.0, 360.0),
            position: None,
            bridge: BridgeMode::None,
            policy: None,
            new_window: false,
            hook: None,
        }
    }
}

pub struct Opened {
    pub label: String,
    /// Every native event of the browser, in delivery order.
    pub timeline: Arc<Timeline>,
    pub browser: BrowserIdentity,
}

/// The event as plain data: its kind and what it carries.
pub fn describe(event: &NativeEvent) -> (&'static str, Value) {
    match &event.kind {
        NativeEventKind::NavigationStarted { navigation, url, same_document } => (
            "NavigationStarted",
            json!({ "navigation": navigation.id().get(), "url": url.as_str(), "same_document": same_document }),
        ),
        NativeEventKind::DocumentCommitted { navigation, document, url } => (
            "DocumentCommitted",
            json!({ "navigation": navigation.id().get(), "document": document.id().get(), "url": url.as_str() }),
        ),
        NativeEventKind::DocumentRegistered { context } => (
            "DocumentRegistered",
            json!({ "document": context.identity().id().get(), "url": context.url().as_str() }),
        ),
        NativeEventKind::DocumentReady { context } => (
            "DocumentReady",
            json!({ "document": context.identity().id().get(), "url": context.url().as_str() }),
        ),
        NativeEventKind::DocumentRevoked { document, reason } => (
            "DocumentRevoked",
            json!({ "document": document.id().get(), "reason": format!("{reason:?}") }),
        ),
        NativeEventKind::NavigationFailed { navigation, url, error } => (
            "NavigationFailed",
            json!({
                "navigation": navigation.id().get(),
                "url": url.as_str(),
                "code": error.code,
                "description": error.description,
            }),
        ),
        NativeEventKind::LoadFinished { navigation, ok } => {
            ("LoadFinished", json!({ "navigation": navigation.id().get(), "ok": ok }))
        }
        NativeEventKind::AddressChanged { url } => ("AddressChanged", json!({ "url": url.as_str() })),
        NativeEventKind::WebviewProcessFailed { kind } => {
            ("WebviewProcessFailed", json!({ "kind": format!("{kind:?}") }))
        }
        NativeEventKind::View(view) => ("View", json!({ "event": format!("{view:?}") })),
        NativeEventKind::Retired { outcome } => {
            ("Retired", json!({ "outcome": format!("{outcome:?}") }))
        }
    }
}

/// Record one native event and keep it on the browser's timeline.
pub fn note_event(label: &str, timeline: &Timeline, event: &NativeEvent) {
    let (kind, mut value) = describe(event);
    if let Value::Object(map) = &mut value {
        map.insert("view".into(), json!(label));
        map.insert("browser".into(), json!(event.browser.id().get()));
    }
    record("native", &format!("native.{kind}"), value.clone());
    timeline.push(kind, value);
}

/// Build a window the way the shell does for its own views: through the native module's
/// builder extension, then `attach` on the UI thread.
pub fn open_direct(ctx: &Ctx, spec: Direct) -> Result<Opened, String> {
    let timeline = Arc::new(Timeline::default());
    let label = spec.label.clone();
    let (send, receive) = mpsc::sync_channel(1);
    let app = ctx.app.clone();
    let build_timeline = timeline.clone();
    record("rust", "win.open", json!({ "label": label, "url": spec.url, "visible": spec.visible, "bridge": format!("{:?}", spec.bridge) }));
    ctx.app
        .run_on_main_thread(move || {
            let _ = send.send(build(&app, spec, build_timeline));
        })
        .map_err(|error| error.to_string())?;
    let browser = receive
        .recv_timeout(Duration::from_secs(60))
        .map_err(|_| "building the window timed out".to_owned())
        .and_then(|result| result);
    match browser {
        Ok(browser) => Ok(Opened { label, timeline, browser }),
        Err(error) => {
            record("rust", "win.open.failed", json!({ "label": label, "error": error }));
            Err(error)
        }
    }
}

fn build(app: &AppHandle, spec: Direct, timeline: Arc<Timeline>) -> Result<BrowserIdentity, String> {
    let native = app.state::<Native>().inner().clone();
    let url = tauri::Url::parse(&spec.url).map_err(|error| error.to_string())?;
    let label = spec.label.clone();
    let sink_label = label.clone();
    let sink_timeline = timeline.clone();
    let hook = spec.hook.clone();
    let sink: EventSink = Arc::new(move |event: &NativeEvent| {
        note_event(&sink_label, &sink_timeline, event);
        if let Some(hook) = &hook {
            hook(event);
        }
    });
    let mut builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(url))
        .title(&label)
        .inner_size(spec.size.0, spec.size.1)
        .visible(spec.visible)
        .focused(spec.focused);
    if let Some((x, y)) = spec.position {
        builder = builder.position(x, y);
    }
    if spec.new_window {
        let asked = native.clone();
        let asked_label = label.clone();
        builder = builder.on_new_window(move |url, _features| {
            record(
                "rust",
                "win.new_window",
                json!({
                    "view": asked_label,
                    "url": url.as_str(),
                    "ui_thread": crate::native::defer::is_ui_thread(),
                    "registered_document": asked
                        .current_context(&asked_label)
                        .map(|context| context.identity().id().get()),
                }),
            );
            tauri::webview::NewWindowResponse::Deny
        });
    }
    let mut native_spec = NativeSpec::new().bridge(spec.bridge).native_events(sink);
    if let Some(policy) = spec.policy {
        native_spec = native_spec.request_policy(policy);
    }
    let (builder, browser) = builder.native(&native, &label, native_spec)?;
    let window = builder.build().map_err(|error| error.to_string())?;
    native.attach(AsRef::<tauri::Webview>::as_ref(&window), &browser)?;
    Ok(browser)
}

/// Destroy a window by label, on the UI thread.
pub fn destroy(ctx: &Ctx, label: &str) {
    let app = ctx.app.clone();
    let name = label.to_owned();
    record("rust", "win.destroy", json!({ "label": label }));
    let _ = ctx.app.run_on_main_thread(move || {
        if let Some(window) = app.get_webview_window(&name) {
            let _ = window.destroy();
        }
    });
}

/// The page operations of the webview behind `label`.
pub fn page_of(ctx: &Ctx, label: &str) -> Result<PageOps, String> {
    ctx.app
        .get_webview(label)
        .ok_or_else(|| format!("no webview {label}"))?
        .page()
}

/// Wait for the fixture page in `label` to report `page.done`.
pub fn wait_done(label: &str, from: usize, seconds: u64) -> bool {
    log::wait_page(label, "page.done", from, Duration::from_secs(seconds)).is_some()
}

/// Wait until the browser reports the end of a load after the `from`th event.
pub fn wait_load(opened: &Opened, from: usize, seconds: u64) -> bool {
    opened.timeline.wait_kind("LoadFinished", from, Duration::from_secs(seconds))
}

pub fn eval(page: &PageOps, js: &str, seconds: u64) -> Result<Value, PageError> {
    page.blocking(Duration::from_secs(seconds), |done| page.evaluate(js, true, None, done))
}

/// Record the outcome of one operation.
pub fn record_op(name: &str, label: &str, started: f64, result: Result<Value, String>) {
    let ms = log::now_ms() - started;
    match result {
        Ok(value) => record("rust", "op", json!({ "name": name, "view": label, "ok": true, "ms": ms, "value": value })),
        Err(error) => record("rust", "op", json!({ "name": name, "view": label, "ok": false, "ms": ms, "error": error })),
    }
}

/// Keeps values alive until the process ends (evaluation handles that must outlive their call).
pub fn keep<T: Send + 'static>(store: &Mutex<Vec<T>>, value: T) {
    store.lock().unwrap_or_else(|error| error.into_inner()).push(value);
}
