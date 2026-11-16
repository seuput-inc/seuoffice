use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use shell_contract::runtime::CancelToken;
use tauri::{AppHandle, Manager, Webview, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::native::{
    BrowserClose, BrowserCloseSubscription, BrowserCreation, BrowserState, CaptureSpec, Done,
    Native, NativeEvent, NativeEventKind, NativeSpec, NavigationStart, ObservedDocument,
    ObservedDocumentSubscription, PageError, QuietSpec, Rect, RequestPolicy, Viewport, WebviewExt,
    WebviewWindowBuilderExt, WindowClose, WindowCloseSubscription, WindowCreation,
};

const CANCELLED: &str = "HTML conversion was cancelled";
const PAGE_CALL_TIMEOUT: Duration = Duration::from_secs(90);
const NETWORK_QUIET: Duration = Duration::from_millis(500);

struct Navigation {
    document: Option<String>,
    started: bool,
}

impl Navigation {
    /// The blank page is admitted until the document navigation starts; the document URL is
    /// the only other admitted target. A navigation can be asked about more than once (the
    /// navigation hook and, on Windows, the request filter), so the document URL stays
    /// admitted once started; the browser revokes the document if it is ever loaded again.
    fn allow(&mut self, url: &str) -> bool {
        if url == "about:blank" {
            return !self.started;
        }
        if self.document.as_deref() != Some(url) {
            return false;
        }
        self.started = true;
        true
    }
}

type CloseWaiter = Box<dyn FnOnce(Result<(), String>) + Send>;

#[derive(Default)]
struct CloseState {
    browser_done: bool,
    window_done: bool,
    browser_subscription: Option<BrowserCloseSubscription>,
    window_subscription: Option<WindowCloseSubscription>,
    waiters: Vec<CloseWaiter>,
}

struct Page {
    id: u64,
    app: AppHandle,
    label: String,
    navigation: Arc<Mutex<Navigation>>,
    grant: Mutex<Option<crate::host::document_assets::TargetGuard>>,
    closing: AtomicBool,
    browser_close_requested: AtomicBool,
    window_close_requested: AtomicBool,
    retrying_close: AtomicBool,
    document: Mutex<Option<(ObservedDocument, ObservedDocumentSubscription)>>,
    browser_close: BrowserClose,
    window_close: WindowClose,
    window: Mutex<Option<WebviewWindow>>,
    close_state: Mutex<CloseState>,
}
static PAGES: LazyLock<Mutex<HashMap<u64, Arc<Page>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn close_error(page: &Arc<Page>, error: String) {
    let waiters = std::mem::take(
        &mut page
            .close_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .waiters,
    );
    for waiter in waiters {
        waiter(Err(error.clone()));
    }
    if page.retrying_close.swap(true, Ordering::AcqRel) {
        return;
    }
    let weak = Arc::downgrade(page);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let Some(page) = weak.upgrade() else { return };
            let done = {
                let state = page.close_state.lock().unwrap_or_else(|p| p.into_inner());
                state.browser_done && state.window_done
            };
            if done {
                page.retrying_close.store(false, Ordering::Release);
                return;
            }
            close(&page);
            let window_ready = page
                .window
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_some();
            if page.browser_close_requested.load(Ordering::Acquire)
                && (!window_ready || page.window_close_requested.load(Ordering::Acquire))
            {
                page.retrying_close.store(false, Ordering::Release);
                if page.browser_close_requested.load(Ordering::Acquire)
                    && (!window_ready || page.window_close_requested.load(Ordering::Acquire))
                {
                    return;
                }
                if page.retrying_close.swap(true, Ordering::AcqRel) {
                    return;
                }
            }
        }
    });
}

