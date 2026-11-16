//! Threads, three kinds:
//!
//! - the UI thread runs the Tauri/CEF event loop and alone touches windows,
//!   menus and dialogs;
//! - the core thread runs every handler, in arrival order, as Electron's main
//!   thread ran every `ipcMain` listener; `main-core`'s state lives there;
//! - workers run what TypeScript awaited (the network, child processes, big
//!   file work on owned data) and post the result back to the core thread.
//!
//! When `main-core` calls back into the UI (a message box through its
//! `MessageBox` trait, a dialog through `Dialog`), the adapter runs that on
//! the UI thread and blocks the caller until it is done ([`run_on`]). Nothing
//! blocks on the UI thread itself.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::ids::ViewId;
use crate::ipc::{EventSink, Sender};

/// A thread's queue: the UI thread or the core thread
pub trait Thread: Send + Sync {
    fn post(&self, job: Box<dyn FnOnce() + Send>);
    fn is_current(&self) -> bool;
}

/// The worker pool
pub trait Workers: Send + Sync {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>);
}

/// Runs `job` on `thread` and waits for its result. Panics when called on
/// that thread itself: the wait could never end.
pub fn run_on<R: Send + 'static>(
    thread: &dyn Thread,
    job: impl FnOnce() -> R + Send + 'static,
) -> R {
    assert!(!thread.is_current(), "run_on its own thread would deadlock");
    let (tx, rx) = mpsc::sync_channel(1);
    thread.post(Box::new(move || {
        let _ = tx.send(job());
    }));
    rx.recv().expect("the thread dropped a job")
}

/// Set once, read from any thread. Observers wake cancellable host operations
/// without polling; a subscription owns its callback until it is dropped.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<CancelState>);

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    observers: Mutex<Vec<Weak<CancelObserver>>>,
    external_cancelled: AtomicBool,
    external_loss: std::sync::OnceLock<CancelToken>,
}

