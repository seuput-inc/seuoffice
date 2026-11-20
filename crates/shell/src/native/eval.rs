//! Script evaluation bound to one registered document.
//!
//! [`DocumentEvaluation`] is the retained handle. Dropping it or calling `cancel` ends Rust
//! delivery only; it cannot interrupt JavaScript that is already running. The script runs
//! behind a bridge check of the public document id, so a document that replaced the
//! target between dispatch and execution answers `StaleDocument` instead of running it.

use std::fmt;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::defer::defer;
use super::document::{DocumentContext, DocumentRevocation};
use super::ids::DocumentId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalError {
    /// The evaluation handle was dropped or cancelled.
    Cancelled,
    /// The document was revoked or replaced before the script could answer.
    StaleDocument,
    /// The webview could not take the script.
    Transport(String),
    /// The script threw.
    Script(String),
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("evaluation was cancelled"),
            Self::StaleDocument => f.write_str("document is no longer current"),
            Self::Transport(message) => write!(f, "evaluation transport failed: {message}"),
            Self::Script(message) => write!(f, "script failed: {message}"),
        }
    }
}

impl std::error::Error for EvalError {}

/// Marker the wrapper throws when the document id check fails.
pub(crate) const STALE_MARKER: &str = "seuoffice_stale_document";

type Completion = Box<dyn FnOnce(Result<Value, EvalError>) + Send>;

struct Request {
    origin: DocumentContext,
    completion: Mutex<Option<Completion>>,
}

impl Request {
    fn finish(&self, result: Result<Value, EvalError>) {
        let Some(completion) = self
            .completion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        else {
            return;
        };
        let origin = self.origin.clone();
        defer(move || {
            let result = if origin.is_live() || matches!(result, Err(EvalError::Cancelled)) {
                result
            } else {
                Err(EvalError::StaleDocument)
            };
            completion(result);
        });
    }

    fn abandon(&self) {
        drop(
            self.completion
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take(),
        );
    }
}

/// Retain until completion. Dropping cancels only this request.
pub struct DocumentEvaluation {
    request: Arc<Request>,
    _loss: DocumentRevocation,
}

impl DocumentEvaluation {
    pub fn cancel(&self) {
        self.request.finish(Err(EvalError::Cancelled));
    }
}

impl Drop for DocumentEvaluation {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The script the webview runs: the bridge check first, then the source as an expression
/// whose value (awaited when it is a promise) is the result.
///
/// The source is embedded, never passed through `eval` or `Function`: the shell's pages carry
/// a CSP without `unsafe-eval`, which an `eval` in the page's world would hit. It is therefore
/// one expression; several statements go in an IIFE.
pub(crate) fn wrap_script(document: DocumentId, source: &str) -> String {
    format!(
        "(async()=>{{if(!window.__seuofficeCheck?.({}))throw new Error(\"{STALE_MARKER}\");\
         return await (\n{source}\n);}})()",
        document.get()
    )
}

fn script_error(message: String) -> EvalError {
    if message.contains(STALE_MARKER) {
        EvalError::StaleDocument
    } else {
        EvalError::Script(message)
    }
}

pub(crate) fn start(
    origin: &DocumentContext,
    source: String,
    completion: Completion,
) -> Result<DocumentEvaluation, EvalError> {
    let control = origin
        .browser()
        .control()
        .ok_or_else(|| EvalError::Transport("the webview is not attached".into()))?;
    let request = Arc::new(Request {
        origin: origin.clone(),
        completion: Mutex::new(Some(completion)),
    });
    let lost = request.clone();
    let subscription = origin
        .on_revoke(move || lost.finish(Err(EvalError::StaleDocument)))
        .ok_or(EvalError::StaleDocument)?;
    let script = wrap_script(origin.identity().id(), &source);
    let reply = request.clone();
    // Dispatch inside the gate: a revocation cannot interleave between check and send.
    let sent = origin.with_live(|| {
        control.evaluate(
            script,
            Box::new(move |result| reply.finish(result.map_err(script_error))),
        )
    });
    match sent {
        Some(Ok(())) => Ok(DocumentEvaluation {
            request,
            _loss: subscription,
        }),
        Some(Err(error)) => {
            request.abandon();
            Err(EvalError::Transport(error))
        }
        None => {
            request.abandon();
            Err(EvalError::StaleDocument)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::browser::testing::{FakeControl, registered_context_with_control};
    use crate::native::events::RevokeReason;

    #[test]
    fn the_script_checks_the_public_document_id_and_embeds_the_source_without_eval() {
        let id = DocumentId::mint().unwrap();
        let script = wrap_script(id, "window.x = \"a\"");
        assert!(script.contains(&format!("__seuofficeCheck?.({})", id.get())));
        assert!(script.contains("return await (\nwindow.x = \"a\"\n);"));
        assert!(!script.contains("eval(") && !script.contains("Function("));
    }

    #[test]
    fn a_result_reaches_the_caller_once_while_the_document_is_live() {
        let control = FakeControl::new();
        let (_browser, context) = registered_context_with_control("eval-ok", control.clone());
        let answers = Arc::new(Mutex::new(Vec::new()));
        let sink = answers.clone();
        let evaluation = context
            .evaluate("1+1".into(), move |r| sink.lock().unwrap().push(r))
            .unwrap();
        control.answer_evaluation(Ok(Value::from(2)));
        control.answer_evaluation(Ok(Value::from(3)));
        assert_eq!(*answers.lock().unwrap(), [Ok(Value::from(2))]);
        drop(evaluation);
    }

    #[test]
    fn a_result_after_revocation_is_stale_and_dropping_cancels() {
        let control = FakeControl::new();
        let (_browser, context) = registered_context_with_control("eval-stale", control.clone());
        let answers = Arc::new(Mutex::new(Vec::new()));
        let sink = answers.clone();
        let evaluation = context
            .evaluate("1".into(), move |r| sink.lock().unwrap().push(r))
            .unwrap();
        context
            .0
            .revoke(RevokeReason::Navigation)
            .unwrap()
            .deliver();
        control.answer_evaluation(Ok(Value::from(1)));
        assert_eq!(*answers.lock().unwrap(), [Err(EvalError::StaleDocument)]);
        drop(evaluation);

        let (_other_browser, other) =
            registered_context_with_control("eval-cancel", FakeControl::new());
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let sink = cancelled.clone();
        drop(
            other
                .evaluate("1".into(), move |r| sink.lock().unwrap().push(r))
                .unwrap(),
        );
        assert_eq!(*cancelled.lock().unwrap(), [Err(EvalError::Cancelled)]);
    }

    #[test]
    fn evaluating_on_a_revoked_document_is_refused_up_front() {
        let (_browser, context) =
            registered_context_with_control("eval-revoked", FakeControl::new());
        context.0.revoke(RevokeReason::Close).unwrap().deliver();
        assert_eq!(
            context.evaluate("1".into(), |_| ()).err(),
            Some(EvalError::StaleDocument)
        );
    }
}
