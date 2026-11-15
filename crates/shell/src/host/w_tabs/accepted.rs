use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

use main_core::docs::byte_handoff::MediaViewLease;
use main_core::shell::tab_manager::{TabError, TabResult};
use shell_contract::{Surface, ViewId, WindowId, ipc::NativeRequestOwner, runtime::CancelToken};
use tauri_runtime_cef::WebviewCefExt;

use super::{HostContext, NATIVE_WINDOWS, ViewSpec, Bridge, Bounds, WindowOptions, frame_event, view_event, navigate_deferred, fail_editor_view};
use crate::views::{AssetViewLease, SlidesViewLease};
mod generated;
pub(super) use generated::{schedule_generated,generated_load_failed,generated_native_registered,generated_document_ready,generated_native_revoked};
pub(super) use generated::{GeneratedBarrierSlot, cleanup_with_barrier};

pub(super) struct PreparedEditorView {
    pub(super) cx: HostContext,
    pub(super) core: Arc<dyn shell_contract::runtime::Thread>,
    pub(super) view: ViewId,
    pub(super) generation: shell_contract::runtime::RendererGeneration,
    pub(super) proof: crate::host::window::NativeClosureProof,
    pub(super) submission: tauri_runtime_cef::NativeBrowserSubmissionGuard,
    pub(super) window: WindowId,
    pub(super) window_lifecycle: Arc<crate::views::NativeWindowLifecycle>,
    pub(super) surface: Surface,
    pub(super) url: String,
    pub(super) deferred: bool,
    pub(super) docs_lease: Option<MediaViewLease>,
    pub(super) slides_lease: Option<SlidesViewLease>,
    pub(super) asset_lease: Option<AssetViewLease>,
    pub(super) network_session: Option<tauri_runtime_cef::NativeNetworkSession>,
    pub(super) invocation_on_frame: Option<Arc<dyn Fn(ViewId, &tauri_runtime_cef::FrameEvent) + Send + Sync>>,
    pub(super) invocation_navigate: Option<Arc<dyn Fn(Result<(), String>) + Send + Sync>>,
}

pub(super) fn cleanup_creation(
    cx: HostContext, view: ViewId, generation: shell_contract::runtime::RendererGeneration,
    close: Option<tauri_runtime_cef::NativeBrowserClose>,
    done: Box<dyn FnOnce(Result<(), String>) + Send>,
) {
    let Some(close) = close else {
        cx.views.remove_view_if_generation(view, &generation);
        return done(Ok(()));
    };
    let retained = Arc::new(std::sync::Mutex::new(None));
    let callback_retained = retained.clone();
    let done = Arc::new(std::sync::Mutex::new(Some(done)));
    let callback_done = done.clone();
    let subscription = close.when_closed(move |_| {
        cx.views.remove_view_if_generation(view, &generation);
        callback_retained.lock().unwrap_or_else(|error| error.into_inner()).take();
        if let Some(done) = callback_done.lock().unwrap_or_else(|error| error.into_inner()).take() {
            done(Ok(()));
        }
    });
    match subscription {
        Ok(subscription) => *retained.lock().unwrap_or_else(|error| error.into_inner()) = Some(subscription),
        Err(error) => {
            if let Some(done) = done.lock().unwrap_or_else(|error| error.into_inner()).take() { done(Err(error)); }
            return;
        }
    }
    if let Err(error) = close.request_close() {
        retained.lock().unwrap_or_else(|error| error.into_inner()).take();
        if let Some(done) = done.lock().unwrap_or_else(|error| error.into_inner()).take() { done(Err(error)); }
    }
}

pub(super) struct InstallError {
    pub(super) error: TabError,
    pub(super) close: Option<tauri_runtime_cef::NativeBrowserClose>,
    pub(super) proof: Option<crate::host::window::NativeClosureProof>,
}

impl InstallError {
    fn after_creation(error: TabError, close: tauri_runtime_cef::NativeBrowserClose, proof: Option<crate::host::window::NativeClosureProof>) -> Self {
        Self { error, close: Some(close), proof }
    }
}

