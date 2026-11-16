//! The WebView2 DevTools protocol bridge: one method call, protocol events, and the page
//! operations built on them. Everything here runs on the UI thread; only a [`Done`] crosses
//! threads, and the quiet timer thread touches nothing but shared plain data.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tauri::Webview;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2DevToolsProtocolEventReceiver,
};
use webview2_com::{
    CallDevToolsProtocolMethodCompletedHandler, DevToolsProtocolEventReceivedEventHandler,
    take_pwstr,
};
use windows::core::{HSTRING, PWSTR};

use crate::native::page::{CaptureSpec, Done, KeyChord, PageError, PdfSpec, QuietSpec, Viewport};

/// A completion that runs on the UI thread.
pub(super) type Reply<T> = Box<dyn FnOnce(Result<T, PageError>)>;

/// A [`Reply`] that both an asynchronous handler and a synchronous failure path may try to
/// complete; the first one wins.
struct Pending<T>(Rc<RefCell<Option<Reply<T>>>>);

impl<T> Clone for Pending<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Pending<T> {
    fn new(reply: Reply<T>) -> Self {
        Self(Rc::new(RefCell::new(Some(reply))))
    }

    fn complete(&self, result: Result<T, PageError>) {
        // Released before the reply runs: a reply may start the next call on this slot's owner.
        let reply = self.0.borrow_mut().take();
        if let Some(reply) = reply {
            reply(result);
        }
    }
}

/// A value that is only ever touched on the UI thread, carried across a thread hop that
/// ends on the UI thread again (`Webview::with_webview`).
pub(super) struct UiBound<T>(T);

// SAFETY: every use of the inner value is inside a `Webview::with_webview` closure, which
// runs on the UI thread that created it; the wrapper only moves the box between threads.
unsafe impl<T> Send for UiBound<T> {}

impl<T> UiBound<T> {
    pub(super) fn new(value: T) -> Self {
        Self(value)
    }

    /// By value, so a closure captures the whole wrapper rather than its non-`Send` field.
    pub(super) fn into_inner(self) -> T {
        self.0
    }
}

/// A registered protocol event handler.
pub(super) struct Subscription {
    receiver: ICoreWebView2DevToolsProtocolEventReceiver,
    token: i64,
}

impl Subscription {
    pub(super) fn cancel(self) {
        // SAFETY: UI thread. A failure means the webview is already gone, which removed
        // the handler with it.
        let _ = unsafe { self.receiver.remove_DevToolsProtocolEventReceived(self.token) };
    }
}

#[derive(Clone)]
pub(super) struct Cdp {
    core: ICoreWebView2,
}

fn answer(
    method: &str, status: windows::core::Result<()>, json: &str,
) -> Result<Value, PageError> {
    if let Err(error) = status {
        let detail = if json.is_empty() { String::new() } else { format!(": {json}") };
        return Err(PageError::Platform(format!("{method} failed: {error}{detail}")));
    }
    if json.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(json)
        .map_err(|error| PageError::Platform(format!("{method} returned invalid JSON: {error}")))
}

