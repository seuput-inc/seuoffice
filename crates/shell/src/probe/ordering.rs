//! P2: the order of navigation commit, document registration and revocation as the shell's
//! event stream reports it (design W7, F0 follow-up 6, S2 and S4 inferences).
//!
//! One browser with the document bridge walks the page sequence of `p2-a.html` / `p2-b.html`:
//! a same-document navigation (fragment, `pushState`), a cross-document one, a reload and a
//! history traversal back (which the system webview may restore from its page cache). The
//! native events, with timestamps, are the record. At every `DocumentRegistered` the probe asks
//! the registered document to evaluate an expression at once (S2: the page may not know its
//! registration yet).

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tauri::Manager;

use super::harness::{Ctx, Direct, app_url, destroy, open_direct, wait_done};
use super::log::record;
use crate::native::{BridgeMode, DocumentEvaluation, NativeEvent, NativeEventKind};

pub fn run(ctx: &Ctx) -> Result<(), String> {
    let label = "probe-p2";
    let evaluations: Arc<Mutex<Vec<DocumentEvaluation>>> = Arc::default();
    let kept = evaluations.clone();
    let mut spec = Direct::new(label, &app_url("shell", "p2-a.html"));
    spec.bridge = BridgeMode::Invoke;
    spec.hook = Some(Arc::new(move |event: &NativeEvent| {
        let NativeEventKind::DocumentRegistered { context } = &event.kind else {
            return;
        };
        let document = context.identity().id().get();
        let started = super::log::now_ms();
        let result = context.evaluate("1 + 1".to_owned(), move |result| {
            record(
                "rust",
                "p2.eval.at_registration",
                json!({
                    "document": document,
                    "ok": result.is_ok(),
                    "value": result.as_ref().ok(),
                    "error": result.as_ref().err().map(ToString::to_string),
                    "ms": super::log::now_ms() - started,
                }),
            );
        });
        match result {
            Ok(handle) => super::harness::keep(&kept, handle),
            Err(error) => record(
                "rust",
                "p2.eval.at_registration",
                json!({ "document": document, "ok": false, "error": error.to_string() }),
            ),
        }
    }));
    let opened = open_direct(ctx, spec)?;
    let ready = opened
        .timeline
        .wait_kind("DocumentReady", 0, Duration::from_secs(30));
    record(
        "rust",
        "p2.wait",
        json!({ "what": "first DocumentReady", "ok": ready }),
    );
    let finished = wait_done(label, 0, 90);
    record(
        "rust",
        "p2.wait",
        json!({ "what": "page sequence", "ok": finished }),
    );
    // late events of the last restore or re-registration
    std::thread::sleep(Duration::from_secs(3));
    // a reload that ignores the cache (the menu's Reload with ignore_cache)
    let base = opened.timeline.len();
    let reload = ctx
        .app
        .get_webview(label)
        .ok_or_else(|| "no webview".to_owned())
        .and_then(|webview| crate::native::menu::reload(&webview, true));
    let registered = opened
        .timeline
        .wait_kind("DocumentRegistered", base, Duration::from_secs(15));
    record(
        "rust",
        "p2.reload_ignoring_cache",
        json!({ "dispatched": reload.is_ok(), "error": reload.err(), "registered_after": registered }),
    );
    std::thread::sleep(Duration::from_secs(1));

    let state = opened.browser.registered_context().map(|context| {
        let (send, receive) = mpsc::channel();
        let evaluation = context.evaluate("document.title + ' ' + location.href".to_owned(), move |result| {
            let _ = send.send(result.map(|value| value.to_string()).map_err(|error| error.to_string()));
        });
        let answer = match evaluation {
            Ok(_handle) => receive.recv_timeout(Duration::from_secs(10)).map_err(|error| error.to_string()).and_then(|result| result),
            Err(error) => Err(error.to_string()),
        };
        json!({ "document": context.identity().id().get(), "url": context.url().as_str(), "evaluate": answer.as_ref().ok(), "evaluate_error": answer.err() })
    });
    record(
        "rust",
        "p2.final",
        json!({ "registered": state.is_some(), "state": state, "browser_url": opened.browser.url().map(|url| url.to_string()) }),
    );
    destroy(ctx, label);
    if finished {
        Ok(())
    } else {
        Err("the P2 page sequence did not finish".into())
    }
}
