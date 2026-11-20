//! `Observe` for WKWebView: a navigation delegate chained in front of wry's.
//!
//! wry installs its own `WKNavigationDelegate` and keeps it alive; WebKit holds a delegate
//! weakly and has room for one. This delegate takes the slot and holds wry's weakly: every
//! selector it does not implement is forwarded to wry's delegate (`respondsToSelector:` and
//! `forwardingTargetForSelector:`), and the ones it implements call wry's implementation
//! after reporting. The web view keeps this object alive through an associated object.
//!
//! What it reports:
//! - main-frame admission, around wry's policy decision (the `on_navigation` hook cannot tell
//!   frames apart on WKWebView) and the per-navigation JavaScript switch of the request policy;
//! - load failures, only from `didFail(Provisional)Navigation`, for the navigation in flight;
//! - the web content process terminating, history-API address changes (KVO of `URL`) and the
//!   first responder entering or leaving the web view (KVO of the window's `firstResponder`).
//!
//! Until the request policy's content rules are installed (see `policy.rs`) every navigation
//! decision is held, so nothing loads before the rules apply.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

use block2::{DynBlock, RcBlock};
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Bool, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::NSView;
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKWebView,
    WKWebpagePreferences,
};
use tauri::Url;
use tauri::webview::PlatformWebview;

use super::Platform;
use crate::native::browser::BrowserIdentity;
use crate::native::events::{LoadError, ProcessFailure, ViewEvent};
use crate::native::lifecycle::BrowserCloseSubscription;
use crate::native::platform::{Observe, with_navigation_frame};

static ASSOCIATION: u8 = 0;
static URL_CONTEXT: u8 = 1;
static FOCUS_CONTEXT: u8 = 2;

fn address(token: &'static u8) -> *const c_void {
    ptr::from_ref(token).cast()
}

type Handler = RcBlock<dyn Fn(WKNavigationActionPolicy, NonNull<WKWebpagePreferences>)>;
type Resume = Box<dyn FnOnce(bool)>;

/// Whether navigation decisions may be answered yet.
enum Gate {
    Open,
    /// Decisions wait here until the request policy's content rules are installed.
    Waiting(Vec<Resume>),
    /// The rules could not be installed: every navigation is refused.
    Failed,
}

struct State {
    /// Dropped when the browser retires: the web view outlives it (wry never frees its web
    /// view), and the browser's event sink must not live as long.
    browser: Option<BrowserIdentity>,
    /// The provisional navigation of the load in flight.
    latest: Option<Retained<WKNavigation>>,
    committed: bool,
    gate: Gate,
    in_view: bool,
    watching_view: bool,
    watching_window: bool,
    retired: Option<BrowserCloseSubscription>,
}

struct Chain {
    wry: Weak<AnyObject>,
    webview: Weak<AnyObject>,
    window: Option<Weak<AnyObject>>,
    javascript: bool,
    state: RefCell<State>,
}

/// Runs a closure that only ever executes on the UI thread inside a `Send` callback.
struct UiOnly<T>(T);

// SAFETY: the wrapped weak reference is only used by `retire`, which does nothing off the UI
// thread.
unsafe impl<T> Send for UiOnly<T> {}

impl UiOnly<Weak<ChainedDelegate>> {
    fn retire(&self) {
        if MainThreadMarker::new().is_some()
            && let Some(delegate) = self.0.load()
        {
            delegate.unwatch();
        }
    }
}

