//! Native platform and shell services of the presentation editor.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use crate::native::{DocumentContext, EditCommand, NativeEventKind, WebviewExt, WindowExt};
use clipboard_rs::common::RustImage;
use clipboard_rs::{Clipboard as _, ClipboardContent, ClipboardContext, RustImageData};
use main_core::packages::electron_utils::generate_handoff::GenerateHandoffTarget;
use main_core::packages::electron_utils::print_window::{
    JsError, RendererAnswer, RendererCall, WindowStep,
};
use main_core::slides::pdf_export::{
    self, ExportPdfLink, ExportPdfPage, ExportSlidesPdfOptions, PdfExportWindow,
};
use shell_contract::hosts::{DialogsHost, MessageBox, OwnedEffectsHost};
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelToken, RendererGeneration, Views};
use shell_contract::{IpcValue, Surface, ViewId, WindowId};
use shell_ipc_d1::slides::engine::{
    Display, EngineError, HeadlessExportTarget, PdfExportAnswer, PreparedOpen,
    PresenterWindowAction, RenderOwner, SlidesShell, SlidesShellHost, TakenHandoff,
};
use shell_ipc_m::hosts::{PrintProfile, PrintSurface, PrintWindowSpec};
use shell_ipc_w::tabs::TabsState;
use tauri::Manager;
#[cfg(not(windows))]
use tauri::{LogicalPosition, LogicalSize};

use crate::host::window::{Bridge, ViewSpec, WindowOptions, WindowSpec};
use crate::host::{
    HostContext, on_ui,
    w::{on_core, post_core},
};

mod display;
#[cfg(target_os = "macos")]
mod fullscreen_move;
use super::shells;
use crate::ipc::native_owner;

type HeadlessDone = Box<dyn FnOnce(Result<(), String>) + Send>;
struct Headless {
    target: Option<String>,
    generation: RendererGeneration,
    invocation: shell_contract::ipc::NativeInvocationTarget,
    origin: Option<DocumentContext>,
    window: tauri::Window,
    done: HeadlessDone,
    timer: mpsc::Sender<()>,
    losses: Vec<shell_contract::runtime::CancelSubscription>,
}

impl Headless {
    fn owned_by(&self, owner: &RenderOwner) -> bool {
        owner
            .context
            .renderer()
            .is_some_and(|generation| generation.same_generation(&self.generation))
            && owner
                .context
                .authority_ref::<native_owner::NativeOwnerAuthority>()
                .is_some_and(|proof| self.origin.as_ref() == Some(&proof.origin))
    }
}

#[derive(Clone)]
struct Shell {
    cx: HostContext,
    dialogs: DialogsHost,
    passwords: Arc<crate::host::document_password::DocumentPasswordService>,
    dialog_memory: shell_ipc_w::SharedDialogMemory,
    save_dir: shell_ipc_w::SharedSaveDirState,
    effects: OwnedEffectsHost,
    headless: Arc<Mutex<HashMap<ViewId, Headless>>>,
}

struct Runtime(Arc<Shell>);

pub(super) fn install(hosts: &mut shell_contract::Hosts, cx: &HostContext) -> Result<(), String> {
    let dialogs = hosts
        .get::<DialogsHost>()
        .cloned()
        .ok_or("Native dialogs must be installed before Slides")?;
    let dialog_memory = hosts
        .get::<shell_ipc_w::SharedDialogMemory>()
        .cloned()
        .ok_or("The shared dialog memory is unavailable")?;
    let save_dir = hosts
        .get::<shell_ipc_w::SharedSaveDirState>()
        .cloned()
        .ok_or("The shared default-save directory is unavailable")?;
    let passwords = hosts
        .get::<Arc<crate::host::document_password::DocumentPasswordService>>()
        .cloned()
        .ok_or("The shared Office password service is unavailable")?;
    let effects = hosts
        .get::<OwnedEffectsHost>()
        .cloned()
        .ok_or("The native owned effect host is unavailable")?;
    let shell = Arc::new(Shell {
        cx: cx.clone(),
        dialogs,
        dialog_memory,
        save_dir,
        passwords,
        effects,
        headless: Arc::default(),
    });
    if !cx.app.manage(Runtime(shell.clone())) {
        return Err("The native Slides shell was already installed".into());
    }
    let host: SlidesShellHost = shell;
    hosts.insert(host);
    Ok(())
}

fn finish_headless(
    cx: &HostContext,
    jobs: &Arc<Mutex<HashMap<ViewId, Headless>>>,
    view: ViewId,
    generation: &RendererGeneration,
    result: Result<(), String>,
) {
    let job = {
        let mut jobs = jobs.lock().unwrap_or_else(|error| error.into_inner());
        if !jobs
            .get(&view)
            .is_some_and(|job| job.generation.same_generation(generation))
        {
            return;
        }
        jobs.remove(&view)
    };
    finish_headless_job(cx, job, result);
}

fn finish_headless_job(cx: &HostContext, job: Option<Headless>, result: Result<(), String>) {
    if let Some(job) = job {
        let _ = job.timer.send(());
        cx.ui.post(Box::new(move || {
            let closed = job.window.close().map_err(|error| error.to_string());
            (job.done)(result.and(closed));
        }));
    }
}

fn terminate_headless(shell: &Shell, view: ViewId, generation: &RendererGeneration) {
    let job = {
        let mut jobs = shell
            .headless
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !jobs
            .get(&view)
            .is_some_and(|job| job.generation.same_generation(generation))
        {
            return;
        }
        jobs.remove(&view)
    };
    finish_headless_job(&shell.cx, job, Err("The Slides renderer stopped".into()));
}

fn register_headless(
    shell: &Shell,
    view: ViewId,
    expected: &RendererGeneration,
    origin: &DocumentContext,
) {
    let (generation, invocation) = {
        let jobs = shell
            .headless
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(job) = jobs.get(&view) else { return };
        if !job.generation.same_generation(expected) {
            return;
        }
        if let Some(first) = &job.origin {
            if first != origin {
                drop(jobs);
                finish_headless(
                    &shell.cx,
                    &shell.headless,
                    view,
                    expected,
                    Err("The hidden Slides document was replaced".into()),
                );
            }
            return;
        }
        (job.generation.clone(), job.invocation.clone())
    };
    let owner = match native_owner::capture_owner(&shell.cx.views, view, &generation) {
        Ok(owner)
            if owner
                .context
                .authority_ref::<native_owner::NativeOwnerAuthority>()
                .is_some_and(|proof| proof.origin == *origin) =>
        {
            owner
        }
        Ok(_) => {
            return finish_headless(
                &shell.cx,
                &shell.headless,
                view,
                expected,
                Err("The hidden Slides registration has a different native origin".into()),
            );
        }
        Err(error) => {
            return finish_headless(&shell.cx, &shell.headless, view, expected, Err(error));
        }
    };
    let registered = native_owner::register_native_invocation_target(&invocation, owner.clone());
    if let Err(error) = registered {
        return finish_headless(&shell.cx, &shell.headless, view, expected, Err(error));
    }
    let Some(loss) = owner.loss_token() else {
        return finish_headless(
            &shell.cx,
            &shell.headless,
            view,
            expected,
            Err("The hidden Slides owner lease is unavailable".into()),
        );
    };
    let jobs = Arc::downgrade(&shell.headless);
    let cx = shell.cx.clone();
    let expected = expected.clone();
    let subscription = loss.on_cancel(move || {
        if let Some(jobs) = jobs.upgrade() {
            finish_headless(
                &cx,
                &jobs,
                view,
                &expected,
                Err("The hidden Slides owner expired".into()),
            );
        }
    });
    let mut jobs = shell
        .headless
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(job) = jobs
        .get_mut(&view)
        .filter(|job| job.generation.same_generation(&generation))
    {
        job.origin = Some(origin.clone());
        job.losses.push(subscription);
    }
}

/// Native CLI ingress. The renderer uses its normal Slides open and PDF routes.
pub(crate) fn export_headless(
    cx: &HostContext,
    input: &str,
    output: &str,
    _format: &str,
    _request: &main_core::packages::electron_utils::headless_export::HeadlessExportRequest,
    operation: &shell_contract::ipc::NativeInvocationOperation,
    done: Box<
        dyn FnOnce(Result<Option<main_core::shell::headless_export::HeadlessExportProduct>, String>)
            + Send,
    >,
) -> Result<(), String> {
    let selection = shell_contract::ipc::NativeInvocationSelection::HeadlessExport {
        surface: Surface::Slides,
        input: PathBuf::from(input),
        output: PathBuf::from(output),
        format: _format.to_owned(),
        page: _request.page,
        ops: _request.ops.clone(),
        batch: _request.batch.map(str::to_owned),
    };
    if operation.selection() != &selection {
        return Err("The selected native Slides export does not match this invocation".into());
    }
    export_headless_pdf(
        cx,
        input.to_owned(),
        output.to_owned(),
        Duration::from_millis(300_000),
        operation.clone(),
        Box::new(move |answer| done(answer.map(|()| None))),
    )
}

