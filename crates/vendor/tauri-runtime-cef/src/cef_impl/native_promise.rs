// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::native_eval::{EvaluationTicket, NativeEvalError, WeakPendingEvaluations};

/// The serializer supplies either a JSON document or the text of a JS rejection.
#[derive(Clone, Copy)]
pub(crate) enum Reaction {
  Fulfilled,
  Rejected,
}

/// Claims a reaction once and publishes only if the exact request survived
/// serialization. The serializer must not retain CEF context or V8 handles.
pub(crate) fn settle(
  pending: &WeakPendingEvaluations,
  ticket: &EvaluationTicket,
  reaction: Reaction,
  serialize: impl FnOnce() -> Result<String, String>,
) -> bool {
  let Some(pending) = pending.upgrade() else { return false };
  if !pending.claim(ticket) { return false; }
  let serialized = serialize();
  if !pending.is_claimed(ticket) { return false; }
  let result = match (reaction, serialized) {
    (Reaction::Fulfilled, Ok(json)) => serde_json::from_str(&json)
      .map_err(|error| NativeEvalError::Serialization(error.to_string())),
    (Reaction::Fulfilled, Err(error)) => Err(NativeEvalError::Serialization(error)),
    (Reaction::Rejected, Ok(error) | Err(error)) => Err(NativeEvalError::JavaScript(error)),
  };
  pending.finish(ticket, result)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{cell::{Cell, RefCell}, rc::Rc};
  use super::super::native_eval::{ContextIdentity, EvalResult, PendingEvaluations};

  fn context() -> ContextIdentity {
    ContextIdentity { browser_id: 1, renderer_child_id: 2, document_token: "document".into(), context_token: "context".into(), world_id: 0 }
  }

  fn request(pending: &PendingEvaluations, output: Rc<RefCell<Vec<EvalResult>>>) -> EvaluationTicket {
    pending.begin(&context(), "nonce".into(), Box::new(move |result| output.borrow_mut().push(result))).unwrap()
  }

  #[test]
  fn settlement_is_once_and_rejects_unsupported_json() {
    let pending = PendingEvaluations::default();
    assert!(pending.register_context(context()));
    let output = Rc::new(RefCell::new(Vec::new()));
    let ticket = request(&pending, output.clone());
    let calls = Cell::new(0);
    assert!(settle(&pending.downgrade(), &ticket, Reaction::Fulfilled, || { calls.set(calls.get() + 1); Ok("{\"n\":3}".into()) }));
    assert!(!settle(&pending.downgrade(), &ticket, Reaction::Fulfilled, || { calls.set(calls.get() + 1); Ok("null".into()) }));
    assert_eq!(calls.get(), 1);
    assert_eq!(output.borrow()[0], Ok(serde_json::json!({"n": 3})));

    let invalid = request(&pending, output.clone());
    assert!(settle(&pending.downgrade(), &invalid, Reaction::Fulfilled, || Ok("undefined".into())));
    assert!(matches!(&output.borrow()[1], Err(NativeEvalError::Serialization(_))));
  }

  #[test]
  fn release_or_cancel_during_serialization_wins() {
    for cancel in [false, true] {
      let pending = PendingEvaluations::default();
      assert!(pending.register_context(context()));
      let output = Rc::new(RefCell::new(Vec::new()));
      let ticket = request(&pending, output.clone());
      assert!(!settle(&pending.downgrade(), &ticket, Reaction::Fulfilled, || {
        if cancel { assert!(pending.cancel(&ticket)); } else { pending.release_context(&context()); }
        Ok("true".into())
      }));
      let expected = if cancel { NativeEvalError::Cancelled } else { NativeEvalError::StaleContext };
      assert_eq!(&*output.borrow(), &[Err(expected)]);
    }
  }

  #[test]
  fn rejected_promise_reports_javascript_error() {
    let pending = PendingEvaluations::default();
    assert!(pending.register_context(context()));
    let output = Rc::new(RefCell::new(Vec::new()));
    let ticket = request(&pending, output.clone());
    assert!(settle(&pending.downgrade(), &ticket, Reaction::Rejected, || Ok("rejected".into())));
    assert_eq!(&*output.borrow(), &[Err(NativeEvalError::JavaScript("rejected".into()))]);
  }
}
