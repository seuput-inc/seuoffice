//! One native browser: its identity, its latest navigation and its committed document.
//!
//! Every transition is the same shape: change state under the browser lock, collect what
//! the change owes, release the lock, then announce through `defer` (events, receipts,
//! callbacks). Nothing user-visible runs under a lock.
//!
//! Lock order: browser state, then document state, then the short locks of reload and
//! observed navigations. Those are only taken after the browser lock was released.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde_json::Value;
use tauri::Url;

use super::defer::{defer, hold};
use super::document::{DocumentContext, DocumentCore, DocumentIdentity, RegisterOutcome, Revoked};
use super::events::{
    EventSink, LoadError, NativeEvent, NativeEventKind, ProcessFailure, RevokeReason, ViewEvent,
};
use super::ids::{BrowserId, CreationId, DocumentId, NavigationId};
use super::ingress::NONCE_LEN;
use super::lifecycle::{BrowserClose, BrowserCloseOutcome, BrowserCloseSubscription};
use super::navigation::{NavigationIdentity, NavigationStart, Navigator, Observed, Reload};
use super::policy::RequestPolicy;

/// What the native layer needs from one live webview. Implemented over the Tauri webview
/// in `native::ext`; tests use a fake. All methods may be called from any thread.
pub trait WebviewControl: Send + Sync {
    fn navigate(&self, url: &Url) -> Result<(), String>;
    fn reload(&self) -> Result<(), String>;
    /// `Webview::close()`; the caller observes the label leaving Tauri's registry.
    fn close(&self) -> Result<(), String>;
    /// Evaluate with promises awaited; the answer is the JSON value of the result.
    fn evaluate(
        &self, script: String, done: Box<dyn FnOnce(Result<Value, String>) + Send>,
    ) -> Result<(), String>;
    /// Run a script and ignore its value (the outbox wake).
    fn run(&self, script: String) -> Result<(), String>;
}

/// What the bridge in a browser's documents may do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BridgeMode {
    /// No bridge: documents cannot register, so no IPC is ever admitted from them.
    #[default]
    None,
    /// Documents register their nonce and call IPC through Tauri's invoke.
    Invoke,
    /// As `Invoke`, plus the synchronous `seuoffice-ipc` XHR channel (`sendSync`).
    Preload,
}

/// What a browser is built with. Immutable for its lifetime.
#[derive(Clone, Default)]
pub(crate) struct Config {
    pub(crate) sink: Option<EventSink>,
    pub(crate) start: NavigationStart,
    pub(crate) bridge: BridgeMode,
    pub(crate) policy: Option<RequestPolicy>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// The browser has no committed document to register for.
    NoDocument,
    Revoked,
    /// A different nonce was already registered: the document was revoked.
    Conflict,
    /// The browser was built without a bridge.
    BridgeDisabled,
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoDocument => "the browser has no committed document",
            Self::Revoked => "the document was revoked",
            Self::Conflict => "a different document nonce was already registered",
            Self::BridgeDisabled => "the browser has no bridge",
        })
    }
}

/// What the navigation hook does with a navigation, by whether the policy is enforced yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BeforePolicy {
    /// The policy is enforced (or there is none): judge the navigation normally.
    Judge,
    /// `about:blank` fetches nothing: allow it without a verdict.
    Blank,
    /// Cancel it; it replays once the policy is enforced.
    Hold,
}

pub(crate) type RegistrationHook = Arc<dyn Fn(&DocumentContext) + Send + Sync>;
type ReadyWaiter = Box<dyn FnOnce(Result<(), String>) + Send>;

enum DocSlot {
    None,
    Committed(Arc<DocumentCore>),
    /// Revoked by process failure or conflict; the next commit starts afresh.
    Lost,
}

struct BrowserLive {
    closed: bool,
    /// Latest admitted cross-document navigation (never replaced by a fragment navigation).
    nav: Option<NavigationIdentity>,
    nav_url: Option<Url>,
    /// Address of the committed document.
    url: Option<Url>,
    doc: DocSlot,
    observation: u64,
    attached: bool,
    initial_loaded: bool,
    /// The request policy is enforced by the OS webview (true from the start without one).
    policy_ready: bool,
    /// The first navigation to a real URL, cancelled until the policy was enforced.
    held_navigation: Option<Url>,
    ready: Option<Result<(), String>>,
    ready_waiters: Vec<ReadyWaiter>,
    /// The navigation whose load outcome was already reported.
    finished: Option<NavigationId>,
    observed: Option<Arc<Observed>>,
    reload: Option<Arc<Reload>>,
}

pub(crate) struct BrowserCore {
    pub(crate) id: BrowserId,
    label: Box<str>,
    creation: CreationId,
    close: BrowserClose,
    config: Config,
    /// Dropped at retirement: the webview and its hooks hold this core, so the control must
    /// not keep the webview alive after the browser is gone.
    control: Mutex<Option<Arc<dyn WebviewControl>>>,
    window_label: OnceLock<Box<str>>,
    on_registered: OnceLock<RegistrationHook>,
    state: Mutex<BrowserLive>,
}

/// Opaque, cloneable identity of one native browser lifetime.
#[derive(Clone)]
pub struct BrowserIdentity {
    pub(crate) id: BrowserId,
    pub(crate) core: Arc<BrowserCore>,
}

