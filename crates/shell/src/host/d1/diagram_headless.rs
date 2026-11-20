pub(crate) mod network;
mod product;

use crate::native::NativeEventKind;
use shell_contract::ipc::{
    NativeInvocationOperation, NativeInvocationSelection, NativeInvocationTarget,
    NativeRequestOwner,
};
use shell_contract::runtime::{CancelSubscription, RendererGeneration, Views};
use shell_contract::{Surface, ViewId};
use std::sync::{Arc, Mutex};

use crate::host::{
    HostContext,
    window::{Bridge, ViewSpec, WindowOptions, WindowSpec},
};
use crate::ipc::native_owner::NativeOwnerAuthority;
use main_core::packages::electron_utils::headless_export::HeadlessExportRequest;
use main_core::shell::headless_export::HeadlessExportProduct;

const PAGE: &str = "genoffice-app://diagram/index.html?headless=1";
type Done = Box<dyn FnOnce(Result<Option<HeadlessExportProduct>, String>) + Send>;

#[derive(Default)]
struct Binding {
    generation: Option<RendererGeneration>,
    target: Option<NativeInvocationTarget>,
    view: Option<ViewId>,
    started: bool,
    closed: bool,
    committed: bool,
    bound: bool,
    owner: Option<NativeRequestOwner>,
    cancellations: Vec<CancelSubscription>,
}

