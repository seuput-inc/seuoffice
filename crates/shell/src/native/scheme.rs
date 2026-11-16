//! Scheme request context and the one-shot response gate.
//!
//! Tauri 2 hands a custom scheme handler the label of the webview that issued the request
//! and the request itself; it does not say which document did. So [`SchemeRequestContext`]
//! resolves the label through the registry at intake and reads that browser's committed
//! document at that moment (the weakened guard W1). Delivery re-checks the same document
//! inside the owner gate, through [`Prepared::accept`]. [`Submitted`] says the answer was
//! given to the webview, never that it was displayed (W4).
//!
//! Hidden target browsers (html-asset, drawio print) admit exactly one controlled navigation,
//! so the browser's navigation at intake is as exact as CEF's frame binding was.
//!
//! The Windows URL shape (`http://<scheme>.localhost/`) and the macOS shape
//! (`<scheme>://localhost/`) stay in `app_scheme::webview_url`, the one translator.

use std::any::Any;
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::UriSchemeResponder;
use tauri::http::Response;

use super::Native;
use super::browser::BrowserIdentity;
use super::defer::defer;
use super::document::{DocumentContext, DocumentIdentity};
use super::navigation::NavigationIdentity;

type Answer = Response<Cow<'static, [u8]>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Open,
    Accepted,
    Submitted,
    Cancelled,
}

struct CancelListener {
    active: AtomicBool,
    callback: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[derive(Default)]
struct Cancellation {
    cancelled: bool,
    listeners: Vec<Arc<CancelListener>>,
}

/// Retain while the cancellation must be observed. Dropping it unregisters the callback.
pub struct RequestCancelSubscription {
    listener: Arc<CancelListener>,
}

impl Drop for RequestCancelSubscription {
    fn drop(&mut self) {
        self.listener.active.store(false, Ordering::Release);
    }
}

fn deliver_cancel(listener: Arc<CancelListener>) {
    defer(move || {
        if !listener.active.swap(false, Ordering::AcqRel) {
            return;
        }
        let callback = listener
            .callback
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(callback) = callback {
            callback();
        }
    });
}

/// State of one request: its answer phase and its cancellation. Cancellation and acceptance
/// serialize on the phase mutex, so a cancelled request can never be accepted.
struct RequestState {
    phase: Mutex<Phase>,
    cancellation: Mutex<Cancellation>,
}

impl RequestState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            phase: Mutex::new(Phase::Open),
            cancellation: Mutex::default(),
        })
    }

    fn cancel(&self) {
        {
            let mut phase = self.phase.lock().unwrap_or_else(|error| error.into_inner());
            if *phase != Phase::Submitted {
                *phase = Phase::Cancelled;
            }
        }
        let listeners = {
            let mut state = self
                .cancellation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.cancelled = true;
            std::mem::take(&mut state.listeners)
        };
        for listener in listeners {
            deliver_cancel(listener);
        }
    }

    fn subscribe(
        &self,
        callback: Box<dyn FnOnce() + Send>,
    ) -> Result<RequestCancelSubscription, String> {
        let listener = Arc::new(CancelListener {
            active: AtomicBool::new(true),
            callback: Mutex::new(Some(callback)),
        });
        let cancelled = {
            let mut state = self
                .cancellation
                .lock()
                .map_err(|_| "Native request cancellation state was poisoned.")?;
            if !state.cancelled {
                state.listeners.retain(|l| l.active.load(Ordering::Acquire));
                state.listeners.push(listener.clone());
            }
            state.cancelled
        };
        if cancelled {
            deliver_cancel(listener.clone());
        }
        Ok(RequestCancelSubscription { listener })
    }

    /// Open to accepted; the only way to answer.
    fn accept(&self) -> Result<(), &'static str> {
        let mut phase = self
            .phase
            .lock()
            .map_err(|_| "Request response state was poisoned.")?;
        match *phase {
            Phase::Open => {
                *phase = Phase::Accepted;
                Ok(())
            }
            Phase::Cancelled => Err("The original native request was cancelled."),
            Phase::Accepted | Phase::Submitted => {
                Err("The original native request was already answered.")
            }
        }
    }

    /// Accepted to submitted; false when the request was cancelled in between.
    fn submit(&self) -> bool {
        let mut phase = self.phase.lock().unwrap_or_else(|error| error.into_inner());
        if *phase == Phase::Accepted {
            *phase = Phase::Submitted;
            true
        } else {
            false
        }
    }

    fn is_open(&self) -> bool {
        *self.phase.lock().unwrap_or_else(|error| error.into_inner()) == Phase::Open
    }
}

