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

use lopdf::Object;
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
use super::page::{Stage, Surface, call_async, object, run};
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

/// A print job that saves to `target`: paper, orientation and margins of the spec (and its
/// scale, see below), no centering, automatic pagination.
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
    // A scale is not WebKit's own: it would shrink the content without giving the pages more
    // room. The job prints unscaled on paper (and margins) `1 / scale` as large and
    // `scale_pages` shrinks the result.
    let grow = 1.0 / spec.scale;
    if grow != 1.0 {
        let paper = info.paperSize();
        info.setPaperSize(NSSize {
            width: paper.width * grow,
            height: paper.height * grow,
        });
    }
    let [top, right, bottom, left] = spec.margins_in;
    info.setTopMargin(top * POINTS * grow);
    info.setRightMargin(right * POINTS * grow);
    info.setBottomMargin(bottom * POINTS * grow);
    info.setLeftMargin(left * POINTS * grow);
    info.setScalingFactor(1.0);
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

fn scaling(error: impl std::fmt::Display) -> PageError {
    PageError::Platform(format!("The PDF pages could not be scaled: {error}"))
}

/// Scale every page of the printed PDF by `factor`: its box and its content. The job itself
/// runs unscaled on paper `1 / factor` as large (see `saving_info`), so that the page breaks
/// fall as they do in Chromium: WebKit's own scaling shrinks the content without giving the
/// pages more room, and two forced pages end up on one sheet.
fn scale_pages(bytes: Vec<u8>, factor: f64) -> Result<Vec<u8>, PageError> {
    if (factor - 1.0).abs() < 1e-9 {
        return Ok(bytes);
    }
    let mut document = lopdf::Document::load_mem(&bytes).map_err(scaling)?;
    for (_, page) in document.get_pages() {
        let [x, y, width, height] = header_footer::media_box(&document, page)
            .map_err(|_| scaling("a page has no valid MediaBox"))?;
        let bodies = document.get_page_contents(page);
        let open = document.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            format!("q\n{factor} 0 0 {factor} 0 0 cm\n").into_bytes(),
        ));
        let close = document.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            b"Q\n".to_vec(),
        ));
        let mut contents = vec![Object::Reference(open)];
        contents.extend(bodies.into_iter().map(Object::Reference));
        contents.push(Object::Reference(close));
        let dictionary = document.get_dictionary_mut(page).map_err(scaling)?;
        dictionary.set("Contents", contents);
        dictionary.set(
            "MediaBox",
            vec![
                Object::Real((x * factor) as f32),
                Object::Real((y * factor) as f32),
                Object::Real(((x + width) * factor) as f32),
                Object::Real(((y + height) * factor) as f32),
            ],
        );
        for key in [&b"CropBox"[..], b"BleedBox", b"TrimBox", b"ArtBox"] {
            dictionary.remove(key);
        }
    }
    let mut scaled = Vec::new();
    document.save_to(&mut scaled).map_err(scaling)?;
    Ok(scaled)
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
        let bytes = bytes.and_then(|bytes| scale_pages(bytes, spec.scale));
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
    if !spec.prefer_css_page_size {
        return begin_job(surface, spec, ranges, done);
    }
    // WebKit applies the `@page` margins of a page but takes its paper from the job, so the
    // size `@page` names is read from the page's style sheets first and becomes the paper.
    let (mtm, view, window, controller) = (
        surface.mtm,
        surface.view.retain(),
        window.retain(),
        surface.controller.retain(),
    );
    call_async(surface, CSS_PAGE_SIZE, move |outcome| {
        let surface = Surface {
            mtm,
            view: &view,
            window: Some(&*window),
            controller: &controller,
        };
        match outcome {
            Ok(answer) => {
                let mut spec = spec;
                let size = answer.and_then(|text| serde_json::from_str::<(f64, f64)>(&text).ok());
                if let Some((width, height)) = size {
                    spec.landscape = width > height;
                    spec.paper_in = Some((width.min(height) / POINTS, width.max(height) / POINTS));
                }
                begin_job(&surface, spec, ranges, done)
            }
            Err(error) => done(Err(error)),
        }
    });
}

