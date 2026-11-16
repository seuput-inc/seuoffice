//! Creation, close and effect-admission state machines for browsers and windows.
//!
//! Ported from the CEF runtime's `browser_close.rs` and `native_window_close.rs` without
//! their CEF-specific parts. Contract changes:
//!
//! - `BrowserCloseOutcome::Retired` means: the close was dispatched through
//!   `Webview::close()`, the label is gone from Tauri's registry (checked on the UI thread)
//!   and the shell authorities were revoked. It is not process exit and not deallocation.
//! - `NotCreated` is minted only when a reservation is dropped unclaimed or the build failed,
//!   never after `build()` returned `Ok`.
//! - `WindowCloseOutcome::Destroyed` is `WindowEvent::Destroyed` after the last admitted
//!   effect finished. `CloseRequested` is cancellable, so it never closes admission; the
//!   shell calls [`WindowClose::begin_close`] when it accepts a close.
//!
//! Callbacks never run under a state lock; they go through `defer`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::defer::defer;
use super::ids::CreationId;

// ---------------------------------------------------------------------------------------
// Browser
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserCloseOutcome {
    /// Closed through `Webview::close()` and gone from Tauri's registry.
    Retired,
    /// Creation failed or was abandoned before any webview existed.
    NotCreated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserCreationOutcome {
    Attached,
    Failed,
}

struct CreationListener {
    active: AtomicBool,
    callback: Mutex<Option<Box<dyn FnOnce(BrowserCreationOutcome) + Send>>>,
}

pub struct BrowserCreationSubscription {
    listener: Arc<CreationListener>,
}

impl Drop for BrowserCreationSubscription {
    fn drop(&mut self) {
        self.listener.active.store(false, Ordering::Release);
    }
}

type CloseCallback = Box<dyn FnOnce(BrowserCloseOutcome) + Send>;

struct Listener {
    active: AtomicBool,
    callback: Mutex<Option<CloseCallback>>,
}

type RequestClose = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

#[derive(Default)]
struct BrowserState {
    terminal: Option<BrowserCloseOutcome>,
    listeners: Vec<Arc<Listener>>,
    creation: Option<BrowserCreationOutcome>,
    creation_listeners: Vec<Arc<CreationListener>>,
    request: Option<RequestClose>,
    claimed: bool,
    submissions: usize,
    requested: bool,
}

/// The close receipt of one reserved browser, available before the webview exists.
#[derive(Clone, Default)]
pub struct BrowserClose {
    state: Arc<Mutex<BrowserState>>,
}

impl std::fmt::Debug for BrowserClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserClose").finish_non_exhaustive()
    }
}

pub struct BrowserCloseSubscription {
    listener: Arc<Listener>,
}

impl Drop for BrowserCloseSubscription {
    fn drop(&mut self) {
        self.listener.active.store(false, Ordering::Release);
    }
}

fn deliver(listener: Arc<Listener>, outcome: BrowserCloseOutcome) {
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
            callback(outcome);
        }
    });
}

fn deliver_creation(listener: Arc<CreationListener>, outcome: BrowserCreationOutcome) {
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
            callback(outcome);
        }
    });
}

/// One live reference to a reservation. The webview builder's hooks hold clones; when the
/// last clone drops without the browser having been claimed, creation failed.
#[derive(Clone)]
pub(crate) struct BrowserSubmission(Arc<Submission>);

struct Submission {
    close: BrowserClose,
    creation: CreationId,
}

impl BrowserSubmission {
    fn new(close: BrowserClose, creation: CreationId) -> Self {
        {
            let mut state = close
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.submissions = state
                .submissions
                .checked_add(1)
                .expect("native submission count exhausted");
        }
        Self(Arc::new(Submission { close, creation }))
    }

    pub(crate) fn close(&self) -> BrowserClose {
        self.0.close.clone()
    }

    pub(crate) fn creation(&self) -> CreationId {
        self.0.creation
    }
}

impl Drop for Submission {
    fn drop(&mut self) {
        self.close.release_submission();
    }
}

/// An owned, unsubmitted creation reservation. Moving it into a [`NativeSpec`] transfers
/// ownership; dropping the last unclaimed submission proves failure.
///
/// [`NativeSpec`]: super::ext::NativeSpec
pub struct BrowserCreation(BrowserSubmission);

impl BrowserCreation {
    /// `None` only when the creation counter is exhausted.
    pub fn new() -> Option<Self> {
        Some(Self(BrowserSubmission::new(
            BrowserClose::default(),
            CreationId::mint()?,
        )))
    }

