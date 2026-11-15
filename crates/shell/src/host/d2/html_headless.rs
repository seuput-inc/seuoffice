use std::sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, Ordering}};
use std::time::Duration;

use main_core::packages::electron_utils::headless_export::HeadlessExportRequest;
use main_core::shell::headless_export::HeadlessExportProduct;
use shell_contract::{Surface, ViewId, WindowId, ipc::{NativeInvocationOperation, NativeInvocationSelection, NativeInvocationTarget}, provided::Provided, runtime::{CancelSubscription, RendererGeneration, Views}};
use shell_ipc_d2::{html::HtmlEditors, markdown::MarkdownEditors};
use tauri_runtime_cef::{FrameEventKind, NativeBrowserCloseSubscription, NativeContextToken, WebviewCefExt};

use crate::host::{HostContext, window::{Bridge, ViewSpec, WindowOptions, WindowSpec}};

pub(super) type Done = Box<dyn FnOnce(Result<Option<HeadlessExportProduct>, String>) + Send>;
const TIMEOUT: Duration = Duration::from_millis(180_000);

#[derive(Clone, Copy)]
pub(super) enum TextExport { HtmlPdf, HtmlDocx, MarkdownPdf }

impl TextExport {
    fn app(self) -> &'static str {
        match self { Self::HtmlPdf | Self::HtmlDocx => "html", Self::MarkdownPdf => "markdown" }
    }
    fn surface(self) -> Surface {
        match self { Self::HtmlPdf | Self::HtmlDocx => Surface::Html, Self::MarkdownPdf => Surface::Markdown }
    }
    fn url(self) -> &'static str {
        match self { Self::HtmlPdf | Self::HtmlDocx => "genoffice-app://html/index.html", Self::MarkdownPdf => "genoffice-app://markdown/index.html" }
    }
    fn available(self, provided: &Provided) -> bool {
        match self {
            Self::HtmlPdf | Self::HtmlDocx => !provided.all::<HtmlEditors>().is_empty(),
            Self::MarkdownPdf => !provided.all::<MarkdownEditors>().is_empty(),
        }
    }
    fn begin(self, provided: &Provided, view: ViewId, input: &str, out: &str, invocation: NativeInvocationTarget, done: Box<dyn FnOnce(Result<(), String>)>) -> Result<(), String> {
        match self {
            Self::HtmlPdf | Self::HtmlDocx => {
                let editor = provided.all::<HtmlEditors>().into_iter().next().ok_or("the HTML headless editor is unavailable")?;
                let format = match self { Self::HtmlDocx => "docx", _ => "pdf" };
                editor.export_headless_invocation(view, input, out, format, TIMEOUT, invocation, done);
            }
            Self::MarkdownPdf => {
                let editor = provided.all::<MarkdownEditors>().into_iter().next().ok_or("the Markdown headless editor is unavailable")?;
                editor.export_pdf_headless_invocation(view, input, out, TIMEOUT, invocation, done);
            }
        }
        Ok(())
    }
    fn fail(self, provided: &Provided, view: ViewId, error: String) -> bool {
        match self {
            Self::HtmlPdf | Self::HtmlDocx => provided.all::<HtmlEditors>().into_iter().next().is_some_and(|editor| editor.fail_headless_export(view, error)),
            Self::MarkdownPdf => provided.all::<MarkdownEditors>().into_iter().next().is_some_and(|editor| editor.fail_headless_export(view, error)),
        }
    }
}

struct Created {
    window: WindowId,
    view: ViewId,
    generation: RendererGeneration,
    webview: tauri::Webview,
}

struct Export {
    cx: HostContext,
    kind: TextExport,
    operation: NativeInvocationOperation,
    target: OnceLock<NativeInvocationTarget>,
    cancellation: Mutex<Vec<CancelSubscription>>,
    created: OnceLock<Created>,
    creation_finished: AtomicBool,
    loading: AtomicBool,
    browser: OnceLock<i32>,
    registered: Mutex<Option<NativeContextToken>>,
    closing: Mutex<Option<NativeBrowserCloseSubscription>>,
    closed: AtomicBool,
    settled: AtomicBool,
    result: Mutex<Option<Result<(), String>>>,
    done: Mutex<Option<Done>>,
}

