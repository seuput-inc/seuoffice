// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{collections::HashMap, sync::{Arc, Mutex, Weak, atomic::{AtomicBool, Ordering}}};
use cef::*;

/// An observation of one original native document, not IPC or V8 authority.
#[derive(Clone)]
pub struct NativeObservedDocument { inner: Arc<Document> }
struct Document {
  browser: crate::FrameNavigationState,
  document: String,
  nonce: String,
  devtools_frame: String,
  devtools_loader: String,
  url: url::Url,
  process: crate::NativeRendererProcess,
  _native: cef::NativeObservedDocument,
  loss: Mutex<Loss>,
}
impl std::fmt::Debug for NativeObservedDocument {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("NativeObservedDocument").finish_non_exhaustive()
  }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeObservedDocumentLoss { Released, ProcessLost, Unknown }
struct Listener { active: AtomicBool, callback: Mutex<Option<Box<dyn FnOnce(NativeObservedDocumentLoss) + Send>>> }
#[derive(Default)]
struct Loss { reason: Option<NativeObservedDocumentLoss>, listeners: Vec<Arc<Listener>> }
pub struct NativeObservedDocumentSubscription { listener: Arc<Listener>, _document: Arc<Document> }
impl Drop for NativeObservedDocumentSubscription {
  fn drop(&mut self) { self.listener.active.store(false, Ordering::Release); }
}
fn deliver(listener: Arc<Listener>, reason: NativeObservedDocumentLoss) {
  crate::native_evaluation::defer_completion(Box::new(move || {
    if !listener.active.swap(false, Ordering::AcqRel) { return; }
    let callback = listener.callback.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(callback) = callback { callback(reason); }
  }));
}
impl NativeObservedDocument {
  pub fn browser_state(&self) -> &crate::FrameNavigationState { &self.inner.browser }
  pub fn url(&self) -> &url::Url { &self.inner.url }
  pub fn document_token(&self) -> &str { &self.inner.document }
  pub fn observer_nonce(&self) -> &str { &self.inner.nonce }
  /// Original native CDP correlation; this is not an execution-context grant.
  pub fn devtools_frame(&self) -> &str { &self.inner.devtools_frame }
  pub fn devtools_loader(&self) -> &str { &self.inner.devtools_loader }
  pub fn process_identity(&self) -> crate::NativeRendererProcessIdentity { self.inner.process.identity() }
  pub fn is_same_document(&self, other: &Self) -> bool { Arc::ptr_eq(&self.inner, &other.inner) }
  pub fn when_lost(&self, callback: impl FnOnce(NativeObservedDocumentLoss) + Send + 'static) -> Result<NativeObservedDocumentSubscription, String> {
    let listener = Arc::new(Listener { active: AtomicBool::new(true), callback: Mutex::new(Some(Box::new(callback))) });
    let reason = {
      let mut loss = self.inner.loss.lock().map_err(|_| "Native document loss state was poisoned.")?;
      if loss.reason.is_none() {
        loss.listeners.retain(|listener| listener.active.load(Ordering::Acquire));
        loss.listeners.push(listener.clone());
      }
      loss.reason
    };
    if let Some(reason) = reason { deliver(listener.clone(), reason); }
    Ok(NativeObservedDocumentSubscription { listener, _document: self.inner.clone() })
  }
  fn new(browser: crate::FrameNavigationState, document: String, nonce: String,
    devtools_frame: String, devtools_loader: String, url: url::Url,
    process: crate::NativeRendererProcess, native: cef::NativeObservedDocument) -> Result<Self, String> {
    let observed_process = native.process().and_then(|process| crate::NativeRendererProcess::from_native(&process))
      .ok_or("Native document process is unavailable.")?;
    if observed_process.identity() != process.identity() {
      return Err("Native document process identity differs from its receipt.".into());
    }
    if document.is_empty() || nonce.is_empty() || devtools_frame.is_empty() || devtools_loader.is_empty() {
      return Err("Native document or CDP correlation identity is missing.".into());
    }
    let inner = Arc::new(Document { browser, document, nonce, devtools_frame, devtools_loader, url, process, _native: native.clone(), loss: Mutex::default() });
    let mut callback = ObservedLoss::new(Arc::downgrade(&inner));
    if native.observe_loss(Some(&mut callback)) == 0 { return Err("Native document loss observer was not installed.".into()); }
    Ok(Self { inner })
  }
}
wrap_native_observed_document_callback! {
  struct ObservedLoss { document: Weak<Document> }
  impl NativeObservedDocumentCallback {
    fn on_lost(&self, reason: i32) {
      let Some(document) = self.document.upgrade() else { return };
      let reason = match reason { 1 => NativeObservedDocumentLoss::Released, 2 => NativeObservedDocumentLoss::ProcessLost, _ => NativeObservedDocumentLoss::Unknown };
      let listeners = {
        let mut loss = document.loss.lock().unwrap_or_else(|e| e.into_inner());
        if loss.reason.is_some() { return; }
        loss.reason = Some(reason);
        std::mem::take(&mut loss.listeners)
      };
      for listener in listeners { deliver(listener, reason); }
    }
  }
}

