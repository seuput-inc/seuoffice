//! Shell-initiated navigations: the observed navigation receipt and the reload transaction.
//!
//! Reload transaction (one per browser, a newer one supersedes the older):
//!
//! ```text
//! Prepared --admission ok--> Dispatched --NavigationStarted(same address)--> Bound(nav)
//!   --DocumentCommitted(nav)--> Committed(doc) --DocumentRegistered(doc)--> Finished
//! any of: a second NavigationStarted before Registered, NavigationFailed, retirement,
//!         process failure, cancel(), refused admission, the predecessor lost before dispatch
//!   => Finished(Err), exactly one completion, the reservation cannot be reused
//! ```
//!
//! A page-initiated `location.reload()` has no identity: it revokes the predecessor at
//! navigation admission, so a reservation still `Prepared` fails and no arbitrary next
//! document can be accepted as the reload's successor.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tauri::Url;

use super::browser::{BrowserCore, BrowserIdentity};
use super::defer::defer;
use super::document::{DocumentContext, DocumentCore, DocumentIdentity, DocumentRevocation};
use super::events::{LoadError, RevokeReason};
use super::ids::{BrowserId, NavigationId};
use super::lifecycle::BrowserClose;

/// Opaque identity of one admitted main-frame navigation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NavigationIdentity {
    pub(crate) id: NavigationId,
    pub(crate) browser: BrowserId,
}

impl NavigationIdentity {
    pub fn id(&self) -> NavigationId {
        self.id
    }
}

/// How the first navigation of a browser starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NavigationStart {
    /// The webview loads its initial URL as built.
    #[default]
    Automatic,
    /// The webview is created at `about:blank`; observers attach, then the caller navigates.
    Explicit,
}

/// Shell-initiated navigation of one browser.
pub struct Navigator<'a>(&'a BrowserIdentity);

impl<'a> Navigator<'a> {
    pub(crate) fn new(browser: &'a BrowserIdentity) -> Self {
        Self(browser)
    }

    /// Completes when the webview is attached and, for an explicit start, its blank page
    /// committed.
    pub fn when_ready(&self, done: impl FnOnce(Result<(), String>) + Send + 'static) {
        self.0.core.when_ready(Box::new(done));
    }

    /// The point after which the caller may start the one controlled navigation of a hidden
    /// browser. Same readiness as [`Navigator::when_ready`].
    pub fn activate_initial(&self, done: impl FnOnce(Result<(), String>) + Send + 'static) {
        self.when_ready(done);
    }

    /// Navigate and receive the committed document once. Only one observed navigation can be
    /// pending per browser.
    pub fn navigate_observed(
        &self,
        url: Url,
        done: impl FnOnce(Result<ObservedDocument, String>) + Send + 'static,
    ) -> Result<ObservedNavigation, String> {
        let control = self.0.control().ok_or("The webview is not attached.")?;
        let observed = Observed::new(url.clone(), Box::new(done), Arc::downgrade(&self.0.core));
        self.0.core.install_observed(observed.clone())?;
        if let Err(error) = control.navigate(&url) {
            observed.abandon();
            return Err(error);
        }
        Ok(ObservedNavigation { pending: observed })
    }
}

// ---------------------------------------------------------------------------------------
// Observed navigation
// ---------------------------------------------------------------------------------------

/// An observation of one document, not authority: the shell's IPC admits nothing from it.
#[derive(Clone)]
pub struct ObservedDocument {
    document: Arc<DocumentCore>,
    url: Url,
}

pub struct ObservedDocumentSubscription {
    _revocation: Option<DocumentRevocation>,
    _document: Arc<DocumentCore>,
}

impl std::fmt::Debug for ObservedDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservedDocument")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl ObservedDocument {
    pub fn browser_state(&self) -> &BrowserIdentity {
        &self.document.browser
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn is_same_document(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.document, &other.document)
    }

    /// Called once when the document stops being live, with the reason. An already lost
    /// document calls back (deferred) at once.
    pub fn when_lost(
        &self,
        callback: impl FnOnce(RevokeReason) + Send + 'static,
    ) -> Result<ObservedDocumentSubscription, String> {
        let revocation = self.document.subscribe_or_deliver(Box::new(callback))?;
        Ok(ObservedDocumentSubscription {
            _revocation: revocation,
            _document: self.document.clone(),
        })
    }
}

