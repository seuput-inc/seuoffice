//! `--probe <name> --out <dir>`: the hosted WebView probes P1-P10 (design 0.5), run by the real
//! shell binary on the real system webview (WKWebView on the macOS runner, WebView2 on the
//! Windows one).
//!
//! Gated the way `--measure-dialog` is: a runtime flag, compiled into every build, inert
//! without it. The shell starts as usual (builder, schemes, core thread, engine host); where
//! it would open its first window, the probe takes over. Each probe opens real windows through
//! the native module (`native::*` builders, `PageOps`, `RequestPolicy`, the shell's schemes),
//! drives fixture pages from `probe-fixtures/`, and writes what happened as JSON lines in
//! `<dir>/<name>.jsonl` plus the artifacts it produced (PDFs, PNGs) beside it.
//!
//! A probe never decides pass or fail. `ci/probe-check.mjs` reads the lines and says, per
//! assertion, observed, violated or unobserved, with the line that shows it. The process exits
//! 0 when the probe ran to its end, 3 when it could not (a window did not open, a panic),
//! 124 when it ran past its deadline.
//!
//! The pages are served by the shell's own app scheme. The workflow stages them under
//! `SEUOFFICE_APPS_DIR` (see `probe-fixtures/README.md`), and starts the shell with
//! `SEUOFFICE_SHELL_PARTIAL=1`, as `--measure-dialog` does, so the table need not be complete.
//!
//! | probe | asks |
//! |---|---|
//! | p1 | synchronous XHR to the `seuoffice-ipc` scheme (WK custom scheme, Windows `.localhost` shape) |
//! | p2 | commit, registration and revocation order; page cache restore |
//! | p3 | secure context, isolation and storage of the five schemes and the app hosts |
//! | p4 | the PDF contract (`PdfSpec`) against a print corpus |
//! | p5 | macOS page operations of the hidden HTML engine |
//! | p6 | Windows CDP page operations |
//! | p7 | navigation failure signals and request policy |
//! | p8 | the JavaScript context menu path, main frame and child frame |
//! | p9 | `on_new_window` and view close timing |
//! | p10 | `--measure-dialog` itself (no code here; the checker reads its JSON) |

mod context;
mod contextmenu;
mod failures;
mod harness;
mod log;
mod ordering;
mod pageops;
mod popups;
mod print;
mod schemes;
mod sync_xhr;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::json;

use crate::host::HostContext;

/// A probe never runs longer than this; the process ends with 124.
const DEADLINE: Duration = Duration::from_secs(420);

pub struct Request {
    pub name: String,
    pub out: PathBuf,
}

/// The `--probe` request of this process, if any.
pub fn request() -> Option<&'static Request> {
    static REQUEST: OnceLock<Option<Request>> = OnceLock::new();
    REQUEST.get_or_init(parse).as_ref()
}

fn parse() -> Option<Request> {
    let mut args = std::env::args().skip(1);
    let (mut name, mut out) = (None, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--probe" => name = args.next(),
            "--out" => out = args.next().map(PathBuf::from),
            _ => {}
        }
    }
    match (name, out) {
        (Some(name), Some(out)) => Some(Request { name, out }),
        (Some(_), None) => {
            eprintln!("shell: --probe needs --out <dir>");
            std::process::exit(2);
        }
        _ => None,
    }
}

/// Register the probe's schemes on the builder, after the shell's own. A no-op without
/// `--probe`.
pub fn install(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    let Some(request) = request() else {
        return builder;
    };
    if let Err(error) = log::init(&request.out, &request.name) {
        eprintln!("shell: probe: {error}");
        std::process::exit(2);
    }
    log::record(
        "rust",
        "probe.start",
        json!({
            "probe": request.name,
            "platform": harness::platform(),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "exe": std::env::current_exe().ok().map(|path| path.display().to_string()),
            "apps_dir": std::env::var("SEUOFFICE_APPS_DIR").ok(),
        }),
    );
    schemes::install(builder)
}

/// The shell stopped before the probe could run: the record ends with the reason, so the
/// checker reports it instead of "the process may have crashed". A no-op without `--probe`.
pub fn startup_failed(why: &str) {
    if let Some(request) = request() {
        log::record(
            "rust",
            "probe.end",
            json!({
                "probe": request.name,
                "ok": false,
                "error": format!("the shell did not start: {why}"),
            }),
        );
    }
}

/// Run the requested probe where the shell would open its first window.
pub fn run(request: &Request, context: &HostContext) -> Result<(), String> {
    let ctx = Arc::new(harness::Ctx::new(context));
    let name = request.name.clone();
    let finished = ctx.clone();
    std::thread::Builder::new()
        .name("probe".into())
        .spawn(move || {
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(&ctx, &name)));
            let (ok, error) = match outcome {
                Ok(Ok(())) => (true, None),
                Ok(Err(error)) => (false, Some(error)),
                Err(_) => (false, Some("the probe panicked".to_owned())),
            };
            log::record(
                "rust",
                "probe.end",
                json!({ "probe": name, "ok": ok, "error": error }),
            );
            finished.app.exit(if ok { 0 } else { 3 });
        })
        .map_err(|error| error.to_string())?;
    std::thread::Builder::new()
        .name("probe-deadline".into())
        .spawn(|| {
            std::thread::sleep(DEADLINE);
            log::record(
                "rust",
                "probe.timeout",
                json!({ "after_s": DEADLINE.as_secs() }),
            );
            std::process::exit(124);
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn dispatch(ctx: &harness::Ctx, name: &str) -> Result<(), String> {
    match name {
        "p1" => sync_xhr::run(ctx),
        "p2" => ordering::run(ctx),
        "p3" => context::run(ctx),
        "p4" => print::run(ctx),
        "p5" | "p6" => pageops::run(ctx, name),
        "p7" => failures::run(ctx),
        "p8" => contextmenu::run(ctx),
        "p9" => popups::run(ctx),
        other => Err(format!("unknown probe {other}")),
    }
}