/// The name 20+ call sites keep during the migration wave; removed in the final cutover.
pub type BrowserState = BrowserIdentity;

impl PartialEq for BrowserIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.core, &other.core)
    }
}

impl Eq for BrowserIdentity {}

impl fmt::Debug for BrowserIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserIdentity").field("id", &self.id).field("label", &self.core.label).finish()
    }
}

impl BrowserIdentity {
    pub fn id(&self) -> BrowserId {
        self.id
    }

    /// Routing only, never identity: a label can be reused by a later browser.
    pub fn label(&self) -> &str {
        &self.core.label
    }

    pub fn creation_id(&self) -> CreationId {
        self.core.creation
    }

    /// A replacement browser never matches, even under a reused label.
    pub fn is_same_browser(&self, other: &Self) -> bool {
        self == other
    }

    /// Admit a short, non-reentrant effect for this browser's lifetime. Browser close
    /// serializes with this gate; navigation does not revoke it.
    pub fn with_live_browser<T>(&self, effect: impl FnOnce() -> T) -> Option<T> {
        let _hold = hold();
        let state = self.core.lock();
        if state.closed {
            return None;
        }
        Some(effect())
    }

    pub fn is_open(&self) -> bool {
        !self.core.lock().closed
    }

    /// The latest admitted main-frame navigation.
    pub fn navigation(&self) -> Option<NavigationIdentity> {
        let state = self.core.lock();
        if state.closed { None } else { state.nav.clone() }
    }

    /// The committed document, registered or not.
    pub fn document(&self) -> Option<DocumentIdentity> {
        let state = self.core.lock();
        match &state.doc {
            DocSlot::Committed(doc) if !state.closed => Some(doc.identity.clone()),
            _ => None,
        }
    }

    /// The committed document when its bridge registered and it is not revoked.
    pub fn registered_context(&self) -> Option<DocumentContext> {
        let committed = {
            let state = self.core.lock();
            match &state.doc {
                DocSlot::Committed(doc) if !state.closed => Some(DocumentContext(doc.clone())),
                _ => None,
            }
        };
        committed.filter(|context| context.is_registered() && context.is_live())
    }

    pub fn url(&self) -> Option<Url> {
        self.core.lock().url.clone()
    }

    /// Advances on navigation, commit, address and load changes. Compared only for
    /// staleness, never for authority.
    pub fn observation(&self) -> u64 {
        self.core.lock().observation
    }

    pub fn when_closed(
        &self, callback: impl FnOnce(BrowserCloseOutcome) + Send + 'static,
    ) -> Result<BrowserCloseSubscription, String> {
        self.core.close.when_closed(callback)
    }

    pub fn close_receipt(&self) -> BrowserClose {
        self.core.close.clone()
    }

    /// One-shot hook run (before the event sink) when a document registers. The shell binds
    /// its view's creation epoch here.
    pub fn bind_registration(
        &self, hook: impl Fn(&DocumentContext) + Send + Sync + 'static,
    ) -> Result<(), String> {
        self.core
            .on_registered
            .set(Arc::new(hook))
            .map_err(|_| "The browser already has a registration hook.".to_owned())
    }

    pub fn navigator(&self) -> Navigator<'_> {
        Navigator::new(self)
    }

    pub(crate) fn control(&self) -> Option<Arc<dyn WebviewControl>> {
        self.core.control()
    }

    pub fn bridge_mode(&self) -> BridgeMode {
        self.core.config.bridge
    }

    pub(crate) fn policy(&self) -> Option<&RequestPolicy> {
        self.core.config.policy.as_ref()
    }

    /// The committed document's core, registered or not (scheme grants watch it).
    pub(crate) fn committed_core(&self) -> Option<Arc<DocumentCore>> {
        let state = self.core.lock();
        match &state.doc {
            DocSlot::Committed(doc) if !state.closed => Some(doc.clone()),
            _ => None,
        }
    }

    /// The shell's own scheme handler reports a main document answered with an error status
    /// (WKWebView reports such loads as successful).
    pub fn report_navigation_failure(&self, error: LoadError) {
        self.core.finish_load(Err(error));
    }
}

/// What the OS adapters (and `tauri_public`) report into the browser. Each call is one
/// observed fact; the browser derives identities, revocation and events from it.
impl BrowserIdentity {
    /// A main-frame navigation passed the shell policy and is about to start. Returns false
    /// when it must be cancelled.
    pub(crate) fn observe_navigation_allowed(&self, url: &Url) -> bool {
        self.core.admit_navigation(url)
    }

    /// The shell policy refused a main-frame navigation.
    pub(crate) fn observe_navigation_refused(&self, url: &Url) {
        self.core.navigation_refused(url);
    }

    pub(crate) fn observe_commit(&self, url: &Url) {
        self.core.commit(url);
    }

    /// The load of the latest navigation ended.
    pub(crate) fn observe_load(&self, outcome: Result<(), LoadError>) {
        self.core.finish_load(outcome);
    }

    /// History-API address change.
    pub(crate) fn observe_address(&self, url: &Url) {
        self.core.address_changed(url);
    }

    pub(crate) fn observe_process_failed(&self, kind: ProcessFailure) {
        self.core.process_failed(kind);
    }

    pub(crate) fn observe_view(&self, event: ViewEvent) {
        self.core.view_event(event);
    }