struct CancelObserver {
    callback: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl CancelObserver {
    fn notify(&self) {
        let callback = self
            .callback
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(callback) = callback {
            callback();
        }
    }
}

/// Dropping the subscription removes its callback unless cancellation already
/// claimed it. A callback must not block or panic.
pub struct CancelSubscription(Arc<CancelObserver>);

impl Drop for CancelSubscription {
    fn drop(&mut self) {
        self.0
            .callback
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancelToken {
    pub fn cancel(&self) {
        self.cancel_with_external_loss(true);
    }

    /// Dispatcher-only lifecycle retirement. Ordinary callers must use cancel:
    /// explicit cancellation, including a later call through an alias, is distinct.
    pub fn cancel_renderer_retired(&self) {
        self.cancel_with_external_loss(false);
    }

    fn cancel_with_external_loss(&self, external: bool) {
        let observers = {
            let mut observers = self.0.observers.lock().unwrap_or_else(|p| p.into_inner());
            if external {
                self.0.external_cancelled.store(true, Ordering::Release);
            }
            if self.0.cancelled.swap(true, Ordering::AcqRel) {
                Vec::new()
            } else {
                std::mem::take(&mut *observers)
            }
        };
        for observer in observers {
            if let Some(observer) = observer.upgrade() {
                observer.notify();
            }
        }
        // The source's observer lock is already released. A public cancel after
        // renderer retirement still wakes this independent signal exactly once.
        if external
            && let Some(signal) = self.0.external_loss.get()
        {
            signal.cancel();
        }
    }

    /// Observe public cancellation separately from expected renderer retirement.
    /// Capture this before admission; lazy allocation never occurs in cancel().
    pub fn external_loss_token(&self) -> CancelToken {
        let signal = self
            .0
            .external_loss
            .get_or_init(CancelToken::default)
            .clone();
        if self.0.external_cancelled.load(Ordering::Acquire) {
            signal.cancel();
        }
        signal
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub fn same_token(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn with_active_all<T, const N: usize>(
        mut tokens: [Option<&Self>; N],
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        tokens.sort_unstable_by_key(|token| token.map(|token| Arc::as_ptr(&token.0)));
        let _guards: [Option<_>; N] = std::array::from_fn(|index| {
            let token = tokens[index]?;
            if index > 0 && tokens[index - 1].is_some_and(|previous| previous.same_token(token)) {
                return None;
            }
            Some(
                token
                    .0
                    .observers
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            )
        });
        if tokens.iter().flatten().all(|token| !token.is_cancelled()) {
            Some(action())
        } else {
            None
        }
    }

    /// Run a short commit while cancellation is excluded.
    pub fn with_active<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        let _guard = self.0.observers.lock().unwrap_or_else(|p| p.into_inner());
        if self.is_cancelled() {
            None
        } else {
            Some(commit())
        }
    }

    /// The callback runs once, possibly before this function returns. It never
    /// runs while the token's observer lock is held.
    pub fn on_cancel(&self, callback: impl FnOnce() + Send + 'static) -> CancelSubscription {
        let observer = Arc::new(CancelObserver {
            callback: Mutex::new(Some(Box::new(callback))),
        });
        let cancelled = {
            let mut observers = self.0.observers.lock().unwrap_or_else(|p| p.into_inner());
            if self.is_cancelled() {
                true
            } else {
                observers.retain(|observer| observer.strong_count() != 0);
                observers.push(Arc::downgrade(&observer));
                false
            }
        };
        if cancelled {
            observer.notify();
        }
        CancelSubscription(observer)
    }
}

/// Unforgeable identity of one renderer document lifetime.
#[derive(Clone, Debug)]
pub struct RendererGeneration(Arc<RendererGenerationState>);

#[derive(Debug)]
struct RendererGenerationState {
    active: AtomicBool,
    gate: Mutex<()>,
    loss: CancelToken,
}

struct ClearGates<'a, T>(&'a mut Vec<T>);

impl<T> Drop for ClearGates<'_, T> {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// Prepared before any outer admission gate; reused without allocating while locked.
pub struct CancelTokenGatePlan<'a> {
    tokens: Vec<&'a CancelToken>,
    guards: Vec<MutexGuard<'a, Vec<Weak<CancelObserver>>>>,
}

impl<'a> CancelTokenGatePlan<'a> {
    pub fn new(tokens: &[&'a CancelToken]) -> Self {
        Self::from_tokens(tokens.to_vec())
    }

    pub fn from_tokens(mut tokens: Vec<&'a CancelToken>) -> Self {
        tokens.sort_unstable_by_key(|token| Arc::as_ptr(&token.0));
        tokens.dedup_by(|a, b| a.same_token(b));
        let guards = Vec::with_capacity(tokens.len());
        Self { tokens, guards }
    }

    pub fn with_active<T>(&mut self, effect: impl FnOnce() -> T) -> Option<T> {
        let held = ClearGates(&mut self.guards);
        for &token in &self.tokens {
            held.0.push(
                token
                    .0
                    .observers
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            );
        }
        if self.tokens.iter().all(|token| !token.is_cancelled()) {
            Some(effect())
        } else {
            None
        }
    }
}

/// Canonical renderer/document gate storage prepared on the execution thread.
pub struct RendererGenerationGatePlan<'a> {
    generations: Vec<&'a RendererGeneration>,
    guards: Vec<MutexGuard<'a, ()>>,
}

pub struct DocumentRetirementPlan<'a> {
    documents: Vec<Option<&'a RendererGeneration>>,
    notifications: Vec<Option<CancelToken>>,
    renewed: Vec<bool>,
}

impl Drop for DocumentRetirementPlan<'_> {
    fn drop(&mut self) {
        for notification in &mut self.notifications {
            if let Some(notification) = notification.take() {
                notification.cancel();
            }
        }
    }
}

