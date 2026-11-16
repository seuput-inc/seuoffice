use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tauri::Webview;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC, COREWEBVIEW2_PRINT_DIALOG_KIND_SYSTEM,
    ICoreWebView2, ICoreWebView2_16,
};
use windows::core::Interface;

use super::Platform;
use super::cdp::{Cdp, Reply};
use crate::native::page::{
    CaptureSpec, Done, EditCommand, KeyChord, PageError, PdfSpec, QuietSpec, Viewport,
};
use crate::native::platform::{PageDriver, PrintOps, PrintOutcome};

/// Run `run` on the UI thread with the webview's COM object. `done` completes exactly once:
/// with the operation's answer, or with the error that kept the operation from starting.
fn on_core<T: Send + 'static>(
    webview: &Webview, done: Done<T>, run: impl FnOnce(ICoreWebView2, Done<T>) + Send + 'static,
) {
    let slot = Arc::new(Mutex::new(Some(done)));
    let take = |slot: &Mutex<Option<Done<T>>>| {
        slot.lock().unwrap_or_else(|error| error.into_inner()).take()
    };
    let ui_slot = slot.clone();
    let sent = webview.with_webview(move |platform| {
        let Some(done) = take(&ui_slot) else {
            return;
        };
        // SAFETY: UI thread, inside `with_webview`; the controller is the webview's own.
        match unsafe { platform.controller().CoreWebView2() } {
            Ok(core) => run(core, done),
            Err(error) => done(Err(PageError::Platform(format!(
                "The WebView2 core is unavailable: {error}"
            )))),
        }
    });
    if let Err(error) = sent
        && let Some(done) = take(&slot)
    {
        done(Err(PageError::Platform(error.to_string())));
    }
}

/// The Chromium editing command a menu action names.
fn command_name(command: EditCommand) -> &'static str {
    match command {
        EditCommand::Cut => "cut",
        EditCommand::Copy => "copy",
        EditCommand::Paste => "paste",
        EditCommand::PasteAndMatchStyle => "pasteAndMatchStyle",
        EditCommand::SelectAll => "selectAll",
        EditCommand::Undo => "undo",
        EditCommand::Redo => "redo",
        EditCommand::Delete => "delete",
    }
}

impl PageDriver for Platform {
    /// Both kinds go through `Runtime.evaluate`: unlike `ExecuteScript`, it reports a thrown
    /// error instead of answering `null`, and it awaits a promise when asked to.
    fn evaluate(webview: &Webview, script: String, await_promise: bool, done: Done<Value>) {
        on_core(webview, done, move |core, done| {
            Cdp::new(core).evaluate(script, await_promise, done);
        });
    }

    fn run_script(webview: &Webview, source: String, done: Done<()>) {
        on_core(webview, done, move |core, done| Cdp::new(core).run_script(source, done));
    }

    fn capture_png(webview: &Webview, spec: CaptureSpec, done: Done<Vec<u8>>) {
        on_core(webview, done, move |core, done| Cdp::new(core).capture(spec, done));
    }

    fn print_pdf(webview: &Webview, spec: PdfSpec, done: Done<Vec<u8>>) {
        on_core(webview, done, move |core, done| Cdp::new(core).print_pdf(&spec, done));
    }

    /// The page gets focus first (as macOS makes the view first responder): Chromium runs an
    /// editing command on the focused frame of the focused page.
    fn edit(webview: &Webview, command: EditCommand, done: Done<()>) {
        let focused = webview.with_webview(|platform| {
            // SAFETY: UI thread, inside `with_webview`. Best effort: a view that cannot take
            // focus still runs the command on its own page.
            let _ = unsafe {
                platform.controller().MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC)
            };
        });
        if let Err(error) = focused {
            return done(Err(PageError::Platform(error.to_string())));
        }
        on_core(webview, done, move |core, done| {
            Cdp::new(core).editing_command(command_name(command), done);
        });
    }

    fn key(webview: &Webview, chord: KeyChord, done: Done<()>) {
        on_core(webview, done, move |core, done| Cdp::new(core).key(&chord, done));
    }

    fn set_viewport(webview: &Webview, viewport: Viewport, done: Done<()>) {
        on_core(webview, done, move |core, done| Cdp::new(core).set_viewport(viewport, done));
    }

    fn add_init_script(webview: &Webview, source: String, done: Done<()>) {
        on_core(webview, done, move |core, done| Cdp::new(core).add_init_script(source, done));
    }

    fn wait_quiet(webview: &Webview, spec: QuietSpec, done: Done<()>) {
        let owner = webview.clone();
        on_core(webview, done, move |core, done| {
            super::cdp::wait_quiet(&owner, Cdp::new(core), spec, done);
        });
    }

    /// Dispatches the reload; a failure of the protocol call itself arrives later and is
    /// logged, since this call has already answered by then.
    fn reload_ignoring_cache(webview: &Webview) -> Result<(), String> {
        webview
            .with_webview(|platform| {
                // SAFETY: UI thread, inside `with_webview`.
                let core = match unsafe { platform.controller().CoreWebView2() } {
                    Ok(core) => core,
                    Err(error) => {
                        eprintln!("shell: the reload found no WebView2 core: {error}");
                        return;
                    }
                };
                let reply: Reply<Value> = Box::new(|result| {
                    if let Err(error) = result {
                        eprintln!("shell: the cache-ignoring reload failed: {error}");
                    }
                });
                Cdp::new(core).call("Page.reload", &json!({ "ignoreCache": true }), reply);
            })
            .map_err(|error| error.to_string())
    }
}

impl PrintOps for Platform {
    /// `ShowPrintUI(SYSTEM)` opens the system dialog and does not say how it ended.
    fn print_dialog(webview: &Webview, done: Done<PrintOutcome>) {
        on_core(webview, done, |core, done| {
            let result = match core.cast::<ICoreWebView2_16>() {
                // SAFETY: UI thread; the call only opens the dialog.
                Ok(core) => unsafe { core.ShowPrintUI(COREWEBVIEW2_PRINT_DIALOG_KIND_SYSTEM) }
                    .map(|()| PrintOutcome::Unreported)
                    .map_err(|error| PageError::Platform(format!("ShowPrintUI failed: {error}"))),
                Err(_) => Err(PageError::Unsupported("the system print dialog")),
            };
            done(result);
        });
    }
}
