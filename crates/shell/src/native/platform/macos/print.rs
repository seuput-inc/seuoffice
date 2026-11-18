//! `PrintOps` over `NSPrintOperation`: the system print dialog, and the file print job behind
//! `print_pdf`. WebKit's `createPDF` only renders one page, so PDFs come from a print job
//! that saves to a file. Both jobs run through `runOperationModalForWindow`: a job run
//! synchronously on the UI thread never finishes (WebKit's printing view waits for the main
//! run loop that the job itself blocks; reproduced with a plain WKWebView and `runOperation`).
//! The completion arrives on AppKit's print thread and is moved to the UI thread before
//! anything is touched.
//! A header and footer, which WebKit cannot print, are stamped onto the finished document
//! (`header_footer.rs`); the page ranges are applied last.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSPaperOrientation, NSPrintInfo, NSPrintJobSavingURL, NSPrintOperation, NSPrintSaveJob,
    NSPrintingPaginationMode, NSWindow,
};
use objc2_foundation::{NSCopying, NSObject, NSPoint, NSRect, NSSize, NSURL};
use objc2_web_kit::{WKPreferences, WKUserContentController, WKWebView};
use tauri::Webview;

use super::Platform;
use super::header_footer::{self, Plan};
use super::header_footer_view;
use super::page::{Stage, Surface, object, run};
use crate::native::page::{Done, HeaderFooter, PageError, PdfSpec, parse_page_ranges};
use crate::native::platform::{PrintOps, PrintOutcome};

/// Points per inch: `NSPrintInfo` measures paper and margins in points.
const POINTS: f64 = 72.0;

fn platform(message: &str) -> PageError {
    PageError::Platform(message.to_owned())
}

/// The print operation lays the page out inside its own view, which must have the size of a
/// page before the job runs; a web view without a size (hidden) falls back to the paper.
fn fit_view(operation: &NSPrintOperation, view: &WKWebView, paper: NSSize) {
    let bounds = view.bounds();
    let sized = bounds.size.width > 0.0 && bounds.size.height > 0.0;
    let size = if sized { bounds.size } else { paper };
    if let Some(page) = operation.view() {
        page.setFrame(NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size,
        });
    }
}

/// WebKit prints page backgrounds only when its (private) preference says so. The preference
/// is switched for the duration of one job and put back when this drops.
struct Backgrounds {
    preferences: Retained<WKPreferences>,
    previous: Option<bool>,
}

impl Backgrounds {
    fn set(surface: &Surface<'_>, wanted: bool) -> Result<Self, PageError> {
        // SAFETY: plain getters of the live web view on the UI thread.
        let preferences = unsafe { surface.view.configuration().preferences() };
        let target = object(&*preferences);
        // SAFETY: both selectors are checked before they are sent.
        unsafe {
            let reads: bool = msg_send![target, respondsToSelector: sel!(_shouldPrintBackgrounds)];
            let writes: bool =
                msg_send![target, respondsToSelector: sel!(_setShouldPrintBackgrounds:)];
            if !(reads && writes) {
                return if wanted {
                    Err(PageError::Unsupported("printBackground"))
                } else {
                    Ok(Self {
                        preferences,
                        previous: None,
                    })
                };
            }
            let previous: bool = msg_send![target, _shouldPrintBackgrounds];
            let _: () = msg_send![target, _setShouldPrintBackgrounds: wanted];
            Ok(Self {
                preferences,
                previous: Some(previous),
            })
        }
    }
}

impl Drop for Backgrounds {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            // SAFETY: `set` checked that the preferences answer to the selector.
            unsafe {
                let target = object(&*self.preferences);
                let _: () = msg_send![target, _setShouldPrintBackgrounds: previous];
            }
        }
    }
}

