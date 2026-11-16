//! The stage 3 shell (decisions 0025, 0029, 0030; the stage 3 shell contract
//! under docs/seuoffice): Tauri 2 on the system webviews (WKWebView on macOS,
//! WebView2 on Windows), with `main-core` linked in this process.
//!
//! Three kinds of thread (the contract, section 3):
//!
//! - the UI thread runs the Tauri event loop; windows, menus, dialogs and
//!   the clipboard are touched there only;
//! - the core thread runs every handler in arrival order, and holds
//!   `main-core`'s state;
//! - workers run what TypeScript awaited and post the result back.
//!
//! At start the shell builds [`Services`], then on the core thread fills the
//! channel table from the six batches (`shell_core::table::build`) and stops
//! with the error printed when a batch is refused or a channel renderers use
//! has no handler. `SEUOFFICE_SHELL_PARTIAL=1` starts with the table as the
//! batches fill it, which is all there is while they are incomplete.
//!
//! `--measure-dialog <out.json>` runs the dialog measurement of the contract
//! (see `measure.rs`) instead of the shell window.

#![cfg_attr(
    all(not(debug_assertions), feature = "windows-gui"),
    windows_subsystem = "windows"
)]

mod app_scheme;
mod drawio_scheme;
mod engine_host;
mod headless;
mod host;
mod index_worker_process;
mod ipc;
mod measure;
mod native;
mod odt_worker;
mod paths;
mod preload;
#[cfg(target_os = "linux")]
mod signer_child_guard;
mod trace;
mod ui_scale;
mod views;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::thread::ThreadId;

use shell_contract::ipc::{EventSink, RegisterBatch};
use shell_contract::runtime::{Services, Thread, Views, Workers, run_on};
use shell_contract::{Hosts, Surface};
use shell_core::BATCHES;
use shell_core::core_thread::{CoreThread, ThreadWorkers};
use shell_core::dispatch::{CatchingWorkers, Dispatcher};
use shell_core::outlet::OutletSink;
use shell_core::table::{Completeness, StartError, build};
use tauri::{AppHandle, DragDropEvent, Manager, WebviewEvent, WindowEvent};

use crate::host::HostContext;
use crate::host::window::{Bridge, ViewSpec, WindowSpec, WindowOptions, Windows};
use crate::ipc::Ipc;
use crate::ipc::outlet::TauriOutlet;
use crate::measure::Measure;
use crate::paths::ShellPaths;
use crate::views::ViewTable;

/// `SEUOFFICE_SHELL_PARTIAL=1`: start with the channels the batches have registered
const PARTIAL_ENV: &str = "SEUOFFICE_SHELL_PARTIAL";

struct Options {
    /// `--measure-dialog <out.json>`
    measure: Option<PathBuf>,
    partial: bool,
    headless_output: Option<headless::Output>,
    invocation_shutdown: shell_contract::runtime::CancelToken,
}

impl Options {
    fn read() -> Result<Self,String> {
        let mut args = std::env::args().skip(1);
        let mut measure = None;
        while let Some(arg) = args.next() {
            if arg == "--measure-dialog" {
                measure = args.next().map(PathBuf::from);
            }
        }
        let headless_output = if headless::is_headless() {
            Some(Arc::new(std::sync::Mutex::new(Some(index_worker_process::isolate_protocol_stdout()?))))
        } else { None };
        Ok(Options {
            partial: std::env::var(PARTIAL_ENV).is_ok_and(|v| v == "1"),
            measure,
            headless_output,
            invocation_shutdown: shell_contract::runtime::CancelToken::default(),
        })
    }

    /// The measurement needs W's channels, not a complete table
    fn completeness(&self) -> Completeness {
        if self.partial || self.measure.is_some() {
            Completeness::AsRegistered
        } else {
            Completeness::Complete
        }
    }
}

/// The Tauri event loop as a [`Thread`]
struct TauriUi {
    app: AppHandle,
    main: ThreadId,
}

impl Thread for TauriUi {
    fn post(&self, job: Box<dyn FnOnce() + Send>) {
        if let Err(error) = self.app.run_on_main_thread(job) {
            eprintln!("shell: a job for the UI thread was not accepted: {error}");
        }
    }

