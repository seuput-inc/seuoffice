//! Everything that differs between WKWebView (macOS) and WebView2 (Windows).
//!
//! `tauri_public` is the baseline built on Tauri's public hooks only; the OS adapters add what
//! those hooks cannot say. The seam is a set of small traits implemented by one marker type
//! per OS ([`Current`]). Each trait is implemented in exactly one adapter file, so the slices
//! that own those files never edit the same file:
//!
//! | trait | macOS file (S1m) | Windows file (S1w) |
//! |---|---|---|
//! | [`Observe`] | `observer.rs` | `observer.rs` |
//! | [`PageDriver`] | `page.rs` (helpers in `edit.rs`) | `page.rs` (helpers in `cdp.rs`) |
//! | [`PrintOps`] | `print.rs` | `page.rs` |
//! | [`WindowOps`] | `window.rs` | `window.rs` |
//! | [`PolicyOps`] | `policy.rs` | `policy.rs` |
//! | [`SpellOps`] | `spell.rs` | `spell.rs` |
//!
//! Windows also exposes `windows::caption::{CaptionStyle, configure, update}`. A missing or
//! partial impl fails the compile in [`assert_adapter`]; there is no runtime fallback and no
//! `todo!()`.
//!
//! Every call that touches a webview is made on the UI thread unless a doc says otherwise.

use std::cell::Cell;

use serde_json::Value;
use tauri::webview::PlatformWebview;
use tauri::{AppHandle, Webview, Window};

use super::browser::BrowserIdentity;
use super::page::{CaptureSpec, Done, EditCommand, KeyChord, PdfSpec, QuietSpec, Viewport};
use super::policy::RequestPolicy;

pub mod tauri_public;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

#[cfg(not(any(target_os = "macos", windows)))]
compile_error!("the SeuOffice shell supports macOS (WKWebView) and Windows (WebView2) only");

#[cfg(target_os = "macos")]
pub(crate) use self::macos::Platform as Current;
#[cfg(windows)]
pub(crate) use self::windows::Platform as Current;

/// Observers that report what Tauri's public hooks cannot, into
/// [`BrowserIdentity`]'s `observe_*` methods.
///
/// The adapter must never infer a navigation failure from "not loading": it reports
/// `observe_load(Err(..))` only from an OS failure signal (WK `didFailProvisionalNavigation` /
/// `didFailNavigation`, WebView2 `NavigationCompleted` with `IsSuccess == false`).
pub(crate) trait Observe {
    /// True when `WebviewBuilder::on_navigation` fires for main-frame navigations only
    /// (WebView2). On WKWebView it fires for every frame, so the adapter reports main-frame
    /// admission itself, from its chained navigation delegate, with
    /// `BrowserIdentity::observe_navigation_allowed` after the shell policy said yes and
    /// `observe_navigation_refused` after it said no.
    const NAVIGATION_HOOK_IS_MAIN_FRAME: bool;

    /// True when the adapter reports the outcome of every load itself (`observe_load` for
    /// success and failure). `tauri_public` then ignores the public `Finished` event. Required
    /// on Windows: wry emits `Finished` from `NavigationCompleted` without checking success.
    const CONFIRMS_LOAD_OUTCOME: bool;

    /// Attach the OS observers: navigation outcome, web content process failure, focus and
    /// blur of the view (`observe_view`), history-API address changes (`observe_address`).
    /// Runs inside `Webview::with_webview`, on the UI thread, once per browser, before the
    /// browser reports ready. An `Err` makes `Navigator::when_ready` fail.
    ///
    /// macOS chains wry's own `WKNavigationDelegate`: it forwards every selector and adds
    /// `didCommitNavigation`-independent failure and termination callbacks. Windows registers
    /// `NavigationCompleted`, `ProcessFailed`, `SourceChanged` and the controller focus events.
    fn attach(platform: &PlatformWebview, browser: BrowserIdentity) -> Result<(), String>;
}