    pub fn receipt(&self) -> BrowserClose {
        self.0.close()
    }

    pub(crate) fn into_submission(self) -> BrowserSubmission {
        self.0
    }
}

impl BrowserClose {
    /// Request destruction of this exact reserved browser, even before attachment. The
    /// terminal receipt is still delivered only after the label left Tauri's registry or a
    /// proven failure before creation.
    pub fn request_close(&self) -> Result<(), String> {
        let request = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Native browser close state was poisoned.")?;
            if state.terminal.is_some() {
                return Ok(());
            }
            state.requested = true;
            state.request.clone()
        };
        if let Some(request) = request {
            request()?;
        }
        Ok(())
    }

    fn release_submission(&self) {
        let listeners = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.submissions = state.submissions.saturating_sub(1);
            if state.submissions != 0 || state.terminal.is_some() || state.claimed {
                return;
            }
            state.terminal = Some(BrowserCloseOutcome::NotCreated);
            std::mem::take(&mut state.listeners)
        };
        self.complete_creation(BrowserCreationOutcome::Failed);
        for listener in listeners {
            deliver(listener, BrowserCloseOutcome::NotCreated);
        }
    }

    /// The browser now exists; abandonment no longer means "not created".
    pub(crate) fn claim(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Native browser close state was poisoned.")?;
        if state.terminal.is_some() || state.claimed {
            return Err("Native browser close reservation was already used.".into());
        }
        state.claimed = true;
        Ok(())
    }

    pub(crate) fn bind_request(&self, request: RequestClose) -> Result<(), String> {
        let requested = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Native browser close state was poisoned.")?;
            if state.terminal.is_some() || !state.claimed || state.request.is_some() {
                return Err("Native browser close reservation was already used.".into());
            }
            state.request = Some(request.clone());
            state.requested
        };
        if requested {
            request()?;
        }
        Ok(())
    }

    pub fn same_reservation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub fn when_created(
        &self,
        callback: impl FnOnce(BrowserCreationOutcome) + Send + 'static,
    ) -> Result<BrowserCreationSubscription, String> {
        let listener = Arc::new(CreationListener {
            active: AtomicBool::new(true),
            callback: Mutex::new(Some(Box::new(callback))),
        });
        let result = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Native browser creation state was poisoned.")?;
            if state.creation.is_none() {
                state
                    .creation_listeners
                    .retain(|l| l.active.load(Ordering::Acquire));
                state.creation_listeners.push(listener.clone());
            }
            state.creation
        };
        if let Some(result) = result {
            deliver_creation(listener.clone(), result);
        }
        Ok(BrowserCreationSubscription { listener })
    }

    pub(crate) fn complete_creation(&self, outcome: BrowserCreationOutcome) {
        let listeners = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.creation.is_some() {
                return;
            }
            state.creation = Some(outcome);
            std::mem::take(&mut state.creation_listeners)
        };
        for listener in listeners {
            deliver_creation(listener, outcome);
        }
    }

    pub fn when_closed(
        &self,
        callback: impl FnOnce(BrowserCloseOutcome) + Send + 'static,
    ) -> Result<BrowserCloseSubscription, String> {
        let listener = Arc::new(Listener {
            active: AtomicBool::new(true),
            callback: Mutex::new(Some(Box::new(callback))),
        });
        let terminal = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Native browser close state was poisoned.")?;
            if state.terminal.is_none() {
                state.listeners.retain(|l| l.active.load(Ordering::Acquire));
                state.listeners.push(listener.clone());
            }
            state.terminal
        };
        if let Some(outcome) = terminal {
            deliver(listener.clone(), outcome);
        }
        Ok(BrowserCloseSubscription { listener })
    }

    /// The first terminal outcome wins; later ones are ignored.
    pub(crate) fn complete(&self, outcome: BrowserCloseOutcome) {
        self.complete_creation(BrowserCreationOutcome::Failed);
        let listeners = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.terminal.is_some() {
                return;
            }
            state.terminal = Some(outcome);
            state.request = None;
            std::mem::take(&mut state.listeners)
        };
        for listener in listeners {
            deliver(listener, outcome);
        }
    }

    #[cfg(test)]
    pub(crate) fn outcomes(&self) -> (Option<BrowserCreationOutcome>, Option<BrowserCloseOutcome>) {
        let state = self.state.lock().unwrap();
        (state.creation, state.terminal)
    }
}

// ---------------------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------------------

/// Native closure of the original window, not physical deallocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowCloseOutcome {
    NotCreated,
    Destroyed,
}