    /// The OS webview enforces the browser's request policy (see `PolicyOps::apply_policy`).
    pub(crate) fn observe_policy_installed(&self) {
        self.core.policy_installed();
    }

    /// The policy could not be enforced after `apply_policy` returned (macOS compiles its
    /// rules asynchronously): the held navigation never replays and readiness fails.
    pub(crate) fn observe_policy_failed(&self, reason: &str) {
        self.core.attach_failed(reason);
    }

    /// The webview left Tauri's registry.
    pub(crate) fn observe_retired(&self) {
        self.core.retire();
    }

    /// The bridge registered its nonce for the committed document.
    pub fn register_document(&self, nonce: [u8; NONCE_LEN]) -> Result<DocumentContext, RegisterError> {
        self.core.register_document(nonce)
    }

    /// The bridge reported render-ready for the document it registered.
    pub fn render_ready(&self, nonce: &[u8; NONCE_LEN]) -> Result<DocumentContext, RegisterError> {
        self.core.render_ready(nonce)
    }

    /// The registered, live document whose nonce is `nonce`.
    pub fn redeem(&self, nonce: &[u8; NONCE_LEN]) -> Option<DocumentContext> {
        self.core.redeem(nonce)
    }
}

/// A fragment-only navigation inside the current document keeps its JS realm.
pub(crate) fn is_same_document_navigation(current: &Url, next: &Url) -> bool {
    fn bare(url: &Url) -> Url {
        let mut url = url.clone();
        url.set_fragment(None);
        url
    }
    next.fragment().is_some() && bare(current) == bare(next)
}