pub(crate) type Completion = Box<dyn FnOnce(Result<NativeObservedDocument, String>) + Send>;
struct Pending { state: Mutex<PendingState> }
struct PendingState {
  completion: Option<Completion>,
  location: Option<(Weak<Mutex<HashMap<u64, Arc<Pending>>>>, u64)>,
}
#[derive(Clone)]
pub struct NativeObservedNavigation { pending: Arc<Pending> }
impl NativeObservedNavigation {
  /// Cancels this receipt. A navigation already admitted to CEF is not stopped;
  /// the owning caller must close or otherwise cancel its native browser.
  pub fn cancel(&self) { self.pending.finish(Err("Observed navigation was cancelled.".into())); }
}
impl Pending {
  fn finish(&self, result: Result<NativeObservedDocument, String>) {
    let (completion, location) = {
      let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
      (state.completion.take(), state.location.take())
    };
    if let Some((registry, issuance)) = location.and_then(|(registry, issuance)| registry.upgrade().map(|registry| (registry, issuance))) {
      if let Ok(mut entries) = registry.lock() {
        if entries.get(&issuance).is_some_and(|entry| std::ptr::eq(Arc::as_ptr(entry), self)) {
          entries.remove(&issuance);
        }
      }
    }
    if let Some(completion) = completion { crate::native_evaluation::defer_completion(Box::new(move || completion(result))); }
  }
}
#[derive(Clone, Default)]
pub(crate) struct Registry(Arc<Mutex<HashMap<u64, Arc<Pending>>>>);
impl std::fmt::Debug for Registry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ObservedNavigationRegistry") }
}
impl Registry {
  fn insert(&self, issuance: u64, pending: Arc<Pending>) -> bool {
    let Ok(mut state) = pending.state.lock() else { return false };
    if state.completion.is_none() || state.location.is_some() { return false; }
    let Ok(mut entries) = self.0.lock() else { return false };
    if entries.len() >= 4096 || entries.contains_key(&issuance) { return false; }
    state.location = Some((Arc::downgrade(&self.0), issuance));
    entries.insert(issuance, pending.clone());
    true
  }
  pub(crate) fn fail(&self, issuance: u64, error: String) {
    let pending = self.0.lock().ok().and_then(|mut entries| entries.remove(&issuance));
    if let Some(pending) = pending { pending.finish(Err(error)); }
  }
  pub(crate) fn close(&self) {
    let entries = self.0.lock().map(|mut entries| std::mem::take(&mut *entries)).unwrap_or_default();
    for (_, pending) in entries { pending.finish(Err("Observed navigation browser closed.".into())); }
  }
  pub(crate) fn complete(&self, browser: crate::FrameNavigationState, issuance: u64, nonce: String,
    committed: bool, error: i32, url: String, document: String, devtools_frame: String, devtools_loader: String,
    process: Option<cef::NativeRendererProcess>, native: Option<cef::NativeObservedDocument>) {
    let pending = self.0.lock().ok().and_then(|mut entries| entries.remove(&issuance));
    let Some(pending) = pending else { return };
    let result = if !committed { Err(format!("Observed navigation failed (CEF error {error}).")) }
    else {
      (|| {
        let process = process.as_ref().and_then(crate::NativeRendererProcess::from_native).ok_or("Native renderer process is unavailable.")?;
        let native = native.ok_or("Native document lifetime is unavailable.")?;
        let url = url::Url::parse(&url).map_err(|e| e.to_string())?;
        NativeObservedDocument::new(browser, document, nonce, devtools_frame, devtools_loader, url, process, native)
      })()
    };
    pending.finish(result);
  }
}