fn terminal(page: &Arc<Page>, browser: bool) {
    let waiters = {
        let mut state = page.close_state.lock().unwrap_or_else(|p| p.into_inner());
        if browser {
            state.browser_done = true;
        } else {
            state.window_done = true;
        }
        if !state.browser_done || !state.window_done {
            return;
        }
        std::mem::take(&mut state.waiters)
    };
    let mut pages = PAGES.lock().unwrap_or_else(|p| p.into_inner());
    if pages
        .get(&page.id)
        .is_some_and(|current| Arc::ptr_eq(current, page))
    {
        pages.remove(&page.id);
    }
    drop(pages);
    for waiter in waiters {
        waiter(Ok(()));
    }
}

fn close(page: &Arc<Page>) {
    page.closing.store(true, Ordering::Release);
    page.grant.lock().unwrap_or_else(|p| p.into_inner()).take();
    if !page.browser_close_requested.swap(true, Ordering::AcqRel)
        && let Err(error) = page.browser_close.request_close()
    {
        page.browser_close_requested.store(false, Ordering::Release);
        close_error(page, error);
    }
    let window = page
        .window
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    if let Some(window) = window {
        if page.window_close_requested.swap(true, Ordering::AcqRel) {
            return;
        }
        let captured = page.clone();
        if let Err(error) = page.app.run_on_main_thread(move || {
            if let Err(error) = window.destroy() {
                captured
                    .window_close_requested
                    .store(false, Ordering::Release);
                close_error(&captured, error.to_string());
            }
        }) {
            page.window_close_requested.store(false, Ordering::Release);
            close_error(page, error.to_string());
        }
    }
}

pub(super) fn close_request(id: u64) {
    let page = PAGES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&id)
        .cloned();
    if let Some(page) = page {
        close(&page);
    }
}

pub(super) fn after_close(id: u64, waiter: CloseWaiter) {
    let page = PAGES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&id)
        .cloned();
    let Some(page) = page else {
        waiter(Ok(()));
        return;
    };
    let mut waiter = Some(waiter);
    {
        let mut state = page.close_state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.browser_done || !state.window_done {
            state.waiters.push(waiter.take().unwrap());
        }
    }
    if let Some(waiter) = waiter {
        waiter(Ok(()));
    } else {
        close(&page);
    }
}

struct Opening {
    id: u64,
    cancel: CancelToken,
    complete: bool,
}

impl Drop for Opening {
    fn drop(&mut self) {
        if !self.complete {
            self.cancel.cancel();
            close_request(self.id);
        }
    }
}

fn webview(page: &Page) -> Result<Webview, String> {
    page.app
        .get_webview(&page.label)
        .ok_or_else(|| "HTML conversion page is closed".into())
}

/// Start one page operation and wait for its single completion, the cancellation or the
/// timeout, whichever comes first. An operation that finishes after that is dropped.
fn page_call<T: Send + 'static>(
    cancel: &CancelToken,
    timeout: Duration,
    call: impl FnOnce(Done<T>),
) -> Result<T, PageError> {
    if cancel.is_cancelled() {
        return Err(PageError::Cancelled);
    }
    let (sent, received) = mpsc::sync_channel(1);
    let wake = sent.clone();
    let subscription = cancel.on_cancel(move || {
        let _ = wake.try_send(Err(PageError::Cancelled));
    });
    call(Box::new(move |result| {
        let _ = sent.try_send(result);
    }));
    let result = received
        .recv_timeout(timeout)
        .unwrap_or(Err(PageError::Timeout));
    drop(subscription);
    if cancel.is_cancelled() {
        Err(PageError::Cancelled)
    } else {
        result
    }
}

fn describe(what: &str, error: PageError) -> String {
    match error {
        PageError::Cancelled => CANCELLED.into(),
        PageError::Timeout => format!("{what} timed out"),
        PageError::Stale => "HTML conversion page is closed".into(),
        other => format!("{what} failed: {other}"),
    }
}

/// A page operation of the opening or configuration phase: any failure ends the page.
fn setup<T: Send + 'static>(
    page: &Arc<Page>,
    cancel: &CancelToken,
    call: impl FnOnce(Done<T>),
) -> Result<T, String> {
    page_call(cancel, PAGE_CALL_TIMEOUT, call).map_err(|error| {
        close(page);
        describe("HTML browser setup", error)
    })
}