impl Export {
    fn fail(self: &Arc<Self>, error: String) {
        let job = self.clone();
        let unavailable = self.clone();
        let unavailable_error = error.clone();
        crate::host::w::post_core_checked(move |provided, _| {
            if let Some(created) = job.created.get() {
                if job.kind.fail(provided, created.view, error.clone()) { return; }
            }
            job.finish(Err(error));
        }, move || unavailable.finish(Err(format!("{unavailable_error}; headless core services are unavailable"))));
    }

    fn finish(self: &Arc<Self>, mut result: Result<(), String>) {
        if self.settled.swap(true, Ordering::AcqRel) { return; }
        if result.is_ok() {
            result = self.target.get().ok_or_else(|| "the hidden text target was not reserved".to_owned())
                .and_then(|target| {
                    let document = target.registered_document()?;
                    let registered = self.registered.lock().unwrap_or_else(|error| error.into_inner()).clone();
                    if !registered.as_ref().is_some_and(|origin| origin.is_verified() && origin.is_live()
                        && self.browser.get() == Some(&origin.browser_id())) {
                        return Err(format!("the {} exporter has no matching registered native document", self.kind.app()));
                    }
                    crate::ipc::native_owner::with_native_invocation_effect(
                        target, document.owner(), self.operation.cancel(), self.operation.shutdown(), || (),
                    ).ok_or_else(|| format!("the {} export authority was revoked", self.kind.app()))
                });
        }
        *self.result.lock().unwrap_or_else(|error| error.into_inner()) = Some(result);
        self.close_created();
    }

    fn close_created(self: &Arc<Self>) {
        let Some(created) = self.created.get() else {
            if self.creation_finished.load(Ordering::Acquire) { self.complete(None); }
            return;
        };
        if self.closed.load(Ordering::Acquire) { self.complete(None); return; }
        let window = created.webview.window();
        let job = self.clone();
        self.cx.ui.post(Box::new(move || {
            if let Err(error) = window.destroy() {
                let error = error.to_string();
                let unavailable = job.clone();
                let unavailable_error = error.clone();
                crate::host::w::post_core_checked(move |_, _| job.complete(Some(error)),
                    move || unavailable.complete(Some(format!("{unavailable_error}; headless core services are unavailable"))));
            }
        }));
    }

    fn complete(&self, cleanup_error: Option<String>) {
        let Some(mut result) = self.result.lock().unwrap_or_else(|error| error.into_inner()).take() else { return; };
        if let Some(error) = cleanup_error {
            result = Err(match result {
                Ok(()) => format!("the {} export completed, but its hidden browser did not close: {error}", self.kind.app()),
                Err(first) => format!("{first}; hidden {} browser cleanup failed: {error}", self.kind.app()),
            });
        }
        let subscription = self.closing.lock().unwrap_or_else(|error| error.into_inner()).take();
        drop(subscription);
        if self.closed.load(Ordering::Acquire) {
            if let Some(created) = self.created.get() { self.cx.views.remove_window(created.window); }
        }
        let done = self.done.lock().unwrap_or_else(|error| error.into_inner()).take();
        if let Some(done) = done { done(result.map(|()| None)); }
    }
}

pub(crate) fn export_headless(
    cx: &HostContext, input: &str, out: &str, format: &str, request: &HeadlessExportRequest, operation: &NativeInvocationOperation, done: Done,
) -> Result<(), String> {
    let kind = match format {
        "pdf" => TextExport::HtmlPdf,
        "docx" => TextExport::HtmlDocx,
        _ => return Err(format!("unsupported HTML export format: {format}")),
    };
    export_text(cx, input, out, format, request, operation, kind, done)
}

