use std::cell::Cell;
use std::rc::Rc;

use tauri::Url;
use tauri::webview::PlatformWebview;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_PROCESS_FAILED_KIND, COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
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
    FocusChangedEventHandler, NavigationCompletedEventHandler, NavigationStartingEventHandler,
    ProcessFailedEventHandler, SourceChangedEventHandler, take_pwstr,
};
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
    result.map_err(|error| format!("The WebView2 {what} handler could not be registered: {error}"))
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