fn export_headless_pdf(
    cx: &HostContext,
    input: String,
    output: String,
    timeout: Duration,
    operation: shell_contract::ipc::NativeInvocationOperation,
    done: HeadlessDone,
) -> Result<(), String> {
    let shell = cx
        .app
        .try_state::<Runtime>()
        .ok_or("The native Slides shell is unavailable")?
        .0
        .clone();
    let ui = cx.ui.clone();
    ui.post(Box::new(move || {
        let frames = shell.clone();
        let closed = shell.clone();
        let identity = Arc::new(OnceLock::<(ViewId, RendererGeneration)>::new());
        let closed_identity = identity.clone();
        let frames_identity = identity.clone();
        let options = WindowOptions {
            background_throttling: Some(tauri::utils::config::BackgroundThrottlingPolicy::Disabled),
            on_frame: Some(Arc::new(move |view, event| {
                let Some((created_view, generation)) = frames_identity.get() else {
                    return;
                };
                if *created_view != view {
                    return;
                }
                match &event.kind {
                    NativeEventKind::DocumentRegistered { context: origin }
                        if origin.browser().is_same_browser(&event.browser) =>
                    {
                        register_headless(&frames, view, generation, origin)
                    }
                    NativeEventKind::DocumentRevoked { document, .. } => {
                        let matches = frames
                            .headless
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .get(&view)
                            .is_some_and(|job| {
                                job.origin
                                    .as_ref()
                                    .is_some_and(|origin| origin.identity() == document)
                            });
                        if matches {
                            finish_headless(
                                &frames.cx,
                                &frames.headless,
                                view,
                                generation,
                                Err("The hidden Slides renderer context was revoked".into()),
                            );
                        }
                    }
                    NativeEventKind::WebviewProcessFailed { .. } => {
                        terminate_headless(&frames, view, generation)
                    }
                    NativeEventKind::NavigationFailed { .. } => finish_headless(
                        &frames.cx,
                        &frames.headless,
                        view,
                        generation,
                        Err("The hidden Slides document could not load".into()),
                    ),
                    _ => {}
                }
            })),
            on_closed: Some(Box::new(move |_| {
                if let Some((view, generation)) = closed_identity.get() {
                    finish_headless(
                        &closed.cx,
                        &closed.headless,
                        *view,
                        generation,
                        Err("The hidden Slides window closed before export completed".into()),
                    );
                }
            })),
            ..Default::default()
        };
        let spec = WindowSpec {
            title: "SeuOffice Slides".into(),
            size: (1280.0, 840.0),
            min_size: None,
            visible: false,
            decorations: false,
            transparent: false,
            resizable: true,
            parent: None,
            focus: false,
            main: false,
            view: ViewSpec {
                url: "about:blank".into(),
                bridge: Bridge::Surface(Surface::Slides),
            },
        };
        let windows = shell.cx.windows.clone();
        let created =
            native_owner::run_native_invocation_ui(shell.cx.ui.as_ref(), &operation, move || {
                windows.open_deferred_window(spec, options)
            })
            .and_then(|result| result)
            .and_then(|(window, view)| {
                let prepare = (|| {
                    let generation = shell
                        .cx
                        .views
                        .renderer_generation(view)
                        .ok_or("The hidden Slides creation generation is unavailable")?;
                    let invocation = operation.reserve_target(view, generation.clone())?;
                    let native = native_window(&shell.cx, window)?;
                    let views = shell.cx.views.clone();
                    native_owner::run_native_invocation_created_ui(
                        shell.cx.ui.as_ref(),
                        &invocation,
                        move || views.prepare_slides_media(view),
                    )??;
                    Ok((view, generation, native, invocation))
                })();
                if prepare.is_err() {
                    log_error(
                        "closing a failed hidden Slides window",
                        shell.cx.windows.close_window(window),
                    );
                }
                prepare
            });
        let (view, generation, window, invocation) = match created {
            Ok(created) => created,
            Err(error) => return done(Err(error)),
        };
        let _ = identity.set((view, generation.clone()));
        let generation_loss = generation.loss_token();
        let (timer, wait) = mpsc::channel();
        shell
            .headless
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                view,
                Headless {
                    target: Some(output),
                    generation: generation.clone(),
                    invocation: invocation.clone(),
                    origin: None,
                    window,
                    done,
                    timer,
                    losses: Vec::new(),
                },
            );
        let jobs = Arc::downgrade(&shell.headless);
        let cx = shell.cx.clone();
        let loss_generation = generation.clone();
        let subscription = generation_loss.on_cancel(move || {
            if let Some(jobs) = jobs.upgrade() {
                finish_headless(
                    &cx,
                    &jobs,
                    view,
                    &loss_generation,
                    Err("The hidden Slides creation generation expired".into()),
                );
            }
        });
        if let Some(job) = shell
            .headless
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&view)
            .filter(|job| job.generation.same_generation(&generation))
        {
            job.losses.push(subscription);
        }
        for loss in [
            operation.cancel().clone(),
            operation.shutdown().clone(),
            operation.loss_token(),
        ] {
            let jobs = Arc::downgrade(&shell.headless);
            let cx = shell.cx.clone();
            let expected = generation.clone();
            let subscription = loss.on_cancel(move || {
                if let Some(jobs) = jobs.upgrade() {
                    finish_headless(
                        &cx,
                        &jobs,
                        view,
                        &expected,
                        Err("The hidden Slides invocation was cancelled".into()),
                    );
                }
            });
            if let Some(job) = shell
                .headless
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get_mut(&view)
                .filter(|job| job.generation.same_generation(&generation))
            {
                job.losses.push(subscription);
            }
        }
        let jobs = Arc::downgrade(&shell.headless);
        let cx = shell.cx.clone();
        let watch_generation = generation.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("slides-headless-watchdog".into())
            .spawn(move || {
                if wait.recv_timeout(timeout).is_err() {
                    if let Some(jobs) = jobs.upgrade() {
                        finish_headless(
                            &cx,
                            &jobs,
                            view,
                            &watch_generation,
                            Err(format!(
                                "Slides export timed out after {}ms",
                                timeout.as_millis()
                            )),
                        );
                    }
                }
            })
        {
            return finish_headless(
                &shell.cx,
                &shell.headless,
                view,
                &generation,
                Err(error.to_string()),
            );
        }
        post_core(move |_, _| {
            let Some(slides) = shell_ipc_d1::slides::views() else {
                return finish_headless(
                    &shell.cx,
                    &shell.headless,
                    view,
                    &generation,
                    Err("The Slides open queue is unavailable".into()),
                );
            };
            if !shell
                .headless
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&view)
                .is_some_and(|job| job.generation.same_generation(&generation))
            {
                return;
            }
            if invocation
                .with_active(|| slides.queue_open(view, &input))
                .is_none()
            {
                return finish_headless(
                    &shell.cx,
                    &shell.headless,
                    view,
                    &generation,
                    Err("The hidden Slides invocation expired before opening".into()),
                );
            }
            let ui = shell.cx.ui.clone();
            ui.post(Box::new(move || {
                if !shell
                    .headless
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&view)
                    .is_some_and(|job| job.generation.same_generation(&generation))
                {
                    return;
                }
                let windows = shell.cx.windows.clone();
                let load = native_owner::run_native_invocation_created_ui(
                    shell.cx.ui.as_ref(),
                    &invocation,
                    move || windows.load_view(view, "genoffice-app://slides/index.html"),
                )
                .and_then(|result| result);
                if let Err(error) = load {
                    finish_headless(&shell.cx, &shell.headless, view, &generation, Err(error));
                }
            }));
        });
    }));
    Ok(())
}

fn log_error(action: &str, result: Result<(), String>) {
    if let Err(error) = result {
        eprintln!("[slides] {action}: {error}");
    }
}

fn native_window(cx: &HostContext, window: WindowId) -> Result<tauri::Window, String> {
    let label = cx
        .views
        .label_of_window(window)
        .ok_or("The presentation window is gone")?;
    cx.app
        .get_window(&label)
        .ok_or_else(|| "The presentation window is gone".into())
}

fn clipboard<T: Send + 'static>(
    cx: &HostContext,
    work: impl FnOnce(&ClipboardContext) -> clipboard_rs::Result<T> + Send + 'static,
) -> Result<T, String> {
    on_ui(&cx.ui, move || {
        let clipboard = ClipboardContext::new().map_err(|error| error.to_string())?;
        work(&clipboard).map_err(|error| error.to_string())
    })
}

fn display_by_id(id: i64) -> Result<display::NativeDisplay, String> {
    display::by_id(id)
}

fn display(native: &display::NativeDisplay, work_area: bool) -> Display {
    if work_area {
        native.work
    } else {
        native.bounds
    }
}

fn set_bounds(window: &tauri::Window, bounds: Display) -> Result<(), String> {
    #[cfg(windows)]
    {
        window
            .set_position(tauri::PhysicalPosition::new(bounds.x, bounds.y))
            .map_err(|error| error.to_string())?;
        window
            .set_size(tauri::PhysicalSize::new(bounds.width, bounds.height))
            .map_err(|error| error.to_string())
    }
    #[cfg(not(windows))]
    {
        window
            .set_position(LogicalPosition::new(bounds.x, bounds.y))
            .map_err(|error| error.to_string())?;
        window
            .set_size(LogicalSize::new(bounds.width, bounds.height))
            .map_err(|error| error.to_string())
    }
}

fn fullscreen_on(cx: &HostContext, window: WindowId, bounds: Display) -> Result<(), String> {
    let window = native_window(cx, window)?;
    fullscreen_window_on(&window, bounds)
}

fn fullscreen_window_on(window: &tauri::Window, bounds: Display) -> Result<(), String> {
    let bounds = display(&display_by_id(bounds.id)?, false);
    #[cfg(target_os = "macos")]
    {
        window
            .set_simple_fullscreen(false)
            .map_err(|error| error.to_string())?;
        set_bounds(&window, bounds)?;
        window
            .set_simple_fullscreen(true)
            .map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        if window.is_fullscreen().map_err(|error| error.to_string())? {
            window
                .set_fullscreen(false)
                .map_err(|error| error.to_string())?;
        }
        set_bounds(&window, bounds)?;
        window
            .set_fullscreen(true)
            .map_err(|error| error.to_string())
    }
}

fn move_presenter_native(native: &tauri::Window, target: Display) -> Result<(), String> {
    let bounds = display(&display_by_id(target.id)?, true);
    let fullscreen = native.is_fullscreen().map_err(|error| error.to_string())?;
    #[cfg(target_os = "macos")]
    let simple = !fullscreen && is_simple_fullscreen(native)?;
    if fullscreen {
        native
            .set_fullscreen(false)
            .map_err(|error| error.to_string())?;
    }
    #[cfg(target_os = "macos")]
    if simple {
        native
            .set_simple_fullscreen(false)
            .map_err(|error| error.to_string())?;
    }
    set_bounds(native, bounds)?;
    if fullscreen {
        // `set_bounds` already moved the window onto the target display.
        #[cfg(target_os = "macos")]
        native
            .set_fullscreen(true)
            .map_err(|error| error.to_string())?;
        #[cfg(windows)]
        native
            .set_fullscreen_on_monitor(tauri::PhysicalPosition::new(
                bounds.x + bounds.width / 2.0,
                bounds.y + bounds.height / 2.0,
            ))
            .map_err(|error| error.to_string())?;
    }
    #[cfg(target_os = "macos")]
    if simple {
        native
            .set_simple_fullscreen(true)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Tauri has no getter for simple fullscreen. Tao's mode hides the title bar and pins the
/// window (not movable); nothing else in the shell does both.
#[cfg(target_os = "macos")]
fn is_simple_fullscreen(window: &tauri::Window) -> Result<bool, String> {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSWindow, NSWindowStyleMask};
    let handle = window
        .native_window_handle()
        .map_err(|error| error.to_string())?;
    handle
        .with_window(|_| -> Result<bool, String> {
            let pointer = window.ns_window().map_err(|error| error.to_string())?;
            // SAFETY: the original window's native effect guard is held on the UI thread.
            let native = unsafe { Retained::retain(pointer.cast::<NSWindow>()) }
                .ok_or("The native presentation window is unavailable")?;
            Ok(!native.isMovable() && !native.styleMask().contains(NSWindowStyleMask::Titled))
        })
        .map_err(|error| error.to_string())?
}

fn prepare_presenter_window(
    cx: &HostContext,
    owner: &RenderOwner,
    expected: WindowId,
) -> Result<tauri::Window, String> {
    let generation = cx
        .views
        .renderer_generation(owner.view)
        .ok_or("The presentation renderer is gone")?;
    if !owner
        .context
        .renderer()
        .is_some_and(|captured| captured.same_generation(&generation))
    {
        return Err(
            "The presentation renderer generation changed before native preparation".into(),
        );
    }
    let sender = cx
        .views
        .sender_of(owner.view)
        .ok_or("The presentation window is gone")?;
    if sender.window != expected {
        return Err("The presentation view moved out of its captured window".into());
    }
    native_window(cx, expected)
}

fn run_presenter_action(
    cx: HostContext,
    origin: RenderOwner,
    destination: RenderOwner,
    cancel: CancelToken,
    pending: CancelToken,
    action: PresenterWindowAction,
    done: Box<dyn FnOnce(Result<(), String>) + Send>,
) {
    let presenter_capture = match &action {
        PresenterWindowAction::SwapDisplays { presenter, .. } => {
            let capture = (|| {
                let generation = origin
                    .context
                    .renderer()
                    .ok_or("The presentation renderer is gone")?;
                let (window, lifecycle) = cx.views.view_window_captured(origin.view, generation)?;
                if window != *presenter {
                    return Err("The presentation view moved out of its captured window".to_owned());
                }
                let native = lifecycle.window_handle()?;
                let permit = lifecycle.admit_native_effect()?;
                Ok((native, lifecycle, permit))
            })();
            match capture {
                Ok(capture) => Some(capture),
                Err(error) => {
                    post_core(move |_, _| done(Err(error)));
                    return;
                }
            }
        }
        _ => None,
    };
    let ui = cx.ui.clone();
    ui.post(Box::new(move || {
        let complete: Box<dyn FnOnce(Result<(), String>) + Send> =
            Box::new(move |answer| post_core(move |_, _| done(answer)));
        match action {
            PresenterWindowAction::ShowFullscreen { window, on } => {
                let answer = (|| {
                    let native = prepare_presenter_window(&cx, &destination, window)?;
                    let view = if on {
                        Some(
                            cx.windows
                                .webview(destination.view)
                                .ok_or("The presentation native webview is gone")?,
                        )
                    } else {
                        None
                    };
                    native_owner::run_accepted_ui_pair(
                        cx.ui.as_ref(),
                        &origin,
                        &destination,
                        &cancel,
                        &pending,
                        || crate::host::w_tabs::set_html_simple_fullscreen_window(&cx, &native, on),
                    )??;
                    if let Some(view) = view {
                        native_owner::run_accepted_ui_pair(
                            cx.ui.as_ref(),
                            &origin,
                            &destination,
                            &cancel,
                            &pending,
                            || view.set_focus().map_err(|error| error.to_string()),
                        )??;
                    }
                    Ok(())
                })();
                complete(answer);
            }
            PresenterWindowAction::Refocus { window } => {
                let answer = (|| {
                    let _native = prepare_presenter_window(&cx, &destination, window)?;
                    let view = cx
                        .windows
                        .webview(destination.view)
                        .ok_or("The presentation native webview is gone")?;
                    native_owner::run_accepted_ui_pair(
                        cx.ui.as_ref(),
                        &origin,
                        &destination,
                        &cancel,
                        &pending,
                        || view.set_focus().map_err(|error| error.to_string()),
                    )?
                })();
                complete(answer);
            }
            PresenterWindowAction::SwapDisplays {
                presenter: _,
                audience,
                presenter_display,
                audience_display,
            } => {
                let (presenter, lifecycle, permit) = match presenter_capture {
                    Some(capture) => capture,
                    None => {
                        return complete(Err(
                            "The original presentation window was not captured".into()
                        ));
                    }
                };
                #[cfg(not(target_os = "macos"))]
                let _ = &lifecycle;
                if !permit.is_current() {
                    return complete(Err("The original presentation window is closing".into()));
                }
                let prepared = (|| {
                    let audience = prepare_presenter_window(&cx, &destination, audience)?;
                    display_by_id(presenter_display.id)?;
                    display_by_id(audience_display.id)?;
                    native_owner::run_accepted_ui_pair(
                        cx.ui.as_ref(),
                        &origin,
                        &destination,
                        &cancel,
                        &pending,
                        || fullscreen_window_on(&audience, presenter_display),
                    )??;
                    Ok(())
                })();
                if let Err(error) = prepared {
                    return complete(Err(error));
                }
                #[cfg(target_os = "macos")]
                match if permit.is_current() {
                    fullscreen_move::is_native_fullscreen(&presenter)
                } else {
                    Err("The original presentation window is closing".into())
                } {
                    Ok(true) => {
                        return fullscreen_move::begin(
                            cx,
                            origin,
                            destination,
                            cancel,
                            pending,
                            presenter,
                            lifecycle,
                            audience_display,
                            complete,
                        );
                    }
                    Ok(false) => {}
                    Err(error) => return complete(Err(error)),
                }
                if !permit.is_current() {
                    return complete(Err("The original presentation window is closing".into()));
                }
                let answer = native_owner::run_accepted_ui_pair(
                    cx.ui.as_ref(),
                    &origin,
                    &destination,
                    &cancel,
                    &pending,
                    || {
                        if permit.is_current() {
                            move_presenter_native(&presenter, audience_display)
                        } else {
                            Err("The original presentation window is closing".into())
                        }
                    },
                )
                .and_then(|answer| answer);
                complete(answer);
            }
        }
    }));
}

fn field<'a>(value: &'a IpcValue, key: &str) -> &'a IpcValue {
    static UNDEFINED: IpcValue = IpcValue::Undefined;
    match value {
        IpcValue::Object(fields) => fields.get(key).unwrap_or(&UNDEFINED),
        _ => &UNDEFINED,
    }
}
fn number(value: &IpcValue) -> f64 {
    if let IpcValue::Number(value) = value {
        *value
    } else {
        f64::NAN
    }
}
fn text(value: &IpcValue) -> Option<&str> {
    if let IpcValue::String(value) = value {
        Some(value)
    } else {
        None
    }
}

