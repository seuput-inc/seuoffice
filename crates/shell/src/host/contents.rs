//! `webContents` of a view (the shared host `Contents`): capture, printToPDF, zoom, focus
//! and closing the window. Capture and PDF are the typed page operations of
//! [`crate::native::PageOps`]; an owned print runs them under one [`Fence`] built from its
//! authority.
//!
//! A page operation blocks its caller until the page answers; the answer needs the UI
//! thread, which a core-thread caller does not hold.

pub(crate) mod owned;
pub(crate) use owned::Target as OwnedPrintTarget;

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use shell_contract::hosts::Contents;
use shell_contract::ids::{ViewId, WindowId};
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelToken, RendererGeneration, Views};
use shell_contract::value::IpcValue;
use shell_contract::{ContentsHost, Hosts};
use tauri::Webview;

use self::owned::{ActorLifetime, Authority, Owner};
use super::HostContext;
use super::window::Windows;
use crate::native::page::blocking;
use crate::native::platform::{Current, PrintOps, PrintOutcome};
use crate::native::{CaptureSpec, DocumentContext, HeaderFooter, PageError, PdfSpec, WebviewExt};
use crate::views::ViewTable;

/// a print of a large document can take a while
pub(crate) const PAGE_TIMEOUT: Duration = Duration::from_secs(90);

/// the longest a system print dialog may stay open before its caller gives up
const DIALOG_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A page operation's error as a caller shows it.
pub(crate) fn page_error_text(error: PageError) -> String {
    error.to_string()
}

/// `webContents.capturePage` of `webview`, blocking. Not from the UI thread.
pub(crate) fn capture_png(webview: &Webview, spec: CaptureSpec) -> Result<Vec<u8>, String> {
    let page = webview.page()?;
    page.blocking(PAGE_TIMEOUT, |done| page.capture_png(spec, None, done))
        .map_err(page_error_text)
}

/// `webContents.printToPDF` of `webview`, blocking. Not from the UI thread.
pub(crate) fn print_pdf(webview: &Webview, spec: PdfSpec) -> Result<Vec<u8>, String> {
    let page = webview.page()?;
    page.blocking(PAGE_TIMEOUT, |done| page.print_pdf(spec, None, done))
        .map_err(page_error_text)
}

/// Script evaluation in `webview`'s page, blocking. Not from the UI thread.
pub(crate) fn evaluate(
    webview: &Webview,
    js: &str,
    await_promise: bool,
) -> Result<Value, PageError> {
    let page = webview.page().map_err(PageError::Platform)?;
    page.blocking(PAGE_TIMEOUT, |done| {
        page.evaluate(js, await_promise, None, done)
    })
}

/// The system print dialog of `webview`; returns when the platform says how it ended
/// (WebView2 does not say: [`PrintOutcome::Unreported`]). Not from the UI thread.
pub(crate) fn print_dialog(webview: &Webview) -> Result<PrintOutcome, String> {
    blocking(DIALOG_TIMEOUT, |done| Current::print_dialog(webview, done)).map_err(page_error_text)
}

struct ShellContents {
    table: Arc<ViewTable>,
    windows: Arc<Windows>,
}

pub fn install(hosts: &mut Hosts, cx: &HostContext) {
    let contents: ContentsHost = Arc::new(ShellContents {
        table: cx.views.clone(),
        windows: cx.windows.clone(),
    });
    hosts.insert(contents);
}