pub(super) fn await_creation(
    child: crate::host::window::CreatedChild,
    done: Box<dyn FnOnce(Result<crate::host::window::CreatedChild, InstallError>) + Send>,
) {
    let retained = Arc::new(std::sync::Mutex::new(None));
    let callback_retained = retained.clone();
    let child = Arc::new(std::sync::Mutex::new(Some(child)));
    let callback_child = child.clone();
    let done = Arc::new(std::sync::Mutex::new(Some(done)));
    let callback_done = done.clone();
    let close = child.lock().unwrap_or_else(|error| error.into_inner()).as_ref().unwrap().close.clone();
    match close.when_created(move |outcome| {
        let child = callback_child.lock().unwrap_or_else(|error| error.into_inner()).take();
        callback_retained.lock().unwrap_or_else(|error| error.into_inner()).take();
        if let (Some(child), Some(done)) = (child, callback_done.lock().unwrap_or_else(|error| error.into_inner()).take()) {
            match outcome {
                tauri_runtime_cef::NativeBrowserCreationOutcome::Attached => done(Ok(child)),
                tauri_runtime_cef::NativeBrowserCreationOutcome::Failed =>
                    done(Err(InstallError::after_creation(TabError("The native browser could not be created".into()), child.close, child.proof))),
            }
        }
    }) {
        Ok(subscription) => *retained.lock().unwrap_or_else(|error| error.into_inner()) = Some(subscription),
        Err(error) => {
            if let (Some(child), Some(done)) = (
                child.lock().unwrap_or_else(|error| error.into_inner()).take(),
                done.lock().unwrap_or_else(|error| error.into_inner()).take(),
            ) {
                done(Err(InstallError::after_creation(TabError(error), child.close, child.proof)));
            }
        }
    }
}

impl PreparedEditorView {
    pub(super) fn install(self) -> Result<crate::host::window::CreatedChild, InstallError> {
        self.install_mode(None)
    }

    fn install_mode(self, generated: Option<u64>) -> Result<crate::host::window::CreatedChild, InstallError> {
        let Self { cx, core, view, generation: _, proof, submission, window, window_lifecycle, surface, url, deferred, docs_lease, slides_lease, asset_lease, network_session, invocation_on_frame, invocation_navigate } = self;
        let initial_url = if deferred { "about:blank".to_owned() } else { url.clone() };
        let child = cx.windows.add_reserved_view(
            view, window, &window_lifecycle, ViewSpec { url: initial_url, bridge: Bridge::Surface(surface) },
            Bounds { x: 0.0, y: 0.0, width: 1.0, height: 1.0 },
            WindowOptions { network_session, ..Default::default() },
            Some(proof),
            Some(submission),
            Some(Arc::new(move |view, event| {
                frame_event(view, event);
                if let Some(callback) = &invocation_on_frame { callback(view, event); }
            })), Some(Arc::new(view_event)),
        ).map_err(|error| InstallError { error: TabError(error.message), close: error.close, proof: error.proof })?;
        let webview = child.webview.clone();
        let close = child.close.clone();
        if let Some(id) = generated {
            generated::constructed(core.clone(), id, webview.clone(), close.clone());
        }
        if docs_lease.is_some() || slides_lease.is_some() || asset_lease.is_some() {
            let webview = webview.clone();
            let ready_view = webview.clone();
            
            let sent = webview.when_navigation_ready(move |ready| {
                if let Err(error) = ready {
                    post_navigation_result(core, cx, view, url, generated, Err(error.to_string()), invocation_navigate);
                    return;
                }
                let failed_cx = cx.clone();
                let failed_core = core.clone();
                let failed_navigation = invocation_navigate.clone();
                if let Err(error) = ready_view.with_cef_webview(move |native| {
                    let bound = if let Some(lease) = docs_lease {
                        cx.views.bind_docs_media_browser(view, &lease, native.snapshot().browser_id, native.frame_navigation_state().clone())
                    } else if let Some(lease) = slides_lease {
                        cx.views.bind_slides_media_browser(view, &lease, native.snapshot().browser_id, native.frame_navigation_state().clone())
                    } else if let Some(lease) = asset_lease {
                        cx.views.bind_native_assets_browser(view, &lease, native.snapshot().browser_id, native.frame_navigation_state().clone())
                    } else {
                        Err("native browser binding requires an editor lease".into())
                    };
                    post_navigation_result(core, cx, view, url, generated, bound, invocation_navigate);
                }) {
                    post_navigation_result(failed_core, failed_cx, view, String::new(), generated, Err(error.to_string()), failed_navigation);
                }
            });
            if let Err(error) = sent {
                return Err(InstallError::after_creation(TabError(error.to_string()), close, child.proof.clone()));
            }
        } else if deferred {
            let webview = webview.clone();
            
            if let Err(error) = webview.when_navigation_ready(move |ready| {
                let ready = ready.map_err(|error| error.to_string());
                post_navigation_result(core, cx, view, url, generated, ready, invocation_navigate);
            }) {
                return Err(InstallError::after_creation(TabError(error.to_string()), close, child.proof.clone()));
            }
        }
        Ok(child)
    }
}

