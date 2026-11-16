//! A committed document and the proof that the bridge registered for it.
//!
//! [`DocumentIdentity`] is minted by Rust at navigation commit. [`DocumentContext`] exists
//! only after the bridge registered its nonce for that document, so holding one means
//! "this document proved itself". Revocation runs at navigation admission, before the
//! successor commits, and `with_live*` fence a short effect against it.
//!
//! Lock order: browser state, then document state. No code that holds a document gate may
//! call back into browser state or UI code.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use tauri::Url;

use super::browser::BrowserIdentity;
use super::defer::{defer, hold};
use super::eval::{self, DocumentEvaluation, EvalError};
use super::events::RevokeReason;
use super::ids::{BrowserId, DocumentId, NavigationId};
use super::ingress::{NONCE_LEN, ReplySink, nonce_eq};

/// Opaque identity of one committed document.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DocumentIdentity {
    pub(crate) id: DocumentId,
    pub(crate) browser: BrowserId,
    pub(crate) navigation: NavigationId,
}

impl DocumentIdentity {
    /// The public id the bridge's `__seuofficeCheck` compares. Not a secret.
    pub fn id(&self) -> DocumentId {
        self.id
    }

    pub fn browser_id(&self) -> BrowserId {
        self.browser
    }

    pub fn navigation_id(&self) -> NavigationId {
        self.navigation
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Life {
    Provisional,
    Registered([u8; NONCE_LEN]),
    Ready([u8; NONCE_LEN]),
    Revoked(RevokeReason),
}

impl fmt::Debug for Life {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provisional => f.write_str("Provisional"),
            Self::Registered(_) => f.write_str("Registered"),
            Self::Ready(_) => f.write_str("Ready"),
            Self::Revoked(reason) => write!(f, "Revoked({reason:?})"),
        }
    }
}

impl Life {
    pub(crate) fn is_registered(&self) -> bool {
        matches!(self, Self::Registered(_) | Self::Ready(_))
    }
}

type RevokeCallback = Box<dyn FnOnce(RevokeReason) + Send + 'static>;

pub(crate) struct RevokeSlot {
    active: AtomicBool,
    callback: Mutex<Option<RevokeCallback>>,
}

