// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{borrow::Cow, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}};
use crate::{NativeContextToken, SchemeRequestProvenance};
use crate::webview::CefInitScript;
use cef::ImplNativeResourceDelivery;

type Response = http::Response<Vec<u8>>;
type Receipt = Arc<Mutex<Option<Result<(), String>>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase { Open, Accepted, Scheduled, Delivered, Cancelled, Failed }

struct CancelListener { active: AtomicBool, callback: Mutex<Option<Box<dyn FnOnce() + Send>>> }
#[derive(Default)]
struct Cancellation { cancelled: bool, listeners: Vec<Arc<CancelListener>> }
pub struct NativeSchemeCancelSubscription { listener: Arc<CancelListener> }
impl Drop for NativeSchemeCancelSubscription {
  fn drop(&mut self) { self.listener.active.store(false, Ordering::Release); }
}
fn deliver_cancel(listener: Arc<CancelListener>) {
  crate::native_evaluation::defer_completion(Box::new(move || {
    if !listener.active.swap(false, Ordering::AcqRel) { return; }
    let callback = listener.callback.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some(callback) = callback { callback(); }
  }));
}

struct Resource {
  phase: Mutex<Phase>,
  scripts: Arc<Vec<CefInitScript>>,
  main_frame: bool,
  provenance: SchemeRequestProvenance,
  cancellation: Mutex<Cancellation>,
  native_delivery: Option<cef::NativeResourceDelivery>,
}

/// One original CEF ResourceHandler. This is not proof of an issuing V8 context.
/// Resource cancellation and native-context revocation are distinct boundaries.
#[derive(Clone)]
pub struct NativeSchemeResponseTarget { resource: Arc<Resource> }

pub struct PreparedNativeSchemeResponse {
  target: NativeSchemeResponseTarget,
  response: Response,
  receipt: Receipt,
}

pub struct AcceptedNativeSchemeResponse {
  prepared: PreparedNativeSchemeResponse,
  responder: tauri::UriSchemeResponder,
}

pub struct RejectedNativeSchemeResponse {
  reason: &'static str,
  _prepared: PreparedNativeSchemeResponse,
  responder: tauri::UriSchemeResponder,
}

impl RejectedNativeSchemeResponse {
  pub fn reason(&self) -> &'static str { self.reason }
  pub fn into_responder(self) -> tauri::UriSchemeResponder { self.responder }
}

#[derive(Clone)]
pub(crate) struct PreparedResponseMarker {
  target: NativeSchemeResponseTarget,
  receipt: Receipt,
}

impl NativeSchemeResponseTarget {
  pub(crate) fn new(scripts: Arc<Vec<CefInitScript>>, main_frame: bool, provenance: SchemeRequestProvenance,
    native_delivery: Option<cef::NativeResourceDelivery>) -> Self {
    Self { resource: Arc::new(Resource { phase: Mutex::new(Phase::Open), scripts, main_frame, provenance,
      native_delivery, cancellation: Mutex::default() }) }
  }

  /// Observes the original ResourceHandler cancellation callback. This is not
  /// native-document authority or a synchronous URLLoader detach receipt.
  pub fn on_cancel(&self, callback: impl FnOnce() + Send + 'static) -> Result<NativeSchemeCancelSubscription, String> {
    let listener = Arc::new(CancelListener { active: AtomicBool::new(true), callback: Mutex::new(Some(Box::new(callback))) });
    let cancelled = {
      let mut state = self.resource.cancellation.lock().map_err(|_| "Native resource cancellation state was poisoned.")?;
      if !state.cancelled {
        state.listeners.retain(|listener| listener.active.load(Ordering::Acquire));
        state.listeners.push(listener.clone());
      }
      state.cancelled
    };
    if cancelled { deliver_cancel(listener.clone()); }
    Ok(NativeSchemeCancelSubscription { listener })
  }

  /// Body ownership, HTML parsing, script injection and CSP hashing run here,
  /// outside every native, asset and application-document admission gate.
  pub fn prepare(&self, response: http::Response<Cow<'static, [u8]>>) -> Result<PreparedNativeSchemeResponse, String> {
    if *self.resource.phase.lock().map_err(|_| "Resource response state was poisoned.")? != Phase::Open {
      return Err("Original native resource is no longer open.".into());
    }
    let response = crate::cef_impl::request_handler::prepare_response(
      response, &self.resource.scripts, self.resource.main_frame);
    Ok(PreparedNativeSchemeResponse { target: self.clone(), response, receipt: Arc::new(Mutex::new(None)) })
  }

  pub(crate) fn cancel(&self) {
    *self.resource.phase.lock().unwrap_or_else(|error| error.into_inner()) = Phase::Cancelled;
    let listeners = {
      let mut state = self.resource.cancellation.lock().unwrap_or_else(|error| error.into_inner());
      state.cancelled = true;
      std::mem::take(&mut state.listeners)
    };
    for listener in listeners { deliver_cancel(listener); }
  }

  pub(crate) fn schedule(&self) -> Result<(), String> {
    let mut phase = self.resource.phase.lock().map_err(|_| "Resource response state was poisoned.")?;
    if *phase != Phase::Accepted { return Err("Original native resource was cancelled or already answered.".into()); }
    *phase = Phase::Scheduled;
    Ok(())
  }

  pub(crate) fn fail(&self) {
    let mut phase = self.resource.phase.lock().unwrap_or_else(|error| error.into_inner());
    if *phase == Phase::Scheduled { *phase = Phase::Failed; }
  }