fn evaluated(answer: Value) -> Result<Value, PageError> {
    if let Some(details) = answer.get("exceptionDetails") {
        let message = ["/exception/description", "/exception/value", "/text"]
            .iter()
            .find_map(|pointer| details.pointer(pointer).and_then(Value::as_str))
            .unwrap_or("script evaluation failed");
        return Err(PageError::Platform(message.to_owned()));
    }
    Ok(answer.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

fn data_bytes(method: &str, answer: &Value) -> Result<Vec<u8>, PageError> {
    let data = answer
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| PageError::Platform(format!("{method} returned no data")))?;
    STANDARD
        .decode(data)
        .map_err(|error| PageError::Platform(format!("{method} returned invalid data: {error}")))
}

/// What `Page.printToPDF` takes for `spec`. Tagging and the outline go across only when asked
/// for, so a Chromium without them is not handed an unknown field.
fn pdf_params(spec: &PdfSpec) -> Value {
    let [top, right, bottom, left] = spec.margins_in;
    let mut params = json!({
        "landscape": spec.landscape,
        "printBackground": spec.print_background,
        "scale": spec.scale,
        "preferCSSPageSize": spec.prefer_css_page_size,
        "marginTop": top,
        "marginRight": right,
        "marginBottom": bottom,
        "marginLeft": left,
    });
    if let Some((width, height)) = spec.paper_in {
        params["paperWidth"] = json!(width);
        params["paperHeight"] = json!(height);
    }
    if let Some(ranges) = &spec.page_ranges {
        params["pageRanges"] = json!(ranges);
    }
    if let Some(templates) = &spec.header_footer {
        params["displayHeaderFooter"] = json!(true);
        params["headerTemplate"] = json!(templates.header_template);
        params["footerTemplate"] = json!(templates.footer_template);
    }
    if spec.tagged {
        params["generateTaggedPDF"] = json!(true);
    }
    if spec.outline {
        params["generateDocumentOutline"] = json!(true);
    }
    params
}

fn clip(x: f64, y: f64, width: f64, height: f64, scale: f64) -> Value {
    json!({ "x": x, "y": y, "width": width, "height": height, "scale": scale })
}

fn virtual_key(chord: &KeyChord) -> u32 {
    let code = chord.code.as_str();
    if let Some(letter) = code.strip_prefix("Key")
        && let [byte] = letter.as_bytes()
        && byte.is_ascii_uppercase()
    {
        return u32::from(*byte);
    }
    if let Some(digit) = code.strip_prefix("Digit")
        && let [byte] = digit.as_bytes()
        && byte.is_ascii_digit()
    {
        return u32::from(*byte);
    }
    if let Some(number) = code.strip_prefix('F').and_then(|rest| rest.parse::<u32>().ok())
        && (1..=12).contains(&number)
    {
        return 111 + number;
    }
    match code {
        "Backspace" => 8,
        "Tab" => 9,
        "Enter" | "NumpadEnter" => 13,
        "Escape" => 27,
        "Space" => 32,
        "PageUp" => 33,
        "PageDown" => 34,
        "End" => 35,
        "Home" => 36,
        "ArrowLeft" => 37,
        "ArrowUp" => 38,
        "ArrowRight" => 39,
        "ArrowDown" => 40,
        "Insert" => 45,
        "Delete" => 46,
        _ => 0,
    }
}

/// The key down and key up events of `chord`. Printable keys without a command modifier carry
/// their text so the page types them; everything else is a raw key event.
fn key_events(chord: &KeyChord) -> VecDeque<(&'static str, Value)> {
    let modifiers = &chord.modifiers;
    let mask = i32::from(modifiers.alt)
        | (i32::from(modifiers.ctrl) << 1)
        | (i32::from(modifiers.meta) << 2)
        | (i32::from(modifiers.shift) << 3);
    let virtual_key = virtual_key(chord);
    let base = |kind: &str| {
        json!({
            "type": kind,
            "modifiers": mask,
            "key": chord.key,
            "code": chord.code,
            "windowsVirtualKeyCode": virtual_key,
            "nativeVirtualKeyCode": virtual_key,
        })
    };
    let text = if modifiers.ctrl || modifiers.alt || modifiers.meta {
        None
    } else if chord.key == "Enter" {
        Some("\r".to_owned())
    } else if chord.key.chars().count() == 1 {
        Some(chord.key.clone())
    } else {
        None
    };
    let mut down = base(if text.is_some() { "keyDown" } else { "rawKeyDown" });
    if let Some(text) = text {
        down["text"] = json!(text);
        down["unmodifiedText"] = json!(text);
    }
    VecDeque::from([("Input.dispatchKeyEvent", down), ("Input.dispatchKeyEvent", base("keyUp"))])
}

impl Cdp {
    pub(super) fn new(core: ICoreWebView2) -> Self {
        Self { core }
    }

    /// One protocol call. `then` runs on the UI thread, exactly once, also when the call could
    /// not be sent.
    pub(super) fn call(&self, method: &'static str, params: &Value, then: Reply<Value>) {
        let pending = Pending::new(then);
        let handler_pending = pending.clone();
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
            move |status, json| {
                handler_pending.complete(answer(method, status, &json));
                Ok(())
            },
        ));
        let name = HSTRING::from(method);
        let parameters = HSTRING::from(params.to_string());
        // SAFETY: UI thread; WebView2 copies both strings and holds the handler it was given.
        let sent =
            unsafe { self.core.CallDevToolsProtocolMethod(&name, &parameters, &handler) };
        if let Err(error) = sent {
            pending.complete(Err(PageError::Platform(format!("{method} failed: {error}"))));
        }
    }

    /// Calls in order, each started after the previous one completed (WebView2 does not
    /// guarantee that calls complete in the order they were sent).
    pub(super) fn steps(&self, mut queue: VecDeque<(&'static str, Value)>, then: Reply<()>) {
        let Some((method, params)) = queue.pop_front() else {
            return then(Ok(()));
        };
        let next = self.clone();
        self.call(
            method,
            &params,
            Box::new(move |result| match result {
                Ok(_) => next.steps(queue, then),
                Err(error) => then(Err(error)),
            }),
        );
    }

    pub(super) fn subscribe(
        &self, event: &str, mut handle: impl FnMut(&Value) + 'static,
    ) -> Result<Subscription, PageError> {
        let fail = |error: windows::core::Error| {
            PageError::Platform(format!("subscribing to {event} failed: {error}"))
        };
        // SAFETY: UI thread; the receiver outlives the registration it owns.
        let receiver = unsafe { self.core.GetDevToolsProtocolEventReceiver(&HSTRING::from(event)) }
            .map_err(fail)?;
        let handler = DevToolsProtocolEventReceivedEventHandler::create(Box::new(
            move |_, args| {
                let Some(args) = args else {
                    return Ok(());
                };
                let mut parameters = PWSTR::null();
                // SAFETY: `args` is live for this callback; the string is taken once.
                unsafe { args.ParameterObjectAsJson(&mut parameters)? };
                if let Ok(value) = serde_json::from_str::<Value>(&take_pwstr(parameters)) {
                    handle(&value);
                }
                Ok(())
            },
        ));
        let mut token = 0i64;
        // SAFETY: UI thread; `token` is written once by the call.
        unsafe { receiver.add_DevToolsProtocolEventReceived(&handler, &mut token) }
            .map_err(fail)?;
        Ok(Subscription { receiver, token })
    }

    pub(super) fn evaluate(&self, expression: String, await_promise: bool, done: Reply<Value>) {
        let params = json!({
            "expression": expression,
            "awaitPromise": await_promise,
            "returnByValue": true,
        });
        self.call(
            "Runtime.evaluate",
            &params,
            Box::new(move |result| done(result.and_then(evaluated))),
        );
    }

    /// A classic script run for its effect: its completion value is not requested, so a value
    /// that cannot be serialized (a DOM node) is no failure; a thrown exception is.
    pub(super) fn run_script(&self, source: String, done: Reply<()>) {
        let params = json!({
            "expression": source,
            "awaitPromise": false,
            "returnByValue": false,
        });
        self.call(
            "Runtime.evaluate",
            &params,
            Box::new(move |result| done(result.and_then(evaluated).map(|_| ()))),
        );
    }

    pub(super) fn print_pdf(&self, spec: &PdfSpec, done: Reply<Vec<u8>>) {
        self.call(
            "Page.printToPDF",
            &pdf_params(spec),
            Box::new(move |result| {
                done(result.and_then(|answer| data_bytes("Page.printToPDF", &answer)))
            }),
        );
    }

    pub(super) fn set_viewport(&self, viewport: Viewport, done: Reply<()>) {
        let params = json!({
            "width": viewport.width,
            "height": viewport.height,
            "deviceScaleFactor": viewport.device_scale_factor,
            "mobile": false,
        });
        self.steps(VecDeque::from([("Emulation.setDeviceMetricsOverride", params)]), done);
    }

    pub(super) fn add_init_script(&self, source: String, done: Reply<()>) {
        let params = json!({ "source": source });
        self.steps(VecDeque::from([("Page.addScriptToEvaluateOnNewDocument", params)]), done);
    }

    pub(super) fn key(&self, chord: &KeyChord, done: Reply<()>) {
        self.steps(key_events(chord), done);
    }

    /// A Chromium editing command (`paste`, `selectAll`, ...) as a key event carrying it.
    pub(super) fn editing_command(&self, command: &'static str, done: Reply<()>) {
        let params = json!({ "type": "rawKeyDown", "commands": [command] });
        self.steps(VecDeque::from([("Input.dispatchKeyEvent", params)]), done);
    }

    pub(super) fn capture(&self, spec: CaptureSpec, done: Reply<Vec<u8>>) {
        if !spec.transparent {
            return self.capture_clipped(spec, done);
        }
        const OVERRIDE: &str = "Emulation.setDefaultBackgroundColorOverride";
        let this = self.clone();
        self.call(
            OVERRIDE,
            &json!({ "color": { "r": 0, "g": 0, "b": 0, "a": 0 } }),
            Box::new(move |result| {
                if let Err(error) = result {
                    return done(Err(error));
                }
                let restore = this.clone();
                this.capture_clipped(
                    spec,
                    Box::new(move |shot| {
                        // The default background is restored whatever the capture said.
                        restore.call(
                            OVERRIDE,
                            &json!({}),
                            Box::new(move |restored| done(shot.and_then(|png| restored.map(|_| png)))),
                        );
                    }),
                );
            }),
        );
    }

    fn capture_clipped(&self, spec: CaptureSpec, done: Reply<Vec<u8>>) {
        if let Some(rect) = spec.clip {
            let clip = clip(rect.x, rect.y, rect.width, rect.height, spec.scale);
            return self.shoot(spec.full_page, Some(clip), done);
        }
        if !spec.full_page && spec.scale == 1.0 {
            return self.shoot(false, None, done);
        }
        let this = self.clone();
        let whole_page = spec.full_page;
        self.call(
            "Page.getLayoutMetrics",
            &json!({}),
            Box::new(move |result| {
                let metrics = match result {
                    Ok(metrics) => metrics,
                    Err(error) => return done(Err(error)),
                };
                let number = |pointer: &str| metrics.pointer(pointer).and_then(Value::as_f64);
                let area = if whole_page {
                    number("/cssContentSize/width").zip(number("/cssContentSize/height")).map(
                        |(width, height)| clip(0.0, 0.0, width, height, spec.scale),
                    )
                } else {
                    number("/cssVisualViewport/clientWidth")
                        .zip(number("/cssVisualViewport/clientHeight"))
                        .map(|(width, height)| {
                            let x = number("/cssVisualViewport/pageX").unwrap_or(0.0);
                            let y = number("/cssVisualViewport/pageY").unwrap_or(0.0);
                            clip(x, y, width, height, spec.scale)
                        })
                };
                match area {
                    Some(area) => this.shoot(whole_page, Some(area), done),
                    None => done(Err(PageError::Platform(
                        "Page.getLayoutMetrics returned no page size".into(),
                    ))),
                }
            }),
        );
    }

    fn shoot(&self, beyond_viewport: bool, clip: Option<Value>, done: Reply<Vec<u8>>) {
        let mut params = json!({ "format": "png", "captureBeyondViewport": beyond_viewport });
        if let Some(clip) = clip {
            params["clip"] = clip;
        }
        self.call(
            "Page.captureScreenshot",
            &params,
            Box::new(move |result| {
                done(result.and_then(|answer| data_bytes("Page.captureScreenshot", &answer)))
            }),
        );
    }
}

