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

use super::{Current, Observe};
use crate::native::browser::BrowserIdentity;
use crate::native::policy::PolicyRequest;

/// The shell scheme a URL was served through: `genoffice-app-diagram://x` on macOS,
/// `http(s)://genoffice-app-diagram.localhost/x` on Windows. `None` for web and inline URLs.
pub(crate) fn bound_scheme(url: &Url) -> Option<&str> {
    match url.scheme() {
        "http" | "https" => url.host_str()?.strip_suffix(".localhost").filter(|scheme| !scheme.is_empty()),
        "about" | "data" | "blob" | "file" | "ws" | "wss" => None,
        other => Some(other),
    }
}

/// The `on_navigation` hook. Returns whether the navigation may start.
///
/// The policy sees every navigation the hook reports as `top_level`: on macOS that includes
/// subframe navigations, which are therefore held to the same allow list.
pub(crate) fn on_navigation(browser: &BrowserIdentity, url: &Url) -> bool {
    if let Some(policy) = browser.policy() {
        let request = PolicyRequest { url: url.as_str(), bound_scheme: bound_scheme(url), top_level: true };
        if !policy.allows(&request) {
            if Current::NAVIGATION_HOOK_IS_MAIN_FRAME {
                browser.observe_navigation_refused(url);
            }
            return false;
        }
    }
    if Current::NAVIGATION_HOOK_IS_MAIN_FRAME { browser.observe_navigation_allowed(url) } else { true }
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
        assert_eq!(bound_scheme(&url("genoffice-app-diagram://localhost/index.html")), Some("genoffice-app-diagram"));
        assert_eq!(bound_scheme(&url("http://genoffice-app-diagram.localhost/index.html")), Some("genoffice-app-diagram"));
        assert_eq!(bound_scheme(&url("https://genoffice-app-diagram.localhost/")), Some("genoffice-app-diagram"));
        assert_eq!(bound_scheme(&url("https://example.org/")), None);
        assert_eq!(bound_scheme(&url("http://localhost/")), None);
        assert_eq!(bound_scheme(&url("data:text/plain,hi")), None);
        assert_eq!(bound_scheme(&url("about:blank")), None);
    }

    #[test]
    fn a_refused_navigation_never_starts_and_leaves_the_document_alone() {
        use crate::native::browser::{BridgeMode, Config};
        use crate::native::lifecycle::BrowserCreation;
        let creation = BrowserCreation::new().unwrap();
        let receipt = creation.receipt();
        let submission = creation.into_submission();
        receipt.claim().unwrap();
        let policy = RequestPolicy::new(|request| request.url.starts_with("https://example.test/"));
        let config = Config { bridge: BridgeMode::Invoke, policy: Some(policy), ..Config::default() };
        let core = crate::native::browser::BrowserCore::new("policy", submission.creation(), receipt, config).unwrap();
        let browser = core.identity();

        assert!(!on_navigation(&browser, &url("https://evil.test/")));
        assert!(browser.navigation().is_none(), "a refused navigation is not admitted");
        assert!(on_navigation(&browser, &url("https://example.test/a")));
        assert_eq!(browser.navigation().is_some(), Current::NAVIGATION_HOOK_IS_MAIN_FRAME);
    }

    #[test]
    fn started_commits_and_finished_confirms_only_without_an_adapter() {
        let (browser, _events) = browser_with_events("public-load", FakeControl::new(), NavigationStart::Automatic);
        let address = url("https://example.test/a");
        on_page_load(&browser, PageLoadEvent::Started, &address);
        assert!(browser.document().is_some());
        let before = browser.observation();
        on_page_load(&browser, PageLoadEvent::Finished, &address);
        assert_eq!(browser.observation() != before, !Current::CONFIRMS_LOAD_OUTCOME);
    }
}