/// Send `$send` to wry's delegate when it is still there and implements `$selector`.
macro_rules! forward {
    ($delegate:expr, $selector:expr, |$wry:ident| $send:expr) => {
        if let Some($wry) = $delegate.ivars().wry.load() {
            // SAFETY: `respondsToSelector:` is understood by every object.
            let responds: bool = unsafe { msg_send![&*$wry, respondsToSelector: $selector] };
            if responds {
                // SAFETY: wry's delegate implements the selector; it gets the very arguments
                // WebKit gave this delegate.
                let _: () = unsafe { $send };
            }
        }
    };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SeuOfficeChainedNavigationDelegate"]
    #[ivars = Chain]
    struct ChainedDelegate;

    unsafe impl NSObjectProtocol for ChainedDelegate {}

    unsafe impl WKNavigationDelegate for ChainedDelegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:preferences:decisionHandler:))]
        fn decide_policy(
            &self,
            webview: &WKWebView,
            action: &WKNavigationAction,
            preferences: &WKWebpagePreferences,
            handler: &DynBlock<dyn Fn(WKNavigationActionPolicy, NonNull<WKWebpagePreferences>)>,
        ) {
            self.decide(webview, action, preferences, handler);
        }

        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start(&self, webview: &WKWebView, navigation: Option<&WKNavigation>) {
            self.ivars().state.borrow_mut().latest =
                navigation.map(|navigation| navigation.retain());
            self.ivars().state.borrow_mut().committed = false;
            forward!(self, sel!(webView:didStartProvisionalNavigation:), |wry| {
                msg_send![&*wry, webView: webview, didStartProvisionalNavigation: navigation]
            });
        }

        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit(&self, webview: &WKWebView, navigation: Option<&WKNavigation>) {
            forward!(self, sel!(webView:didCommitNavigation:), |wry| {
                msg_send![&*wry, webView: webview, didCommitNavigation: navigation]
            });
            if self.is_current(navigation) {
                self.ivars().state.borrow_mut().committed = true;
            }
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, webview: &WKWebView, navigation: Option<&WKNavigation>) {
            // A navigation that was replaced must not end the load of the one that replaced it.
            if self.is_current(navigation) {
                forward!(self, sel!(webView:didFinishNavigation:), |wry| {
                    msg_send![&*wry, webView: webview, didFinishNavigation: navigation]
                });
            }
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            webview: &WKWebView,
            navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.failed(navigation, error);
            forward!(self, sel!(webView:didFailProvisionalNavigation:withError:), |wry| {
                msg_send![
                    &*wry,
                    webView: webview,
                    didFailProvisionalNavigation: navigation,
                    withError: error
                ]
            });
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(
            &self,
            webview: &WKWebView,
            navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.failed(navigation, error);
            forward!(self, sel!(webView:didFailNavigation:withError:), |wry| {
                msg_send![&*wry, webView: webview, didFailNavigation: navigation, withError: error]
            });
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn did_terminate(&self, webview: &WKWebView) {
            if let Some(browser) = self.browser() {
                browser.observe_process_failed(ProcessFailure::ContentProcessTerminated);
            }
            forward!(self, sel!(webViewWebContentProcessDidTerminate:), |wry| {
                msg_send![&*wry, webViewWebContentProcessDidTerminate: webview]
            });
        }
    }

    impl ChainedDelegate {
        #[unsafe(method(respondsToSelector:))]
        fn responds_to_selector(&self, selector: Sel) -> Bool {
            // SAFETY: NSObject's own implementation.
            let own: Bool = unsafe { msg_send![super(self), respondsToSelector: selector] };
            if own.as_bool() {
                return own;
            }
            match self.ivars().wry.load() {
                // SAFETY: `respondsToSelector:` is understood by every object.
                Some(wry) => unsafe { msg_send![&*wry, respondsToSelector: selector] },
                None => Bool::NO,
            }
        }

        #[unsafe(method_id(forwardingTargetForSelector:))]
        fn forwarding_target(&self, _selector: Sel) -> Option<Retained<AnyObject>> {
            self.ivars().wry.load()
        }

        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            _key_path: Option<&NSString>,
            object: Option<&AnyObject>,
            _change: Option<&AnyObject>,
            context: *mut c_void,
        ) {
            if context.cast_const() == address(&URL_CONTEXT) {
                self.address_changed(object);
            } else if context.cast_const() == address(&FOCUS_CONTEXT) {
                self.focus_changed();
            }
        }
    }
);

/// Everything of one navigation decision that has to outlive the delegate call, because the
/// decision may be held until the content rules are installed.
struct Decision {
    delegate: Retained<ChainedDelegate>,
    webview: Retained<WKWebView>,
    action: Retained<WKNavigationAction>,
    preferences: Retained<WKWebpagePreferences>,
    handler: Handler,
}

/// Whether the action navigates the main frame, and to what.
fn describe(action: &WKNavigationAction) -> (bool, Option<Url>) {
    // SAFETY: plain getters on the action WebKit passed.
    unsafe {
        let main_frame = action
            .targetFrame()
            .is_some_and(|frame| frame.isMainFrame());
        let url = action
            .request()
            .URL()
            .and_then(|url| url.absoluteString())
            .and_then(|text| Url::parse(&text.to_string()).ok());
        (main_frame, url)
    }
}

