// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{cell::RefCell, collections::{HashMap, HashSet}, rc::{Rc, Weak}};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An exact renderer-native context identity, never a frame's current document.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ContextIdentity {
  pub(crate) browser_id: i32,
  pub(crate) renderer_child_id: i32,
  pub(crate) document_token: String,
  pub(crate) context_token: String,
  pub(crate) world_id: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", content = "message", rename_all = "snake_case")]
pub enum NativeEvalError {
  StaleContext,
  Cancelled,
  DuplicateRequest,
  JavaScript(String),
  Serialization(String),
  TransportClosed,
  Internal,
}

pub(crate) type EvalResult = Result<Value, NativeEvalError>;
type Completion = Box<dyn FnOnce(EvalResult)>;

#[derive(Clone)]
pub(crate) struct EvaluationTicket {
  context: ContextIdentity,
  nonce: String,
  instance: Rc<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase { Pending, Settling }

struct Request {
  instance: Rc<()>,
  phase: Phase,
  completion: Completion,
}

#[derive(Default)]
struct State {
  live: HashSet<ContextIdentity>,
  retired: HashSet<ContextIdentity>,
  requests: HashMap<ContextIdentity, HashMap<String, Request>>,
}

/// Renderer-thread-only request arbitration. Never hold a borrow while running
/// V8, serializing values, or invoking a completion: each can reenter it.
#[derive(Clone, Default)]
pub(crate) struct PendingEvaluations(Rc<RefCell<State>>);

/// V8 settlement handlers must not keep a released registry alive.
#[derive(Clone)]
pub(crate) struct WeakPendingEvaluations(Weak<RefCell<State>>);

impl WeakPendingEvaluations {
  pub(crate) fn upgrade(&self) -> Option<PendingEvaluations> {
    self.0.upgrade().map(PendingEvaluations)
  }
}

impl PendingEvaluations {
  pub(crate) fn downgrade(&self) -> WeakPendingEvaluations {
    WeakPendingEvaluations(Rc::downgrade(&self.0))
  }

  pub(crate) fn register_context(&self, context: ContextIdentity) -> bool {
    let mut state = self.0.borrow_mut();
    if state.retired.contains(&context) { return false; }
    state.live.insert(context)
  }

  pub(crate) fn begin(
    &self,
    context: &ContextIdentity,
    nonce: String,
    completion: Completion,
  ) -> Result<EvaluationTicket, NativeEvalError> {
    let mut state = self.0.borrow_mut();
    if !state.live.contains(context) { return Err(NativeEvalError::StaleContext); }
    let requests = state.requests.entry(context.clone()).or_default();
    if requests.contains_key(&nonce) { return Err(NativeEvalError::DuplicateRequest); }
    let instance = Rc::new(());
    requests.insert(nonce.clone(), Request { instance: instance.clone(), phase: Phase::Pending, completion });
    Ok(EvaluationTicket { context: context.clone(), nonce, instance })
  }

  /// Claim once before examining a promise result or invoking a JS serializer.
  pub(crate) fn claim(&self, ticket: &EvaluationTicket) -> bool {
    let mut state = self.0.borrow_mut();
    if !state.live.contains(&ticket.context) { return false; }
    let Some(request) = state.requests.get_mut(&ticket.context).and_then(|requests| requests.get_mut(&ticket.nonce)) else { return false };
    if !Rc::ptr_eq(&request.instance, &ticket.instance) || request.phase != Phase::Pending { return false; }
    request.phase = Phase::Settling;
    true
  }

  /// Serialization may reenter lifecycle callbacks; inspect no V8 result if
  /// the exact claimed request was cancelled or its context was released.
  pub(crate) fn is_claimed(&self, ticket: &EvaluationTicket) -> bool {
    let state = self.0.borrow();
    state.live.contains(&ticket.context)
      && state.requests.get(&ticket.context)
        .and_then(|requests| requests.get(&ticket.nonce))
        .is_some_and(|request| Rc::ptr_eq(&request.instance, &ticket.instance)
          && request.phase == Phase::Settling)
  }