pub(crate) fn export_headless(
    cx: &HostContext,
    input: &str,
    out: &str,
    format: &str,
    request: &HeadlessExportRequest,
    operation: &NativeInvocationOperation,
    done: Done,
) -> Result<(), String> {
    match operation.selection() {
        NativeInvocationSelection::HeadlessExport {
            surface,
            input: selected_input,
            output,
            format: selected_format,
            page,
            ops,
            batch,
        } if *surface == Surface::Diagram
            && selected_input == std::path::Path::new(input)
            && output == std::path::Path::new(out)
            && selected_format == format
            && *page == request.page
            && ops == &request.ops
            && batch.as_deref() == request.batch => {}
        _ => return Err("The native export selection does not match the diagram job".into()),
    }
    let operation = operation.clone();
    let mut captured = None;
    if !crate::host::w::on_core_services_local(|_, services| captured = Some(services.clone())) {
        return Err("Core services are not available".into());
    }
    let services = captured.ok_or("Core services are not available")?;
    if shell_ipc_d1::diagram::views().is_none() {
        return Err("The diagram editor is not registered".into());
    }
    let policy = network::request_policy();
    let input = input.to_owned();
    let out = out.to_owned();
    let format = format.to_owned();
    let request = request.clone();
    let context = cx.clone();
    let workers = services.workers.clone();
    let core = services.core.clone();
    services.workers.spawn(Box::new(move || {
        let prepare = operation.with_active(|| || {
            let job = product::job(&format, &request)?;
            let exists = shell_ipc_d1::diagram::prepare_hidden_open_path(Some(&input)).is_some();
            Ok::<_, String>((job, exists))
        });
        let (job, input_exists) = match prepare {
            Some(prepare) => match prepare() { Ok(value) => value, Err(error) => return done(Err(error)) },
            None => return done(Err("The diagram job expired".into())),
        };
        let binding = Arc::new(Mutex::new(Binding::default()));
        let ui = context.ui.clone();
        ui.post(Box::new(move || {
            let event_binding = binding.clone();
            let event_views = context.views.clone();
            let event_core = core.clone();
            let closed_core = core.clone();
            let closed_binding = binding.clone();
            let windows = context.windows.clone();
            let creation_views=context.views.clone();
            let construct = crate::ipc::native_owner::run_native_invocation_ui(context.ui.as_ref(), &operation, move || {
                let reserved=windows.reserve_deferred_window(Surface::Diagram)?;
                let proof=reserved.proof.clone();
                let generation=reserved.created.clone();
                let reserved_view=reserved.view;
                let (window,view)=windows.open_reserved_deferred_window(WindowSpec {
                title: String::new(), size: (1300.0, 880.0), min_size: None, visible: false,
                decorations: true, transparent: false, resizable: true, parent: None, focus: false, main: false,
                view: ViewSpec { url: PAGE.into(), bridge: Bridge::Surface(Surface::Diagram) },
            }, WindowOptions {
                background_throttling: Some(tauri::utils::config::BackgroundThrottlingPolicy::Disabled),
                request_policy: Some(policy),
                on_frame: Some(Arc::new(move |view, event| {
                    let mut state = event_binding.lock().unwrap_or_else(|error| error.into_inner());
                    match &event.kind {
                        NativeEventKind::NavigationStarted { .. } => {
                            state.committed = false;
                            if state.bound
                                && let Some(generation) = state.generation.clone()
                            {
                                    event_core.post(Box::new(move || {
                                        if let Some(views) = shell_ipc_d1::diagram::views() { views.fail_headless_job_generation(view, &generation, "the diagram page stopped"); }
                                    }));
                                }
                        }
                        NativeEventKind::DocumentCommitted { url, .. } => {
                            state.committed = crate::app_scheme::electron_url(url).as_deref() == Some(PAGE);
                        }
                        NativeEventKind::DocumentRegistered { context: origin } if state.committed && !state.bound
                            && origin.browser().is_same_browser(&event.browser) => {
                            state.bound = true;
                            let generation = state.generation.clone();
                            let target = state.target.clone();
                            let registered = generation.as_ref()
                                .and_then(|generation| event_views.capture_native_owner(view,generation))
                                .filter(|captured| captured.surface==Surface::Diagram && captured.origin==*origin && captured.browser_owner.is_some())
                                .and_then(|captured| crate::ipc::native_owner::owner_with_browser(view,captured.origin,captured.renderer,(),captured.browser_owner))
                                .zip(target)
                                .and_then(|(owner, target)| {
                                    crate::ipc::native_owner::register_native_invocation_target(&target, owner.clone()).ok().map(|()| owner)
                                });
                            match registered {
                                Some(owner) => state.owner = Some(owner),
                                None => if let Some(generation) = state.generation.clone() {
                                    event_core.post(Box::new(move || {
                                        if let Some(views) = shell_ipc_d1::diagram::views() { views.fail_headless_job_generation(view, &generation, "the diagram native registration was refused"); }
                                    }));
                                },
                            }
                        }
                        NativeEventKind::DocumentRevoked { document, .. } if state.owner.as_ref()
                            .and_then(|owner| owner.context.authority_ref::<NativeOwnerAuthority>())
                            .is_some_and(|owner| owner.origin.identity() == document) => {
                            if let Some(generation) = state.generation.clone() {
                                event_core.post(Box::new(move || {
                                    if let Some(views) = shell_ipc_d1::diagram::views() { views.fail_headless_job_generation(view, &generation, "the diagram page stopped"); }
                                }));
                            }
                        }
                        NativeEventKind::WebviewProcessFailed { .. } if state.started => {
                            if let Some(generation) = state.generation.clone() {
                                event_core.post(Box::new(move || {
                                    if let Some(views) = shell_ipc_d1::diagram::views() { views.fail_headless_job_generation(view, &generation, "the diagram page stopped"); }
                                }));
                            }
                        }
                        NativeEventKind::NavigationFailed { .. } if state.started && !state.bound => {
                            if let Some(generation) = state.generation.clone() {
                                event_core.post(Box::new(move || {
                                    if let Some(views) = shell_ipc_d1::diagram::views() { views.fail_headless_job_generation(view, &generation, "the diagram page did not load"); }
                                }));
                            }
                        }
                        _ => {}
                    }
                })),
                on_closed: Some(Box::new(move |_| {
                    let binding = {
                        let mut state = closed_binding.lock().unwrap_or_else(|error| error.into_inner());
                        state.closed = true;
                        state.view.zip(state.generation.clone())
                    };
                    if let Some((view, generation)) = binding {
                        closed_core.post(Box::new(move || {
                            if let Some(views) = shell_ipc_d1::diagram::views() { views.fail_headless_job_generation(view, &generation, "the diagram page stopped"); }
                        }));
                    }
                })),
                ..WindowOptions::default()
            }, reserved)?;
                let adopted=creation_views.prepare_browser_owner(reserved_view,&generation,proof.owner())
                    .and_then(|browser| generation.with_active(||proof.reservation().with_initial_owner(proof.owner(),|transition| {
                        browser.validate(transition)?.commit();
                        Ok(())
                    })).ok_or_else(||"The diagram renderer generation expired".to_owned())?);
                if let Err(error)=adopted { let _=windows.close_window(window); return Err(error); }
                Ok((window,view))
            });
            let opened = match construct {
                Ok(opened) => opened,
                Err(error) => return done(Err(error)),
            };
            let (window, view) = match opened { Ok(value) => value, Err(error) => return done(Err(error)) };
            let Some(generation) = context.views.renderer_generation(view) else {
                let _ = context.windows.close_window(window);
                return done(Err("The diagram renderer generation is unavailable".into()));
            };
            let target = match operation.reserve_target(view, generation.clone()) {
                Ok(target) => target,
                Err(error) => { let _ = context.windows.close_window(window); return done(Err(error)); }
            };
            {
                let mut state = binding.lock().unwrap_or_else(|error| error.into_inner());
                state.generation = Some(generation);
                state.target = Some(target.clone());
                state.view = Some(view);
            }
            let core_context = context.clone();
            let core_binding = binding.clone();
            let load_core = core.clone();
            core.post(Box::new(move || {
                let Some(views) = shell_ipc_d1::diagram::views() else {
                    let ui = core_context.ui.clone();
                    ui.post(Box::new(move || { let _ = core_context.windows.close_window(window); done(Err("The diagram editor was released".into())); }));
                    return;
                };
                if core_binding.lock().unwrap_or_else(|error| error.into_inner()).closed {
                    return done(Err("the diagram page stopped".into()));
                }
                let prepared = target.with_active(|| views.open_fresh_hidden_view_prepared(
                    view, input_exists.then_some(input.as_str()),
                )).unwrap_or_else(|| Err("The diagram job expired".into()));
                if let Err(error) = prepared {
                    let ui = core_context.ui.clone();
                    ui.post(Box::new(move || { let _ = core_context.windows.close_window(window); done(Err(error)); }));
                    return;
                }
                let authorization = core_binding.clone();
                let finish_context = core_context.clone();
                let finish_binding = core_binding.clone();
                let finish_workers = workers.clone();
                let finish_target = target.clone();
                views.run_headless_job(view, job, Box::new(move |cx| {
                    let binding = authorization.lock().unwrap_or_else(|error| error.into_inner());
                    let Some(owner) = &binding.owner else { return false; };
                    let Some(expected) = owner.context.authority_ref::<NativeOwnerAuthority>() else { return false; };
                    let Some(actual) = cx.native.as_ref().and_then(|context| context.authority_ref::<NativeOwnerAuthority>()) else { return false; };
                    expected.origin == actual.origin && expected.origin.is_registered() && !cx.cancel.is_cancelled()
                }), Box::new(move |report| {
                    let owner = finish_binding.lock().unwrap_or_else(|error| error.into_inner()).owner.clone();
                    finish_workers.spawn(Box::new(move || {
                        let result = if !matches!(shell_ipc_d1::value::field(&report, "ok"), shell_contract::IpcValue::Bool(true)) {
                            Err(shell_ipc_d1::value::as_str(shell_ipc_d1::value::field(&report, "error")).unwrap_or("the diagram page sent no report").to_owned())
                        } else { match owner {
                            None => Err("The diagram renderer was released".into()),
                            Some(owner) => finish_product(&finish_context, &input, &out, &format, request.page, &report, &owner, &finish_target),
                        }};
                        let ui = finish_context.ui.clone();
                        ui.post(Box::new(move || { let _ = finish_context.windows.close_window(window); done(result.map(Some)); }));
                    }));
                }));
                let mut subscriptions = Vec::new();
                for token in [target.operation().cancel(), target.operation().shutdown()] {
                    let cancellation_core = load_core.clone();
                    let generation = target.created().clone();
                    subscriptions.push(token.on_cancel(move || {
                        cancellation_core.post(Box::new(move || {
                            if let Some(views) = shell_ipc_d1::diagram::views() { views.fail_headless_job_generation(view, &generation, "the diagram job was cancelled"); }
                        }));
                    }));
                }
                core_binding.lock().unwrap_or_else(|error| error.into_inner()).cancellations = subscriptions;
                let ui_context = core_context.clone();
                let load_binding = core_binding.clone();
                core_context.ui.post(Box::new(move || {
                    load_binding.lock().unwrap_or_else(|error| error.into_inner()).started = true;
                    let result = crate::ipc::native_owner::run_native_invocation_created_ui(
                        ui_context.ui.as_ref(), &target, || ui_context.windows.load_view(view, PAGE),
                    ).and_then(|result| result);
                    if let Err(error) = result {
                        let generation = target.created().clone();
                        load_core.post(Box::new(move || {
                            if let Some(views) = shell_ipc_d1::diagram::views() { views.fail_headless_job_generation(view, &generation, &error); }
                        }));
                    }
                }));
            }));
        }));
    }));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finish_product(
    cx: &HostContext,
    input: &str,
    out: &str,
    format: &str,
    page: Option<f64>,
    report: &shell_contract::IpcValue,
    owner: &NativeRequestOwner,
    target: &NativeInvocationTarget,
) -> Result<HeadlessExportProduct, String> {
    let cancel = target.operation().cancel().clone();
    let pending = target.operation().shutdown().clone();
    let mut open = |name: &str, bytes: &[u8]| {
        super::diagram::open_native_invocation(name, bytes, target, owner, &cancel, &pending)
    };
    let mut admit = |effect: &mut dyn FnMut()| {
        crate::ipc::native_owner::with_native_invocation_effect(
            target, owner, &cancel, &pending, effect,
        )
        .ok_or_else(|| main_core::node_fs::NodeError {
            code: None,
            message: "The diagram job expired".into(),
        })
    };
    let mut render = |pages: &[main_core::diagram::print_document::PrintPage], title: &str| {
        crate::host::print::render_diagram_pdf_native_invocation(
            cx, target, owner, pages, title, &cancel, &pending,
        )
    };
    product::product(
        input,
        out,
        format,
        page,
        report,
        &mut open,
        &mut render,
        &mut admit,
    )
}