/// Report a main-frame decision to the browser: `Allow` admits the navigation (which revokes
/// the current document) and may still refuse it; `Cancel` is a refusal.
fn admit(
    browser: &BrowserIdentity,
    main_frame: bool,
    url: Option<&Url>,
    policy: WKNavigationActionPolicy,
) -> WKNavigationActionPolicy {
    let Some(url) = url.filter(|_| main_frame) else {
        return policy;
    };
    if policy == WKNavigationActionPolicy::Allow {
        if browser.observe_navigation_allowed(url) {
            policy
        } else {
            WKNavigationActionPolicy::Cancel
        }
    } else {
        if policy == WKNavigationActionPolicy::Cancel {
            browser.observe_navigation_refused(url);
        }
        policy
    }
}

impl Decision {
    fn run(self, open: bool) {
        let Self {
            delegate,
            webview,
            action,
            preferences,
            handler,
        } = self;
        let chain = delegate.ivars();
        let (main_frame, url) = describe(&action);
        let browser = delegate.browser();
        let (Some(browser), Some(wry), true) = (browser.clone(), chain.wry.load(), open) else {
            let refused = WKNavigationActionPolicy::Cancel;
            let policy = match &browser {
                Some(browser) => admit(browser, main_frame, url.as_ref(), refused),
                None => refused,
            };
            handler.call((policy, NonNull::from(&*preferences)));
            return;
        };
        if !chain.javascript {
            // SAFETY: the preferences object belongs to this decision.
            unsafe { preferences.setAllowsContentJavaScript(false) };
        }
        let reply: RcBlock<dyn Fn(WKNavigationActionPolicy)> =
            RcBlock::new(move |policy: WKNavigationActionPolicy| {
                let policy = admit(&browser, main_frame, url.as_ref(), policy);
                handler.call((policy, NonNull::from(&*preferences)));
            });
        // SAFETY: `respondsToSelector:` is understood by every object; wry's delegate gets the
        // arguments WebKit gave this one, with a handler of the two-argument variant's type.
        unsafe {
            let responds: bool = msg_send![
                &*wry,
                respondsToSelector: sel!(webView:decidePolicyForNavigationAction:decisionHandler:)
            ];
            if responds {
                // wry's delegate calls the `on_navigation` hook synchronously from here; the
                // hook only gets a URL, so it is told which frame this decision is for.
                with_navigation_frame(main_frame, || {
                    let _: () = msg_send![
                        &*wry,
                        webView: &*webview,
                        decidePolicyForNavigationAction: &*action,
                        decisionHandler: &*reply
                    ];
                });
            } else {
                reply.call((WKNavigationActionPolicy::Allow,));
            }
        }
    }
}