/// The typed replacement of the CDP surface. Every operation completes `done` exactly once, on
/// any thread. [`PageOps`](super::page::PageOps) validates specs and applies the fence before
/// calling these.
pub(crate) trait PageDriver {
    /// Evaluate `script` in the page's main frame. The script is ONE EXPRESSION (several
    /// statements go in an IIFE), embedded as it is and never passed through `eval` or
    /// `Function`: the shell's pages carry a CSP without `unsafe-eval`. The answer is the JSON
    /// value of the expression; with `await_promise` a promise is awaited first.
    /// macOS: `callAsyncJavaScript` with `return await (<script>)` in the page content world.
    /// Windows: CDP `Runtime.evaluate { expression, awaitPromise, returnByValue }`.
    fn evaluate(webview: &Webview, script: String, await_promise: bool, done: Done<Value>);

    /// Run `source` as a classic script of the main frame, for its effect: statements,
    /// declarations and a trailing `;` are fine, and no value comes back. Like `evaluate`, it
    /// never goes through `eval`. macOS: `evaluateJavaScript` (a script the page's CSP does not
    /// govern). Windows: CDP `Runtime.evaluate` without `returnByValue`.
    fn run_script(webview: &Webview, source: String, done: Done<()>);

    /// PNG bytes. macOS: `takeSnapshot` (clip, full page by resizing an offscreen window to
    /// `scrollHeight`, transparent background). Windows: CDP `Page.captureScreenshot`.
    fn capture_png(webview: &Webview, spec: CaptureSpec, done: Done<Vec<u8>>);

    /// PDF bytes. Windows: CDP `Page.printToPDF` with the spec's fields. macOS: a file print
    /// operation of the page; the `header_footer` templates are rendered by a hidden web view
    /// and stamped onto the pages, then `page_ranges` is applied to the result.
    fn print_pdf(webview: &Webview, spec: PdfSpec, done: Done<Vec<u8>>);

    /// macOS: send the matching selector to the web view after making it first responder.
    /// Windows: CDP `Input.dispatchKeyEvent` with Chromium editing `commands`.
    fn edit(webview: &Webview, command: EditCommand, done: Done<()>);

    /// A trusted key event: macOS `NSEvent` through the responder chain, Windows CDP
    /// `Input.dispatchKeyEvent`.
    fn key(webview: &Webview, chord: KeyChord, done: Done<()>);

    /// Layout size of a hidden browser. macOS: an offscreen window of that CSS size plus a
    /// snapshot scale. Windows: CDP `Emulation.setDeviceMetricsOverride`.
    fn set_viewport(webview: &Webview, viewport: Viewport, done: Done<()>);

    /// A script that runs at the start of every document loaded next. macOS: a
    /// `WKUserScript` at document start. Windows: CDP `Page.addScriptToEvaluateOnNewDocument`.
    fn add_init_script(webview: &Webview, source: String, done: Done<()>);

    /// Loaded and no resource in flight for `spec.quiet_ms`, within `spec.timeout_ms`.
    /// macOS: an in-page observer script. Windows: CDP `Network.*` and `Page.loadEventFired`.
    fn wait_quiet(webview: &Webview, spec: QuietSpec, done: Done<()>);

    /// Reload bypassing the cache (`reload()` keeps it). macOS `reloadFromOrigin`, Windows CDP
    /// `Page.reload { ignoreCache: true }`. The webview's own `reload()` is `Webview::reload`.
    fn reload_ignoring_cache(webview: &Webview) -> Result<(), String>;
}

/// How a print dialog ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintOutcome {
    #[cfg(target_os = "macos")]
    Printed,
    Cancelled,
    /// The platform cannot say (WebView2 `ShowPrintUI`); the caller answers success as the
    /// CEF build did.
    #[cfg(windows)]
    Unreported,
}

pub(crate) trait PrintOps {
    /// The system print dialog for the page. macOS: an `NSPrintOperation` with a panel,
    /// reporting printed or cancelled. Windows: `ShowPrintUI(SYSTEM)`, `Unreported`.
    fn print_dialog(webview: &Webview, done: Done<PrintOutcome>);
}

pub(crate) trait WindowOps {
    /// Show without activating: macOS `orderFrontRegardless`, Windows `ShowWindow(SW_SHOWNOACTIVATE)`.
    fn show_inactive(window: &Window) -> Result<(), String>;