type ObservedCompletion = Box<dyn FnOnce(Result<ObservedDocument, String>) + Send>;

enum ObservedPhase {
    Dispatched,
    Bound(NavigationId),
    Finished,
}

struct ObservedState {
    phase: ObservedPhase,
    completion: Option<ObservedCompletion>,
}

pub(crate) struct Observed {
    url: Url,
    core: Weak<BrowserCore>,
    state: Mutex<ObservedState>,
}

/// Cancelling the receipt does not stop a navigation the webview already started; the
/// owner closes the browser for that.
#[derive(Clone)]
pub struct ObservedNavigation {
    pending: Arc<Observed>,
}

impl ObservedNavigation {
    pub fn cancel(&self) {
        self.pending
            .finish(Err("Observed navigation was cancelled.".into()));
    }
}

impl Observed {
    fn new(url: Url, completion: ObservedCompletion, core: Weak<BrowserCore>) -> Arc<Self> {
        Arc::new(Self {
            url,
            core,
            state: Mutex::new(ObservedState {
                phase: ObservedPhase::Dispatched,
                completion: Some(completion),
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ObservedState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn is_finished(&self) -> bool {
        matches!(self.lock().phase, ObservedPhase::Finished)
    }

    /// Finish without calling the completion (the caller reports the error itself).
    fn abandon(self: &Arc<Self>) {
        let completion = {
            let mut state = self.lock();
            state.phase = ObservedPhase::Finished;
            state.completion.take()
        };
        drop(completion);
        if let Some(core) = self.core.upgrade() {
            core.clear_observed(self);
        }
    }

    fn finish(self: &Arc<Self>, result: Result<ObservedDocument, String>) {
        let completion = {
            let mut state = self.lock();
            if matches!(state.phase, ObservedPhase::Finished) {
                return;
            }
            state.phase = ObservedPhase::Finished;
            state.completion.take()
        };
        if let Some(core) = self.core.upgrade() {
            core.clear_observed(self);
        }
        if let Some(completion) = completion {
            defer(move || completion(result));
        }
    }

    pub(crate) fn on_navigation_started(
        self: &Arc<Self>,
        navigation: &NavigationIdentity,
        url: &Url,
    ) {
        let failure = {
            let mut state = self.lock();
            match state.phase {
                ObservedPhase::Dispatched if url.as_str() == self.url.as_str() => {
                    state.phase = ObservedPhase::Bound(navigation.id);
                    None
                }
                ObservedPhase::Dispatched => {
                    Some("A different navigation started before the observed one.")
                }
                ObservedPhase::Bound(_) => {
                    Some("A second navigation started before the observed one committed.")
                }
                ObservedPhase::Finished => None,
            }
        };
        if let Some(failure) = failure {
            self.finish(Err(failure.into()));
        }
    }

    pub(crate) fn on_committed(
        self: &Arc<Self>,
        navigation: &NavigationIdentity,
        document: &Arc<DocumentCore>,
        url: &Url,
    ) {
        let accepted = {
            let state = self.lock();
            match state.phase {
                ObservedPhase::Bound(bound) => Some(bound == navigation.id),
                ObservedPhase::Dispatched => Some(url.as_str() == self.url.as_str()),
                ObservedPhase::Finished => None,
            }
        };
        match accepted {
            Some(true) => {
                self.finish(Ok(ObservedDocument {
                    document: document.clone(),
                    url: url.clone(),
                }));
            }
            Some(false) => self.finish(Err("A different document committed.".into())),
            None => {}
        }
    }

    pub(crate) fn on_failed(self: &Arc<Self>, navigation: &NavigationIdentity, error: &LoadError) {
        let affected = match self.lock().phase {
            ObservedPhase::Bound(bound) => bound == navigation.id,
            ObservedPhase::Dispatched => true,
            ObservedPhase::Finished => false,
        };
        if affected {
            self.finish(Err(format!("Observed navigation failed: {error}.")));
        }
    }

    /// The shell policy refused a navigation to `url`.
    pub(crate) fn on_refused(self: &Arc<Self>, url: &Url) {
        let affected = matches!(self.lock().phase, ObservedPhase::Dispatched)
            && url.as_str() == self.url.as_str();
        if affected {
            self.finish(Err("The shell refused the observed navigation.".into()));
        }
    }

    pub(crate) fn on_gone(self: &Arc<Self>, reason: &str) {
        self.finish(Err(reason.to_owned()));
    }
}

// ---------------------------------------------------------------------------------------
// Reload transaction
// ---------------------------------------------------------------------------------------

type ReloadCompletion = Box<dyn FnOnce(Result<DocumentContext, String>) + Send>;

/// Runs after the owner reducer decided and before the reload is dispatched; may refuse.
pub type ReloadAdmission = Box<dyn FnOnce() -> Result<(), String> + Send>;

enum ReloadPhase {
    Prepared,
    Dispatched,
    Bound(NavigationId),
    Committed(DocumentIdentity),
    Finished,
}

type FailureCallback = Box<dyn FnOnce(String) + Send>;

struct FailureListener {
    active: AtomicBool,
    callback: Mutex<Option<FailureCallback>>,
}

pub struct ReloadFailureSubscription {
    listener: Arc<FailureListener>,
}

impl Drop for ReloadFailureSubscription {
    fn drop(&mut self) {
        self.listener.active.store(false, Ordering::Release);
    }
}

fn deliver_failure(listener: Arc<FailureListener>, error: String) {
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
            callback(error);
        }
    });
}

struct ReloadState {
    phase: ReloadPhase,
    completion: Option<ReloadCompletion>,
    failure: Option<String>,
    listeners: Vec<Arc<FailureListener>>,
    successor: Option<DocumentIdentity>,
}

pub(crate) struct Reload {
    core: Weak<BrowserCore>,
    browser: BrowserId,
    predecessor: DocumentIdentity,
    url: Url,
    handles: AtomicUsize,
    state: Mutex<ReloadState>,
}

/// Retain until completion. Cloning shares the transaction; dropping the last handle fails
/// it. The approved intent is never exposed to JS.
pub struct ReloadReservation(Arc<Reload>);

#[derive(Clone)]
pub struct ReloadIdentity(Weak<Reload>);

impl ReloadIdentity {
    /// True for the successor registered by THIS transaction.
    pub fn matches_registered(&self, context: &DocumentContext) -> bool {
        self.0
            .upgrade()
            .is_some_and(|reload| reload.matches(context))
    }
}

impl Clone for ReloadReservation {
    fn clone(&self) -> Self {
        self.0.handles.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}

impl Drop for ReloadReservation {
    fn drop(&mut self) {
        if self.0.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0
                .finish(Err("Native reload reservation was dropped.".into()));
        }
    }
}

impl std::fmt::Debug for ReloadReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadReservation").finish_non_exhaustive()
    }
}

