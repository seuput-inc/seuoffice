//! P5 (macOS: the hidden HTML engine's page operations) and P6 (Windows: the CDP operations
//! behind the same calls). One suite, run on the platform it names, through `PageOps`:
//!
//! - `evaluate` with and without a promise, a rejection, a large result, a statement list;
//!   `run_script`; evaluation in a window whose page scripts are off and on a page with the
//!   shell's CSP;
//! - `capture_png`: viewport, full page, clip, scale, transparent and opaque backgrounds,
//!   from a shown and from a hidden window, with a page whose pixels tell a flipped clip from
//!   a straight one;
//! - `set_viewport`, `add_init_script`;
//! - `wait_quiet` against requests the probe makes slow (the `html-asset` stand-in logs when it
//!   answers);
//! - `edit` and `key` on a textarea and a contenteditable element.
//!
//! The probe records what each call returned and how long it took. Sources: design P5/P6,
//! S1m, S1w and F0 inferences (statement lists, `callAsyncJavaScript` and scripts off, snapshot
//! size and orientation, hidden windows, `wait_quiet` and in-flight requests, editing needs
//! focus, trusted keys).

use std::time::Duration;

use serde_json::{Value, json};

use super::harness::{Ctx, Direct, Opened, app_url, destroy, eval, open_direct, page_of, platform, record_op, wait_done, wait_load};
use super::log::{self, record};
use crate::native::{
    CaptureSpec, EditCommand, KeyChord, Modifiers, PageOps, QuietSpec, Rect, RequestPolicy, Viewport,
};

