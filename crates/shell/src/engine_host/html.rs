use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, LazyLock, Mutex, mpsc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shell_contract::runtime::CancelToken;
use tauri::{AppHandle, Manager, Webview, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_runtime_cef::{DevToolsProtocol, NativeObservedDocument, NativeObservedDocumentSubscription, WebviewCefExt, WebviewWindowBuilderCefExt, NativeBrowserClose, NativeBrowserCloseSubscription, NativeBrowserSubmissionGuard, NativeWindowClose, NativeWindowCloseSubscription, NativeWindowSubmissionGuard};

use crate::host::contents::{cdp, forget_webview};

#[derive(Default)]
struct Network { active: HashSet<String>, loaded: bool, changed: Option<Instant> }
struct Navigation { document: Option<String>, started: bool }
impl Navigation {
    fn allow(&mut self, url: &str) -> bool {
        if url == "about:blank" && !self.started { return true; }
        if self.started || self.document.as_deref() != Some(url) { return false; }
        self.started = true;
        true
    }
}
type CloseWaiter = Box<dyn FnOnce(Result<(), String>) + Send>;
#[derive(Default)]
struct CloseState {
    browser_done: bool,
    window_done: bool,
    browser_subscription: Option<NativeBrowserCloseSubscription>,
    window_subscription: Option<NativeWindowCloseSubscription>,
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
    network: Arc<(Mutex<Network>, Condvar)>,
    browser: Mutex<Option<tauri_runtime_cef::FrameNavigationState>>,
    document: Mutex<Option<(NativeObservedDocument, NativeObservedDocumentSubscription)>>,
    browser_close: NativeBrowserClose,
    window_close: NativeWindowClose,
    window: Mutex<Option<WebviewWindow>>,
    close_state: Mutex<CloseState>,
}
static PAGES: LazyLock<Mutex<HashMap<u64, Arc<Page>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn close_error(page: &Arc<Page>, error: String) {
    let waiters = std::mem::take(&mut page.close_state.lock().unwrap_or_else(|p| p.into_inner()).waiters);
    for waiter in waiters { waiter(Err(error.clone())); }
    if page.retrying_close.swap(true, Ordering::AcqRel) { return; }
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
            let window_ready = page.window.lock().unwrap_or_else(|p| p.into_inner()).is_some();
            if page.browser_close_requested.load(Ordering::Acquire)
                && (!window_ready || page.window_close_requested.load(Ordering::Acquire)) {
                page.retrying_close.store(false, Ordering::Release);
                if page.browser_close_requested.load(Ordering::Acquire)
                    && (!window_ready || page.window_close_requested.load(Ordering::Acquire)) { return; }
                if page.retrying_close.swap(true, Ordering::AcqRel) { return; }
            }
        }
    });
}

fn terminal(page: &Arc<Page>, browser: bool) {
    let waiters = {
        let mut state = page.close_state.lock().unwrap_or_else(|p| p.into_inner());
        if browser { state.browser_done = true; } else { state.window_done = true; }
        if !state.browser_done || !state.window_done { return; }
        std::mem::take(&mut state.waiters)
    };
    let mut pages = PAGES.lock().unwrap_or_else(|p| p.into_inner());
    if pages.get(&page.id).is_some_and(|current| Arc::ptr_eq(current, page)) { pages.remove(&page.id); }
    drop(pages);
    for waiter in waiters { waiter(Ok(())); }
}

fn close(page: &Arc<Page>) {
    page.closing.store(true, Ordering::Release);
    page.grant.lock().unwrap_or_else(|p| p.into_inner()).take();
    forget_webview(&page.label);
    if !page.browser_close_requested.swap(true, Ordering::AcqRel) {
        if let Err(error) = page.browser_close.request_close() {
            page.browser_close_requested.store(false, Ordering::Release);
            close_error(page, error);
        }
    }
    let window = page.window.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if let Some(window) = window {
        if page.window_close_requested.swap(true, Ordering::AcqRel) { return; }
        let captured = page.clone();
        if let Err(error) = page.app.run_on_main_thread(move || {
            if let Err(error) = window.destroy() {
                captured.window_close_requested.store(false, Ordering::Release);
                close_error(&captured, error.to_string());
            }
        }) {
            page.window_close_requested.store(false, Ordering::Release);
            close_error(page, error.to_string());
        }
    }
}

