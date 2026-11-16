//! P9: `on_new_window` on the system webviews (url, attribution to the registered document) and
//! the timing of `Webview::close` (design P9; S3, S4 and S6 inferences).
//!
//! Parts, each a `po.suite` line in the record:
//!
//! - `new-window`: the popups a page asks for five ways (script, features string, anchor with
//!   `target=_blank`, from a child frame, from a trusted keydown). The `on_new_window` hook
//!   records the url, whether it runs on the UI thread and which document the native layer
//!   says is current (`win.new_window`).
//! - `child-close`: a child webview of a bare window (the shape of the shell's tabs):
//!   `navigate_observed(about:blank)` of a registered document, then `request_close` and how
//!   soon the label leaves Tauri's registry and the `Retired` event follows.
//! - `window-retired`: destroying a window retires its browser through the forwarded
//!   `WindowEvent::Destroyed`.
//! - `focus`: focus events per view of two windows.
//! - `real-windows`: the shell's own `Windows` opens a window with a transparent child window
//!   parented to it.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde_json::{Value, json};
use shell_contract::Surface;
use tauri::{AppHandle, Manager, WebviewUrl};

use super::harness::{
    Ctx, Direct, app_url, destroy, eval, note_event, open_direct, page_of, record_op, wait_done,
};
use super::log::{self, Timeline, record};
use crate::host::window::{Bridge, ViewSpec, WindowOptions, WindowSpec};
use crate::native::{
    BridgeMode, BrowserIdentity, EventSink, KeyChord, Modifiers, Native, NativeEvent, NativeSpec,
    WebviewBuilderExt,
};

const HOST: &str = "probe-p9-host";
const CHILD: &str = "probe-p9-child";