/// The size of the first page as the page's `@page` rules give it, in points: `[width, height]`,
/// or nothing when no rule names a size (the job's own paper stays). `@page :first` beats `@page`.
const CSS_PAGE_SIZE: &str = r#"
const MM = 72 / 25.4;
const UNIT = { mm: MM, cm: 10 * MM, q: MM / 4, in: 72, pt: 1, pc: 12, px: 0.75 };
const NAMED = {
  a5: [148 * MM, 210 * MM], a4: [210 * MM, 297 * MM], a3: [297 * MM, 420 * MM],
  b5: [176 * MM, 250 * MM], b4: [250 * MM, 353 * MM],
  letter: [612, 792], legal: [612, 1008], ledger: [1224, 792],
};
const parse = (text) => {
  let size = null;
  let landscape = null;
  const lengths = [];
  for (const word of text.trim().toLowerCase().split(/\s+/)) {
    const length = /^(\d*\.?\d+)(mm|cm|q|in|pt|pc|px)$/.exec(word);
    if (word === 'landscape') landscape = true;
    else if (word === 'portrait') landscape = false;
    else if (NAMED[word]) size = NAMED[word];
    else if (length) lengths.push(Number(length[1]) * UNIT[length[2]]);
    else if (word !== 'auto') return null;
  }
  if (lengths.length === 1) return [lengths[0], lengths[0]];
  if (lengths.length === 2) return lengths;
  if (!size) return null;
  let [width, height] = size;
  if (landscape !== null && (width > height) !== landscape) [width, height] = [height, width];
  return [width, height];
};
let plain = null;
let first = null;
const visit = (rules) => {
  for (const rule of rules) {
    if (typeof CSSPageRule !== 'undefined' && rule instanceof CSSPageRule) {
      const text = rule.style.getPropertyValue('size') ||
        ((/(?:^|[;{\s])size\s*:\s*([^;}]+)/.exec(rule.cssText) || [])[1] || '');
      const size = text ? parse(text) : null;
      if (!size) continue;
      const selector = rule.selectorText.trim();
      if (selector === '') plain = size;
      else if (selector === ':first') first = size;
    } else if (rule.cssRules &&
        (!rule.media || rule.media.length === 0 || /print|all/i.test(rule.media.mediaText))) {
      visit(rule.cssRules);
    }
  }
};
for (const sheet of document.styleSheets) {
  try { visit(sheet.cssRules); } catch (_) {}
}
const found = first || plain;
return found ? JSON.stringify(found) : undefined;
"#;

/// Create the print operation of `spec` and run it; the answer reaches `done` from the UI
/// thread when the job ends.
fn begin_job(
    surface: &Surface<'_>,
    spec: PdfSpec,
    ranges: Vec<RangeInclusive<u32>>,
    done: Done<Vec<u8>>,
) {
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

    fn one_page_pdf() -> Vec<u8> {
        let mut document = lopdf::Document::with_version("1.5");
        let tree_id = document.new_object_id();
        let content = document.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            b"0 0 100 50 re f".to_vec(),
        ));
        let size = || {
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(600),
                Object::Integer(800),
            ])
        };
        let page = document.add_object(lopdf::Dictionary::from_iter([
            ("Type", Object::Name(b"Page".to_vec())),
            ("Parent", Object::Reference(tree_id)),
            ("MediaBox", size()),
            ("CropBox", size()),
            ("Contents", Object::Reference(content)),
        ]));
        document.objects.insert(
            tree_id,
            Object::Dictionary(lopdf::Dictionary::from_iter([
                ("Type", Object::Name(b"Pages".to_vec())),
                ("Count", Object::Integer(1)),
                ("Kids", Object::Array(vec![Object::Reference(page)])),
            ])),
        );
        let catalog = document.add_object(lopdf::Dictionary::from_iter([
            ("Type", Object::Name(b"Catalog".to_vec())),
            ("Pages", Object::Reference(tree_id)),
        ]));
        document.trailer.set("Root", Object::Reference(catalog));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn scaled_pages_shrink_their_box_and_their_content_together() {
        let scaled = scale_pages(one_page_pdf(), 0.5).unwrap();
        let document = lopdf::Document::load_mem(&scaled).unwrap();
        let (_, page) = document.get_pages().into_iter().next().unwrap();
        assert_eq!(
            header_footer::media_box(&document, page).unwrap(),
            [0.0, 0.0, 300.0, 400.0]
        );
        assert!(
            document
                .get_dictionary(page)
                .unwrap()
                .get(b"CropBox")
                .is_err()
        );
        let content = String::from_utf8(document.get_page_content(page).unwrap()).unwrap();
        assert!(content.starts_with("q\n0.5 0 0 0.5 0 0 cm\n"), "{content}");
        assert!(content.contains("0 0 100 50 re f"));
        assert!(content.trim_end().ends_with('Q'));
    }
}
