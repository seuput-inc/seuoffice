// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};

/// Native API closure of the original window, not physical memory deallocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeWindowCloseOutcome { NotCreated, NativeClosed }
type Callback = Box<dyn FnOnce(NativeWindowCloseOutcome) + Send>;
struct Listener { active: AtomicBool, callback: Mutex<Option<Callback>> }
pub struct NativeWindowCloseSubscription { listener: Arc<Listener> }
impl Drop for NativeWindowCloseSubscription {
  fn drop(&mut self) { self.listener.active.store(false, Ordering::Release); }
}
struct ClosingListener { active: AtomicBool, callback: Mutex<Option<Box<dyn FnOnce() + Send>>> }
pub struct NativeWindowClosingSubscription { listener: Arc<ClosingListener> }
impl Drop for NativeWindowClosingSubscription {
  fn drop(&mut self) { self.listener.active.store(false, Ordering::Release); }
}
#[derive(Default)]
struct State {
  claimed: bool,
  allocated: bool,
  closing: bool,
  observed: bool,
  preclose_required: bool,
  preclose_started: bool,
  preclose_result: Option<bool>,
  active: usize,
  // Enabled only after every native-effect ingress uses this reservation.
  effects_connected: bool,
  outcome: Option<NativeWindowCloseOutcome>,
  listeners: Vec<Arc<Listener>>,
  closing_listeners: Vec<Arc<ClosingListener>>,
}
#[derive(Clone, Default)]
pub struct NativeWindowClose { state: Arc<Mutex<State>> }
pub struct NativeWindowSubmissionGuard { submission: Option<NativeWindowSubmission> }
#[derive(Clone)]
pub(crate) struct NativeWindowSubmission(Arc<Submission>);
impl std::fmt::Debug for NativeWindowSubmission {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.debug_struct("NativeWindowSubmission").finish_non_exhaustive()
  }
}
struct Submission { close: NativeWindowClose }
/// One admitted operation on an exact native window. Hold it until that
/// operation completes; retaining a window handle is not itself an effect.
pub struct NativeWindowEffect { close: NativeWindowClose }

