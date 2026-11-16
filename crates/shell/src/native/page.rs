//! Typed page operations: what CEF gave through DevTools messages, as explicit calls.
//!
//! One authority parameter, [`Fence`], replaces the twelve `cdp_*` variants: it runs the
//! effect only if every captured owner is still live. Every operation completes exactly once
//! through [`Done`], including when the fence refuses.
//!
//! The OS work is `platform::PageDriver`; this module owns the types, the validation and the
//! fencing, none of which depend on the OS.

use std::fmt;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::Value;

use super::defer::is_ui_thread;
use super::platform::{Current, PageDriver};

/// The one authority argument: run the effect only if all captured owners are live. An
/// `Err` means the effect did not run.
pub type Fence = Arc<dyn Fn(&mut dyn FnMut()) -> Result<(), String> + Send + Sync>;

pub type Done<T> = Box<dyn FnOnce(Result<T, PageError>) + Send + 'static>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageError {
    /// The platform cannot do this (for example PDF header and footer templates on macOS).
    Unsupported(&'static str),
    /// The fence refused: an owner was revoked.
    Stale,
    Cancelled,
    Timeout,
    Invalid(String),
    Platform(String),
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "{what} is not supported on this platform"),
            Self::Stale => f.write_str("the document is no longer current"),
            Self::Cancelled => f.write_str("the page operation was cancelled"),
            Self::Timeout => f.write_str("the page operation timed out"),
            Self::Invalid(message) | Self::Platform(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PageError {}

/// A rectangle in CSS pixels of the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureSpec {
    pub clip: Option<Rect>,
    pub full_page: bool,
    pub transparent: bool,
    pub scale: f64,
}

impl Default for CaptureSpec {
    fn default() -> Self {
        Self {
            clip: None,
            full_page: false,
            transparent: false,
            scale: 1.0,
        }
    }
}

impl CaptureSpec {
    pub fn validate(&self) -> Result<(), PageError> {
        if !(self.scale.is_finite() && self.scale > 0.0) {
            return Err(PageError::Invalid("capture scale must be positive".into()));
        }
        if let Some(clip) = self.clip
            && !(clip.x.is_finite()
                && clip.y.is_finite()
                && clip.width.is_finite()
                && clip.height.is_finite()
                && clip.width > 0.0
                && clip.height > 0.0)
        {
            return Err(PageError::Invalid(
                "capture clip must have a positive size".into(),
            ));
        }
        Ok(())
    }
}

/// Header and footer HTML templates (Chromium syntax). Windows hands them to Chromium; macOS
/// renders them with WebKit and stamps them onto the printed PDF.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderFooter {
    pub header_template: String,
    pub footer_template: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PdfSpec {
    pub landscape: bool,
    /// Paper size in inches; `None` keeps the page's own size.
    pub paper_in: Option<(f64, f64)>,
    /// Top, right, bottom, left, in inches.
    pub margins_in: [f64; 4],
    pub scale: f64,
    pub print_background: bool,
    pub prefer_css_page_size: bool,
    /// Chromium range syntax: "1-3, 5, 8-". `None` prints every page.
    pub page_ranges: Option<String>,
    pub header_footer: Option<HeaderFooter>,
    pub tagged: bool,
    pub outline: bool,
}

impl Default for PdfSpec {
    fn default() -> Self {
        Self {
            landscape: false,
            paper_in: None,
            margins_in: [0.4; 4],
            scale: 1.0,
            print_background: false,
            prefer_css_page_size: false,
            page_ranges: None,
            header_footer: None,
            tagged: false,
            outline: false,
        }
    }
}

impl PdfSpec {
    pub fn validate(&self) -> Result<(), PageError> {
        if !(self.scale.is_finite() && (0.1..=2.0).contains(&self.scale)) {
            return Err(PageError::Invalid(
                "PDF scale must be between 0.1 and 2".into(),
            ));
        }
        if self
            .margins_in
            .iter()
            .any(|margin| !margin.is_finite() || *margin < 0.0)
        {
            return Err(PageError::Invalid(
                "PDF margins must not be negative".into(),
            ));
        }
        if let Some((width, height)) = self.paper_in
            && !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0)
        {
            return Err(PageError::Invalid("PDF paper size must be positive".into()));
        }
        if let Some(ranges) = &self.page_ranges {
            parse_page_ranges(ranges)?;
        }
        Ok(())
    }
}