struct PdfRequest<'a> {
    pages: Vec<ExportPdfPage>,
    links: Option<Vec<Vec<ExportPdfLink>>>,
    path: &'a str,
    width: f64,
    height: f64,
    font_css: Option<&'a str>,
}

impl<'a> PdfRequest<'a> {
    fn parse(op: &'a IpcValue) -> Result<Self, String> {
        let pages = match field(op, "pages") {
            IpcValue::Array(pages) => pages
                .iter()
                .map(|page| {
                    if let Some(svg) = text(field(page, "svg")) {
                        Ok(ExportPdfPage::Svg(svg.to_owned()))
                    } else if let Some(png) = text(field(page, "png")) {
                        Ok(ExportPdfPage::Png(png.to_owned()))
                    } else {
                        Err("A presentation PDF page has no SVG or PNG".to_owned())
                    }
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("Presentation PDF pages are required".into()),
        };
        let links = if let IpcValue::Array(pages) = field(op, "links") {
            Some(
                pages
                    .iter()
                    .map(|page| {
                        if let IpcValue::Array(links) = page {
                            links
                                .iter()
                                .map(|link| ExportPdfLink {
                                    x: number(field(link, "x")),
                                    y: number(field(link, "y")),
                                    w: number(field(link, "w")),
                                    h: number(field(link, "h")),
                                    href: text(field(link, "href")).unwrap_or("").to_owned(),
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    })
                    .collect::<Vec<Vec<_>>>(),
            )
        } else {
            None
        };
        Ok(Self {
            pages,
            links,
            path: text(field(op, "filePath")).ok_or("Presentation PDF destination is required")?,
            width: number(field(op, "widthPx")),
            height: number(field(op, "heightPx")),
            font_css: text(field(op, "fontCss")),
        })
    }
}

struct PdfWindow(Box<dyn PrintSurface>);
impl PdfExportWindow for PdfWindow {
    fn destroy(&mut self) -> Result<(), JsError> {
        self.0.destroy();
        Ok(())
    }
}

enum PdfAuthority<'a> {
    Renderer {
        origin: &'a NativeRequestOwner,
        destination: &'a NativeRequestOwner,
        participants: &'a [NativeRequestOwner],
        invocation: Option<&'a shell_contract::ipc::NativeInvocationDocument>,
    },
    Existing {
        destination: &'a shell_contract::ipc::NativeSaveDestination,
        participants: &'a [NativeRequestOwner],
    },
}

fn render_pdf(
    cx: &HostContext,
    op: &IpcValue,
    authority: PdfAuthority<'_>,
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
) -> PdfExportAnswer {
    let outcome = (|| -> Result<PdfExportAnswer, String> {
        if cx.ui.is_current() || crate::host::w::on_core_local(|_| {}) {
            return Err("Slides PDF rendering must run on a worker".into());
        }
        let request = PdfRequest::parse(op)?;
        let timeout_ms = 300_000.0;
        let stop = CancelToken::default();
        let mut losses = Vec::new();
        let mut watch = |loss: CancelToken| {
            let stop = stop.clone();
            losses.push(loss.on_cancel(move || stop.cancel()));
        };
        watch(cancel.clone());
        watch(pending.clone());
        for loss in additional {
            watch(loss.clone());
        }
        match &authority {
            PdfAuthority::Renderer {
                origin,
                destination,
                participants,
                invocation,
            } => {
                watch(
                    origin
                        .loss_token()
                        .ok_or("Presentation PDF source lease is unavailable")?,
                );
                watch(
                    destination
                        .loss_token()
                        .ok_or("Presentation PDF target lease is unavailable")?,
                );
                for owner in participants.iter() {
                    watch(
                        owner
                            .loss_token()
                            .ok_or("Presentation PDF participant lease is unavailable")?,
                    );
                }
                if let Some(document) = invocation {
                    let operation = document.operation();
                    for loss in [
                        operation.loss_token(),
                        operation.cancel().clone(),
                        operation.shutdown().clone(),
                    ] {
                        watch(loss);
                    }
                }
            }
            PdfAuthority::Existing {
                destination,
                participants,
            } => {
                let operation = destination.binding().operation();
                for loss in [
                    operation.loss_token(),
                    operation.cancel().clone(),
                    operation.shutdown().clone(),
                ] {
                    watch(loss);
                }
                for owner in participants
                    .iter()
                    .chain(std::iter::once(destination.binding().owner()))
                {
                    watch(
                        owner
                            .loss_token()
                            .ok_or("Presentation PDF owner lease is unavailable")?,
                    );
                }
            }
        }
        let (timer_tx, timer_rx) = mpsc::channel();
        let deadline = stop.clone();
        let timer = std::thread::Builder::new()
            .name("slides-pdf-watchdog".into())
            .spawn(move || {
                if timer_rx
                    .recv_timeout(Duration::from_millis(timeout_ms as u64))
                    .is_err()
                {
                    deadline.cancel();
                }
            })
            .map_err(|error| error.to_string())?;
        let result = (|| {
            let spec = PrintWindowSpec {
                profile: PrintProfile::Scripted,
                width: None,
                height: None,
                parent: None,
                dialog_chrome: false,
            };
            let mut print_losses = additional.to_vec();
            print_losses.push(stop.clone());
            let surface = match &authority {
                PdfAuthority::Renderer {
                    origin,
                    destination: _,
                    participants,
                    invocation: Some(document),
                } => crate::host::print::window_native_invocation_many_extra(
                    cx,
                    spec,
                    document,
                    origin,
                    participants,
                    cancel,
                    pending,
                    &print_losses,
                )?,
                PdfAuthority::Renderer {
                    origin,
                    destination,
                    participants,
                    invocation: None,
                } => crate::host::print::window_owned_many_extra(
                    cx,
                    spec,
                    origin,
                    destination,
                    participants,
                    cancel,
                    pending,
                    &print_losses,
                )?,
                PdfAuthority::Existing {
                    destination,
                    participants,
                } => crate::host::print::window_existing_document(
                    cx,
                    spec,
                    destination.binding(),
                    participants,
                    cancel,
                    pending,
                    &print_losses,
                )?,
            };
            let temp = std::env::temp_dir();
            let temp = temp
                .to_str()
                .ok_or("The temporary directory is not a Unicode path")?;
            let options = ExportSlidesPdfOptions {
                pages: &request.pages,
                width_px: request.width,
                height_px: request.height,
                file_path: request.path,
                links: request.links.as_deref(),
                font_css: request.font_css,
                timeout_ms,
                tmp_dir: temp,
            };
            let mut step = pdf_export::begin_export_slides_pdf(&options, || Ok(PdfWindow(surface)));
            let started = Instant::now();
            let result = loop {
                match step {
                    WindowStep::Done(result) => break result.map_err(|error| error.message),
                    WindowStep::Await { call, mut job } => {
                        let answer = if stop.is_cancelled()
                            || started.elapsed() >= Duration::from_millis(timeout_ms as u64)
                        {
                            RendererAnswer::TimedOut
                        } else {
                            match call {
                                RendererCall::LoadFile { path } => RendererAnswer::Loaded(
                                    job.window().0.load_file(&path).map_err(JsError::plain),
                                ),
                                RendererCall::ExecuteJavaScript {
                                    script,
                                    user_gesture,
                                    race_ms,
                                } => RendererAnswer::Executed(job.window().0.execute_javascript(
                                    script,
                                    user_gesture,
                                    race_ms,
                                )),
                                RendererCall::PrintToPdf { options } => RendererAnswer::Pdf(
                                    job.window()
                                        .0
                                        .print_to_pdf(&options)
                                        .map_err(JsError::plain),
                                ),
                                RendererCall::Print { options } => {
                                    RendererAnswer::Printed(Ok(job.window().0.print(&options)))
                                }
                            }
                        };
                        let answer = if stop.is_cancelled() {
                            RendererAnswer::TimedOut
                        } else {
                            answer
                        };
                        // The caller supplied a private staging path. Its owned
                        // final publication and reveal occur only after this job.
                        step = job.resume(answer, &mut |_| Ok(()));
                    }
                }
            };
            result
        })();
        let _ = timer_tx.send(());
        let _ = timer.join();
        drop(losses);
        let result = result?;
        Ok(PdfExportAnswer {
            ok: result.ok,
            path: result.path,
            error: result.error,
        })
    })();
    outcome.unwrap_or_else(|error| PdfExportAnswer {
        ok: false,
        path: None,
        error: Some(error),
    })
}

#[cfg(target_os = "macos")]
fn microphone(done: Box<dyn FnOnce(Result<bool, String>) + Send>) {
    use objc2::{
        msg_send,
        runtime::{AnyClass, Bool},
    };
    use objc2_foundation::NSString;
    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVMediaTypeAudio: *const NSString;
    }
    let Some(device) = AnyClass::get(c"AVCaptureDevice") else {
        return done(Err("AVCaptureDevice is unavailable".into()));
    };
    // SAFETY: AVFoundation provides this process-lifetime NSString constant.
    let media = unsafe { &*AVMediaTypeAudio };
    // SAFETY: the selectors and scalar ABI are AVFoundation's public AVCaptureDevice API.
    let status: isize = unsafe { msg_send![device, authorizationStatusForMediaType: media] };
    if status == 3 {
        return done(Ok(true));
    }
    let done = Mutex::new(Some(done));
    let completion = block2::RcBlock::new(move |granted: Bool| {
        let done = done
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(done) = done {
            done(Ok(granted.as_bool()));
        }
    });
    // SAFETY: AVCaptureDevice copies the block for the asynchronous permission answer.
    unsafe {
        let _: () =
            msg_send![device, requestAccessForMediaType: media, completionHandler: &*completion];
    }
}

#[cfg(not(target_os = "macos"))]
fn microphone(done: Box<dyn FnOnce(Result<bool, String>) + Send>) {
    done(Ok(true));
}

#[derive(Clone)]
enum OpenOwner {
    Renderer {
        source: RenderOwner,
        destination: RenderOwner,
        cancel: CancelToken,
        pending: CancelToken,
    },
    Prepared {
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
    },
}

enum ImportDestination {
    Renderer(PathBuf),
    Prepared(shell_contract::ipc::RecoveryPreparedDestination),
}

impl ImportDestination {
    fn path(&self) -> &Path {
        match self {
            Self::Renderer(path) => path,
            Self::Prepared(receipt) => receipt.path(),
        }
    }

    fn slides_pptx(self) -> Self {
        match self {
            Self::Renderer(path) => {
                let mut name = path.into_os_string();
                name.push(".pptx");
                Self::Renderer(PathBuf::from(name))
            }
            Self::Prepared(receipt) => Self::Prepared(receipt.for_slides_pptx()),
        }
    }

    fn admit(
        &self,
        owner: &OpenOwner,
        effect: &mut dyn FnMut(),
    ) -> Result<(), main_core::node_fs::NodeError> {
        let accepted = match self {
            Self::Renderer(_) => owner.effect(effect),
            Self::Prepared(receipt) => match owner {
                OpenOwner::Prepared { target, snapshot }
                    if receipt.target().same_binding(target)
                        && receipt.snapshot().same_snapshot(snapshot) =>
                {
                    native_owner::with_recovery_seed_snapshot(target, snapshot, effect)
                }
                _ => None,
            },
        };
        accepted.ok_or_else(|| main_core::node_fs::NodeError {
            code: None,
            message: "The presentation copy destination expired".into(),
        })
    }
}

impl OpenOwner {
    fn effect<R>(&self, effect: impl FnOnce() -> R) -> Option<R> {
        match self {
            Self::Renderer {
                source,
                destination,
                cancel,
                pending,
            } => native_owner::with_paired_effect(source, destination, cancel, pending, effect),
            Self::Prepared { target, snapshot } => {
                native_owner::with_recovery_seed_snapshot(target, snapshot, effect)
            }
        }
    }

    fn check(&self) -> Result<(), String> {
        self.effect(|| ())
            .ok_or_else(|| "The presentation open request expired".into())
    }

    fn loss_tokens(&self) -> Result<Vec<CancelToken>, String> {
        match self {
            Self::Renderer {
                source,
                destination,
                cancel,
                pending,
            } => Ok(vec![
                source
                    .loss_token()
                    .ok_or("The presentation source lease is unavailable")?,
                destination
                    .loss_token()
                    .ok_or("The presentation destination lease is unavailable")?,
                cancel.clone(),
                pending.clone(),
            ]),
            Self::Prepared { target, snapshot } => {
                use shell_contract::ipc::RecoveryOpenContext;
                let mut tokens = vec![
                    target.created().loss_token(),
                    snapshot.revision_loss_token(),
                ];
                match &snapshot.context {
                    RecoveryOpenContext::NativeStartup(operation) => tokens.extend([
                        operation.loss_token(),
                        operation.cancel().clone(),
                        operation.shutdown().clone(),
                    ]),
                    RecoveryOpenContext::Renderer {
                        source,
                        participants,
                        cancel,
                        pending,
                    } => {
                        tokens.push(
                            source
                                .loss_token()
                                .ok_or("The recovery source lease is unavailable")?,
                        );
                        for owner in participants {
                            tokens.push(
                                owner
                                    .loss_token()
                                    .ok_or("A recovery participant lease is unavailable")?,
                            );
                        }
                        tokens.extend([cancel.clone(), pending.clone()]);
                    }
                }
                if let Some(request) = &snapshot.request {
                    tokens.extend([request.cancel.clone(), request.pending.clone()]);
                    for owner in std::iter::once(&request.source).chain(&request.participants) {
                        tokens.push(
                            owner
                                .loss_token()
                                .ok_or("A recovery request participant is unavailable")?,
                        );
                    }
                }
                if let Some(startup) = &snapshot.startup {
                    tokens.extend([
                        startup.startup.loss_token(),
                        startup.startup.cancel().clone(),
                        startup.startup.shutdown().clone(),
                        startup.cancel.clone(),
                        startup.pending.clone(),
                    ]);
                    for owner in std::iter::once(&startup.shell).chain(&startup.participants) {
                        tokens.push(
                            owner
                                .loss_token()
                                .ok_or("A recovery startup participant is unavailable")?,
                        );
                    }
                }
                if let Some(command) = &snapshot.command {
                    tokens.extend([
                        command.command.loss_token(),
                        command.command.cancel().clone(),
                        command.command.shutdown().clone(),
                        command.cancel.clone(),
                        command.pending.clone(),
                    ]);
                    for owner in &command.participants {
                        tokens.push(
                            owner
                                .loss_token()
                                .ok_or("A recovery command participant is unavailable")?,
                        );
                    }
                }
                Ok(tokens)
            }
        }
    }

    fn message(
        &self,
        dialogs: &DialogsHost,
        parent: Option<WindowId>,
        options: MessageBox,
    ) -> Result<shell_contract::hosts::MessageBoxAnswer, String> {
        let (send, receive) = mpsc::sync_channel(1);
        let done = Box::new(move |answer| {
            let _ = send.send(answer);
        });
        match self {
            Self::Renderer {
                source,
                destination,
                cancel,
                pending,
            } => dialogs.message_owned(
                parent,
                options,
                source.clone(),
                destination.clone(),
                cancel.clone(),
                pending.clone(),
                done,
            ),
            Self::Prepared { target, snapshot } => {
                dialogs.message_prepared(parent, options, target.clone(), snapshot.clone(), done)
            }
        }
        let answer = receive
            .recv()
            .map_err(|_| "The presentation dialog was released")??;
        self.check()?;
        Ok(answer)
    }

    fn save(
        &self,
        dialogs: &DialogsHost,
        parent: Option<WindowId>,
        options: shell_contract::hosts::FileDialog,
    ) -> Result<Option<ImportDestination>, String> {
        let (send, receive) = mpsc::sync_channel(1);
        match self {
            Self::Renderer {
                source,
                destination,
                cancel,
                pending,
            } => dialogs.save_owned(
                parent,
                options,
                source.clone(),
                destination.clone(),
                cancel.clone(),
                pending.clone(),
                Box::new(move |answer| {
                    let _ = send.send(answer.map(|path| path.map(ImportDestination::Renderer)));
                }),
            ),
            Self::Prepared { target, snapshot } => dialogs.save_prepared(
                parent,
                options,
                target.clone(),
                snapshot.clone(),
                Box::new(move |answer| {
                    let _ = send.send(answer.map(|path| path.map(ImportDestination::Prepared)));
                }),
            ),
        }
        let answer = receive
            .recv()
            .map_err(|_| "The presentation save dialog was released")??;
        self.check()?;
        Ok(answer)
    }

    fn admit(&self, effect: &mut dyn FnMut()) -> Result<(), main_core::node_fs::NodeError> {
        self.effect(effect)
            .ok_or_else(|| main_core::node_fs::NodeError {
                code: None,
                message: "The presentation open request expired".into(),
            })
    }

    fn admit_default_directory(
        &self,
        effect: &mut dyn FnMut(),
    ) -> Result<(), main_core::node_fs::NodeError> {
        match self {
            Self::Renderer { .. } => self.admit(effect),
            Self::Prepared { target, snapshot } => {
                native_owner::run_recovery_prepare_action_snapshot(target, snapshot, effect)
                    .map_err(|message| main_core::node_fs::NodeError {
                        code: None,
                        message,
                    })
            }
        }
    }

    fn unlock(
        &self,
        service: &crate::host::document_password::DocumentPasswordService,
        parent: Option<WindowId>,
        title: &str,
        bytes: &[u8],
    ) -> Result<Option<(Vec<u8>, String)>, String> {
        self.check()?;
        let result = match self {
            Self::Renderer {
                source,
                destination,
                cancel,
                pending,
            } => {
                let prompt = service.create_owned(
                    parent,
                    title.to_owned(),
                    source.clone(),
                    destination.clone(),
                    cancel.clone(),
                    pending.clone(),
                )?;
                crate::host::document_password::unlock_office(prompt, bytes)
            }
            Self::Prepared { target, snapshot } => crate::host::document_password::unlock_prepared(
                service,
                target.clone(),
                snapshot,
                title.to_owned(),
                bytes,
            ),
        };
        self.check()?;
        result
    }
}

fn canonical_import_path(path: &Path) -> PathBuf {
    let absolute = PathBuf::from(jscompat::node_path::host::resolve(&[
        &path.to_string_lossy()
    ]));
    std::fs::canonicalize(&absolute).unwrap_or_else(|_| {
        match (absolute.parent(), absolute.file_name()) {
            (Some(parent), Some(name)) => std::fs::canonicalize(parent)
                .map(|parent| parent.join(name))
                .unwrap_or(absolute.clone()),
            _ => absolute.clone(),
        }
    })
}

fn same_import_path(left: &Path, right: &Path) -> bool {
    canonical_import_path(left).to_string_lossy().to_lowercase()
        == canonical_import_path(right)
            .to_string_lossy()
            .to_lowercase()
}

fn ui_lang(cx: &HostContext) -> main_core::packages::i18n::Lang {
    use shell_ipc_w::editor_files::UiLanguage;
    crate::host::w_files::ShellLanguage(cx.ui.clone()).lang()
}

enum ImportText {
    PptDetail,
    OdpDetail,
    KeepOriginal,
    PptFailed,
    OdpFailed,
    PptFailedDetail,
    OdpFailedDetail,
    Encrypted,
    EncryptedDetail,
    Legacy,
    LegacyDetail,
}
fn import_text(korean: bool, key: ImportText) -> &'static str {
    use ImportText::*;
    match (korean, key) {
        (false, PptDetail) => {
            "SeuOffice rebuilds the deck as editable text boxes, in the original slide order and size; shape geometry, images and text formatting are not carried over yet. The original file is never changed; the converted .pptx is a separate copy that later saves write to."
        }
        (true, PptDetail) => {
            "SeuOffice는 데크를 원본 슬라이드 순서와 크기대로 편집 가능한 텍스트 상자로 다시 만듭니다. 도형 위치, 이미지, 텍스트 서식은 아직 옮겨지지 않습니다. 원본 파일은 변경되지 않으며, 변환된 .pptx는 이후 저장이 기록되는 별도의 사본입니다."
        }
        (false, OdpDetail) => {
            "SeuOffice edits .pptx, not ODF. Each page becomes a slide with its text boxes and pictures placed at their original positions; masters, placeholder layouts, tables and shape styling are not preserved. The original file is never changed; the converted .pptx is a separate copy that later saves write to."
        }
        (true, OdpDetail) => {
            "SeuOffice는 ODF가 아니라 .pptx를 편집합니다. 각 페이지가 슬라이드가 되고 텍스트 상자와 이미지는 원래 위치에 배치됩니다. 마스터, 자리 표시자 레이아웃, 표, 도형 스타일은 유지되지 않습니다. 원본 파일은 변경되지 않으며, 변환된 .pptx는 이후 저장이 기록되는 독립적인 사본입니다."
        }
        (false, KeepOriginal) => {
            "Choose a different name to keep the original presentation unchanged."
        }
        (true, KeepOriginal) => "원본 프레젠테이션을 보존하려면 다른 이름을 선택하세요.",
        (false, PptFailed) => "Cannot convert this .ppt",
        (true, PptFailed) => "이 .ppt를 변환할 수 없습니다",
        (false, OdpFailed) => "Cannot convert this .odp",
        (true, OdpFailed) => "이 .odp를 변환할 수 없습니다",
        (false, PptFailedDetail) => {
            "The file may be damaged or not a valid PowerPoint 97-2003 presentation."
        }
        (true, PptFailedDetail) => {
            "파일이 손상되었거나 유효한 PowerPoint 97-2003 프레젠테이션이 아닐 수 있습니다."
        }
        (false, OdpFailedDetail) => "The file may be damaged or not a valid ODF presentation.",
        (true, OdpFailedDetail) => {
            "파일이 손상되었거나 유효한 ODF 프레젠테이션이 아닐 수 있습니다."
        }
        (false, Encrypted) => "Cannot open password-protected file",
        (true, Encrypted) => "암호로 보호된 파일을 열 수 없음",
        (false, EncryptedDetail) => {
            "This file uses an encryption method this app cannot unlock. Open it in PowerPoint, remove the password (File → Info → Protect Presentation), save, and try again."
        }
        (true, EncryptedDetail) => {
            "이 파일에 쓰인 암호 방식은 이 앱에서 풀 수 없습니다. PowerPoint에서 암호를 해제하고 저장한 후 다시 시도하세요."
        }
        (false, Legacy) => "Cannot open legacy .ppt",
        (true, Legacy) => ".ppt 형식을 열 수 없음",
        (false, LegacyDetail) => {
            "This is a PowerPoint 97-2003 (.ppt) binary file. Only .pptx is supported. Open it in PowerPoint/WPS/Keynote and \"Save As .pptx\", then try again."
        }
        (true, LegacyDetail) => {
            "PowerPoint 97-2003(.ppt) 이진 형식입니다. 이 앱은 .pptx만 지원합니다. PowerPoint 등에서 열어 \".pptx로 저장\" 후 다시 시도하세요."
        }
    }
}

fn import_message(korean: bool, extension: &str) -> String {
    if korean {
        format!("이 .{extension} 프레젠테이션을 변환할까요?")
    } else {
        format!("Convert this .{extension} presentation?")
    }
}

impl Shell {
    fn import_target(
        &self,
        owner: &OpenOwner,
        parent: Option<WindowId>,
        source: &Path,
        mut suggestion: PathBuf,
        korean: bool,
    ) -> Result<Option<ImportDestination>, String> {
        use jscompat::{JsObject, JsonValue};
        use main_core::packages::electron_utils::default_save_dir::configured_default_save_dir_authorized;
        use main_core::packages::electron_utils::dialog_memory::{
            FallbackDir, prepare_dialog_options, remember_save_path,
        };
        let before = self
            .save_dir
            .lock()
            .map_err(|_| "The shared save directory is unavailable")?
            .clone();
        let mut resolved = before.clone();
        let drafts = configured_default_save_dir_authorized(
            &crate::paths::ShellPaths::new(),
            &mut resolved,
            main_core::packages::pdf_structure::files::system_now_ms(),
            &mut |effect| owner.admit_default_directory(effect),
        )
        .map_err(|error| error.0)?;
        owner
            .effect(|| match self.save_dir.try_lock() {
                Ok(mut state) => {
                    state.publish_resolution(&before, resolved);
                    Ok(())
                }
                Err(std::sync::TryLockError::WouldBlock) => Ok(()),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    Err("The shared save directory is unavailable")
                }
            })
            .ok_or("The presentation import request expired")??;
        loop {
            owner.check()?;
            let mut options = JsObject::new();
            options.set(
                "defaultPath",
                JsonValue::String(suggestion.to_string_lossy().into_owned()),
            );
            let prepared = {
                let memory = self
                    .dialog_memory
                    .lock()
                    .map_err(|_| "The shared dialog memory is unavailable")?;
                prepare_dialog_options(&memory, &options, Some(&FallbackDir::Dir(&drafts)))
            };
            let default_path = prepared.get("defaultPath").and_then(|value| {
                if let JsonValue::String(path) = value {
                    Some(PathBuf::from(path))
                } else {
                    None
                }
            });
            let picked = owner.save(
                &self.dialogs,
                parent,
                shell_contract::hosts::FileDialog {
                    default_path,
                    filters: vec![shell_contract::hosts::FileFilter {
                        name: "PowerPoint".into(),
                        extensions: vec!["pptx".into()],
                    }],
                    ..Default::default()
                },
            )?;
            let Some(picked) = picked else {
                return Ok(None);
            };
            let path = picked.path();
            owner
                .effect(|| {
                    let mut memory = self
                        .dialog_memory
                        .lock()
                        .map_err(|_| "The shared dialog memory is unavailable")?;
                    remember_save_path(&mut memory, &path.to_string_lossy());
                    Ok::<_, String>(())
                })
                .ok_or("The presentation import request expired")??;
            if same_import_path(source, &path) {
                owner.message(
                    &self.dialogs,
                    parent,
                    MessageBox {
                        kind: Some("error".into()),
                        message: "SeuOffice".into(),
                        detail: Some(import_text(korean, ImportText::KeepOriginal).into()),
                        ..Default::default()
                    },
                )?;
                continue;
            }
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pptx"))
            {
                return Ok(Some(picked));
            }
            let appended = picked.slides_pptx();
            if same_import_path(source, appended.path()) {
                owner.message(
                    &self.dialogs,
                    parent,
                    MessageBox {
                        kind: Some("error".into()),
                        message: "SeuOffice".into(),
                        detail: Some(import_text(korean, ImportText::KeepOriginal).into()),
                        ..Default::default()
                    },
                )?;
                continue;
            }
            if !appended.path().exists() {
                return Ok(Some(appended));
            }
            // The OS confirmed the entered name, not this appended destination.
            suggestion = appended.path().to_owned();
        }
    }
}

fn persist_import_skip(settings_path: String, owner: OpenOwner) -> Result<(), String> {
    let queue = main_core::shell::app_settings::queue::for_path(&settings_path);
    let (send, receive) = mpsc::sync_channel(1);
    queue.submit(move |completion| {
        let result = (|| {
            use jscompat::{JsObject, JsonValue, json_parse};
            use main_core::packages::electron_utils::legacy_import_prefs::{
                LEGACY_IMPORT_KINDS, LEGACY_IMPORT_SKIP_KEY,
            };
            owner.check()?;
            let mut settings = match std::fs::read(&settings_path)
                .ok()
                .and_then(|bytes| json_parse(&String::from_utf8_lossy(&bytes)).ok())
            {
                Some(JsonValue::Object(settings)) => settings,
                _ => JsObject::new(),
            };
            let mut map = JsObject::new();
            if let Some(JsonValue::Object(raw)) = settings.get(LEGACY_IMPORT_SKIP_KEY) {
                for kind in LEGACY_IMPORT_KINDS {
                    if raw.get(kind) == Some(&JsonValue::Bool(true)) {
                        map.set(kind, JsonValue::Bool(true));
                    }
                }
            }
            map.set("odp", JsonValue::Bool(true));
            settings.set(LEGACY_IMPORT_SKIP_KEY, JsonValue::Object(map));
            main_core::packages::electron_utils::atomic_write::write_json_atomic_authorized(
                &settings_path,
                &JsonValue::Object(settings),
                &mut |effect| owner.admit(effect),
            )
            .map_err(|error| error.message)
        })();
        drop(completion);
        let _ = send.send(result);
    });
    receive
        .recv()
        .map_err(|_| "The import preference writer was released".to_owned())?
}

impl Shell {
    fn prepare_worker(
        &self,
        parent: Option<WindowId>,
        input: &str,
        owner: &OpenOwner,
    ) -> Result<PreparedOpen, String> {
        owner.check()?;
        let korean = ui_lang(&self.cx) == main_core::packages::i18n::Lang::Ko;
        let source = Path::new(input);
        let extension = source
            .extension()
            .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let legacy = extension == "ppt";
        let mut path = input.to_owned();
        if legacy || matches!(extension.as_str(), "odp" | "otp") {
            const PPT_LIMIT: u64 = 256 * 1024 * 1024;
            if legacy && std::fs::metadata(source).is_ok_and(|metadata| metadata.len() > PPT_LIMIT)
            {
                owner.message(
                    &self.dialogs,
                    parent,
                    MessageBox {
                        kind: Some("error".into()),
                        message: "SeuOffice".into(),
                        detail: Some(if korean {
                            "256MB 한도 초과".into()
                        } else {
                            "exceeds the 256MB limit".into()
                        }),
                        ..Default::default()
                    },
                )?;
                return Ok(PreparedOpen::Rejected);
            }
            let settings = crate::paths::ShellPaths::new()
                .settings_file()
                .to_string_lossy()
                .into_owned();
            if legacy || !main_core::packages::electron_utils::legacy_import_prefs::read_legacy_import_skip(&settings, "odp") {
                let answer = owner.message(&self.dialogs, parent, MessageBox {
                    kind: Some("warning".into()), message: import_message(korean, &extension),
                    detail: Some(import_text(korean, if legacy { ImportText::PptDetail } else { ImportText::OdpDetail }).into()),
                    buttons: if korean { vec!["변환 후 열기".into(), "취소".into()] } else { vec!["Convert and open".into(), "Cancel".into()] },
                    default_id: Some(0), cancel_id: Some(1), no_link: true,
                    checkbox_label: (!legacy).then(|| if korean { "다음부터 묻지 않고 변환".into() } else { "Convert without asking next time".into() }),
                    ..Default::default()
                })?;
                if answer.response != 0 { return Ok(PreparedOpen::Rejected); }
                if !legacy && answer.checkbox_checked {
                    if let Err(error) = persist_import_skip(settings, owner.clone()) {
                        owner.check()?;
                        eprintln!("[slides] cannot remember optional import confirmation: {error}");
                    }
                }
            }
            let stem = source
                .file_stem()
                .ok_or("The presentation source has no file name")?
                .to_string_lossy();
            let name = if legacy {
                format!(
                    "{stem} ({}).pptx",
                    if korean { "텍스트만" } else { "text only" }
                )
            } else {
                format!("{stem}.pptx")
            };
            let suggestion = source.parent().unwrap_or_else(|| Path::new("")).join(name);
            let Some(target) = self.import_target(owner, parent, source, suggestion, korean)?
            else {
                return Ok(PreparedOpen::Rejected);
            };
            let converted = (|| -> Result<Vec<String>, String> {
                owner.check()?;
                let bytes = std::fs::read(source).map_err(|error| error.to_string())?;
                owner.check()?;
                let stop = CancelToken::default();
                let mut losses = Vec::new();
                for loss in owner.loss_tokens()? {
                    let stop = stop.clone();
                    losses.push(loss.on_cancel(move || stop.cancel()));
                }
                let (bytes, warnings) =
                    main_core::slides::legacy_presentation::convert_presentation_cancellable(
                        &bytes,
                        legacy,
                        &|| stop.is_cancelled(),
                    )?;
                owner.check()?;
                if same_import_path(source, target.path()) {
                    return Err(import_text(korean, ImportText::KeepOriginal).into());
                }
                let path = target
                    .path()
                    .to_str()
                    .ok_or("The presentation copy destination is not a Unicode path")?;
                main_core::packages::electron_utils::atomic_write::atomic_write_file_authorized(
                    path,
                    &bytes,
                    &mut |effect| target.admit(owner, effect),
                )
                .map_err(|error| error.message)?;
                drop(losses);
                Ok(warnings)
            })();
            let warnings = match converted {
                Ok(warnings) => warnings,
                Err(error) => {
                    owner.check()?;
                    eprintln!("[slides] presentation import: {error}");
                    owner.message(
                        &self.dialogs,
                        parent,
                        MessageBox {
                            kind: Some("error".into()),
                            message: import_text(
                                korean,
                                if legacy {
                                    ImportText::PptFailed
                                } else {
                                    ImportText::OdpFailed
                                },
                            )
                            .into(),
                            detail: Some(
                                import_text(
                                    korean,
                                    if legacy {
                                        ImportText::PptFailedDetail
                                    } else {
                                        ImportText::OdpFailedDetail
                                    },
                                )
                                .into(),
                            ),
                            ..Default::default()
                        },
                    )?;
                    return Ok(PreparedOpen::Rejected);
                }
            };
            if !warnings.is_empty() {
                owner.message(
                    &self.dialogs,
                    parent,
                    MessageBox {
                        kind: Some("warning".into()),
                        message: import_message(korean, &extension),
                        detail: Some(warnings.join("\n")),
                        buttons: vec![if korean { "확인".into() } else { "OK".into() }],
                        ..Default::default()
                    },
                )?;
            }
            path = target
                .path()
                .to_str()
                .ok_or("The presentation copy path is not Unicode")?
                .to_owned();
        }
        owner.check()?;
        use std::io::Read;
        let mut header = [0; 8];
        let read = std::fs::File::open(&path).and_then(|mut file| file.read(&mut header));
        if read.is_err() || !main_core::slides::cfb_sniff::is_cfb_header(&header) {
            return Ok(PreparedOpen::Plain(path));
        }
        let bytes = std::fs::read(&path).ok();
        let kind = bytes
            .as_deref()
            .and_then(main_core::slides::cfb_sniff::cfb_kind);
        use main_core::slides::cfb_sniff::CfbKind;
        if matches!(kind, Some(CfbKind::Encrypted | CfbKind::Damaged)) && self.is_headless() {
            return Ok(PreparedOpen::Rejected);
        }
        if kind == Some(CfbKind::Damaged) {
            owner.message(
                &self.dialogs,
                parent,
                MessageBox {
                    kind: Some("error".into()),
                    message: "SeuOffice".into(),
                    detail: Some(import_text(korean, ImportText::PptFailedDetail).into()),
                    ..Default::default()
                },
            )?;
            return Ok(PreparedOpen::Rejected);
        }
        if kind == Some(CfbKind::Encrypted)
            && let Some(bytes) = bytes
        {
            let title = Path::new(&path)
                .file_name()
                .ok_or("The presentation path has no file name")?
                .to_string_lossy();
            match owner.unlock(&self.passwords, parent, &title, &bytes) {
                Ok(Some((bytes, password))) => {
                    return Ok(PreparedOpen::Unlocked {
                        path,
                        bytes,
                        password,
                    });
                }
                Ok(None) => return Ok(PreparedOpen::Rejected),
                Err(error) => {
                    owner.check()?;
                    eprintln!("[slides] cannot unlock presentation: {error}");
                }
            }
        }
        let encrypted = kind == Some(CfbKind::Encrypted);
        owner.message(
            &self.dialogs,
            parent,
            MessageBox {
                kind: Some("warning".into()),
                message: import_text(
                    korean,
                    if encrypted {
                        ImportText::Encrypted
                    } else {
                        ImportText::Legacy
                    },
                )
                .into(),
                detail: Some(
                    import_text(
                        korean,
                        if encrypted {
                            ImportText::EncryptedDetail
                        } else {
                            ImportText::LegacyDetail
                        },
                    )
                    .into(),
                ),
                buttons: vec![if korean { "확인".into() } else { "OK".into() }],
                ..Default::default()
            },
        )?;
        Ok(PreparedOpen::Rejected)
    }
}

type AudienceDone = Box<dyn FnOnce(Result<Option<(RenderOwner, WindowId)>, EngineError>) + Send>;
struct AudienceCreated {
    window: WindowId,
    view: ViewId,
    generation: RendererGeneration,
}
struct AudienceState {
    done: Option<AudienceDone>,
    owner: Option<RenderOwner>,
    ready: bool,
}
struct AudienceStart {
    cx: HostContext,
    source: RenderOwner,
    cancel: CancelToken,
    pending: CancelToken,
    created: OnceLock<AudienceCreated>,
    state: Mutex<AudienceState>,
    losses: Mutex<Vec<shell_contract::runtime::CancelSubscription>>,
    closing: std::sync::atomic::AtomicBool,
}

impl AudienceStart {
    fn fail(self: &Arc<Self>, message: String) {
        self.pending.cancel();
        let done = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .done
            .take();
        if let Some(done) = done {
            done(Err(EngineError(message)));
        }
        if !self.closing.swap(true, std::sync::atomic::Ordering::AcqRel) {
            if let Some(created) = self.created.get() {
                let window = created.window;
                let windows = self.cx.windows.clone();
                self.cx.ui.post(Box::new(move || {
                    log_error(
                        "closing an expired audience window",
                        windows.close_window(window),
                    )
                }));
            }
        }
    }