fn deliver(listener: Arc<Listener>, outcome: NativeWindowCloseOutcome) {
  crate::native_evaluation::defer_completion(Box::new(move || {
    if !listener.active.swap(false, Ordering::AcqRel) { return; }
    let callback = listener.callback.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(callback) = callback { callback(outcome); }
  }));
}
fn finish(state: &mut State, outcome: NativeWindowCloseOutcome) -> Vec<Arc<Listener>> {
  if state.outcome.is_some() { return Vec::new(); }
  state.outcome = Some(outcome);
  std::mem::take(&mut state.listeners)
}
fn deliver_all(listeners: Vec<Arc<Listener>>, outcome: NativeWindowCloseOutcome) {
  for listener in listeners { deliver(listener, outcome); }
}
fn deliver_closing(listeners: Vec<Arc<ClosingListener>>) {
  for listener in listeners {
    crate::native_evaluation::defer_completion(Box::new(move || {
      if !listener.active.swap(false, Ordering::AcqRel) { return; }
      let callback = listener.callback.lock().unwrap_or_else(|e| e.into_inner()).take();
      if let Some(callback) = callback { callback(); }
    }));
  }
}
impl Drop for Submission {
  fn drop(&mut self) {
    let (closing, listeners) = {
      let Ok(mut state) = self.close.state.lock() else { return };
      if state.claimed || state.allocated { return; }
      state.closing = true;
      let closing = std::mem::take(&mut state.closing_listeners);
      let listeners = if state.active == 0 { finish(&mut state, NativeWindowCloseOutcome::NotCreated) } else { Vec::new() };
      (closing, listeners)
    };
    deliver_closing(closing);
    deliver_all(listeners, NativeWindowCloseOutcome::NotCreated);
  }
}
impl NativeWindowSubmission {
  pub(crate) fn close(&self) -> NativeWindowClose { self.0.close.clone() }
}
impl NativeWindowSubmissionGuard {
  pub fn new() -> Self { Self { submission: Some(NativeWindowSubmission(Arc::new(Submission { close: NativeWindowClose::default() }))) } }
  pub fn receipt(&self) -> NativeWindowClose { self.submission.as_ref().expect("guard consumed").close() }
  pub(crate) fn submit(mut self) -> NativeWindowSubmission { self.submission.take().expect("guard consumed") }
}
impl Default for NativeWindowSubmissionGuard { fn default() -> Self { Self::new() } }
impl NativeWindowClose {
  /// Observe closure admission revocation, including an already-closing window.
  /// This is not the later native-close acknowledgement.
  pub fn when_closing(&self, callback: impl FnOnce() + Send + 'static) -> Result<NativeWindowClosingSubscription, String> {
    let listener = Arc::new(ClosingListener { active: AtomicBool::new(true), callback: Mutex::new(Some(Box::new(callback))) });
    let closing = {
      let mut state = self.state.lock().map_err(|_| "native window state poisoned")?;
      if !state.closing {
        state.closing_listeners.retain(|entry| entry.active.load(Ordering::Acquire));
        state.closing_listeners.push(listener.clone());
      }
      state.closing
    };
    if closing { deliver_closing(vec![listener.clone()]); }
    Ok(NativeWindowClosingSubscription { listener })
  }
  pub fn when_closed(&self, callback: impl FnOnce(NativeWindowCloseOutcome) + Send + 'static) -> Result<NativeWindowCloseSubscription, String> {
    let listener = Arc::new(Listener { active: AtomicBool::new(true), callback: Mutex::new(Some(Box::new(callback))) });
    let outcome = {
      let mut state = self.state.lock().map_err(|_| "native window state poisoned")?;
      if state.outcome.is_none() {
        state.listeners.retain(|entry| entry.active.load(Ordering::Acquire));
        state.listeners.push(listener.clone());
      }
      state.outcome
    };
    if let Some(outcome) = outcome { deliver(listener.clone(), outcome); }
    Ok(NativeWindowCloseSubscription { listener })
  }
  pub(crate) fn same(&self, other: &Self) -> bool { Arc::ptr_eq(&self.state, &other.state) }
  pub(crate) fn is_open(&self) -> bool {
    self.state.lock().is_ok_and(|state| !state.closing && state.outcome.is_none())
  }
  pub(crate) fn claim(&self) -> bool {
    let Ok(mut state) = self.state.lock() else { return false };
    if state.claimed || state.closing || state.outcome.is_some() { return false; }
    state.claimed = true;
    true
  }
  pub(crate) fn allocated(&self) -> bool {
    let Ok(mut state) = self.state.lock() else { return false };
    if !state.claimed || state.allocated || state.closing || state.outcome.is_some() { return false; }
    state.allocated = true;
    true
  }
  pub(crate) fn not_created(&self) {
    let (closing, listeners) = {
      let Ok(mut state) = self.state.lock() else { return };
      if state.allocated { return; }
      state.closing = true;
      let closing = std::mem::take(&mut state.closing_listeners);
      let listeners = if state.active == 0 { finish(&mut state, NativeWindowCloseOutcome::NotCreated) } else { Vec::new() };
      (closing, listeners)
    };
    deliver_closing(closing);
    deliver_all(listeners, NativeWindowCloseOutcome::NotCreated);
  }
  pub(crate) fn begin_close(&self) {
    let closing = if let Ok(mut state) = self.state.lock() {
      state.closing = true;
      std::mem::take(&mut state.closing_listeners)
    } else { Vec::new() };
    deliver_closing(closing);
  }
  pub(crate) fn admit(&self) -> Option<NativeWindowEffect> {
    let mut state = self.state.lock().ok()?;
    if state.closing || state.outcome.is_some() { return None; }
    state.active = state.active.checked_add(1)?;
    Some(NativeWindowEffect { close: self.clone() })
  }

  /// Admit one effect on this original reservation. Admission may occur before
  /// allocation; the operation must check `is_current` immediately on the UI
  /// thread before touching the native window.
  pub fn admit_effect(&self) -> Option<NativeWindowEffect> { self.admit() }
  #[cfg(target_os = "macos")]
  pub(crate) fn require_native_preclose(&self) {
    if let Ok(mut state) = self.state.lock() { state.preclose_required = true; }
  }
  #[cfg(target_os = "macos")]
  pub(crate) fn admit_native_preclose(&self) -> Option<NativeWindowEffect> {
    let closing = {
      let mut state = self.state.lock().ok()?;
      if !state.allocated || !state.preclose_required || state.preclose_started || state.outcome.is_some() { return None; }
      let active = state.active.checked_add(1)?;
      state.preclose_started = true;
      state.active = active;
      state.closing = true;
      std::mem::take(&mut state.closing_listeners)
    };
    deliver_closing(closing);
    Some(NativeWindowEffect { close: self.clone() })
  }
  #[cfg(target_os = "macos")]
  pub(crate) fn complete_native_preclose(&self, success: bool) {
    if let Ok(mut state) = self.state.lock() { state.preclose_result = Some(success); }
  }
  /// Only after the original native close callback has returned to the UI loop.
  pub(crate) fn native_close_observed(&self) {
    let listeners = {
      let Ok(mut state) = self.state.lock() else { return };
      if !state.allocated { return; }
      state.closing = true;
      state.observed = true;
      if state.active != 0 || !state.effects_connected || (state.preclose_required && state.preclose_result != Some(true)) { return; }
      finish(&mut state, NativeWindowCloseOutcome::NativeClosed)
    };
    deliver_all(listeners, NativeWindowCloseOutcome::NativeClosed);
  }
}
impl Drop for NativeWindowEffect {
  fn drop(&mut self) {
    let (listeners, outcome) = {
      let Ok(mut state) = self.close.state.lock() else { return };
      state.active -= 1;
      if state.active != 0 { return; }
      let outcome = if state.closing && !state.allocated {
        NativeWindowCloseOutcome::NotCreated
      } else if state.observed && state.effects_connected && (!state.preclose_required || state.preclose_result == Some(true)) {
        NativeWindowCloseOutcome::NativeClosed
      } else { return };
      (finish(&mut state, outcome), outcome)
    };
    deliver_all(listeners, outcome);
  }
}

