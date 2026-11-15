// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::sync::{Arc, LazyLock, Mutex};
use cef::*;
use crate::{NativeContextRevocation, NativeContextToken, NativeEvalError};

type ResultValue = Result<serde_json::Value, NativeEvalError>;
type Completion = Box<dyn FnOnce(ResultValue) + Send>;
static COMPLETIONS: LazyLock<Mutex<Vec<Box<dyn FnOnce() + Send>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

struct Request {
  origin: NativeContextToken,
  nonce: String,
  completion: Mutex<Option<Completion>>,
}

impl Request {
  fn finish(&self, result: ResultValue) {
    let Some(completion) = self.completion.lock().unwrap_or_else(|error| error.into_inner()).take() else { return };
    let origin = self.origin.clone();
    defer_completion(Box::new(move || {
      let result = if origin.is_verified() || matches!(&result, Err(NativeEvalError::Cancelled)) {
        result
      } else { Err(NativeEvalError::StaleContext) };
      completion(result);
    }));
  }
}

/// Retain until completion. Dropping cancels only this exact native request.
pub struct NativeEvaluation {
  request: Arc<Request>,
  endpoint: NativeContextEvaluation,
  _loss: NativeContextRevocation,
}

impl NativeEvaluation {
  pub fn cancel(&self) {
    self.request.finish(Err(NativeEvalError::Cancelled));
    let mut task = CancelEvaluation::new(self.endpoint.clone(), self.request.nonce.clone());
    let _ = post_task(sys::cef_thread_id_t::TID_UI.into(), Some(&mut task));
  }
}
impl Drop for NativeEvaluation {
  fn drop(&mut self) { self.cancel(); }
}

impl NativeContextToken {
  /// Await promises and serialize by value in this captured native context.
  /// No CEF-frame/CDP/current-context fallback is permitted.
  pub fn evaluate(&self, source: String, completion: impl FnOnce(ResultValue) + Send + 'static) -> Result<NativeEvaluation, NativeEvalError> {
    let endpoint = self.endpoints().ok_or(NativeEvalError::StaleContext)?.evaluation;
    let nonce = crate::cef_impl::trusted_init::random_capability().ok_or(NativeEvalError::Internal)?;
    let request = Arc::new(Request { origin: self.clone(), nonce, completion: Mutex::new(Some(Box::new(completion))) });
    let loss = request.clone();
    let subscription = self.on_revoke(move || loss.finish(Err(NativeEvalError::StaleContext)))
      .ok_or(NativeEvalError::StaleContext)?;
    let mut task = StartEvaluation::new(endpoint.clone(), request.clone(), source);
    if post_task(sys::cef_thread_id_t::TID_UI.into(), Some(&mut task)) != 1 {
      request.completion.lock().unwrap_or_else(|error| error.into_inner()).take();
      return Err(NativeEvalError::TransportClosed);
    }
    Ok(NativeEvaluation { request, endpoint, _loss: subscription })
  }
}

wrap_native_context_evaluation_callback! {
  struct EvaluationReply { request: Arc<Request> }
  impl NativeContextEvaluationCallback {
    fn on_complete(&self, nonce: Option<&CefString>, code: Option<&CefString>, payload: Option<&CefString>) {
      let (Some(nonce), Some(code), Some(payload)) = (nonce, code, payload) else {
        self.request.finish(Err(NativeEvalError::Internal));
        return;
      };
      if nonce.to_string() != self.request.nonce {
        self.request.finish(Err(NativeEvalError::Internal));
        return;
      }
      let code = code.to_string();
      let payload = payload.to_string();
      let result = if code.is_empty() {
        serde_json::from_str(&payload).map_err(|error| NativeEvalError::Serialization(error.to_string()))
      } else { Err(crate::native_invoke::eval_error(&code, payload)) };
      self.request.finish(result);
    }
  }
}

wrap_task! {
  struct StartEvaluation { endpoint: NativeContextEvaluation, request: Arc<Request>, source: String }
  impl Task {
    fn execute(&self) {
      if self.request.completion.lock().unwrap_or_else(|error| error.into_inner()).is_none() { return; }
      let mut callback = EvaluationReply::new(self.request.clone());
      let admitted = self.request.origin.with_live(|| self.endpoint.evaluate(
        Some(&CefString::from(self.request.nonce.as_str())), Some(&CefString::from(self.source.as_str())), Some(&mut callback)));
      // Native callbacks only enqueue completion. They never run client code or
      // reacquire this registry while Evaluate holds the short admission gate.
      if admitted.is_none() { self.request.finish(Err(NativeEvalError::StaleContext)); }
    }
  }
}
wrap_task! {
  struct CancelEvaluation { endpoint: NativeContextEvaluation, nonce: String }
  impl Task {
    fn execute(&self) { self.endpoint.cancel(Some(&CefString::from(self.nonce.as_str()))); }
  }
}
wrap_task! {
  struct DrainEvaluations;
  impl Task { fn execute(&self) { drain_completions(); } }
}

pub(crate) fn defer_completion(completion: Box<dyn FnOnce() + Send>) {
  COMPLETIONS.lock().unwrap_or_else(|error| error.into_inner()).push(completion);
  let mut task = DrainEvaluations::new();
  if post_task(sys::cef_thread_id_t::TID_UI.into(), Some(&mut task)) != 1 {
    log::error!("native completion retained for runtime shutdown");
  }
}

pub(crate) fn drain_completions() {
  loop {
    let batch = {
      let mut queue = COMPLETIONS.lock().unwrap_or_else(|error| error.into_inner());
      if queue.is_empty() { return; }
      std::mem::take(&mut *queue)
    };
    for completion in batch {
      if std::panic::catch_unwind(std::panic::AssertUnwindSafe(completion)).is_err() {
        log::error!("native evaluation completion panicked");
      }
    }
  }
}