    fn closed(self: &Arc<Self>) {
        self.closing
            .store(true, std::sync::atomic::Ordering::Release);
        self.pending.cancel();
        let done = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .done
            .take();
        if let Some(done) = done {
            done(Err(EngineError(
                "The audience window closed before registration".into(),
            )));
        }
        if let Some(created) = self.created.get() {
            let view = created.view;
            post_core(move |_, _| shell_ipc_d1::slides::audience_closed(view));
        }
    }

    fn registered(self: &Arc<Self>, view: ViewId, origin: &DocumentContext) {
        let Some(created) = self.created.get() else {
            return;
        };
        if created.view != view {
            return;
        }
        let target = match native_owner::capture_owner(&self.cx.views, view, &created.generation) {
            Ok(target) => target,
            Err(error) => return self.fail(error),
        };
        if !target
            .context
            .authority_ref::<native_owner::NativeOwnerAuthority>()
            .is_some_and(|proof| proof.origin == *origin)
        {
            return self
                .fail("The audience registration does not match its created document".into());
        }
        {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(first) = &state.owner {
                let same = first
                    .context
                    .authority_ref::<native_owner::NativeOwnerAuthority>()
                    .is_some_and(|proof| proof.origin == *origin);
                drop(state);
                if !same {
                    self.fail("The audience document was replaced".into());
                }
                return;
            }
        }
        let accepted = native_owner::with_paired_effect(
            &self.source,
            &target,
            &self.cancel,
            &self.pending,
            || {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.owner = Some(target.clone());
                state.done.take()
            },
        );
        match accepted {
            Some(Some(done)) => {
                let Some(loss) = target.loss_token() else {
                    done(Err(EngineError(
                        "The audience owner lease is unavailable".into(),
                    )));
                    return self.fail("The audience owner lease is unavailable".into());
                };
                let weak = Arc::downgrade(self);
                let subscription = loss.on_cancel(move || {
                    if let Some(start) = weak.upgrade() {
                        start.fail("The audience owner expired".into());
                    }
                });
                self.losses
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(subscription);
                if self.pending.is_cancelled() {
                    done(Err(EngineError(
                        "The audience owner expired before registration completed".into(),
                    )));
                } else {
                    done(Ok(Some((target, created.window))));
                }
            }
            Some(None) => {}
            None => self.fail("The presenter request expired before audience registration".into()),
        }
    }

