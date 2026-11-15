// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeBrowserCloseOutcome {
  /// The original browser delivered CEF OnBeforeClose.
  Closed,
  /// Creation failed before any native browser existed.
  NotCreated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeBrowserCreationOutcome { Attached, Failed }

struct CreationListener {
  active: AtomicBool,
  callback: Mutex<Option<Box<dyn FnOnce(NativeBrowserCreationOutcome) + Send>>>,
}
pub struct NativeBrowserCreationSubscription { listener: Arc<CreationListener> }
impl Drop for NativeBrowserCreationSubscription {
  fn drop(&mut self) { self.listener.active.store(false, Ordering::Release); }
}

type Callback = Box<dyn FnOnce(NativeBrowserCloseOutcome) + Send>;
struct Listener { active: AtomicBool, callback: Mutex<Option<Callback>> }
#[derive(Default)]
struct State {
  terminal: Option<NativeBrowserCloseOutcome>,
  listeners: Vec<Arc<Listener>>,
  creation: Option<NativeBrowserCreationOutcome>,
  creation_listeners: Vec<Arc<CreationListener>>,
  request: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
  claimed: bool,
  submissions: usize,
  requested: bool,
}

/// Exact native creation reservation, available before OnAfterCreated.
#[derive(Clone, Default)]
pub struct NativeBrowserClose { state: Arc<Mutex<State>> }
impl std::fmt::Debug for NativeBrowserClose {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("NativeBrowserClose").finish_non_exhaustive() }
}

pub struct NativeBrowserCloseSubscription { listener: Arc<Listener> }
impl Drop for NativeBrowserCloseSubscription {
  fn drop(&mut self) { self.listener.active.store(false, Ordering::Release); }
}

#[derive(Clone)]
pub(crate) struct NativeBrowserSubmission(Arc<Submission>);
struct Submission { close: NativeBrowserClose }
impl NativeBrowserSubmission {
  pub(crate) fn new(close: NativeBrowserClose) -> Self {
    {
      let mut state = close.state.lock().unwrap_or_else(|error| error.into_inner());
      state.submissions = state.submissions.checked_add(1).expect("native submission count exhausted");
    }
    Self(Arc::new(Submission { close }))
  }
  pub(crate) fn close(&self) -> NativeBrowserClose { self.0.close.clone() }
}
impl Drop for Submission {
  fn drop(&mut self) { self.close.release_submission(); }
}

/// An owned, unsubmitted creation reservation. Moving it into a builder
/// transfers ownership; dropping the last unclaimed submission proves failure.
pub struct NativeBrowserSubmissionGuard(NativeBrowserSubmission);
impl NativeBrowserSubmissionGuard {
  pub fn new() -> Self { Self(NativeBrowserSubmission::new(NativeBrowserClose::default())) }
  pub fn receipt(&self) -> NativeBrowserClose { self.0.close() }
  pub(crate) fn into_submission(self) -> NativeBrowserSubmission { self.0 }
}
impl Default for NativeBrowserSubmissionGuard {
  fn default() -> Self { Self::new() }
}

impl NativeBrowserClose {
  pub(crate) fn weak(&self) -> impl Fn() -> Option<Self> + Send + Sync + 'static {
    let state = Arc::downgrade(&self.state);
    move || state.upgrade().map(|state| Self { state })
  }
}

fn deliver(listener: Arc<Listener>, outcome: NativeBrowserCloseOutcome) {
  crate::native_evaluation::defer_completion(Box::new(move || {
    if !listener.active.swap(false, Ordering::AcqRel) { return; }
    let callback = listener.callback.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some(callback) = callback { callback(outcome); }
  }));
}

fn deliver_creation(listener: Arc<CreationListener>, outcome: NativeBrowserCreationOutcome) {
  crate::native_evaluation::defer_completion(Box::new(move || {
    if !listener.active.swap(false, Ordering::AcqRel) { return; }
    let callback = listener.callback.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some(callback) = callback { callback(outcome); }
  }));
}

impl NativeBrowserClose {
  /// Request destruction of this exact reserved browser, even before attachment.
  /// The terminal receipt is still delivered only after native close or a
  /// proven failure before browser creation.
  pub fn request_close(&self) -> Result<(), String> {
    let request = {
      let mut state = self.state.lock().map_err(|_| "Native browser close state was poisoned.")?;
      if state.terminal.is_some() { return Ok(()); }
      state.requested = true;
      state.request.clone()
    };
    if let Some(request) = request { request()?; }
    Ok(())
  }

  fn release_submission(&self) {
    let listeners = {
      let Ok(mut state) = self.state.lock() else { return };
      state.submissions = state.submissions.saturating_sub(1);
      if state.submissions != 0 || state.terminal.is_some() || state.claimed { return; }
      state.terminal = Some(NativeBrowserCloseOutcome::NotCreated);
      std::mem::take(&mut state.listeners)
    };
    self.complete_creation(NativeBrowserCreationOutcome::Failed);
    for listener in listeners { deliver(listener, NativeBrowserCloseOutcome::NotCreated); }
  }

