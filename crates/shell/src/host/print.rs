//! Batch M's printing (host/print.rs): the hidden windows the print handlers
//! load a document into ([`window`]), the printer list ([`printers`]) and the
//! printing tools they run ([`run_command`]).
//!
//! The shell prints two ways:
//!
//! - **a silent job** (a `deviceName`): the page goes to PDF through the typed page
//!   operations ([`crate::native::PageOps`]) and the PDF to the printer: `lp` on macOS,
//!   the GDI loop of [`windows`] on Windows;
//! - **the system dialog**: macOS reports whether the user printed or cancelled; WebView2
//!   does not say, so there the job answers success.

mod owned;
mod security;
#[cfg(any(windows, test))]
mod windows;

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use main_core::packages::electron_utils::print_cups::{CupsPrintJob, DriverOption, DuplexMode};
use main_core::packages::electron_utils::print_window::{ExecOutcome, PrintReport};
use main_core::packages::electron_utils::printer_options::CommandOutcome;
use serde_json::{Value, json};
use shell_contract::runtime::{CancelSubscription, CancelToken, Thread, Views};
use shell_contract::value::IpcValue;
use shell_contract::{Surface, ViewId};
use shell_ipc_m::hosts::{PrintProfile, PrintSurface, PrintWindowSpec, Printer};
use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use super::contents::{self, pdf_spec};
use super::{HostContext, on_ui};
use crate::native::platform::{Current, PrintOps, PrintOutcome};
use crate::native::{
    Blocks, EventSink, Native, NativeEvent, NativeEventKind, NativeSpec, NavigationIdentity,
    NavigationStart, ObservedDocument, ObservedDocumentSubscription, PageError, PdfSpec,
    RequestPolicy, WebviewExt, WebviewWindowBuilderExt,
};
use crate::views::ViewTable;

/// Electron's `loadFile` resolved when the page loaded; a page that never
/// does is a failure, not a hang
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest a print preview may stay open before the window is let go
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const PREVIEW_POLL: Duration = Duration::from_millis(300);

/// The flag a page's `afterprint` sets: the print preview closed
#[cfg(windows)]
const AFTERPRINT_FLAG: &str = "__seuofficeAfterPrint";

pub fn install(_hosts: &mut shell_contract::Hosts, _cx: &HostContext) {
    // the print host is part of `MHostRef`, which host/menu.rs inserts
}

// ---- printers ----

/// `webContents.getPrintersAsync()` as `{ name, displayName }`
pub(crate) fn printers() -> Vec<Printer> {
    #[cfg(windows)]
    {
        windows::printers()
            .map(|list| {
                list.into_iter()
                    .map(|(name, display_name, _)| Printer { name, display_name })
                    .collect()
            })
            .unwrap_or_else(|error| {
                eprintln!("shell: printers: {error}");
                Vec::new()
            })
    }
    #[cfg(not(windows))]
    {
        cups_printers()
    }
}

/// The CUPS destinations (`lpstat -e`), each with its `printer-info` as the
/// display name, which is what Chromium showed on macOS and Linux
#[cfg(not(windows))]
fn cups_printers() -> Vec<Printer> {
    let CommandOutcome::Exited {
        code: Some(0),
        stdout,
        ..
    } = run_command("/usr/bin/lpstat", &["-e".into()], None)
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            let info = match run_command("/usr/bin/lpoptions", &["-p".into(), name.into()], None) {
                CommandOutcome::Exited {
                    code: Some(0),
                    stdout,
                    ..
                } => printer_info(&String::from_utf8_lossy(&stdout)),
                _ => None,
            };
            Printer {
                name: name.to_owned(),
                display_name: info.unwrap_or_else(|| name.to_owned()),
            }
        })
        .collect()
}

/// `printer-info` of `lpoptions -p <name>`: `key=value` pairs, a value with
/// spaces quoted with `'`
#[cfg(not(windows))]
fn printer_info(options: &str) -> Option<String> {
    let start = options.find("printer-info=")? + "printer-info=".len();
    let rest = &options[start..];
    let value = if let Some(quoted) = rest.strip_prefix('\'') {
        quoted.split('\'').next()?
    } else {
        rest.split_whitespace().next()?
    };
    Some(value.to_owned()).filter(|v| !v.is_empty())
}

// ---- commands ----

/// `execFile`/`spawn` of a printing tool, `stdin` written then closed
pub(crate) fn run_command(file: &str, args: &[String], stdin: Option<&[u8]>) -> CommandOutcome {
    let mut command = Command::new(file);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return CommandOutcome::Failed(format!("spawn {file} {error}")),
    };
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        use std::io::Write;
        // a tool that exits early closes the pipe: its exit code tells why
        let _ = pipe.write_all(bytes);
    }
    match child.wait_with_output() {
        Ok(output) => CommandOutcome::Exited {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        },
        Err(error) => CommandOutcome::Failed(error.to_string()),
    }
}

// ---- print windows ----

/// `new BrowserWindow({ show: false, ... })` for printing. The window is made
/// when the first page loads into it.
pub(crate) fn window(
    app: AppHandle,
    ui: Arc<dyn Thread>,
    views: Arc<ViewTable>,
    spec: PrintWindowSpec,
) -> Box<dyn PrintSurface> {
    Box::new(new_window(app, ui, views, spec))
}

fn new_window(
    app: AppHandle,
    ui: Arc<dyn Thread>,
    views: Arc<ViewTable>,
    spec: PrintWindowSpec,
) -> PrintWindow {
    PrintWindow {
        app,
        ui,
        views,
        spec,
        window: None,
        loaded: Arc::default(),
        allowed_page: Arc::default(),
        asset_origin: None,
        asset_guard: None,
        drawio_guard: None,
        drawio_session: None,
        drawio_document: None,
        drawio_document_loss: None,
        document_navigation: None,
        cancel_subscription: None,
        asset_load_failed: None,
        destroyed: false,
        owner: None,
    }
}

