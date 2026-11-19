use std::cell::Cell;
use std::rc::Rc;

use std::sync::atomic::{AtomicU8, Ordering};
use tauri::Url;
use tauri::webview::PlatformWebview;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_KEY_EVENT_KIND, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN,
    COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP, COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN,
    COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_UP, COREWEBVIEW2_PROCESS_FAILED_KIND,
    COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
    COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED,
    COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
    COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED, COREWEBVIEW2_WEB_ERROR_STATUS,
    COREWEBVIEW2_WEB_ERROR_STATUS_CANNOT_CONNECT,
    COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_COMMON_NAME_IS_INCORRECT,
    COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_EXPIRED,
    COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_IS_INVALID,
    COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_REVOKED,
    COREWEBVIEW2_WEB_ERROR_STATUS_CLIENT_CERTIFICATE_CONTAINS_ERRORS,
    COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_ABORTED,
    COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_RESET, COREWEBVIEW2_WEB_ERROR_STATUS_DISCONNECTED,
    COREWEBVIEW2_WEB_ERROR_STATUS_ERROR_HTTP_INVALID_SERVER_RESPONSE,
    COREWEBVIEW2_WEB_ERROR_STATUS_HOST_NAME_NOT_RESOLVED,
    COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED,
    COREWEBVIEW2_WEB_ERROR_STATUS_REDIRECT_FAILED,
    COREWEBVIEW2_WEB_ERROR_STATUS_SERVER_UNREACHABLE, COREWEBVIEW2_WEB_ERROR_STATUS_TIMEOUT,
    COREWEBVIEW2_WEB_ERROR_STATUS_UNEXPECTED_ERROR,
    COREWEBVIEW2_WEB_ERROR_STATUS_VALID_AUTHENTICATION_CREDENTIALS_REQUIRED,
    COREWEBVIEW2_WEB_ERROR_STATUS_VALID_PROXY_AUTHENTICATION_REQUIRED,
};
use webview2_com::{
    AcceleratorKeyPressedEventHandler, FocusChangedEventHandler, NavigationCompletedEventHandler,
    NavigationStartingEventHandler, ProcessFailedEventHandler, SourceChangedEventHandler,
    take_pwstr,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};
use windows::core::{BOOL, PWSTR};

use super::Platform;
use crate::native::browser::BrowserIdentity;
use crate::native::events::{LoadError, ProcessFailure, ViewEvent};
use crate::native::platform::Observe;

/// Which navigations the load outcome may come from. `Completed` is reported only for the
/// latest navigation that was not cancelled before it started, so the cancellation of a
/// superseded navigation is never taken for the failure of its successor.
#[derive(Default)]
struct Tracked {
    latest: Cell<Option<u64>>,
    cancelled: Cell<Option<u64>>,
}

impl Tracked {
    fn started(&self, id: u64, cancelled: bool) {
        if cancelled {
            self.cancelled.set(Some(id));
        } else {
            self.latest.set(Some(id));
        }
    }

    fn reports(&self, id: u64) -> bool {
        self.cancelled.get() != Some(id) && self.latest.get().is_none_or(|latest| latest == id)
    }
}

