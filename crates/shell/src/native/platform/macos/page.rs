//! `PageDriver` for WKWebView.
//!
//! Every operation hops to the UI thread through `Webview::with_webview` (inline when already
//! there) and completes its `Done` exactly once, including when the hop itself fails.
//! WebKit's completion handlers arrive on the UI thread; the `Done` they call may run on it.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, Message, msg_send, sel};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSWindow};
use objc2_foundation::{NSDictionary, NSError, NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_web_kit::{
    WKContentWorld, WKSnapshotConfiguration, WKUserContentController, WKUserScript,
    WKUserScriptInjectionTime, WKWebView,
};
use serde_json::Value;
use tauri::Webview;
use tauri::webview::PlatformWebview;

use super::{Platform, edit, print};
use crate::native::page::{
    CaptureSpec, Done, EditCommand, KeyChord, PageError, PdfSpec, QuietSpec, Viewport,
};
use crate::native::platform::PageDriver;

/// The AppKit and WebKit objects behind a `PlatformWebview`, valid inside one UI-thread job.
pub(super) struct Surface<'a> {
    pub(super) mtm: MainThreadMarker,
    pub(super) view: &'a WKWebView,
    pub(super) window: Option<&'a NSWindow>,
    pub(super) controller: &'a WKUserContentController,
}

impl<'a> Surface<'a> {
    pub(super) fn new(platform: &'a PlatformWebview) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let view = std::ptr::NonNull::new(platform.inner().cast::<WKWebView>())?;
        let controller =
            std::ptr::NonNull::new(platform.controller().cast::<WKUserContentController>())?;
        let window = std::ptr::NonNull::new(platform.ns_window().cast::<NSWindow>());
        // SAFETY: Tauri hands out pointers to the live web view, its content controller and
        // its window for the duration of the `with_webview` callback, on the UI thread.
        unsafe {
            Some(Self {
                mtm,
                view: view.as_ref(),
                window: window.map(|window| window.as_ref()),
                controller: controller.as_ref(),
            })
        }
    }
}

/// An Objective-C object as the untyped receiver `msg_send!` takes.
pub(super) fn object<T: Message>(value: &T) -> &AnyObject {
    // SAFETY: every Objective-C object pointer is a valid `AnyObject` reference.
    unsafe { &*(value as *const T).cast::<AnyObject>() }
}

fn unavailable() -> PageError {
    PageError::Platform("The web view is not available on the UI thread.".to_owned())
}

fn take<T>(slot: &Mutex<Option<Done<T>>>) -> Option<Done<T>> {
    slot.lock().unwrap_or_else(|error| error.into_inner()).take()
}

/// Run `job` on the UI thread with the web view's objects. When the hop fails, `done`
/// completes with the error instead of being dropped.
pub(super) fn run<T: Send + 'static>(
    webview: &Webview,
    done: Done<T>,
    job: impl FnOnce(&Surface<'_>, Done<T>) + Send + 'static,
) {
    let slot = Arc::new(Mutex::new(Some(done)));
    let queued = slot.clone();
    let sent = webview.with_webview(move |platform| {
        let Some(done) = take(&queued) else { return };
        match Surface::new(&platform) {
            Some(surface) => job(&surface, done),
            None => done(Err(unavailable())),
        }
    });
    if let Err(error) = sent
        && let Some(done) = take(&slot)
    {
        done(Err(PageError::Platform(error.to_string())));
    }
}

/// The message of a failed script: WebKit keeps the JavaScript exception text in `userInfo`.
fn script_error(error: &NSError) -> String {
    let key = NSString::from_str("WKJavaScriptExceptionMessage");
    error
        .userInfo()
        .objectForKey(&key)
        .and_then(|message| message.downcast_ref::<NSString>().map(|text| text.to_string()))
        .unwrap_or_else(|| error.localizedDescription().to_string())
}

fn answer(result: *mut AnyObject, error: *mut NSError) -> Result<Option<String>, PageError> {
    // SAFETY: WebKit passes null or live objects that outlive its completion handler.
    if let Some(error) = unsafe { error.as_ref() } {
        return Err(PageError::Platform(script_error(error)));
    }
    let Some(result) = (unsafe { result.as_ref() }) else { return Ok(None) };
    Ok(result.downcast_ref::<NSString>().map(|text| text.to_string()))
}

/// Run `body` as the body of an async function in the page world of the main frame. The
/// completion gets the string the body returned, or `None` for any other value.
pub(super) fn call_async(
    surface: &Surface<'_>,
    body: &str,
    completion: impl FnOnce(Result<Option<String>, PageError>) + 'static,
) {
    let slot = RefCell::new(Some(completion));
    let block: RcBlock<dyn Fn(*mut AnyObject, *mut NSError)> =
        RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
            if let Some(completion) = slot.borrow_mut().take() {
                completion(answer(result, error));
            }
        });
    let arguments: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
    // SAFETY: the body is a plain string and the block matches the completion handler type.
    unsafe {
        let world = WKContentWorld::pageWorld(surface.mtm);
        surface.view.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
            &NSString::from_str(body),
            Some(&*arguments),
            None,
            &world,
            Some(&*block),
        );
    }
}

