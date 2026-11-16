//! The baseline observer: what Tauri's public hooks say, translated into browser reports.
//!
//! - `on_navigation` runs before commit on both OSes. It applies the shell's request policy
//!   and, where the hook is main-frame only, admits the navigation (which revokes the current
//!   document before the successor can commit).
//! - `PageLoadEvent::Started` is the commit of a main-frame document: WKWebView
//!   `didCommitNavigation`, WebView2 `ContentLoading`.
//! - `PageLoadEvent::Finished` ends a load successfully only where the adapter does not
//!   confirm outcomes itself ([`Observe::CONFIRMS_LOAD_OUTCOME`]).
//!
//! Window focus and destruction come through `Native::on_window_event`.

use tauri::Url;
use tauri::webview::PageLoadEvent;

use super::{Current, Observe, navigation_is_main_frame};
use crate::native::browser::{BeforePolicy, BrowserIdentity};
use crate::native::policy::PolicyRequest;

/// The shell scheme a URL was served through: `genoffice-app-diagram://x` on macOS,
/// `http(s)://genoffice-app-diagram.localhost/x` on Windows. `None` for web and inline URLs.
pub(crate) fn bound_scheme(url: &Url) -> Option<&str> {
    match url.scheme() {
        "http" | "https" => url
            .host_str()?
            .strip_suffix(".localhost")
            .filter(|scheme| !scheme.is_empty()),
        "about" | "data" | "blob" | "file" | "ws" | "wss" => None,
        other => Some(other),
    }
}

/// The `on_navigation` hook. Returns whether the navigation may start.
///
/// Before the OS webview enforces the browser's policy, a navigation to a real URL is
/// cancelled and remembered; it replays once the policy is in force (see
/// `PolicyOps::apply_policy`), so the initial request never escapes it. `about:blank` passes.
///
/// The policy sees every navigation the hook reports as `top_level`: on macOS that includes
/// subframe navigations (with `main_frame` false), which are held to the same allow list.
pub(crate) fn on_navigation(browser: &BrowserIdentity, url: &Url) -> bool {
    match browser.core.before_policy(url) {
        BeforePolicy::Blank => return true,
        BeforePolicy::Hold => return false,
        BeforePolicy::Judge => {}
    }
    if let Some(policy) = browser.policy() {
        let request = PolicyRequest {
            url: url.as_str(),
            bound_scheme: bound_scheme(url),
            top_level: true,
            main_frame: navigation_is_main_frame(),
        };
        if !policy.allows(&request) {
            if Current::NAVIGATION_HOOK_IS_MAIN_FRAME {
                browser.observe_navigation_refused(url);
            }
            return false;
        }
    }
    if Current::NAVIGATION_HOOK_IS_MAIN_FRAME {
        browser.observe_navigation_allowed(url)
    } else {
        true
    }
}