fn describe(status: COREWEBVIEW2_WEB_ERROR_STATUS) -> &'static str {
    match status {
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_COMMON_NAME_IS_INCORRECT => {
            "the certificate's name does not match the host"
        }
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_EXPIRED => "the certificate has expired",
        COREWEBVIEW2_WEB_ERROR_STATUS_CLIENT_CERTIFICATE_CONTAINS_ERRORS => {
            "the client certificate contains errors"
        }
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_REVOKED => "the certificate was revoked",
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_IS_INVALID => "the certificate is invalid",
        COREWEBVIEW2_WEB_ERROR_STATUS_SERVER_UNREACHABLE => "the server is unreachable",
        COREWEBVIEW2_WEB_ERROR_STATUS_TIMEOUT => "the request timed out",
        COREWEBVIEW2_WEB_ERROR_STATUS_ERROR_HTTP_INVALID_SERVER_RESPONSE => {
            "the server's response is invalid"
        }
        COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_ABORTED => "the connection was aborted",
        COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_RESET => "the connection was reset",
        COREWEBVIEW2_WEB_ERROR_STATUS_DISCONNECTED => "the network is disconnected",
        COREWEBVIEW2_WEB_ERROR_STATUS_CANNOT_CONNECT => "the connection could not be made",
        COREWEBVIEW2_WEB_ERROR_STATUS_HOST_NAME_NOT_RESOLVED => "the host name was not resolved",
        COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED => "the navigation was cancelled",
        COREWEBVIEW2_WEB_ERROR_STATUS_REDIRECT_FAILED => "a redirect failed",
        COREWEBVIEW2_WEB_ERROR_STATUS_UNEXPECTED_ERROR => "an unexpected error occurred",
        COREWEBVIEW2_WEB_ERROR_STATUS_VALID_AUTHENTICATION_CREDENTIALS_REQUIRED => {
            "authentication credentials are required"
        }
        COREWEBVIEW2_WEB_ERROR_STATUS_VALID_PROXY_AUTHENTICATION_REQUIRED => {
            "proxy authentication credentials are required"
        }
        _ => "the navigation failed",
    }
}