fn open(app: &AppHandle, id: u64, request: &Value, cancel: &CancelToken) -> Result<Value, String> {
    if cancel.is_cancelled() {
        return Err(CANCELLED.into());
    }
    use crate::host::document_assets::{AssetKind, TargetDocument};
    let (kind, document) = if let Some(encoded) = request.get("htmlBase64").and_then(Value::as_str)
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "invalid generated HTML bytes")?;
        (AssetKind::Docs, TargetDocument::Generated(bytes))
    } else {
        let path = request
            .get("path")
            .and_then(Value::as_str)
            .ok_or("HTML conversion needs a document")?;
        (
            AssetKind::Html,
            TargetDocument::Existing(std::path::PathBuf::from(path)),
        )
    };
    let owner = request
        .get("ownerViewId")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("HTML conversion needs an originating view")?;
    let label = format!("seuoffice-html-convert-{id}");
    let browser_creation = BrowserCreation::new().ok_or("Native creation ids are exhausted.")?;
    let window_creation = WindowCreation::new().ok_or("Native creation ids are exhausted.")?;
    let page = Arc::new(Page {
        id,
        app: app.clone(),
        label: label.clone(),
        navigation: Arc::new(Mutex::new(Navigation {
            document: None,
            started: false,
        })),
        grant: Mutex::new(None),
        closing: AtomicBool::new(false),
        browser_close_requested: AtomicBool::new(false),
        window_close_requested: AtomicBool::new(false),
        retrying_close: AtomicBool::new(false),
        document: Mutex::new(None),
        browser_close: browser_creation.receipt(),
        window_close: window_creation.receipt(),
        window: Mutex::new(None),
        close_state: Mutex::new(CloseState::default()),
    });
    {
        let mut pages = PAGES.lock().map_err(|_| "HTML page registry poisoned")?;
        if pages.contains_key(&id) {
            return Err("HTML conversion page already exists".into());
        }
        if cancel.is_cancelled() {
            return Err(CANCELLED.into());
        }
        pages.insert(id, page.clone());
    }
    let mut opening = Opening {
        id,
        cancel: cancel.clone(),
        complete: false,
    };
    let browser_page = Arc::downgrade(&page);
    let browser_subscription = page.browser_close.when_closed(move |_| {
        if let Some(page) = browser_page.upgrade() {
            terminal(&page, true);
        }
    })?;
    let window_page = Arc::downgrade(&page);
    let window_subscription = page.window_close.when_closed(move |_| {
        if let Some(page) = window_page.upgrade() {
            terminal(&page, false);
        }
    })?;
    {
        let mut state = page
            .close_state
            .lock()
            .map_err(|_| "HTML close state poisoned")?;
        state.browser_subscription = Some(browser_subscription);
        state.window_subscription = Some(window_subscription);
    }
    let (blank_tx, blank_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let blank_cancel = blank_tx.clone();
    let blank_subscription = cancel.on_cancel(move || {
        let _ = blank_cancel.try_send(Err(CANCELLED.into()));
    });
    let (tx, rx) = mpsc::sync_channel(1);
    let app_for_ui = app.clone();
    let page_for_ui = page.clone();
    let cancel_for_ui = cancel.clone();
    app.run_on_main_thread(move || {
        if cancel_for_ui.is_cancelled() {
            let _ = tx.send(Err(CANCELLED.to_owned()));
            return;
        }
        let result = build(
            &app_for_ui,
            &page_for_ui,
            browser_creation,
            window_creation,
            blank_tx,
            &cancel_for_ui,
        );
        let _ = tx.send(result);
    })
    .map_err(|e| e.to_string())?;
    let browser = rx
        .recv_timeout(Duration::from_secs(30))
        .map_err(|_| "HTML page creation did not finish".to_owned())??;
    blank_rx
        .recv_timeout(Duration::from_secs(30))
        .map_err(|_| "HTML browser did not initialize".to_owned())??;
    drop(blank_subscription);
    if cancel.is_cancelled() {
        close(&page);
        return Err(CANCELLED.into());
    }
    let grant = crate::host::document_assets::grant_target(
        &page.label,
        shell_contract::ViewId(owner),
        kind,
        browser,
        document,
    )?;
    grant.set_cancel_token(cancel.clone())?;
    let url = grant.document_url();
    if cancel.is_cancelled() {
        return Err(CANCELLED.into());
    }
    *page
        .navigation
        .lock()
        .map_err(|_| "HTML navigation policy poisoned")? = Navigation {
        document: Some(url.clone()),
        started: false,
    };
    *page.grant.lock().map_err(|_| "HTML asset grant poisoned")? = Some(grant);
    opening.complete = true;
    Ok(json!({"url": url}))
}