    fn ready(self: &Arc<Self>, origin: &DocumentContext, display: Display) {
        let owner = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(owner) = state.owner.clone() else {
                return;
            };
            if state.ready
                || !owner
                    .context
                    .authority_ref::<native_owner::NativeOwnerAuthority>()
                    .is_some_and(|proof| proof.origin == *origin)
            {
                return;
            }
            state.ready = true;
            owner
        };
        let Some(created) = self.created.get() else {
            return;
        };
        let result = native_owner::run_accepted_ui_pair(
            self.cx.ui.as_ref(),
            &self.source,
            &owner,
            &self.cancel,
            &self.pending,
            || {
                self.cx.windows.show(created.window, false)?;
                fullscreen_on(&self.cx, created.window, display)
            },
        )
        .and_then(|result| result);
        if let Err(error) = result {
            self.fail(error);
        }
    }
}

fn open_audience(
    cx: &HostContext,
    display: Display,
    source: RenderOwner,
    cancel: CancelToken,
    done: AudienceDone,
) {
    let start = Arc::new(AudienceStart {
        cx: cx.clone(),
        source,
        cancel,
        pending: CancelToken::default(),
        created: OnceLock::new(),
        state: Mutex::new(AudienceState {
            done: Some(done),
            owner: None,
            ready: false,
        }),
        losses: Mutex::new(Vec::new()),
        closing: std::sync::atomic::AtomicBool::new(false),
    });
    let Some(source_loss) = start.source.loss_token() else {
        return start.fail("The presenter owner lease is unavailable".into());
    };
    for loss in [source_loss, start.cancel.clone()] {
        let weak = Arc::downgrade(&start);
        let subscription = loss.on_cancel(move || {
            if let Some(start) = weak.upgrade() {
                start.fail("The presenter request was cancelled".into());
            }
        });
        start
            .losses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(subscription);
    }
    let ui = cx.ui.clone();
    ui.post(Box::new(move || {
        let result = native_owner::run_accepted_ui_pair(
            start.cx.ui.as_ref(),
            &start.source,
            &start.source,
            &start.cancel,
            &start.pending,
            || {
                let display = display(&display_by_id(display.id)?, false);
                let frames = start.clone();
                let closed = start.clone();
                let options = WindowOptions {
                    on_frame: Some(Arc::new(move |view, event| match &event.kind {
                        NativeEventKind::DocumentRegistered { context: origin }
                            if origin.browser().is_same_browser(&event.browser) =>
                        {
                            frames.registered(view, origin)
                        }
                        NativeEventKind::DocumentReady { context: origin }
                            if origin.browser().is_same_browser(&event.browser) =>
                        {
                            frames.ready(origin, display)
                        }
                        NativeEventKind::DocumentRevoked { document, .. } => {
                            let matches = frames
                                .state
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .owner
                                .as_ref()
                                .is_some_and(|owner| {
                                    owner
                                        .context
                                        .authority_ref::<native_owner::NativeOwnerAuthority>()
                                        .is_some_and(|proof| proof.origin.identity() == document)
                                });
                            if matches {
                                frames.fail("The audience renderer context was revoked".into());
                            }
                        }
                        NativeEventKind::WebviewProcessFailed { .. } => {
                            if frames
                                .created
                                .get()
                                .is_some_and(|created| created.view == view)
                            {
                                frames.fail("The audience renderer stopped".into());
                            }
                        }
                        NativeEventKind::NavigationFailed { .. } => {
                            frames.fail("The audience document could not load".into())
                        }
                        _ => {}
                    })),
                    on_closed: Some(Box::new(move |_| closed.closed())),
                    ..Default::default()
                };
                let spec = WindowSpec {
                    title: "SeuOffice Slides".into(),
                    size: (display.width, display.height),
                    min_size: None,
                    visible: false,
                    decorations: false,
                    transparent: false,
                    resizable: true,
                    parent: None,
                    focus: false,
                    main: false,
                    view: ViewSpec {
                        url: "about:blank".into(),
                        bridge: Bridge::Surface(Surface::Slides),
                    },
                };
                let (window, view) = start.cx.windows.open_deferred_window(spec, options)?;
                let generation = start
                    .cx
                    .views
                    .renderer_generation(view)
                    .ok_or("The audience creation generation is unavailable")?;
                if start
                    .created
                    .set(AudienceCreated {
                        window,
                        view,
                        generation,
                    })
                    .is_err()
                {
                    return Err("The audience native identity was already assigned".into());
                }
                if start.pending.is_cancelled() {
                    start.cx.windows.close_window(window)?;
                    return Err("The presenter request expired during audience construction".into());
                }
                start.cx.views.prepare_slides_media(view)?;
                let native = native_window(&start.cx, window)?;
                set_bounds(&native, display)?;
                native
                    .set_background_color(Some(tauri::utils::config::Color(0, 0, 0, 255)))
                    .map_err(|error| error.to_string())?;
                start
                    .cx
                    .windows
                    .load_view(view, "genoffice-app://slides/index.html?mode=audience")
            },
        )
        .and_then(|result| result);
        if let Err(error) = result {
            start.fail(error);
        }
    }));
}

