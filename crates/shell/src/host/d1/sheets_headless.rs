//! Hidden Sheets renderer using its ordinary queued-open and PDF export handshake.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{
    Arc, Condvar, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

use crate::native::{
    BrowserCloseSubscription, BrowserId, DocumentContext, NativeEventKind, WebviewExt,
};
use main_core::packages::electron_utils::headless_export::HeadlessExportRequest;
use main_core::shell::headless_export::HeadlessExportProduct;
use shell_contract::{
    Surface, ViewId, WindowId,
    ipc::{NativeInvocationOperation, NativeInvocationTarget},
    runtime::{CancelSubscription, RendererGeneration, Views},
};

use crate::host::{
    HostContext,
    window::{Bridge, ViewSpec, WindowOptions, WindowSpec},
};

type Done = Box<dyn FnOnce(Result<Option<HeadlessExportProduct>, String>) + Send>;
const URL: &str = "genoffice-app://sheets/index.html";
const TIMEOUT: Duration = Duration::from_millis(600_000);

static NEXT_EXPORT: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static REGISTRATIONS: RefCell<HashMap<u64, Rc<shell_ipc_d1::sheets::RegisteredTab>>> = RefCell::new(HashMap::new());
}

fn terminal_cleanup_ready(
    browser_closed: bool,
    actor_drained: bool,
    window_terminal: bool,
) -> bool {
    browser_closed && actor_drained && window_terminal
}

struct Created {
    window: WindowId,
    view: ViewId,
    generation: RendererGeneration,
    webview: OnceLock<tauri::Webview>,
    target: NativeInvocationTarget,
    proof: crate::host::window::NativeClosureProof,
    window_lifecycle: Arc<crate::views::NativeWindowLifecycle>,
}

#[derive(Default)]
struct Deadline {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl Deadline {
    fn stop(&self) {
        *self
            .stopped
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
        self.wake.notify_all();
    }

    fn expired(&self, timeout: Duration) -> bool {
        let stopped = self
            .stopped
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (stopped, elapsed) = self
            .wake
            .wait_timeout_while(stopped, timeout, |stopped| !*stopped)
            .unwrap_or_else(|error| error.into_inner());
        elapsed.timed_out() && !*stopped
    }
}

struct Export {
    id: u64,
    native_receipt: Mutex<Option<shell_contract::ipc::NativeClosureReceipt>>,
    cx: HostContext,
    operation: NativeInvocationOperation,
    cancellation: Mutex<Vec<CancelSubscription>>,
    created: OnceLock<Created>,
    creation_finished: AtomicBool,
    actor_started: AtomicBool,
    committed: AtomicBool,
    registered: OnceLock<DocumentContext>,
    browser: OnceLock<BrowserId>,
    closing: Mutex<Option<BrowserCloseSubscription>>,
    closed: AtomicBool,
    settled: AtomicBool,
    cleanup_started: AtomicBool,
    cleanup_finished: AtomicBool,
    actor_drained: AtomicBool,
    window_terminal: AtomicBool,
    result: Mutex<Option<Result<(), String>>>,
    done: Mutex<Option<Done>>,
    deadline: Arc<Deadline>,
}

struct CreationGuard(Option<Arc<Export>>);
impl Drop for CreationGuard {
    fn drop(&mut self) {
        if let Some(job) = self.0.take() {
            job.creation_finished.store(true, Ordering::Release);
            if job.settled.load(Ordering::Acquire) {
                job.close_created();
            } else {
                job.finish(Err(
                    "The hidden Sheets constructor task was discarded".into()
                ));
            }
        }
    }
}

struct DestroyGuard(Option<Arc<Export>>);
impl Drop for DestroyGuard {
    fn drop(&mut self) {
        if let Some(job) = self.0.take() {
            job.append_cleanup_error("The hidden Sheets destruction task was discarded".into());
            job.report_cleanup_failure();
        }
    }
}

impl Export {
    fn finish(self: &Arc<Self>, mut result: Result<(), String>) {
        if self.settled.swap(true, Ordering::AcqRel) {
            return;
        }
        if result.is_ok() {
            result = match self.created.get() {
                Some(created)
                    if created.generation.is_active()
                        && self.cx.views.renderer_generation(created.view).is_some_and(
                            |current| current.same_generation(&created.generation),
                        ) =>
                {
                    if self.registered.get().is_some_and(|origin| {
                        origin.is_registered() && self.browser.get() == Some(&origin.browser().id())
                    }) {
                        created.target.registered_document().and_then(|document| {
                            crate::ipc::native_owner::with_native_invocation_effect(
                                &created.target,
                                document.owner(),
                                self.operation.cancel(),
                                self.operation.shutdown(),
                                || (),
                            )
                            .ok_or_else(|| {
                                "The Sheets native invocation expired before completion".into()
                            })
                        })
                    } else {
                        Err("The Sheets exporter has no matching registered native document".into())
                    }
                }
                _ => Err("The Sheets export renderer was replaced".into()),
            };
        }
        *self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(result);
        self.deadline.stop();
        self.close_created();
    }