    fn is_current(&self) -> bool {
        std::thread::current().id() == self.main
    }
}

/// Why the shell did not start, for a person
fn describe(error: &StartError) -> String {
    match error {
        StartError::Register(error) => {
            format!("a batch could not register its channels: {error:?}")
        }
        StartError::Missing(missing) => {
            let shown: Vec<String> = missing
                .iter()
                .take(12)
                .map(|(channel, kind)| format!("{channel} ({kind:?})"))
                .collect();
            format!(
                "{} channels that renderers use have no handler yet, e.g. {}{}. \
                 {PARTIAL_ENV}=1 starts with the channels that are registered.",
                missing.len(),
                shown.join(", "),
                if missing.len() > shown.len() {
                    ", …"
                } else {
                    ""
                },
            )
        }
    }
}

/// A start that failed: print it, leave a note in userData, tell the person, end
fn fail(app: &AppHandle, paths: &ShellPaths, quiet: bool, why: String) {
    use shell_contract::runtime::{AppPaths, PathName};
    eprintln!("shell: cannot start: {why}");
    let user_data = paths.path(PathName::UserData);
    if std::fs::create_dir_all(&user_data).is_ok() {
        let _ = std::fs::write(user_data.join("shell-start-error.txt"), format!("{why}\n"));
    }
    let app = app.clone();
    let shown = app.run_on_main_thread({
        let app = app.clone();
        move || {
            if !quiet {
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title("SeuOffice")
                    .set_description(&why)
                    .show();
            }
            host::quit::startup_failed();
            app.exit(1);
        }
    });
    if shown.is_err() {
        host::quit::startup_failed();
        app.exit(1);
    }
}

fn install_first_shell_startup(
    services: Services, context: HostContext, paths: Arc<ShellPaths>,
    shutdown: shell_contract::runtime::CancelToken,
    window: shell_contract::WindowId, owner: shell_contract::ipc::NativeRequestOwner,
) {
    host::w::on_core(move |provided| {
        use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
        use std::rc::Rc;
        if shutdown.is_cancelled() { return; }
        let app = context.app.clone();
        let result = (|| {
            let tabs = provided.all::<shell_ipc_w::tabs::TabsState>().into_iter().next()
                .ok_or("The first native Shell tab manager is unavailable.")?;
            let native = Arc::new(NativeInvocationIssuer::new(shutdown.clone()));
            let recovery_native = native.clone();
            let startup_owner = owner.clone();
            let startup_issuer: host::recovery_launch::StartupIssuer = Rc::new(move |selection, cancel| {
                let NativeInvocationSelection::StartupLaunch { paths, reopened_by_installer } = selection else {
                    return Err("The startup issuer cannot issue a recovery or CLI operation.".into());
                };
                Ok(native.issue_startup(paths, reopened_by_installer, startup_owner.clone(), cancel))
            });
            let launch_paths = host::os::take_startup_paths()?;
            let reopened = std::env::args().any(|arg| arg == "--updated");
            let startup = startup_issuer(NativeInvocationSelection::StartupLaunch {
                paths: launch_paths.into_iter().map(PathBuf::from).collect(),
                reopened_by_installer: reopened,
            }, shell_contract::runtime::CancelToken::default())?;
            let recovery_startup = startup.clone();
            let recovery_issuer: host::recovery_launch::RecoveryIssuer = Rc::new(move |selection, cancel| {
                recovery_native.issue_recovery_child(&recovery_startup, selection, cancel)
            });
            crate::ipc::native_owner::with_startup_operation_effect(
                &startup, &owner, &[], startup.cancel(), &shutdown,
                || tabs.initialize_first_shell_window(shell_ipc_w::tabs::host::ShellWindow { window, view: owner.view }),
            ).ok_or("The first native Shell registration was revoked.")??;
            tabs.install_spare_creator_authority(startup.clone())?;
            let failure_app = app.clone();
            let failure_paths = paths.clone();
            let failure_shutdown = shutdown.clone();
            host::recovery_launch::install_native_launch(
                services, provided, context, window, owner, startup,
                recovery_issuer, startup_issuer, Box::new(move |result| {
                    if let Err(error) = result {
                        if !failure_shutdown.is_cancelled() {
                            fail(&failure_app, &failure_paths, false, error);
                        }
                    }
                }),
            )
        })();
        if let Err(error) = result {
            if !shutdown.is_cancelled() { fail(&app, &paths, false, error); }
        }
    });
}