impl ChainedDelegate {
    fn new(mtm: MainThreadMarker, chain: Chain) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(chain);
        // SAFETY: NSObject's plain `init`.
        unsafe { msg_send![super(this), init] }
    }

    fn browser(&self) -> Option<BrowserIdentity> {
        self.ivars().state.borrow().browser.clone()
    }

    fn decide(
        &self,
        webview: &WKWebView,
        action: &WKNavigationAction,
        preferences: &WKWebpagePreferences,
        handler: &DynBlock<dyn Fn(WKNavigationActionPolicy, NonNull<WKWebpagePreferences>)>,
    ) {
        let decision = Decision {
            delegate: self.retain(),
            webview: webview.retain(),
            action: action.retain(),
            preferences: preferences.retain(),
            handler: handler.copy(),
        };
        let open = {
            let mut state = self.ivars().state.borrow_mut();
            match &mut state.gate {
                Gate::Open => true,
                Gate::Failed => false,
                Gate::Waiting(queue) => {
                    queue.push(Box::new(move |open| decision.run(open)));
                    return;
                }
            }
        };
        decision.run(open);
    }

    /// Answer every held decision (allowed when `next` is open) and take `next` as the gate.
    fn settle(&self, next: Gate) {
        let open = matches!(next, Gate::Open);
        let held = match std::mem::replace(&mut self.ivars().state.borrow_mut().gate, next) {
            Gate::Waiting(held) => held,
            _ => Vec::new(),
        };
        for resume in held {
            resume(open);
        }
    }

    fn is_current(&self, navigation: Option<&WKNavigation>) -> bool {
        match (&self.ivars().state.borrow().latest, navigation) {
            (Some(latest), Some(navigation)) => ptr::eq(&**latest, navigation),
            _ => true,
        }
    }

    /// A load failure: from the OS, for the navigation in flight, and not a cancellation (the
    /// load was replaced or stopped, which the replacement reports itself).
    fn failed(&self, navigation: Option<&WKNavigation>, error: &NSError) {
        const URL_ERROR_CANCELLED: isize = -999;
        if !self.is_current(navigation) {
            return;
        }
        let code = error.code();
        if code == URL_ERROR_CANCELLED && error.domain().to_string() == "NSURLErrorDomain" {
            return;
        }
        let description = error.localizedDescription().to_string();
        if let Some(browser) = self.browser() {
            browser.observe_load(Err(LoadError {
                code: i32::try_from(code).ok(),
                description,
            }));
        }
    }

    fn address_changed(&self, object: Option<&AnyObject>) {
        let Some(view) = object else { return };
        // Between a navigation's start and its commit `URL` already names the pending page.
        if !self.ivars().state.borrow().committed {
            return;
        }
        // SAFETY: `URL` is a plain getter of WKWebView.
        let url: Option<Retained<NSURL>> = unsafe { msg_send![view, URL] };
        let Some(url) = url
            .and_then(|url| url.absoluteString())
            .and_then(|text| Url::parse(&text.to_string()).ok())
        else {
            return;
        };
        let Some(browser) = self.browser() else {
            return;
        };
        if browser.url().as_ref() != Some(&url) {
            browser.observe_address(&url);
        }
    }

    fn view_has_focus(&self) -> bool {
        let chain = self.ivars();
        let (Some(window), Some(view)) = (
            chain.window.as_ref().and_then(|window| window.load()),
            chain.webview.load(),
        ) else {
            return false;
        };
        // SAFETY: `firstResponder` is a plain getter of NSWindow.
        let responder: Option<Retained<AnyObject>> = unsafe { msg_send![&*window, firstResponder] };
        responder.is_some_and(|responder| {
            responder.downcast_ref::<NSView>().is_some_and(|responder| {
                // SAFETY: `isDescendantOf:` is a plain getter of NSView.
                unsafe { msg_send![responder, isDescendantOf: &*view] }
            })
        })
    }

    fn focus_changed(&self) {
        let inside = self.view_has_focus();
        let changed = {
            let mut state = self.ivars().state.borrow_mut();
            std::mem::replace(&mut state.in_view, inside) != inside
        };
        if changed && let Some(browser) = self.browser() {
            browser.observe_view(if inside {
                ViewEvent::Focus
            } else {
                ViewEvent::Blur
            });
        }
    }

    /// Start the key-value observers and arrange for them to end with the browser.
    fn watch(&self, view: &AnyObject, window: Option<&AnyObject>) -> Result<(), String> {
        let key = |name: &str| NSString::from_str(name);
        // SAFETY: `URL` and `firstResponder` are observable properties; the observer is this
        // object, which removes itself in `unwatch` before the browser is gone.
        unsafe {
            let _: () = msg_send![
                view,
                addObserver: self,
                forKeyPath: &*key("URL"),
                options: 0usize,
                context: address(&URL_CONTEXT).cast_mut()
            ];
            self.ivars().state.borrow_mut().watching_view = true;
            if let Some(window) = window {
                let _: () = msg_send![
                    window,
                    addObserver: self,
                    forKeyPath: &*key("firstResponder"),
                    options: 0usize,
                    context: address(&FOCUS_CONTEXT).cast_mut()
                ];
                self.ivars().state.borrow_mut().watching_window = true;
            }
        }
        let weak = UiOnly(Weak::new(self));
        let browser = self
            .browser()
            .ok_or_else(|| "The browser retired before its observers attached.".to_owned())?;
        let retired = browser
            .when_closed(move |_| weak.retire())
            .map_err(|error| {
                format!("The navigation observer could not follow the browser: {error}")
            })?;
        let inside = self.view_has_focus();
        let mut state = self.ivars().state.borrow_mut();
        state.in_view = inside;
        state.retired = Some(retired);
        Ok(())
    }

    /// The browser retired: stop observing and refuse what is still held.
    fn unwatch(&self) {
        let (had_view, had_window) = {
            let mut state = self.ivars().state.borrow_mut();
            state.browser = None;
            (
                std::mem::take(&mut state.watching_view),
                std::mem::take(&mut state.watching_window),
            )
        };
        let key = |name: &str| NSString::from_str(name);
        let chain = self.ivars();
        // SAFETY: each observer was added in `watch` and is removed once, with its context.
        unsafe {
            if had_view && let Some(view) = chain.webview.load() {
                let _: () = msg_send![
                    &*view,
                    removeObserver: self,
                    forKeyPath: &*key("URL"),
                    context: address(&URL_CONTEXT).cast_mut()
                ];
            }
            if had_window && let Some(window) = chain.window.as_ref().and_then(|w| w.load()) {
                let _: () = msg_send![
                    &*window,
                    removeObserver: self,
                    forKeyPath: &*key("firstResponder"),
                    context: address(&FOCUS_CONTEXT).cast_mut()
                ];
            }
        }
        self.settle(Gate::Failed);
    }
}