/// Which browser, navigation and document a scheme request belongs to. `browser` is `None`
/// for a request that came from no registered browser: the caller denies it.
#[derive(Clone)]
pub struct SchemeRequestContext {
    browser: Option<BrowserIdentity>,
    navigation: Option<NavigationIdentity>,
    document: Option<DocumentIdentity>,
    state: Arc<RequestState>,
    /// Keeps the cancellation watches alive for as long as the context is.
    _watches: Arc<Mutex<Vec<Box<dyn Any + Send>>>>,
}

impl SchemeRequestContext {
    /// At the top of every scheme handler: `webview_label` is `UriSchemeContext::webview_label()`.
    pub fn capture(native: &Native, webview_label: &str) -> Self {
        let state = RequestState::new();
        let browser = native.browser(webview_label);
        let mut watches: Vec<Box<dyn Any + Send>> = Vec::new();
        let (navigation, document) = match &browser {
            None => (None, None),
            Some(browser) => {
                let committed = browser.committed_core();
                let cancel = state.clone();
                match &committed {
                    // A document answers for the requests it issued: they end with it.
                    Some(core) => {
                        if let Ok(Some(revocation)) =
                            core.subscribe_or_deliver(Box::new(move |_| cancel.cancel()))
                        {
                            watches.push(Box::new(revocation));
                        }
                    }
                    // A hidden target root request, before commit: it ends with the browser.
                    None => match browser.when_closed(move |_| cancel.cancel()) {
                        Ok(subscription) => watches.push(Box::new(subscription)),
                        Err(_) => state.cancel(),
                    },
                }
                (
                    browser.navigation(),
                    committed.map(|core| core.identity.clone()),
                )
            }
        };
        Self {
            browser,
            navigation,
            document,
            state,
            _watches: Arc::new(Mutex::new(watches)),
        }
    }

    pub fn browser_id(&self) -> Option<super::BrowserId> {
        self.browser.as_ref().map(BrowserIdentity::id)
    }

    pub fn browser_state(&self) -> Option<&BrowserIdentity> {
        self.browser.as_ref()
    }

    pub fn navigation(&self) -> Option<&NavigationIdentity> {
        self.navigation.as_ref()
    }

    /// The committed document at intake; `None` before the first commit.
    pub fn document(&self) -> Option<&DocumentIdentity> {
        self.document.as_ref()
    }

    /// The registered, live document the request is attributed to (W1): the one that was
    /// committed at intake and still is.
    pub fn current_document_context(&self) -> Option<DocumentContext> {
        let context = self.browser.as_ref()?.registered_context()?;
        (Some(context.identity()) == self.document.as_ref()).then_some(context)
    }

    /// Observe the end of this request's document (or browser). The callback runs once,
    /// outside every lock.
    pub fn on_cancel(
        &self,
        callback: impl FnOnce() + Send + 'static,
    ) -> Result<RequestCancelSubscription, String> {
        self.state.subscribe(Box::new(callback))
    }

    /// The gate for answering this request, once.
    pub fn response_gate(&self) -> ResponseGate {
        ResponseGate {
            state: self.state.clone(),
        }
    }
}

/// One per request. Prepare outside every gate, accept inside the owner gate, submit outside.
#[derive(Clone)]
pub struct ResponseGate {
    state: Arc<RequestState>,
}

impl ResponseGate {
    /// Body ownership and header work belong here, outside every admission gate.
    pub fn prepare(&self, response: Answer) -> Result<Prepared, String> {
        if !self.state.is_open() {
            return Err("The original native request is no longer open.".into());
        }
        Ok(Prepared {
            state: self.state.clone(),
            response,
        })
    }
}

pub struct Prepared {
    state: Arc<RequestState>,
    response: Answer,
}

pub struct Accepted {
    state: Arc<RequestState>,
    response: Answer,
    responder: UriSchemeResponder,
}

pub struct Rejected {
    responder: UriSchemeResponder,
}

impl Rejected {
    pub fn into_responder(self) -> UriSchemeResponder {
        self.responder
    }
}

/// What happened to an accepted answer. Neither variant says the page displayed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// Given to the webview.
    Responded,
    /// The request was cancelled between acceptance and submission; nothing was sent.
    Cancelled,
}

impl Prepared {
    /// A bounded state transition only: the caller holds its independently captured document
    /// gate around this call. One shot.
    pub fn accept(self, responder: UriSchemeResponder) -> Result<Accepted, Rejected> {
        match self.state.accept() {
            Ok(()) => Ok(Accepted {
                state: self.state,
                response: self.response,
                responder,
            }),
            Err(_) => Err(Rejected { responder }),
        }
    }
}