/// Create the hidden window and its native browser on the UI thread. The browser starts at
/// `about:blank`; `blank` completes once it is attached and committed, or when it failed.
fn build(
    app: &AppHandle,
    page: &Arc<Page>,
    browser_creation: BrowserCreation,
    window_creation: WindowCreation,
    blank: mpsc::SyncSender<Result<(), String>>,
    cancel: &CancelToken,
) -> Result<BrowserState, String> {
    let native = app
        .try_state::<Native>()
        .ok_or("The native module is not installed.")?;
    let label = page.label.as_str();
    native.reserve_window(label, window_creation)?;
    let navigation = page.navigation.clone();
    // The page is JavaScript-enabled (the converter measures the live layout), has one
    // admitted document navigation, and leaves sub-resources to the shell's scheme handlers.
    let policy = RequestPolicy::new(move |request| {
        if !request.main_frame {
            return true;
        }
        navigation
            .lock()
            .map(|mut navigation| navigation.allow(request.url))
            .unwrap_or(false)
    })
    .with_javascript(true);
    let failed = blank.clone();
    let owner = Arc::downgrade(page);
    let id = page.id;
    let spec = NativeSpec::new()
        .navigation_start(NavigationStart::Explicit)
        .native_creation(browser_creation)
        .request_policy(policy)
        .native_events(Arc::new(move |event: &NativeEvent| match &event.kind {
            NativeEventKind::NavigationFailed { url, .. } if url.as_str() == "about:blank" => {
                let _ = failed.try_send(Err("HTML browser failed to initialize".into()));
            }
            NativeEventKind::WebviewProcessFailed { .. } => {
                let _ = failed.try_send(Err("HTML browser process failed".into()));
                if let Some(page) = owner.upgrade()
                    && !page.closing.load(Ordering::Acquire)
                {
                    super::target_crashed(id);
                }
            }
            _ => {}
        }));
    let created = (|| {
        let blank_url = tauri::Url::parse("about:blank").map_err(|e| e.to_string())?;
        let (builder, browser) =
            WebviewWindowBuilder::new(app, label, WebviewUrl::External(blank_url))
                .native(&native, label, spec)?;
        let window = builder
            .title("HTML conversion")
            .visible(false)
            .focused(false)
            .skip_taskbar(true)
            .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
            .build()
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((window, browser))
    })();
    let (window, browser) = match created {
        Ok(created) => created,
        Err(error) => {
            native.window_failed(label);
            return Err(error);
        }
    };
    let view: &Webview = window.as_ref();
    let attached = native
        .window_created(&view.window())
        .and_then(|()| native.attach(view, &browser));
    if let Err(error) = attached {
        let _ = window.destroy();
        return Err(error);
    }
    let owner = page.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) && !owner.closing.load(Ordering::Acquire)
        {
            super::target_crashed(id);
        }
    });
    *page
        .window
        .lock()
        .map_err(|_| "HTML window state poisoned")? = Some(window);
    if page.closing.load(Ordering::Acquire) {
        close(page);
    }
    if cancel.is_cancelled() {
        close(page);
        return Err(CANCELLED.into());
    }
    browser.navigator().when_ready(move |result| {
        let _ = blank.try_send(result);
    });
    Ok(browser)
}

