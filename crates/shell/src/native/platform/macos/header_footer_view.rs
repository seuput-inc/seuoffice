//! The hidden WebKit view that renders the wrapper pages of `header_footer.rs` to PDF strips.
//!
//! One view serves every job of a print, one job at a time: load the wrapper page, then
//! `createPDF` the header rectangle and the footer rectangle of it. Everything runs on the UI
//! thread and nothing waits: each WebKit completion starts the next step, and the last one
//! hands the strips to the caller's closure. The view has its own non-persistent data store,
//! runs no page script and loads no resource (the wrapper's content security policy), so a
//! template can only paint.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Display;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_foundation::{
    NSData, NSError, NSLocale, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use objc2_web_kit::{
    WKNavigation, WKNavigationDelegate, WKPDFConfiguration, WKWebView, WKWebViewConfiguration,
    WKWebsiteDataStore,
};

use super::header_footer::{Context, Job, Strips};
use super::page::{Surface, object};
use crate::native::page::PageError;

fn rendering(what: impl Display) -> PageError {
    PageError::Platform(format!("The header and footer could not be rendered: {what}"))
}

/// What the templates' `date`, `title` and `url` classes show: the page's own title and
/// address, and now.
pub(super) fn context(surface: &Surface<'_>) -> Context {
    // SAFETY: plain getters of the live web view on the UI thread.
    let (title, url) = unsafe { (surface.view.title(), surface.view.URL()) };
    Context {
        date: short_date_and_time(),
        title: title.map(|title| title.to_string()).unwrap_or_default(),
        url: url
            .and_then(|url| url.absoluteString())
            .map(|address| address.to_string())
            .unwrap_or_default(),
    }
}

/// The short date and short time of the user's first language, as Chromium formats `date`
/// (`Intl.DateTimeFormat` with `dateStyle` and `timeStyle` short).
fn short_date_and_time() -> String {
    let languages = NSLocale::preferredLanguages();
    let locale = match languages.firstObject() {
        Some(language) => NSLocale::localeWithLocaleIdentifier(&language),
        None => NSLocale::currentLocale(),
    };
    let (Some(formatter_class), Some(date_class)) =
        (AnyClass::get(c"NSDateFormatter"), AnyClass::get(c"NSDate"))
    else {
        return String::new();
    };
    // NSDateFormatterShortStyle.
    let short: usize = 1;
    // SAFETY: Foundation's own classes and selectors with their documented signatures.
    // `NSDateFormatter` is addressed by name because objc2-foundation's feature for it is not
    // enabled in this crate.
    unsafe {
        let formatter: Retained<AnyObject> = msg_send![formatter_class, new];
        let _: () = msg_send![&*formatter, setDateStyle: short];
        let _: () = msg_send![&*formatter, setTimeStyle: short];
        let _: () = msg_send![&*formatter, setLocale: &*locale];
        let now: Retained<AnyObject> = msg_send![date_class, date];
        let text: Option<Retained<NSString>> = msg_send![&*formatter, stringFromDate: &*now];
        text.map(|text| text.to_string()).unwrap_or_default()
    }
}

type Settled = Box<dyn FnOnce(Result<(), String>)>;

struct Watch {
    waiting: RefCell<Option<Settled>>,
}

define_class!(
    /// Tells the render how the load in flight ended. The web view holds its navigation
    /// delegate weakly, so the render keeps this object.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SeuOfficeHeaderFooterLoad"]
    #[ivars = Watch]
    struct LoadWatch;

    unsafe impl NSObjectProtocol for LoadWatch {}

    unsafe impl WKNavigationDelegate for LoadWatch {
        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, _view: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.settle(Ok(()));
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(
            &self,
            _view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.settle(Err(error.localizedDescription().to_string()));
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            _view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.settle(Err(error.localizedDescription().to_string()));
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn did_terminate(&self, _view: &WKWebView) {
            self.settle(Err("the web content process ended".to_owned()));
        }
    }
);

impl LoadWatch {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(Watch { waiting: RefCell::new(None) });
        // SAFETY: NSObject's plain `init`.
        unsafe { msg_send![super(this), init] }
    }

    fn expect(&self, settled: Settled) {
        *self.ivars().waiting.borrow_mut() = Some(settled);
    }

    fn settle(&self, outcome: Result<(), String>) {
        // The borrow ends before the closure runs: it starts the next load and calls `expect`.
        let waiting = self.ivars().waiting.borrow_mut().take();
        if let Some(settled) = waiting {
            settled(outcome);
        }
    }
}

type Finished = Box<dyn FnOnce(Result<Vec<Strips>, PageError>)>;

struct Run {
    mtm: MainThreadMarker,
    view: Retained<WKWebView>,
    watch: Retained<LoadWatch>,
    jobs: VecDeque<Job>,
    strips: Vec<Strips>,
    done: Option<Finished>,
}

type Shared = Rc<RefCell<Run>>;