fn field<'a>(value: &'a IpcValue, key: &str) -> Option<&'a IpcValue> {
    match value {
        IpcValue::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

fn number(value: Option<&IpcValue>) -> Option<f64> {
    match value {
        Some(IpcValue::Number(n)) if n.is_finite() => Some(*n),
        _ => None,
    }
}

/// Electron's named paper sizes, in inches
fn paper(name: &str) -> Option<(f64, f64)> {
    Some(match name {
        "A0" => (33.11, 46.81),
        "A1" => (23.39, 33.11),
        "A2" => (16.54, 23.39),
        "A3" => (11.69, 16.54),
        "A4" => (8.27, 11.69),
        "A5" => (5.83, 8.27),
        "A6" => (4.13, 5.83),
        "Legal" => (8.5, 14.0),
        "Letter" => (8.5, 11.0),
        "Tabloid" => (11.0, 17.0),
        _ => return None,
    })
}

/// The template a header or footer shows when `displayHeaderFooter` names none.
const EMPTY_TEMPLATE: &str = "<span></span>";

/// `webContents.printToPDF(options)` as a [`PdfSpec`]. A key Electron has and `PdfSpec` names
/// alike goes across; the page size and the margins (inches in both) are spelled out. What
/// `options` leaves out stays at the spec's default, which is the browser's default.
pub(crate) fn pdf_spec(options: &IpcValue) -> PdfSpec {
    let mut spec = PdfSpec::default();
    let flag = |key: &str| match field(options, key) {
        Some(IpcValue::Bool(value)) => Some(*value),
        _ => None,
    };
    if let Some(landscape) = flag("landscape") {
        spec.landscape = landscape;
    }
    if let Some(background) = flag("printBackground") {
        spec.print_background = background;
    }
    if let Some(css_size) = flag("preferCSSPageSize") {
        spec.prefer_css_page_size = css_size;
    }
    if let Some(tagged) = flag("generateTaggedPDF") {
        spec.tagged = tagged;
    }
    if let Some(outline) = flag("generateDocumentOutline") {
        spec.outline = outline;
    }
    if let Some(scale) = number(field(options, "scale")) {
        spec.scale = scale;
    }
    if let Some(IpcValue::String(ranges)) = field(options, "pageRanges") {
        spec.page_ranges = Some(ranges.clone());
    }
    match field(options, "pageSize") {
        Some(IpcValue::String(name)) => {
            if let Some(size) = paper(name) {
                spec.paper_in = Some(size);
            }
        }
        Some(size @ IpcValue::Object(_)) => {
            if let (Some(width), Some(height)) =
                (number(field(size, "width")), number(field(size, "height")))
            {
                spec.paper_in = Some((width, height));
            }
        }
        _ => {}
    }
    if let Some(margins) = field(options, "margins") {
        for (slot, key) in spec
            .margins_in
            .iter_mut()
            .zip(["top", "right", "bottom", "left"])
        {
            if let Some(margin) = number(field(margins, key)) {
                *slot = margin;
            }
        }
    }
    if flag("displayHeaderFooter") == Some(true) {
        let template = |key: &str| match field(options, key) {
            Some(IpcValue::String(text)) => text.clone(),
            _ => EMPTY_TEMPLATE.to_owned(),
        };
        spec.header_footer = Some(HeaderFooter {
            header_template: template("headerTemplate"),
            footer_template: template("footerTemplate"),
        });
    }
    spec
}

/// The browser and committed document an owned print is bound to. `origin` must be the
/// registered document of the very browser behind `webview`.
fn capture_target(
    webview: &Webview,
    origin: &DocumentContext,
    generation: RendererGeneration,
) -> Result<OwnedPrintTarget, String> {
    let frame = webview.native_browser()?;
    if !frame.is_same_browser(origin.browser()) {
        return Err("The captured print document is not in this view.".to_owned());
    }
    let document = origin
        .current_document()
        .ok_or_else(|| "The captured print document is no longer current.".to_owned())?;
    Ok(OwnedPrintTarget {
        frame,
        document,
        generation,
    })
}

impl ShellContents {
    fn webview_of(&self, view: ViewId) -> Result<Webview, String> {
        self.windows
            .webview(view)
            .ok_or_else(|| format!("no view {}", view.0))
    }

    /// One print of `view` under `authority`. `proof` is the owner whose native origin must
    /// independently prove it is the captured browser and document.
    #[allow(clippy::too_many_arguments)]
    fn print_owned(
        &self,
        view: ViewId,
        proof: &NativeRequestOwner,
        authority: Authority,
        actor: CancelToken,
        generation: RendererGeneration,
        cancel: &CancelToken,
        pending: &CancelToken,
        options: &IpcValue,
    ) -> Result<Vec<u8>, String> {
        let _actor_lifetime = ActorLifetime(actor.clone());
        let origin = &proof
            .context
            .authority_ref::<crate::ipc::native_owner::NativeOwnerAuthority>()
            .ok_or("Print destination is not authenticated.")?
            .origin;
        // The view lookup obtains only a transport conduit.
        let webview = self.webview_of(view)?;
        let target = capture_target(&webview, origin, generation)?;
        let owner = Owner {
            authority,
            cancel: cancel.clone(),
            pending: pending.clone(),
            actor,
            target,
        };
        owner.print_pdf(&webview, pdf_spec(options))
    }
}

impl Contents for ShellContents {
    fn window_of(&self, view: ViewId) -> Option<WindowId> {
        self.table.sender_of(view).map(|s| s.window)
    }

    fn capture_png(&self, view: ViewId) -> Result<Vec<u8>, String> {
        capture_png(&self.webview_of(view)?, CaptureSpec::default())
    }

    fn print_to_pdf_owned(
        &self,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let generation = destination
            .context
            .renderer()
            .cloned()
            .ok_or("Print renderer is unavailable.")?;
        let authority = Authority::Pair {
            origin: origin.clone(),
            destination: destination.clone(),
        };
        self.print_owned(
            destination.view,
            destination,
            authority,
            CancelToken::default(),
            generation,
            cancel,
            pending,
            &options,
        )
    }

    fn print_to_pdf_owned_many_extra(
        &self,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let actor = CancelToken::default();
        let mut captured = Vec::with_capacity(participants.len() + 1);
        captured.push(destination.clone());
        captured.extend_from_slice(participants);
        let authority = Authority::OwnedMany {
            origin: origin.clone(),
            participants: captured,
            additional: additional.to_vec(),
            stop: actor.clone(),
        };
        self.print_owned(
            destination.view,
            destination,
            authority,
            actor,
            RendererGeneration::new(),
            cancel,
            pending,
            &options,
        )
    }

    fn print_to_pdf_native_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let actor = CancelToken::default();
        let authority = Authority::Native {
            invocation: document.target().clone(),
            owner: owner.clone(),
            participants: participants.to_vec(),
            stop: actor.clone(),
        };
        self.print_owned(
            owner.view,
            owner,
            authority,
            actor,
            RendererGeneration::new(),
            cancel,
            pending,
            &options,
        )
    }

    fn print_to_pdf_existing_document(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let actor = CancelToken::default();
        let authority = Authority::ExistingDocument {
            binding: binding.clone(),
            participants: participants.to_vec(),
            additional: additional.to_vec(),
            stop: actor.clone(),
        };
        let owner = binding.owner();
        self.print_owned(
            owner.view,
            owner,
            authority,
            actor,
            RendererGeneration::new(),
            cancel,
            pending,
            &options,
        )
    }

    fn print_to_pdf(&self, view: ViewId, options: IpcValue) -> Result<Vec<u8>, String> {
        print_pdf(&self.webview_of(view)?, pdf_spec(&options))
    }

    /// `webContents.print`: a named printer prints silently through the spooler
    /// (`host/print.rs`); otherwise the system print dialog opens and the answer is how it
    /// ended. WebView2 does not say, so a shown dialog is `true` there.
    fn print(&self, view: ViewId, options: IpcValue) -> Result<bool, String> {
        let webview = self.webview_of(view)?;
        if let Some(IpcValue::Bool(true)) = field(&options, "silent") {
            return super::print::print_silently_ipc(&webview, &options).map(|()| true);
        }
        Ok(print_dialog(&webview)? != PrintOutcome::Cancelled)
    }

    fn zoom_factor(&self, view: ViewId) -> f64 {
        self.table.zoom(view)
    }

    fn set_zoom_factor(&self, view: ViewId, factor: f64) {
        self.table.set_zoom(view, factor);
        if let Some(webview) = self.windows.webview(view)
            && let Err(error) = webview.set_zoom(factor)
        {
            eprintln!("shell: setZoomFactor on view {}: {error}", view.0);
        }
    }

    fn focus(&self, view: ViewId) {
        if let Some(webview) = self.windows.webview(view)
            && let Err(error) = webview.set_focus()
        {
            eprintln!("shell: focus view {}: {error}", view.0);
        }
    }

    fn close_window(&self, window: WindowId) {
        if let Err(error) = self.windows.close_window(window) {
            eprintln!("shell: close window {}: {error}", window.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(entries: Vec<(&str, IpcValue)>) -> IpcValue {
        IpcValue::Object(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        )
    }

    #[test]
    fn electron_options_become_a_pdf_spec() {
        let options = object(vec![
            ("landscape", IpcValue::Bool(true)),
            ("printBackground", IpcValue::Bool(true)),
            ("pageSize", IpcValue::String("A4".into())),
            (
                "margins",
                object(vec![
                    ("top", IpcValue::Number(0.0)),
                    ("left", IpcValue::Number(0.5)),
                ]),
            ),
            ("pageRanges", IpcValue::String("1-2".into())),
            ("scale", IpcValue::Number(0.9)),
        ]);
        let spec = pdf_spec(&options);
        assert_eq!(
            spec,
            PdfSpec {
                landscape: true,
                print_background: true,
                scale: 0.9,
                page_ranges: Some("1-2".into()),
                paper_in: Some((8.27, 11.69)),
                margins_in: [0.0, 0.4, 0.4, 0.5],
                ..PdfSpec::default()
            }
        );
        let custom = object(vec![(
            "pageSize",
            object(vec![
                ("width", IpcValue::Number(5.0)),
                ("height", IpcValue::Number(7.0)),
            ]),
        )]);
        assert_eq!(pdf_spec(&custom).paper_in, Some((5.0, 7.0)));
        assert_eq!(pdf_spec(&IpcValue::Undefined), PdfSpec::default());
    }

    #[test]
    fn header_and_footer_templates_travel_only_when_they_are_displayed() {
        let templates = |display: bool| {
            object(vec![
                ("displayHeaderFooter", IpcValue::Bool(display)),
                ("headerTemplate", IpcValue::String("<h>".into())),
            ])
        };
        assert_eq!(pdf_spec(&templates(false)).header_footer, None);
        assert_eq!(
            pdf_spec(&templates(true)).header_footer,
            Some(HeaderFooter {
                header_template: "<h>".into(),
                footer_template: EMPTY_TEMPLATE.into(),
            })
        );
    }

    #[test]
    fn a_page_error_reads_as_its_own_message() {
        assert_eq!(
            page_error_text(PageError::Timeout),
            PageError::Timeout.to_string()
        );
        assert_eq!(
            page_error_text(PageError::Unsupported("transparent")),
            "transparent is not supported on this platform"
        );
    }
}