type WindowCallback = Box<dyn FnOnce(WindowCloseOutcome) + Send>;

struct WindowListener {
    active: AtomicBool,
    callback: Mutex<Option<WindowCallback>>,
}

pub struct WindowCloseSubscription {
    listener: Arc<WindowListener>,
}

impl Drop for WindowCloseSubscription {
    fn drop(&mut self) {
        self.listener.active.store(false, Ordering::Release);
    }
}

struct ClosingListener {
    active: AtomicBool,
    callback: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

pub struct WindowClosingSubscription {
    listener: Arc<ClosingListener>,
}

impl Drop for WindowClosingSubscription {
    fn drop(&mut self) {
        self.listener.active.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct WindowState {
    claimed: bool,
    allocated: bool,
    closing: bool,
    destroyed: bool,
    active: usize,
    outcome: Option<WindowCloseOutcome>,
    listeners: Vec<Arc<WindowListener>>,
    closing_listeners: Vec<Arc<ClosingListener>>,
}

#[derive(Clone, Default)]
pub struct WindowClose {
    state: Arc<Mutex<WindowState>>,
}

impl std::fmt::Debug for WindowClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowClose").finish_non_exhaustive()
    }
}

/// An owned, unsubmitted window reservation; dropping it unclaimed proves "not created".
pub struct WindowCreation {
    submission: Option<WindowSubmission>,
}

/// Clones live in the window builder's hooks; the last drop of an unclaimed reservation
/// reports `NotCreated`.
#[derive(Clone)]
pub(crate) struct WindowSubmission(Arc<WindowSubmissionInner>);

struct WindowSubmissionInner {
    close: WindowClose,
    creation: CreationId,
}

/// One admitted operation on an exact window. Hold it until the operation completes;
/// retaining a window handle is not itself an effect.
pub struct WindowEffect {
    close: WindowClose,
}

fn deliver_window(listener: Arc<WindowListener>, outcome: WindowCloseOutcome) {
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
            callback(outcome);
        }
    });
}

fn deliver_window_all(listeners: Vec<Arc<WindowListener>>, outcome: WindowCloseOutcome) {
    for listener in listeners {
        deliver_window(listener, outcome);
    }
}

