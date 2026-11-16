//! P3: what a document gets from the system webview depending on the scheme it is loaded
//! from: secure context, `crypto.subtle`, cross-origin isolation, streaming WebAssembly,
//! storage (design 0.5 P3, 3.5; S7 and S5 inferences).
//!
//! Each target is loaded in its own window and its `p3.context` record is the evidence. The
//! targets are the app hosts the shell serves with its real handler (`shell`, `docs`, `sheets`,
//! and the engine host, which the handler serves cross-origin isolated), the five schemes CEF
//! registered natively (stand-in handlers in probe mode, see `schemes.rs`), and the HWP shell
//! page with its same-origin studio frame.

use std::time::Duration;

use serde_json::json;

use super::harness::{Ctx, Direct, app_url, destroy, open_direct, scheme_url, wait_done};
use super::log::record;
use super::schemes::STANDINS;
use crate::app_scheme;

pub fn run(ctx: &Ctx) -> Result<(), String> {
    let engine = format!(
        "{}/probe/p3-context.html",
        app_scheme::native_origin(crate::engine_host::SCHEME_HOST)
    );
    let mut targets: Vec<(String, String)> = vec![
        ("app-shell".into(), app_url("shell", "p3-context.html")),
        ("app-docs".into(), app_url("docs", "p3-context.html")),
        // after docs: its `localStorage` must not hold docs' value
        ("app-sheets".into(), app_url("sheets", "p3-context.html")),
        ("app-engine".into(), engine),
    ];
    for scheme in STANDINS {
        targets.push((scheme.to_owned(), scheme_url(scheme, "p3/page.html")));
    }
    targets.push(("app-hwp".into(), app_url("hwp", "p3-hwp.html")));

    let mut missing = Vec::new();
    for (index, (name, url)) in targets.iter().enumerate() {
        let label = format!("probe-p3-{index}");
        record(
            "rust",
            "p3.target",
            json!({ "name": name, "url": url, "view": label }),
        );
        let from = super::log::pages().len();
        let opened = match open_direct(ctx, Direct::new(&label, url)) {
            Ok(opened) => opened,
            Err(error) => {
                record(
                    "rust",
                    "p3.target.failed",
                    json!({ "name": name, "error": error }),
                );
                missing.push(name.clone());
                continue;
            }
        };
        let done = wait_done(&label, from, 40);
        record(
            "rust",
            "p3.target.end",
            json!({ "name": name, "view": label, "page_done": done, "events": opened.timeline.len() }),
        );
        if !done {
            missing.push(name.clone());
        }
        destroy(ctx, &label);
        std::thread::sleep(Duration::from_millis(500));
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("no report from: {}", missing.join(", ")))
    }
}