/// A print job that saves to `target`: paper, orientation, margins and scale of the spec,
/// no centering, automatic pagination.
fn saving_info(spec: &PdfSpec, target: &Path) -> Result<Retained<NSPrintInfo>, PageError> {
    let info = NSPrintInfo::new();
    // The orientation first: it swaps the paper size, which is then given as oriented.
    info.setOrientation(if spec.landscape {
        NSPaperOrientation::Landscape
    } else {
        NSPaperOrientation::Portrait
    });
    if let Some((width, height)) = spec.paper_in {
        let (width, height) = (width * POINTS, height * POINTS);
        info.setPaperSize(if spec.landscape {
            NSSize {
                width: height,
                height: width,
            }
        } else {
            NSSize { width, height }
        });
    }
    let [top, right, bottom, left] = spec.margins_in;
    info.setTopMargin(top * POINTS);
    info.setRightMargin(right * POINTS);
    info.setBottomMargin(bottom * POINTS);
    info.setLeftMargin(left * POINTS);
    info.setScalingFactor(spec.scale);
    info.setHorizontallyCentered(false);
    info.setVerticallyCentered(false);
    info.setHorizontalPagination(NSPrintingPaginationMode::Automatic);
    info.setVerticalPagination(NSPrintingPaginationMode::Automatic);
    let url = NSURL::from_file_path(target)
        .ok_or_else(|| platform("The print target is not a file path."))?;
    // SAFETY: the extern statics are AppKit constants; the dictionary takes an NSURL under
    // the saving-URL key, which is what a save job reads.
    unsafe {
        info.setJobDisposition(NSPrintSaveJob);
        let _: () = msg_send![&*info.dictionary(), setObject: &*url, forKey: NSPrintJobSavingURL];
    }
    Ok(info)
}

fn scratch_file() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let number = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "seuoffice-print-{}-{number}.pdf",
        std::process::id()
    ))
}

fn slicing(error: impl std::fmt::Display) -> PageError {
    PageError::Platform(format!("The PDF pages could not be selected: {error}"))
}

/// Keep only the pages the ranges name. No ranges keep every page.
fn slice_pages(bytes: Vec<u8>, ranges: &[RangeInclusive<u32>]) -> Result<Vec<u8>, PageError> {
    if ranges.is_empty() {
        return Ok(bytes);
    }
    let mut document = lopdf::Document::load_mem(&bytes).map_err(slicing)?;
    let total = u32::try_from(document.get_pages().len()).unwrap_or(u32::MAX);
    let dropped: Vec<u32> = (1..=total)
        .filter(|page| !ranges.iter().any(|range| range.contains(page)))
        .collect();
    if dropped.len() == usize::try_from(total).unwrap_or(usize::MAX) {
        return Err(PageError::Invalid(
            "the page range selects no page".to_owned(),
        ));
    }
    if dropped.is_empty() {
        return Ok(bytes);
    }
    document.delete_pages(&dropped);
    document.prune_objects();
    let mut sliced = Vec::new();
    document.save_to(&mut sliced).map_err(slicing)?;
    Ok(sliced)
}

/// Stamp the templates onto the printed PDF `bytes` and keep the pages `ranges` names. The
/// strips come from a hidden web view, so the answer arrives later on the UI thread.
fn with_templates(
    surface: &Surface<'_>,
    spec: &PdfSpec,
    templates: &HeaderFooter,
    bytes: Vec<u8>,
    ranges: Vec<RangeInclusive<u32>>,
    done: Done<Vec<u8>>,
) {
    let mut document = match lopdf::Document::load_mem(&bytes) {
        Ok(document) => document,
        Err(error) => return done(Err(header_footer::failure(error))),
    };
    let context = header_footer_view::context(surface);
    let planned = header_footer::plan(&document, spec.margins_in, templates, &ranges, &context);
    let Plan { jobs, stamps } = match planned {
        Ok(plan) => plan,
        Err(error) => return done(Err(error)),
    };
    if jobs.is_empty() {
        return done(slice_pages(bytes, &ranges));
    }
    header_footer_view::render(
        surface.mtm,
        jobs,
        Box::new(move |rendered| {
            let stamped = rendered.and_then(|strips| {
                header_footer::apply(&mut document, &stamps, &strips)?;
                let mut out = Vec::new();
                document.save_to(&mut out).map_err(header_footer::failure)?;
                Ok(out)
            });
            done(stamped.and_then(|out| slice_pages(out, &ranges)));
        }),
    );
}