fn deliver_closing(listeners: Vec<Arc<ClosingListener>>) {
    for listener in listeners {
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
}

fn finish(state: &mut WindowState, outcome: WindowCloseOutcome) -> Vec<Arc<WindowListener>> {
    if state.outcome.is_some() {
        return Vec::new();
    }
    state.outcome = Some(outcome);
    std::mem::take(&mut state.listeners)
}

impl Drop for WindowSubmissionInner {
    fn drop(&mut self) {
        self.close.abandon();
    }
}

impl WindowSubmission {
    pub(crate) fn close(&self) -> WindowClose {
        self.0.close.clone()
    }

    pub(crate) fn creation(&self) -> CreationId {
        self.0.creation
    }
}

impl WindowCreation {
    /// `None` only when the creation counter is exhausted.
    pub fn new() -> Option<Self> {
        let inner = WindowSubmissionInner {
            close: WindowClose::default(),
            creation: CreationId::mint()?,
        };
        Some(Self {
            submission: Some(WindowSubmission(Arc::new(inner))),
        })
    }

    pub fn receipt(&self) -> WindowClose {
        self.submission
            .as_ref()
            .expect("window reservation consumed")
            .close()
    }

    pub fn creation_id(&self) -> CreationId {
        self.submission
            .as_ref()
            .expect("window reservation consumed")
            .creation()
    }

    pub(crate) fn submit(mut self) -> WindowSubmission {
        self.submission.take().expect("window reservation consumed")
    }
}

impl WindowClose {
    /// Observe closure admission revocation, including an already closing window. This is
    /// not the later destroy acknowledgement.
    pub fn when_closing(
        &self,
        callback: impl FnOnce() + Send + 'static,
    ) -> Result<WindowClosingSubscription, String> {
        let listener = Arc::new(ClosingListener {
            active: AtomicBool::new(true),
            callback: Mutex::new(Some(Box::new(callback))),
        });
        let closing = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "native window state poisoned")?;
            if !state.closing {
                state
                    .closing_listeners
                    .retain(|l| l.active.load(Ordering::Acquire));
                state.closing_listeners.push(listener.clone());
            }
            state.closing
        };
        if closing {
            deliver_closing(vec![listener.clone()]);
        }
        Ok(WindowClosingSubscription { listener })
    }

    pub fn when_closed(
        &self,
        callback: impl FnOnce(WindowCloseOutcome) + Send + 'static,
    ) -> Result<WindowCloseSubscription, String> {
        let listener = Arc::new(WindowListener {
            active: AtomicBool::new(true),
            callback: Mutex::new(Some(Box::new(callback))),
        });
        let outcome = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "native window state poisoned")?;
            if state.outcome.is_none() {
                state.listeners.retain(|l| l.active.load(Ordering::Acquire));
                state.listeners.push(listener.clone());
            }
            state.outcome
        };
        if let Some(outcome) = outcome {
            deliver_window(listener.clone(), outcome);
        }
        Ok(WindowCloseSubscription { listener })
    }

    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub fn is_open(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| !state.closing && state.outcome.is_none())
    }

    pub(crate) fn claim(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.claimed || state.closing || state.outcome.is_some() {
            return false;
        }
        state.claimed = true;
        true
    }

    /// The native window now exists.
    pub(crate) fn allocated(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if !state.claimed || state.allocated || state.closing || state.outcome.is_some() {
            return false;
        }
        state.allocated = true;
        true
    }

    fn abandon(&self) {
        let (closing, listeners) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.claimed || state.allocated {
                return;
            }
            state.closing = true;
            let closing = std::mem::take(&mut state.closing_listeners);
            let listeners = if state.active == 0 {
                finish(&mut state, WindowCloseOutcome::NotCreated)
            } else {
                Vec::new()
            };
            (closing, listeners)
        };
        deliver_closing(closing);
        deliver_window_all(listeners, WindowCloseOutcome::NotCreated);
    }

    /// The window build failed before a native window existed.
    pub(crate) fn not_created(&self) {
        let (closing, listeners) = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.allocated {
                return;
            }
            state.closing = true;
            let closing = std::mem::take(&mut state.closing_listeners);
            let listeners = if state.active == 0 {
                finish(&mut state, WindowCloseOutcome::NotCreated)
            } else {
                Vec::new()
            };
            (closing, listeners)
        };
        deliver_closing(closing);
        deliver_window_all(listeners, WindowCloseOutcome::NotCreated);
    }

    /// The shell accepted a close: no new effect is admitted from here on.
    pub fn begin_close(&self) {
        let closing = if let Ok(mut state) = self.state.lock() {
            state.closing = true;
            std::mem::take(&mut state.closing_listeners)
        } else {
            Vec::new()
        };
        deliver_closing(closing);
    }

    /// Admit one effect on this reservation. Admission may precede allocation; the operation
    /// must check [`WindowEffect::is_current`] on the UI thread right before touching the
    /// native window.
    pub fn admit_effect(&self) -> Option<WindowEffect> {
        let mut state = self.state.lock().ok()?;
        if state.closing || state.outcome.is_some() {
            return None;
        }
        state.active = state.active.checked_add(1)?;
        Some(WindowEffect {
            close: self.clone(),
        })
    }

    /// `WindowEvent::Destroyed` was observed. The receipt completes after the last admitted
    /// effect finished.
    pub(crate) fn destroyed(&self) {
        let listeners = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if !state.allocated {
                return;
            }
            state.closing = true;
            state.destroyed = true;
            if state.active != 0 {
                return;
            }
            finish(&mut state, WindowCloseOutcome::Destroyed)
        };
        deliver_window_all(listeners, WindowCloseOutcome::Destroyed);
    }

    #[cfg(test)]
    pub(crate) fn outcome(&self) -> Option<WindowCloseOutcome> {
        self.state.lock().unwrap().outcome
    }
}

impl Drop for WindowEffect {
    fn drop(&mut self) {
        let (listeners, outcome) = {
            let Ok(mut state) = self.close.state.lock() else {
                return;
            };
            state.active -= 1;
            if state.active != 0 {
                return;
            }
            let outcome = if state.closing && !state.allocated {
                WindowCloseOutcome::NotCreated
            } else if state.destroyed {
                WindowCloseOutcome::Destroyed
            } else {
                return;
            };
            (finish(&mut state, outcome), outcome)
        };
        deliver_window_all(listeners, outcome);
    }
}

impl WindowEffect {
    /// True only while the original window is allocated and has not begun closing.
    pub fn is_current(&self) -> bool {
        self.close
            .state
            .lock()
            .is_ok_and(|state| state.allocated && !state.closing && state.outcome.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_close_and_late_subscribe_report_the_original_terminal_once() {
        let receipt = BrowserClose::default();
        receipt.complete(BrowserCloseOutcome::NotCreated);
        receipt.complete(BrowserCloseOutcome::Retired);
        assert_eq!(receipt.outcomes().1, Some(BrowserCloseOutcome::NotCreated));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let _late = receipt
            .when_closed(move |outcome| sink.lock().unwrap().push(outcome))
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), [BrowserCloseOutcome::NotCreated]);
        assert!(BrowserClose::default().outcomes().1.is_none());
    }