/// The first window: the shell window, or the measurement's test page
fn open_first_window(windows: &Windows, options: &Options, services: &Services,
    context: &HostContext, paths: &Arc<ShellPaths>) -> Result<(), String> {
    let (url, bridge, main, title) = if options.measure.is_some() {
        (
            "genoffice-app://shell/measure.html",
            Bridge::Measure,
            false,
            "SeuOffice IPC measurement",
        )
    } else {
        (
            "genoffice-app://shell/index.html",
            Bridge::Surface(Surface::Shell),
            true,
            "SeuOffice",
        )
    };
    let first = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let services = services.clone();
    let context = context.clone();
    let paths = paths.clone();
    let shutdown = options.invocation_shutdown.clone();
    type StartupFrame = Arc<dyn Fn(shell_contract::ViewId, &native::NativeEvent) + Send + Sync>;
    let startup_frame: Option<StartupFrame> = main.then(|| {
        Arc::new(move |view, event: &native::NativeEvent| {
            use std::sync::atomic::Ordering;
            let native::NativeEventKind::DocumentRegistered { context: origin } = &event.kind else { return; };
            if shutdown.is_cancelled() || !event.browser.is_same_browser(origin.browser()) { return; }
            let Some(sender) = context.views.sender_of(view) else { return; };
            let Some(generation) = context.views.renderer_generation(view) else { return; };
            let Some(captured) = context.views.capture_native_owner(view, &generation) else { return; };
            if sender.surface != Surface::Shell || captured.surface != Surface::Shell || captured.origin != *origin { return; }
            let Some(owner) = crate::ipc::native_owner::owner(view, captured.origin, captured.renderer, ()) else { return; };
            if first.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() { return; }
            install_first_shell_startup(services.clone(), context.clone(), paths.clone(), shutdown.clone(), sender.window, owner);
        }) as StartupFrame
    });
    windows
        .open_window_with_options(WindowSpec {
            title: title.into(),
            size: (1280.0, 860.0),
            min_size: None,
            visible: true,
            decorations: true,
            transparent: false,
            resizable: true,
            parent: None,
            focus: true,
            main,
            view: ViewSpec {
                url: url.into(),
                bridge,
            },
        }, WindowOptions { on_frame: startup_frame, ..WindowOptions::default() })
        .map(|_| ())
}