  pub(crate) fn claim(&self) -> Result<(), String> {
    let mut state = self.state.lock().map_err(|_| "Native browser close state was poisoned.")?;
    if state.terminal.is_some() || state.claimed {
      return Err("Native browser close reservation was already used.".into());
    }
    state.claimed = true;
    Ok(())
  }

  pub(crate) fn bind_request(&self, request: Arc<dyn Fn() -> Result<(), String> + Send + Sync>) -> Result<(), String> {
    let requested = {
      let mut state = self.state.lock().map_err(|_| "Native browser close state was poisoned.")?;
      if state.terminal.is_some() || !state.claimed || state.request.is_some() {
        return Err("Native browser close reservation was already used.".into());
      }
      state.request = Some(request.clone());
      state.requested
    };
    if requested { request()?; }
    Ok(())
  }

  pub fn same_reservation(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.state, &other.state)
  }

  pub fn when_created(&self, callback: impl FnOnce(NativeBrowserCreationOutcome) + Send + 'static) -> Result<NativeBrowserCreationSubscription, String> {
    let listener = Arc::new(CreationListener { active: AtomicBool::new(true), callback: Mutex::new(Some(Box::new(callback))) });
    let result = {
      let mut state = self.state.lock().map_err(|_| "Native browser creation state was poisoned.")?;
      if state.creation.is_none() {
        state.creation_listeners.retain(|listener| listener.active.load(Ordering::Acquire));
        state.creation_listeners.push(listener.clone());
      }
      state.creation
    };
    if let Some(result) = result { deliver_creation(listener.clone(), result); }
    Ok(NativeBrowserCreationSubscription { listener })
  }

  pub(crate) fn complete_creation(&self, outcome: NativeBrowserCreationOutcome) {
    let listeners = {
      let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
      if state.creation.is_some() { return; }
      state.creation = Some(outcome);
      std::mem::take(&mut state.creation_listeners)
    };
    for listener in listeners { deliver_creation(listener, outcome); }
  }

  pub fn when_closed(&self, callback: impl FnOnce(NativeBrowserCloseOutcome) + Send + 'static) -> Result<NativeBrowserCloseSubscription, String> {
    let listener = Arc::new(Listener { active: AtomicBool::new(true), callback: Mutex::new(Some(Box::new(callback))) });
    let terminal = {
      let mut state = self.state.lock().map_err(|_| "Native browser close state was poisoned.")?;
      if state.terminal.is_none() {
        state.listeners.retain(|listener| listener.active.load(Ordering::Acquire));
        state.listeners.push(listener.clone());
      }
      state.terminal
    };
    if let Some(outcome) = terminal { deliver(listener.clone(), outcome); }
    Ok(NativeBrowserCloseSubscription { listener })
  }

  #[cfg(test)]
  pub(crate) fn outcomes(&self) -> (Option<NativeBrowserCreationOutcome>, Option<NativeBrowserCloseOutcome>) {
    let state = self.state.lock().unwrap();
    (state.creation, state.terminal)
  }

  pub(crate) fn complete(&self, outcome: NativeBrowserCloseOutcome) {
    self.complete_creation(NativeBrowserCreationOutcome::Failed);
    let listeners = {
      let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
      if state.terminal.is_some() { return; }
      state.terminal = Some(outcome);
      state.request = None;
      std::mem::take(&mut state.listeners)
    };
    for listener in listeners { deliver(listener, outcome); }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn duplicate_close_and_late_subscribe_report_original_terminal_once() {
    let receipt = NativeBrowserClose::default();
    receipt.complete(NativeBrowserCloseOutcome::NotCreated);
    receipt.complete(NativeBrowserCloseOutcome::Closed);
    assert_eq!(receipt.state.lock().unwrap().terminal, Some(NativeBrowserCloseOutcome::NotCreated));
    let other = NativeBrowserClose::default();
    assert!(other.state.lock().unwrap().terminal.is_none());
  }

  #[test]
  fn reservation_can_only_be_claimed_once() {
    let receipt = NativeBrowserClose::default();
    assert!(receipt.claim().is_ok());
    assert!(receipt.clone().claim().is_err());
    drop(NativeBrowserSubmission::new(receipt.clone()));
    assert!(receipt.state.lock().unwrap().terminal.is_none());
  }

  #[test]
  fn abandoned_submission_fails_creation_and_close() {
    let receipt = NativeBrowserClose::default();
    let submission = NativeBrowserSubmission::new(receipt.clone());
    let clone = submission.clone();
    drop(submission);
    assert!(receipt.state.lock().unwrap().terminal.is_none());
    drop(clone);
    let state = receipt.state.lock().unwrap();
    assert_eq!(state.terminal, Some(NativeBrowserCloseOutcome::NotCreated));
    assert_eq!(state.creation, Some(NativeBrowserCreationOutcome::Failed));
  }

  #[test]
  fn owned_guard_does_not_retire_another_submission() {
    let guard = NativeBrowserSubmissionGuard::new();
    let receipt = guard.receipt();
    let submitted = NativeBrowserSubmission::new(receipt.clone());
    drop(guard);
    assert_eq!(receipt.outcomes(), (None, None));
    assert!(receipt.claim().is_ok());
    drop(submitted);
    assert_eq!(receipt.outcomes(), (None, None));
  }
}