    fn append_cleanup_error(&self, error: String) {
        let mut result = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(current) = result.take() {
            *result = Some(Err(match current {
                Ok(()) => format!("The Sheets export completed, but cleanup failed: {error}"),
                Err(first) => format!("{first}; hidden Sheets browser cleanup failed: {error}"),
            }));
        }
    }

    fn close_created(self: &Arc<Self>) {
        if self.created.get().is_none() {
            if self.creation_finished.load(Ordering::Acquire) {
                self.complete();
            }
            return;
        }
        if !self.creation_finished.load(Ordering::Acquire) {
            return;
        }
        if !self.window_terminal.load(Ordering::Acquire)
            && !self.cleanup_started.swap(true, Ordering::AcqRel)
        {
            self.clone().destroy();
        }
        let proof = self
            .native_receipt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(proof) = proof else {
            return;
        };
        if self.cleanup_finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let job = self.clone();
        let unavailable = self.clone();
        crate::host::w::post_core_checked(
            move |_, _| {
                let registration = REGISTRATIONS
                    .with(|registrations| registrations.borrow().get(&job.id).cloned());
                match (registration, shell_ipc_d1::sheets::views()) {
                    (Some(registration), Some(editor)) => {
                        let completed = job.clone();
                        editor.finish_headless_export(
                            registration,
                            proof,
                            Box::new(move |result| match result {
                                Ok(()) => {
                                    REGISTRATIONS.with(|registrations| {
                                        registrations.borrow_mut().remove(&completed.id);
                                    });
                                    completed.actor_drained.store(true, Ordering::Release);
                                    completed.complete();
                                }
                                Err(error) => {
                                    completed.append_cleanup_error(error);
                                    completed.report_cleanup_failure();
                                }
                            }),
                        );
                    }
                    (None, _) if !job.actor_started.load(Ordering::Acquire) => {
                        job.actor_drained.store(true, Ordering::Release);
                        job.complete();
                    }
                    _ => {
                        job.append_cleanup_error(
                            "The Sheets headless registration is unavailable".into(),
                        );
                        job.report_cleanup_failure();
                    }
                }
            },
            move || {
                unavailable.append_cleanup_error("The Sheets core services are unavailable".into());
                unavailable.report_cleanup_failure();
            },
        );
    }

    fn destroy(self: Arc<Self>) {
        let Some(created) = self.created.get() else {
            self.complete();
            return;
        };
        if let Err(error) = created.proof.close().request_close() {
            self.append_cleanup_error(error);
            self.report_cleanup_failure();
        }
        let Ok(window) = created.window_lifecycle.window_handle() else {
            return;
        };
        let ui = self.cx.ui.clone();
        let pending = DestroyGuard(Some(self.clone()));
        ui.post(Box::new(move || {
            let mut pending = pending;
            if self.window_terminal.load(Ordering::Acquire) {
                drop(pending.0.take());
                return;
            }
            if let Err(error) = window.destroy() {
                self.append_cleanup_error(error.to_string());
                self.report_cleanup_failure();
            }
            drop(pending.0.take());
        }));
    }

