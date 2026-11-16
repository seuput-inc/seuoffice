//! P1: a synchronous XHR from a page to the shell's `seuoffice-ipc` scheme (design W6).
//!
//! The page is opened as a real shell view (`Windows::open_window_with_options`, the shell
//! surface's preload bridge), so the real `sendSync`, its nonce and the scheme's handler are
//! all in play. What the page and the handler saw is in the record: the page's `p1.*` lines and
//! the `scheme.request` lines of the handler wrapper.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use shell_contract::Surface;
use shell_contract::ids::ViewId;

use super::harness::{Ctx, app_url, note_event};
use super::log::{Timeline, record, wait_page};
use crate::host::window::{Bridge, ViewSpec, WindowOptions, WindowSpec};
use crate::native::NativeEvent;

pub fn run(ctx: &Ctx) -> Result<(), String> {
    let timeline = Arc::new(Timeline::default());
    // the real window names its view `view-<id>`; the label is not known before it opens, and
    // events can arrive first, so they are recorded under the id the callback gets
    let sink_timeline = timeline.clone();
    let on_frame: Arc<dyn Fn(ViewId, &NativeEvent) + Send + Sync> =
        Arc::new(move |view: ViewId, event: &NativeEvent| {
            note_event(&format!("view-{}", view.0), &sink_timeline, event);
        });
    let spec = WindowSpec {
        title: "probe p1".into(),
        size: (640.0, 480.0),
        min_size: None,
        visible: true,
        decorations: true,
        transparent: false,
        resizable: true,
        parent: None,
        focus: false,
        main: false,
        view: ViewSpec {
            url: "genoffice-app://shell/probe/p1-sync-xhr.html".into(),
            bridge: Bridge::Surface(Surface::Shell),
        },
    };
    let options = WindowOptions {
        on_frame: Some(on_frame),
        ..WindowOptions::default()
    };
    let (window, view) = ctx.windows.open_window_with_options(spec, options)?;
    let label = format!("view-{}", view.0);
    record(
        "rust",
        "win.open",
        json!({ "label": label, "window": window.0, "url": app_url("shell", "p1-sync-xhr.html"), "real": true }),
    );
    let done = wait_page(&label, "page.done", 0, Duration::from_secs(90)).is_some();
    record("rust", "p1.wait", json!({ "page_done": done }));
    super::harness::destroy(ctx, &label);
    if done {
        Ok(())
    } else {
        Err("the P1 page did not finish".into())
    }
}