impl Reload {
    fn lock(&self) -> std::sync::MutexGuard<'_, ReloadState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn matches(&self, context: &DocumentContext) -> bool {
        let candidate = context.identity();
        if context.browser().id() != self.browser || *candidate == self.predecessor {
            return false;
        }
        let state = self.lock();
        match &state.phase {
            ReloadPhase::Committed(document) => document == candidate,
            ReloadPhase::Finished => state.successor.as_ref() == Some(candidate),
            ReloadPhase::Prepared | ReloadPhase::Dispatched | ReloadPhase::Bound(_) => false,
        }
    }

    pub(crate) fn finish(self: &Arc<Self>, result: Result<DocumentContext, String>) {
        let (completion, listeners, failure) = {
            let mut state = self.lock();
            if matches!(state.phase, ReloadPhase::Finished) {
                return;
            }
            state.phase = ReloadPhase::Finished;
            match &result {
                Ok(context) => state.successor = Some(context.identity().clone()),
                Err(error) => state.failure = Some(error.clone()),
            }
            (
                state.completion.take(),
                std::mem::take(&mut state.listeners),
                state.failure.clone(),
            )
        };
        if let Some(core) = self.core.upgrade() {
            core.clear_reload(self);
        }
        if let Some(error) = failure {
            for listener in listeners {
                deliver_failure(listener, error.clone());
            }
        }
        if let Some(completion) = completion {
            defer(move || {
                completion(result.and_then(|context| {
                    if context.is_live() {
                        Ok(context)
                    } else {
                        Err("Reloaded native document was released before completion.".into())
                    }
                }));
            });
        }
    }

    fn fail(self: &Arc<Self>, reason: &str) {
        self.finish(Err(reason.to_owned()));
    }

    /// The predecessor ended before the reload was dispatched: a page-initiated navigation
    /// won the race, and the successor of an arbitrary navigation is not a reload.
    pub(crate) fn on_document_revoked(self: &Arc<Self>, document: &DocumentIdentity) {
        let affected =
            matches!(self.lock().phase, ReloadPhase::Prepared) && *document == self.predecessor;
        if affected {
            self.fail("Original native reload document was replaced before dispatch.");
        }
    }

    pub(crate) fn on_navigation_started(
        self: &Arc<Self>,
        navigation: &NavigationIdentity,
        url: &Url,
    ) {
        let failure = {
            let mut state = self.lock();
            match state.phase {
                ReloadPhase::Dispatched if url.as_str() == self.url.as_str() => {
                    state.phase = ReloadPhase::Bound(navigation.id);
                    None
                }
                ReloadPhase::Dispatched => {
                    Some("The dispatched reload became a different navigation.")
                }
                ReloadPhase::Prepared => {
                    Some("A navigation replaced the reload document before dispatch.")
                }
                ReloadPhase::Bound(_) | ReloadPhase::Committed(_) => {
                    Some("A second navigation started before the reload registered.")
                }
                ReloadPhase::Finished => None,
            }
        };
        if let Some(failure) = failure {
            self.fail(failure);
        }
    }

    pub(crate) fn on_committed(
        self: &Arc<Self>,
        navigation: &NavigationIdentity,
        document: &DocumentIdentity,
        url: &Url,
    ) {
        enum Step {
            Commit,
            Fail(&'static str),
            Ignore,
        }
        let step = {
            let state = self.lock();
            match &state.phase {
                ReloadPhase::Bound(bound) if *bound == navigation.id => Step::Commit,
                ReloadPhase::Dispatched if url.as_str() == self.url.as_str() => Step::Commit,
                ReloadPhase::Dispatched | ReloadPhase::Bound(_) => {
                    Step::Fail("A different document committed during the reload.")
                }
                ReloadPhase::Prepared | ReloadPhase::Committed(_) | ReloadPhase::Finished => {
                    Step::Ignore
                }
            }
        };
        match step {
            Step::Commit => {
                let mut state = self.lock();
                if matches!(state.phase, ReloadPhase::Bound(_) | ReloadPhase::Dispatched) {
                    state.phase = ReloadPhase::Committed(document.clone());
                }
            }
            Step::Fail(reason) => self.fail(reason),
            Step::Ignore => {}
        }
    }

    pub(crate) fn on_registered(self: &Arc<Self>, context: &DocumentContext) {
        enum Next {
            Finish,
            Fail,
            Ignore,
        }
        let next = match &self.lock().phase {
            ReloadPhase::Committed(document) if document == context.identity() => Next::Finish,
            ReloadPhase::Dispatched | ReloadPhase::Bound(_) | ReloadPhase::Committed(_) => {
                Next::Fail
            }
            ReloadPhase::Prepared | ReloadPhase::Finished => Next::Ignore,
        };
        match next {
            Next::Finish => self.finish(Ok(context.clone())),
            Next::Fail => self.fail("The reload was replaced by a different approved document."),
            Next::Ignore => {}
        }
    }

    pub(crate) fn on_failed(self: &Arc<Self>, navigation: &NavigationIdentity) {
        let affected = match &self.lock().phase {
            ReloadPhase::Dispatched => true,
            ReloadPhase::Bound(bound) => *bound == navigation.id,
            ReloadPhase::Committed(document) => document.navigation == navigation.id,
            ReloadPhase::Prepared | ReloadPhase::Finished => false,
        };
        if affected {
            self.fail("The reloaded document failed to load.");
        }
    }

    pub(crate) fn on_refused(self: &Arc<Self>, url: &Url) {
        let affected = matches!(self.lock().phase, ReloadPhase::Dispatched)
            && url.as_str() == self.url.as_str();
        if affected {
            self.fail("The shell refused the reload navigation.");
        }
    }

    pub(crate) fn on_gone(self: &Arc<Self>, reason: &str) {
        self.fail(reason);
    }
}