pub fn run(ctx: &Ctx, probe: &str) -> Result<(), String> {
    let wanted = if probe == "p5" { "macos" } else { "windows" };
    if platform() != wanted {
        record("rust", "probe.skip", json!({ "probe": probe, "platform": platform(), "wanted": wanted }));
        return Ok(());
    }
    let mut problems = Vec::new();
    for (name, suite) in [
        ("evaluate", evaluate_suite as fn(&Ctx) -> Result<(), String>),
        ("hidden", hidden_suite),
        ("viewport", viewport_suite),
        ("scripts-off", scripts_off_suite),
        ("csp", csp_suite),
        ("quiet", quiet_suite),
        ("input", input_suite),
        ("init-script", init_script_suite),
    ] {
        record("rust", "po.suite", json!({ "suite": name }));
        if let Err(error) = suite(ctx) {
            record("rust", "po.suite.failed", json!({ "suite": name, "error": error }));
            problems.push(format!("{name}: {error}"));
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(problems.join("; ")) }
}

/// Open a fixture page and wait for it to load (and report, when its scripts run).
fn open_page(ctx: &Ctx, label: &str, page: &str, visible: bool, policy: Option<RequestPolicy>) -> Result<Opened, String> {
    let scripts = policy.as_ref().is_none_or(RequestPolicy::javascript);
    let from = log::pages().len();
    let mut spec = Direct::new(label, &app_url("shell", page));
    spec.visible = visible;
    spec.size = (640.0, 480.0);
    spec.policy = policy;
    let opened = open_direct(ctx, spec)?;
    if !wait_load(&opened, 0, 40) {
        return Err(format!("{page} did not finish loading"));
    }
    if scripts && !wait_done(label, from, 20) {
        record("rust", "po.page.silent", json!({ "view": label, "page": page }));
    }
    std::thread::sleep(Duration::from_millis(400));
    Ok(opened)
}

fn evaluate(ctx: &Ctx, label: &str, name: &str, js: &str) {
    let started = log::now_ms();
    let result = page_of(ctx, label).and_then(|page| eval(&page, js, 30).map_err(|error| error.to_string()));
    record_op(name, label, started, result);
}

fn capture(ctx: &Ctx, label: &str, name: &str, spec: CaptureSpec) {
    let started = log::now_ms();
    let result = page_of(ctx, label).and_then(|page| {
        page.blocking(Duration::from_secs(60), |done| page.capture_png(spec, None, done))
            .map_err(|error| error.to_string())
    });
    match result {
        Ok(bytes) => {
            let file = format!("po-{name}.png");
            let written = std::fs::write(ctx.artifact(&file), &bytes).err().map(|error| error.to_string());
            record_op(&format!("capture.{name}"), label, started, Ok(json!({ "file": file, "bytes": bytes.len(), "write_error": written })));
        }
        Err(error) => record_op(&format!("capture.{name}"), label, started, Err(error)),
    }
}

fn unit<T>(result: Result<T, crate::native::PageError>) -> Result<Value, String> {
    result.map(|_| Value::Null).map_err(|error| error.to_string())
}

fn with_page(ctx: &Ctx, label: &str, name: &str, call: impl FnOnce(&PageOps) -> Result<Value, String>) {
    let started = log::now_ms();
    let result = page_of(ctx, label).and_then(|page| call(&page));
    record_op(name, label, started, result);
}

const BOX: Rect = Rect { x: 50.0, y: 60.0, width: 200.0, height: 100.0 };
const CORNER: Rect = Rect { x: 0.0, y: 0.0, width: 40.0, height: 40.0 };

fn evaluate_suite(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-po-main";
    open_page(ctx, label, "po-engine.html", true, None)?;
    evaluate(ctx, label, "eval.sync", "1 + 1");
    evaluate(ctx, label, "eval.promise", "new Promise((resolve) => setTimeout(() => resolve({ a: 1 }), 300))");
    evaluate(ctx, label, "eval.reject", "Promise.reject(new Error('boom'))");
    evaluate(ctx, label, "eval.big", "'x'.repeat(1048576)");
    evaluate(ctx, label, "eval.undefined", "undefined");
    evaluate(ctx, label, "eval.statement-list", "var q = 1; q");
    evaluate(ctx, label, "eval.title", "document.title");
    evaluate(ctx, label, "eval.metrics", "[innerWidth, innerHeight, devicePixelRatio, document.documentElement.scrollHeight]");
    with_page(ctx, label, "run_script.set", |page| {
        unit(page.blocking(Duration::from_secs(30), |done| page.run_script("window.__rs = 7;", None, done)))
    });
    evaluate(ctx, label, "run_script.read", "window.__rs");

    capture(ctx, label, "viewport", CaptureSpec::default());
    capture(ctx, label, "fullpage", CaptureSpec { full_page: true, ..CaptureSpec::default() });
    capture(ctx, label, "clip-box", CaptureSpec { clip: Some(BOX), ..CaptureSpec::default() });
    capture(ctx, label, "clip-box-2x", CaptureSpec { clip: Some(BOX), scale: 2.0, ..CaptureSpec::default() });
    capture(ctx, label, "corner-transparent", CaptureSpec { clip: Some(CORNER), transparent: true, ..CaptureSpec::default() });
    capture(ctx, label, "corner-opaque", CaptureSpec { clip: Some(CORNER), ..CaptureSpec::default() });
    destroy(ctx, label);
    Ok(())
}

fn hidden_suite(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-po-hidden";
    open_page(ctx, label, "po-engine.html", false, None)?;
    evaluate(ctx, label, "hidden.metrics", "[innerWidth, innerHeight, devicePixelRatio, document.hidden]");
    capture(ctx, label, "hidden-clip-box", CaptureSpec { clip: Some(BOX), ..CaptureSpec::default() });
    capture(ctx, label, "hidden-fullpage", CaptureSpec { full_page: true, ..CaptureSpec::default() });
    destroy(ctx, label);
    Ok(())
}

fn viewport_suite(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-po-viewport";
    open_page(ctx, label, "po-engine.html", false, None)?;
    with_page(ctx, label, "viewport.set", |page| {
        unit(page.blocking(Duration::from_secs(30), |done| {
            page.set_viewport(Viewport { width: 800, height: 600, device_scale_factor: 2.0 }, done)
        }))
    });
    std::thread::sleep(Duration::from_millis(500));
    evaluate(ctx, label, "viewport.metrics", "[innerWidth, innerHeight, devicePixelRatio]");
    capture(ctx, label, "viewport-clip", CaptureSpec { clip: Some(BOX), ..CaptureSpec::default() });
    destroy(ctx, label);
    Ok(())
}

fn scripts_off_suite(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-po-nojs";
    let policy = RequestPolicy::allow_all().with_javascript(false);
    open_page(ctx, label, "po-engine.html", false, Some(policy))?;
    // the page's own script sets this flag; it must not have run
    evaluate(ctx, label, "nojs.flag", "typeof window.__probeLoaded");
    evaluate(ctx, label, "nojs.sync", "1 + 1");
    destroy(ctx, label);
    Ok(())
}

fn csp_suite(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-po-csp";
    open_page(ctx, label, "po-csp.html", false, None)?;
    with_page(ctx, label, "csp.run_script", |page| {
        unit(page.blocking(Duration::from_secs(30), |done| page.run_script("window.__csp = 1;", None, done)))
    });
    evaluate(ctx, label, "csp.read", "window.__csp");
    evaluate(ctx, label, "csp.sync", "1 + 1");
    destroy(ctx, label);
    Ok(())
}

fn wait_quiet(ctx: &Ctx, label: &str, name: &str, quiet_ms: u32) {
    with_page(ctx, label, name, |page| {
        unit(page.blocking(Duration::from_secs(20), |done| {
            page.wait_quiet(QuietSpec { quiet_ms, timeout_ms: 8000 }, None, done)
        }))
    });
}

fn quiet_suite(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-po-quiet";
    open_page(ctx, label, "po-quiet.html", false, None)?;
    wait_quiet(ctx, label, "quiet.idle", 300);
    // a request already in flight when the call is made
    evaluate(ctx, label, "quiet.pending.start", "(() => { window.slow(1500, 'b'); return true })()");
    wait_quiet(ctx, label, "quiet.pending", 300);
    std::thread::sleep(Duration::from_millis(500));
    // a request that starts after the call, inside the quiet window
    evaluate(ctx, label, "quiet.after.start", "(() => { setTimeout(() => window.slow(800, 'c'), 150); return true })()");
    wait_quiet(ctx, label, "quiet.after", 300);
    std::thread::sleep(Duration::from_millis(500));
    destroy(ctx, label);
    Ok(())
}

fn edit(ctx: &Ctx, label: &str, name: &str, command: EditCommand) {
    with_page(ctx, label, name, |page| unit(page.blocking(Duration::from_secs(20), |done| page.edit(command, done))));
}

fn key(ctx: &Ctx, label: &str, name: &str, key: &str, code: &str) {
    let chord = KeyChord { key: key.to_owned(), code: code.to_owned(), modifiers: Modifiers::default() };
    with_page(ctx, label, name, |page| unit(page.blocking(Duration::from_secs(20), |done| page.key(chord, done))));
}

fn input_suite(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-po-input";
    open_page(ctx, label, "po-input.html", true, None)?;
    let focus = "(() => { const t = document.getElementById('t'); t.focus(); t.setSelectionRange(0, 0); return document.activeElement.id })()";
    let value = "document.getElementById('t').value";
    evaluate(ctx, label, "input.focus", focus);
    edit(ctx, label, "input.edit.select-all", EditCommand::SelectAll);
    evaluate(ctx, label, "input.selection", "(() => { const t = document.getElementById('t'); return [t.selectionStart, t.selectionEnd, t.value.length] })()");
    edit(ctx, label, "input.edit.copy", EditCommand::Copy);
    evaluate(ctx, label, "input.caret-end", "(() => { const t = document.getElementById('t'); t.setSelectionRange(t.value.length, t.value.length); return t.value.length })()");
    edit(ctx, label, "input.edit.paste", EditCommand::Paste);
    evaluate(ctx, label, "input.value.after-paste", value);
    edit(ctx, label, "input.edit.select-all-2", EditCommand::SelectAll);
    edit(ctx, label, "input.edit.cut", EditCommand::Cut);
    evaluate(ctx, label, "input.value.after-cut", value);
    edit(ctx, label, "input.edit.undo", EditCommand::Undo);
    evaluate(ctx, label, "input.value.after-undo", value);

    evaluate(ctx, label, "input.focus-2", focus);
    key(ctx, label, "input.key.a", "a", "KeyA");
    evaluate(ctx, label, "input.key.a.read", "[document.getElementById('t').value, window.__keys.slice(-1)]");
    evaluate(
        ctx,
        label,
        "input.focus-ce",
        "(() => { const e = document.getElementById('ce'); e.focus(); getSelection().selectAllChildren(e); getSelection().collapseToEnd(); return document.activeElement.id })()",
    );
    key(ctx, label, "input.key.space", " ", "Space");
    evaluate(
        ctx,
        label,
        "input.key.space.read",
        "[document.getElementById('ce').textContent, navigator.userActivation ? navigator.userActivation.isActive : null, window.__keys.slice(-1)]",
    );
    destroy(ctx, label);
    Ok(())
}

fn init_script_suite(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-po-init";
    open_page(ctx, label, "blank.html", false, None)?;
    with_page(ctx, label, "init.add", |page| {
        unit(page.blocking(Duration::from_secs(30), |done| page.add_init_script("window.__initMarker = 42;", done)))
    });
    let from = log::pages().len();
    let url = tauri::Url::parse(&app_url("shell", "po-target.html")).map_err(|error| error.to_string())?;
    let started = log::now_ms();
    let navigated = ctx
        .app
        .get_webview(label)
        .ok_or_else(|| "no webview".to_owned())
        .and_then(|webview| webview.navigate(url).map_err(|error| error.to_string()));
    record_op("init.navigate", label, started, navigated.map(|()| Value::Null));
    wait_done(label, from, 20);
    destroy(ctx, label);
    Ok(())
}
