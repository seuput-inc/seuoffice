use tauri::Url;
use tauri::webview::PlatformWebview;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT, COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
    ICoreWebView2_22,
};
use webview2_com::{WebResourceRequestedEventHandler, take_pwstr};
use windows::core::{HSTRING, Interface, PWSTR};

use super::Platform;
use crate::native::browser::BrowserIdentity;
use crate::native::platform::PolicyOps;
use crate::native::platform::tauri_public::bound_scheme;
use crate::native::policy::{PolicyRequest, RequestPolicy};

const BLOCKED_STATUS: i32 = 403;

impl PolicyOps for Platform {
    /// A request the policy refuses is answered with a 403, so the page sees a failed fetch
    /// rather than a hang; an allowed request is left alone. A refused request that the
    /// shell's own scheme handler (wry) answers asynchronously could be overwritten by that
    /// later answer; the allow lists never refuse the shell's own scheme, and the `on_navigation`
    /// hook refuses main-frame navigations before they start.
    ///
    /// WebView2 cannot say which frame a document request belongs to, so these requests are
    /// `top_level` (the document class) but never `main_frame`: the navigation hook judged the
    /// main frame's navigation once, as it started.
    fn apply_policy(
        platform: &PlatformWebview,
        browser: &BrowserIdentity,
        policy: &RequestPolicy,
    ) -> Result<(), String> {
        let failed =
            |what: &'static str| move |error: windows::core::Error| format!("{what}: {error}");
        // SAFETY: UI thread, inside the `with_webview` job that attached the observers.
        let core = unsafe { platform.controller().CoreWebView2() }
            .map_err(failed("The WebView2 core is unavailable"))?;
        let environment = platform.environment();
        let request_policy = policy.clone();
        let handler = WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else {
                return Ok(());
            };
            // SAFETY: UI thread; `args` and the request it returns are live for this callback.
            let (uri, context) = unsafe {
                let request = args.Request()?;
                let mut uri = PWSTR::null();
                request.Uri(&mut uri)?;
                let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT::default();
                args.ResourceContext(&mut context)?;
                (take_pwstr(uri), context)
            };
            let allowed = match Url::parse(&uri) {
                Ok(url) => request_policy.allows(&PolicyRequest {
                    url: &uri,
                    bound_scheme: bound_scheme(&url),
                    top_level: context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT,
                    main_frame: false,
                }),
                // A request the shell cannot read is a request it cannot vouch for.
                Err(_) => false,
            };
            if allowed {
                return Ok(());
            }
            let reason = HSTRING::from("Blocked by the SeuOffice request policy");
            // SAFETY: as above.
            unsafe {
                let response = environment.CreateWebResourceResponse(
                    None,
                    BLOCKED_STATUS,
                    &reason,
                    &HSTRING::new(),
                )?;
                args.SetResponse(&response)
            }
        }));

        // SAFETY: UI thread, inside the `with_webview` closure that attached the observers; the
        // settings, the filter and the handler belong to this webview.
        crate::trace::trace!("attach", "windows: applying the request policy");
        unsafe {
            let settings = core
                .Settings()
                .map_err(failed("The WebView2 settings are unavailable"))?;
            settings
                .SetIsScriptEnabled(policy.javascript())
                .map_err(failed("The page script policy could not be applied"))?;
            crate::trace::trace!("attach", "windows: script policy applied");

            // Every request of every frame and worker goes through the policy, not only the
            // documents: the `_22` filter names the request sources, older runtimes take the
            // default sources.
            let everything = HSTRING::from("*");
            let filter = failed("The request filter could not be added");
            match core.cast::<ICoreWebView2_22>() {
                Ok(core_22) => core_22
                    .AddWebResourceRequestedFilterWithRequestSourceKinds(
                        &everything,
                        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                        COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
                    )
                    .map_err(filter)?,
                Err(_) => core
                    .AddWebResourceRequestedFilter(
                        &everything,
                        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                    )
                    .map_err(filter)?,
            }
            crate::trace::trace!("attach", "windows: request filter added");

            let mut token = 0i64;
            core.add_WebResourceRequested(&handler, &mut token)
                .map_err(failed("The request policy handler could not be registered"))?;
        }
        crate::trace::trace!("attach", "windows: policy filter and handler in place");
        browser.observe_policy_installed();
        crate::trace::trace!("attach", "windows: policy installed");
        Ok(())
    }
}