/// The page script's answer as JSON: no value (`undefined`) is `null`.
fn json_of(answer: Option<String>) -> Result<Value, PageError> {
    match answer {
        None => Ok(Value::Null),
        Some(text) => serde_json::from_str(&text).map_err(|error| {
            PageError::Platform(format!("The page answered with invalid JSON: {error}"))
        }),
    }
}

/// The script is one expression, embedded as it is: the page's CSP has no `unsafe-eval`, so
/// nothing here may go through `eval`. `callAsyncJavaScript` takes a function body, so the
/// expression's value (awaited when asked) is returned and serialized by value. Windows
/// answers the same contract through `Runtime.evaluate`.
fn evaluate_in(surface: &Surface<'_>, script: &str, await_promise: bool, done: Done<Value>) {
    let wait = if await_promise { "await " } else { "" };
    let body = format!(
        "const r = {wait}(\n{script}\n);\n\
         return r === undefined ? undefined : JSON.stringify(r);"
    );
    call_async(surface, &body, move |outcome| done(outcome.and_then(json_of)));
}

/// `WKErrorJavaScriptResultTypeIsUnsupported`: the script ran, but its completion value (a
/// DOM node, a promise) cannot be handed to native code. For a script run for its effect that
/// is not a failure.
const RESULT_TYPE_UNSUPPORTED: isize = 5;

/// Run `source` as a classic script of the main frame, in the page world, for its effect.
/// `evaluateJavaScript` compiles the text as a script, which the page's CSP does not govern
/// (no `eval` is involved), and statements are fine. The completion value is dropped.
fn script_async(surface: &Surface<'_>, source: &str, done: Done<()>) {
    let slot = RefCell::new(Some(done));
    let block: RcBlock<dyn Fn(*mut AnyObject, *mut NSError)> =
        RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
            let Some(done) = slot.borrow_mut().take() else { return };
            // SAFETY: WebKit passes null or live objects that outlive its completion handler.
            let unsupported = unsafe { error.as_ref() }
                .is_some_and(|error| error.code() == RESULT_TYPE_UNSUPPORTED);
            done(if unsupported { Ok(()) } else { answer(result, error).map(|_| ()) });
        });
    // SAFETY: the source is a plain string and the block matches the completion handler type.
    unsafe {
        surface
            .view
            .evaluateJavaScript_completionHandler(&NSString::from_str(source), Some(&*block));
    }
}

/// What a snapshot temporarily changes about the view; dropping it puts everything back.
struct Stage {
    view: Retained<WKWebView>,
    window: Option<Retained<NSWindow>>,
    revealed: Option<(f64, bool)>,
    unhid: bool,
    frames: Option<(Option<NSRect>, NSRect)>,
    background: Option<bool>,
}

impl Stage {
    fn new(surface: &Surface<'_>) -> Self {
        Self {
            view: surface.view.retain(),
            window: surface.window.map(|window| window.retain()),
            revealed: None,
            unhid: false,
            frames: None,
            background: None,
        }
    }

    /// A hidden window or view does not render, so a snapshot of it would be blank: show it
    /// invisibly (fully transparent, ignoring the mouse) for the duration of the snapshot.
    fn reveal(&mut self) {
        if let Some(window) = &self.window
            && !window.isVisible()
        {
            self.revealed = Some((window.alphaValue(), window.ignoresMouseEvents()));
            window.setAlphaValue(0.0);
            window.setIgnoresMouseEvents(true);
            window.orderFrontRegardless();
        }
        if self.view.isHidden() {
            self.view.setHidden(false);
            self.unhid = true;
        }
    }