/// A file print job in flight. Everything it borrowed from the web view is held here, so the
/// job outlives the UI-thread call that started it and finishes when AppKit reports back.
struct SaveJob {
    mtm: MainThreadMarker,
    view: Retained<WKWebView>,
    window: Retained<NSWindow>,
    controller: Retained<WKUserContentController>,
    spec: PdfSpec,
    ranges: Vec<RangeInclusive<u32>>,
    target: PathBuf,
    stage: Stage,
    backgrounds: Backgrounds,
    done: Done<Vec<u8>>,
}

impl SaveJob {
    /// Read the scratch file back and remove it, put the view back as it was, then stamp the
    /// templates and keep the ranges.
    fn finish(self, printed: bool) {
        let Self {
            mtm,
            view,
            window,
            controller,
            spec,
            ranges,
            target,
            stage,
            backgrounds,
            done,
        } = self;
        let bytes = if printed {
            std::fs::read(&target).map_err(|error| {
                PageError::Platform(format!("The printed PDF could not be read: {error}"))
            })
        } else {
            Err(platform("The print operation did not complete."))
        };
        let _ = std::fs::remove_file(&target);
        drop(backgrounds);
        drop(stage);
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(error) => return done(Err(error)),
        };
        let surface = Surface {
            mtm,
            view: &view,
            window: Some(&window),
            controller: &controller,
        };
        match &spec.header_footer {
            Some(templates) => with_templates(&surface, &spec, templates, bytes, ranges, done),
            None => done(slice_pages(bytes, &ranges)),
        }
    }
}

/// The PDF of a print job to a scratch file, read back and removed. Tagged output and
/// outlines have no WebKit equivalent and are refused rather than dropped. The job needs the
/// web view's window on screen, so a hidden one is shown invisibly until the job ends.
pub(super) fn save_pdf(surface: &Surface<'_>, spec: PdfSpec, done: Done<Vec<u8>>) {
    let refused = if spec.tagged {
        Some("generateTaggedPDF")
    } else if spec.outline {
        Some("generateDocumentOutline")
    } else {
        None
    };
    if let Some(option) = refused {
        return done(Err(PageError::Unsupported(option)));
    }
    let ranges = match spec
        .page_ranges
        .as_deref()
        .map(parse_page_ranges)
        .transpose()
    {
        Ok(ranges) => ranges.unwrap_or_default(),
        Err(error) => return done(Err(error)),
    };
    let Some(window) = surface.window else {
        return done(Err(platform("The web view is not in a window.")));
    };
    let mut stage = Stage::new(surface);
    stage.reveal();
    let backgrounds = match Backgrounds::set(surface, spec.print_background) {
        Ok(backgrounds) => backgrounds,
        Err(error) => return done(Err(error)),
    };
    let target = scratch_file();
    let info = match saving_info(&spec, &target) {
        Ok(info) => info,
        Err(error) => return done(Err(error)),
    };
    // SAFETY: the info is a complete save-job description; the operation is created and run
    // on the UI thread.
    let operation = unsafe { surface.view.printOperationWithPrintInfo(&info) };
    operation.setShowsPrintPanel(false);
    operation.setShowsProgressPanel(false);
    fit_view(&operation, surface.view, info.paperSize());
    let job = SaveJob {
        mtm: surface.mtm,
        view: surface.view.retain(),
        window: window.retain(),
        controller: surface.controller.retain(),
        spec,
        ranges,
        target,
        stage,
        backgrounds,
        done,
    };
    run_modal(
        surface.mtm,
        &operation,
        window,
        Box::new(move |printed| job.finish(printed)),
    );
}

/// What runs on the UI thread once a print job reports back, with whether it went through
type Finish = Box<dyn FnOnce(bool)>;

struct Answer {
    finish: RefCell<Option<Finish>>,
    /// written on AppKit's print thread, read on the UI thread after the hop
    success: Cell<bool>,
}