impl ReloadReservation {
    /// Native UI only, outside owner gates. Reserves the transaction; performs no reload and
    /// grants a successor no authority. A newer reservation supersedes an older one.
    pub fn prepare(predecessor: &DocumentContext) -> Result<Self, String> {
        let browser = predecessor.browser();
        if !predecessor.is_live() || predecessor.current_document().is_none() {
            return Err("Original native reload document is not committed and current.".into());
        }
        let reload = Arc::new(Reload {
            core: Arc::downgrade(&browser.core),
            browser: browser.id(),
            predecessor: predecessor.identity().clone(),
            url: predecessor.url().clone(),
            handles: AtomicUsize::new(1),
            state: Mutex::new(ReloadState {
                phase: ReloadPhase::Prepared,
                completion: None,
                failure: None,
                listeners: Vec::new(),
                successor: None,
            }),
        });
        if let Some(previous) = browser.core.install_reload(reload.clone()) {
            previous.fail("Native reload was superseded before completion.");
        }
        Ok(Self(reload))
    }

    pub fn identity(&self) -> ReloadIdentity {
        ReloadIdentity(Arc::downgrade(&self.0))
    }

    pub fn belongs_to(&self, browser: &BrowserIdentity) -> bool {
        self.0.browser == browser.id()
    }