    /// Make `child` modal to `owner`: macOS `beginSheet:`, Windows `EnableWindow(owner, false)`
    /// remembering the owner's prior enabled state.
    fn begin_modal(child: &Window, owner: &Window) -> Result<(), String>;

    /// Undo `begin_modal`; Windows restores the owner's enabled state when the last modal
    /// child ends.
    fn end_modal(child: &Window) -> Result<(), String>;

    /// Label of the application window the OS considers key (macOS `NSApp.keyWindow`
    /// cross-checked against the app's windows). Windows: the foreground window of this
    /// process, or `None`.
    fn key_window_label(app: &AppHandle) -> Result<Option<String>, String>;
}

pub(crate) trait PolicyOps {
    /// Enforce `policy` on the webview behind `platform`, after creation, on the UI thread,
    /// from the same `with_webview` job that attached the observers.
    /// Windows: a `WebResourceRequested` filter answering allow or block with the exact
    /// decision, and `SetIsScriptEnabled(policy.javascript())`.
    /// macOS (no per-request interception for http(s)): a `WKContentRuleList` compiled from
    /// `policy.blocks()`, plus `allowsContentJavaScript = policy.javascript()` through the
    /// chained navigation delegate.
    ///
    /// Contract: call `BrowserIdentity::observe_policy_installed` exactly once, as soon as
    /// the policy is enforced (Windows: before returning; macOS: when the rules compiled,
    /// before releasing the held navigations). Until then the browser cancels and remembers
    /// its first navigation to a real URL and replays it afterwards, so no initial request
    /// escapes the policy, and `Navigator::when_ready` waits.
    fn apply_policy(
        platform: &PlatformWebview,
        browser: &BrowserIdentity,
        policy: &RequestPolicy,
    ) -> Result<(), String>;
}

pub(crate) trait SpellOps {
    /// Replacement suggestions for a misspelled word; empty when there are none.
    fn suggestions(word: &str) -> Result<Vec<String>, String>;
    fn is_misspelled(word: &str) -> Result<bool, String>;
    /// Add to the user dictionary.
    fn learn(word: &str) -> Result<bool, String>;
    /// Remove from the user dictionary.
    fn unlearn(word: &str) -> Result<bool, String>;
}

/// The whole seam, checked once so a missing impl names itself at compile time.
pub(crate) trait Adapter:
    Observe + PageDriver + PrintOps + WindowOps + PolicyOps + SpellOps
{
}

impl<T: Observe + PageDriver + PrintOps + WindowOps + PolicyOps + SpellOps> Adapter for T {}

const _: fn() = || {
    fn assert_adapter<T: Adapter>() {}
    assert_adapter::<Current>();
};

/// Attach the OS observers, then enforce the browser's request policy.
pub(crate) fn attach(platform: &PlatformWebview, browser: &BrowserIdentity) -> Result<(), String> {
    Current::attach(platform, browser.clone())?;
    match browser.policy() {
        Some(policy) => Current::apply_policy(platform, browser, policy),
        None => Ok(()),
    }
}

thread_local! {
    static NAVIGATION_FRAME: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Adapters that learn the frame of a navigation before Tauri's URL-only `on_navigation`
/// hook runs (macOS: the chained delegate calls wry's synchronously) wrap that call in this,
/// so the hook can tell the main frame from a subframe.
#[cfg(target_os = "macos")]
pub(crate) fn with_navigation_frame<T>(main_frame: bool, call: impl FnOnce() -> T) -> T {
    let previous = NAVIGATION_FRAME.with(|frame| frame.replace(Some(main_frame)));
    let result = call();
    NAVIGATION_FRAME.with(|frame| frame.set(previous));
    result
}

/// Whether the navigation the hook is judging is the main frame's. Without an adapter's word
/// it is what the platform's hook reports: main-frame only on Windows, unknown on macOS (a
/// subframe is then not mistaken for the main frame).
pub(crate) fn navigation_is_main_frame() -> bool {
    NAVIGATION_FRAME
        .with(Cell::get)
        .unwrap_or(Current::NAVIGATION_HOOK_IS_MAIN_FRAME)
}