fn open(mtm: MainThreadMarker) -> Result<(Retained<WKWebView>, Retained<LoadWatch>), PageError> {
    let nothing = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize { width: 0.0, height: 0.0 },
    };
    // SAFETY: a configuration and a web view of their own on the UI thread, sharing nothing
    // with the page that is being printed. Every optional selector is checked before it is
    // sent.
    unsafe {
        let configuration = WKWebViewConfiguration::new(mtm);
        configuration.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));
        configuration.defaultWebpagePreferences().setAllowsContentJavaScript(false);
        let view = WKWebView::initWithFrame_configuration(mtm.alloc(), nothing, &configuration);
        let target = object(&*view);
        let create = sel!(createPDFWithConfiguration:completionHandler:);
        let can_make_pdf: Bool = msg_send![target, respondsToSelector: create];
        if !can_make_pdf.as_bool() {
            return Err(rendering("this macOS cannot make a PDF of a web page (macOS 12 or later)"));
        }
        let draws: Bool = msg_send![target, respondsToSelector: sel!(_setDrawsBackground:)];
        if draws.as_bool() {
            let _: () = msg_send![target, _setDrawsBackground: false];
        }
        let watch = LoadWatch::new(mtm);
        let _: () = msg_send![target, setNavigationDelegate: &*watch];
        Ok((view, watch))
    }
}

/// Render `jobs` one after another; `done` gets one [`Strips`] per job, in order, or the
/// first failure.
pub(super) fn render(mtm: MainThreadMarker, jobs: Vec<Job>, done: Finished) {
    match open(mtm) {
        Err(error) => done(Err(error)),
        Ok((view, watch)) => {
            let run = Run {
                mtm,
                view,
                watch,
                jobs: jobs.into(),
                strips: Vec::new(),
                done: Some(done),
            };
            next(Rc::new(RefCell::new(run)));
        }
    }
}

fn finish(run: &Shared, outcome: Result<(), PageError>) {
    let (done, strips) = {
        let mut state = run.borrow_mut();
        (state.done.take(), std::mem::take(&mut state.strips))
    };
    if let Some(done) = done {
        done(outcome.map(|()| strips));
    }
}

/// Load the next job's wrapper page, or finish when none is left.
fn next(run: Shared) {
    let job = run.borrow_mut().jobs.pop_front();
    let Some(job) = job else { return finish(&run, Ok(())) };
    let (view, watch) = {
        let state = run.borrow();
        (state.view.clone(), state.watch.clone())
    };
    let html = NSString::from_str(&job.html);
    let size = NSSize { width: job.canvas.width, height: job.canvas.height };
    let resume = run.clone();
    watch.expect(Box::new(move |loaded| match loaded {
        Ok(()) => strips(resume, job),
        Err(message) => finish(&resume, Err(rendering(format!("a page did not load: {message}")))),
    }));
    // The layout width is the page's, so the web view has the page's size while it loads.
    view.setFrame(NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size });
    // SAFETY: a plain string load on the UI thread.
    if unsafe { view.loadHTMLString_baseURL(&html, None) }.is_none() {
        watch.settle(Err("WebKit did not start the load".to_owned()));
    }
}

type Rect = (f64, f64, f64, f64);

type Made = Box<dyn FnOnce(Shared, Result<Option<Vec<u8>>, PageError>)>;

/// The PDF of one part of the loaded page: `None` for no part.
fn pdf(run: Shared, rect: Option<Rect>, then: Made) {
    let Some((x, y, width, height)) = rect else { return then(run, Ok(None)) };
    let (view, mtm) = {
        let state = run.borrow();
        (state.view.clone(), state.mtm)
    };
    // SAFETY: a plain configuration object on the UI thread; the optional selector is checked
    // before it is sent.
    let configuration = unsafe {
        let configuration = WKPDFConfiguration::new(mtm);
        configuration
            .setRect(NSRect { origin: NSPoint { x, y }, size: NSSize { width, height } });
        let transparent: Bool = msg_send![
            object(&*configuration),
            respondsToSelector: sel!(setAllowTransparentBackground:)
        ];
        if transparent.as_bool() {
            configuration.setAllowTransparentBackground(true);
        }
        configuration
    };
    let state = RefCell::new(Some((run, then)));
    let block: RcBlock<dyn Fn(*mut NSData, *mut NSError)> =
        RcBlock::new(move |data: *mut NSData, error: *mut NSError| {
            let Some((run, then)) = state.borrow_mut().take() else { return };
            // SAFETY: WebKit passes null or live objects that outlive this handler.
            let outcome = match (unsafe { data.as_ref() }, unsafe { error.as_ref() }) {
                (Some(data), _) => Ok(Some(data.to_vec())),
                (None, Some(error)) => Err(rendering(error.localizedDescription())),
                (None, None) => Err(rendering("WebKit returned no PDF")),
            };
            then(run, outcome);
        });
    // SAFETY: the block matches the completion handler type.
    unsafe { view.createPDFWithConfiguration_completionHandler(Some(&*configuration), &block) };
}

/// Take the header and then the footer of the page that has just loaded, then go on.
fn strips(run: Shared, job: Job) {
    let header = job.header.then(|| job.canvas.header_rect());
    let footer = job.footer.then(|| job.canvas.footer_rect());
    let after_header: Made = Box::new(move |run, header| match header {
        Err(error) => finish(&run, Err(error)),
        Ok(header) => {
            let after_footer: Made = Box::new(move |run, footer| match footer {
                Err(error) => finish(&run, Err(error)),
                Ok(footer) => {
                    run.borrow_mut().strips.push(Strips { header, footer });
                    next(run);
                }
            });
            pdf(run, footer, after_footer);
        }
    });
    pdf(run, header, after_header);
}