pub(super) fn close_request(id: u64) {
    let page = PAGES.lock().unwrap_or_else(|p| p.into_inner()).get(&id).cloned();
    if let Some(page) = page { close(&page); }
}

pub(super) fn after_close(id: u64, waiter: CloseWaiter) {
    let page = PAGES.lock().unwrap_or_else(|p| p.into_inner()).get(&id).cloned();
    let Some(page) = page else { waiter(Ok(())); return };
    let mut waiter = Some(waiter);
    {
        let mut state = page.close_state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.browser_done || !state.window_done { state.waiters.push(waiter.take().unwrap()); }
    }
    if let Some(waiter) = waiter { waiter(Ok(())); } else { close(&page); }
}

struct Opening { id: u64, cancel: CancelToken, complete: bool }
impl Drop for Opening {
    fn drop(&mut self) {
        if !self.complete { self.cancel.cancel(); close_request(self.id); }
    }
}

fn webview(page: &Page) -> Result<Webview, String> {
    page.app.get_webview(&page.label).ok_or_else(|| "HTML conversion page is closed".into())
}

fn setup(page: &Arc<Page>, view: &Webview, method: &str, params: Value, cancel: &CancelToken) -> Result<(), String> {
    let browser = page.browser.lock().map_err(|_| "HTML browser identity poisoned")?
        .clone().ok_or("HTML browser identity is unavailable")?;
    let (sent, received) = mpsc::sync_channel(1);
    let wake = sent.clone();
    let subscription = cancel.on_cancel(move || { let _ = wake.try_send(Err("HTML conversion was cancelled".to_owned())); });
    if cancel.is_cancelled() { return Err("HTML conversion was cancelled".into()); }
    let request = view.send_browser_setup_protocol(browser, method.to_owned(), params, move |result| {
        let _ = sent.try_send(result.map(|_| ()));
    })?;
    let result = received.recv_timeout(Duration::from_secs(90))
        .map_err(|_| "HTML browser setup timed out".to_owned()).and_then(|result| result);
    drop(subscription);
    if result.is_err() || cancel.is_cancelled() {
        request.cancel();
        close(page);
        return result.and(Err("HTML conversion was cancelled".into()));
    }
    result
}