/// The chained delegate of a web view, by the association `attach` made.
fn associated(view: &AnyObject) -> Option<Retained<ChainedDelegate>> {
    // SAFETY: reads the association this module wrote; the object is retained at once.
    let object =
        unsafe { objc2::ffi::objc_getAssociatedObject(ptr::from_ref(view), address(&ASSOCIATION)) };
    let object = NonNull::new(object.cast_mut())?;
    // SAFETY: only this module stores under that key, and it stores a `ChainedDelegate`.
    unsafe { Retained::retain(object.as_ptr().cast::<ChainedDelegate>()) }
}

/// Lets the request policy release or refuse the navigations held for it.
pub(super) struct GateHandle(Retained<ChainedDelegate>);

impl GateHandle {
    pub(super) fn open(&self) {
        self.0.settle(Gate::Open);
    }

    pub(super) fn fail(&self) {
        self.0.settle(Gate::Failed);
    }
}

/// The gate of the web view behind `view` (a `PlatformWebview::inner` pointer), if observed.
pub(super) fn gate_for(view: *mut c_void) -> Option<GateHandle> {
    let view = NonNull::new(view.cast::<AnyObject>())?;
    // SAFETY: the pointer names the live web view of the calling `with_webview` job.
    associated(unsafe { view.as_ref() }).map(GateHandle)
}

impl Observe for Platform {
    const NAVIGATION_HOOK_IS_MAIN_FRAME: bool = false;
    const CONFIRMS_LOAD_OUTCOME: bool = false;

    fn attach(platform: &PlatformWebview, browser: BrowserIdentity) -> Result<(), String> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| "Observers must attach on the UI thread.".to_owned())?;
        let view = NonNull::new(platform.inner().cast::<AnyObject>())
            .ok_or_else(|| "The web view handle is empty.".to_owned())?;
        // SAFETY: the handle names the live web view of the calling `with_webview` job.
        let view: &AnyObject = unsafe { view.as_ref() };
        if associated(view).is_some() {
            return Err("The navigation observer is already attached.".to_owned());
        }
        // SAFETY: `navigationDelegate` is a plain getter of WKWebView.
        let wry: Option<Retained<AnyObject>> = unsafe { msg_send![view, navigationDelegate] };
        let wry = wry.ok_or_else(|| "The web view has no navigation delegate.".to_owned())?;
        // SAFETY: as above, for the window the web view sits in.
        let window: Option<&AnyObject> = NonNull::new(platform.ns_window().cast::<AnyObject>())
            .map(|window| unsafe { window.as_ref() });
        let javascript = browser.policy().is_none_or(|policy| policy.javascript());
        let gate = if browser.policy().is_some() {
            Gate::Waiting(Vec::new())
        } else {
            Gate::Open
        };
        let chain = Chain {
            wry: Weak::from_retained(&wry),
            webview: Weak::new(view),
            window: window.map(Weak::new),
            javascript,
            state: RefCell::new(State {
                browser: Some(browser),
                latest: None,
                committed: false,
                gate,
                in_view: false,
                watching_view: false,
                watching_window: false,
                retired: None,
            }),
        };
        let delegate = ChainedDelegate::new(mtm, chain);
        // SAFETY: the association retains the delegate for the life of the web view; WebKit
        // holds it weakly, wry's delegate stays alive in wry.
        unsafe {
            objc2::ffi::objc_setAssociatedObject(
                ptr::from_ref(view).cast_mut(),
                address(&ASSOCIATION),
                Retained::as_ptr(&delegate).cast_mut().cast(),
                objc2::ffi::OBJC_ASSOCIATION_RETAIN_NONATOMIC,
            );
            let _: () = msg_send![view, setNavigationDelegate: &*delegate];
        }
        delegate.watch(view, window)
    }
}
