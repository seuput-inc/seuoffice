// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}};
use cef::*;
use crate::{FrameNavigationState, NativeContextToken};

type Completion = Box<dyn FnOnce(Result<NativeContextToken, String>) + Send>;

pub(crate) struct NativeReloadDispatch {
  reservation: NativeReloadReservation,
  admission: Option<NativeReloadAdmission>,
  completion: Option<Completion>,
}
impl NativeReloadDispatch {
  pub(crate) fn new(reservation: NativeReloadReservation, admission: NativeReloadAdmission, completion: Completion) -> Self {
    Self { reservation, admission: Some(admission), completion: Some(completion) }
  }
  pub(crate) fn execute(mut self, browser: &Browser, state: &FrameNavigationState) {
    let admission = self.admission.take().expect("native reload admission");
    let completion = self.completion.take().expect("native reload completion");
    self.reservation.dispatch(browser, state, admission, completion);
  }
}
impl Drop for NativeReloadDispatch {
  fn drop(&mut self) {
    if let Some(completion) = self.completion.take() {
      crate::native_evaluation::defer_completion(Box::new(move || completion(Err("Native reload view closed before dispatch.".into()))));
    }
  }
}
pub type NativeReloadAdmission = Box<dyn FnOnce() -> Result<(), String> + Send>;

enum Phase { Prepared, Dispatched, Bound(NativeContextToken), Finished }
struct RequestState {
  phase: Phase,
  completion: Option<Completion>,
  failure: Option<String>,
  listeners: Vec<Arc<FailureListener>>,
}
struct FailureListener { active: AtomicBool, callback: Mutex<Option<Box<dyn FnOnce(String) + Send>>> }
pub struct NativeReloadFailureSubscription { listener: Arc<FailureListener> }
impl Drop for NativeReloadFailureSubscription {
  fn drop(&mut self) { self.listener.active.store(false, Ordering::Release); }
}
fn deliver_failure(listener: Arc<FailureListener>, error: String) {
  crate::native_evaluation::defer_completion(Box::new(move || {
    if !listener.active.swap(false, Ordering::AcqRel) { return; }
    let callback = listener.callback.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some(callback) = callback { callback(error); }
  }));
}
struct Request {
  original: NativeContextToken,
  browser: FrameNavigationState,
  issuance: u64,
  intent: String,
  handles: AtomicUsize,
  state: Mutex<RequestState>,
}

#[derive(Clone, Default)]
pub(crate) struct NativeReloadTracker(Arc<Mutex<Option<Arc<Request>>>>);
impl std::fmt::Debug for NativeReloadTracker {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("NativeReloadTracker").finish_non_exhaustive() }
}

/// Retain until completion. The native-approved intent is never exposed to JS.
pub struct NativeReloadReservation { request: Arc<Request> }

#[derive(Clone)]
pub struct NativeReloadIdentity { request: std::sync::Weak<Request> }
impl NativeReloadIdentity {
  pub fn matches_registered(&self, origin: &NativeContextToken) -> bool {
    self.request.upgrade().is_some_and(|request| request.matches(origin)
      && request.state.lock().is_ok_and(|state| matches!(state.phase, Phase::Dispatched | Phase::Bound(_))))
  }
}

impl Clone for NativeReloadReservation {
  fn clone(&self) -> Self {
    self.request.handles.fetch_add(1, Ordering::Relaxed);
    Self { request: self.request.clone() }
  }
}
impl Drop for NativeReloadReservation {
  fn drop(&mut self) {
    if self.request.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
      self.request.finish(Err("Native reload reservation was dropped.".into()));
    }
  }
}
impl std::fmt::Debug for NativeReloadReservation {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("NativeReloadReservation").finish_non_exhaustive() }
}