fn open(app: &AppHandle, id: u64, request: &Value, cancel: &CancelToken) -> Result<Value, String> {
    if cancel.is_cancelled() { return Err("HTML conversion was cancelled".into()); }
    use base64::Engine as _;
    use crate::host::document_assets::{AssetKind, TargetDocument};
    let (kind, document) = if let Some(encoded) = request.get("htmlBase64").and_then(Value::as_str) {
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_| "invalid generated HTML bytes")?;
        (AssetKind::Docs, TargetDocument::Generated(bytes))
    } else {
        let path = request.get("path").and_then(Value::as_str).ok_or("HTML conversion needs a document")?;
        (AssetKind::Html, TargetDocument::Existing(std::path::PathBuf::from(path)))
    };
    let owner = request.get("ownerViewId").and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok()).ok_or("HTML conversion needs an originating view")?;
    let label = format!("seuoffice-html-convert-{id}");
    let browser_submission = NativeBrowserSubmissionGuard::new();
    let window_submission = NativeWindowSubmissionGuard::new();
    let page = Arc::new(Page {
        id, app: app.clone(), label: label.clone(),
        navigation: Arc::new(Mutex::new(Navigation { document: None, started: false })),
        grant: Mutex::new(None),
        closing: AtomicBool::new(false),
        browser_close_requested: AtomicBool::new(false),
        window_close_requested: AtomicBool::new(false),
        retrying_close: AtomicBool::new(false),
        network: Arc::new((Mutex::new(Network::default()), Condvar::new())),
        browser: Mutex::new(None),
        document: Mutex::new(None),
        browser_close: browser_submission.receipt(),
        window_close: window_submission.receipt(),
        window: Mutex::new(None),
        close_state: Mutex::new(CloseState::default()),
    });
    {
        let mut pages = PAGES.lock().map_err(|_| "HTML page registry poisoned")?;
        if pages.contains_key(&id) { return Err("HTML conversion page already exists".into()); }
        if cancel.is_cancelled() { return Err("HTML conversion was cancelled".into()); }
        pages.insert(id, page.clone());
    }
    let mut opening = Opening { id, cancel: cancel.clone(), complete: false };
    let browser_page = Arc::downgrade(&page);
    let browser_subscription = page.browser_close.when_closed(move |_| {
        if let Some(page) = browser_page.upgrade() { terminal(&page, true); }
    })?;
    let window_page = Arc::downgrade(&page);
    let window_subscription = page.window_close.when_closed(move |_| {
        if let Some(page) = window_page.upgrade() { terminal(&page, false); }
    })?;
    {
        let mut state = page.close_state.lock().map_err(|_| "HTML close state poisoned")?;
        state.browser_subscription = Some(browser_subscription);
        state.window_subscription = Some(window_subscription);
    }
    let (blank_tx, blank_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let blank_cancel = blank_tx.clone();
    let blank_subscription = cancel.on_cancel(move || { let _ = blank_cancel.try_send(Err("HTML conversion was cancelled".into())); });
    let blank_failed = blank_tx.clone();
    let blank_loaded = blank_tx;
    let (tx, rx) = mpsc::sync_channel(1);
    let app_for_ui = app.clone();
    let page_for_ui = page.clone();
    let page_for_event = page.clone();
    let navigation_for_ui = page.navigation.clone();
    let cancel_for_ui = cancel.clone();
    app.run_on_main_thread(move || {
        if cancel_for_ui.is_cancelled() { let _ = tx.send(Err("HTML conversion was cancelled".to_owned())); return; }
        let result = (|| {
            let blank = tauri::Url::parse("about:blank").map_err(|e| e.to_string())?;
            let window = WebviewWindowBuilder::new(&app_for_ui, &label, WebviewUrl::External(blank))
                .native_browser_submission(browser_submission)
                .native_window_submission(window_submission)
                .title("HTML conversion").visible(false).focused(false).skip_taskbar(true)
                .on_navigation(move |url| {
                    let Ok(mut navigation) = navigation_for_ui.lock() else { return false; };
                    navigation.allow(url.as_str())
                })
                .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
                .on_page_load(move |_, payload| {
                    if payload.url().as_str() == "about:blank" && payload.event() == tauri::webview::PageLoadEvent::Finished {
                        let _ = blank_loaded.try_send(Ok(()));
                    }
                })
                .on_frame_event(move |event| {
                    if event.is_main && matches!(&event.kind, tauri_runtime_cef::FrameEventKind::NavigationFailed { url } if url.as_str() == "about:blank") {
                        let _ = blank_failed.try_send(Err("HTML browser failed to initialize".into()));
                    }
                })
                .build().map_err(|e| e.to_string())?;
            window.on_window_event(move |event| {
                if matches!(event, tauri::WindowEvent::Destroyed) && !page_for_event.closing.load(Ordering::Acquire) {
                    super::target_crashed(id);
                }
            });
            *page_for_ui.window.lock().map_err(|_| "HTML window state poisoned")? = Some(window);
            if page_for_ui.closing.load(Ordering::Acquire) { close(&page_for_ui); }
            if cancel_for_ui.is_cancelled() { close(&page_for_ui); return Err("HTML conversion was cancelled".into()); }
            Ok(())
        })();
        let _ = tx.send(result);
    }).map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(30)).map_err(|_| "HTML page creation did not finish".to_owned())??;
    blank_rx.recv_timeout(Duration::from_secs(30)).map_err(|_| "HTML browser did not initialize".to_owned())??;
    drop(blank_subscription);
    if cancel.is_cancelled() { close(&page); return Err("HTML conversion was cancelled".into()); }
    let view = webview(&page)?;
    let (native_tx, native_rx) = mpsc::sync_channel(1);
    let native_cancel = native_tx.clone();
    let native_subscription = cancel.on_cancel(move || { let _ = native_cancel.try_send(Err("HTML conversion was cancelled".to_owned())); });
    view.with_cef_webview_async(move |native| {
        let _ = native_tx.try_send(native.map(|native| native.frame_navigation_state().clone()).map_err(|error| error.to_string()));
    }).map_err(|e| e.to_string())?;
    let native = native_rx.recv_timeout(Duration::from_secs(30)).map_err(|_| "HTML browser identity was not observed")??;
    drop(native_subscription);
    if cancel.is_cancelled() { return Err("HTML conversion was cancelled".into()); }
    *page.browser.lock().map_err(|_| "HTML browser identity poisoned")? = Some(native.clone());
    let grant = crate::host::document_assets::grant_target(
        &page.label, shell_contract::ViewId(owner), kind, native, document,
    )?;
    grant.set_cancel_token(cancel.clone())?;
    let url = grant.document_url();
    if cancel.is_cancelled() { return Err("HTML conversion was cancelled".into()); }
    *page.navigation.lock().map_err(|_| "HTML navigation policy poisoned")? = Navigation { document: Some(url.clone()), started: false };
    *page.grant.lock().map_err(|_| "HTML asset grant poisoned")? = Some(grant);
    let observed = page.network.clone();
    let (registered, registration) = mpsc::sync_channel(1);
    let cancelled_registration = registered.clone();
    let registration_subscription = cancel.on_cancel(move || { let _ = cancelled_registration.try_send(Err("HTML conversion was cancelled".to_owned())); });
    view.on_dev_tools_protocol_async(move |event| {
        let DevToolsProtocol::Event { method, params } = event else { return; };
        let Ok(data) = serde_json::from_slice::<Value>(&params) else { return; };
        let (lock, changed) = &*observed;
        let Ok(mut network) = lock.lock() else { return; };
        match method.as_str() {
            "Network.requestWillBeSent" => if let Some(id) = data.get("requestId").and_then(Value::as_str) { network.active.insert(id.to_owned()); },
            "Network.loadingFinished" | "Network.loadingFailed" => if let Some(id) = data.get("requestId").and_then(Value::as_str) { network.active.remove(id); },
            "Page.loadEventFired" => network.loaded = true,
            _ => return,
        }
        network.changed = Some(Instant::now());
        changed.notify_all();
    }, move |result| { let _ = registered.try_send(result.map_err(|error| error.to_string())); }).map_err(|e| e.to_string())?;
    registration.recv_timeout(Duration::from_secs(30)).map_err(|_| "HTML browser observer did not initialize".to_owned())??;
    drop(registration_subscription);
    if cancel.is_cancelled() { return Err("HTML conversion was cancelled".into()); }
    setup(&page, &view, "Page.enable", json!({}), cancel)?;
    setup(&page, &view, "Network.enable", json!({}), cancel)?;
    if cancel.is_cancelled() { return Err("HTML conversion was cancelled".into()); }
    opening.complete = true;
    Ok(json!({"url": url}))
}