impl<'a> RendererGenerationGatePlan<'a> {
    pub fn new(generations: &[&'a RendererGeneration]) -> Self {
        Self::from_generations(generations.to_vec())
    }

    pub fn from_generations(mut generations: Vec<&'a RendererGeneration>) -> Self {
        generations.sort_unstable_by_key(|generation| Arc::as_ptr(&generation.0));
        generations.dedup_by(|a, b| a.same_generation(b));
        let guards = Vec::with_capacity(generations.len());
        Self {
            generations,
            guards,
        }
    }

    pub fn prepare_retirement(
        &self,
        documents: &[&'a RendererGeneration],
    ) -> Result<DocumentRetirementPlan<'a>, String> {
        if !documents.iter().all(|document| {
            self.generations
                .iter()
                .any(|generation| generation.same_generation(document))
        }) {
            return Err("Document retirement was not admitted.".into());
        }
        let mut documents = documents.to_vec();
        documents.sort_unstable_by_key(|document| Arc::as_ptr(&document.0));
        documents.dedup_by(|a, b| a.same_generation(b));
        let count = documents.len();
        Ok(DocumentRetirementPlan {
            documents: documents.into_iter().map(Some).collect(),
            notifications: vec![None; count],
            renewed: vec![false; count],
        })
    }

    pub fn with_active<T>(&mut self, effect: impl FnOnce() -> T) -> Option<T> {
        let held = ClearGates(&mut self.guards);
        for &generation in &self.generations {
            held.0.push(
                generation
                    .0
                    .gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            );
        }
        if self
            .generations
            .iter()
            .all(|generation| generation.is_active())
        {
            Some(effect())
        } else {
            None
        }
    }

    pub fn with_active_retiring<T>(
        &mut self,
        retirement: &mut DocumentRetirementPlan<'a>,
        effect: impl FnOnce(&mut AdmittedDocumentRetirement<'_>) -> T,
    ) -> Option<T> {
        // A plan cannot be reused with a different, incomplete admission set.
        if !retirement.documents.iter().flatten().all(|document| {
            self.generations
                .iter()
                .any(|generation| generation.same_generation(document))
        }) {
            return None;
        }
        self.with_active(|| {
            effect(&mut AdmittedDocumentRetirement {
                documents: &retirement.documents,
                notifications: &mut retirement.notifications,
                renewed: &mut retirement.renewed,
            })
        })
    }
}

/// A terminal document transition may retire only its captured, already-locked
/// document generations. Notifications belong to the outermost admission caller.
pub struct AdmittedDocumentRetirement<'a> {
    documents: &'a [Option<&'a RendererGeneration>],
    notifications: &'a mut [Option<CancelToken>],
    renewed: &'a mut [bool],
}

impl AdmittedDocumentRetirement<'_> {
    /// Preflight every staged renewal before invalidating any participant.
    /// The caller stages this reference slice before entering admission.
    pub fn validate_owner_renewals(
        &self,
        prepared: &[&crate::ipc::PreparedDocumentOwner],
    ) -> Result<(), String> {
        self.validate_owner_renewals_iter(prepared.iter().copied())
    }

    /// Borrow staged successor slots only for preflight, so the same callback
    /// can subsequently consume them without a self-referential reference vector.
    pub fn validate_owner_renewals_iter<'b>(
        &self,
        prepared: impl Iterator<Item = &'b crate::ipc::PreparedDocumentOwner> + Clone,
    ) -> Result<(), String> {
        for (position, candidate) in prepared.clone().enumerate() {
            let index = self
                .documents
                .iter()
                .position(|document| {
                    document.is_some_and(|document| {
                        document.same_generation(&candidate.source_document)
                    })
                })
                .ok_or("Document renewal was not admitted.")?;
            if self.renewed[index]
                || self.notifications[index].is_some()
                || prepared.clone().take(position).any(|previous| {
                    previous
                        .source_document
                        .same_generation(&candidate.source_document)
                })
            {
                return Err("Document renewal participants are not distinct and unretired.".into());
            }
        }
        Ok(())
    }

    pub fn retire(&mut self, document: &RendererGeneration) -> Result<(), String> {
        let index = self
            .documents
            .iter()
            .position(|candidate| {
                candidate.is_some_and(|candidate| candidate.same_generation(document))
            })
            .ok_or("Document retirement was not admitted.")?;
        document.0.active.store(false, Ordering::SeqCst);
        self.notifications[index] = Some(document.loss_token());
        Ok(())
    }

    /// Admit a pre-subscribed successor after this exact old document retired.
    /// This performs no locking, allocation, lookup, or observer registration.
    pub fn renew_owner(
        &mut self,
        prepared: crate::ipc::PreparedDocumentOwner,
    ) -> Result<crate::ipc::NativeRequestOwner, String> {
        let index = self
            .documents
            .iter()
            .position(|candidate| {
                candidate
                    .is_some_and(|candidate| candidate.same_generation(&prepared.source_document))
            })
            .ok_or("Document renewal was not admitted.")?;
        if self.notifications[index].is_none() || self.renewed[index] {
            return Err("Document renewal requires one completed retirement.".into());
        }
        self.renewed[index] = true;
        Ok(prepared.owner)
    }
}