    fn resize(&mut self, width: f64, height: f64) {
        let size = NSSize { width, height };
        if self.frames.is_none() {
            let window = self.window.as_ref().map(|window| window.frame());
            self.frames = Some((window, self.view.frame()));
        }
        if let Some(window) = &self.window {
            window.setContentSize(size);
        }
        self.view.setFrame(NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size });
    }

    /// Stop the view painting its own background, so the snapshot keeps its alpha.
    fn transparent(&mut self) -> Result<(), PageError> {
        let view = object(&*self.view);
        // SAFETY: both selectors are checked before they are sent.
        unsafe {
            let reads: bool = msg_send![view, respondsToSelector: sel!(_drawsBackground)];
            let writes: bool = msg_send![view, respondsToSelector: sel!(_setDrawsBackground:)];
            if !(reads && writes) {
                return Err(PageError::Unsupported("transparent"));
            }
            self.background = Some(msg_send![view, _drawsBackground]);
            let _: () = msg_send![view, _setDrawsBackground: false];
        }
        Ok(())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Some(drawn) = self.background {
            // SAFETY: `transparent` checked that the view answers to the selector.
            unsafe {
                let _: () = msg_send![object(&*self.view), _setDrawsBackground: drawn];
            }
        }
        if let Some((window_frame, view_frame)) = self.frames.take() {
            if let (Some(window), Some(frame)) = (&self.window, window_frame) {
                window.setFrame_display(frame, false);
            }
            self.view.setFrame(view_frame);
        }
        if self.unhid {
            self.view.setHidden(true);
        }
        if let (Some(window), Some((alpha, ignores_mouse))) = (&self.window, self.revealed.take()) {
            window.orderOut(None);
            window.setAlphaValue(alpha);
            window.setIgnoresMouseEvents(ignores_mouse);
        }
    }
}

/// Snapshots larger than this edge are clamped: the bitmap would not fit in memory.
const MAX_EDGE: f64 = 16384.0;

fn png_of(image: &NSImage) -> Result<Vec<u8>, PageError> {
    let failed =
        |what: &str| PageError::Platform(format!("The snapshot could not be encoded: {what}."));
    let tiff = image.TIFFRepresentation().ok_or_else(|| failed("no bitmap"))?;
    let bitmap = NSBitmapImageRep::imageRepWithData(&tiff).ok_or_else(|| failed("no image data"))?;
    let properties: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
    // SAFETY: an empty property dictionary is valid for every file type.
    let png = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties)
    };
    png.map(|data| data.to_vec()).ok_or_else(|| failed("no PNG data"))
}

/// Take the snapshot of `spec`'s region, then put the view back before answering.
fn snapshot(stage: Stage, spec: CaptureSpec, mtm: MainThreadMarker, done: Done<Vec<u8>>) {
    let view = stage.view.clone();
    let backing = stage.window.as_ref().map_or(1.0, |window| window.backingScaleFactor());
    let region = match spec.clip {
        Some(clip) => NSRect {
            origin: NSPoint { x: clip.x, y: clip.y },
            size: NSSize { width: clip.width, height: clip.height },
        },
        None => view.bounds(),
    };
    // The image has `snapshotWidth` points, `backing` pixels each: ask for the pixel size
    // the spec names (CSS pixels times its scale).
    let width = (region.size.width * spec.scale).min(MAX_EDGE) / backing;
    // SAFETY: plain configuration object on the UI thread.
    let configuration = unsafe {
        let configuration = WKSnapshotConfiguration::new(mtm);
        configuration.setRect(region);
        configuration.setSnapshotWidth(Some(&*NSNumber::new_f64(width)));
        configuration
    };
    let state = RefCell::new(Some((stage, done)));
    let block: RcBlock<dyn Fn(*mut NSImage, *mut NSError)> =
        RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            let Some((stage, done)) = state.borrow_mut().take() else { return };
            // SAFETY: WebKit passes null or live objects that outlive this handler.
            let outcome = match (unsafe { image.as_ref() }, unsafe { error.as_ref() }) {
                (Some(image), _) => png_of(image),
                (None, Some(error)) => {
                    Err(PageError::Platform(error.localizedDescription().to_string()))
                }
                (None, None) => Err(PageError::Platform("WebKit returned no snapshot.".to_owned())),
            };
            drop(stage);
            done(outcome);
        });
    // SAFETY: the block matches the completion handler type.
    unsafe { view.takeSnapshotWithConfiguration_completionHandler(Some(&*configuration), &block) };
}

fn page_size(answer: Option<String>) -> Result<(f64, f64), PageError> {
    let size: Vec<f64> = serde_json::from_str(&answer.unwrap_or_default())
        .map_err(|_| PageError::Platform("The page size could not be measured.".to_owned()))?;
    match size[..] {
        [width, height] if width > 0.0 && height > 0.0 => {
            Ok((width.min(MAX_EDGE), height.min(MAX_EDGE)))
        }
        _ => Err(PageError::Platform("The page has no size to capture.".to_owned())),
    }
}

fn capture(surface: &Surface<'_>, spec: CaptureSpec, done: Done<Vec<u8>>) {
    let mut stage = Stage::new(surface);
    stage.reveal();
    if spec.transparent
        && let Err(error) = stage.transparent()
    {
        return done(Err(error));
    }
    let mtm = surface.mtm;
    if !spec.full_page {
        return snapshot(stage, spec, mtm, done);
    }
    let body = "return JSON.stringify([document.documentElement.scrollWidth, \
                document.documentElement.scrollHeight]);";
    call_async(surface, body, move |outcome| match outcome.and_then(page_size) {
        Ok((width, height)) => {
            stage.resize(width, height);
            snapshot(stage, spec, mtm, done);
        }
        Err(error) => done(Err(error)),
    });
}