pub(crate) fn window_owned(
    cx: &HostContext,
    spec: PrintWindowSpec,
    origin: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<Box<dyn PrintSurface>, String> {
    let mut window = new_window(cx.app.clone(), cx.ui.clone(), cx.views.clone(), spec);
    window.owner = Some(owned::Owner::new(
        cx,
        origin.clone(),
        destination.clone(),
        cancel.clone(),
        pending.clone(),
        window.loaded.clone(),
    )?);
    Ok(Box::new(window))
}

pub(crate) fn window_native_invocation(
    cx: &HostContext,
    spec: PrintWindowSpec,
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &shell_contract::ipc::NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<Box<dyn PrintSurface>, String> {
    let mut window = new_window(cx.app.clone(), cx.ui.clone(), cx.views.clone(), spec);
    window.owner = Some(owned::Owner::new_invocation(
        cx,
        target.clone(),
        owner.clone(),
        cancel.clone(),
        pending.clone(),
        window.loaded.clone(),
    )?);
    Ok(Box::new(window))
}

pub(crate) fn window_owned_many_extra(
    cx: &HostContext,
    spec: PrintWindowSpec,
    origin: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
) -> Result<Box<dyn PrintSurface>, String> {
    let mut window = new_window(cx.app.clone(), cx.ui.clone(), cx.views.clone(), spec);
    window.owner = Some(owned::Owner::new_many_extra(
        cx,
        origin.clone(),
        destination.clone(),
        participants,
        cancel.clone(),
        pending.clone(),
        additional,
        None,
        window.loaded.clone(),
    )?);
    Ok(Box::new(window))
}

pub(crate) fn window_native_invocation_many_extra(
    cx: &HostContext,
    spec: PrintWindowSpec,
    document: &shell_contract::ipc::NativeInvocationDocument,
    origin: &shell_contract::ipc::NativeRequestOwner,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
) -> Result<Box<dyn PrintSurface>, String> {
    let mut window = new_window(cx.app.clone(), cx.ui.clone(), cx.views.clone(), spec);
    window.owner = Some(owned::Owner::new_many_extra(
        cx,
        document.owner().clone(),
        origin.clone(),
        participants,
        cancel.clone(),
        pending.clone(),
        additional,
        Some(document.target().clone()),
        window.loaded.clone(),
    )?);
    Ok(Box::new(window))
}

pub(crate) fn window_existing_document(
    cx: &HostContext,
    spec: PrintWindowSpec,
    binding: &shell_contract::ipc::NativeExistingDocumentBinding,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
) -> Result<Box<dyn PrintSurface>, String> {
    let mut window = new_window(cx.app.clone(), cx.ui.clone(), cx.views.clone(), spec);
    window.owner = Some(owned::Owner::new_existing_document(
        cx,
        binding,
        participants,
        cancel.clone(),
        pending.clone(),
        additional,
        window.loaded.clone(),
    )?);
    Ok(Box::new(window))
}

pub(crate) fn markdown_window_existing_document(
    cx: &HostContext,
    binding: &shell_contract::ipc::NativeExistingDocumentBinding,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
) -> Result<Box<dyn PrintSurface>, String> {
    let mut window = new_window(
        cx.app.clone(),
        cx.ui.clone(),
        cx.views.clone(),
        PrintWindowSpec {
            profile: PrintProfile::Static,
            width: None,
            height: None,
            parent: None,
            dialog_chrome: false,
        },
    );
    window.owner = Some(owned::Owner::new_existing_document(
        cx,
        binding,
        participants,
        cancel.clone(),
        pending.clone(),
        additional,
        window.loaded.clone(),
    )?);
    window.asset_origin = Some((
        binding.owner().view,
        super::document_assets::AssetKind::Markdown,
    ));
    window.document_navigation = Some(Arc::default());
    window.asset_load_failed = Some(Arc::default());
    Ok(Box::new(window))
}

pub(crate) fn markdown_window_owned(
    cx: &HostContext,
    origin: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<Box<dyn PrintSurface>, String> {
    let mut window = new_window(
        cx.app.clone(),
        cx.ui.clone(),
        cx.views.clone(),
        PrintWindowSpec {
            profile: PrintProfile::Static,
            width: None,
            height: None,
            parent: None,
            dialog_chrome: false,
        },
    );
    window.owner = Some(owned::Owner::new(
        cx,
        origin.clone(),
        destination.clone(),
        cancel.clone(),
        pending.clone(),
        window.loaded.clone(),
    )?);
    window.asset_origin = Some((
        destination.view,
        super::document_assets::AssetKind::Markdown,
    ));
    window.document_navigation = Some(Arc::default());
    window.asset_load_failed = Some(Arc::default());
    Ok(Box::new(window))
}

pub(crate) fn markdown_window_native_invocation(
    cx: &HostContext,
    document: &shell_contract::ipc::NativeInvocationDocument,
    owner: &shell_contract::ipc::NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<Box<dyn PrintSurface>, String> {
    let mut window = new_window(
        cx.app.clone(),
        cx.ui.clone(),
        cx.views.clone(),
        PrintWindowSpec {
            profile: PrintProfile::Static,
            width: None,
            height: None,
            parent: None,
            dialog_chrome: false,
        },
    );
    window.owner = Some(owned::Owner::new_invocation(
        cx,
        document.target().clone(),
        owner.clone(),
        cancel.clone(),
        pending.clone(),
        window.loaded.clone(),
    )?);
    window.asset_origin = Some((owner.view, super::document_assets::AssetKind::Markdown));
    window.document_navigation = Some(Arc::default());
    window.asset_load_failed = Some(Arc::default());
    Ok(Box::new(window))
}

/// Renders one editor's HTML on a scoped, unbridged static print browser.
/// Runs on a worker, never on the UI or core thread.
pub(crate) fn render_html_pdf(
    cx: &HostContext,
    origin: ViewId,
    html: &str,
    doc_path: Option<&str>,
    cancel: &CancelToken,
) -> Result<Vec<u8>, String> {
    if !cx
        .views
        .sender_of(origin)
        .is_some_and(|sender| sender.surface == Surface::Html)
    {
        return Err("the HTML document view is no longer available".into());
    }
    if cancel.is_cancelled() {
        return Err("HTML printing was cancelled".into());
    }
    let mut surface = PrintWindow {
        app: cx.app.clone(),
        ui: cx.ui.clone(),
        views: cx.views.clone(),
        spec: PrintWindowSpec {
            profile: PrintProfile::Static,
            width: None,
            height: None,
            parent: None,
            dialog_chrome: false,
        },
        window: None,
        loaded: Arc::default(),
        allowed_page: Arc::default(),
        asset_origin: Some((origin, super::document_assets::AssetKind::Html)),
        asset_guard: None,
        drawio_guard: None,
        drawio_session: None,
        drawio_document: None,
        drawio_document_loss: None,
        document_navigation: Some(Arc::default()),
        cancel_subscription: None,
        asset_load_failed: Some(Arc::default()),
        destroyed: false,
        owner: None,
    };
    let result = surface
        .load_document(html, doc_path, cancel)
        .and_then(|()| {
            if cancel.is_cancelled() {
                return Err("HTML printing was cancelled".into());
            }
            surface
                .print_to_pdf(&json!({
                    "pageSize": "A4",
                    "printBackground": true,
                    "margins": { "top": 0.6, "bottom": 0.6, "left": 0.6, "right": 0.6 }
                }))
                .and_then(|bytes| {
                    if cancel.is_cancelled() {
                        Err("HTML printing was cancelled".into())
                    } else {
                        Ok(bytes)
                    }
                })
        });
    surface.destroy();
    result
}

pub(crate) fn render_html_pdf_owned(
    cx: &HostContext,
    origin: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    html: &str,
    doc_path: Option<&str>,
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<Vec<u8>, String> {
    if !cx
        .views
        .sender_of(destination.view)
        .is_some_and(|sender| sender.surface == Surface::Html)
    {
        return Err("the HTML document view is no longer available".into());
    }
    let mut surface = new_window(
        cx.app.clone(),
        cx.ui.clone(),
        cx.views.clone(),
        PrintWindowSpec {
            profile: PrintProfile::Static,
            width: None,
            height: None,
            parent: None,
            dialog_chrome: false,
        },
    );
    let owner = owned::Owner::new(
        cx,
        origin.clone(),
        destination.clone(),
        cancel.clone(),
        pending.clone(),
        surface.loaded.clone(),
    )?;
    owner.worker()?;
    surface.asset_origin = Some((destination.view, super::document_assets::AssetKind::Html));
    surface.document_navigation = Some(Arc::default());
    surface.asset_load_failed = Some(Arc::default());
    surface.owner = Some(owner.clone());
    let result = surface
        .load_document(html, doc_path, &owner.loss_token())
        .and_then(|()| {
            owner.check()?;
            surface
                .print_to_pdf(&json!({
                    "pageSize": "A4", "printBackground": true,
                    "margins": { "top": 0.6, "bottom": 0.6, "left": 0.6, "right": 0.6 }
                }))
                .and_then(|bytes| owner.effect(|| bytes))
        });
    surface.destroy();
    result
}

pub(crate) fn render_html_pdf_existing_document(
    cx: &HostContext,
    binding: &shell_contract::ipc::NativeExistingDocumentBinding,
    participants: &[shell_contract::ipc::NativeRequestOwner],
    html: &str,
    doc_path: Option<&str>,
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
) -> Result<Vec<u8>, String> {
    if !cx
        .views
        .sender_of(binding.owner().view)
        .is_some_and(|sender| sender.surface == Surface::Html)
    {
        return Err("the HTML document view is no longer available".into());
    }
    let mut surface = new_window(
        cx.app.clone(),
        cx.ui.clone(),
        cx.views.clone(),
        PrintWindowSpec {
            profile: PrintProfile::Static,
            width: None,
            height: None,
            parent: None,
            dialog_chrome: false,
        },
    );
    let owner = owned::Owner::new_existing_document(
        cx,
        binding,
        participants,
        cancel.clone(),
        pending.clone(),
        additional,
        surface.loaded.clone(),
    )?;
    owner.worker()?;
    surface.asset_origin = Some((
        binding.owner().view,
        super::document_assets::AssetKind::Html,
    ));
    surface.document_navigation = Some(Arc::default());
    surface.asset_load_failed = Some(Arc::default());
    surface.owner = Some(owner.clone());
    let result = surface
        .load_document(html, doc_path, &owner.loss_token())
        .and_then(|()| {
            owner.check()?;
            surface
                .print_to_pdf(&json!({
                    "pageSize": "A4", "printBackground": true,
                    "margins": { "top": 0.6, "bottom": 0.6, "left": 0.6, "right": 0.6 }
                }))
                .and_then(|bytes| owner.effect(|| bytes))
        });
    surface.destroy();
    result
}

pub(crate) fn render_html_pdf_native_invocation(
    cx: &HostContext,
    document: &shell_contract::ipc::NativeInvocationDocument,
    owner: &shell_contract::ipc::NativeRequestOwner,
    html: &str,
    doc_path: Option<&str>,
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<Vec<u8>, String> {
    if !cx
        .views
        .sender_of(owner.view)
        .is_some_and(|sender| sender.surface == Surface::Html)
    {
        return Err("the HTML document view is no longer available".into());
    }
    let mut surface = new_window(
        cx.app.clone(),
        cx.ui.clone(),
        cx.views.clone(),
        PrintWindowSpec {
            profile: PrintProfile::Static,
            width: None,
            height: None,
            parent: None,
            dialog_chrome: false,
        },
    );
    let owner = owned::Owner::new_invocation(
        cx,
        document.target().clone(),
        owner.clone(),
        cancel.clone(),
        pending.clone(),
        surface.loaded.clone(),
    )?;
    owner.worker()?;
    surface.asset_origin = Some((
        owner.destination.view,
        super::document_assets::AssetKind::Html,
    ));
    surface.document_navigation = Some(Arc::default());
    surface.asset_load_failed = Some(Arc::default());
    surface.owner = Some(owner.clone());
    let result = surface
        .load_document(html, doc_path, &owner.loss_token())
        .and_then(|()| {
            owner.check()?;
            surface
                .print_to_pdf(&json!({
                    "pageSize": "A4", "printBackground": true,
                    "margins": { "top": 0.6, "bottom": 0.6, "left": 0.6, "right": 0.6 }
                }))
                .and_then(|bytes| owner.effect(|| bytes))
        });
    surface.destroy();
    result
}

pub(crate) fn render_diagram_pdf_native_invocation(
    cx: &HostContext,
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &shell_contract::ipc::NativeRequestOwner,
    pages: &[main_core::diagram::print_document::PrintPage],
    title: &str,
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<Vec<u8>, String> {
    let html = main_core::diagram::print_document::build_print_html(pages, title);
    let mut surface = new_window(
        cx.app.clone(),
        cx.ui.clone(),
        cx.views.clone(),
        PrintWindowSpec {
            profile: PrintProfile::Static,
            width: Some(900.0),
            height: Some(1200.0),
            parent: None,
            dialog_chrome: false,
        },
    );
    let owner = owned::Owner::new_invocation(
        cx,
        target.clone(),
        owner.clone(),
        cancel.clone(),
        pending.clone(),
        surface.loaded.clone(),
    )?;
    owner.worker()?;
    surface.drawio_session = Some(super::d1::diagram_headless::network::request_policy());
    surface.document_navigation = Some(Arc::default());
    surface.asset_load_failed = Some(Arc::default());
    surface.owner = Some(owner.clone());
    let result = surface
        .load_drawio_document(html.into_bytes(), &owner.loss_token())
        .and_then(|()| {
            let ready = main_core::packages::electron_utils::print_window::PRINT_READY_SCRIPT;
            let expression = format!(
                "Promise.race([{ready}, new Promise(resolve => setTimeout(resolve, 5000))])"
            );
            let _ = surface.evaluate(&surface.webview()?, &expression, true);
            owner.check()?;
            surface
                .print_to_pdf(&json!({
                    "printBackground": true, "preferCSSPageSize": true,
                    "margins": { "top": 0, "bottom": 0, "left": 0, "right": 0 }
                }))
                .and_then(|bytes| owner.effect(|| bytes))
        });
    surface.destroy();
    result
}

#[derive(Default)]
struct DocumentNavigation {
    expected: Option<Url>,
    initial_seen: bool,
    target: Option<Url>,
    consumed: bool,
}

impl DocumentNavigation {
    fn allow(&mut self, url: &Url) -> bool {
        if !self.initial_seen && url.as_str() == "about:blank" {
            self.initial_seen = true;
            return true;
        }
        if !self.consumed && self.target.as_ref() == Some(url) {
            self.initial_seen = true;
            self.consumed = true;
            self.target = None;
            return true;
        }
        false
    }
}

type LoadSignal = Arc<Mutex<Option<mpsc::SyncSender<()>>>>;

struct PrintWindow {
    app: AppHandle,
    ui: Arc<dyn Thread>,
    views: Arc<ViewTable>,
    spec: PrintWindowSpec,
    window: Option<WebviewWindow>,
    /// where the next finished page load is reported
    loaded: LoadSignal,
    allowed_page: Arc<Mutex<Option<Url>>>,
    asset_origin: Option<(ViewId, super::document_assets::AssetKind)>,
    asset_guard: Option<super::document_assets::TargetGuard>,
    drawio_guard: Option<crate::drawio_scheme::print::PrintGrant>,
    drawio_session: Option<RequestPolicy>,
    drawio_document: Option<ObservedDocument>,
    drawio_document_loss: Option<ObservedDocumentSubscription>,
    document_navigation: Option<Arc<Mutex<DocumentNavigation>>>,
    cancel_subscription: Option<CancelSubscription>,
    asset_load_failed: Option<Arc<std::sync::atomic::AtomicBool>>,
    destroyed: bool,
    owner: Option<owned::Owner>,
}

/// What the print browser's events change: the load signal, the failure flag, and the
/// owner's target and lifetime. Runs on the UI thread.
struct PrintEvents {
    loaded: LoadSignal,
    failed: Option<Arc<std::sync::atomic::AtomicBool>>,
    document: Option<Arc<Mutex<DocumentNavigation>>>,
    owner: Option<owned::Owner>,
    drawio_mode: bool,
    /// The latest main-frame navigation and the address it started for.
    navigations: Mutex<Option<(NavigationIdentity, Url)>>,
}

impl PrintEvents {
    fn on_event(&self, event: &NativeEvent) {
        match &event.kind {
            NativeEventKind::NavigationStarted {
                navigation,
                url,
                same_document,
            } => {
                if !same_document {
                    self.remember(navigation, url);
                }
                self.navigated(url, !same_document);
            }
            NativeEventKind::DocumentCommitted {
                navigation, url, ..
            } => {
                self.remember(navigation, url);
                self.navigated(url, true);
            }
            NativeEventKind::AddressChanged { url } => self.navigated(url, false),
            NativeEventKind::Retired { .. } => {
                if let Some(owner) = &self.owner {
                    owner.stop();
                }
            }
            NativeEventKind::WebviewProcessFailed { .. } if !self.drawio_mode => {
                if let Some(owner) = &self.owner {
                    owner.stop();
                }
            }
            NativeEventKind::LoadFinished { navigation, ok } => self.finished(navigation, *ok),
            _ => {}
        }
    }

    fn remember(&self, navigation: &NavigationIdentity, url: &Url) {
        if let Ok(mut latest) = self.navigations.lock() {
            *latest = Some((navigation.clone(), url.clone()));
        }
    }

    fn expected(&self, url: &Url) -> bool {
        self.document.as_ref().is_some_and(|state| {
            state
                .lock()
                .ok()
                .is_some_and(|state| state.expected.as_ref() == Some(url))
        })
    }

    /// A navigation or address change. The diagram browser admits only its one reserved
    /// document; any other address ends the print. A new document invalidates the target.
    fn navigated(&self, url: &Url, new_document: bool) {
        let Some(owner) = &self.owner else { return };
        if self.drawio_mode && !self.expected(url) {
            owner.stop();
            return;
        }
        if new_document {
            owner.invalidate_target();
        }
    }

    /// A load ended. With a reserved document only that document's load counts. A failed
    /// load is flagged, and either way the waiting caller is woken.
    fn finished(&self, navigation: &NavigationIdentity, ok: bool) {
        if self.document.is_some() {
            let url = self.navigations.lock().ok().and_then(|latest| {
                latest
                    .as_ref()
                    .filter(|(seen, _)| seen == navigation)
                    .map(|(_, url)| url.clone())
            });
            if !url.is_some_and(|url| self.expected(&url)) {
                return;
            }
        }
        if !ok && let Some(failed) = &self.failed {
            failed.store(true, std::sync::atomic::Ordering::Release);
        }
        if let Ok(mut waiting) = self.loaded.lock()
            && let Some(tx) = waiting.take()
        {
            let _ = tx.try_send(());
        }
    }
}

static NEXT_LABEL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl PrintWindow {
    fn scripts_on(&self) -> bool {
        security::scripts_enabled(self.spec.profile)
    }

    fn load_drawio_document(&mut self, bytes: Vec<u8>, cancel: &CancelToken) -> Result<(), String> {
        let owner = self
            .owner
            .clone()
            .ok_or("The diagram print authority is unavailable")?;
        owner.worker()?;
        if self.destroyed || self.window.is_some() || self.drawio_session.is_none() {
            return Err("The diagram print browser is not fresh".into());
        }
        let blank = Url::parse("about:blank").map_err(|error| error.to_string())?;
        self.document_navigation
            .as_ref()
            .ok_or("The diagram navigation state is unavailable")?
            .lock()
            .map_err(|_| "The diagram navigation state is poisoned")?
            .expected = Some(blank.clone());
        self.window = Some(self.build(blank)?);
        let webview = self.webview()?;
        let (activation_tx, activation_rx) = mpsc::sync_channel(1);
        let cancelled = activation_tx.clone();
        let activation_loss = cancel.on_cancel(move || {
            let _ = cancelled.try_send(Err("The diagram print was cancelled".to_owned()));
        });
        let activation_owner = owner.clone();
        let activation_view = webview.clone();
        owner.on_ui(move || {
            activation_owner.accepted(|| {
                activation_view.native_browser().map(|browser| {
                    browser.navigator().activate_initial(move |result| {
                        let _ = activation_tx.try_send(result);
                    });
                })
            })?
        })?;
        activation_rx
            .recv_timeout(LOAD_TIMEOUT)
            .map_err(|_| "The diagram print browser activation timed out".to_owned())??;
        drop(activation_loss);
        owner.check()?;
        let browser = webview.native_browser()?;
        let authority_owner = owner.clone();
        let authority: crate::drawio_scheme::print::Admission =
            Arc::new(move |effect| authority_owner.effect(|| effect())?);
        let guard = crate::drawio_scheme::print::reserve(browser.clone(), bytes, authority)?;
        let target = Url::parse(&guard.url()).map_err(|error| error.to_string())?;
        let observed_target = target.clone();
        {
            let mut navigation = self
                .document_navigation
                .as_ref()
                .ok_or("The diagram navigation state is unavailable")?
                .lock()
                .map_err(|_| "The diagram navigation state is poisoned")?;
            navigation.target = Some(target.clone());
            navigation.expected = Some(target.clone());
        }
        self.drawio_guard = Some(guard);
        let (loaded_tx, loaded_rx) = mpsc::sync_channel(1);
        let cancelled = loaded_tx.clone();
        let loaded_loss = cancel.on_cancel(move || {
            let _ = cancelled.try_send(());
        });
        *self
            .loaded
            .lock()
            .map_err(|_| "The diagram load state is poisoned")? = Some(loaded_tx);
        let (observed_tx, observed_rx) = mpsc::sync_channel(1);
        let cancelled = observed_tx.clone();
        let observed_loss = owner.loss_token().on_cancel(move || {
            let _ = cancelled.try_send(Err("The diagram print was cancelled".to_owned()));
        });
        let navigation_owner = owner.clone();
        let navigation_view = webview.clone();
        let loss_owner = owner.clone();
        let navigation = owner.on_ui(move || {
            navigation_owner.accepted(|| {
                navigation_view.native_browser().and_then(|browser| {
                    browser
                        .navigator()
                        .navigate_observed(target, move |result| {
                            let result = result.and_then(|document| {
                                let subscription =
                                    document.when_lost(move |_| loss_owner.stop())?;
                                Ok((document, subscription))
                            });
                            let _ = observed_tx.try_send(result);
                        })
                })
            })?
        })?;
        let observed = match observed_rx.recv_timeout(LOAD_TIMEOUT) {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                navigation.cancel();
                owner.stop();
                return Err(error);
            }
            Err(_) => {
                navigation.cancel();
                owner.stop();
                return Err("The diagram print navigation timed out".into());
            }
        };
        drop(observed_loss);
        if !observed.0.browser_state().is_same_browser(&browser)
            || observed.0.url() != &observed_target
        {
            owner.stop();
            return Err("The diagram print navigation did not commit the reserved document".into());
        }
        self.drawio_document = Some(observed.0);
        self.drawio_document_loss = Some(observed.1);
        loaded_rx
            .recv_timeout(LOAD_TIMEOUT)
            .map_err(|_| "The diagram print page did not load".to_owned())?;
        drop(loaded_loss);
        owner.check()?;
        if self
            .asset_load_failed
            .as_ref()
            .is_some_and(|failed| failed.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err("The diagram print page failed to load".into());
        }
        let start = Instant::now();
        while !self
            .drawio_guard
            .as_ref()
            .ok_or("The diagram print grant was released")?
            .served()?
        {
            owner.check()?;
            if start.elapsed() >= LOAD_TIMEOUT {
                return Err("The diagram print document did not become ready".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        owner.bind_target(&webview)?;
        owner.check_drawio_target(
            self.drawio_guard
                .as_ref()
                .ok_or("The diagram print grant was released")?,
        )?;
        Ok(())
    }

    fn load_document(
        &mut self,
        html: &str,
        doc_path: Option<&str>,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        let result = self.load_document_inner(html, doc_path, cancel);
        if result.is_err() {
            self.destroy();
        }
        result
    }

    fn load_document_inner(
        &mut self,
        html: &str,
        doc_path: Option<&str>,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        let base = doc_path.map(|path| {
            let directory = jscompat::node_path::host::dirname(path);
            main_core::html::preview_document::asset_base_href(&directory)
        });
        let page = main_core::html::preview_document::build_preview_document(html, base.as_deref());
        self.load_scoped_document(page.into_bytes(), cancel)
    }

    fn load_scoped_document(&mut self, bytes: Vec<u8>, cancel: &CancelToken) -> Result<(), String> {
        if let Some(owner) = &self.owner {
            owner.worker()?;
        }
        let (origin, kind) = self.asset_origin.ok_or("missing document asset owner")?;
        if self.destroyed || self.window.is_some() {
            return Err("a document print surface permits only one page".into());
        }
        let (blank_tx, blank_rx) = mpsc::sync_channel(1);
        let cancel_blank = blank_tx.clone();
        let blank_subscription = cancel.on_cancel(move || {
            let _ = cancel_blank.try_send(());
        });
        *self
            .loaded
            .lock()
            .map_err(|_| "print load state is poisoned")? = Some(blank_tx);
        let blank = Url::parse("about:blank").map_err(|error| error.to_string())?;
        self.document_navigation
            .as_ref()
            .ok_or("missing print navigation state")?
            .lock()
            .map_err(|_| "print navigation state is poisoned")?
            .expected = Some(blank.clone());
        self.window = Some(self.build(blank)?);
        let window = self
            .window
            .as_ref()
            .ok_or("the print browser is gone")?
            .clone();
        self.cancel_subscription = Some(cancel.on_cancel(move || {
            let _ = window.destroy();
        }));
        if cancel.is_cancelled() {
            return Err("HTML printing was cancelled".into());
        }
        let webview = self.webview()?;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let cancel_ready = ready_tx.clone();
        let ready_subscription = cancel.on_cancel(move || {
            let _ = cancel_ready.try_send(Err("HTML printing was cancelled".to_owned()));
        });
        webview
            .native_browser()?
            .navigator()
            .when_ready(move |result| {
                let _ = ready_tx.try_send(result);
            });
        ready_rx.recv_timeout(LOAD_TIMEOUT).map_err(|_| {
            "ERR_TIMED_OUT: the print browser navigation was not initialized".to_owned()
        })??;
        drop(ready_subscription);
        blank_rx
            .recv_timeout(LOAD_TIMEOUT)
            .map_err(|_| "ERR_TIMED_OUT: the print browser did not initialize".to_owned())?;
        drop(blank_subscription);
        if cancel.is_cancelled() {
            return Err("HTML printing was cancelled".into());
        }
        if self
            .asset_load_failed
            .as_ref()
            .is_some_and(|failed| failed.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err("the print browser failed to initialize".into());
        }
        if cancel.is_cancelled() {
            return Err("HTML printing was cancelled".into());
        }
        let native = webview.native_browser()?;
        if cancel.is_cancelled() {
            return Err("HTML printing was cancelled".into());
        }
        let label = self
            .window
            .as_ref()
            .ok_or("the print browser is gone")?
            .label()
            .to_owned();
        let grant = || {
            super::document_assets::grant_target(
                &label,
                origin,
                kind,
                native,
                super::document_assets::TargetDocument::Generated(Vec::new()),
            )
        };
        let guard = match &self.owner {
            Some(owner) => owner.effect(grant)??,
            None => grant()?,
        };
        guard.set_cancel_token(
            self.owner
                .as_ref()
                .map(owned::Owner::loss_token)
                .unwrap_or_else(|| cancel.clone()),
        )?;
        if let Some(owner) = &self.owner {
            owner.bind_asset_authority(&guard)?;
        }
        if cancel.is_cancelled() {
            return Err("HTML printing was cancelled".into());
        }
        let target =
            Url::parse(&guard.set_root_document(bytes)?).map_err(|error| error.to_string())?;
        {
            let mut navigation = self
                .document_navigation
                .as_ref()
                .ok_or("missing print navigation state")?
                .lock()
                .map_err(|_| "print navigation state is poisoned")?;
            navigation.target = Some(target.clone());
            navigation.expected = Some(target.clone());
        }
        self.asset_guard = Some(guard);
        if let Some(failed) = &self.asset_load_failed {
            failed.store(false, std::sync::atomic::Ordering::Release);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let cancel_load = tx.clone();
        let load_subscription = cancel.on_cancel(move || {
            let _ = cancel_load.try_send(());
        });
        *self
            .loaded
            .lock()
            .map_err(|_| "print load state is poisoned")? = Some(tx);
        let window = self
            .window
            .as_ref()
            .ok_or("the print browser is gone")?
            .clone();
        if let Some(owner) = &self.owner {
            owner.navigate(window.as_ref(), target)?;
        } else {
            on_ui(&self.ui, move || window.navigate(target)).map_err(|error| error.to_string())?;
        }
        rx.recv_timeout(LOAD_TIMEOUT)
            .map_err(|_| "ERR_TIMED_OUT: the print page did not load".to_owned())?;
        drop(load_subscription);
        if cancel.is_cancelled() {
            return Err("HTML printing was cancelled".into());
        }
        if self
            .asset_load_failed
            .as_ref()
            .is_some_and(|failed| failed.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err("the scoped print page failed to load".into());
        }
        if !self
            .asset_guard
            .as_ref()
            .ok_or("the print asset grant is gone")?
            .root_served()?
        {
            return Err("the scoped print page was not admitted".into());
        }
        if let Some(owner) = &self.owner {
            owner.bind_target(&self.webview()?)?;
            owner.check()?;
        }
        Ok(())
    }

    /// Loads `url` and waits for the page: into a new window the first time
    fn load(&mut self, url: Url) -> Result<(), String> {
        if let Some(owner) = &self.owner {
            owner.worker()?;
        }
        let failed = self
            .asset_load_failed
            .get_or_insert_with(Arc::default)
            .clone();
        failed.store(false, std::sync::atomic::Ordering::Release);
        let (tx, rx) = mpsc::sync_channel(1);
        *self.loaded.lock().unwrap() = Some(tx);
        match &self.window {
            Some(window) => {
                let window = window.clone();
                if let Some(owner) = &self.owner {
                    owner.navigate(window.as_ref(), url)?;
                } else {
                    on_ui(&self.ui, move || window.navigate(url))
                        .map_err(|error| error.to_string())?;
                }
            }
            None => self.window = Some(self.build(url)?),
        }
        rx.recv_timeout(LOAD_TIMEOUT)
            .map_err(|_| "ERR_TIMED_OUT: the print page did not load".to_owned())?;
        if failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err("ERR_FAILED: the print page failed to load".into());
        }
        if let Some(owner) = &self.owner {
            owner.bind_target(&self.webview()?)?;
        }
        #[cfg(windows)]
        {
            if self.scripts_on() {
                self.watch_afterprint();
            }
        }
        if let Some(owner) = &self.owner {
            owner.check()?;
        }
        Ok(())
    }

    /// What the print browser admits: its navigations and its resource requests.
    fn request_policy(&self) -> RequestPolicy {
        let mindmap = matches!(
            self.spec.profile,
            PrintProfile::MindmapExport | PrintProfile::MindmapPrint
        );
        let policy = match (&self.document_navigation, &self.drawio_session) {
            (Some(navigation), session) => {
                let navigation = navigation.clone();
                let session_policy = session.clone();
                // Local files are never a print document's business; the web is the diagram
                // session's call, and open otherwise.
                let web = session
                    .as_ref()
                    .map(RequestPolicy::blocks)
                    .unwrap_or_default();
                RequestPolicy::new(move |request| {
                    let Ok(url) = Url::parse(request.url) else {
                        return false;
                    };
                    if request.main_frame {
                        return navigation.lock().is_ok_and(|mut state| state.allow(&url));
                    }
                    url.scheme() != "file"
                        && session_policy
                            .as_ref()
                            .is_none_or(|session| session.allows(request))
                })
                .with_blocks(Blocks { files: true, ..web })
            }
            (None, _) if mindmap => {
                let page = self.allowed_page.clone();
                // The page is a local file with local siblings: the allow function judges
                // them; only the remote web is refused wholesale.
                RequestPolicy::new(move |request| {
                    page.lock()
                        .ok()
                        .and_then(|page| {
                            page.as_ref()
                                .map(|page| security::allows(request.url, page))
                        })
                        .unwrap_or(false)
                })
                .with_blocks(Blocks {
                    web: true,
                    ..Blocks::NOTHING
                })
            }
            (None, _) => RequestPolicy::allow_all(),
        };
        let policy = policy.with_javascript(self.scripts_on());
        match &self.drawio_session {
            Some(session) => policy.with_storage(session.storage()),
            None => policy,
        }
    }

    fn build(&self, url: Url) -> Result<WebviewWindow, String> {
        let label = format!(
            "print-{}",
            NEXT_LABEL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let app = self.app.clone();
        let spec = self.spec.clone();
        let policy = self.request_policy();
        let document_mode = self.document_navigation.is_some();
        let drawio_mode = self.drawio_session.is_some();
        let start = if drawio_mode {
            NavigationStart::Explicit
        } else {
            NavigationStart::Automatic
        };
        let events = Arc::new(PrintEvents {
            loaded: self.loaded.clone(),
            failed: self.asset_load_failed.clone(),
            document: self.document_navigation.clone(),
            owner: self.owner.clone(),
            drawio_mode,
            navigations: Mutex::default(),
        });
        let parent = spec
            .parent
            .and_then(|w| self.views.label_of_window(w))
            .and_then(|label| app.get_webview_window(&label));
        let owner = self.owner.clone();
        let construct = move || {
            let build = move || {
                let (width, height) = if spec.dialog_chrome {
                    (900.0, 700.0)
                } else {
                    (spec.width.unwrap_or(800.0), spec.height.unwrap_or(600.0))
                };
                let native = app.state::<Native>().inner().clone();
                let sink: EventSink = Arc::new(move |event: &NativeEvent| events.on_event(event));
                let native_spec = NativeSpec::new()
                    .native_events(sink)
                    .request_policy(policy)
                    .navigation_start(start);
                let (mut builder, browser) = WebviewWindowBuilder::new(
                    &app,
                    &label,
                    WebviewUrl::External(url),
                )
                .native(&native, &label, native_spec)?;
                builder = builder.visible(false).inner_size(width, height);
                if document_mode {
                    builder = builder.on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny);
                }
                if spec.dialog_chrome {
                    builder = builder.closable(false).skip_taskbar(true);
                }
                if let Some(parent) = &parent {
                    builder = builder.parent(parent).map_err(|e| e.to_string())?;
                }
                let window = builder.build().map_err(|e| e.to_string())?;
                if let Err(error) = native.attach(window.as_ref(), &browser) {
                    let _ = window.destroy();
                    return Err(error);
                }
                Ok(window)
            };
            match owner {
                Some(owner) => {
                    let window = owner.accepted(build)??;
                    owner.retain_window(&window)?;
                    Ok(window)
                }
                None => build(),
            }
        };
        let window = match &self.owner {
            Some(owner) => owner.on_ui(construct),
            None => on_ui(&self.ui, construct),
        }?;
        Ok(window)
    }

    fn webview(&self) -> Result<tauri::Webview, String> {
        self.window
            .as_ref()
            .map(|w| w.as_ref().clone())
            .ok_or_else(|| "the print window has no page".to_owned())
    }

    #[cfg(windows)]
    fn evaluate(
        &self,
        webview: &tauri::Webview,
        js: &str,
        await_promise: bool,
    ) -> Result<Value, PageError> {
        evaluate_in(self.owner.as_ref(), webview, js, await_promise)
    }

    fn ui_action(&self, action: impl FnOnce() + Send + 'static) -> Result<(), String> {
        let owner = self.owner.clone();
        let accepted = move || match owner {
            Some(owner) => owner.accepted(action),
            None => Ok(action()),
        };
        match &self.owner {
            Some(owner) => owner.on_ui(accepted),
            None => on_ui(&self.ui, accepted),
        }
    }

    /// Remembers in the page when its print preview closed (`afterprint`)
    #[cfg(windows)]
    fn watch_afterprint(&self) {
        let Ok(webview) = self.webview() else { return };
        // One expression (`evaluate` takes no statement list): the comma operator sequences
        // the flag reset and the listener, and `undefined` is the answer.
        let script = format!(
            "(window.{AFTERPRINT_FLAG} = false, addEventListener('afterprint', () => {{ window.{AFTERPRINT_FLAG} = true }}), undefined)"
        );
        let _ = self.evaluate(&webview, &script, false);
    }

    /// Opens the system print dialog and waits until it closed. macOS says how it ended.
    /// WebView2 does not: there it waits for the page's `afterprint` or the window going
    /// away, whichever comes first; a page without scripts cannot report `afterprint`, so its
    /// window stays until the user closes it. `false` means the user cancelled.
    fn preview(&mut self) -> Result<bool, String> {
        if let Some(owner) = &self.owner {
            owner.worker()?;
        }
        let window = self.window.clone().ok_or("the print window has no page")?;
        let webview = self.webview()?;
        let shown = window.clone();
        self.ui_action(move || {
            let _ = shown.show();
            let _ = shown.set_focus();
        })?;
        let (tx, rx) = mpsc::sync_channel(1);
        let dialog_view = webview.clone();
        let open = move || {
            Current::print_dialog(
                &dialog_view,
                Box::new(move |result| {
                    let _ = tx.try_send(result);
                }),
            );
        };
        match &self.owner {
            Some(owner) => owner.effect(open)?,
            None => open(),
        }
        let started = Instant::now();
        let mut outcome = None;
        while started.elapsed() < PREVIEW_TIMEOUT {
            match rx.recv_timeout(PREVIEW_POLL) {
                Ok(result) => {
                    outcome = Some(result.map_err(contents::page_error_text)?);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("The print dialog went away.".to_owned());
                }
            }
            if let Some(owner) = &self.owner {
                owner.check()?;
            }
            if !window.is_visible().unwrap_or(false) {
                break;
            }
        }
        if let Some(owner) = &self.owner {
            owner.check()?;
        }
        match outcome {
            Some(PrintOutcome::Cancelled) => return Ok(false),
            #[cfg(windows)]
            Some(PrintOutcome::Unreported) => {
                self.wait_for_afterprint(&window, &webview, started)?
            }
            #[cfg(target_os = "macos")]
            Some(PrintOutcome::Printed) | None => {}
            #[cfg(windows)]
            None => {}
        }
        Ok(true)
    }

    #[cfg(windows)]
    fn wait_for_afterprint(
        &self,
        window: &WebviewWindow,
        webview: &tauri::Webview,
        started: Instant,
    ) -> Result<(), String> {
        while started.elapsed() < PREVIEW_TIMEOUT {
            std::thread::sleep(PREVIEW_POLL);
            if let Some(owner) = &self.owner {
                owner.check()?;
            }
            if !window.is_visible().unwrap_or(false) {
                break;
            }
            if self.scripts_on()
                && let Ok(answer) = self.evaluate(
                    webview,
                    &format!("window.{AFTERPRINT_FLAG} === true"),
                    false,
                )
                && answer == true
            {
                break;
            }
        }
        if let Some(owner) = &self.owner {
            owner.check()?;
        }
        Ok(())
    }

    fn silent(&self, options: &Value) -> PrintReport {
        let printed = if let Some(owner) = &self.owner {
            self.webview()
                .and_then(|webview| owner.print_pdf(&webview, silent_pdf_spec(options)))
                .and_then(|bytes| {
                    let accepted = owner.effect(|| || spool(&bytes, options))?;
                    accepted()
                })
        } else {
            self.webview()
                .and_then(|webview| print_silently(&webview, options))
        };
        match printed {
            Ok(()) => PrintReport {
                success: true,
                failure_reason: String::new(),
            },
            Err(error) => failed(error),
        }
    }
}

impl PrintSurface for PrintWindow {
    fn load_file(&mut self, path: &str) -> Result<(), String> {
        if matches!(
            self.asset_origin,
            Some((_, super::document_assets::AssetKind::Markdown))
        ) {
            let owner = self
                .owner
                .clone()
                .ok_or("missing Markdown print authority")?;
            owner.worker()?;
            let bytes = std::fs::read(path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    format!("ERR_FILE_NOT_FOUND: {path}")
                } else {
                    main_core::node_fs::node_error(&error, "open", path).message
                }
            })?;
            owner.check()?;
            let result = self.load_scoped_document(bytes, &owner.loss_token());
            if result.is_err() {
                self.destroy();
            }
            return result;
        }
        if self.asset_origin.is_some() {
            return Err("document print pages must be supplied as scoped HTML".into());
        }
        let url = Url::from_file_path(Path::new(path))
            .map_err(|()| format!("ERR_FILE_NOT_FOUND: {path}"))?;
        if matches!(
            self.spec.profile,
            PrintProfile::MindmapExport | PrintProfile::MindmapPrint
        ) {
            *self
                .allowed_page
                .lock()
                .map_err(|_| "mindmap print policy unavailable")? = Some(url.clone());
        }
        self.load(url)
    }

    fn load_url(&mut self, url: &str) -> Result<(), String> {
        if self.asset_origin.is_some() {
            return Err("document print surfaces require a generated local preview".into());
        }
        if matches!(
            self.spec.profile,
            PrintProfile::MindmapExport | PrintProfile::MindmapPrint
        ) {
            return Err("mindmap print requires a local page file".to_owned());
        }
        let url = crate::app_scheme::webview_url(url)
            .or_else(|| Url::parse(url).ok())
            .ok_or_else(|| format!("ERR_INVALID_URL: {url}"))?;
        self.load(url)
    }

    fn execute_javascript(
        &mut self,
        script: &str,
        _user_gesture: bool,
        race_ms: Option<f64>,
    ) -> ExecOutcome {
        if let Some(owner) = &self.owner {
            if let Err(error) = owner.worker() {
                return ExecOutcome::Throws(error);
            }
        }
        let webview = match self.webview() {
            Ok(webview) => webview,
            Err(error) => return ExecOutcome::Throws(error),
        };
        let script = script.to_owned();
        let (tx, rx) = mpsc::sync_channel(1);
        let owner = self.owner.clone();
        std::thread::spawn(move || {
            let _ = tx.send(evaluate_in(owner.as_ref(), &webview, &script, true));
        });
        // without a race the call waits as long as the page does
        let settled = match race_ms {
            Some(ms) => rx.recv_timeout(Duration::from_millis(ms.max(0.0) as u64)),
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        if let Some(owner) = &self.owner {
            if let Err(error) = owner.check() {
                return ExecOutcome::Throws(error);
            }
        }
        match settled {
            Err(mpsc::RecvTimeoutError::Timeout) => ExecOutcome::Pending,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                ExecOutcome::Throws("the page went away".to_owned())
            }
            // the page's own exception; every other error is the call failing
            Ok(Err(PageError::Platform(message))) => ExecOutcome::Rejected(message),
            Ok(Err(error)) => ExecOutcome::Throws(error.to_string()),
            Ok(Ok(_)) => ExecOutcome::Settled,
        }
    }

    fn print(&mut self, options: &Value) -> PrintReport {
        if options["silent"] == true {
            return self.silent(options);
        }
        match self.preview() {
            Ok(true) => PrintReport {
                success: true,
                failure_reason: String::new(),
            },
            Ok(false) => failed("Print job canceled"),
            Err(error) => failed(error),
        }
    }

    fn print_to_pdf(&mut self, options: &Value) -> Result<Vec<u8>, String> {
        let spec = pdf_spec(&ipc_value(options));
        let webview = self.webview()?;
        match &self.owner {
            Some(owner) => owner.print_pdf(&webview, spec),
            None => contents::print_pdf(&webview, spec),
        }
    }

    fn show(&mut self) {
        if let Some(window) = self.window.clone() {
            let _ = self.ui_action(move || {
                let _ = window.show();
            });
        }
    }

    fn focus(&mut self) {
        if let Some(window) = self.window.clone() {
            let _ = self.ui_action(move || {
                let _ = window.set_focus();
            });
        }
    }

    fn is_destroyed(&self) -> bool {
        self.destroyed || self.owner.as_ref().is_some_and(owned::Owner::is_stopped)
    }

    fn destroy(&mut self) {
        self.destroyed = true;
        if let Some(owner) = &self.owner {
            owner.stop();
        }
        self.cancel_subscription.take();
        self.asset_guard.take();
        self.drawio_document_loss.take();
        self.drawio_document.take();
        self.drawio_guard.take();
        if let Some(window) = self.window.take() {
            // the native registry retires the browser when the window is destroyed
            if self.owner.is_some() {
                self.ui.post(Box::new(move || {
                    let _ = window.destroy();
                }));
            } else {
                on_ui(&self.ui, move || {
                    let _ = window.destroy();
                });
            }
        }
    }
}

impl Drop for PrintWindow {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// `webContents.print({ silent: true, deviceName, ... })` of any webview: the
/// page as PDF, with the print options read as `printToPDF`'s (page size,
/// landscape, margins, scale, background), then to the job's printer.
/// Blocks: call it off the UI thread. Also what the shared `Contents` host
/// does for a view's own silent print.
pub(crate) fn print_silently(webview: &tauri::Webview, options: &Value) -> Result<(), String> {
    let pdf = contents::print_pdf(webview, silent_pdf_spec(options))?;
    spool(&pdf, options)
}

/// `print`'s options as the PDF spec of a silent job; `print` draws backgrounds unless told not to.
fn silent_pdf_spec(options: &Value) -> PdfSpec {
    let mut spec = pdf_spec(&ipc_value(&print_as_pdf_options(options)));
    spec.print_background = options["printBackground"] != false;
    spec
}

/// One script evaluation in `webview`'s page, under the owner's authority when there is one.
fn evaluate_in(
    owner: Option<&owned::Owner>,
    webview: &tauri::Webview,
    js: &str,
    await_promise: bool,
) -> Result<Value, PageError> {
    match owner {
        Some(owner) => owner.evaluate(webview, js, await_promise),
        None => contents::evaluate(webview, js, await_promise),
    }
}

/// [`print_silently`] with the options as a handler passed them to
/// `Contents::print`
pub(crate) fn print_silently_ipc(
    webview: &tauri::Webview,
    options: &IpcValue,
) -> Result<(), String> {
    print_silently(webview, &json_of(options))
}

fn json_of(value: &IpcValue) -> Value {
    match value {
        IpcValue::Bool(b) => json!(b),
        IpcValue::Number(n) => serde_json::Number::from_f64(*n).map_or(Value::Null, Value::Number),
        IpcValue::String(s) => json!(s),
        IpcValue::Array(items) => Value::Array(items.iter().map(json_of).collect()),
        IpcValue::Object(entries) => Value::Object(
            entries
                .iter()
                .filter(|(_, v)| !matches!(v, IpcValue::Undefined))
                .map(|(k, v)| (k.clone(), json_of(v)))
                .collect(),
        ),
        IpcValue::Undefined | IpcValue::Null | IpcValue::Bytes(..) | IpcValue::Date(_) => {
            Value::Null
        }
    }
}

fn failed(reason: impl Into<String>) -> PrintReport {
    PrintReport {
        success: false,
        failure_reason: reason.into(),
    }
}

/// `print`'s options as `printToPDF` names them: Electron's `pageSize` of a
/// print is microns (`{ width, height }`) or a paper name, `printToPDF`'s inches
fn print_as_pdf_options(options: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in ["landscape", "scale", "pageRanges", "preferCSSPageSize"] {
        if let Some(value) = options.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(factor) = options["scaleFactor"].as_f64() {
        out.insert("scale".into(), json!(factor / 100.0));
    }
    match &options["pageSize"] {
        Value::String(name) => {
            out.insert("pageSize".into(), json!(name));
        }
        Value::Object(size) => {
            if let (Some(w), Some(h)) = (size["width"].as_f64(), size["height"].as_f64()) {
                out.insert(
                    "pageSize".into(),
                    json!({ "width": w / 25400.0, "height": h / 25400.0 }),
                );
            }
        }
        _ => {}
    }
    if options["margins"]["marginType"] == "none" {
        out.insert(
            "margins".into(),
            json!({ "top": 0, "bottom": 0, "left": 0, "right": 0 }),
        );
    } else if options["margins"]["marginType"] == "custom" {
        // print's custom margins are CSS pixels, printToPDF's inches
        let margins = &options["margins"];
        let inches = |key: &str| margins[key].as_f64().unwrap_or(0.0) / 96.0;
        out.insert(
            "margins".into(),
            json!({ "top": inches("top"), "bottom": inches("bottom"), "left": inches("left"), "right": inches("right") }),
        );
    }
    Value::Object(out)
}

/// A PDF to the job's printer, without a dialog
fn spool(pdf: &[u8], options: &Value) -> Result<(), String> {
    #[cfg(windows)]
    {
        windows::print_pdf(pdf, &silent_job(options), "SeuOffice")
    }
    #[cfg(not(windows))]
    {
        let job = cups_job(options);
        let result = main_core::packages::electron_utils::print_cups::print_pdf_via_cups(
            pdf,
            &job,
            &mut |file, args, stdin| run_command(file, args, stdin),
        );
        if result.ok {
            Ok(())
        } else {
            Err(result.error.unwrap_or_else(|| "lp failed".into()))
        }
    }
}

fn duplex_of(options: &Value) -> Option<DuplexMode> {
    match options["duplexMode"].as_str()? {
        "simplex" => Some(DuplexMode::Simplex),
        "longEdge" => Some(DuplexMode::LongEdge),
        "shortEdge" => Some(DuplexMode::ShortEdge),
        _ => None,
    }
}

fn device_name(options: &Value) -> Option<String> {
    options["deviceName"]
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

#[cfg_attr(windows, allow(dead_code))]
fn cups_job(options: &Value) -> CupsPrintJob {
    CupsPrintJob {
        device_name: device_name(options),
        copies: options["copies"].as_f64(),
        collate: options["collate"].as_bool(),
        duplex_mode: duplex_of(options),
        driver_options: options["driverOptions"].as_array().map(|list| {
            list.iter()
                .filter_map(|o| {
                    Some(DriverOption {
                        name: o["name"].as_str()?.to_owned(),
                        value: o["value"].as_str()?.to_owned(),
                    })
                })
                .collect()
        }),
    }
}

#[cfg(any(windows, test))]
fn silent_job(options: &Value) -> windows::SilentJob {
    windows::SilentJob {
        device_name: device_name(options),
        copies: options["copies"].as_f64().map_or(1, |c| c.max(1.0) as u32),
        collate: options["collate"].as_bool().unwrap_or(false),
        duplex: duplex_of(options).map(|d| match d {
            DuplexMode::Simplex => windows::Duplex::Simplex,
            DuplexMode::LongEdge => windows::Duplex::LongEdge,
            DuplexMode::ShortEdge => windows::Duplex::ShortEdge,
        }),
        landscape: options["landscape"].as_bool().unwrap_or(false),
        color: options["color"].as_bool(),
        page_ranges: options["pageRanges"]
            .as_array()
            .map(|ranges| {
                ranges
                    .iter()
                    .filter_map(|r| Some((r["from"].as_u64()? as u32, r["to"].as_u64()? as u32)))
                    .collect()
            })
            .unwrap_or_default(),
        ..windows::SilentJob::default()
    }
}

/// JSON as the IPC value `pdf_spec` reads
fn ipc_value(value: &Value) -> IpcValue {
    match value {
        Value::Null => IpcValue::Null,
        Value::Bool(b) => IpcValue::Bool(*b),
        Value::Number(n) => IpcValue::Number(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => IpcValue::String(s.clone()),
        Value::Array(items) => IpcValue::Array(items.iter().map(ipc_value).collect()),
        Value::Object(map) => {
            IpcValue::Object(map.iter().map(|(k, v)| (k.clone(), ipc_value(v))).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_print_page_cannot_navigate_to_another_document_or_reload() {
        let blank = Url::parse("about:blank").unwrap();
        let root = Url::parse("html-asset://capability/_document").unwrap();
        let other = Url::parse("file:///private/secret.html").unwrap();
        let mut policy = DocumentNavigation::default();
        assert!(policy.allow(&blank));
        assert!(!policy.allow(&other));
        policy.target = Some(root.clone());
        assert!(policy.allow(&root));
        assert!(!policy.allow(&root));
        assert!(!policy.allow(&blank));
        assert!(!policy.allow(&other));
    }

    #[test]
    fn diagram_print_reservation_permits_only_its_single_root_navigation() {
        let root = Url::parse("drawio-app://editor/__ieum/print/0123456789abcdef0123456789abcdef")
            .unwrap();
        let other = Url::parse("drawio-app://editor/__ieum/print/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap();
        let mut policy = DocumentNavigation {
            target: Some(root.clone()),
            ..DocumentNavigation::default()
        };
        assert!(!policy.allow(&other));
        assert!(policy.allow(&root));
        assert!(!policy.allow(&root));
        assert!(!policy.allow(&Url::parse("about:blank").unwrap()));
    }

    #[cfg(not(windows))]
    #[test]
    fn printer_info_reads_a_quoted_or_bare_value() {
        assert_eq!(
            printer_info("copies=1 printer-info='Office Laser' printer-is-shared=false").as_deref(),
            Some("Office Laser")
        );
        assert_eq!(
            printer_info("printer-info=Home device-uri=ipp://x").as_deref(),
            Some("Home")
        );
        assert_eq!(printer_info("copies=1"), None);
    }

    #[test]
    fn print_options_become_pdf_options() {
        let options = json!({
            "silent": true,
            "pageSize": { "width": 210000, "height": 297000 },
            "margins": { "marginType": "custom", "top": 96, "bottom": 48, "left": 0, "right": 0 },
            "scaleFactor": 150,
            "landscape": true,
        });
        let pdf = print_as_pdf_options(&options);
        assert_eq!(pdf["landscape"], true);
        assert_eq!(pdf["scale"], 1.5);
        assert!((pdf["pageSize"]["width"].as_f64().unwrap() - 8.2677).abs() < 1e-3);
        assert_eq!(pdf["margins"]["top"], 1.0);
        assert_eq!(pdf["margins"]["bottom"], 0.5);
        let none =
            print_as_pdf_options(&json!({ "margins": { "marginType": "none" }, "pageSize": "A4" }));
        assert_eq!(none["margins"]["left"], 0);
        assert_eq!(none["pageSize"], "A4");
    }

    #[test]
    fn a_silent_job_reads_its_printer_and_copies() {
        let options = json!({
            "deviceName": "office", "copies": 3, "collate": true, "duplexMode": "longEdge",
            "driverOptions": [{ "name": "ColorModel", "value": "Gray" }, { "name": 1 }],
        });
        let job = cups_job(&options);
        assert_eq!(job.device_name.as_deref(), Some("office"));
        assert_eq!(job.copies, Some(3.0));
        assert_eq!(job.duplex_mode, Some(DuplexMode::LongEdge));
        assert_eq!(job.driver_options.unwrap().len(), 1);
        assert_eq!(cups_job(&json!({ "deviceName": "" })).device_name, None);
        let windows = silent_job(&json!({ "copies": 0, "pageRanges": [{ "from": 2, "to": 3 }] }));
        assert_eq!(windows.copies, 1);
        assert_eq!(windows.page_ranges, vec![(2, 3)]);
    }
}