  /// Completes only the claimed instance. A release or cancellation during
  /// serialization wins and has already delivered the corresponding error.
  pub(crate) fn finish(&self, ticket: &EvaluationTicket, result: EvalResult) -> bool {
    let mut state = self.0.borrow_mut();
    if !state.live.contains(&ticket.context) { return false; }
    let Some(requests) = state.requests.get_mut(&ticket.context) else { return false };
    if !requests.get(&ticket.nonce).is_some_and(|request| {
      Rc::ptr_eq(&request.instance, &ticket.instance) && request.phase == Phase::Settling
    }) { return false; }
    let request = requests.remove(&ticket.nonce).expect("checked request");
    drop(state);
    (request.completion)(result);
    true
  }

  pub(crate) fn cancel(&self, ticket: &EvaluationTicket) -> bool {
    let mut state = self.0.borrow_mut();
    let Some(requests) = state.requests.get_mut(&ticket.context) else { return false };
    if !requests.get(&ticket.nonce).is_some_and(|request| Rc::ptr_eq(&request.instance, &ticket.instance)) { return false; }
    let request = requests.remove(&ticket.nonce).expect("checked request");
    drop(state);
    (request.completion)(Err(NativeEvalError::Cancelled));
    true
  }

  pub(crate) fn release_context(&self, context: &ContextIdentity) {
    let mut state = self.0.borrow_mut();
    state.live.remove(context);
    state.retired.insert(context.clone());
    let requests = state.requests.remove(context).unwrap_or_default();
    drop(state);
    for (_, request) in requests { (request.completion)(Err(NativeEvalError::StaleContext)); }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{Arc, Mutex};

  fn context(document: &str, v8: &str) -> ContextIdentity {
    ContextIdentity { browser_id: 1, renderer_child_id: 2, document_token: document.into(), context_token: v8.into(), world_id: 0 }
  }

  #[test]
  fn release_during_serialization_cancels_once_and_never_targets_successor() {
    let registry = PendingEvaluations::default();
    let old = context("old", "v8-old");
    let new = context("new", "v8-new");
    assert!(registry.register_context(old.clone()));
    let output = Arc::new(Mutex::new(Vec::new()));
    let result = output.clone();
    let ticket = registry.begin(&old, "nonce".into(), Box::new(move |value| result.lock().unwrap().push(value))).unwrap();
    assert!(registry.claim(&ticket));
    assert!(registry.is_claimed(&ticket));
    registry.release_context(&old);
    assert!(!registry.is_claimed(&ticket));
    assert!(registry.register_context(new.clone()));
    let new_output = output.clone();
    let successor = registry.begin(&new, "nonce".into(), Box::new(move |value| new_output.lock().unwrap().push(value))).unwrap();
    assert!(!registry.finish(&ticket, Ok(Value::Bool(true))));
    assert_eq!(*output.lock().unwrap(), vec![Err(NativeEvalError::StaleContext)]);
    assert!(registry.claim(&successor));
    assert!(registry.finish(&successor, Ok(Value::Bool(true))));
    assert_eq!(*output.lock().unwrap(), vec![Err(NativeEvalError::StaleContext), Ok(Value::Bool(true))]);
    assert!(!registry.register_context(old));
  }

  #[test]
  fn reentrant_cancel_and_nonce_reuse_do_not_complete_new_request() {
    let registry = PendingEvaluations::default();
    let owner = context("document", "v8");
    assert!(registry.register_context(owner.clone()));
    let old = registry.begin(&owner, "nonce".into(), Box::new(|_| {})).unwrap();
    assert!(registry.claim(&old));
    assert!(!registry.claim(&old));
    assert!(registry.cancel(&old));
    assert!(!registry.is_claimed(&old));
    let new = registry.begin(&owner, "nonce".into(), Box::new(|_| {})).unwrap();
    assert!(!registry.finish(&old, Ok(Value::Null)));
    assert!(registry.claim(&new));
    assert!(registry.finish(&new, Ok(Value::Null)));
    assert!(!registry.cancel(&new));
  }

  #[test]
  fn completion_can_reenter_release_without_holding_registry_lock() {
    let registry = PendingEvaluations::default();
    let owner = context("document", "v8");
    assert!(registry.register_context(owner.clone()));
    let inside = registry.clone();
    let captured = owner.clone();
    let ticket = registry.begin(&owner, "nonce".into(), Box::new(move |_| {
      inside.release_context(&captured);
    })).unwrap();
    assert!(registry.claim(&ticket));
    assert!(registry.finish(&ticket, Ok(Value::Null)));
    assert!(!registry.register_context(owner));
  }
}