/// Builds the table on the core thread, then opens the first window. Off the
/// UI thread: registering may ask the UI thread for something, and a window
/// is made by the UI thread.
fn start(
    app: AppHandle,
    services: Services,
    windows: Arc<Windows>,
    paths: Arc<ShellPaths>,
    context: HostContext,
    options: Options,
) {
    let quiet = options.measure.is_some() || headless::is_headless() || std::env::var_os("CI").is_some();
    let failure_app = app.clone();
    let failure_paths = paths.clone();
    let output = options.headless_output.clone();
    let failure_output = output.clone();
    let failure_services = services.clone();
    let spawned = std::thread::Builder::new()
        .name("shell-start".into())
        .spawn(move || {
            let mcp_services = services.clone();
            let fail_here = |why: String| {
                if let Some(output) = &output {
                    headless::startup_failed(app.clone(),&mcp_services,output.clone(),why);
                } else {
                    fail(&app,&paths,quiet,why);
                }
            };
            let Some(engine) = app.try_state::<Arc<engine_host::EngineHost>>() else {
                fail_here("engine service was not installed".to_owned());
                return;
            };
            if let Err(error) = engine.wait_ready() {
                fail_here(error);
                return;
            }
            let completeness = options.completeness();
            let core = services.core.clone();
            // the shell provides what the batches need of each other on the core thread, first
            let batches: Vec<RegisterBatch> =
                std::iter::once(host::w::register_hooks as RegisterBatch)
                    .chain(BATCHES.iter().copied())
                    .collect();
            let built = run_on(core.as_ref(), move || {
                let table = build(&services, &batches, completeness)?;
                let handlers = table.len();
                ipc::install_dispatcher(Dispatcher::new(table));
                services.views.on_renderer_replaced(Box::new(ipc::renderer_replaced));
                Ok::<usize, StartError>(handlers)
            });
            match built {
                Ok(handlers) => {
                    eprintln!("shell: {handlers} handlers registered ({completeness:?})");
                    if let Some(output) = &output {
                        run_on(core.as_ref(), host::quit::startup_ready);
                        headless::start(context,mcp_services,output.clone(),options.invocation_shutdown);
                        return;
                    }
                    let Some(mcp) = mcp_services.hosts.get::<Arc<host::app_mcp::AppMcp>>() else {
                        fail_here("app MCP runtime was not installed".to_owned());
                        return;
                    };
                    if let Err(error) = mcp.start(&mcp_services) {
                        fail_here(error);
                        return;
                    }
                    if let Err(error) = open_first_window(&windows, &options, &mcp_services, &context, &paths) {
                        fail_here(error);
                    } else {
                        run_on(core.as_ref(), host::quit::startup_ready);
                    }
                }
                Err(error) => fail_here(describe(&error)),
            }
        });
    if let Err(error) = spawned {
        let message = format!("cannot start the start-up thread: {error}");
        if let Some(output) = failure_output {
            headless::startup_failed(failure_app,&failure_services,output,message);
        } else {
            fail(&failure_app,&failure_paths,quiet,message);
        }
    }
}

/// The renderer folders, found once on the first request
static ROOTS: OnceLock<Vec<(String, String)>> = OnceLock::new();