/// The failures that end the web content of the browser. A helper process (GPU, utility,
/// plugin) or an out-of-process subframe going away leaves the document alone.
fn failure(kind: COREWEBVIEW2_PROCESS_FAILED_KIND) -> Option<ProcessFailure> {
    match kind {
        COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => {
            Some(ProcessFailure::BrowserProcessExited)
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED => {
            Some(ProcessFailure::RenderProcessExited)
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE => {
            Some(ProcessFailure::Unresponsive)
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED => Some(ProcessFailure::Unknown),
        _ => None,
    }
}

fn registered(what: &str, result: windows::core::Result<()>) -> Result<(), String> {
    crate::trace::trace!("attach", "windows: {what} handler registered");
    result.map_err(|error| format!("The WebView2 {what} handler could not be registered: {error}"))
}

/// The modifiers held, as the key events of this handler have shown them. The handler runs
/// after the key has been posted, so the keyboard's own state is no guide: a runner sends
/// Ctrl, S and both releases at once, and Ctrl is long up when S arrives here.
static MODIFIERS: AtomicU8 = AtomicU8::new(0);
const CTRL: u8 = 1;
const SHIFT: u8 = 2;
const ALT: u8 = 4;

/// The bit of a modifier key's virtual-key code; the generic and the left/right codes
fn modifier_bit(key: u32) -> Option<u8> {
    match key {
        0x11 | 0xA2 | 0xA3 => Some(CTRL),
        0x10 | 0xA0 | 0xA1 => Some(SHIFT),
        0x12 | 0xA4 | 0xA5 => Some(ALT),
        _ => None,
    }
}

/// The modifiers held now. Shift alone raises no accelerator event, so it is never seen going
/// down here: its own state is read, which holds for a hand on the keyboard (the key is still
/// down when the letter is handled).
fn held() -> u8 {
    let held = MODIFIERS.load(Ordering::Relaxed);
    // SAFETY: a plain read of the keyboard's physical state.
    if unsafe { GetAsyncKeyState(i32::from(VK_SHIFT.0)) } < 0 {
        held | SHIFT
    } else {
        held
    }
}

/// The menu accelerator of the Save shortcut being pressed: Ctrl+S or Ctrl+Shift+S
fn file_shortcut() -> Option<&'static str> {
    let held = held();
    if held & CTRL == 0 || held & ALT != 0 {
        return None;
    }
    Some(if held & SHIFT != 0 {
        "CmdOrCtrl+Shift+S"
    } else {
        "CmdOrCtrl+S"
    })
}

impl Observe for Platform {
    /// wry's `NavigationStarting` handler runs the shell's `on_navigation` hook, which sees
    /// main-frame navigations only (subframes raise `FrameNavigationStarting`).
    const NAVIGATION_HOOK_IS_MAIN_FRAME: bool = true;
    /// wry raises `Finished` from `NavigationCompleted` without reading `IsSuccess`.
    const CONFIRMS_LOAD_OUTCOME: bool = true;

    /// The document commit is wry's `ContentLoading`, reported by `tauri_public` as `Started`;
    /// registering a second `ContentLoading` handler here would commit every document twice.
    fn attach(platform: &PlatformWebview, browser: BrowserIdentity) -> Result<(), String> {
        let controller = platform.controller();
        // SAFETY: called on the UI thread, inside `Webview::with_webview`; every COM call below
        // is made on that thread, and the handlers it registers run there too.
        crate::trace::trace!("attach", "windows: reading the WebView2 core");
        let core = unsafe { controller.CoreWebView2() }
            .map_err(|error| format!("The WebView2 core is unavailable: {error}"))?;
        let mut token = 0i64;
        let tracked = Rc::new(Tracked::default());

        let started = tracked.clone();
        let handler = NavigationStartingEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else {
                return Ok(());
            };
            let (mut id, mut cancelled) = (0u64, BOOL::default());
            // SAFETY: `args` is live for this callback.
            unsafe {
                args.NavigationId(&mut id)?;
                // wry's handler ran first and cancels what the shell policy refused.
                args.Cancel(&mut cancelled)?;
            }
            started.started(id, cancelled.as_bool());
            Ok(())
        }));
        // SAFETY: as above; `token` is written once per call.
        registered("NavigationStarting", unsafe {
            core.add_NavigationStarting(&handler, &mut token)
        })?;

        let identity = browser.clone();
        let completed = tracked;
        let handler = NavigationCompletedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else {
                return Ok(());
            };
            let mut id = 0u64;
            // SAFETY: `args` is live for this callback.
            unsafe { args.NavigationId(&mut id)? };
            if !completed.reports(id) {
                return Ok(());
            }
            let mut success = BOOL::default();
            let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
            // SAFETY: as above.
            unsafe {
                args.IsSuccess(&mut success)?;
                args.WebErrorStatus(&mut status)?;
            }
            if success.as_bool() {
                identity.observe_load(Ok(()));
            } else {
                identity.observe_load(Err(LoadError {
                    code: Some(status.0),
                    description: describe(status).to_owned(),
                }));
            }
            Ok(())
        }));
        // SAFETY: as above.
        registered("NavigationCompleted", unsafe {
            core.add_NavigationCompleted(&handler, &mut token)
        })?;

        let identity = browser.clone();
        let handler = SourceChangedEventHandler::create(Box::new(move |sender, args| {
            let (Some(sender), Some(args)) = (sender, args) else {
                return Ok(());
            };
            let mut new_document = BOOL::default();
            // SAFETY: both objects are live for this callback.
            unsafe { args.IsNewDocument(&mut new_document)? };
            // A new document is committed through `ContentLoading`; only history-API address
            // changes are reported here.
            if new_document.as_bool() {
                return Ok(());
            }
            let mut source = PWSTR::null();
            // SAFETY: as above; the string is taken once.
            unsafe { sender.Source(&mut source)? };
            if let Ok(url) = Url::parse(&take_pwstr(source)) {
                identity.observe_address(&url);
            }
            Ok(())
        }));
        // SAFETY: as above.
        registered("SourceChanged", unsafe {
            core.add_SourceChanged(&handler, &mut token)
        })?;

        let identity = browser.clone();
        let handler = ProcessFailedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else {
                return Ok(());
            };
            let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
            // SAFETY: `args` is live for this callback.
            unsafe { args.ProcessFailedKind(&mut kind)? };
            if let Some(failure) = failure(kind) {
                identity.observe_process_failed(failure);
            }
            Ok(())
        }));
        // SAFETY: as above.
        registered("ProcessFailed", unsafe {
            core.add_ProcessFailed(&handler, &mut token)
        })?;

        let identity = browser.clone();
        let handler = FocusChangedEventHandler::create(Box::new(move |_, _| {
            identity.observe_view(ViewEvent::Focus);
            Ok(())
        }));
        // SAFETY: as above.
        registered("GotFocus", unsafe {
            controller.add_GotFocus(&handler, &mut token)
        })?;

        let identity = browser.clone();
        let handler = FocusChangedEventHandler::create(Box::new(move |_, _| {
            identity.observe_view(ViewEvent::Blur);
            Ok(())
        }));
        // SAFETY: as above.
        registered("LostFocus", unsafe {
            controller.add_LostFocus(&handler, &mut token)
        })?;

        // WebView2 passes no unhandled key on to the window, so the menu's accelerators are dead
        // while a page has the focus. The ones no page handles itself are taken here and run as
        // the menu item they belong to: the Save shortcuts of the pages that have none, and
        // Close (Ctrl+W), which closes a tab only through the menu.
        let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else {
                return Ok(());
            };
            let (mut kind, mut key) = (COREWEBVIEW2_KEY_EVENT_KIND::default(), 0u32);
            // SAFETY: `args` is live for this callback.
            unsafe {
                args.KeyEventKind(&mut kind)?;
                args.VirtualKey(&mut key)?;
            }
            let down = kind == COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
                || kind == COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN;
            let up = kind == COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP
                || kind == COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_UP;
            if let Some(bit) = modifier_bit(key) {
                if down {
                    MODIFIERS.fetch_or(bit, Ordering::Relaxed);
                } else if up {
                    MODIFIERS.fetch_and(!bit, Ordering::Relaxed);
                }
                return Ok(());
            }
            if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN {
                return Ok(());
            }
            let handled = if key == u32::from(b'S') {
                let shortcut = file_shortcut();
                let handled = shortcut.is_some_and(crate::host::menu::run_file_accelerator);
                crate::trace::trace!(
                    "accel",
                    "S pressed: shortcut {shortcut:?}, handled {handled}"
                );
                handled
            } else if key == u32::from(b'W') {
                let close = held() == CTRL;
                let handled = close && crate::host::menu::run_close_accelerator();
                crate::trace::trace!(
                    "accel",
                    "W pressed: Ctrl alone {close}, handled {handled}"
                );
                handled
            } else {
                return Ok(());
            };
            if handled {
                // SAFETY: as above.
                unsafe { args.SetHandled(true)? };
            }
            Ok(())
        }));
        // SAFETY: as above.
        registered("AcceleratorKeyPressed", unsafe {
            controller.add_AcceleratorKeyPressed(&handler, &mut token)
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED;

    use super::*;

    #[test]
    fn only_the_latest_uncancelled_navigation_reports_its_outcome() {
        let tracked = Tracked::default();
        assert!(
            tracked.reports(1),
            "an outcome before any start is reported"
        );
        tracked.started(1, false);
        tracked.started(2, false);
        assert!(
            !tracked.reports(1),
            "the superseded navigation's cancellation is ignored"
        );
        assert!(tracked.reports(2));
        tracked.started(3, true);
        assert!(
            !tracked.reports(3),
            "a navigation the policy cancelled has no outcome"
        );
        assert!(tracked.reports(2));
    }

    #[test]
    fn only_web_content_failures_are_reported() {
        assert_eq!(
            failure(COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED),
            Some(ProcessFailure::RenderProcessExited)
        );
        assert_eq!(
            failure(COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED),
            Some(ProcessFailure::BrowserProcessExited)
        );
        assert_eq!(
            failure(COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED),
            None
        );
    }
}