/// Pages named by Chromium's range syntax, 1-based and inclusive; an open end is `u32::MAX`.
/// An empty text names every page (an empty list).
pub fn parse_page_ranges(text: &str) -> Result<Vec<std::ops::RangeInclusive<u32>>, PageError> {
    let invalid = || PageError::Invalid(format!("invalid page range \"{text}\""));
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut ranges = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        let number = |digits: &str| digits.trim().parse::<u32>().ok().filter(|page| *page >= 1);
        let range = match part.split_once('-') {
            None => {
                let page = number(part).ok_or_else(invalid)?;
                page..=page
            }
            Some((start, end)) => {
                let start = if start.trim().is_empty() {
                    1
                } else {
                    number(start).ok_or_else(invalid)?
                };
                let end = if end.trim().is_empty() {
                    u32::MAX
                } else {
                    number(end).ok_or_else(invalid)?
                };
                if start > end {
                    return Err(invalid());
                }
                start..=end
            }
        };
        ranges.push(range);
    }
    Ok(ranges)
}

/// Layout size of a hidden browser, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
}

/// Quiet means: loaded, no resource in flight for `quiet_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuietSpec {
    pub quiet_ms: u32,
    pub timeout_ms: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditCommand {
    Cut,
    Copy,
    Paste,
    PasteAndMatchStyle,
    SelectAll,
    Undo,
    Redo,
    Delete,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

/// A trusted key event for the page (HWP studio and Space shortcuts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyChord {
    /// `KeyboardEvent.key`, for example "e" or "Enter".
    pub key: String,
    /// `KeyboardEvent.code`, for example "KeyE".
    pub code: String,
    pub modifiers: Modifiers,
}

/// Completes once. Holds the caller's `Done` until the effect or the fence takes it.
struct DoneSlot<T>(Arc<Mutex<Option<Done<T>>>>);

impl<T> Clone for DoneSlot<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> DoneSlot<T> {
    fn new(done: Done<T>) -> Self {
        Self(Arc::new(Mutex::new(Some(done))))
    }

    fn take(&self) -> Option<Done<T>> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

/// Run `effect` under `fence` (or directly without one). The effect receives the `Done` and
/// owns completing it. When the fence refuses, `done` completes with [`PageError::Stale`]
/// and the effect never runs.
pub(crate) fn run_fenced<T: 'static>(
    fence: Option<&Fence>,
    done: Done<T>,
    effect: impl FnOnce(Done<T>),
) {
    let slot = DoneSlot::new(done);
    let mut effect = Some(effect);
    let mut run = {
        let slot = slot.clone();
        move || {
            if let (Some(effect), Some(done)) = (effect.take(), slot.take()) {
                effect(done);
            }
        }
    };
    match fence {
        Some(fence) => {
            let _ = fence(&mut run);
        }
        None => run(),
    }
    if let Some(done) = slot.take() {
        done(Err(PageError::Stale));
    }
}

/// The sync shape callers expect from the old `cdp()`: block this thread for the answer.
/// Never call it on the UI thread, which the answer needs.
pub fn blocking<T: Send + 'static>(
    timeout: Duration,
    call: impl FnOnce(Done<T>),
) -> Result<T, PageError> {
    if is_ui_thread() {
        return Err(PageError::Platform(
            "A blocking page operation was started on the UI thread.".into(),
        ));
    }
    blocking_unchecked(timeout, call)
}

fn blocking_unchecked<T: Send + 'static>(
    timeout: Duration,
    call: impl FnOnce(Done<T>),
) -> Result<T, PageError> {
    let (send, receive) = mpsc::sync_channel(1);
    call(Box::new(move |result| {
        let _ = send.send(result);
    }));
    match receive.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(PageError::Timeout),
        Err(RecvTimeoutError::Disconnected) => Err(PageError::Cancelled),
    }
}