impl RevokeSlot {
    fn new(callback: RevokeCallback) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(true),
            callback: Mutex::new(Some(callback)),
        })
    }

    fn take(&self) -> Option<RevokeCallback> {
        if !self.active.swap(false, Ordering::AcqRel) {
            return None;
        }
        self.callback
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

/// Retain while loss of the document must be observed. Dropping it unregisters the
/// callback unless revocation already claimed it.
pub struct DocumentRevocation {
    slot: Arc<RevokeSlot>,
}

impl Drop for DocumentRevocation {
    fn drop(&mut self) {
        drop(self.slot.take());
    }
}

pub(crate) struct DocState {
    pub(crate) life: Life,
    listeners: Vec<Arc<RevokeSlot>>,
}

/// A revocation that already happened. Deliver it after every lock is released.
#[must_use]
pub(crate) struct Revoked {
    pub(crate) identity: DocumentIdentity,
    pub(crate) previous: Life,
    pub(crate) reason: RevokeReason,
    listeners: Vec<Arc<RevokeSlot>>,
}

impl Revoked {
    /// True when the document had registered, i.e. consumers know it.
    pub(crate) fn was_registered(&self) -> bool {
        self.previous.is_registered()
    }

    pub(crate) fn deliver(self) {
        for slot in self.listeners {
            let reason = self.reason;
            defer(move || {
                if let Some(callback) = slot.take() {
                    callback(reason);
                }
            });
        }
    }
}

pub(crate) enum RegisterOutcome {
    Registered,
    Unchanged,
    /// A second, different nonce: the document was revoked.
    Conflict(Option<Revoked>),
    Revoked,
}

pub(crate) struct DocumentCore {
    pub(crate) identity: DocumentIdentity,
    pub(crate) browser: BrowserIdentity,
    pub(crate) url: Url,
    revoked: Arc<AtomicBool>,
    pub(crate) state: Mutex<DocState>,
    reply: ReplySink,
}

impl DocumentCore {
    pub(crate) fn new(
        identity: DocumentIdentity,
        browser: BrowserIdentity,
        url: Url,
        wake: Box<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Arc<Self> {
        let revoked = Arc::new(AtomicBool::new(false));
        let reply = ReplySink::new(identity.id, revoked.clone(), wake);
        Arc::new(Self {
            identity,
            browser,
            url,
            revoked,
            state: Mutex::new(DocState {
                life: Life::Provisional,
                listeners: Vec::new(),
            }),
            reply,
        })
    }

    fn lock(&self) -> MutexGuard<'_, DocState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn revoke_locked(&self, state: &mut DocState, reason: RevokeReason) -> Option<Revoked> {
        if matches!(state.life, Life::Revoked(_)) {
            return None;
        }
        let previous = std::mem::replace(&mut state.life, Life::Revoked(reason));
        self.revoked.store(true, Ordering::Release);
        self.reply.close();
        Some(Revoked {
            identity: self.identity.clone(),
            previous,
            reason,
            listeners: std::mem::take(&mut state.listeners),
        })
    }

    /// Revoke once. `None` when it was already revoked.
    pub(crate) fn revoke(&self, reason: RevokeReason) -> Option<Revoked> {
        let mut state = self.lock();
        self.revoke_locked(&mut state, reason)
    }

    pub(crate) fn register(&self, nonce: [u8; NONCE_LEN]) -> RegisterOutcome {
        let mut state = self.lock();
        match state.life {
            Life::Provisional => {
                state.life = Life::Registered(nonce);
                RegisterOutcome::Registered
            }
            Life::Registered(known) | Life::Ready(known) if nonce_eq(&known, &nonce) => {
                RegisterOutcome::Unchanged
            }
            Life::Registered(_) | Life::Ready(_) => {
                RegisterOutcome::Conflict(self.revoke_locked(&mut state, RevokeReason::Conflict))
            }
            Life::Revoked(_) => RegisterOutcome::Revoked,
        }
    }

    /// Registered to ready; false in any other state.
    pub(crate) fn mark_ready(&self) -> bool {
        let mut state = self.lock();
        match state.life {
            Life::Registered(nonce) => {
                state.life = Life::Ready(nonce);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn nonce_matches(&self, nonce: &[u8; NONCE_LEN]) -> bool {
        match self.lock().life {
            Life::Registered(known) | Life::Ready(known) => nonce_eq(&known, nonce),
            Life::Provisional | Life::Revoked(_) => false,
        }
    }

    pub(crate) fn life(&self) -> Life {
        self.lock().life
    }

    pub(crate) fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Without waiting for the document lock. `None` means already revoked or contended
    /// and must be treated as loss.
    fn subscribe(&self, callback: RevokeCallback) -> Option<DocumentRevocation> {
        let mut state = self.state.try_lock().ok()?;
        if matches!(state.life, Life::Revoked(_)) {
            return None;
        }
        state
            .listeners
            .retain(|slot| slot.active.load(Ordering::Acquire));
        let slot = RevokeSlot::new(callback);
        state.listeners.push(slot.clone());
        Some(DocumentRevocation { slot })
    }

    /// Like `subscribe`, but an already revoked document delivers its reason (deferred).
    pub(crate) fn subscribe_or_deliver(
        &self,
        callback: RevokeCallback,
    ) -> Result<Option<DocumentRevocation>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Native document state was poisoned.")?;
        if let Life::Revoked(reason) = state.life {
            drop(state);
            defer(move || callback(reason));
            return Ok(None);
        }
        state
            .listeners
            .retain(|slot| slot.active.load(Ordering::Acquire));
        let slot = RevokeSlot::new(callback);
        state.listeners.push(slot.clone());
        Ok(Some(DocumentRevocation { slot }))
    }

    pub(crate) fn reply(&self) -> &ReplySink {
        &self.reply
    }
}

/// An authenticated document: committed, and registered by its bridge.
#[derive(Clone)]
pub struct DocumentContext(pub(crate) Arc<DocumentCore>);

impl PartialEq for DocumentContext {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for DocumentContext {}

impl fmt::Debug for DocumentContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentContext")
            .field("document", &self.0.identity.id)
            .finish_non_exhaustive()
    }
}

impl DocumentContext {
    pub fn identity(&self) -> &DocumentIdentity {
        &self.0.identity
    }

    pub fn browser(&self) -> &BrowserIdentity {
        &self.0.browser
    }

    pub fn url(&self) -> &Url {
        &self.0.url
    }

    /// Registered and not revoked.
    pub fn is_registered(&self) -> bool {
        self.0.life().is_registered()
    }

    /// Negative-only notification: false means revoked. A true result is not authority and
    /// never replaces `with_live`.
    pub fn is_live(&self) -> bool {
        !self.0.is_revoked()
    }

    pub fn is_revoked(&self) -> bool {
        self.0.is_revoked()
    }

    /// Subscribe atomically against revocation. `None` means already revoked or contended
    /// and must be treated as owner loss. The callback runs at most once, outside every
    /// lock.
    pub fn on_revoke(
        &self,
        callback: impl FnOnce() + Send + 'static,
    ) -> Option<DocumentRevocation> {
        self.0.subscribe(Box::new(move |_| callback()))
    }

    /// Run a short, non-reentrant effect atomically against revocation. The closure must
    /// not enter UI, modal or core waits.
    pub fn with_live<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
        Self::with_live_all([Some(self)], operation)
    }

    pub fn with_live_pair<T>(&self, other: &Self, operation: impl FnOnce() -> T) -> Option<T> {
        Self::with_live_all([Some(self), Some(other)], operation)
    }

    /// Fence every given owner in canonical order. Aliases share one lock.
    pub fn with_live_all<T, const N: usize>(
        contexts: [Option<&Self>; N],
        operation: impl FnOnce() -> T,
    ) -> Option<T> {
        let present: Vec<&Self> = contexts.into_iter().flatten().collect();
        DocumentGatePlan::new(&present)?.with_live(operation)
    }

    /// Still the browser's committed document.
    pub fn current_document(&self) -> Option<DocumentIdentity> {
        self.0
            .browser
            .document()
            .filter(|document| *document == self.0.identity)
    }

    /// Present once the bridge reported render-ready for this exact document.
    pub fn document_ready(&self) -> Option<DocumentReady> {
        matches!(self.0.life(), Life::Ready(_)).then(|| DocumentReady {
            origin: self.clone(),
        })
    }

    /// Await promises and serialize by value in this document. A document that was revoked
    /// before the script ran answers `EvalError::StaleDocument`. Retain the result until
    /// completion; dropping it cancels Rust delivery (not running JS).
    pub fn evaluate(
        &self,
        source: String,
        completion: impl FnOnce(Result<Value, EvalError>) + Send + 'static,
    ) -> Result<DocumentEvaluation, EvalError> {
        eval::start(self, source, Box::new(completion))
    }

    pub fn reply_sink(&self) -> Option<ReplySink> {
        self.is_live().then(|| self.0.reply().clone())
    }
}

/// Evidence that the bridge reported readiness for this exact registered document.
#[derive(Clone)]
pub struct DocumentReady {
    origin: DocumentContext,
}

impl DocumentReady {
    pub fn origin(&self) -> &DocumentContext {
        &self.origin
    }

    pub fn is_live(&self) -> bool {
        self.origin.is_live()
    }

    pub fn on_revoke(
        &self,
        callback: impl FnOnce() + Send + 'static,
    ) -> Option<DocumentRevocation> {
        self.origin.on_revoke(callback)
    }

    pub fn with_live<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
        self.origin.with_live(operation)
    }
}