impl Default for RendererGeneration {
    fn default() -> Self {
        Self::new()
    }
}

impl RendererGeneration {
    pub fn new() -> Self {
        Self(Arc::new(RendererGenerationState {
            active: AtomicBool::new(true),
            gate: Mutex::new(()),
            loss: CancelToken::default(),
        }))
    }
    pub fn is_active(&self) -> bool {
        self.0.active.load(Ordering::SeqCst)
    }
    pub fn same_generation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn revoke(&self) {
        self.revoke_deferred().cancel();
    }

    /// Revoke under the gate and return a notification to fire after outer locks are released.
    pub fn revoke_deferred(&self) -> CancelToken {
        {
            let _guard = self.0.gate.lock().unwrap_or_else(|p| p.into_inner());
            self.0.active.store(false, Ordering::SeqCst);
        }
        self.0.loss.clone()
    }

    /// Fires once when this renderer generation is revoked.
    pub fn loss_token(&self) -> CancelToken {
        self.0.loss.clone()
    }

    /// Run only the final commit while this exact generation cannot be revoked.
    pub fn with_active<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        let _guard = self.0.gate.lock().unwrap_or_else(|p| p.into_inner());
        if self.is_active() {
            Some(commit())
        } else {
            None
        }
    }

    /// Acquire both generation gates in stable identity order, locking aliases once.
    pub fn with_active_pair<T>(&self, other: &Self, commit: impl FnOnce() -> T) -> Option<T> {
        Self::with_active_all([Some(self), Some(other)], commit)
    }

    pub fn with_active_all<T, const N: usize>(
        mut generations: [Option<&Self>; N],
        commit: impl FnOnce() -> T,
    ) -> Option<T> {
        generations.sort_unstable_by_key(|generation| {
            generation.map(|generation| Arc::as_ptr(&generation.0))
        });
        let _guards: [Option<_>; N] = std::array::from_fn(|index| {
            let generation = generations[index]?;
            if index > 0
                && generations[index - 1]
                    .is_some_and(|previous| previous.same_generation(generation))
            {
                return None;
            }
            Some(
                generation
                    .0
                    .gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            )
        });
        if generations
            .iter()
            .flatten()
            .all(|generation| generation.is_active())
        {
            Some(commit())
        } else {
            None
        }
    }

    /// Hold the same canonical gates as an ordinary commit, with a restricted
    /// retirement capability. Callers must notify only after ALL outer gates drop.
    pub fn with_active_all_retiring<T, const N: usize>(
        mut generations: [Option<&Self>; N],
        documents: [Option<&Self>; 2],
        notifications: &mut [Option<CancelToken>; 2],
        commit: impl FnOnce(&mut AdmittedDocumentRetirement<'_>) -> T,
    ) -> Option<T> {
        if !documents.iter().flatten().all(|document| {
            generations
                .iter()
                .flatten()
                .any(|generation| generation.same_generation(document))
        }) {
            return None;
        }
        generations.sort_unstable_by_key(|generation| {
            generation.map(|generation| Arc::as_ptr(&generation.0))
        });
        let _guards: [Option<_>; N] = std::array::from_fn(|index| {
            let generation = generations[index]?;
            if index > 0
                && generations[index - 1]
                    .is_some_and(|previous| previous.same_generation(generation))
            {
                return None;
            }
            Some(
                generation
                    .0
                    .gate
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            )
        });
        if !generations
            .iter()
            .flatten()
            .all(|generation| generation.is_active())
        {
            return None;
        }
        let mut renewed = [false, false];
        Some(commit(&mut AdmittedDocumentRetirement {
            documents: &documents,
            notifications,
            renewed: &mut renewed,
        }))
    }
}