/// The `on_page_load` hook.
pub(crate) fn on_page_load(browser: &BrowserIdentity, event: PageLoadEvent, url: &Url) {
    match event {
        PageLoadEvent::Started => browser.observe_commit(url),
        PageLoadEvent::Finished => {
            if !Current::CONFIRMS_LOAD_OUTCOME {
                browser.observe_load(Ok(()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::browser::testing::{FakeControl, browser_with_events, url};
    use crate::native::navigation::NavigationStart;
    use crate::native::policy::RequestPolicy;

    #[test]
    fn the_bound_scheme_is_the_same_on_both_url_shapes() {
        assert_eq!(
            bound_scheme(&url("genoffice-app-diagram://localhost/index.html")),
            Some("genoffice-app-diagram")
        );
        assert_eq!(
            bound_scheme(&url("http://genoffice-app-diagram.localhost/index.html")),
            Some("genoffice-app-diagram")
        );
        assert_eq!(
            bound_scheme(&url("https://genoffice-app-diagram.localhost/")),
            Some("genoffice-app-diagram")
        );
        assert_eq!(bound_scheme(&url("https://example.org/")), None);
        assert_eq!(bound_scheme(&url("http://localhost/")), None);
        assert_eq!(bound_scheme(&url("data:text/plain,hi")), None);
        assert_eq!(bound_scheme(&url("about:blank")), None);
    }

    fn policy_browser(
        label: &str,
        allow: fn(&PolicyRequest<'_>) -> bool,
        control: std::sync::Arc<FakeControl>,
    ) -> BrowserIdentity {
        use crate::native::browser::{BridgeMode, BrowserCore, Config};
        use crate::native::lifecycle::BrowserCreation;
        let creation = BrowserCreation::new().unwrap();
        let receipt = creation.receipt();
        let submission = creation.into_submission();
        receipt.claim().unwrap();
        let config = Config {
            bridge: BridgeMode::Invoke,
            policy: Some(RequestPolicy::new(allow)),
            ..Config::default()
        };
        let core = BrowserCore::new(label, submission.creation(), receipt, config).unwrap();
        core.mark_attached(control, label);
        core.identity()
    }

    #[test]
    fn a_refused_navigation_never_starts_and_leaves_the_document_alone() {
        let browser = policy_browser(
            "policy",
            |request| request.url.starts_with("https://example.test/"),
            FakeControl::new(),
        );
        browser.observe_policy_installed();

        assert!(!on_navigation(&browser, &url("https://evil.test/")));
        assert!(
            browser.navigation().is_none(),
            "a refused navigation is not admitted"
        );
        assert!(on_navigation(&browser, &url("https://example.test/a")));
        assert_eq!(
            browser.navigation().is_some(),
            Current::NAVIGATION_HOOK_IS_MAIN_FRAME
        );
    }

    #[test]
    fn the_first_real_navigation_is_held_until_the_policy_is_enforced_and_then_replayed() {
        let control = FakeControl::new();
        let browser = policy_browser("policy-hold", |_| true, control.clone());
        let ready = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = ready.clone();
        browser
            .navigator()
            .when_ready(move |result| sink.lock().unwrap().push(result));

        assert!(
            on_navigation(&browser, &url("about:blank")),
            "a blank page fetches nothing"
        );
        assert!(
            !on_navigation(&browser, &url("https://example.test/a")),
            "held, not started"
        );
        assert!(
            !on_navigation(&browser, &url("https://example.test/b")),
            "only the first is kept"
        );
        assert!(browser.navigation().is_none());
        assert!(
            ready.lock().unwrap().is_empty(),
            "not ready before the policy is enforced"
        );
        assert!(control.navigations.lock().unwrap().is_empty());

        browser.observe_policy_installed();
        assert_eq!(
            *control.navigations.lock().unwrap(),
            ["https://example.test/a"]
        );
        assert_eq!(*ready.lock().unwrap(), [Ok(())]);
        assert!(
            on_navigation(&browser, &url("https://example.test/a")),
            "judged normally now"
        );
    }

    #[test]
    fn a_main_frame_flag_set_by_an_adapter_reaches_the_policy() {
        use crate::native::platform::with_navigation_frame;
        use std::sync::atomic::{AtomicBool, Ordering};
        static SAW_MAIN_FRAME: AtomicBool = AtomicBool::new(false);
        let browser = policy_browser(
            "policy-frame",
            |request| {
                SAW_MAIN_FRAME.store(request.main_frame, Ordering::SeqCst);
                true
            },
            FakeControl::new(),
        );
        browser.observe_policy_installed();
        with_navigation_frame(false, || {
            on_navigation(&browser, &url("https://example.test/f"))
        });
        assert!(!SAW_MAIN_FRAME.load(Ordering::SeqCst));
        with_navigation_frame(true, || {
            on_navigation(&browser, &url("https://example.test/m"))
        });
        assert!(SAW_MAIN_FRAME.load(Ordering::SeqCst));
    }

    #[test]
    fn started_commits_and_finished_confirms_only_without_an_adapter() {
        let (browser, _events) = browser_with_events(
            "public-load",
            FakeControl::new(),
            NavigationStart::Automatic,
        );
        let address = url("https://example.test/a");
        on_page_load(&browser, PageLoadEvent::Started, &address);
        assert!(browser.document().is_some());
        let before = browser.observation();
        on_page_load(&browser, PageLoadEvent::Finished, &address);
        assert_eq!(
            browser.observation() != before,
            !Current::CONFIRMS_LOAD_OUTCOME
        );
    }
}