pub(super) fn dispatch(
    app: &AppHandle,
    id: u64,
    request: &Value,
    operation: &str,
    input: &Value,
    cancel: &CancelToken,
) -> Result<Value, String> {
    if operation == "html.open" {
        return open(app, id, request, cancel);
    }
    if operation == "html.close" {
        close_request(id);
        return Ok(Value::Null);
    }
    if cancel.is_cancelled() {
        return Err(CANCELLED.into());
    }
    let page = PAGES
        .lock()
        .map_err(|_| "HTML page registry poisoned")?
        .get(&id)
        .cloned()
        .ok_or("HTML conversion page is closed")?;
    let view = webview(&page)?;
    let ops = view.page()?;
    let result = match operation {
        "html.viewport" => {
            let edge = |name: &str| {
                input
                    .get(name)
                    .and_then(Value::as_u64)
                    .filter(|n| *n > 0 && *n <= 32768)
                    .and_then(|n| u32::try_from(n).ok())
            };
            let width = edge("width").ok_or("invalid viewport width")?;
            let height = edge("height").ok_or("invalid viewport height")?;
            let scale = input
                .get("deviceScaleFactor")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            if !scale.is_finite() || scale <= 0.0 {
                return Err("invalid device scale factor".into());
            }
            let viewport = Viewport {
                width,
                height,
                device_scale_factor: scale,
            };
            setup(&page, cancel, |done| ops.set_viewport(viewport, done))?;
            Value::Null
        }
        "html.goto" => {
            let url = input
                .get("url")
                .and_then(Value::as_str)
                .ok_or("navigation URL is missing")?;
            let belongs = page
                .navigation
                .lock()
                .map_err(|_| "HTML navigation policy poisoned")?
                .document
                .as_deref()
                == Some(url);
            if !belongs {
                return Err("navigation URL does not belong to this request".into());
            }
            let timeout = input
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .ok_or("navigation timeout is missing")?;
            let deadline = Instant::now()
                .checked_add(Duration::from_millis(timeout))
                .ok_or("navigation timeout is too large")?;
            let (sent, received) = mpsc::sync_channel(1);
            let wake = sent.clone();
            let cancelled = cancel.on_cancel(move || {
                let _ = wake.try_send(Err(CANCELLED.to_owned()));
            });
            let parsed = tauri::Url::parse(url).map_err(|error| error.to_string())?;
            let navigation =
                view.native_browser()?
                    .navigator()
                    .navigate_observed(parsed, move |result| {
                        let _ = sent.try_send(result);
                    })?;
            let observed = received
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| "HTML navigation timed out".to_owned())
                .and_then(|result| result);
            drop(cancelled);
            let document = match observed {
                Ok(document) if !cancel.is_cancelled() => document,
                Ok(_) => {
                    navigation.cancel();
                    close_request(id);
                    return Err(CANCELLED.into());
                }
                Err(error) => {
                    navigation.cancel();
                    close_request(id);
                    return Err(error);
                }
            };
            let lost = Arc::new(AtomicBool::new(false));
            let notified = lost.clone();
            let observed = document.clone();
            let owner = Arc::downgrade(&page);
            let subscription = document.when_lost(move |_| {
                notified.store(true, Ordering::Release);
                if let Some(page) = owner.upgrade() {
                    let matches = page
                        .document
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .as_ref()
                        .is_some_and(|(document, _)| document.is_same_document(&observed));
                    if matches && !page.closing.load(Ordering::Acquire) {
                        super::target_crashed(id);
                    }
                }
            })?;
            *page
                .document
                .lock()
                .map_err(|_| "HTML document state poisoned")? = Some((document, subscription));
            if lost.load(Ordering::Acquire) && !page.closing.load(Ordering::Acquire) {
                super::target_crashed(id);
                return Err("HTML conversion document was lost".into());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let spec = QuietSpec {
                quiet_ms: u32::try_from(NETWORK_QUIET.as_millis()).unwrap_or(u32::MAX),
                timeout_ms: u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX),
            };
            page_call(cancel, remaining + Duration::from_secs(5), |done| {
                ops.wait_quiet(spec, None, done)
            })
            .map_err(|error| describe("HTML navigation", error))?;
            Value::Null
        }
        "html.evaluate" => {
            let expression = input
                .get("expression")
                .and_then(Value::as_str)
                .ok_or("expression is missing")?;
            page_call(cancel, PAGE_CALL_TIMEOUT, |done| {
                ops.evaluate(expression, true, None, done)
            })
            .map_err(|error| describe("JavaScript evaluation", error))?
        }
        // A classic script (the converter's helper scripts are IIFEs with a trailing `;`),
        // run for its effect; `html.evaluate` takes one expression and answers with its value.
        "html.script" => {
            let source = input
                .get("source")
                .and_then(Value::as_str)
                .ok_or("script source is missing")?;
            page_call(cancel, PAGE_CALL_TIMEOUT, |done| {
                ops.run_script(source, None, done)
            })
            .map_err(|error| describe("JavaScript script", error))?;
            Value::Null
        }
        "html.initScript" => {
            let source = input
                .get("source")
                .and_then(Value::as_str)
                .ok_or("initialization script is missing")?;
            setup(&page, cancel, |done| ops.add_init_script(source, done))?;
            Value::Null
        }
        "html.screenshot" => {
            let transparent = input
                .get("omitBackground")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let clip = match input.get("clip").filter(|value| !value.is_null()) {
                Some(clip) => Some(screenshot_clip(clip)?),
                None => None,
            };
            // A clip is in page coordinates and may lie beyond the viewport.
            let spec = CaptureSpec {
                clip,
                full_page: clip.is_some(),
                transparent,
                scale: 1.0,
            };
            let png = page_call(cancel, PAGE_CALL_TIMEOUT, |done| {
                ops.capture_png(spec, None, done)
            })
            .map_err(|error| describe("HTML screenshot", error))?;
            Value::String(base64::engine::general_purpose::STANDARD.encode(png))
        }
        _ => return Err("unknown HTML page operation".into()),
    };
    if cancel.is_cancelled() {
        return Err(CANCELLED.into());
    }
    Ok(result)
}