fn spawn_answer<T: Send + 'static>(
    name: &str,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
    done: Box<dyn FnOnce(Result<T, String>) + Send>,
) {
    let done = Arc::new(Mutex::new(Some(done)));
    let worker_done = done.clone();
    let spawned = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err("The native presentation worker panicked".into()));
            let done = worker_done
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(done) = done {
                done(result);
            }
        });
    if let Err(error) = spawned {
        let done = done
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(done) = done {
            done(Err(error.to_string()));
        }
    }
}

impl SlidesShell for Shell {
    fn opened(&self, view: ViewId, path: &str) {
        let path = path.to_owned();
        post_core(move |_, _| shells::file_saved(view, &path, true));
    }

    fn file_saved_owned(
        &self,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        effects: &OwnedEffectsHost,
        path: PathBuf,
        participants: &[RenderOwner],
        additional: &[CancelToken],
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<PathBuf>, String>)>,
    ) {
        shells::file_saved_many_owned_extra(
            origin.clone(),
            destination.clone(),
            participants.to_vec(),
            cancel.clone(),
            pending.clone(),
            additional.to_vec(),
            effects.clone(),
            path,
            true,
            done,
        );
    }

    fn file_saved_existing_document(
        &self,
        revision: shell_contract::ipc::NativeSaveRevisionAuthority,
        expected_path: PathBuf,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
        on_revision: std::rc::Rc<
            dyn Fn(&shell_contract::ipc::NativeSaveRevisionReceipt) -> Result<(), String>,
        >,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<PathBuf>, String>)>,
    ) {
        shells::file_saved_existing_document(
            revision,
            expected_path,
            participants,
            cancel,
            pending,
            additional,
            effects,
            true,
            on_revision,
            done,
        );
    }

    fn set_window_title_existing_document(
        &self,
        destination: shell_contract::ipc::NativeSaveDestination,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
        title: String,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        let mut done = Some(done);
        let on_core = crate::host::w::on_core_local(|provided| {
            let done = done.take().expect("the title callback is taken once");
            let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
                return done(Err("The tab manager is unavailable".into()));
            };
            tabs.set_slides_document_title_existing_document(
                title,
                destination,
                participants,
                cancel,
                pending,
                additional,
                effects,
                done,
            );
        });
        if !on_core {
            if let Some(done) = done {
                done(Err(
                    "The presentation title must be changed on the core thread".into(),
                ));
            }
        }
    }

    fn set_window_title_owned(
        &self,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        title: String,
        effects: &OwnedEffectsHost,
        participants: &[RenderOwner],
        additional: &[CancelToken],
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let mut done = Some(done);
        let on_core = crate::host::w::on_core_local(|provided| {
            let done = done.take().expect("the title callback is taken once");
            let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
                return done(Err("The tab manager is unavailable".into()));
            };
            tabs.set_slides_document_title_owned_extra(
                title,
                origin.clone(),
                destination.clone(),
                participants.to_vec(),
                cancel.clone(),
                pending.clone(),
                additional.to_vec(),
                effects.clone(),
                done,
            );
        });
        if !on_core {
            if let Some(done) = done {
                done(Err(
                    "The presentation title must be changed on the core thread".into(),
                ));
            }
        }
    }

    fn prepare_reveal_export(
        &self,
        output: shell_contract::ipc::SavedPublication,
        written: Option<shell_contract::ipc::NativeSavedInput>,
        participants: Vec<RenderOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
        effects: OwnedEffectsHost,
        accepted: shell_contract::hosts::PdfReloadAccepted,
        wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<Box<dyn shell_contract::hosts::PreparedGeneratedPath>, String> {
        let mut result = None;
        let on_core = crate::host::w::on_core_local(|provided| {
            result = Some(match provided.all::<TabsState>().into_iter().next() {
                Some(tabs) => tabs
                    .prepare_generated_output(
                        output,
                        written,
                        "pdf",
                        participants,
                        cancel,
                        pending,
                        additional,
                        effects,
                        accepted,
                        wake,
                    )
                    .map(|prepared| {
                        Box::new(prepared) as Box<dyn shell_contract::hosts::PreparedGeneratedPath>
                    }),
                None => Err("The tab manager is unavailable".into()),
            });
        });
        if !on_core {
            return Err("The exported presentation must be opened on the core thread".into());
        }
        result
            .ok_or_else(|| "The tab manager did not prepare the exported presentation".to_owned())?
    }

    fn presenter_window_owned(
        &self,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        action: PresenterWindowAction,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let cx = self.cx.clone();
        let origin = origin.clone();
        let destination = destination.clone();
        let cancel = cancel.clone();
        let pending = pending.clone();
        let PresenterWindowAction::ShowFullscreen { on, .. } = action else {
            return run_presenter_action(cx, origin, destination, cancel, pending, action, done);
        };
        let mut done = Some(done);
        let on_core = crate::host::w::on_core_local(|provided| {
            let done = done.take().expect("the presenter callback is taken once");
            let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
                return done(Err("The tab manager is unavailable".into()));
            };
            tabs.set_content_bleed_owned(
                origin.clone(),
                destination.clone(),
                cancel.clone(),
                pending.clone(),
                self.effects.clone(),
                on,
                Box::new(move |answer| match answer {
                    Ok(()) => {
                        run_presenter_action(cx, origin, destination, cancel, pending, action, done)
                    }
                    Err(error) => done(Err(error)),
                }),
            );
        });
        if !on_core {
            if let Some(done) = done {
                done(Err(
                    "Presentation fullscreen must be changed on the core thread".into(),
                ));
            }
        }
    }

    fn reload_view(&self, view: ViewId) {
        post_core(move |_, _| shells::reload_view(view));
    }

    fn dialog_parent(&self) -> Option<WindowId> {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            for window in cx.app.windows().into_values() {
                match window.is_focused() {
                    Ok(true) => {
                        if let Some(id) = cx.views.window_of_window_label(window.label()) {
                            return Some(id);
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        eprintln!("[slides] cannot inspect a native dialog parent: {error}")
                    }
                }
            }
            cx.views.main().map(|sender| sender.window)
        })
    }

    fn host_window_for(&self, view: ViewId) -> Option<WindowId> {
        self.cx.views.sender_of(view).map(|sender| sender.window)
    }
    fn is_headless(&self) -> bool {
        std::env::args_os().any(|arg| arg == "--headless-export")
    }
    fn new_document_dir(&self, view: ViewId) -> Option<String> {
        shells::new_document_dir(view)
    }
    fn can_write_new_document(&self) -> bool {
        true
    }

    fn write_new_document_owned(
        &self,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        effects: &OwnedEffectsHost,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        name: &str,
        bytes: &[u8],
    ) -> Result<Option<String>, String> {
        crate::host::w_projects::write_new_document_owned(
            origin,
            destination,
            cancel,
            pending,
            effects,
            admission,
            name,
            bytes,
        )
    }

    fn is_path_open_elsewhere(&self, path: &str) -> Result<bool, EngineError> {
        let path = path.to_owned();
        on_core(move |provided| {
            let tabs = provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .ok_or_else(|| EngineError("The tab manager is unavailable".into()))?;
            Ok(tabs.is_path_open_elsewhere(&path))
        })
        .ok_or_else(|| EngineError("Core services are unavailable".into()))?
    }

    fn take_headless_target_owned(
        &self,
        owner: &RenderOwner,
        cancel: &CancelToken,
    ) -> Result<Option<HeadlessExportTarget>, String> {
        let invocation = {
            let jobs = self
                .headless
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(job) = jobs.get(&owner.view) else {
                return Ok(None);
            };
            job.invocation.clone()
        };
        let document = invocation.registered_document()?;
        native_owner::with_native_invocation_effect(&invocation, owner, cancel, cancel, || {
            let mut jobs = self
                .headless
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(job) = jobs.get_mut(&owner.view) else {
                return Ok(None);
            };
            if !job.owned_by(owner) || !job.generation.same_generation(invocation.created()) {
                return Err(
                    "The headless Slides target belongs to a different native document".into(),
                );
            }
            Ok(job.target.take().map(|path| HeadlessExportTarget {
                path,
                invocation: document,
            }))
        })
        .ok_or_else(|| "The headless Slides request expired".to_owned())?
    }

    fn headless_export_done_owned(
        &self,
        owner: &RenderOwner,
        cancel: &CancelToken,
        ok: bool,
        error: Option<String>,
    ) -> Result<(), String> {
        let invocation = {
            let jobs = self
                .headless
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(job) = jobs.get(&owner.view) else {
                return Ok(());
            };
            job.invocation.clone()
        };
        let generation =
            native_owner::with_native_invocation_effect(&invocation, owner, cancel, cancel, || {
                let jobs = self
                    .headless
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let Some(job) = jobs.get(&owner.view) else {
                    return Ok(None);
                };
                if !job.owned_by(owner) || !job.generation.same_generation(invocation.created()) {
                    return Err(
                        "The headless Slides completion belongs to a different native document"
                            .to_owned(),
                    );
                }
                Ok(Some(job.generation.clone()))
            })
            .ok_or("The headless Slides request expired")??;
        if let Some(generation) = generation {
            let result = if ok {
                Ok(())
            } else {
                Err(error.unwrap_or_else(|| "Slides export failed".into()))
            };
            finish_headless(&self.cx, &self.headless, owner.view, &generation, result);
        }
        Ok(())
    }

    fn read_clipboard_buffer(&self, format: &str) -> Result<Vec<u8>, EngineError> {
        let format = format.to_owned();
        clipboard(&self.cx, move |clipboard| {
            if !clipboard
                .available_formats()?
                .iter()
                .any(|available| available == &format)
            {
                return Ok(Vec::new());
            }
            clipboard.get_buffer(&format)
        })
        .map_err(EngineError)
    }

    fn write_clipboard_buffer(&self, format: &str, bytes: &[u8]) {
        let format = format.to_owned();
        let bytes = bytes.to_vec();
        log_error(
            "writing presentation clipboard data",
            clipboard(&self.cx, move |clipboard| {
                clipboard.set_buffer(&format, bytes)
            }),
        );
    }

    fn normalize_png(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        let image = image::load_from_memory(bytes).ok()?;
        let mut png = std::io::Cursor::new(Vec::new());
        match image.write_to(&mut png, image::ImageFormat::Png) {
            Ok(()) => Some(png.into_inner()),
            Err(error) => {
                eprintln!("[slides] cannot encode clipboard image: {error}");
                None
            }
        }
    }

    fn write_image_and_html(&self, png: &[u8], html: &str) {
        let png = png.to_vec();
        let html = html.to_owned();
        log_error(
            "writing presentation image clipboard",
            clipboard(&self.cx, move |clipboard| {
                clipboard.set(vec![
                    ClipboardContent::Image(RustImageData::from_bytes(&png)?),
                    ClipboardContent::Html(html),
                ])
            }),
        );
    }

    fn native_clipboard(&self, view: ViewId, op: &str) {
        let command = match op {
            "cut" => EditCommand::Cut,
            "copy" => EditCommand::Copy,
            "paste" => EditCommand::Paste,
            _ => return eprintln!("[slides] unknown native clipboard command: {op}"),
        };
        let Some(webview) = self.cx.windows.webview(view) else {
            return eprintln!("[slides] clipboard target is gone");
        };
        let page = match webview.page() {
            Ok(page) => page,
            Err(error) => return log_error("dispatching native clipboard command", Err(error)),
        };
        page.edit(
            command,
            Box::new(|result| {
                log_error(
                    "dispatching native clipboard command",
                    result.map_err(|error| error.to_string()),
                );
            }),
        );
    }

    fn prepare_for_open_owned(
        &self,
        parent: Option<WindowId>,
        path: &str,
        source: RenderOwner,
        destination: RenderOwner,
        cancel: CancelToken,
        pending: CancelToken,
        done: Box<dyn FnOnce(Result<PreparedOpen, String>) + Send>,
    ) {
        let shell = self.clone();
        let path = path.to_owned();
        let owner = OpenOwner::Renderer {
            source,
            destination,
            cancel,
            pending,
        };
        spawn_answer(
            "slides-prepare-open",
            move || shell.prepare_worker(parent, &path, &owner),
            done,
        );
    }

    fn prepare_for_recovery_open(
        &self,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        path: String,
        done: Box<dyn FnOnce(Result<PreparedOpen, String>) + Send>,
    ) {
        let shell = self.clone();
        spawn_answer(
            "slides-prepare-recovery",
            move || {
                let generation = shell
                    .cx
                    .views
                    .renderer_generation(target.view())
                    .ok_or("The recovery view is unavailable")?;
                if !generation.same_generation(target.created()) {
                    return Err("The recovery view generation changed".into());
                }
                let parent = shell
                    .cx
                    .views
                    .sender_of(target.view())
                    .ok_or("The recovery window is unavailable")?
                    .window;
                let owner = OpenOwner::Prepared { target, snapshot };
                shell.prepare_worker(Some(parent), &path, &owner)
            },
            done,
        );
    }

    fn unlock_recovery_copy_prepared(
        &self,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        title: String,
        bytes: Vec<u8>,
        done: Box<dyn FnOnce(Result<Option<(Vec<u8>, String)>, String>) + Send>,
    ) {
        let service = self.passwords.clone();
        spawn_answer(
            "slides-unlock-recovery",
            move || {
                crate::host::document_password::unlock_prepared(
                    &service, target, &snapshot, title, &bytes,
                )
            },
            done,
        );
    }

    fn unlock_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        title: String,
        done: Box<dyn FnOnce(Result<Option<(Vec<u8>, String)>, String>) + Send>,
    ) {
        let service = self.passwords.clone();
        spawn_answer(
            "slides-unlock-saved",
            move || crate::host::document_password::unlock_saved(&service, document, title),
            done,
        );
    }

    fn export_pdf_owned(
        &self,
        op: &IpcValue,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        participants: &[RenderOwner],
        additional: &[CancelToken],
        _effects: &OwnedEffectsHost,
    ) -> PdfExportAnswer {
        render_pdf(
            &self.cx,
            op,
            PdfAuthority::Renderer {
                origin,
                destination,
                participants,
                invocation: None,
            },
            cancel,
            pending,
            additional,
        )
    }

    fn export_pdf_native_invocation(
        &self,
        op: &IpcValue,
        document: &shell_contract::ipc::NativeInvocationDocument,
        origin: &RenderOwner,
        destination: &RenderOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        participants: &[RenderOwner],
        additional: &[CancelToken],
        _effects: &OwnedEffectsHost,
    ) -> PdfExportAnswer {
        render_pdf(
            &self.cx,
            op,
            PdfAuthority::Renderer {
                origin,
                destination,
                participants,
                invocation: Some(document),
            },
            cancel,
            pending,
            additional,
        )
    }

    fn export_pdf_existing_document(
        &self,
        op: &IpcValue,
        destination: &shell_contract::ipc::NativeSaveDestination,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        _effects: &OwnedEffectsHost,
    ) -> PdfExportAnswer {
        render_pdf(
            &self.cx,
            op,
            PdfAuthority::Existing {
                destination,
                participants,
            },
            cancel,
            pending,
            additional,
        )
    }

    fn ask_microphone(&self, done: Box<dyn FnOnce(Result<bool, String>) + Send>) {
        self.cx.ui.post(Box::new(move || microphone(done)));
    }

    fn displays(&self) -> Result<Vec<Display>, EngineError> {
        on_ui(&self.cx.ui, || {
            display::all()
                .map(|displays| displays.into_iter().map(|display| display.bounds).collect())
        })
        .map_err(EngineError)
    }

    fn display_of(&self, window: Option<WindowId>) -> Result<Display, EngineError> {
        let window = window
            .map(|id| {
                self.cx
                    .views
                    .window_lifecycle(id)
                    .ok_or("The presentation window is gone".to_owned())?
                    .window_handle()
            })
            .transpose()
            .map_err(EngineError)?;
        on_ui(&self.cx.ui, move || match window {
            Some(window) => {
                let handle = window
                    .native_window_handle()
                    .map_err(|error| error.to_string())?;
                handle
                    .with_window(|_| display::for_window(&window))
                    .map_err(|error| error.to_string())?
            }
            None => display::primary(),
        })
        .map_err(EngineError)
    }

    fn open_audience_window(
        &self,
        display: Display,
        source: RenderOwner,
        cancel: CancelToken,
        done: AudienceDone,
    ) {
        open_audience(&self.cx, display, source, cancel, done);
    }

    fn close_audience_window(&self, window: WindowId) {
        let cx = self.cx.clone();
        log_error(
            "closing audience window",
            on_ui(&self.cx.ui, move || {
                #[cfg(target_os = "macos")]
                {
                    let native = native_window(&cx, window)?;
                    native
                        .set_simple_fullscreen(false)
                        .map_err(|error| error.to_string())?;
                }
                cx.windows.close_window(window)
            }),
        );
    }

    fn set_window_title(&self, view: ViewId, title: &str) {
        let title = title.to_owned();
        let cx = self.cx.clone();
        let title_for_tabs = title.clone();
        post_core(move |_, _| shells::set_title(view, &title_for_tabs));
        log_error(
            "updating detached presentation title",
            on_ui(&self.cx.ui, move || {
                let sender = cx
                    .views
                    .sender_of(view)
                    .ok_or("The presentation view is gone")?;
                if cx
                    .views
                    .main()
                    .is_some_and(|main| main.window == sender.window)
                {
                    return Ok(());
                }
                native_window(&cx, sender.window)?
                    .set_title(&title)
                    .map_err(|error| error.to_string())
            }),
        );
    }

    fn take_generate_handoff_owned(
        &self,
        consumer: &RenderOwner,
        cancel: &CancelToken,
    ) -> Result<Option<TakenHandoff>, String> {
        shells::take_handoff(consumer, cancel, GenerateHandoffTarget::Slides).map(|handoff| {
            handoff.map(|handoff| TakenHandoff {
                id: handoff.id,
                text: handoff.text,
                materials: handoff
                    .materials
                    .into_iter()
                    .map(|material| (material.path, material.name))
                    .collect(),
            })
        })
    }
    fn report_generate_run_started_owned(
        &self,
        consumer: &RenderOwner,
        cancel: &CancelToken,
        paths: &[String],
    ) -> Result<bool, String> {
        shells::report_handoff(consumer, cancel, GenerateHandoffTarget::Slides, paths)
    }
    fn assert_file_saved(&self, path: String, then: Box<dyn FnOnce(Result<(), String>) + Send>) {
        super::save_guard::check(path, then);
    }
    fn random_uuid(&self) -> String {
        jscompat::random_uuid()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(fields: impl IntoIterator<Item = (&'static str, IpcValue)>) -> IpcValue {
        IpcValue::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }

    #[test]
    fn pdf_pages_keep_vector_raster_fonts_and_fractional_links() {
        let op = object([
            (
                "pages",
                IpcValue::Array(vec![
                    object([(
                        "svg",
                        IpcValue::String("<svg><text>한글</text></svg>".into()),
                    )]),
                    object([("png", IpcValue::String("iVBORw0KGgo=".into()))]),
                ]),
            ),
            ("filePath", IpcValue::String("/private/stage.pdf".into())),
            ("widthPx", IpcValue::Number(1280.5)),
            ("heightPx", IpcValue::Number(720.25)),
            (
                "fontCss",
                IpcValue::String("@font-face{font-family:Deck}".into()),
            ),
            (
                "links",
                IpcValue::Array(vec![IpcValue::Array(vec![
                    object([
                        ("x", IpcValue::Number(0.125)),
                        ("y", IpcValue::Number(0.25)),
                        ("w", IpcValue::Number(0.5)),
                        ("h", IpcValue::Number(0.125)),
                        (
                            "href",
                            IpcValue::String("https://example.test/?a=1&b=2".into()),
                        ),
                    ]),
                    object([
                        ("x", IpcValue::Number(f64::INFINITY)),
                        ("href", IpcValue::String("https://invalid.test".into())),
                    ]),
                ])]),
            ),
        ]);
        let request = PdfRequest::parse(&op).unwrap();
        assert_eq!(
            request.pages,
            vec![
                ExportPdfPage::Svg("<svg><text>한글</text></svg>".into()),
                ExportPdfPage::Png("iVBORw0KGgo=".into())
            ]
        );
        assert_eq!((request.width, request.height), (1280.5, 720.25));
        assert_eq!(request.path, "/private/stage.pdf");
        assert_eq!(request.font_css, Some("@font-face{font-family:Deck}"));
        let links = request.links.as_ref().unwrap();
        assert!(links[0][1].x.is_infinite());
        assert_eq!(
            pdf_export::build_pdf_link_overlays(Some(&links[0])),
            "<a href=\"https://example.test/?a=1&amp;b=2\" style=\"left:12.5%;top:25%;width:50%;height:12.5%\"></a>"
        );
    }

    #[test]
    fn missing_pdf_page_content_is_not_replaced_with_empty_output() {
        let op = object([
            ("pages", IpcValue::Array(vec![object([])])),
            ("filePath", IpcValue::String("/private/stage.pdf".into())),
        ]);
        assert!(PdfRequest::parse(&op).is_err());
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("slides-import-{}", jscompat::random_uuid()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn import_never_overwrites_the_original_through_dot_or_case_aliases() {
        let scratch = Scratch::new();
        let original = scratch.0.join("Source.PPT");
        std::fs::write(&original, b"original").unwrap();
        assert!(same_import_path(&original, &scratch.0.join("./Source.PPT")));
        assert!(same_import_path(&original, &scratch.0.join("source.ppt")));
        assert!(!same_import_path(&original, &scratch.0.join("Source.pptx")));
        assert_eq!(std::fs::read(original).unwrap(), b"original");
    }

    #[test]
    fn selected_recovery_copy_suffix_preserves_bound_target_and_parent() {
        use shell_contract::ipc::{
            NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision,
            RecoveryOpenContext, RecoveryOpenTarget, RecoveryPreparedDestination,
        };
        let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
            NativeInvocationSelection::RecoveryFile {
                surface: Surface::Slides,
                path: PathBuf::from("/source/deck.ppt"),
                decision: NativeRecoveryDecision::Original,
            },
            CancelToken::default(),
        );
        let generation = RendererGeneration::new();
        let (_issuer, reservation) =
            shell_contract::ipc::NativeClosureIssuer::new(ViewId(42), generation.clone());
        let target = RecoveryOpenTarget::bind(
            RecoveryOpenContext::NativeStartup(operation),
            ViewId(42),
            generation,
            reservation,
        )
        .unwrap();
        let snapshot = target.authority_snapshot().unwrap();
        let selected = RecoveryPreparedDestination::from_native_save_dialog(
            target.clone(),
            snapshot,
            PathBuf::from("/output/deck"),
        )
        .unwrap();
        let result = ImportDestination::Prepared(selected).slides_pptx();
        assert_eq!(result.path(), Path::new("/output/deck.pptx"));
        let ImportDestination::Prepared(receipt) = result else {
            panic!("the selected destination receipt must be retained")
        };
        assert!(receipt.target().same_binding(&target));
    }

    #[cfg(unix)]
    #[test]
    fn import_detects_parent_symlinks_and_appended_pptx_source_aliases() {
        let scratch = Scratch::new();
        let real = scratch.0.join("real");
        let alias = scratch.0.join("alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let original = real.join("source.pptx");
        std::fs::write(&original, b"original").unwrap();
        let entered = alias.join("source");
        let appended = PathBuf::from(format!("{}.pptx", entered.display()));
        assert!(same_import_path(&original, &appended));
        assert_eq!(
            canonical_import_path(&alias.join("new.pptx")),
            real.join("new.pptx")
        );
    }
}