/// What the quiet timer reads and the protocol events write.
struct Quiet {
    /// The setup calls (`Page.enable`, `Network.enable`, the `readyState` check) finished.
    armed: bool,
    loaded: bool,
    active: HashSet<String>,
    changed: Instant,
    /// A setup step failed; the timer thread answers with it.
    failed: Option<PageError>,
}

type Shared = Arc<(Mutex<Quiet>, Condvar)>;

fn locked(shared: &Shared) -> MutexGuard<'_, Quiet> {
    shared.0.lock().unwrap_or_else(|error| error.into_inner())
}

fn touch(shared: &Shared, change: impl FnOnce(&mut Quiet)) {
    let mut state = locked(shared);
    change(&mut state);
    state.changed = Instant::now();
    drop(state);
    shared.1.notify_all();
}

fn request_id(params: &Value) -> Option<String> {
    params.get("requestId").and_then(Value::as_str).map(str::to_owned)
}

/// Waits until the page is loaded and no request has been in flight for `spec.quiet_ms`,
/// within `spec.timeout_ms`. Events are subscribed before `Network.enable`; requests that
/// started before this call are not counted (`document.readyState` seeds `loaded`).
pub(super) fn wait_quiet(webview: &Webview, cdp: Cdp, spec: QuietSpec, done: Done<()>) {
    let started = Instant::now();
    let shared: Shared = Arc::new((
        Mutex::new(Quiet {
            armed: false,
            loaded: false,
            active: HashSet::new(),
            changed: started,
            failed: None,
        }),
        Condvar::new(),
    ));
    let mut subscriptions = Vec::new();
    for event in [
        "Network.requestWillBeSent",
        "Network.loadingFinished",
        "Network.loadingFailed",
        "Page.loadEventFired",
    ] {
        let sink = shared.clone();
        let subscribed = cdp.subscribe(event, move |params| match event {
            "Network.requestWillBeSent" => {
                if let Some(id) = request_id(params) {
                    touch(&sink, |state| {
                        state.active.insert(id);
                    });
                }
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                if let Some(id) = request_id(params) {
                    touch(&sink, |state| {
                        state.active.remove(&id);
                    });
                }
            }
            _ => touch(&sink, |state| state.loaded = true),
        });
        match subscribed {
            Ok(subscription) => subscriptions.push(subscription),
            Err(error) => {
                subscriptions.into_iter().for_each(Subscription::cancel);
                return done(Err(error));
            }
        }
    }

    let cleanup = UiBound::new((cdp.clone(), subscriptions));
    let owner = webview.clone();
    let timer_state = shared.clone();
    let spawned = std::thread::Builder::new().name("webview-quiet".into()).spawn(move || {
        let result = await_quiet(&timer_state, started, spec);
        // The handlers and the protocol domains are released on the UI thread.
        let _ = owner.with_webview(move |_| {
            let (cdp, subscriptions) = cleanup.into_inner();
            subscriptions.into_iter().for_each(Subscription::cancel);
            cdp.call("Network.disable", &json!({}), Box::new(|_| ()));
        });
        done(result);
    });
    if let Err(error) = spawned {
        // The closure (and with it `done`) is gone; the caller sees `Cancelled`.
        eprintln!("shell: the page quiet timer could not start: {error}");
        return;
    }

    let armed = shared.clone();
    let failed = shared;
    let probe = cdp.clone();
    cdp.steps(
        VecDeque::from([("Page.enable", json!({})), ("Network.enable", json!({}))]),
        Box::new(move |result| match result {
            Err(error) => touch(&failed, |state| state.failed = Some(error)),
            Ok(()) => probe.evaluate(
                "document.readyState".into(),
                false,
                Box::new(move |state| match state {
                    Ok(state) => touch(&armed, |quiet| {
                        quiet.loaded |= state.as_str() == Some("complete");
                        quiet.armed = true;
                    }),
                    Err(error) => touch(&armed, |quiet| quiet.failed = Some(error)),
                }),
            ),
        }),
    );
}