impl Accepted {
    /// Runs outside all owner gates.
    pub fn submit(self) -> Submitted {
        if self.state.submit() {
            self.responder.respond(self.response);
            Submitted::Responded
        } else {
            Submitted::Cancelled
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::NativeSpec;
    use crate::native::browser::{
        BridgeMode,
        testing::{TEST_NONCE, url},
    };

    fn browser(native: &Native, label: &str) -> BrowserIdentity {
        let spec = NativeSpec::new().bridge(BridgeMode::Invoke);
        let (identity, _submission) = native.register(label, spec).unwrap();
        identity.core.claim_creation().unwrap();
        identity
    }

    fn counted() -> (Arc<Mutex<u32>>, impl FnOnce() + Send + 'static) {
        let count = Arc::new(Mutex::new(0));
        let hit = count.clone();
        (count, move || *hit.lock().unwrap() += 1)
    }

    #[test]
    fn a_request_without_a_registered_browser_has_no_authority() {
        let native = Native::default();
        let context = SchemeRequestContext::capture(&native, "nobody");
        assert!(context.browser_state().is_none());
        assert!(context.document().is_none());
        assert!(context.current_document_context().is_none());
    }

    #[test]
    fn a_request_is_attributed_to_the_document_committed_at_intake_and_ends_with_it() {
        let native = Native::default();
        let browser = browser(&native, "scheme-doc");
        let address = url("https://example.test/a");
        browser.core.admit_navigation(&address);
        browser.core.commit(&address);
        let registered = browser.register_document(TEST_NONCE).unwrap();

        let context = SchemeRequestContext::capture(&native, "scheme-doc");
        assert_eq!(context.document(), Some(registered.identity()));
        assert_eq!(context.current_document_context(), Some(registered.clone()));
        let (cancelled, callback) = counted();
        let _subscription = context.on_cancel(callback).unwrap();

        browser
            .core
            .admit_navigation(&url("https://example.test/b"));
        assert_eq!(*cancelled.lock().unwrap(), 1, "the issuing document ended");
        assert!(context.current_document_context().is_none());
        assert!(
            context
                .response_gate()
                .prepare(Response::new(Cow::Borrowed(&b""[..])))
                .is_err()
        );
    }

    #[test]
    fn a_hidden_target_request_before_commit_ends_with_its_browser() {
        let native = Native::default();
        let browser = browser(&native, "scheme-target");
        browser
            .core
            .admit_navigation(&url("html-asset://token/root.html"));
        let context = SchemeRequestContext::capture(&native, "scheme-target");
        assert!(context.document().is_none(), "nothing committed yet");
        assert!(
            context.navigation().is_some(),
            "the one admitted navigation"
        );
        let (cancelled, callback) = counted();
        let _subscription = context.on_cancel(callback).unwrap();
        browser.core.retire();
        assert_eq!(*cancelled.lock().unwrap(), 1);
    }

    #[test]
    fn a_subscription_to_an_already_cancelled_request_calls_back_and_a_dropped_one_does_not() {
        let native = Native::default();
        let browser = browser(&native, "scheme-late");
        browser.core.retire();
        let context = SchemeRequestContext::capture(&native, "scheme-late");
        assert!(
            context.browser_state().is_none(),
            "a retired browser is no registered browser"
        );

        let state = RequestState::new();
        let (kept, keep) = counted();
        let (dropped, drop_it) = counted();
        let _subscription = state.subscribe(Box::new(keep)).unwrap();
        drop(state.subscribe(Box::new(drop_it)).unwrap());
        state.cancel();
        state.cancel();
        assert_eq!(*kept.lock().unwrap(), 1);
        assert_eq!(*dropped.lock().unwrap(), 0);
        let (late, late_callback) = counted();
        let _late_subscription = state.subscribe(Box::new(late_callback)).unwrap();
        assert_eq!(*late.lock().unwrap(), 1);
    }

    #[test]
    fn the_gate_accepts_once_and_cancellation_wins_over_a_pending_answer() {
        let state = RequestState::new();
        assert!(state.accept().is_ok());
        assert!(state.accept().is_err(), "one shot");
        assert!(state.submit());
        assert!(!state.submit());

        let cancelled = RequestState::new();
        cancelled.cancel();
        assert!(cancelled.accept().is_err());

        let raced = RequestState::new();
        assert!(raced.accept().is_ok());
        raced.cancel();
        assert!(!raced.submit(), "nothing is sent for a cancelled request");
    }
}