fn post_navigation_result(
    core: Arc<dyn shell_contract::runtime::Thread>, cx: HostContext,
    view: ViewId, url: String, generated: Option<u64>, result: Result<(), String>,
    invocation_navigate: Option<Arc<dyn Fn(Result<(), String>) + Send + Sync>>,
) {
    if let Some(navigate) = invocation_navigate { navigate(result); return; }
    core.post(Box::new(move || {
        if let Some(id) = generated { generated::navigation_ready(id, result); }
        else {
            match result {
                Ok(()) => navigate_deferred(cx, view, url),
                Err(error) => fail_editor_view(cx, view, error),
            }
        }
    }));
}

fn rollback(cx: &HostContext, view: ViewId) {
    cx.views.remove_view(view);
    if let Some(host) = NATIVE_WINDOWS.get() {
        host.deferred_views.lock().unwrap_or_else(|error| error.into_inner()).remove(&view);
        host.fail_page_waiters(view);
    }
}

type Completion = Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<ViewId>)>;
struct Reopen {
    done: Box<dyn FnOnce(Result<(String, Option<String>, NativeRequestOwner, CancelToken), String>)>,
    _subscriptions: Vec<shell_contract::runtime::CancelSubscription>,
}
thread_local! {
    static COMPLETIONS: RefCell<HashMap<u64, Completion>> = RefCell::new(HashMap::new());
    static REVEALS: RefCell<HashMap<u64, Box<dyn FnOnce(TabResult<()>)>>> = RefCell::new(HashMap::new());
    static REOPENS: RefCell<HashMap<u64, Reopen>> = RefCell::new(HashMap::new());
}
static NEXT: AtomicU64 = AtomicU64::new(1);

pub(super) fn cleanup_proof(
    cx: HostContext, view: ViewId, generation: shell_contract::runtime::RendererGeneration,
    proof: crate::host::window::NativeClosureProof,
    done: Box<dyn FnOnce(Result<shell_contract::ipc::NativeBrowserRetirement, String>) + Send>,
) {
    let done = Arc::new(std::sync::Mutex::new(Some(done)));
    let callback_done = done.clone();
    if let Err(error) = proof.watch(Box::new(move |receipt| {
        cx.views.remove_view_if_generation(view, &generation);
        if let Some(done) = callback_done.lock().unwrap_or_else(|error| error.into_inner()).take() {
            done(Ok(shell_contract::ipc::NativeBrowserRetirement::Closed(receipt)));
        }
    })) {
        if let Some(done) = done.lock().unwrap_or_else(|error| error.into_inner()).take() { done(Err(error)); }
        return;
    }
    if let Err(error) = proof.close().request_close() {
        if let Some(done) = done.lock().unwrap_or_else(|error| error.into_inner()).take() { done(Err(error)); }
    }
}