    fn report_cleanup_failure(&self) {
        let result = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let done = self
            .done
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let (Some(done), Some(result)) = (done, result) {
            done(result.map(|()| None));
        }
    }

    fn complete(&self) {
        if self.created.get().is_some()
            && !terminal_cleanup_ready(
                self.closed.load(Ordering::Acquire),
                self.actor_drained.load(Ordering::Acquire),
                self.window_terminal.load(Ordering::Acquire),
            )
        {
            return;
        }
        let subscription = self
            .closing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        drop(subscription);
        if self.closed.load(Ordering::Acquire)
            && let Some(created) = self.created.get()
        {
            self.cx
                .views
                .remove_view_if_generation(created.view, &created.generation);
            self.cx.views.remove_window(created.window);
        }
        let result = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let Some(result) = result else {
            return;
        };
        let done = self
            .done
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(done) = done {
            done(result.map(|()| None));
        }
    }
}

pub(crate) fn export_headless(
    cx: &HostContext,
    input: &str,
    out: &str,
    _format: &str,
    _request: &HeadlessExportRequest,
    operation: &NativeInvocationOperation,
    done: Done,
) -> Result<(), String> {
    let mut accepted = Err("Sheets headless export requires the registered core thread".to_owned());
    let expected_url =
        crate::app_scheme::webview_url(URL).ok_or("The Sheets renderer URL is unavailable")?;
    let input = input.to_owned();
    let out = out.to_owned();
    let cx = cx.clone();
    let operation = operation.clone();
    let mut done = Some(done);
    crate::host::w::on_core_services_local(|_, _| {
        if shell_ipc_d1::sheets::views().is_none() {
            accepted = Err("The Sheets editor is unavailable".into());
            return;
        }
        let job = Arc::new(Export {
            id: NEXT_EXPORT.fetch_add(1, Ordering::Relaxed),
            native_receipt: Mutex::new(None),
            cx: cx.clone(),
            created: OnceLock::new(),
            creation_finished: AtomicBool::new(false),
            operation: operation.clone(),
            cancellation: Mutex::new(Vec::new()),
            actor_started: AtomicBool::new(false),
            committed: AtomicBool::new(false),
            registered: OnceLock::new(),
            browser: OnceLock::new(),
            closing: Mutex::new(None),
            closed: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            actor_drained: AtomicBool::new(false),
            window_terminal: AtomicBool::new(false),
            cleanup_started: AtomicBool::new(false),
            cleanup_finished: AtomicBool::new(false),
            result: Mutex::new(None),
            done: Mutex::new(done.take()),
            deadline: Arc::new(Deadline::default()),
        });
        for cancel in [operation.cancel(), operation.shutdown()] {
            let weak = Arc::downgrade(&job);
            let subscription = cancel.on_cancel(move || {
                if let Some(job) = weak.upgrade() {
                    job.finish(Err("The Sheets headless invocation was cancelled".into()));
                }
            });
            job.cancellation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(subscription);
        }
        let ui = cx.ui.clone();
        let creation_guard = CreationGuard(Some(job.clone()));
        ui.post(Box::new(move || {
            let mut creation_guard = creation_guard;
            let reserved = crate::ipc::native_owner::run_native_invocation_ui(
                job.cx.ui.as_ref(), &job.operation, || job.cx.windows.reserve_deferred_window(Surface::Sheets),
            ).and_then(|result| result);
            let reserved = match reserved {
                Ok(reserved) => reserved,
                Err(error) => {
                    job.creation_finished.store(true, Ordering::Release);
                    job.finish(Err(error));
                    drop(creation_guard.0.take());
                    return;
                }
            };
            let view = reserved.view;
            let generation = reserved.created.clone();
            let proof = reserved.proof.clone();
            let window_lifecycle = reserved.window_lifecycle.clone();
            let target = match job.operation.reserve_target(view, generation.clone()) {
                Ok(target) => target,
                Err(error) => {
                    drop(reserved);
                    job.creation_finished.store(true, Ordering::Release);
                    job.finish(Err(error));
                    drop(creation_guard.0.take());
                    return;
                }
            };
            let created = Created {
                window: reserved.window, view, generation, webview: OnceLock::new(),
                target: target.clone(), proof: proof.clone(), window_lifecycle: window_lifecycle.clone(),
            };
            if job.created.set(created).is_err() {
                drop(reserved);
                job.creation_finished.store(true, Ordering::Release);
                job.finish(Err("The hidden Sheets reservation was installed twice".into()));
                drop(creation_guard.0.take());
                return;
            }
            let window_job = job.clone();
            if let Err(error) = window_lifecycle.when_terminal(Box::new(move |_| {
                window_job.window_terminal.store(true, Ordering::Release);
                window_job.complete();
            })) {
                drop(reserved);
                job.creation_finished.store(true, Ordering::Release);
                job.finish(Err(error));
                job.report_cleanup_failure();
                drop(creation_guard.0.take());
                return;
            }
            let closed_job = job.clone();
            let subscription = match proof.when_closed(move |receipt| {
                let Some(created) = closed_job.created.get() else { return; };
                if !receipt.matches(created.proof.reservation(), created.view, &created.generation) {
                    closed_job.finish(Err("The Sheets browser closure did not match its reservation".into()));
                    return;
                }
                *closed_job.native_receipt.lock().unwrap_or_else(|error| error.into_inner()) = Some(receipt.clone());
                closed_job.closed.store(true, Ordering::Release);
                if !closed_job.settled.load(Ordering::Acquire) {
                    let message = match receipt.outcome() {
                        shell_contract::ipc::NativeClosureOutcome::Closed => "sheets renderer stopped (destroyed)",
                        shell_contract::ipc::NativeClosureOutcome::NotCreated => "The hidden Sheets browser could not be created",
                    };
                    closed_job.finish(Err(message.into()));
                }
                closed_job.close_created();
            }) {
                Ok(subscription) => subscription,
                Err(error) => {
                    drop(reserved);
                    job.creation_finished.store(true, Ordering::Release);
                    job.finish(Err(error));
                    job.report_cleanup_failure();
                    drop(creation_guard.0.take());
                    return;
                }
            };
            *job.closing.lock().unwrap_or_else(|error| error.into_inner()) = Some(subscription);
            let weak = Arc::downgrade(&job);
            let lost = target.operation().loss_token().on_cancel(move || {
                if let Some(job) = weak.upgrade() { job.finish(Err("The Sheets headless invocation ended".into())); }
            });
            job.cancellation.lock().unwrap_or_else(|error| error.into_inner()).push(lost);
            let core_job = job.clone();
            let unavailable = job.clone();
            crate::host::w::post_core_checked(move |_, _| {
                let mut creation_guard = creation_guard;
                if core_job.settled.load(Ordering::Acquire) {
                    drop(reserved);
                    core_job.creation_finished.store(true, Ordering::Release);
                    core_job.close_created();
                    drop(creation_guard.0.take());
                    return;
                }
                let Some(editor) = shell_ipc_d1::sheets::views() else {
                    drop(reserved);
                    core_job.creation_finished.store(true, Ordering::Release);
                    core_job.finish(Err("The Sheets editor is unavailable".into()));
                    core_job.close_created();
                    drop(creation_guard.0.take());
                    return;
                };
                let finished = core_job.clone();
                let registration = editor.begin_headless_export(view, target, &input, &out, Box::new(move |report| {
                    finished.finish(if report.ok { Ok(()) } else { Err(report.error.unwrap_or_else(|| "sheets export failed".into())) });
                }), proof.reservation(), proof.owner());
                let registration = match registration {
                    Ok(registration) => registration,
                    Err(error) => {
                        drop(reserved);
                        core_job.creation_finished.store(true, Ordering::Release);
                        core_job.finish(Err(error));
                        core_job.close_created();
                        drop(creation_guard.0.take());
                        return;
                    }
                };
                REGISTRATIONS.with(|registrations| { registrations.borrow_mut().insert(core_job.id, Rc::new(registration)); });
                core_job.actor_started.store(true, Ordering::Release);
                let timer = core_job.deadline.clone();
                let timed = Arc::downgrade(&core_job);
                if let Err(error) = std::thread::Builder::new().name("sheets-headless-deadline".into()).spawn(move || {
                    if timer.expired(TIMEOUT)
                        && let Some(job) = timed.upgrade()
                    {
                        job.finish(Err("sheets export timed out after 600000ms".into()));
                    }
                }) {
                    drop(reserved);
                    core_job.creation_finished.store(true, Ordering::Release);
                    core_job.finish(Err(error.to_string()));
                    core_job.close_created();
                    drop(creation_guard.0.take());
                    return;
                }
                let ui = core_job.cx.ui.clone();
                ui.post(Box::new(move || {
                    let mut creation_guard = creation_guard;
                    if core_job.settled.load(Ordering::Acquire) {
                        drop(reserved);
                        core_job.creation_finished.store(true, Ordering::Release);
                        core_job.close_created();
                        drop(creation_guard.0.take());
                        return;
                    }
                    let frame_job = core_job.clone();
                    let navigation_url = expected_url.clone();
                    let navigated = AtomicBool::new(false);
                    let options = WindowOptions {
                        background_throttling: Some(tauri::utils::config::BackgroundThrottlingPolicy::Disabled),
                        on_navigation: Some(Arc::new(move |url| {
                            if url.as_str() == "about:blank" { return !navigated.load(Ordering::Acquire); }
                            url == &navigation_url && !navigated.swap(true, Ordering::AcqRel)
                        })),
                        on_open: Some(Arc::new(|_, _, _| {})),
                        on_frame: Some(Arc::new(move |_, event| {
                            match &event.kind {
                                NativeEventKind::DocumentCommitted { url, .. } => {
                                    if frame_job.committed.load(Ordering::Acquire) {
                                        frame_job.finish(Err("The hidden Sheets document was replaced".into()));
                                    } else if url == &expected_url { frame_job.committed.store(true, Ordering::Release); }
                                }
                                NativeEventKind::DocumentRegistered { context: origin }
                                    if frame_job.committed.load(Ordering::Acquire)
                                    && event.browser.is_same_browser(origin.browser()) => {
                                    if let Some(first) = frame_job.registered.get() {
                                        if first != origin { frame_job.finish(Err("The hidden Sheets native document was replaced".into())); }
                                    } else if let Some(created) = frame_job.created.get() {
                                        let Some(captured) = frame_job.cx.views.capture_native_owner(created.view, &created.generation) else {
                                            frame_job.finish(Err("The Sheets native browser owner is unavailable".into())); return;
                                        };
                                        if captured.origin != *origin || captured.surface != Surface::Sheets || captured.browser_owner.is_none() {
                                            frame_job.finish(Err("The Sheets native browser registration changed".into())); return;
                                        }
                                        let Some(owner) = crate::ipc::native_owner::owner_with_browser(
                                            created.view, captured.origin, captured.renderer, (), captured.browser_owner,
                                        ) else {
                                            frame_job.finish(Err("The Sheets native registration could not be authenticated".into())); return;
                                        };
                                        if let Err(error) = crate::ipc::native_owner::register_native_invocation_target(&created.target, owner) {
                                            frame_job.finish(Err(error)); return;
                                        }
                                        let _ = frame_job.registered.set(origin.clone());
                                    }
                                }
                                NativeEventKind::DocumentRevoked { document, .. }
                                    if frame_job.registered.get().is_some_and(|first| first.identity() == document) => {
                                    frame_job.finish(Err("sheets renderer stopped (document revoked)".into()));
                                }
                                NativeEventKind::WebviewProcessFailed { .. } => {
                                    frame_job.finish(Err("sheets renderer stopped (terminated)".into()));
                                }
                                NativeEventKind::NavigationFailed { .. } => frame_job.finish(Err("The Sheets renderer did not load".into())),
                                _ => {}
                            }
                        })),
                        ..Default::default()
                    };
                    let spec = WindowSpec {
                        title: "Sheets".into(), size: (1440.0, 900.0), min_size: None, visible: false,
                        decorations: true, transparent: false, resizable: true, parent: None, focus: false, main: false,
                        view: ViewSpec { url: URL.into(), bridge: Bridge::Surface(Surface::Sheets) },
                    };
                    let result = crate::ipc::native_owner::run_native_invocation_ui(
                        core_job.cx.ui.as_ref(), &core_job.operation,
                        || core_job.cx.windows.open_reserved_deferred_window(spec, options, reserved),
                    ).and_then(|result| result);
                    if let Err(error) = result {
                        core_job.creation_finished.store(true, Ordering::Release);
                        core_job.finish(Err(error));
                        core_job.close_created();
                        drop(creation_guard.0.take());
                        return;
                    }
                    let Some(webview) = core_job.cx.windows.webview(view) else {
                        core_job.creation_finished.store(true, Ordering::Release);
                        core_job.finish(Err("The hidden Sheets webview was not constructed".into()));
                        core_job.close_created();
                        drop(creation_guard.0.take());
                        return;
                    };
                    if let Some(created) = core_job.created.get() { let _ = created.webview.set(webview.clone()); }
                    core_job.creation_finished.store(true, Ordering::Release);
                    if core_job.settled.load(Ordering::Acquire) {
                        core_job.close_created();
                        drop(creation_guard.0.take());
                        return;
                    }
                    let navigation = core_job.clone();
                    match webview.native_browser() {
                        Ok(browser) => {
                            let browser_id = browser.id();
                            browser.navigator().when_ready(move |result| {
                                if let Err(error) = result { navigation.finish(Err(error)); return; }
                                if navigation.settled.load(Ordering::Acquire) { return; }
                                if navigation.browser.set(browser_id).is_err() {
                                    navigation.finish(Err("The hidden Sheets browser identity was bound twice".into())); return;
                                }
                                let bound = navigation.clone();
                                let ui = bound.cx.ui.clone();
                                ui.post(Box::new(move || {
                                    let Some(created) = bound.created.get() else { return; };
                                    if bound.settled.load(Ordering::Acquire) { return; }
                                    let result = crate::ipc::native_owner::run_native_invocation_created_ui(
                                        bound.cx.ui.as_ref(), &created.target, || bound.cx.windows.load_view(view, URL),
                                    ).and_then(|result| result);
                                    if let Err(error) = result { bound.finish(Err(error)); }
                                }));
                            });
                        }
                        Err(error) => core_job.finish(Err(error)),
                    }
                    drop(creation_guard.0.take());
                }));
            }, move || unavailable.finish(Err("The Sheets core services are unavailable".into())));
        }));
        accepted = Ok(());
    });
    accepted
}

#[cfg(test)]
mod tests {
    use super::Deadline;
    use std::time::Duration;

    #[test]
    fn cleanup_waits_for_browser_window_and_worker_drain_in_any_order() {
        for browser in [false, true] {
            for drain in [false, true] {
                for window in [false, true] {
                    assert_eq!(
                        super::terminal_cleanup_ready(browser, drain, window),
                        browser && drain && window
                    );
                }
            }
        }
    }

    #[test]
    fn deadline_stops_without_waiting_for_the_export_timeout() {
        let deadline = Deadline::default();
        deadline.stop();
        assert!(!deadline.expired(Duration::from_secs(600)));
    }

    #[test]
    fn an_unsettled_export_reaches_its_deadline() {
        assert!(Deadline::default().expired(Duration::ZERO));
    }
}