impl NativeWindowEffect {
  /// True only while the original native window is allocated and has not
  /// begun closing. Recheck immediately before the actual UI operation.
  pub fn is_current(&self) -> bool {
    self.close.state.lock().is_ok_and(|state| state.allocated && !state.closing && state.outcome.is_none())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn unsubmitted_is_not_created_but_allocated_is_not() {
    let guard = NativeWindowSubmissionGuard::new();
    let close = guard.receipt();
    drop(guard);
    assert_eq!(close.state.lock().unwrap().outcome, Some(NativeWindowCloseOutcome::NotCreated));

    let guard = NativeWindowSubmissionGuard::new();
    let close = guard.receipt();
    let submission = guard.submit();
    assert!(close.claim());
    assert!(close.allocated());
    close.not_created();
    drop(submission);
    assert_eq!(close.state.lock().unwrap().outcome, None);
  }

  #[test]
  fn native_observation_waits_for_last_accepted_effect() {
    let guard = NativeWindowSubmissionGuard::new();
    let close = guard.receipt();
    let _submission = guard.submit();
    assert!(close.claim());
    assert!(close.allocated());
    close.state.lock().unwrap().effects_connected = true;
    let effect = close.admit().unwrap();
    close.begin_close();
    assert!(close.admit().is_none());
    close.native_close_observed();
    assert_eq!(close.state.lock().unwrap().outcome, None);
    drop(effect);
    assert_eq!(close.state.lock().unwrap().outcome, Some(NativeWindowCloseOutcome::NativeClosed));
    close.native_close_observed();
    assert_eq!(close.state.lock().unwrap().outcome, Some(NativeWindowCloseOutcome::NativeClosed));
  }

  #[test]
  fn unallocated_failure_waits_for_previously_admitted_work() {
    let guard = NativeWindowSubmissionGuard::new();
    let close = guard.receipt();
    let effect = close.admit().unwrap();
    drop(guard);
    assert!(close.admit().is_none());
    assert_eq!(close.state.lock().unwrap().outcome, None);
    drop(effect);
    assert_eq!(close.state.lock().unwrap().outcome, Some(NativeWindowCloseOutcome::NotCreated));
  }

  #[test]
  fn unrelated_reservations_never_compare_equal() {
    let first = NativeWindowSubmissionGuard::new();
    let second = NativeWindowSubmissionGuard::new();
    assert!(first.receipt().same(&first.receipt()));
    assert!(!first.receipt().same(&second.receipt()));
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn native_preclose_revokes_new_effects_before_cleanup() {
    let guard = NativeWindowSubmissionGuard::new();
    let close = guard.receipt();
    let _submission = guard.submit();
    assert!(close.claim());
    assert!(close.allocated());
    close.require_native_preclose();
    let effect = close.admit_native_preclose().unwrap();
    assert!(close.admit().is_none());
    assert!(close.admit_native_preclose().is_none());
    assert!(close.state.lock().unwrap().closing);
    drop(effect);
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn preclose_must_finish_and_unwind_before_native_terminal() {
    for success in [false, true] {
      let guard = NativeWindowSubmissionGuard::new();
      let close = guard.receipt();
      let _submission = guard.submit();
      assert!(close.claim());
      assert!(close.allocated());
      close.require_native_preclose();
      close.state.lock().unwrap().effects_connected = true;
      close.begin_close();
      close.native_close_observed();
      assert_eq!(close.state.lock().unwrap().outcome, None);
      let effect = close.admit_native_preclose().unwrap();
      close.complete_native_preclose(success);
      assert_eq!(close.state.lock().unwrap().outcome, None);
      drop(effect);
      assert_eq!(close.state.lock().unwrap().outcome,
        success.then_some(NativeWindowCloseOutcome::NativeClosed));
    }
  }
}