pub(super) fn export_text(cx: &HostContext, input: &str, out: &str, format: &str, request: &HeadlessExportRequest, operation: &NativeInvocationOperation, kind: TextExport, done: Done) -> Result<(), String> {
    match operation.selection() {
        NativeInvocationSelection::HeadlessExport { surface, input: selected_input, output, format: selected_format, page, ops, batch }
            if *surface == kind.surface() && selected_input == std::path::Path::new(input)
                && output == std::path::Path::new(out) && selected_format == format
                && *page == request.page && *ops == request.ops && batch.as_deref() == request.batch => {}
        _ => return Err("the headless invocation does not match the requested text export".into()),
    }
    if operation.with_active(|| ()).is_none() { return Err("the headless invocation is no longer active".into()); }
    let mut accepted = Err(format!("{} headless export requires the registered core thread", kind.app()));
    let cx = cx.clone();
    let input = input.to_owned();
    let out = out.to_owned();
    let mut done = Some(done);
    crate::host::w::on_core_services_local(|provided, _| {
        if !kind.available(provided) {
            accepted = Err(format!("the {} headless editor is unavailable", kind.app()));
            return;
        }
        let job = Arc::new(Export {
            cx: cx.clone(), kind, operation: operation.clone(), target: OnceLock::new(), cancellation: Mutex::new(Vec::new()),
            created: OnceLock::new(), creation_finished: AtomicBool::new(false), loading: AtomicBool::new(false),
            browser: OnceLock::new(), registered: Mutex::new(None), closing: Mutex::new(None),
            closed: AtomicBool::new(false), settled: AtomicBool::new(false), result: Mutex::new(None), done: Mutex::new(done.take()),
        });
        for token in [operation.cancel().clone(), operation.shutdown().clone(), operation.loss_token()] {
            let weak = Arc::downgrade(&job);
            let subscription = token.on_cancel(move || {
                if let Some(job) = weak.upgrade() { job.fail("the headless invocation was cancelled".into()); }
            });
            job.cancellation.lock().unwrap_or_else(|error| error.into_inner()).push(subscription);
        }
        let ui = cx.ui.clone();
        ui.post(Box::new(move || {
            let frame_job = job.clone();
            let on_frame = Arc::new(move |view: ViewId, event: &tauri_runtime_cef::FrameEvent| {
                match &event.kind {
                    FrameEventKind::NativeContextRegistered { origin } if event.is_main && origin.is_verified()
                        && event.browser_id == origin.browser_id() && event.frame_id == origin.frame_id()
                        && frame_job.loading.load(Ordering::Acquire) => {
                        let result = (|| {
                            let target = frame_job.target.get().ok_or("the hidden text target was not reserved")?;
                            if target.view() != view || frame_job.browser.get() != Some(&origin.browser_id()) {
                                return Err("the hidden text browser registered an unexpected context".to_owned());
                            }
                            let owner = crate::ipc::native_owner::capture_owner(&frame_job.cx.views, view, target.created())?;
                            if !owner.context.authority_ref::<crate::ipc::native_owner::NativeOwnerAuthority>()
                                .is_some_and(|authority| authority.origin == *origin) {
                                return Err("the hidden text document registered another native context".to_owned());
                            }
                            crate::ipc::native_owner::register_native_invocation_target(target, owner)?;
                            *frame_job.registered.lock().unwrap_or_else(|error| error.into_inner()) = Some(origin.clone());
                            Ok::<(), String>(())
                        })();
                        if let Err(error) = result { frame_job.fail(error); }
                    }
                    FrameEventKind::RendererTerminated => frame_job.fail(format!("{} renderer stopped (terminated)", kind.app())),
                    _ => {}
                }
            });
            let spec = WindowSpec {
                title: kind.app().into(), size: (1200.0, 850.0), min_size: None, visible: false,
                decorations: true, transparent: false, resizable: true, parent: None, focus: false, main: false,
                view: ViewSpec { url: kind.url().into(), bridge: Bridge::Surface(kind.surface()) },
            };
            let options = WindowOptions {
                background_throttling: Some(tauri::utils::config::BackgroundThrottlingPolicy::Disabled),
                on_frame: Some(on_frame), ..Default::default()
            };
            let result = (|| {
                let (window, view) = crate::ipc::native_owner::run_native_invocation_ui(
                    job.cx.ui.as_ref(), &job.operation, || job.cx.windows.open_deferred_window(spec, options),
                )??;
                let Some(webview) = job.cx.windows.webview(view) else {
                    let cleanup = job.cx.windows.close_window(window);
                    return Err(format!("the hidden {} webview was not constructed; cleanup: {cleanup:?}", kind.app()));
                };
                let window_handle = webview.window();
                let prepared = (|| {
                    let generation = job.cx.views.renderer_generation(view).ok_or("the hidden text creation epoch is unavailable")?;
                    let close = webview.native_browser_close().map_err(|error| error.to_string())?;
                    Ok::<_, String>((generation, close))
                })();
                let (generation, close) = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => return Err(match window_handle.destroy() {
                        Ok(()) => error, Err(cleanup) => format!("{error}; hidden text browser cleanup failed: {cleanup}"),
                    }),
                };
                let closed_job = job.clone();
                let closing = close.when_closed(move |_| {
                    closed_job.closed.store(true, Ordering::Release);
                    let unavailable = closed_job.clone();
                    crate::host::w::post_core_checked(move |_, _| {
                        if !closed_job.settled.load(Ordering::Acquire) {
                            closed_job.fail(format!("{} renderer stopped (destroyed)", kind.app()));
                        } else { closed_job.close_created(); }
                    }, move || {
                        if unavailable.settled.load(Ordering::Acquire) {
                            unavailable.complete(Some("headless core services are unavailable".into()));
                        } else { unavailable.finish(Err("headless core services are unavailable".into())); }
                    });
                });
                let closing = match closing {
                    Ok(closing) => closing,
                    Err(error) => return Err(match window_handle.destroy() {
                        Ok(()) => error, Err(cleanup) => format!("{error}; hidden text browser cleanup failed: {cleanup}"),
                    }),
                };
                *job.closing.lock().unwrap_or_else(|error| error.into_inner()) = Some(closing);
                Ok(Created { window, view, generation, webview })
            })();
            let created = match result {
                Ok(created) => created,
                Err(error) => {
                    job.creation_finished.store(true, Ordering::Release);
                    if job.settled.load(Ordering::Acquire) { job.close_created(); } else { job.fail(error); }
                    return;
                }
            };
            let view = created.view;
            match job.operation.reserve_target(view, created.generation.clone()) {
                Ok(target) => { if job.target.set(target).is_err() { job.fail("the hidden text target was reserved twice".into()); return; } }
                Err(error) => {
                    let _ = job.created.set(created);
                    job.creation_finished.store(true, Ordering::Release);
                    job.fail(error);
                    return;
                }
            }
            if job.created.set(created).is_err() { job.fail("the hidden text browser was constructed twice".into()); return; }
            job.creation_finished.store(true, Ordering::Release);
            let unavailable = job.clone();
            crate::host::w::post_core_checked(move |provided, _| {
                if job.settled.load(Ordering::Acquire) { job.close_created(); return; }
                if job.closed.load(Ordering::Acquire) { return job.finish(Err(format!("{} renderer stopped (destroyed)", kind.app()))); }
                let finished = job.clone();
                let Some(invocation) = job.target.get().cloned() else { return job.finish(Err("the hidden text target was not reserved".into())); };
                if let Err(error) = kind.begin(provided, view, &input, &out, invocation, Box::new(move |result| finished.finish(result))) {
                    return job.finish(Err(error));
                }
                let webview = job.created.get().expect("the created browser was installed").webview.clone();
                let ready_view = webview.clone();
                let navigation_job = job.clone();
                if let Err(error) = webview.when_navigation_ready(move |result| {
                    if let Err(error) = result { navigation_job.fail(error.to_string()); return; }
                    let bind_job = navigation_job.clone();
                    if let Err(error) = ready_view.with_cef_webview(move |native| {
                        if bind_job.settled.load(Ordering::Acquire) { return; }
                        if bind_job.browser.set(native.snapshot().browser_id).is_err() {
                            bind_job.fail("the hidden text browser identity was bound twice".into()); return;
                        }
                        let load_job = bind_job.clone();
                        bind_job.cx.ui.post(Box::new(move || {
                            let Some(target) = load_job.target.get() else { return load_job.fail("the hidden text target was not reserved".into()); };
                            let result = crate::ipc::native_owner::run_native_invocation_created_ui(
                                load_job.cx.ui.as_ref(), target, || {
                                    load_job.loading.store(true, Ordering::Release);
                                    load_job.cx.windows.load_view(view, kind.url())
                                },
                            ).and_then(|result| result);
                            if let Err(error) = result { load_job.fail(error); }
                        }));
                    }) { navigation_job.fail(error.to_string()); }
                }) { job.fail(error.to_string()); }
            }, move || {
                if unavailable.settled.load(Ordering::Acquire) { unavailable.close_created(); }
                else { unavailable.finish(Err("headless core services are unavailable".into())); }
            });
        }));
        accepted = Ok(());
    });
    accepted
}