impl Request {
  fn finish(&self, result: Result<NativeContextToken, String>) {
    let (completion, listeners, failure) = {
      let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
      if matches!(state.phase, Phase::Finished) { return; }
      state.phase = Phase::Finished;
      state.failure = result.as_ref().err().cloned();
      (state.completion.take(), std::mem::take(&mut state.listeners), state.failure.clone())
    };
    self.browser.native_contexts.cancel_navigation_intent(&self.browser, &self.intent);
    let retired = {
      let mut pending = self.browser.native_reload.0.lock().unwrap_or_else(|error| error.into_inner());
      if pending.as_ref().is_some_and(|request| request.issuance == self.issuance) { pending.take() } else { None }
    };
    drop(retired);
    if let Some(error) = failure {
      for listener in listeners { deliver_failure(listener, error.clone()); }
    }
    if let Some(completion) = completion {
      crate::native_evaluation::defer_completion(Box::new(move || {
        let result = result.and_then(|origin| if origin.is_verified() { Ok(origin) } else { Err("Reloaded native document was released before completion.".into()) });
        completion(result);
      }));
    }
  }

  fn matches(&self, origin: &NativeContextToken) -> bool {
    origin.browser_id() == self.original.browser_id()
      && origin.browser_state().is_same_browser(&self.browser)
      && origin.navigation_issuance == Some(self.issuance)
      && *origin != self.original && !origin.is_revoked()
  }
}

impl NativeReloadReservation {
  pub fn cancel(&self) { self.request.finish(Err("Native reload was cancelled.".into())); }
  pub fn identity(&self) -> NativeReloadIdentity { NativeReloadIdentity { request: Arc::downgrade(&self.request) } }
  pub fn browser_close(&self) -> crate::NativeBrowserClose { self.request.browser.browser_close.clone() }
  pub fn on_failure(&self, callback: impl FnOnce(String) + Send + 'static) -> Result<NativeReloadFailureSubscription, String> {
    let listener = Arc::new(FailureListener { active: AtomicBool::new(true), callback: Mutex::new(Some(Box::new(callback))) });
    let failure = {
      let mut state = self.request.state.lock().map_err(|_| "Native reload state was poisoned.")?;
      if !matches!(state.phase, Phase::Finished) { state.listeners.push(listener.clone()); }
      else if state.failure.is_none() { return Err("Native reload has already completed.".into()); }
      state.failure.clone()
    };
    if let Some(error) = failure { deliver_failure(listener.clone(), error); }
    Ok(NativeReloadFailureSubscription { listener })
  }

  /// Immutable private issuance matching only; callers separately admit liveness.
  pub fn matches_registered(&self, origin: &NativeContextToken) -> bool {
    self.request.matches(origin) && self.request.state.lock().is_ok_and(|state|
      matches!(state.phase, Phase::Dispatched | Phase::Bound(_)))
  }

  pub fn belongs_to(&self, original: &NativeContextToken) -> bool { self.request.original == *original }

  pub(crate) fn dispatch(&self, browser: &Browser, state: &FrameNavigationState,
    admission: NativeReloadAdmission, completion: Completion) {
    let request = &self.request;
    {
      let mut pending = request.state.lock().unwrap_or_else(|error| error.into_inner());
      if !matches!(pending.phase, Phase::Prepared) {
        drop(pending);
        crate::native_evaluation::defer_completion(Box::new(move || completion(Err("Native reload was already dispatched or cancelled.".into()))));
        return;
      }
      pending.completion = Some(completion);
    }
    if !state.is_same_browser(&request.browser) || browser.identifier() != request.original.browser_id()
      || !request.original.is_verified()
      || !request.original.endpoints().is_some_and(|endpoint| endpoint.evaluation.is_current_document() == 1)
      || !state.native_contexts.has_navigation_intent(state, request.issuance, &request.intent) {
      request.finish(Err("Original native reload document was replaced.".into()));
      return;
    }
    let Some(host) = browser.host() else { request.finish(Err("Original native reload browser is unavailable.".into())); return };
    if let Err(error) = admission() { request.finish(Err(error)); return; }
    // The owner reducer has committed and released its locks. This exact UI
    // stack now performs the accepted native action without another queue hop.
    {
      let mut pending = request.state.lock().unwrap_or_else(|error| error.into_inner());
      if !matches!(pending.phase, Phase::Prepared) { return; }
      pending.phase = Phase::Dispatched;
    }
    let mut callback = ApprovedReloadStarted::new(request.clone());
    host.reload_approved_main_frame(0, request.issuance,
      Some(&CefString::from(request.intent.as_str())), Some(&mut callback));
  }
}