    /// The receipt of the browser being reloaded; `None` once the browser is gone.
    pub fn browser_close(&self) -> Option<BrowserClose> {
        self.0
            .core
            .upgrade()
            .map(|core| core.close_receipt().clone())
    }

    pub fn on_failure(
        &self,
        callback: impl FnOnce(String) + Send + 'static,
    ) -> Result<ReloadFailureSubscription, String> {
        let listener = Arc::new(FailureListener {
            active: AtomicBool::new(true),
            callback: Mutex::new(Some(Box::new(callback))),
        });
        let failure = {
            let mut state = self.0.lock();
            if !matches!(state.phase, ReloadPhase::Finished) {
                state.listeners.push(listener.clone());
            } else if state.failure.is_none() {
                return Err("Native reload has already completed.".into());
            }
            state.failure.clone()
        };
        if let Some(error) = failure {
            deliver_failure(listener.clone(), error);
        }
        Ok(ReloadFailureSubscription { listener })
    }

    pub fn cancel(&self) {
        self.0.fail("Native reload was cancelled.");
    }

    /// Dispatch the approved reload. The completion runs once with the registered successor
    /// or the reason the transaction failed.
    pub(crate) fn dispatch(
        &self,
        browser: &BrowserIdentity,
        admission: ReloadAdmission,
        completion: ReloadCompletion,
    ) {
        let reload = &self.0;
        {
            let mut state = reload.lock();
            if !matches!(state.phase, ReloadPhase::Prepared) {
                drop(state);
                defer(move || {
                    completion(Err(
                        "Native reload was already dispatched or cancelled.".into()
                    ))
                });
                return;
            }
            state.completion = Some(completion);
        }
        let current = browser.id() == reload.browser
            && browser.document().as_ref() == Some(&reload.predecessor);
        if !current {
            reload.fail("Original native reload document was replaced.");
            return;
        }
        let Some(control) = browser.control() else {
            reload.fail("Original native reload browser is unavailable.");
            return;
        };
        if let Err(error) = admission() {
            reload.fail(&error);
            return;
        }
        {
            let mut state = reload.lock();
            if !matches!(state.phase, ReloadPhase::Prepared) {
                return;
            }
            state.phase = ReloadPhase::Dispatched;
        }
        if let Err(error) = control.reload() {
            reload.fail(&error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::browser::testing::{
        FakeControl, TEST_NONCE, browser_with_events, registered_context_with_control, url,
    };

    type Results<T> = Arc<Mutex<Vec<Result<T, String>>>>;

    fn recorder<T: Send + 'static>() -> (Results<T>, impl FnOnce(Result<T, String>) + Send + 'static)
    {
        let results: Results<T> = Arc::default();
        let sink = results.clone();
        (results, move |result| sink.lock().unwrap().push(result))
    }

    fn page() -> Url {
        url("https://example.test/a")
    }

    fn prepared(
        label: &str,
    ) -> (
        Arc<FakeControl>,
        BrowserIdentity,
        DocumentContext,
        ReloadReservation,
    ) {
        let control = FakeControl::new();
        let (browser, context) = registered_context_with_control(label, control.clone());
        let reservation = ReloadReservation::prepare(&context).unwrap();
        (control, browser, context, reservation)
    }

    fn dispatch_ok(
        reservation: &ReloadReservation,
        browser: &BrowserIdentity,
    ) -> Results<DocumentContext> {
        let (results, completion) = recorder();
        reservation.dispatch(browser, Box::new(|| Ok(())), Box::new(completion));
        results
    }

    #[test]
    fn a_reload_finishes_with_the_successor_registered_by_its_own_navigation() {
        let (control, browser, old, reservation) = prepared("reload-ok");
        let identity = reservation.identity();
        let results = dispatch_ok(&reservation, &browser);
        assert_eq!(*control.reloads.lock().unwrap(), 1);

        assert!(browser.core.admit_navigation(&page()));
        assert!(
            old.is_revoked(),
            "the predecessor ends when the reload navigation is admitted"
        );
        assert!(
            results.lock().unwrap().is_empty(),
            "the predecessor's loss is not a failure"
        );
        browser.core.commit(&page());
        assert!(
            results.lock().unwrap().is_empty(),
            "committed but not registered yet"
        );
        let successor = browser.core.register_document(TEST_NONCE).unwrap();

        let results = results.lock().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap(), &successor);
        assert!(identity.matches_registered(&successor));
        assert!(!identity.matches_registered(&old));
    }

    #[test]
    fn a_newer_reservation_supersedes_the_older_one() {
        let (_control, browser, context, first) = prepared("reload-supersede");
        let (failures, on_failure) = recorder::<()>();
        let _subscription = first
            .on_failure(move |reason| on_failure(Err(reason)))
            .unwrap();
        let second = ReloadReservation::prepare(&context).unwrap();
        assert_eq!(failures.lock().unwrap().len(), 1);
        assert!(
            failures.lock().unwrap()[0]
                .as_ref()
                .unwrap_err()
                .contains("superseded")
        );

        let stale = dispatch_ok(&first, &browser);
        assert!(
            stale.lock().unwrap()[0].is_err(),
            "a superseded reservation cannot be dispatched"
        );
        let live = dispatch_ok(&second, &browser);
        assert!(
            live.lock().unwrap().is_empty(),
            "the newer reservation is still in flight"
        );
    }

    #[test]
    fn a_page_initiated_navigation_before_dispatch_fails_the_reservation() {
        let (control, browser, _context, reservation) = prepared("reload-raced");
        assert!(
            browser
                .core
                .admit_navigation(&url("https://example.test/elsewhere"))
        );
        let results = dispatch_ok(&reservation, &browser);
        assert!(results.lock().unwrap()[0].is_err());
        assert_eq!(
            *control.reloads.lock().unwrap(),
            0,
            "no reload was dispatched"
        );
    }

    #[test]
    fn a_second_navigation_before_registration_fails_the_reload_once() {
        let (_control, browser, _old, reservation) = prepared("reload-second");
        let results = dispatch_ok(&reservation, &browser);
        assert!(browser.core.admit_navigation(&page()));
        assert!(browser.core.admit_navigation(&page()));
        browser.core.commit(&page());
        browser.core.register_document(TEST_NONCE).unwrap();
        let results = results.lock().unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[test]
    fn refused_admission_and_refused_dispatch_report_the_reason() {
        let (control, browser, _old, reservation) = prepared("reload-refused");
        let (results, completion) = recorder::<DocumentContext>();
        reservation.dispatch(
            &browser,
            Box::new(|| Err("the owner moved on".into())),
            Box::new(completion),
        );
        assert_eq!(
            results.lock().unwrap()[0].as_ref().unwrap_err(),
            "the owner moved on"
        );
        assert_eq!(*control.reloads.lock().unwrap(), 0);

        let (control, browser, _old, reservation) = prepared("reload-webview-refused");
        control.fail_reload.store(true, Ordering::Release);
        let results = dispatch_ok(&reservation, &browser);
        assert_eq!(
            results.lock().unwrap()[0].as_ref().unwrap_err(),
            "reload refused"
        );
    }

    #[test]
    fn retirement_and_a_load_failure_fail_an_in_flight_reload() {
        let (_control, browser, _old, reservation) = prepared("reload-failure");
        let results = dispatch_ok(&reservation, &browser);
        assert!(browser.core.admit_navigation(&page()));
        browser.core.finish_load(Err(LoadError {
            code: Some(-3),
            description: "offline".into(),
        }));
        assert!(results.lock().unwrap()[0].is_err());

        let (_control, browser, _old, reservation) = prepared("reload-retired");
        let results = dispatch_ok(&reservation, &browser);
        browser.core.retire();
        assert!(results.lock().unwrap()[0].is_err());
    }

    #[test]
    fn the_last_dropped_handle_fails_the_transaction_and_a_clone_keeps_it_alive() {
        let (_control, _browser, _context, reservation) = prepared("reload-handles");
        let (failures, on_failure) = recorder::<()>();
        let _subscription = reservation
            .on_failure(move |reason| on_failure(Err(reason)))
            .unwrap();
        let clone = reservation.clone();
        drop(reservation);
        assert!(failures.lock().unwrap().is_empty());
        drop(clone);
        assert_eq!(failures.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_observed_navigation_completes_with_the_committed_document_and_reports_its_loss() {
        let control = FakeControl::new();
        let (browser, _events) =
            browser_with_events("observed", control.clone(), NavigationStart::Automatic);
        let (results, done) = recorder::<ObservedDocument>();
        let _receipt = browser.navigator().navigate_observed(page(), done).unwrap();
        assert_eq!(*control.navigations.lock().unwrap(), [page().to_string()]);
        assert!(
            browser
                .navigator()
                .navigate_observed(page(), |_| ())
                .is_err(),
            "one observed navigation at a time"
        );

        assert!(browser.core.admit_navigation(&page()));
        browser.core.commit(&page());
        let observed = results.lock().unwrap().pop().unwrap().unwrap();
        assert_eq!(observed.url(), &page());
        assert!(observed.browser_state().is_same_browser(&browser));

        let lost = Arc::new(Mutex::new(Vec::new()));
        let sink = lost.clone();
        let _subscription = observed
            .when_lost(move |reason| sink.lock().unwrap().push(reason))
            .unwrap();
        assert!(
            browser
                .core
                .admit_navigation(&url("https://example.test/next"))
        );
        assert_eq!(*lost.lock().unwrap(), [RevokeReason::Navigation]);

        let late = Arc::new(Mutex::new(Vec::new()));
        let sink = late.clone();
        let _already = observed
            .when_lost(move |reason| sink.lock().unwrap().push(reason))
            .unwrap();
        assert_eq!(*late.lock().unwrap(), [RevokeReason::Navigation]);
    }

    #[test]
    fn an_observed_navigation_fails_on_a_load_failure_a_refusal_or_cancel() {
        let control = FakeControl::new();
        let (browser, _events) =
            browser_with_events("observed-fail", control.clone(), NavigationStart::Automatic);
        let (results, done) = recorder::<ObservedDocument>();
        browser.navigator().navigate_observed(page(), done).unwrap();
        browser.core.navigation_refused(&page());
        assert!(
            results.lock().unwrap()[0]
                .as_ref()
                .unwrap_err()
                .contains("refused")
        );

        let (results, done) = recorder::<ObservedDocument>();
        let receipt = browser.navigator().navigate_observed(page(), done).unwrap();
        receipt.cancel();
        receipt.cancel();
        assert_eq!(results.lock().unwrap().len(), 1);

        let (results, done) = recorder::<ObservedDocument>();
        browser.navigator().navigate_observed(page(), done).unwrap();
        assert!(browser.core.admit_navigation(&page()));
        browser.core.finish_load(Err(LoadError {
            code: None,
            description: "dns".into(),
        }));
        assert!(results.lock().unwrap()[0].is_err());
    }
}
