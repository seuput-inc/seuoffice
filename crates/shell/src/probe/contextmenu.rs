//! P8: the JavaScript `contextmenu` path in the main frame and in the HWP studio child frame
//! (design 3.6; S7 inferences).
//!
//! The page (`p8-main.html`, served as the `hwp` host with a `studio/` child of the same
//! origin) dispatches `contextmenu` events at a text node, an editable block, a textarea and an
//! image, then asks the shell's command the way the bridge does. What it records: whether the
//! bridge's listener cancelled each event, the caret and `insertText` primitives the
//! misspelling replacement relies on, and what the child frame has (a bridge or not).
//!
//! The window is a native browser with the document bridge but no shell view, so the real
//! context menu handler refuses the request instead of showing a menu; its refusal line
//! ("a context menu of 'probe-p8-main' was refused") in the shell's log shows the request went
//! through the real ingress to the real handler. The checker reads that log.

use std::time::Duration;

use serde_json::json;

use super::harness::{Ctx, Direct, app_url, destroy, open_direct, wait_done};
use super::log::{self, record};
use crate::native::BridgeMode;

pub const LABEL: &str = "probe-p8-main";

pub fn run(ctx: &Ctx) -> Result<(), String> {
    let from = log::pages().len();
    let mut spec = Direct::new(LABEL, &app_url("hwp", "p8-main.html"));
    spec.bridge = BridgeMode::Invoke;
    spec.size = (640.0, 520.0);
    let opened = open_direct(ctx, spec)?;
    let ready = opened.timeline.wait_kind("DocumentReady", 0, Duration::from_secs(30));
    let done = wait_done(LABEL, from, 60);
    record("rust", "p8.wait", json!({ "document_ready": ready, "page_done": done }));
    // the command runs on the UI thread and the refusal is logged there
    std::thread::sleep(Duration::from_secs(2));
    destroy(ctx, LABEL);
    if done { Ok(()) } else { Err("the P8 page did not finish".into()) }
}