/// A terminal observation for the original native browser reservation.
/// The numeric view is routing only; consumers must match the receipt against
/// their own captured reservation and owner before retiring actor state.
#[derive(Clone)]
pub struct NativeViewTerminal {
    pub view: ViewId,
    pub reservation: crate::ipc::NativeClosureReservation,
    pub owner: crate::ipc::NativeBrowserOwner,
    pub receipt: crate::ipc::NativeClosureReceipt,
}

/// Retain while the core listener must receive terminal observations.
pub struct NativeViewRetirementSubscription(Arc<AtomicBool>);
impl NativeViewRetirementSubscription {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
    pub fn is_active(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn state(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}
impl Default for NativeViewRetirementSubscription {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for NativeViewRetirementSubscription {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Core-thread native terminal notifications, separate from numeric view removal.
pub trait NativeViewRetirements: Send + Sync {
    fn on_native_terminal(
        &self,
        listener: Box<dyn Fn(NativeViewTerminal) -> Result<(), String> + Send + Sync>,
    ) -> NativeViewRetirementSubscription;
    /// Resolve only the browser owner captured by this authenticated ingress.
    /// The returned identity is not itself a live effect admission.
    fn authenticated_browser_owner(
        &self,
        owner: &crate::ipc::NativeRequestOwner,
    ) -> Option<crate::ipc::NativeBrowserOwner>;
    /// Drain retained listener deliveries on the core thread. A delivery ACK
    /// means only that listeners returned, not that their actor cleanup settled.
    fn drain_native_terminals(&self) -> Result<usize, String>;
    /// Number of native observations still awaiting listener delivery.
    fn pending_native_terminals(&self) -> Result<usize, String>;
    /// Request another core wake after a dropped or failed wakeup.
    fn wake_native_terminals(&self);
}
pub type NativeViewRetirementsHost = Arc<dyn NativeViewRetirements>;

/// The views the shell has, and their end
pub trait Views: Send + Sync {
    fn sender_of(&self, view: ViewId) -> Option<Sender>;
    fn all(&self) -> Vec<Sender>;
    /// `listener` runs on the core thread after a view is destroyed, once the
    /// shell has cancelled the view's pending calls
    fn on_destroyed(&self, listener: Box<dyn Fn(ViewId) + Send + Sync>);
    /// Current renderer document identity, if the view still exists.
    fn renderer_generation(&self, view: ViewId) -> Option<RendererGeneration>;
    /// On core, reports exactly the renderer generation that was replaced.
    fn on_renderer_replaced(&self, listener: Box<dyn Fn(ViewId, RendererGeneration) + Send + Sync>);
}

/// Electron's `app.getPath` names the handlers use. Paths stay where the
/// Electron build kept them (decisions 0013, 0019): the shell reads the same
/// userData folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathName {
    Home,
    AppData,
    UserData,
    Temp,
    Exe,
    Desktop,
    Documents,
    Downloads,
    Logs,
}

pub trait AppPaths: Send + Sync {
    fn path(&self, name: PathName) -> PathBuf;
}

/// What every batch's handlers may use. Dialogs, the OS shell, the
/// clipboard, web contents and each batch's own hosts are in `hosts`
/// ([`crate::hosts`]).
#[derive(Clone)]
pub struct Services {
    pub ui: Arc<dyn Thread>,
    pub core: Arc<dyn Thread>,
    pub workers: Arc<dyn Workers>,
    pub events: Arc<dyn EventSink>,
    pub views: Arc<dyn Views>,
    pub paths: Arc<dyn AppPaths>,
    pub hosts: Arc<crate::hosts::Hosts>,
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use std::sync::{Barrier, atomic::AtomicUsize};

    #[test]
    fn late_subscribers_are_notified_once_and_dropped_ones_are_not_called() {
        let token = CancelToken::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let dropped_calls = calls.clone();
        let dropped = token.on_cancel(move || {
            dropped_calls.fetch_add(100, Ordering::Relaxed);
        });
        drop(dropped);
        token.cancel();
        let late_calls = calls.clone();
        let _late = token.on_cancel(move || {
            late_calls.fetch_add(1, Ordering::Relaxed);
        });
        token.cancel();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn registering_while_cancellation_runs_does_not_lose_the_wakeup() {
        let token = CancelToken::default();
        let barrier = Arc::new(Barrier::new(2));
        let (send, receive) = mpsc::channel();
        let cancel = token.clone();
        let started = barrier.clone();
        let worker = std::thread::spawn(move || {
            started.wait();
            cancel.cancel();
        });
        barrier.wait();
        let _subscription = token.on_cancel(move || {
            send.send(()).unwrap();
        });
        worker.join().unwrap();
        receive
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn callbacks_can_reenter_the_token_without_holding_its_lock() {
        let token = CancelToken::default();
        let reentered = token.clone();
        let (send, receive) = mpsc::channel();
        let _subscription = token.on_cancel(move || {
            reentered.cancel();
            let _nested = reentered.on_cancel(move || {
                send.send(()).unwrap();
            });
        });
        token.cancel();
        receive
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn revocation_before_commit_refuses_the_commit() {
        let generation = RendererGeneration::new();
        generation.revoke();
        assert_eq!(generation.with_active(|| 1), None);
    }

    #[test]
    fn commit_before_revocation_completes_before_revoker() {
        let generation = RendererGeneration::new();
        let other = generation.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            other.with_active(|| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                7
            })
        });
        entered_rx.recv().unwrap();
        let revoker = generation.clone();
        let revoke = std::thread::spawn(move || revoker.revoke());
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), Some(7));
        revoke.join().unwrap();
        assert!(!generation.is_active());
    }

    #[test]
    fn cancelled_token_refuses_commit() {
        let token = CancelToken::default();
        token.cancel();
        assert_eq!(token.with_active(|| 1), None);
    }

    #[test]
    fn renderer_loss_notifies_existing_and_late_subscribers_once() {
        let generation = RendererGeneration::new();
        let loss = generation.loss_token();
        let (send, receive) = mpsc::channel();
        let first = send.clone();
        let _subscription = loss.on_cancel(move || first.send(1).unwrap());
        generation.revoke();
        generation.revoke();
        let _late = loss.on_cancel(move || send.send(2).unwrap());
        assert_eq!(receive.recv().unwrap(), 1);
        assert_eq!(receive.recv().unwrap(), 2);
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn terminal_retirement_invalidates_before_later_admission_and_notifies_after_unlock() {
        let renderer = RendererGeneration::new();
        let document = RendererGeneration::new();
        let observed = document.clone();
        let (send, receive) = mpsc::channel();
        let _subscription = document.loss_token().on_cancel(move || {
            // The notification can reenter this exact gate, but cannot admit work.
            send.send(observed.with_active(|| ())).unwrap();
        });
        let mut notifications = [None, None];
        let mut current_document = 1;
        let committed = RendererGeneration::with_active_all_retiring(
            [Some(&renderer), Some(&document)],
            [Some(&document), None],
            &mut notifications,
            |retirement| {
                retirement.retire(&document).unwrap();
                current_document = 2;
                assert!(!document.is_active());
                assert!(receive.try_recv().is_err());
            },
        );
        assert_eq!(committed, Some(()));
        assert_eq!(current_document, 2);
        assert_eq!(document.with_active(|| "old worker"), None);
        for notification in notifications.into_iter().flatten() {
            notification.cancel();
        }
        assert_eq!(receive.recv().unwrap(), None);
    }

    #[test]
    fn terminal_retirement_deduplicates_renderer_document_and_created_aliases() {
        let generation = RendererGeneration::new();
        let mut notifications = [None, None];
        assert_eq!(
            RendererGeneration::with_active_all_retiring(
                [Some(&generation), Some(&generation), Some(&generation)],
                [Some(&generation), Some(&generation)],
                &mut notifications,
                |retirement| {
                    retirement.retire(&generation).unwrap();
                    retirement.retire(&generation).unwrap();
                },
            ),
            Some(())
        );
        assert!(!generation.is_active());
        assert_eq!(notifications.iter().flatten().count(), 1);
        for notification in notifications.into_iter().flatten() {
            notification.cancel();
        }
    }

    #[test]
    fn terminal_retirement_cannot_retire_an_uncaptured_or_unlocked_document() {
        let document = RendererGeneration::new();
        let unrelated = RendererGeneration::new();
        let mut notifications = [None, None];
        assert_eq!(
            RendererGeneration::with_active_all_retiring(
                [Some(&document)],
                [Some(&document), None],
                &mut notifications,
                |retirement| retirement.retire(&unrelated).is_err(),
            ),
            Some(true)
        );
        assert!(unrelated.is_active());
        assert!(document.is_active());
        assert!(
            RendererGeneration::with_active_all_retiring(
                [Some(&document)],
                [Some(&unrelated), None],
                &mut notifications,
                |_| panic!("unlocked retirement must not be admitted"),
            )
            .is_none()
        );
        assert!(notifications.iter().all(Option::is_none));
    }

    #[test]
    fn many_document_transition_retires_every_participant_without_a_two_owner_cap() {
        let documents: Vec<_> = (0..5).map(|_| RendererGeneration::new()).collect();
        let refs: Vec<_> = documents.iter().collect();
        let mut admission = RendererGenerationGatePlan::new(&refs);
        let mut retirement = admission.prepare_retirement(&refs).unwrap();
        let mut published = false;
        assert_eq!(
            admission.with_active_retiring(&mut retirement, |capability| {
                for document in &documents {
                    capability.retire(document).unwrap();
                }
                published = true;
            }),
            Some(())
        );
        assert!(published);
        assert!(documents.iter().all(|document| !document.is_active()));
        assert!(
            documents
                .iter()
                .all(|document| !document.loss_token().is_cancelled())
        );
        drop(retirement);
        assert!(
            documents
                .iter()
                .all(|document| document.loss_token().is_cancelled())
        );
    }

    #[test]
    fn many_document_refusal_keeps_other_participants_live_and_does_not_publish() {
        let documents: Vec<_> = (0..4).map(|_| RendererGeneration::new()).collect();
        let refs: Vec<_> = documents.iter().collect();
        let mut admission = RendererGenerationGatePlan::new(&refs);
        let mut retirement = admission.prepare_retirement(&refs).unwrap();
        documents[2].revoke();
        assert!(
            admission
                .with_active_retiring(&mut retirement, |_| panic!("revoked participant"))
                .is_none()
        );
        assert!(documents[0].is_active() && documents[1].is_active() && documents[3].is_active());
    }

    #[test]
    fn prepared_generation_plan_reuses_alias_guard_storage_after_return_and_unwind() {
        let first = RendererGeneration::new();
        let second = RendererGeneration::new();
        let mut plan = RendererGenerationGatePlan::new(&[&second, &first, &first]);
        let capacity = plan.guards.capacity();
        assert_eq!(plan.generations.len(), 2);
        assert_eq!(plan.with_active(|| 7), Some(7));
        assert!(plan.guards.is_empty());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            plan.with_active::<()>(|| panic!("unwind prepared guards"));
        }));
        assert!(result.is_err());
        assert!(plan.guards.is_empty());
        assert_eq!(plan.guards.capacity(), capacity);
        assert_eq!(plan.with_active(|| 8), Some(8));
        assert_eq!(plan.guards.capacity(), capacity);
    }

    #[test]
    fn renewal_slot_preflight_releases_its_borrow_before_consuming_every_successor() {
        use crate::ipc::{NativeContext, NativeRequestOwner};
        let documents: Vec<_> = (0..4).map(|_| RendererGeneration::new()).collect();
        let owners: Vec<_> = documents
            .iter()
            .enumerate()
            .map(|(index, document)| NativeRequestOwner {
                view: ViewId(index as u32),
                context: NativeContext::with_owner_loss(
                    (),
                    RendererGeneration::new(),
                    CancelToken::default(),
                )
                .with_document_lease(document.clone()),
            })
            .collect();
        let mut successors: Vec<_> = owners
            .iter()
            .map(|owner| Some(owner.prepare_document_successor().unwrap()))
            .collect();
        let refs: Vec<_> = documents.iter().collect();
        let mut admission = RendererGenerationGatePlan::new(&refs);
        let mut retirement = admission.prepare_retirement(&refs).unwrap();
        let mut published = Vec::with_capacity(documents.len());
        let capacity = published.capacity();
        assert_eq!(
            admission.with_active_retiring(&mut retirement, |capability| {
                capability
                    .validate_owner_renewals_iter(successors.iter().filter_map(Option::as_ref))
                    .unwrap();
                for (document, successor) in documents.iter().zip(&mut successors) {
                    capability.retire(document).unwrap();
                    published.push(capability.renew_owner(successor.take().unwrap()).unwrap());
                }
            }),
            Some(())
        );
        assert!(successors.iter().all(Option::is_none));
        assert_eq!(published.len(), documents.len());
        assert_eq!(published.capacity(), capacity);
        drop(retirement);
        assert!(
            owners
                .iter()
                .all(|owner| owner.loss_token().unwrap().is_cancelled())
        );
        assert!(
            published
                .iter()
                .all(|owner| !owner.loss_token().unwrap().is_cancelled())
        );
    }

    #[test]
    fn explicit_cancel_after_renderer_retirement_still_wakes_the_same_alias() {
        let token = CancelToken::default();
        let alias = token.clone();
        let external = token.external_loss_token();
        let (send, receive) = mpsc::channel();
        let _subscription = external.on_cancel(move || send.send(()).unwrap());
        token.cancel_renderer_retired();
        assert!(token.is_cancelled());
        assert!(!external.is_cancelled());
        assert!(receive.try_recv().is_err());
        alias.cancel();
        alias.cancel();
        assert!(external.is_cancelled());
        receive.recv().unwrap();
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn renderer_retirement_cannot_erase_earlier_explicit_cancellation() {
        let token = CancelToken::default();
        token.cancel();
        token.cancel_renderer_retired();
        assert!(token.external_loss_token().is_cancelled());
        assert_eq!(token.with_active(|| ()), None);
    }

    #[test]
    fn independent_cancellation_callbacks_can_reenter_the_original_alias() {
        let token = CancelToken::default();
        let external = token.external_loss_token();
        let alias = token.clone();
        let (send, receive) = mpsc::channel();
        let _subscription = external.on_cancel(move || {
            alias.cancel();
            send.send(()).unwrap();
        });
        token.cancel_renderer_retired();
        token.cancel();
        receive.recv().unwrap();
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn racing_external_signal_subscription_and_retirement_does_not_lose_explicit_cancel() {
        let token = CancelToken::default();
        let barrier = Arc::new(Barrier::new(3));
        let subscriber = token.clone();
        let subscribed = barrier.clone();
        let (send, receive) = mpsc::channel();
        let subscribe = std::thread::spawn(move || {
            subscribed.wait();
            subscriber
                .external_loss_token()
                .on_cancel(move || send.send(()).unwrap())
        });
        let cancel = token.clone();
        let started = barrier.clone();
        let worker = std::thread::spawn(move || {
            started.wait();
            cancel.cancel_renderer_retired();
            cancel.cancel();
        });
        barrier.wait();
        let _subscription = subscribe.join().unwrap();
        worker.join().unwrap();
        receive
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(receive.try_recv().is_err());
    }
}