fn await_quiet(shared: &Shared, started: Instant, spec: QuietSpec) -> Result<(), PageError> {
    let quiet = Duration::from_millis(u64::from(spec.quiet_ms));
    let deadline = started + Duration::from_millis(u64::from(spec.timeout_ms));
    let mut state = locked(shared);
    loop {
        if let Some(error) = state.failed.take() {
            return Err(error);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(PageError::Timeout);
        }
        let mut wait = deadline - now;
        if state.armed && state.loaded && state.active.is_empty() {
            let idle = now.duration_since(state.changed);
            if idle >= quiet {
                return Ok(());
            }
            wait = wait.min(quiet - idle);
        }
        state = shared
            .1
            .wait_timeout(state, wait.max(Duration::from_millis(1)))
            .unwrap_or_else(|error| error.into_inner())
            .0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::page::{HeaderFooter, Modifiers};

    #[test]
    fn the_pdf_parameters_name_only_what_the_spec_sets() {
        let plain = pdf_params(&PdfSpec::default());
        assert_eq!(plain["marginTop"], json!(0.4));
        assert!(plain.get("paperWidth").is_none());
        assert!(plain.get("displayHeaderFooter").is_none());
        assert!(plain.get("generateTaggedPDF").is_none());

        let spec = PdfSpec {
            landscape: true,
            paper_in: Some((8.27, 11.69)),
            page_ranges: Some("1-2".into()),
            header_footer: Some(HeaderFooter {
                header_template: "<h>".into(),
                footer_template: "<f>".into(),
            }),
            tagged: true,
            ..PdfSpec::default()
        };
        let params = pdf_params(&spec);
        assert_eq!(params["paperHeight"], json!(11.69));
        assert_eq!(params["pageRanges"], json!("1-2"));
        assert_eq!(params["displayHeaderFooter"], json!(true));
        assert_eq!(params["headerTemplate"], json!("<h>"));
        assert_eq!(params["generateTaggedPDF"], json!(true));
    }

    #[test]
    fn a_command_chord_is_a_raw_key_event_and_a_letter_is_typed() {
        let ctrl_e = KeyChord {
            key: "e".into(),
            code: "KeyE".into(),
            modifiers: Modifiers { ctrl: true, ..Modifiers::default() },
        };
        let events = key_events(&ctrl_e);
        assert_eq!(events[0].1["type"], json!("rawKeyDown"));
        assert_eq!(events[0].1["modifiers"], json!(2));
        assert_eq!(events[0].1["windowsVirtualKeyCode"], json!(69));
        assert_eq!(events[1].1["type"], json!("keyUp"));

        let typed = KeyChord { key: "a".into(), code: "KeyA".into(), modifiers: Modifiers::default() };
        let events = key_events(&typed);
        assert_eq!(events[0].1["type"], json!("keyDown"));
        assert_eq!(events[0].1["text"], json!("a"));
    }

    #[test]
    fn a_script_exception_surfaces_its_description() {
        let thrown = json!({"exceptionDetails": {"exception": {"description": "Error: stale"}}});
        assert_eq!(evaluated(thrown), Err(PageError::Platform("Error: stale".into())));
        let value = json!({"result": {"type": "number", "value": 2}});
        assert_eq!(evaluated(value), Ok(json!(2)));
        assert_eq!(evaluated(json!({"result": {"type": "undefined"}})), Ok(Value::Null));
    }
}