  /// Called by the queued CEF IO task after all owner gates have been released.
  /// The original native detach gate is independent of Rust Cancel delivery.
  pub(crate) fn claim_delivery(&self) -> bool {
    let Ok(mut phase) = self.resource.phase.lock() else { return false };
    if *phase != Phase::Scheduled || self.resource.native_delivery.as_ref().is_none_or(|gate| gate.is_cancelled() != 0) { return false; }
    *phase = Phase::Delivered;
    true
  }

  pub(crate) fn take_prepared(&self, response: http::Response<Cow<'static, [u8]>>) -> Result<(Response, Receipt), String> {
    let (mut parts, body) = response.into_parts();
    let marker = parts.extensions.remove::<PreparedResponseMarker>().ok_or("Native response preparation marker is missing.")?;
    if !Arc::ptr_eq(&self.resource, &marker.target.resource) {
      *marker.receipt.lock().unwrap_or_else(|error| error.into_inner()) = Some(Err("Native response belongs to a different resource.".into()));
      return Err("Native response belongs to a different resource.".into());
    }

    Ok((http::Response::from_parts(parts, body.into_owned()), marker.receipt))
  }
  pub(crate) fn prepare_unowned(&self, response: http::Response<Cow<'static, [u8]>>) -> Result<(Response, Receipt), String> {
    let prepared = self.prepare(response)?;
    {
      let mut phase = self.resource.phase.lock().map_err(|_| "Resource response state was poisoned.")?;
      if *phase != Phase::Open || !self.resource.native_delivery.as_ref().is_some_and(|gate| gate.try_accept() != 0) {
        return Err("Original native resource was detached, revoked or already answered.".into());
      }
      *phase = Phase::Accepted;
    }
    self.take_prepared(prepared.into_response())
  }
}

impl PreparedNativeSchemeResponse {
  /// Only a bounded original-resource state transition. The caller must hold
  /// its independently captured document/native owner gate around this call.
  /// No CEF callback, task posting, body preparation or latest-view lookup runs.
  pub fn accept(self, responder: tauri::UriSchemeResponder) -> Result<AcceptedNativeSchemeResponse, RejectedNativeSchemeResponse> {
    let claim = match self.target.resource.phase.lock() {
      Ok(mut phase) if *phase == Phase::Open
        && self.target.resource.native_delivery.as_ref().is_some_and(|gate| gate.try_accept() != 0) => {
          *phase = Phase::Accepted; Ok(())
        }
      Ok(_) => Err("Original native resource was cancelled or already answered."),
      Err(_) => Err("Resource response state was poisoned."),
    };
    match claim {
      Ok(()) => Ok(AcceptedNativeSchemeResponse { prepared: self, responder }),
      Err(reason) => Err(RejectedNativeSchemeResponse { reason, _prepared: self, responder }),
    }
  }

  /// Fences acceptance with an independently authenticated original context.
  /// Browser equality is a destination check, not scheme-request origin proof.
  /// Do not call while already holding this token's registry gate; use accept.
  pub fn accept_for_native(self, origin: &NativeContextToken, responder: tauri::UriSchemeResponder) -> Result<AcceptedNativeSchemeResponse, RejectedNativeSchemeResponse> {
    let provenance = &self.target.resource.provenance;
    if provenance.native_document_owner() != Some(origin) {
      return Err(RejectedNativeSchemeResponse { reason: "Native response destination differs from its captured owner.", _prepared: self, responder });
    }
    let mut pending = Some((self, responder));
    origin.with_live(|| {
      let (prepared, responder) = pending.take().expect("original response admission");
      prepared.accept(responder)
    }).unwrap_or_else(|| {
      let (prepared, responder) = pending.take().expect("unaccepted original response");
      Err(RejectedNativeSchemeResponse { reason: "Native document was revoked before resource acceptance.", _prepared: prepared, responder })
    })
  }

  pub(crate) fn into_response(self) -> http::Response<Cow<'static, [u8]>> {
    let (mut parts, body) = self.response.into_parts();
    parts.extensions.insert(PreparedResponseMarker { target: self.target, receipt: self.receipt });
    http::Response::from_parts(parts, Cow::Owned(body))
  }
}

impl AcceptedNativeSchemeResponse {
  /// Runs outside all owner gates. Success means the immutable answer was
  /// queued to the original resource's IO callback, not that CEF consumed it.
  pub fn finish(self) -> Result<(), String> {
    let receipt = self.prepared.receipt.clone();
    self.responder.respond(self.prepared.into_response());
    let result = receipt.lock().map_err(|_| "Native response receipt was poisoned.")?.take()
      .ok_or_else(|| "Native response callback did not acknowledge enqueue.".to_owned())?;
    result
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  fn target() -> NativeSchemeResponseTarget {
    NativeSchemeResponseTarget::new(Arc::new(Vec::new()), true, SchemeRequestProvenance {
      browser_id: None, frame_id: None, frame_is_main: None, browser_state: None, navigation: None, document: None,
      native_document_owner: None,
    }, None)
  }
  #[test]
  fn original_resource_cancel_prevents_delivery_and_never_cancels_successor() {
    let original = target();
    let replacement = target();
    *original.resource.phase.lock().unwrap() = Phase::Accepted;
    original.schedule().unwrap();
    original.cancel();
    assert!(!original.claim_delivery());
    assert_eq!(*replacement.resource.phase.lock().unwrap(), Phase::Open);
  }
  #[test]
  fn acceptance_and_cancel_have_one_resource_mutex_order() {
    let resource = target();
    resource.cancel();
    assert!(resource.schedule().is_err());
    assert!(!resource.claim_delivery());
    let accepted = target();
    *accepted.resource.phase.lock().unwrap() = Phase::Accepted;
    accepted.schedule().unwrap();
    assert!(!accepted.claim_delivery(), "Rust state alone cannot replace the native delivery gate");
  }
}