    #[test]
    fn a_browser_reservation_can_only_be_claimed_once() {
        let receipt = BrowserClose::default();
        assert!(receipt.claim().is_ok());
        assert!(receipt.clone().claim().is_err());
    }

    #[test]
    fn an_abandoned_unclaimed_reservation_fails_creation_and_close() {
        let creation = BrowserCreation::new().unwrap();
        let receipt = creation.receipt();
        let hook = creation.into_submission();
        let other_hook = hook.clone();
        drop(hook);
        assert_eq!(receipt.outcomes(), (None, None));
        drop(other_hook);
        assert_eq!(
            receipt.outcomes(),
            (
                Some(BrowserCreationOutcome::Failed),
                Some(BrowserCloseOutcome::NotCreated)
            )
        );
    }

    #[test]
    fn a_claimed_reservation_is_never_reported_not_created() {
        let creation = BrowserCreation::new().unwrap();
        let receipt = creation.receipt();
        let hook = creation.into_submission();
        receipt.claim().unwrap();
        drop(hook);
        assert_eq!(receipt.outcomes(), (None, None));
        receipt.complete(BrowserCloseOutcome::Retired);
        assert_eq!(receipt.outcomes().1, Some(BrowserCloseOutcome::Retired));
    }

    #[test]
    fn request_close_before_binding_runs_when_the_request_is_bound() {
        let receipt = BrowserClose::default();
        receipt.request_close().unwrap();
        receipt.claim().unwrap();
        let calls = Arc::new(Mutex::new(0));
        let counted = calls.clone();
        receipt
            .bind_request(Arc::new(move || {
                *counted.lock().unwrap() += 1;
                Ok(())
            }))
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn an_unsubmitted_window_is_not_created_but_an_allocated_one_is_not() {
        let creation = WindowCreation::new().unwrap();
        let close = creation.receipt();
        drop(creation);
        assert_eq!(close.outcome(), Some(WindowCloseOutcome::NotCreated));

        let creation = WindowCreation::new().unwrap();
        let close = creation.receipt();
        let submission = creation.submit();
        assert!(close.claim());
        assert!(close.allocated());
        close.not_created();
        drop(submission);
        assert_eq!(close.outcome(), None);
    }

    #[test]
    fn destroy_waits_for_the_last_admitted_effect() {
        let creation = WindowCreation::new().unwrap();
        let close = creation.receipt();
        let _submission = creation.submit();
        assert!(close.claim());
        assert!(close.allocated());
        let effect = close.admit_effect().unwrap();
        assert!(effect.is_current());
        close.begin_close();
        assert!(close.admit_effect().is_none());
        assert!(!effect.is_current());
        close.destroyed();
        assert_eq!(close.outcome(), None);
        drop(effect);
        assert_eq!(close.outcome(), Some(WindowCloseOutcome::Destroyed));
        close.destroyed();
        assert_eq!(close.outcome(), Some(WindowCloseOutcome::Destroyed));
    }

    #[test]
    fn unallocated_failure_waits_for_previously_admitted_work() {
        let creation = WindowCreation::new().unwrap();
        let close = creation.receipt();
        let effect = close.admit_effect().unwrap();
        drop(creation);
        assert!(close.admit_effect().is_none());
        assert_eq!(close.outcome(), None);
        drop(effect);
        assert_eq!(close.outcome(), Some(WindowCloseOutcome::NotCreated));
    }

    #[test]
    fn closing_subscribers_hear_a_close_that_already_began() {
        let creation = WindowCreation::new().unwrap();
        let close = creation.receipt();
        let _submission = creation.submit();
        assert!(close.claim() && close.allocated());
        close.begin_close();
        let heard = Arc::new(Mutex::new(false));
        let flag = heard.clone();
        let _subscription = close
            .when_closing(move || *flag.lock().unwrap() = true)
            .unwrap();
        assert!(*heard.lock().unwrap());
        assert!(!close.is_open());
    }

    #[test]
    fn unrelated_reservations_never_compare_equal() {
        let first = WindowCreation::new().unwrap();
        let second = WindowCreation::new().unwrap();
        assert!(first.receipt().same(&first.receipt()));
        assert!(!first.receipt().same(&second.receipt()));
    }
}