fn finish_install(
    id: u64, core: Arc<dyn shell_contract::runtime::Thread>, cx: HostContext,
    view: ViewId, generation: shell_contract::runtime::RendererGeneration,
    fallback: Option<crate::host::window::NativeClosureProof>,
    result: Result<crate::host::window::CreatedChild, InstallError>,
) {
    let finish = Box::new(move |result: Result<crate::host::window::CreatedChild, InstallError>| {
        let result = match result {
            Ok(child) if !generation.is_active() => Err(InstallError::after_creation(
                TabError("The created view was replaced before attachment".into()), child.close, child.proof)),
            result => result,
        };
        use shell_ipc_w::tabs::host::{ViewCreationCleanup, ViewCreationCleanupState, ViewCreationFailure, ViewCreationReady};
        match result {
            Ok(child) => {
                let Some(proof) = child.proof.or(fallback) else {
                    if let Err(error) = child.close.request_close() {
                        eprintln!("[shell] closing an unbound native browser failed: {error}");
                    }
                    eprintln!("[shell] native browser lacks its captured closure proof");
                    return;
                };
                let watched = proof.clone();
                let closure: main_core::shell::tab_manager::ViewClosureWatch<shell_contract::ipc::NativeBrowserRetirement> =
                    Arc::new(move |done| watched.watch(Box::new(move |receipt| done(shell_contract::ipc::NativeBrowserRetirement::Closed(receipt)))));
                let reservation = shell_ipc_w::tabs::host::NativeBrowserBinding {
                    reservation: proof.reservation().clone(), owner: proof.owner().clone(),
                    producer: Arc::new(proof.clone()), closure: closure.clone(),
                    registration: shell_ipc_w::tabs::host::NativeBrowserRegistration::Unregistered,
                };
                let cleanup: ViewCreationCleanup = Arc::new(move |done| {
                    cleanup_proof(cx.clone(), view, generation.clone(), proof.clone(), done);
                });
                core.post(Box::new(move || {
                    let callback = COMPLETIONS.with(|pending| pending.borrow_mut().remove(&id));
                    if let Some(callback) = callback { callback(Ok(ViewCreationReady { view: child.view, cleanup, closure, reservation })); }
                }));
            }
            Err(error) => {
                let Some(proof) = error.proof.or(fallback) else {
                    cx.views.remove_view_if_generation(view, &generation);
                    return core.post(Box::new(move || {
                        if let Some(callback) = COMPLETIONS.with(|pending| pending.borrow_mut().remove(&id)) {
                            callback(Err(ViewCreationFailure { error: error.error, cleanup: ViewCreationCleanupState::Complete }));
                        }
                    }));
                };
                let cleanup: ViewCreationCleanup = Arc::new(move |done| {
                    cleanup_proof(cx.clone(), view, generation.clone(), proof.clone(), done);
                });
                let retry = cleanup.clone();
                cleanup(Box::new(move |result| {
                    let (error, cleanup) = match result {
                        Ok(receipt) => (error.error, ViewCreationCleanupState::Acknowledged(receipt)),
                        Err(cleanup_error) => (
                            TabError(format!("{}; creation cleanup failed: {cleanup_error}", error.error.0)),
                            ViewCreationCleanupState::Pending(retry),
                        ),
                    };
                    core.post(Box::new(move || {
                        let callback = COMPLETIONS.with(|pending| pending.borrow_mut().remove(&id));
                        if let Some(callback) = callback { callback(Err(ViewCreationFailure { error, cleanup })); }
                    }));
                }));
            }
        }
    });
    match result {
        Ok(child) => await_creation(child, finish),
        Err(error) => finish(Err(error)),
    }
}

pub(super) fn schedule_plain(prepared: PreparedEditorView, done: Completion) {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    COMPLETIONS.with(|pending| pending.borrow_mut().insert(id, done));
    let ui = prepared.cx.ui.clone();
    let core = prepared.core.clone();
    let cx = prepared.cx.clone();
    let view = prepared.view;
    let generation = prepared.generation.clone();
    let proof = prepared.proof.clone();
    ui.post(Box::new(move || {
        let action = generation.with_active(|| move || prepared.install());
        let result = match action {
            Some(action) => action(),
            None => Err(InstallError { error: TabError("The view was replaced before creation".into()), close: None, proof: None }),
        };
        finish_install(id, core, cx, view, generation, Some(proof), result);
    }));
}