fn screenshot_clip(clip: &Value) -> Result<Rect, String> {
    let clip = clip.as_object().ok_or("invalid screenshot clip")?;
    let number = |name: &str| {
        clip.get(name)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
    };
    match (number("x"), number("y"), number("width"), number("height")) {
        (Some(x), Some(y), Some(width), Some(height)) => Ok(Rect {
            x,
            y,
            width,
            height,
        }),
        _ => Err("invalid screenshot clip".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Navigation, screenshot_clip};
    use serde_json::json;

    #[test]
    fn target_admits_only_one_authorized_navigation() {
        let mut navigation = Navigation {
            document: Some("html-asset://cap/_document".into()),
            started: false,
        };
        assert!(navigation.allow("about:blank"));
        assert!(!navigation.allow("file:///private/data"));
        assert!(navigation.allow("html-asset://cap/_document"));
        assert!(
            navigation.allow("html-asset://cap/_document"),
            "one navigation can be asked about twice"
        );
        assert!(!navigation.allow("html-asset://cap/other"));
        assert!(!navigation.allow("about:blank"));
    }

    #[test]
    fn screenshot_clip_needs_four_finite_numbers() {
        let rect = screenshot_clip(&json!({"x": 1, "y": 2, "width": 3.5, "height": 4})).unwrap();
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (1.0, 2.0, 3.5, 4.0)
        );
        assert!(screenshot_clip(&json!({"x": 1, "y": 2, "width": 3})).is_err());
        assert!(screenshot_clip(&json!("clip")).is_err());
    }
}