pub(super) fn dispatch(app: &AppHandle, id: u64, request: &Value, operation: &str, input: &Value, cancel: &CancelToken) -> Result<Value, String> {
    if operation == "html.open" { return open(app, id, request, cancel); }
    if operation == "html.close" { close_request(id); return Ok(Value::Null); }
    if cancel.is_cancelled() { return Err("HTML conversion was cancelled".into()); }
    let page = PAGES.lock().map_err(|_| "HTML page registry poisoned")?.get(&id).cloned().ok_or("HTML conversion page is closed")?;
    let view = webview(&page)?;
    let result = match operation {
        "html.viewport" => {
            let width = input.get("width").and_then(Value::as_u64).filter(|n| *n > 0 && *n <= 32768).ok_or("invalid viewport width")?;
            let height = input.get("height").and_then(Value::as_u64).filter(|n| *n > 0 && *n <= 32768).ok_or("invalid viewport height")?;
            let scale = input.get("deviceScaleFactor").and_then(Value::as_f64).unwrap_or(1.0);
            if !scale.is_finite() || scale <= 0.0 { return Err("invalid device scale factor".into()); }
            setup(&page, &view, "Emulation.setDeviceMetricsOverride", json!({"width":width,"height":height,"deviceScaleFactor":scale,"mobile":false}), cancel)?;
            Value::Null
        }
        "html.goto" => {
            let url = input.get("url").and_then(Value::as_str).ok_or("navigation URL is missing")?;
            if page.navigation.lock().map_err(|_| "HTML navigation policy poisoned")?.document.as_deref() != Some(url) {
                return Err("navigation URL does not belong to this request".into());
            }
            let timeout = input.get("timeoutMs").and_then(Value::as_u64).ok_or("navigation timeout is missing")?;
            let deadline = Instant::now().checked_add(Duration::from_millis(timeout)).ok_or("navigation timeout is too large")?;
            let (lock, changed) = &*page.network;
            { let mut state = lock.lock().map_err(|_| "network state poisoned")?; *state = Network::default(); }
            let (sent, received) = mpsc::sync_channel(1);
            let wake = sent.clone();
            let cancelled = cancel.on_cancel(move || { let _ = wake.try_send(Err("HTML conversion was cancelled".to_owned())); });
            let parsed = tauri::Url::parse(url).map_err(|error| error.to_string())?;
            let navigation = view.navigate_observed(parsed, move |result| { let _ = sent.try_send(result); })
                .map_err(|error| error.to_string())?;
            let observed = received.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| "HTML navigation timed out".to_owned()).and_then(|result| result);
            drop(cancelled);
            let document = match observed {
                Ok(document) if !cancel.is_cancelled() => document,
                Ok(_) => {
                    navigation.cancel();
                    close_request(id);
                    return Err("HTML conversion was cancelled".into());
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
                    let matches = page.document.lock().unwrap_or_else(|error| error.into_inner())
                        .as_ref().is_some_and(|(document, _)| document.is_same_document(&observed));
                    if matches && !page.closing.load(Ordering::Acquire) { super::target_crashed(id); }
                }
            })?;
            *page.document.lock().map_err(|_| "HTML document state poisoned")? = Some((document, subscription));
            if lost.load(Ordering::Acquire) && !page.closing.load(Ordering::Acquire) {
                super::target_crashed(id);
                return Err("HTML conversion document was lost".into());
            }
            let mut state = lock.lock().map_err(|_| "network state poisoned")?;
            loop {
                if cancel.is_cancelled() { return Err("HTML conversion was cancelled".into()); }
                let now = Instant::now();
                if state.loaded && state.active.is_empty() && state.changed.is_some_and(|last| now.duration_since(last) >= Duration::from_millis(500)) { break; }
                if now >= deadline { return Err("HTML navigation timed out".into()); }
                let wait = deadline.saturating_duration_since(now).min(Duration::from_millis(50));
                state = changed.wait_timeout(state, wait).map_err(|_| "network state poisoned")?.0;
            }
            Value::Null
        }
        "html.evaluate" => {
            let expression = input.get("expression").and_then(Value::as_str).ok_or("expression is missing")?;
            let answer = cdp(&view, "Runtime.evaluate", json!({"expression":expression,"awaitPromise":true,"returnByValue":true}))?;
            if let Some(exception) = answer.get("exceptionDetails") { return Err(format!("JavaScript evaluation failed: {exception}")); }
            answer.get("result").and_then(|value| value.get("value")).cloned().unwrap_or(Value::Null)
        }
        "html.initScript" => {
            let source = input.get("source").and_then(Value::as_str).ok_or("initialization script is missing")?;
            setup(&page, &view, "Page.addScriptToEvaluateOnNewDocument", json!({"source":source}), cancel)?;
            Value::Null
        }
        "html.screenshot" => {
            let transparent = input.get("omitBackground").and_then(Value::as_bool).unwrap_or(false);
            let mut params = json!({"format":"png","captureBeyondViewport":true});
            if let Some(clip) = input.get("clip").filter(|value| !value.is_null()) {
                let mut clip = clip.as_object().ok_or("invalid screenshot clip")?.clone();
                for name in ["x", "y", "width", "height"] { if !clip.get(name).and_then(Value::as_f64).is_some_and(f64::is_finite) { return Err("invalid screenshot clip".into()); } }
                clip.insert("scale".into(), json!(1)); params["clip"] = Value::Object(clip);
            }
            if transparent { cdp(&view, "Emulation.setDefaultBackgroundColorOverride", json!({"color":{"r":0,"g":0,"b":0,"a":0}}))?; }
            let captured = cdp(&view, "Page.captureScreenshot", params);
            if transparent { cdp(&view, "Emulation.setDefaultBackgroundColorOverride", json!({}))?; }
            let answer = captured?;
            Value::String(answer.get("data").and_then(Value::as_str).ok_or("screenshot returned no data")?.to_owned())
        }
        _ => return Err("unknown HTML page operation".into()),
    };
    if cancel.is_cancelled() { return Err("HTML conversion was cancelled".into()); }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::Navigation;

    #[test]
    fn target_admits_only_one_authorized_navigation() {
        let mut navigation = Navigation { document: Some("html-asset://cap/_document".into()), started: false };
        assert!(navigation.allow("about:blank"));
        assert!(!navigation.allow("file:///private/data"));
        assert!(navigation.allow("html-asset://cap/_document"));
        assert!(!navigation.allow("html-asset://cap/_document"));
        assert!(!navigation.allow("about:blank"));
    }
}