/// Page operations of one browser, obtained from [`WebviewExt::page`].
///
/// [`WebviewExt::page`]: super::ext::WebviewExt::page
#[derive(Clone)]
pub struct PageOps {
    webview: tauri::Webview,
}

impl PageOps {
    pub(crate) fn new(webview: tauri::Webview) -> Self {
        Self { webview }
    }

    /// Evaluate ONE EXPRESSION in the page (several statements go in an IIFE; a trailing `;`
    /// or a statement list is a syntax error on macOS). With `await_promise` a returned
    /// promise is awaited and the answer is the JSON value of its result. The expression is
    /// never passed through `eval`: the shell's pages have no `unsafe-eval`. A classic script
    /// is [`PageOps::run_script`].
    pub fn evaluate(&self, js: &str, await_promise: bool, fence: Option<Fence>, done: Done<Value>) {
        let webview = self.webview.clone();
        let script = js.to_owned();
        run_fenced(fence.as_ref(), done, move |done| {
            Current::evaluate(&webview, script, await_promise, done);
        });
    }

    /// Run a classic script (statements, declarations, a trailing `;`) for its effect; no
    /// value comes back. For page helpers a caller installs (`addScript`), not for answers.
    pub fn run_script(&self, source: &str, fence: Option<Fence>, done: Done<()>) {
        let webview = self.webview.clone();
        let source = source.to_owned();
        run_fenced(fence.as_ref(), done, move |done| {
            Current::run_script(&webview, source, done)
        });
    }

    pub fn capture_png(&self, spec: CaptureSpec, fence: Option<Fence>, done: Done<Vec<u8>>) {
        if let Err(error) = spec.validate() {
            return done(Err(error));
        }
        let webview = self.webview.clone();
        run_fenced(fence.as_ref(), done, move |done| {
            Current::capture_png(&webview, spec, done)
        });
    }

    pub fn print_pdf(&self, spec: PdfSpec, fence: Option<Fence>, done: Done<Vec<u8>>) {
        if let Err(error) = spec.validate() {
            return done(Err(error));
        }
        let webview = self.webview.clone();
        run_fenced(fence.as_ref(), done, move |done| {
            Current::print_pdf(&webview, spec, done)
        });
    }

    pub fn edit(&self, command: EditCommand, done: Done<()>) {
        Current::edit(&self.webview, command, done);
    }

    pub fn key(&self, chord: KeyChord, done: Done<()>) {
        Current::key(&self.webview, chord, done);
    }

    pub fn set_viewport(&self, viewport: Viewport, done: Done<()>) {
        Current::set_viewport(&self.webview, viewport, done);
    }

    /// A script that runs at the start of every document this webview loads next.
    pub fn add_init_script(&self, source: &str, done: Done<()>) {
        Current::add_init_script(&self.webview, source.to_owned(), done);
    }

    pub fn wait_quiet(&self, spec: QuietSpec, fence: Option<Fence>, done: Done<()>) {
        let webview = self.webview.clone();
        run_fenced(fence.as_ref(), done, move |done| {
            Current::wait_quiet(&webview, spec, done)
        });
    }