define_class!(
    /// A print operation reports back to this object; it keeps itself alive until then.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SeuOfficePrintAnswer"]
    #[ivars = Answer]
    struct PrintAnswer;

    impl PrintAnswer {
        /// AppKit calls this on the thread that ran the job (observed: not the main thread),
        /// where windows, web views and the caller's `Done` must not be touched: the answer
        /// is passed on to the UI thread.
        #[unsafe(method(printOperationDidRun:success:contextInfo:))]
        fn did_run(&self, _operation: &NSPrintOperation, success: Bool, _context: *mut c_void) {
            self.ivars().success.set(success.as_bool());
            // SAFETY: `finishOnMain` is defined below with no arguments; the receiver stays
            // retained until the selector has run.
            unsafe {
                let _: () = msg_send![
                    self,
                    performSelectorOnMainThread: sel!(finishOnMain),
                    withObject: None::<&AnyObject>,
                    waitUntilDone: Bool::NO
                ];
            }
        }

        #[unsafe(method(finishOnMain))]
        fn finish_on_main(&self) {
            let finish = self.ivars().finish.borrow_mut().take();
            if let Some(finish) = finish {
                finish(self.ivars().success.get());
            }
            // SAFETY: balances the retain `run_modal` leaked so the object lives until now;
            // nothing touches `self` afterwards.
            drop(unsafe { Retained::from_raw((self as *const Self).cast_mut()) });
        }
    }
);

impl PrintAnswer {
    fn new(mtm: MainThreadMarker, finish: Finish) -> Retained<Self> {
        let answer = Answer {
            finish: RefCell::new(Some(finish)),
            success: Cell::new(false),
        };
        let this = mtm.alloc::<Self>().set_ivars(answer);
        // SAFETY: NSObject's plain `init`.
        unsafe { msg_send![super(this), init] }
    }
}

/// Run `operation` for `window` without holding the UI thread; `finish` runs on the UI thread
/// with whether the job went through.
fn run_modal(
    mtm: MainThreadMarker,
    operation: &NSPrintOperation,
    window: &NSWindow,
    finish: Finish,
) {
    let answer = PrintAnswer::new(mtm, finish);
    // One retain is leaked on purpose: AppKit does not retain the delegate, and `finish_on_main`
    // releases it.
    let _ = Retained::into_raw(answer.clone());
    // SAFETY: the selector is implemented by `PrintAnswer` with the signature AppKit calls
    // (operation, success, context).
    unsafe {
        operation.runOperationModalForWindow_delegate_didRunSelector_contextInfo(
            window,
            Some(object(&*answer)),
            Some(sel!(printOperationDidRun:success:contextInfo:)),
            std::ptr::null_mut(),
        );
    }
}

impl PrintOps for Platform {
    fn print_dialog(webview: &Webview, done: Done<PrintOutcome>) {
        run(webview, done, |surface, done| {
            let Some(window) = surface.window else {
                return done(Err(platform("The web view is not in a window.")));
            };
            let info = NSPrintInfo::sharedPrintInfo().copy();
            // SAFETY: the copy of the shared info is a complete job description.
            let operation = unsafe { surface.view.printOperationWithPrintInfo(&info) };
            operation.setShowsPrintPanel(true);
            operation.setShowsProgressPanel(true);
            fit_view(&operation, surface.view, info.paperSize());
            run_modal(
                surface.mtm,
                &operation,
                window,
                Box::new(move |success| {
                    done(Ok(if success {
                        PrintOutcome::Printed
                    } else {
                        PrintOutcome::Cancelled
                    }));
                }),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_ranges_keep_the_document_as_it_is() {
        let bytes = b"%PDF-1.4 not parsed when every page is kept".to_vec();
        assert_eq!(slice_pages(bytes.clone(), &[]).unwrap(), bytes);
    }

    #[test]
    fn scratch_files_are_unique_per_job() {
        assert_ne!(scratch_file(), scratch_file());
    }
}