pub(super) fn schedule_child(
    cx: HostContext, core: Arc<dyn shell_contract::runtime::Thread>, view: ViewId,
    generation: shell_contract::runtime::RendererGeneration,
    proof: Option<crate::host::window::NativeClosureProof>,
    create: Box<dyn FnOnce() -> Result<crate::host::window::CreatedChild, InstallError> + Send>,
    done: Completion,
) {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    COMPLETIONS.with(|pending| pending.borrow_mut().insert(id, done));
    let ui = cx.ui.clone();
    ui.post(Box::new(move || {
        let action = generation.with_active(|| create);
        let result = match action {
            Some(action) => action(),
            None => Err(InstallError { error: TabError("The view was replaced before creation".into()), close: None, proof: None }),
        };
        finish_install(id, core, cx, view, generation, proof, result);
    }));
}

pub(super) fn schedule(prepared: PreparedEditorView, owner: NativeRequestOwner, cancel: CancelToken, pending: CancelToken, done: Completion) {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    COMPLETIONS.with(|pending| pending.borrow_mut().insert(id, done));
    let ui = prepared.cx.ui.clone();
    let task_ui = ui.clone();
    let core = prepared.core.clone();
    let cx = prepared.cx.clone();
    let view = prepared.view;
    let generation = prepared.generation.clone();
    let proof = prepared.proof.clone();
    ui.post(Box::new(move || {
        let result = crate::ipc::native_owner::run_accepted_created_ui(task_ui.as_ref(), &owner, &generation, &cancel, &pending, move || prepared.install())
            .map_err(|error| InstallError { error: TabError(error), close: None, proof: None }).and_then(|result| result);
        finish_install(id, core, cx, view, generation, Some(proof), result);
    }));
}

pub(super) fn reveal(cx: &HostContext, core: Arc<dyn shell_contract::runtime::Thread>, view: ViewId, expected: &shell_contract::runtime::RendererGeneration, owner: NativeRequestOwner, cancel: CancelToken, done: Box<dyn FnOnce(TabResult<()>)>) {
    let destination = match crate::ipc::native_owner::capture_owner(&cx.views, view, expected) {
        Ok(destination) => destination,
        Err(error) => { done(Err(TabError(error))); return; }
    };
    let Some(webview) = cx.windows.webview(view) else {
        done(Err(TabError("the target editor view is gone".into())));
        return;
    };
    let window = webview.window().clone();
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    REVEALS.with(|pending| pending.borrow_mut().insert(id, done));
    let ui = cx.ui.clone();
    let task_ui = ui.clone();
    ui.post(Box::new(move || {
        let result = crate::ipc::native_owner::run_accepted_ui_pair(task_ui.as_ref(), &owner, &destination, &cancel, &cancel, move || {
            window.unminimize().map_err(|error| TabError(error.to_string()))?;
            window.show().map_err(|error| TabError(error.to_string()))?;
            window.set_focus().map_err(|error| TabError(error.to_string()))?;
            webview.set_focus().map_err(|error| TabError(error.to_string()))
        }).map_err(TabError).and_then(|result| result);
        core.post(Box::new(move || {
            let callback = REVEALS.with(|pending| pending.borrow_mut().remove(&id));
            if let Some(callback) = callback { callback(result); }
        }));
    }));
}

pub(super) fn store_reopen(done: Box<dyn FnOnce(Result<(String, Option<String>, NativeRequestOwner, CancelToken), String>)>, destination:&NativeRequestOwner, pending:&CancelToken) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut subscriptions = Vec::new();
    for loss in destination.loss_token().into_iter().chain(std::iter::once(pending.clone())) {
        subscriptions.push(loss.on_cancel(move || {
            crate::host::w::post_core(move |_,_| complete_reopen(id,Err("A captured Drive reopen participant was released".into())));
        }));
    }
    REOPENS.with(|requests| requests.borrow_mut().insert(id, Reopen { done, _subscriptions:subscriptions }));
    id
}

pub(super) fn complete_reopen(id: u64, result: Result<(String, Option<String>, NativeRequestOwner, CancelToken), String>) {
    let callback = REOPENS.with(|pending| pending.borrow_mut().remove(&id));
    if let Some(callback) = callback { (callback.done)(result); }
}