pub fn run(ctx: &Ctx) -> Result<(), String> {
    let mut problems = Vec::new();
    for (name, part) in [
        ("new-window", new_window as fn(&Ctx) -> Result<(), String>),
        ("child-close", child_close),
        ("window-retired", window_retired),
        ("focus", focus),
        ("real-windows", real_windows),
    ] {
        record("rust", "po.suite", json!({ "suite": name }));
        if let Err(error) = part(ctx) {
            record(
                "rust",
                "po.suite.failed",
                json!({ "suite": name, "error": error }),
            );
            problems.push(format!("{name}: {error}"));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

fn new_window(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-p9-main";
    let from = log::pages().len();
    let mut spec = Direct::new(label, &app_url("shell", "p9-main.html"));
    spec.bridge = BridgeMode::Invoke;
    spec.new_window = true;
    let opened = open_direct(ctx, spec)?;
    let ready = opened
        .timeline
        .wait_kind("DocumentReady", 0, Duration::from_secs(30));
    let done = wait_done(label, from, 60);
    record(
        "rust",
        "p9.wait",
        json!({ "document_ready": ready, "page_done": done }),
    );
    for variant in ["open", "features", "anchor", "iframe"] {
        record("rust", "p9.variant.start", json!({ "name": variant }));
        let started = log::now_ms();
        let result = page_of(ctx, label).and_then(|page| {
            eval(&page, &format!("window.__variant('{variant}')"), 20)
                .map_err(|error| error.to_string())
        });
        record_op(&format!("variant.{variant}"), label, started, result);
        std::thread::sleep(Duration::from_millis(1200));
    }
    record("rust", "p9.variant.start", json!({ "name": "key" }));
    let started = log::now_ms();
    let chord = KeyChord {
        key: "x".into(),
        code: "KeyX".into(),
        modifiers: Modifiers::default(),
    };
    let result = page_of(ctx, label).and_then(|page| {
        page.blocking(Duration::from_secs(20), |done| page.key(chord, done))
            .map(|()| Value::Null)
            .map_err(|error| error.to_string())
    });
    record_op("variant.key", label, started, result);
    std::thread::sleep(Duration::from_millis(1500));
    destroy(ctx, label);
    Ok(())
}

/// A bare window with one child webview, built on the UI thread as the shell builds its tabs.
fn build_child(
    app: &AppHandle,
    native: &Native,
    timeline: &Arc<Timeline>,
    url: &str,
) -> Result<BrowserIdentity, String> {
    let window = tauri::window::WindowBuilder::new(app, HOST)
        .title(HOST)
        .inner_size(520.0, 420.0)
        .build()
        .map_err(|error| error.to_string())?;
    let sink_timeline = timeline.clone();
    let sink: EventSink =
        Arc::new(move |event: &NativeEvent| note_event(CHILD, &sink_timeline, event));
    let spec = NativeSpec::new()
        .bridge(BridgeMode::Invoke)
        .native_events(sink);
    let url = tauri::Url::parse(url).map_err(|error| error.to_string())?;
    let builder =
        tauri::webview::WebviewBuilder::<tauri::Wry>::new(CHILD, WebviewUrl::External(url));
    let (builder, browser) = builder.native(native, CHILD, spec)?;
    let webview = window
        .add_child(
            builder,
            tauri::LogicalPosition::new(0.0, 0.0),
            tauri::LogicalSize::new(500.0, 380.0),
        )
        .map_err(|error| error.to_string())?;
    native.attach(&webview, &browser)?;
    Ok(browser)
}

fn child_close(ctx: &Ctx) -> Result<(), String> {
    let timeline = Arc::new(Timeline::default());
    let (send, receive) = mpsc::sync_channel(1);
    let app = ctx.app.clone();
    let native = ctx.native();
    let build_timeline = timeline.clone();
    let url = app_url("shell", "blank.html");
    record(
        "rust",
        "win.open",
        json!({ "label": CHILD, "url": url, "child": true }),
    );
    ctx.app
        .run_on_main_thread(move || {
            let _ = send.send(build_child(&app, &native, &build_timeline, &url));
        })
        .map_err(|error| error.to_string())?;
    let browser = receive
        .recv_timeout(Duration::from_secs(60))
        .map_err(|_| "building the child webview timed out".to_owned())??;
    let ready = timeline.wait_kind("DocumentReady", 0, Duration::from_secs(30));
    record("rust", "p9.child.ready", json!({ "document_ready": ready }));

    // a registered document navigated to about:blank: does the blank page commit?
    let (send, receive) = mpsc::channel();
    let started = log::now_ms();
    let blank = tauri::Url::parse("about:blank").map_err(|error| error.to_string())?;
    let navigation = browser.navigator().navigate_observed(blank, move |result| {
        let _ = send.send(result.map(|document| document.url().to_string()));
    });
    let dispatch_error = navigation.as_ref().err().cloned();
    let answer = receive.recv_timeout(Duration::from_secs(15));
    record(
        "rust",
        "p9.observed",
        json!({
            "dispatched": navigation.is_ok(),
            "dispatch_error": dispatch_error,
            "answer": answer.as_ref().ok().map(|result| result.as_ref().ok()),
            "answer_error": answer.as_ref().ok().and_then(|result| result.as_ref().err()),
            "timeout": answer.is_err(),
            "ms": log::now_ms() - started,
        }),
    );
    drop(navigation);

    // close: when does the label leave the registry, and when does `Retired` follow
    let started = log::now_ms();
    let requested = browser.close_receipt().request_close();
    let gone_at_return = ctx.app.get_webview(CHILD).is_none();
    let mut removed_after = None;
    for _ in 0..300 {
        if ctx.app.get_webview(CHILD).is_none() {
            removed_after = Some(log::now_ms() - started);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let retired = timeline.wait_kind("Retired", 0, Duration::from_secs(5));
    record(
        "rust",
        "p9.close",
        json!({
            "request_ok": requested.is_ok(),
            "request_error": requested.as_ref().err(),
            "gone_at_return": gone_at_return,
            "removed_after_ms": removed_after,
            "retired_event": retired,
            "ms": log::now_ms() - started,
        }),
    );
    destroy(ctx, HOST);
    std::thread::sleep(Duration::from_millis(500));
    Ok(())
}

fn window_retired(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-p9-win";
    let mut spec = Direct::new(label, &app_url("shell", "blank.html"));
    spec.bridge = BridgeMode::Invoke;
    let opened = open_direct(ctx, spec)?;
    let ready = opened
        .timeline
        .wait_kind("DocumentReady", 0, Duration::from_secs(30));
    let started = log::now_ms();
    destroy(ctx, label);
    let retired = opened
        .timeline
        .wait_kind("Retired", 0, Duration::from_secs(8));
    record(
        "rust",
        "p9.window_retired",
        json!({ "document_ready": ready, "retired": retired, "ms": log::now_ms() - started }),
    );
    Ok(())
}

fn focus(ctx: &Ctx) -> Result<(), String> {
    let labels = ["probe-p9-f1", "probe-p9-f2"];
    let mut windows = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        let mut spec = Direct::new(label, &app_url("shell", "blank.html"));
        spec.position = Some((40.0 + 60.0 * index as f64, 40.0 + 60.0 * index as f64));
        windows.push(open_direct(ctx, spec)?);
    }
    for opened in &windows {
        opened
            .timeline
            .wait_kind("LoadFinished", 0, Duration::from_secs(30));
    }
    for opened in &windows {
        let base = opened.timeline.len();
        let asked = ctx
            .app
            .get_webview_window(&opened.label)
            .ok_or_else(|| format!("no window {}", opened.label))
            .and_then(|window| window.set_focus().map_err(|error| error.to_string()));
        let focused = opened.timeline.wait(Duration::from_secs(4), |recs| {
            recs.iter()
                .skip(base)
                .any(|rec| rec.kind == "View" && rec.v["event"] == "Focus")
        });
        let is_focused = ctx
            .app
            .get_webview_window(&opened.label)
            .and_then(|window| window.is_focused().ok());
        record(
            "rust",
            "p9.focus",
            json!({ "view": opened.label, "set_focus_ok": asked.is_ok(), "set_focus_error": asked.err(), "focus_event": focused, "is_focused": is_focused }),
        );
    }
    for opened in &windows {
        destroy(ctx, &opened.label);
    }
    Ok(())
}

fn real_windows(ctx: &Ctx) -> Result<(), String> {
    let spec = |parent, transparent| WindowSpec {
        title: "probe p9 real".into(),
        size: (480.0, 320.0),
        min_size: None,
        visible: true,
        decorations: true,
        transparent,
        resizable: true,
        parent,
        focus: false,
        main: false,
        view: ViewSpec {
            url: "genoffice-app://shell/probe/blank.html".into(),
            bridge: Bridge::Surface(Surface::Shell),
        },
    };
    let mut labels = Vec::new();
    let first = ctx
        .windows
        .open_window_with_options(spec(None, false), WindowOptions::default());
    record(
        "rust",
        "p9.real",
        json!({ "what": "window", "ok": first.is_ok(), "error": first.as_ref().err() }),
    );
    let Ok((window, view)) = first else {
        return Ok(());
    };
    labels.push(format!("view-{}", view.0));
    std::thread::sleep(Duration::from_millis(1500));
    // a child window above it, transparent (macOS needs the private API the config enables)
    let second = ctx
        .windows
        .open_window_with_options(spec(Some(window), true), WindowOptions::default());
    record(
        "rust",
        "p9.real",
        json!({ "what": "transparent child window", "ok": second.is_ok(), "error": second.as_ref().err() }),
    );
    if let Ok((_, view)) = second {
        labels.push(format!("view-{}", view.0));
    }
    std::thread::sleep(Duration::from_millis(1500));
    for label in labels.iter().rev() {
        destroy(ctx, label);
    }
    std::thread::sleep(Duration::from_millis(500));
    Ok(())
}