impl NativeContextToken {
  /// Native UI only, outside owner gates. Reserves an issuance but performs no
  /// reload and does not grant a successor context any authority.
  pub fn prepare_reload(&self, native: &crate::Webview) -> Result<NativeReloadReservation, String> {
    self.capture_current_document(native).ok_or("Original native reload document is not committed and current.")?;
    let browser = self.browser_state().clone();
    let (issuance, intent) = browser.native_contexts.issue_navigation_intent(browser.clone(), self.browser_id())
      .ok_or("Native reload intent could not be reserved.")?;
    let request = Arc::new(Request { original: self.clone(), browser: browser.clone(), issuance, intent,
      handles: AtomicUsize::new(1), state: Mutex::new(RequestState { phase: Phase::Prepared, completion: None, failure: None, listeners: Vec::new() }) });
    let previous = browser.native_reload.0.lock().map_err(|_| "Native reload state was poisoned.")?.replace(request.clone());
    if let Some(previous) = previous { previous.finish(Err("Native reload was superseded before completion.".into())); }
    Ok(NativeReloadReservation { request })
  }
}

impl NativeReloadTracker {
  pub(crate) fn registered(&self, origin: &NativeContextToken) {
    let request = self.0.lock().ok().and_then(|request| request.clone());
    let Some(request) = request else { return };
    if !request.matches(origin) {
      request.finish(Err("Native reload was replaced by a different approved document.".into()));
      return;
    }
    let mut state = request.state.lock().unwrap_or_else(|error| error.into_inner());
    if matches!(state.phase, Phase::Dispatched) { state.phase = Phase::Bound(origin.clone()); }
  }

  pub(crate) fn document_ready(&self, origin: &NativeContextToken) {
    let request = self.0.lock().ok().and_then(|request| request.clone());
    let Some(request) = request else { return };
    let bound = request.state.lock().is_ok_and(|state| matches!(&state.phase, Phase::Bound(bound) if bound == origin));
    if bound && request.matches(origin) && origin.is_verified() {
      request.finish(Ok(origin.clone()));
    }
  }

  pub(crate) fn document_lost(&self, origin: &NativeContextToken) {
    let request = self.0.lock().ok().and_then(|request| request.clone());
    let Some(request) = request else { return };
    let affected = request.state.lock().is_ok_and(|state| match &state.phase {
      Phase::Prepared => request.original == *origin,
      Phase::Bound(bound) => bound == origin,
      Phase::Dispatched | Phase::Finished => false,
    });
    if affected { request.finish(Err("The exact native reload document lost its renderer.".into())); }
  }

  pub(crate) fn closed(&self) {
    let request = self.0.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some(request) = request { request.finish(Err("Original native reload browser closed.".into())); }
  }
}

wrap_approved_history_navigation_callback! {
  struct ApprovedReloadStarted { request: Arc<Request> }
  impl ApprovedHistoryNavigationCallback {
    fn on_complete(&self, outcome: ApprovedHistoryNavigationOutcome) {
      if outcome != sys::cef_approved_history_navigation_outcome_t::CEF_APPROVED_HISTORY_STARTED_CROSS_DOCUMENT.into() {
        let request = self.request.clone();
        crate::native_evaluation::defer_completion(Box::new(move || {
          request.finish(Err("Approved reload was cancelled or did not create a successor document.".into()));
        }));
      }
    }
  }
}