pub(crate) struct Dispatch { url: url::Url, pending: Arc<Pending>, dispatched: bool }
impl Dispatch {
  pub(crate) fn new(url: url::Url, callback: Completion) -> (Self, NativeObservedNavigation) {
    let pending = Arc::new(Pending { state: Mutex::new(PendingState { completion: Some(callback), location: None }) });
    (Self { url, pending: pending.clone(), dispatched: false }, NativeObservedNavigation { pending })
  }
  pub(crate) fn execute(mut self, browser: &cef::Browser, state: &crate::FrameNavigationState) {
    if state.trusted_init_mode() != Some(crate::TrustedInitMode::Unprotected) {
      self.pending.finish(Err("Observed navigation requires an unprotected browser.".into())); return;
    }
    let Some(issuance) = state.next_navigation_issuance(browser.identifier()) else { self.pending.finish(Err("Native browser is closed.".into())); return };
    let Some(intent) = crate::cef_impl::trusted_init::random_capability() else { self.pending.finish(Err("Native navigation nonce is unavailable.".into())); return };
    if !state.observed_navigation.insert(issuance, self.pending.clone()) { self.pending.finish(Err("Native navigation reservation failed.".into())); return; }
    let Some(host) = browser.host() else { state.observed_navigation.fail(issuance, "Native browser host is unavailable.".into()); return };
    let mut callback = ObservedLoad::new(state.observed_navigation.clone(), issuance);
    self.dispatched = true;
    host.load_approved_main_frame_url(Some(&CefString::from(self.url.as_str())), issuance,
      Some(&CefString::from(intent.as_str())), Some(&mut callback));
  }
}
impl Drop for Dispatch {
  fn drop(&mut self) { if !self.dispatched { self.pending.finish(Err("Observed navigation was not dispatched.".into())); } }
}
wrap_approved_navigation_callback! {
  struct ObservedLoad { registry: Registry, issuance: u64 }
  impl ApprovedNavigationCallback {
    fn on_complete(&self, accepted: i32) {
      if accepted == 0 { self.registry.fail(self.issuance, "Native navigation was rejected.".into()); }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cancel_removes_only_its_reservation_and_late_failure_is_idempotent() {
    let registry = Registry::default();
    let pending = Arc::new(Pending { state: Mutex::new(PendingState {
      completion: Some(Box::new(|_| {})), location: None,
    }) });
    assert!(registry.insert(1, pending.clone()));
    let other = Arc::new(Pending { state: Mutex::new(PendingState {
      completion: Some(Box::new(|_| {})), location: None,
    }) });
    assert!(registry.insert(2, other));
    NativeObservedNavigation { pending: pending.clone() }.cancel();
    assert!(!registry.0.lock().unwrap().contains_key(&1));
    assert!(registry.0.lock().unwrap().contains_key(&2));
    registry.fail(1, "late".into());
    assert!(pending.state.lock().unwrap().completion.is_none());
  }

  #[test]
  fn already_cancelled_reservation_cannot_be_dispatched() {
    let registry = Registry::default();
    let pending = Arc::new(Pending { state: Mutex::new(PendingState {
      completion: Some(Box::new(|_| {})), location: None,
    }) });
    NativeObservedNavigation { pending: pending.clone() }.cancel();
    assert!(!registry.insert(7, pending));
    assert!(registry.0.lock().unwrap().is_empty());
  }
}