fn main() {
    match index_worker_process::dispatch_if_sign_worker() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("[pdf-sign-worker] {error}");
            std::process::exit(1);
        }
    }
    match index_worker_process::dispatch_if_decode_worker() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("[pdf-image-decoder] {error}");
            std::process::exit(1);
        }
    }
    match index_worker_process::dispatch_if_odt_worker() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("[odt-worker] {error}");
            std::process::exit(1);
        }
    }
    let worker_heap = match index_worker_process::worker_heap_mb() {
        Ok(heap) => heap,
        Err(error) => { eprintln!("[file-index] {error}"); std::process::exit(2); }
    };
    let worker_mode = worker_heap.is_some();
    let worker_output = if worker_mode {
        match index_worker_process::isolate_protocol_stdout() {
            Ok(output) => Some(output),
            Err(error) => { eprintln!("[file-index] {error}"); std::process::exit(2); }
        }
    } else { None };
    let options = match Options::read() {
        Ok(options) => options,
        Err(error) => { eprintln!("cannot prepare command output: {error}"); std::process::exit(3); }
    };
    // Headless exports are independent processes, never handed to a GUI instance.
    if !worker_mode && !headless::is_headless() && host::os::hand_over_to_running_instance() {
        return;
    }
    // the measurement runs unwatched: its path is always traced
    if options.measure.is_some() || std::env::var("SEUOFFICE_SHELL_TRACE").is_ok_and(|v| v == "1") {
        trace::enable();
    }
    // the thread that starts the event loop is the UI thread
    let ui_thread = std::thread::current().id();

    let core = CoreThread::spawn("seuoffice-core");
    let core_thread: Arc<dyn Thread> = core.clone();
    let outlet = Arc::new(TauriOutlet::default());
    let views = Arc::new(ViewTable::new(outlet.clone(), core_thread.clone()));
    let paths = Arc::new(ShellPaths::new());

    let mut builder = tauri::Builder::default()
        .invoke_system(include_str!("../bridge/invoke-system.js"))
        .invoke_handler(tauri::generate_handler![
            ipc::ipc_register,
            ipc::ipc_render_ready,
            native::menu::ipc_context_menu,
            ipc::ipc_invoke,
            ipc::ipc_send,
            ipc::ipc_listen,
            engine_host::engine_host_ready,
            engine_host::engine_host_reply,
            engine_host::engine_host_callback
        ]);
    builder = native::install(builder)
        .register_asynchronous_uri_scheme_protocol(ipc::sync::SCHEME, ipc::sync::handle);
    let drawio_assets = Arc::new(OnceLock::<Result<Arc<drawio_scheme::DrawioAssets>, String>>::new());
    builder = builder.register_asynchronous_uri_scheme_protocol("drawio-app", move |ctx, request, responder| {
        let assets = drawio_assets.get_or_init(|| {
            drawio_scheme::DrawioAssets::new(paths::resources_path(ctx.app_handle()), Vec::new()).map(Arc::new)
        });
        match assets {
            Ok(assets) => {
                let origin = native::SchemeRequestContext::capture(
                    &ctx.app_handle().state::<native::Native>(), ctx.webview_label(),
                );
                drawio_scheme::handle(assets.clone(), origin, request, responder)
            }
            Err(error) => {
                eprintln!("shell: cannot initialize Diagram assets: {error}");
                let mut response = tauri::http::Response::new(Vec::new());
                *response.status_mut() = tauri::http::StatusCode::INTERNAL_SERVER_ERROR;
                responder.respond(response);
            }
        }
    });
    for (name, scheme) in [
        ("seuoffice-docx-media", host::asset_protocol::Scheme::Docx),
        ("html-asset", host::asset_protocol::Scheme::Html),
        ("md-asset", host::asset_protocol::Scheme::Markdown),
        ("seuoffice-slide-media", host::asset_protocol::Scheme::Slides),
    ] {
        let scheme_views = views.clone();
        let scheme_core = core_thread.clone();
        builder = builder.register_asynchronous_uri_scheme_protocol(name, move |ctx, request, responder| {
            let origin = native::SchemeRequestContext::capture(
                &ctx.app_handle().state::<native::Native>(), ctx.webview_label(),
            );
            host::asset_protocol::handle(
                scheme, ctx.app_handle().clone(), scheme_views.clone(), scheme_core.clone(),
                ctx.webview_label().to_owned(), origin, request, responder,
            );
        });
    }

    // one scheme per renderer host: one origin each, as Electron's were
    let measure_page = options.measure.is_some();
    for host in app_scheme::hosts() {
        builder = builder.register_asynchronous_uri_scheme_protocol(
            app_scheme::scheme_of(host),
            move |ctx, request, responder| {
                let roots = ROOTS.get_or_init(|| {
                    app_scheme::roots(paths::resources_path(ctx.app_handle()), measure_page)
                });
                let label = ctx.webview_label().to_owned();
                // reading a file is not for the UI thread
                tauri::async_runtime::spawn_blocking(move || {
                    let response = app_scheme::serve(roots, host, &request);
                    trace::trace!(
                        "scheme",
                        "'{label}' {} {}{} -> {} ({} bytes, {})",
                        request.method(),
                        host,
                        request.uri().path(),
                        response.status().as_u16(),
                        response.body().len(),
                        response
                            .headers()
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("no content type")
                    );
                    responder.respond(response);
                });
            },
        );
    }

    let setup_views = views.clone();
    let setup_paths = paths.clone();
    let setup_core = core.clone();
    let invocation_shutdown = options.invocation_shutdown.clone();
    let cleanup_shutdown = shell_contract::runtime::CancelToken::default();
    let setup_cleanup_shutdown = cleanup_shutdown.clone();
    let build_failure_output = options.headless_output.clone();
    let application = builder
        .on_window_event(|window, event| {
            if let Some(registry) = window.try_state::<native::Native>() {
                registry.on_window_event(window, event);
            }
            if let WindowEvent::Destroyed = event {
                engine_host::window_destroyed(window.app_handle(), window.label());
            }
            if let Some(ipc) = window.try_state::<Ipc>()
                && let Some(id) = ipc.views.window_of_window_label(window.label())
            {
                host::w_tabs::window_event(id, event);
                if let WindowEvent::Destroyed = event {
                    host::window::window_destroyed(id);
                    ipc.views.remove_window(id);
                }
            }
        })
        .on_page_load(|webview, payload| {
            // `did-finish-load`: the stored interface scale goes into every page
            trace::trace!(
                "page",
                "'{}' {:?} {}",
                webview.label(),
                payload.event(),
                payload.url()
            );
            if !headless::is_headless() && payload.event() == tauri::webview::PageLoadEvent::Finished {
                ui_scale::on_page_finished(webview);
            }
        })
        .on_webview_event(|webview, event| {
            // the paths of a drop, for `webUtils.getPathForFile`
            if let WebviewEvent::DragDrop(DragDropEvent::Drop { paths, .. }) = event
                && let Some(ipc) = webview.try_state::<Ipc>()
            {
                ipc.views.set_dropped(webview.label(), paths.clone());
            }
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            native::Native::bind_ui(&handle);
            let views = setup_views;
            let paths = setup_paths;
            engine_host::install(&handle).map_err(std::io::Error::other)?;
            if let Some(output) = worker_output {
                let worker_app = handle.clone();
                let engine = handle.state::<Arc<engine_host::EngineHost>>().inner().clone();
                std::thread::Builder::new().name("seuoffice-index-protocol".into()).spawn(move || {
                    let result = engine.wait_ready().and_then(|()| host::w_home::index_worker::run_index_worker(output));
                    let code = match result {
                        Ok(code) => code,
                        Err(error) => { eprintln!("[file-index] {error}"); 1 }
                    };
                    worker_app.exit(code);
                }).map_err(std::io::Error::other)?;
                return Ok(());
            }
            host::native_commands::install(options.invocation_shutdown.clone()).map_err(std::io::Error::other)?;
            if !headless::is_headless() {
                app.manage(host::vault_watch::start(paths.settings_file()).map_err(std::io::Error::other)?);
                ui_scale::install(&handle, &paths.settings_file().to_string_lossy());
            }

            let ui: Arc<dyn Thread> = Arc::new(TauriUi {
                app: handle.clone(),
                main: ui_thread,
            });
            let windows = Arc::new(Windows::new(handle.clone(), views.clone()));
            let context = HostContext {
                app: handle.clone(),
                ui: ui.clone(),
                views: views.clone(),
                windows: windows.clone(),
                cleanup_shutdown: setup_cleanup_shutdown,
                headless_cleanup: host::headless_cleanup::Supervisor::new(),
            };
            let mut hosts = Hosts::default();
            host::install(&mut hosts, &context).map_err(std::io::Error::other)?;

            let events: Arc<dyn EventSink> = Arc::new(OutletSink {
                outlet: outlet.clone(),
                views: views.clone() as Arc<dyn Views>,
            });
            let workers: Arc<dyn Workers> = Arc::new(CatchingWorkers(Arc::new(ThreadWorkers)));
            let services = Services {
                ui: ui.clone(),
                core: core_thread.clone(),
                workers,
                events,
                views: views.clone(),
                paths: paths.clone(),
                hosts: Arc::new(hosts),
            };
            host::quit::register_services(services.clone())
                .inspect_err(|_| host::quit::startup_failed())
                .map_err(std::io::Error::other)?;

            let measure = options
                .measure
                .clone()
                .map(|out| Measure::new(out, ui, handle.clone()));
            app.manage(Ipc {
                core: setup_core,
                views,
                outlet,
                measure,
            });
            start(handle, services, windows, paths, context, options);
            Ok(())
        })
        .build(tauri::generate_context!())
        .inspect_err(|_| host::quit::startup_failed());
    let application = match application {
        Ok(application) => application,
        Err(error) => {
            if let Some(output) = build_failure_output {
                std::process::exit(headless::build_failed(&output,error.to_string()));
            }
            panic!("error while building the tauri application: {error}");
        }
    };
    application.run(move |app, event| {
            if matches!(event,tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit) {
                invocation_shutdown.cancel();
            }
            if matches!(event, tauri::RunEvent::Exit) {
                cleanup_shutdown.cancel();
            }
            if !worker_mode { host::on_run_event(app, &event); }
        });
}