    pub fn blocking<T: Send + 'static>(
        &self,
        timeout: Duration,
        call: impl FnOnce(Done<T>),
    ) -> Result<T, PageError> {
        blocking(timeout, call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Collected<T> = Arc<Mutex<Vec<Result<T, PageError>>>>;

    fn collect<T: Send + 'static>() -> (Collected<T>, Done<T>) {
        let results = Arc::new(Mutex::new(Vec::new()));
        let sink = results.clone();
        (
            results,
            Box::new(move |result| sink.lock().unwrap().push(result)),
        )
    }

    fn fence_that(live: bool) -> Fence {
        Arc::new(move |effect: &mut dyn FnMut()| {
            if live {
                effect();
                Ok(())
            } else {
                Err("owner revoked".to_owned())
            }
        })
    }

    #[test]
    fn a_live_fence_runs_the_effect_once_and_the_effect_completes_the_operation() {
        let (results, done) = collect::<u32>();
        let runs = Arc::new(Mutex::new(0));
        let counted = runs.clone();
        run_fenced(Some(&fence_that(true)), done, move |done| {
            *counted.lock().unwrap() += 1;
            done(Ok(7));
        });
        assert_eq!(*runs.lock().unwrap(), 1);
        assert_eq!(*results.lock().unwrap(), [Ok(7)]);
    }

    #[test]
    fn a_refusing_fence_never_runs_the_effect_and_completes_stale() {
        let (results, done) = collect::<u32>();
        let ran = Arc::new(Mutex::new(false));
        let flag = ran.clone();
        run_fenced(Some(&fence_that(false)), done, move |_| {
            *flag.lock().unwrap() = true
        });
        assert!(!*ran.lock().unwrap());
        assert_eq!(*results.lock().unwrap(), [Err(PageError::Stale)]);
    }

    #[test]
    fn a_fence_that_never_runs_the_effect_cannot_leave_the_caller_waiting() {
        let (results, done) = collect::<()>();
        let silent: Fence = Arc::new(|_: &mut dyn FnMut()| Ok::<(), String>(()));
        run_fenced(Some(&silent), done, |_| {
            unreachable!("the fence did not run the effect")
        });
        assert_eq!(*results.lock().unwrap(), [Err(PageError::Stale)]);
    }

    #[test]
    fn without_a_fence_the_effect_runs_directly() {
        let (results, done) = collect::<u32>();
        run_fenced(None, done, |done| done(Ok(1)));
        assert_eq!(*results.lock().unwrap(), [Ok(1)]);
    }

    #[test]
    fn blocking_returns_the_answer_of_another_thread_and_reports_timeout_and_drop() {
        let answer = blocking_unchecked(Duration::from_secs(5), |done: Done<u32>| {
            std::thread::spawn(move || done(Ok(42)));
        });
        assert_eq!(answer, Ok(42));
        let late = blocking_unchecked(Duration::from_millis(20), |done: Done<u32>| {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                done(Ok(1));
            });
        });
        assert_eq!(late, Err(PageError::Timeout));
        let dropped = blocking_unchecked(Duration::from_secs(5), |done: Done<u32>| drop(done));
        assert_eq!(dropped, Err(PageError::Cancelled));
    }

    #[test]
    fn page_ranges_follow_the_chromium_syntax() {
        assert_eq!(
            parse_page_ranges("").unwrap(),
            Vec::<std::ops::RangeInclusive<u32>>::new()
        );
        assert_eq!(
            parse_page_ranges("1-3, 5, 8-").unwrap(),
            [1..=3, 5..=5, 8..=u32::MAX]
        );
        assert_eq!(parse_page_ranges("-2").unwrap(), [1..=2]);
        assert!(parse_page_ranges("0").is_err());
        assert!(parse_page_ranges("3-1").is_err());
        assert!(parse_page_ranges("a").is_err());
        assert!(parse_page_ranges("1,,2").is_err());
    }

    #[test]
    fn specs_reject_values_the_platform_would_misread() {
        assert!(PdfSpec::default().validate().is_ok());
        assert!(
            PdfSpec {
                scale: 0.0,
                ..PdfSpec::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            PdfSpec {
                margins_in: [0.0, 0.0, -1.0, 0.0],
                ..PdfSpec::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            PdfSpec {
                paper_in: Some((8.27, 0.0)),
                ..PdfSpec::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            PdfSpec {
                page_ranges: Some("x".into()),
                ..PdfSpec::default()
            }
            .validate()
            .is_err()
        );
        assert!(CaptureSpec::default().validate().is_ok());
        assert!(
            CaptureSpec {
                scale: f64::NAN,
                ..CaptureSpec::default()
            }
            .validate()
            .is_err()
        );
        let empty = Rect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 10.0,
        };
        assert!(
            CaptureSpec {
                clip: Some(empty),
                ..CaptureSpec::default()
            }
            .validate()
            .is_err()
        );
    }
}