fn resize(surface: &Surface<'_>, viewport: Viewport) -> Result<(), PageError> {
    if viewport.width == 0
        || viewport.height == 0
        || !(viewport.device_scale_factor.is_finite() && viewport.device_scale_factor > 0.0)
    {
        return Err(PageError::Invalid("the viewport must have a positive size".to_owned()));
    }
    let window = surface
        .window
        .ok_or_else(|| PageError::Platform("The web view is not in a window.".to_owned()))?;
    let size = NSSize { width: f64::from(viewport.width), height: f64::from(viewport.height) };
    window.setContentSize(size);
    surface.view.setFrame(NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size });
    Ok(())
}

/// The in-page quiet detector: the document loaded, its fonts and images settled, and no
/// resource finished loading for `quiet_ms`. Resource timing only reports finished requests,
/// so a request still in flight for longer than the quiet window is not seen.
fn quiet_script(spec: QuietSpec) -> String {
    format!(
        r"const quiet = {quiet};
const limit = {limit};
let over = false;
let last = performance.now();
let observer = null;
try {{
  observer = new PerformanceObserver(() => {{ last = performance.now(); }});
  observer.observe({{ type: 'resource' }});
}} catch (_) {{}}
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const settled = (async () => {{
  if (document.readyState !== 'complete') {{
    await new Promise((resolve) => window.addEventListener('load', resolve, {{ once: true }}));
  }}
  try {{ await document.fonts.ready; }} catch (_) {{}}
  last = performance.now();
  while (!over) {{
    const loading = Array.from(document.images).some((image) => !image.complete);
    if (!loading && performance.now() - last >= quiet) return 'quiet';
    await pause(Math.max(10, Math.min(quiet / 4, 100)));
  }}
  return 'timeout';
}})();
const outcome = await Promise.race([settled, pause(limit).then(() => 'timeout')]);
over = true;
if (observer) observer.disconnect();
return outcome;",
        quiet = spec.quiet_ms,
        limit = spec.timeout_ms,
    )
}

impl PageDriver for Platform {
    fn evaluate(webview: &Webview, script: String, await_promise: bool, done: Done<Value>) {
        run(webview, done, move |surface, done| evaluate_in(surface, &script, await_promise, done));
    }

    fn capture_png(webview: &Webview, spec: CaptureSpec, done: Done<Vec<u8>>) {
        run(webview, done, move |surface, done| capture(surface, spec, done));
    }

    fn print_pdf(webview: &Webview, spec: PdfSpec, done: Done<Vec<u8>>) {
        run(webview, done, move |surface, done| print::save_pdf(surface, spec, done));
    }

    fn edit(webview: &Webview, command: EditCommand, done: Done<()>) {
        run(webview, done, move |surface, done| edit::edit(surface, command, done));
    }

    fn key(webview: &Webview, chord: KeyChord, done: Done<()>) {
        run(webview, done, move |surface, done| done(edit::key(surface, &chord)));
    }

    fn set_viewport(webview: &Webview, viewport: Viewport, done: Done<()>) {
        run(webview, done, move |surface, done| done(resize(surface, viewport)));
    }

    fn add_init_script(webview: &Webview, source: String, done: Done<()>) {
        run(webview, done, move |surface, done| {
            // SAFETY: the script is a plain string; it runs at the start of every frame's
            // document, as `Page.addScriptToEvaluateOnNewDocument` does.
            unsafe {
                let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                    surface.mtm.alloc(),
                    &NSString::from_str(&source),
                    WKUserScriptInjectionTime::AtDocumentStart,
                    false,
                );
                surface.controller.addUserScript(&script);
            }
            done(Ok(()));
        });
    }

    fn run_script(webview: &Webview, source: String, done: Done<()>) {
        run(webview, done, move |surface, done| script_async(surface, &source, done));
    }

    fn wait_quiet(webview: &Webview, spec: QuietSpec, done: Done<()>) {
        run(webview, done, move |surface, done| {
            call_async(surface, &quiet_script(spec), move |outcome| {
                done(match outcome {
                    Ok(Some(state)) if state == "quiet" => Ok(()),
                    Ok(_) => Err(PageError::Timeout),
                    Err(error) => Err(error),
                });
            });
        });
    }

    fn reload_ignoring_cache(webview: &Webview) -> Result<(), String> {
        webview
            .with_webview(|platform| {
                if let Some(surface) = Surface::new(&platform) {
                    // SAFETY: plain reload of the live web view on the UI thread.
                    let _ = unsafe { surface.view.reloadFromOrigin() };
                }
            })
            .map_err(|error| error.to_string())
    }
}