/// Prepared outside every authority gate, on the final effect's thread.
pub struct DocumentGatePlan<'a> {
    cores: Vec<&'a DocumentCore>,
}

impl<'a> DocumentGatePlan<'a> {
    /// Sorted by address and deduplicated: every plan locks in the same order, so two
    /// plans over overlapping documents cannot deadlock. `None` for no documents.
    pub fn new(contexts: &[&'a DocumentContext]) -> Option<Self> {
        if contexts.is_empty() {
            return None;
        }
        let mut cores: Vec<&'a DocumentCore> = contexts.iter().map(|context| &*context.0).collect();
        cores.sort_unstable_by_key(|core| std::ptr::from_ref::<DocumentCore>(core) as usize);
        cores.dedup_by(|left, right| std::ptr::eq(*left, *right));
        Some(Self { cores })
    }

    /// Consuming the plan keeps lock storage local. The guard vector is allocated before
    /// the first lock, so acquisition, validation and the effect allocate nothing here.
    pub fn with_live<T>(self, operation: impl FnOnce() -> T) -> Option<T> {
        let _hold = hold();
        let mut guards: Vec<MutexGuard<'a, DocState>> = Vec::with_capacity(self.cores.len());
        for core in self.cores.iter().copied() {
            guards.push(core.state.lock().ok()?);
        }
        if guards.iter().any(|guard| !guard.life.is_registered()) {
            return None;
        }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
            Ok(value) => Some(value),
            Err(panic) => {
                // A panicking effect leaves the fenced state unknown: fail closed.
                let revoked: Vec<Revoked> = guards
                    .iter_mut()
                    .zip(&self.cores)
                    .filter_map(|(guard, core)| core.revoke_locked(guard, RevokeReason::Fault))
                    .collect();
                drop(guards);
                for revocation in revoked {
                    revocation.deliver();
                }
                std::panic::resume_unwind(panic)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::native::browser::testing::registered_context;

    #[test]
    fn with_live_runs_for_a_live_context_and_not_after_revocation() {
        let (_browser, context) = registered_context("doc-live");
        assert_eq!(context.with_live(|| 7), Some(7));
        context.0.revoke(RevokeReason::Close).unwrap().deliver();
        assert!(context.is_revoked());
        assert_eq!(context.with_live(|| 7), None);
    }

    #[test]
    fn gate_plan_dedupes_aliases_and_locks_in_one_order() {
        let (_a_browser, a) = registered_context("gate-a");
        let (_b_browser, b) = registered_context("gate-b");
        let forward = DocumentGatePlan::new(&[&a, &b, &a]).unwrap();
        let backward = DocumentGatePlan::new(&[&b, &a]).unwrap();
        assert_eq!(forward.cores.len(), 2);
        let order = |plan: &DocumentGatePlan<'_>| -> Vec<usize> {
            plan.cores
                .iter()
                .map(|core| std::ptr::from_ref::<DocumentCore>(core) as usize)
                .collect()
        };
        assert_eq!(order(&forward), order(&backward));
        assert_eq!(forward.with_live(|| "both"), Some("both"));
        assert_eq!(DocumentContext::with_live_pair(&a, &a, || 1), Some(1));
    }

    #[test]
    fn one_revoked_owner_refuses_the_whole_gate() {
        let (_a_browser, a) = registered_context("gate-revoked-a");
        let (_b_browser, b) = registered_context("gate-revoked-b");
        b.0.revoke(RevokeReason::Navigation).unwrap().deliver();
        let mut ran = false;
        assert_eq!(a.with_live_pair(&b, || ran = true), None);
        assert!(!ran);
        assert!(DocumentGatePlan::new(&[]).is_none());
        assert_eq!(
            DocumentContext::with_live_all::<(), 2>([None, None], || ()),
            None
        );
    }

    #[test]
    fn a_panicking_effect_revokes_every_fenced_document() {
        let (_a_browser, a) = registered_context("gate-panic-a");
        let (_b_browser, b) = registered_context("gate-panic-b");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            a.with_live_pair(&b, || panic!("effect failed"))
        }));
        assert!(result.is_err());
        assert!(a.is_revoked() && b.is_revoked());
    }

    #[test]
    fn revocation_callbacks_fire_once_and_dropped_subscriptions_never() {
        let (_browser, context) = registered_context("doc-callbacks");
        let fired = Arc::new(Mutex::new(Vec::new()));
        let kept = fired.clone();
        let _subscription = context
            .on_revoke(move || kept.lock().unwrap().push("kept"))
            .unwrap();
        let dropped = fired.clone();
        drop(
            context
                .on_revoke(move || dropped.lock().unwrap().push("dropped"))
                .unwrap(),
        );
        context
            .0
            .revoke(RevokeReason::Navigation)
            .unwrap()
            .deliver();
        assert!(context.0.revoke(RevokeReason::Close).is_none());
        assert_eq!(*fired.lock().unwrap(), ["kept"]);
        assert!(context.on_revoke(|| ()).is_none());
    }

    #[test]
    fn a_different_nonce_for_the_same_document_fails_closed() {
        let (_browser, context) = registered_context("doc-conflict");
        assert!(matches!(
            context.0.register([1; NONCE_LEN]),
            RegisterOutcome::Conflict(Some(_))
        ));
        assert!(context.is_revoked());
        assert!(matches!(
            context.0.register([1; NONCE_LEN]),
            RegisterOutcome::Revoked
        ));
    }
}