impl BrowserCore {
    pub(crate) fn new(
        label: &str, creation: CreationId, close: BrowserClose, config: Config,
    ) -> Option<Arc<Self>> {
        let config_without_policy = config.policy.is_none();
        Some(Arc::new(Self {
            id: BrowserId::mint()?,
            label: label.into(),
            creation,
            close,
            config,
            control: Mutex::new(None),
            window_label: OnceLock::new(),
            on_registered: OnceLock::new(),
            state: Mutex::new(BrowserLive {
                closed: false,
                nav: None,
                nav_url: None,
                url: None,
                doc: DocSlot::None,
                observation: 0,
                attached: false,
                initial_loaded: false,
                policy_ready: config_without_policy,
                held_navigation: None,
                ready: None,
                ready_waiters: Vec::new(),
                finished: None,
                observed: None,
                reload: None,
            }),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, BrowserLive> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn identity(self: &Arc<Self>) -> BrowserIdentity {
        BrowserIdentity { id: self.id, core: self.clone() }
    }

    pub(crate) fn window_label(&self) -> Option<&str> {
        self.window_label.get().map(|label| &**label)
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn close_receipt(&self) -> &BrowserClose {
        &self.close
    }

    fn emit(self: &Arc<Self>, kind: NativeEventKind) {
        let Some(sink) = self.config.sink.clone() else { return };
        let event = NativeEvent { browser: self.identity(), kind };
        defer(move || sink(&event));
    }

    fn mint_navigation(&self) -> Option<NavigationIdentity> {
        Some(NavigationIdentity { id: NavigationId::mint()?, browser: self.id })
    }

    fn wake_for(self: &Arc<Self>, document: DocumentId) -> Box<dyn Fn() -> Result<(), String> + Send + Sync> {
        let core = Arc::downgrade(self);
        Box::new(move || {
            let core = core.upgrade().ok_or("The browser is gone.")?;
            let control = core.control().ok_or("The webview is not attached.")?;
            control.run(format!("window.__seuofficeWake?.({})", document.get()))
        })
    }

    // -------------------------------------------------------------------------------
    // Attachment and readiness
    // -------------------------------------------------------------------------------

    fn control(&self) -> Option<Arc<dyn WebviewControl>> {
        self.control.lock().unwrap_or_else(|error| error.into_inner()).clone()
    }

    /// The creation reservation now has a webview behind it: abandonment no longer means
    /// "not created". Called once after `build()` returned `Ok`.
    pub(crate) fn claim_creation(&self) -> Result<(), String> {
        self.close.claim()
    }

    /// The OS observers are attached and the webview is usable. Called once on the UI thread.
    pub(crate) fn mark_attached(
        self: &Arc<Self>, control: Arc<dyn WebviewControl>, window_label: &str,
    ) {
        if self.lock().closed {
            return;
        }
        *self.control.lock().unwrap_or_else(|error| error.into_inner()) = Some(control);
        let _ = self.window_label.set(window_label.into());
        self.lock().attached = true;
        self.ready_check();
        self.replay_held_navigation();
    }

    /// The OS webview enforces the request policy. Releases `Navigator::when_ready` and
    /// replays the navigation that was held for it.
    pub(crate) fn policy_installed(self: &Arc<Self>) {
        self.lock().policy_ready = true;
        self.ready_check();
        self.replay_held_navigation();
    }

    /// What the navigation hook does with a navigation before the policy is enforced.
    pub(crate) fn before_policy(&self, url: &Url) -> BeforePolicy {
        let mut state = self.lock();
        if state.policy_ready || state.closed {
            return BeforePolicy::Judge;
        }
        if url.as_str() == "about:blank" {
            return BeforePolicy::Blank;
        }
        if state.held_navigation.is_none() {
            state.held_navigation = Some(url.clone());
        }
        BeforePolicy::Hold
    }

    /// Navigate to the held URL once the webview is attached and the policy is enforced. The
    /// navigation goes through the hook like any other, now under the policy.
    fn replay_held_navigation(self: &Arc<Self>) {
        let url = {
            let mut state = self.lock();
            if !(state.attached && state.policy_ready) || state.closed {
                return;
            }
            state.held_navigation.take()
        };
        let Some(url) = url else { return };
        let outcome = match self.control() {
            Some(control) => control.navigate(&url),
            None => Err("The webview is not attached.".to_owned()),
        };
        if let Err(error) = outcome {
            eprintln!("shell: the held navigation to {url} could not start: {error}");
            if let Some(navigation) = self.mint_navigation() {
                {
                    let mut state = self.lock();
                    state.nav = Some(navigation);
                    state.nav_url = Some(url);
                    state.finished = None;
                }
                self.finish_load(Err(LoadError { code: None, description: error }));
            }
        }
    }

    /// The OS observers could not be attached: nothing waits for readiness forever.
    pub(crate) fn attach_failed(self: &Arc<Self>, error: &str) {
        let waiters = {
            let mut state = self.lock();
            if state.ready.is_some() {
                return;
            }
            state.ready = Some(Err(error.to_owned()));
            std::mem::take(&mut state.ready_waiters)
        };
        for waiter in waiters {
            let error = error.to_owned();
            defer(move || waiter(Err(error)));
        }
    }

    fn ready_check(self: &Arc<Self>) {
        let waiters = {
            let mut state = self.lock();
            if state.ready.is_some()
                || !state.attached
                || !state.policy_ready
                || (self.config.start == NavigationStart::Explicit && !state.initial_loaded)
            {
                return;
            }
            state.ready = Some(Ok(()));
            std::mem::take(&mut state.ready_waiters)
        };
        for waiter in waiters {
            defer(move || waiter(Ok(())));
        }
    }

    pub(crate) fn when_ready(self: &Arc<Self>, done: ReadyWaiter) {
        let mut state = self.lock();
        match state.ready.clone() {
            Some(result) => {
                drop(state);
                defer(move || done(result));
            }
            None => state.ready_waiters.push(done),
        }
    }

    // -------------------------------------------------------------------------------
    // Navigation and documents
    // -------------------------------------------------------------------------------

    /// Announce a document that ended: reload bookkeeping first, then the event, then the
    /// subscribers. Called with no lock held.
    fn announce_revoked(self: &Arc<Self>, revoked: Revoked, reload: Option<&Arc<Reload>>) {
        if let Some(reload) = reload {
            reload.on_document_revoked(&revoked.identity);
        }
        if revoked.was_registered() {
            self.emit(NativeEventKind::DocumentRevoked {
                document: revoked.identity.clone(),
                reason: revoked.reason,
            });
        }
        revoked.deliver();
    }

    fn navigation_started(
        self: &Arc<Self>, navigation: &NavigationIdentity, url: &Url, same_document: bool,
        reload: Option<&Arc<Reload>>, observed: Option<&Arc<Observed>>,
    ) {
        self.emit(NativeEventKind::NavigationStarted {
            navigation: navigation.clone(),
            url: url.clone(),
            same_document,
        });
        if !same_document {
            if let Some(reload) = reload {
                reload.on_navigation_started(navigation, url);
            }
            if let Some(observed) = observed {
                observed.on_navigation_started(navigation, url);
            }
        }
    }

    /// A main-frame navigation passed the shell policy and is about to start. Returns
    /// false when it must be cancelled (the browser is closed or ids are exhausted).
    /// A cross-document navigation revokes the current document before it returns, so no
    /// call of the old document can be admitted after the new navigation began.
    pub(crate) fn admit_navigation(self: &Arc<Self>, url: &Url) -> bool {
        let Some(navigation) = self.mint_navigation() else { return false };
        let mut revoked = None;
        let (same_document, reload, observed) = {
            let mut state = self.lock();
            if state.closed {
                return false;
            }
            let same_document = matches!(state.doc, DocSlot::Committed(_))
                && state.url.as_ref().is_some_and(|current| is_same_document_navigation(current, url));
            state.observation = state.observation.saturating_add(1);
            if same_document {
                state.url = Some(url.clone());
            } else {
                if let DocSlot::Committed(old) = std::mem::replace(&mut state.doc, DocSlot::None) {
                    revoked = old.revoke(RevokeReason::Navigation);
                }
                state.nav = Some(navigation.clone());
                state.nav_url = Some(url.clone());
                state.finished = None;
            }
            (same_document, state.reload.clone(), state.observed.clone())
        };
        if let Some(revoked) = revoked {
            self.announce_revoked(revoked, reload.as_ref());
        }
        self.navigation_started(&navigation, url, same_document, reload.as_ref(), observed.as_ref());
        if same_document {
            self.emit(NativeEventKind::AddressChanged { url: url.clone() });
        }
        true
    }

    /// The shell policy refused a navigation: nothing starts, but a pending observed
    /// navigation or reload must not wait for it forever.
    pub(crate) fn navigation_refused(self: &Arc<Self>, url: &Url) {
        let (reload, observed) = {
            let state = self.lock();
            (state.reload.clone(), state.observed.clone())
        };
        if let Some(reload) = reload {
            reload.on_refused(url);
        }
        if let Some(observed) = observed {
            observed.on_refused(url);
        }
    }

    /// The webview committed a main-frame document.
    pub(crate) fn commit(self: &Arc<Self>, url: &Url) {
        let Some(document_id) = DocumentId::mint() else { return };
        let mut revoked = None;
        let mut implicit = None;
        let (navigation, document, reload, observed) = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            // A commit nobody admitted: the first document, a restored page, a redirect.
            if state.nav.is_none() || matches!(state.doc, DocSlot::Committed(_)) {
                let Some(fresh) = self.mint_navigation() else { return };
                if let DocSlot::Committed(old) = std::mem::replace(&mut state.doc, DocSlot::None) {
                    revoked = old.revoke(RevokeReason::Navigation);
                }
                state.nav = Some(fresh.clone());
                state.nav_url = Some(url.clone());
                state.finished = None;
                implicit = Some(fresh);
            }
            let Some(navigation) = state.nav.clone() else { return };
            let identity = DocumentIdentity { id: document_id, browser: self.id, navigation: navigation.id };
            let document = DocumentCore::new(identity, self.identity(), url.clone(), self.wake_for(document_id));
            state.doc = DocSlot::Committed(document.clone());
            state.url = Some(url.clone());
            state.initial_loaded = true;
            state.observation = state.observation.saturating_add(1);
            (navigation, document, state.reload.clone(), state.observed.clone())
        };
        if let Some(revoked) = revoked {
            self.announce_revoked(revoked, reload.as_ref());
        }
        if let Some(fresh) = &implicit {
            self.navigation_started(fresh, url, false, reload.as_ref(), observed.as_ref());
        }
        self.emit(NativeEventKind::DocumentCommitted {
            navigation: navigation.clone(),
            document: document.identity.clone(),
            url: url.clone(),
        });
        self.emit(NativeEventKind::AddressChanged { url: url.clone() });
        if let Some(reload) = &reload {
            reload.on_committed(&navigation, &document.identity, url);
        }
        if let Some(observed) = &observed {
            observed.on_committed(&navigation, &document, url);
        }
        self.ready_check();
    }

    /// The address changed without a navigation (history API).
    pub(crate) fn address_changed(self: &Arc<Self>, url: &Url) {
        {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.url = Some(url.clone());
            state.observation = state.observation.saturating_add(1);
        }
        self.emit(NativeEventKind::AddressChanged { url: url.clone() });
    }

    /// The load of the latest navigation ended, successfully or not. Reported once per
    /// navigation; the first report wins.
    pub(crate) fn finish_load(self: &Arc<Self>, outcome: Result<(), LoadError>) {
        let (navigation, url, reload, observed) = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            let (Some(navigation), Some(url)) = (state.nav.clone(), state.nav_url.clone()) else {
                return;
            };
            if state.finished == Some(navigation.id) {
                return;
            }
            state.finished = Some(navigation.id);
            state.observation = state.observation.saturating_add(1);
            (navigation, url, state.reload.clone(), state.observed.clone())
        };
        match outcome {
            Ok(()) => self.emit(NativeEventKind::LoadFinished { navigation, ok: true }),
            Err(error) => {
                self.emit(NativeEventKind::NavigationFailed {
                    navigation: navigation.clone(),
                    url,
                    error: error.clone(),
                });
                if let Some(reload) = &reload {
                    reload.on_failed(&navigation);
                }
                if let Some(observed) = &observed {
                    observed.on_failed(&navigation, &error);
                }
                self.emit(NativeEventKind::LoadFinished { navigation, ok: false });
            }
        }
        self.ready_check();
    }

    pub(crate) fn view_event(self: &Arc<Self>, event: ViewEvent) {
        {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.observation = state.observation.saturating_add(1);
        }
        self.emit(NativeEventKind::View(event));
    }

    // -------------------------------------------------------------------------------
    // Registration
    // -------------------------------------------------------------------------------

    /// The bridge registered `nonce` for the committed document.
    pub(crate) fn register_document(
        self: &Arc<Self>, nonce: [u8; NONCE_LEN],
    ) -> Result<DocumentContext, RegisterError> {
        if self.config.bridge == BridgeMode::None {
            return Err(RegisterError::BridgeDisabled);
        }
        let (document, reload) = {
            let state = self.lock();
            match &state.doc {
                DocSlot::Committed(doc) if !state.closed => (doc.clone(), state.reload.clone()),
                _ => return Err(RegisterError::NoDocument),
            }
        };
        match document.register(nonce) {
            RegisterOutcome::Registered => {
                let context = DocumentContext(document);
                if let Some(hook) = self.on_registered.get().cloned() {
                    let hooked = context.clone();
                    defer(move || hook(&hooked));
                }
                self.emit(NativeEventKind::DocumentRegistered { context: context.clone() });
                if let Some(reload) = reload {
                    reload.on_registered(&context);
                }
                Ok(context)
            }
            RegisterOutcome::Unchanged => Ok(DocumentContext(document)),
            RegisterOutcome::Conflict(revoked) => {
                {
                    let mut state = self.lock();
                    if matches!(&state.doc, DocSlot::Committed(doc) if Arc::ptr_eq(doc, &document)) {
                        state.doc = DocSlot::Lost;
                    }
                }
                if let Some(revoked) = revoked {
                    self.announce_revoked(revoked, reload.as_ref());
                }
                Err(RegisterError::Conflict)
            }
            RegisterOutcome::Revoked => Err(RegisterError::Revoked),
        }
    }

    /// The bridge reported render-ready for the document it registered.
    pub(crate) fn render_ready(
        self: &Arc<Self>, nonce: &[u8; NONCE_LEN],
    ) -> Result<DocumentContext, RegisterError> {
        let context = self.redeem(nonce).ok_or(RegisterError::NoDocument)?;
        if context.0.mark_ready() {
            self.emit(NativeEventKind::DocumentReady { context: context.clone() });
        }
        Ok(context)
    }

    /// The registered, live document of this browser whose nonce is `nonce`.
    pub(crate) fn redeem(&self, nonce: &[u8; NONCE_LEN]) -> Option<DocumentContext> {
        let state = self.lock();
        match &state.doc {
            DocSlot::Committed(doc) if !state.closed && doc.nonce_matches(nonce) => {
                Some(DocumentContext(doc.clone()))
            }
            _ => None,
        }
    }

    // -------------------------------------------------------------------------------
    // Loss and retirement
    // -------------------------------------------------------------------------------

    /// The web content process is gone. The document is revoked; the browser stays open so
    /// the shell can reload or close it.
    pub(crate) fn process_failed(self: &Arc<Self>, kind: ProcessFailure) {
        let mut revoked = None;
        let (reload, observed) = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            if let DocSlot::Committed(old) = std::mem::replace(&mut state.doc, DocSlot::Lost) {
                revoked = old.revoke(RevokeReason::ProcessFailed);
            }
            state.nav = None;
            state.observation = state.observation.saturating_add(1);
            (state.reload.clone(), state.observed.clone())
        };
        if let Some(revoked) = revoked {
            self.announce_revoked(revoked, reload.as_ref());
        }
        self.emit(NativeEventKind::WebviewProcessFailed { kind });
        let reason = "The web content process failed.";
        if let Some(reload) = reload {
            reload.on_gone(reason);
        }
        if let Some(observed) = observed {
            observed.on_gone(reason);
        }
    }

    /// The webview left Tauri's registry. Idempotent.
    pub(crate) fn retire(self: &Arc<Self>) {
        self.retire_with(BrowserCloseOutcome::Retired);
    }

    /// End this browser's life. `NotCreated` is for a build that failed before any webview
    /// existed.
    pub(crate) fn retire_with(self: &Arc<Self>, outcome: BrowserCloseOutcome) {
        let mut revoked = None;
        let (reload, observed, waiters) = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            if let DocSlot::Committed(old) = std::mem::replace(&mut state.doc, DocSlot::Lost) {
                revoked = old.revoke(RevokeReason::Close);
            }
            state.nav = None;
            state.observation = state.observation.saturating_add(1);
            if state.ready.is_none() {
                state.ready = Some(Err("The browser closed before it was ready.".into()));
            }
            (state.reload.take(), state.observed.take(), std::mem::take(&mut state.ready_waiters))
        };
        drop(self.control.lock().unwrap_or_else(|error| error.into_inner()).take());
        if let Some(revoked) = revoked {
            self.announce_revoked(revoked, reload.as_ref());
        }
        let reason = "The browser closed.";
        if let Some(reload) = reload {
            reload.on_gone(reason);
        }
        if let Some(observed) = observed {
            observed.on_gone(reason);
        }
        for waiter in waiters {
            defer(move || waiter(Err(reason.to_owned())));
        }
        self.emit(NativeEventKind::Retired { outcome });
        let close = self.close.clone();
        defer(move || close.complete(outcome));
    }

    // -------------------------------------------------------------------------------
    // Reservations used by the navigation module
    // -------------------------------------------------------------------------------

    /// Replace the pending reload; the previous one is returned so the caller can fail it.
    pub(crate) fn install_reload(&self, reload: Arc<Reload>) -> Option<Arc<Reload>> {
        self.lock().reload.replace(reload)
    }

    pub(crate) fn clear_reload(&self, reload: &Arc<Reload>) {
        let mut state = self.lock();
        if state.reload.as_ref().is_some_and(|current| Arc::ptr_eq(current, reload)) {
            state.reload = None;
        }
    }

    pub(crate) fn install_observed(&self, observed: Arc<Observed>) -> Result<(), String> {
        let mut state = self.lock();
        if state.closed {
            return Err("The browser is closed.".into());
        }
        if state.observed.as_ref().is_some_and(|current| !current.is_finished()) {
            return Err("An observed navigation is already pending for this browser.".into());
        }
        state.observed = Some(observed);
        Ok(())
    }

    pub(crate) fn clear_observed(&self, observed: &Arc<Observed>) {
        let mut state = self.lock();
        if state.observed.as_ref().is_some_and(|current| Arc::ptr_eq(current, observed)) {
            state.observed = None;
        }
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.lock().attached
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Fakes shared by the unit tests of the pure state machines.

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::native::lifecycle::BrowserCreation;

    /// The nonce `registered_context*` registers.
    pub(crate) const TEST_NONCE: [u8; NONCE_LEN] = [7; NONCE_LEN];

    type Pending = Box<dyn FnOnce(Result<Value, String>) + Send>;

    #[derive(Default)]
    pub(crate) struct FakeControl {
        evaluations: Mutex<VecDeque<Pending>>,
        pub(crate) navigations: Mutex<Vec<String>>,
        pub(crate) reloads: Mutex<u32>,
        pub(crate) closes: Mutex<u32>,
        pub(crate) scripts: Mutex<Vec<String>>,
        pub(crate) fail_reload: AtomicBool,
    }

    impl FakeControl {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// Answer the oldest evaluation that is still waiting; a no-op when none is.
        pub(crate) fn answer_evaluation(&self, result: Result<Value, String>) {
            let next = self.evaluations.lock().unwrap().pop_front();
            if let Some(done) = next {
                done(result);
            }
        }
    }

    impl WebviewControl for FakeControl {
        fn navigate(&self, url: &Url) -> Result<(), String> {
            self.navigations.lock().unwrap().push(url.to_string());
            Ok(())
        }

        fn reload(&self) -> Result<(), String> {
            if self.fail_reload.load(Ordering::Acquire) {
                return Err("reload refused".into());
            }
            *self.reloads.lock().unwrap() += 1;
            Ok(())
        }

        fn close(&self) -> Result<(), String> {
            *self.closes.lock().unwrap() += 1;
            Ok(())
        }

        fn evaluate(
            &self, script: String, done: Box<dyn FnOnce(Result<Value, String>) + Send>,
        ) -> Result<(), String> {
            self.scripts.lock().unwrap().push(script);
            self.evaluations.lock().unwrap().push_back(done);
            Ok(())
        }

        fn run(&self, script: String) -> Result<(), String> {
            self.scripts.lock().unwrap().push(script);
            Ok(())
        }
    }

    pub(crate) type Events = Arc<Mutex<Vec<NativeEventKind>>>;

    pub(crate) fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    /// An attached browser whose events are recorded. The creation is claimed.
    pub(crate) fn browser_with_events(
        label: &str, control: Arc<FakeControl>, start: NavigationStart,
    ) -> (BrowserIdentity, Events) {
        let events: Events = Arc::default();
        let recorded = events.clone();
        let sink: EventSink = Arc::new(move |event| recorded.lock().unwrap().push(event.kind.clone()));
        let creation = BrowserCreation::new().unwrap();
        let receipt = creation.receipt();
        let submission = creation.into_submission();
        receipt.claim().unwrap();
        let config = Config { sink: Some(sink), start, bridge: BridgeMode::Invoke, policy: None };
        let core = BrowserCore::new(label, submission.creation(), receipt, config).unwrap();
        core.mark_attached(control, label);
        (core.identity(), events)
    }

    /// A browser with one admitted, committed and registered document.
    pub(crate) fn registered_context_with_control(
        label: &str, control: Arc<FakeControl>,
    ) -> (BrowserIdentity, DocumentContext) {
        let (browser, _events) = browser_with_events(label, control, NavigationStart::Automatic);
        let address = url("https://example.test/a");
        assert!(browser.core.admit_navigation(&address));
        browser.core.commit(&address);
        let context = browser.core.register_document(TEST_NONCE).unwrap();
        (browser, context)
    }

    pub(crate) fn registered_context(label: &str) -> (BrowserIdentity, DocumentContext) {
        registered_context_with_control(label, FakeControl::new())
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn kinds(events: &Events) -> Vec<&'static str> {
        events
            .lock()
            .unwrap()
            .iter()
            .map(|kind| match kind {
                NativeEventKind::NavigationStarted { .. } => "started",
                NativeEventKind::DocumentCommitted { .. } => "committed",
                NativeEventKind::DocumentRegistered { .. } => "registered",
                NativeEventKind::DocumentReady { .. } => "ready",
                NativeEventKind::DocumentRevoked { .. } => "revoked",
                NativeEventKind::NavigationFailed { .. } => "failed",
                NativeEventKind::LoadFinished { ok: true, .. } => "loaded",
                NativeEventKind::LoadFinished { ok: false, .. } => "load-failed",
                NativeEventKind::AddressChanged { .. } => "address",
                NativeEventKind::WebviewProcessFailed { .. } => "process-failed",
                NativeEventKind::View(_) => "view",
                NativeEventKind::Retired { .. } => "retired",
            })
            .collect()
    }

    #[test]
    fn a_cross_document_navigation_revokes_the_document_before_the_new_navigation_starts() {
        let (browser, events) = browser_with_events("nav-revoke", FakeControl::new(), NavigationStart::Automatic);
        let first = url("https://example.test/a");
        assert!(browser.core.admit_navigation(&first));
        browser.core.commit(&first);
        let context = browser.core.register_document(TEST_NONCE).unwrap();
        events.lock().unwrap().clear();

        assert!(browser.core.admit_navigation(&url("https://example.test/b")));
        assert!(context.is_revoked(), "revoked at admission, before the successor commits");
        assert_eq!(context.with_live(|| ()), None);
        assert!(browser.document().is_none());
        assert_eq!(kinds(&events), ["revoked", "started"]);
        assert!(matches!(
            events.lock().unwrap()[0],
            NativeEventKind::DocumentRevoked { reason: RevokeReason::Navigation, .. }
        ));
    }

    #[test]
    fn a_fragment_navigation_keeps_the_document_and_a_plain_reload_does_not() {
        let (browser, context) = registered_context("nav-fragment");
        assert!(browser.core.admit_navigation(&url("https://example.test/a#section")));
        assert!(context.is_live(), "same document: the JS realm survived");
        assert_eq!(browser.url().unwrap().fragment(), Some("section"));
        assert!(browser.core.admit_navigation(&url("https://example.test/a")));
        assert!(context.is_revoked(), "the same URL without a fragment is a reload");
    }

    #[test]
    fn an_unadmitted_commit_replaces_and_revokes_the_previous_document() {
        let (browser, events) = browser_with_events("nav-implicit", FakeControl::new(), NavigationStart::Automatic);
        let first = url("https://example.test/a");
        browser.core.commit(&first);
        let context = browser.core.register_document(TEST_NONCE).unwrap();
        events.lock().unwrap().clear();
        browser.core.commit(&url("https://example.test/restored"));
        assert!(context.is_revoked());
        assert_eq!(kinds(&events), ["revoked", "started", "committed", "address"]);
        assert!(browser.registered_context().is_none(), "the successor has not registered");
    }

    #[test]
    fn registration_is_idempotent_and_a_second_nonce_revokes_the_document() {
        let (browser, context) = registered_context("register");
        let again = browser.core.register_document(TEST_NONCE).unwrap();
        assert_eq!(again, context);
        assert_eq!(browser.core.redeem(&TEST_NONCE), Some(context.clone()));
        assert!(browser.core.redeem(&[9; NONCE_LEN]).is_none());
        assert_eq!(browser.core.register_document([9; NONCE_LEN]).err(), Some(RegisterError::Conflict));
        assert!(context.is_revoked());
        assert!(browser.core.redeem(&TEST_NONCE).is_none());
    }

    #[test]
    fn render_ready_emits_once_and_the_document_reports_ready() {
        let (browser, events) = browser_with_events("ready", FakeControl::new(), NavigationStart::Automatic);
        let address = url("https://example.test/a");
        browser.core.admit_navigation(&address);
        browser.core.commit(&address);
        let context = browser.core.register_document(TEST_NONCE).unwrap();
        assert!(context.document_ready().is_none());
        browser.core.render_ready(&TEST_NONCE).unwrap();
        browser.core.render_ready(&TEST_NONCE).unwrap();
        assert!(context.document_ready().is_some());
        assert_eq!(kinds(&events), ["started", "committed", "address", "registered", "ready"]);
    }

    #[test]
    fn a_failed_load_reports_failure_then_finish_once() {
        let (browser, events) = browser_with_events("failed-load", FakeControl::new(), NavigationStart::Automatic);
        let address = url("https://example.test/a");
        browser.core.admit_navigation(&address);
        events.lock().unwrap().clear();
        let error = LoadError { code: Some(-1), description: "refused".into() };
        browser.core.finish_load(Err(error.clone()));
        browser.core.finish_load(Ok(()));
        assert_eq!(kinds(&events), ["failed", "load-failed"]);
    }

    #[test]
    fn a_process_failure_revokes_the_document_but_keeps_the_browser_open() {
        let (browser, context) = registered_context("process");
        browser.core.process_failed(ProcessFailure::ContentProcessTerminated);
        assert!(context.is_revoked());
        assert!(browser.is_open());
        assert!(browser.document().is_none());
        let next = url("https://example.test/again");
        browser.core.commit(&next);
        assert!(browser.document().is_some());
    }

    #[test]
    fn retiring_revokes_closes_the_receipt_once_and_refuses_new_navigation() {
        let (browser, context) = registered_context("retire");
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let sink = outcomes.clone();
        let _subscription = browser.when_closed(move |outcome| sink.lock().unwrap().push(outcome)).unwrap();
        browser.core.retire();
        browser.core.retire();
        assert!(context.is_revoked());
        assert!(!browser.is_open());
        assert!(browser.with_live_browser(|| ()).is_none());
        assert!(!browser.core.admit_navigation(&url("https://example.test/late")));
        assert_eq!(*outcomes.lock().unwrap(), [BrowserCloseOutcome::Retired]);
    }

    #[test]
    fn an_explicit_start_is_ready_only_after_attachment_and_the_blank_commit() {
        let creation = crate::native::lifecycle::BrowserCreation::new().unwrap();
        let receipt = creation.receipt();
        let submission = creation.into_submission();
        receipt.claim().unwrap();
        let config = Config { start: NavigationStart::Explicit, bridge: BridgeMode::Invoke, ..Config::default() };
        let core = BrowserCore::new("explicit", submission.creation(), receipt, config).unwrap();
        let ready = Arc::new(Mutex::new(Vec::new()));
        let sink = ready.clone();
        core.when_ready(Box::new(move |result| sink.lock().unwrap().push(result)));
        core.mark_attached(FakeControl::new(), "explicit");
        assert!(ready.lock().unwrap().is_empty(), "attached but the blank page did not commit");
        core.commit(&url("about:blank"));
        assert_eq!(*ready.lock().unwrap(), [Ok(())]);
        let late = Arc::new(Mutex::new(Vec::new()));
        let sink = late.clone();
        core.when_ready(Box::new(move |result| sink.lock().unwrap().push(result)));
        assert_eq!(*late.lock().unwrap(), [Ok(())]);
    }

    #[test]
    fn identities_of_two_browsers_never_match_even_under_one_label() {
        let (first, _) = registered_context("reused-label");
        let (second, _) = registered_context("reused-label");
        assert!(!first.is_same_browser(&second));
        assert!(first.is_same_browser(&first.clone()));
        assert_ne!(first.id(), second.id());
    }
}
