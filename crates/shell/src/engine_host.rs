//! Exact JavaScript dependency steps left at the engine host boundary.
//! Replies bypass the core dispatcher: a synchronous native engine call may
//! hold the core thread while the isolated service page computes its answer.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::native::{
    BridgeMode, DocumentContext, DocumentEvaluation, Native, NativeEvent, NativeEventKind,
    NativeSpec, RequestPolicy, WebviewWindowBuilderExt,
};
use serde_json::Value;
use seu_host::HostError;
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelToken, Thread};
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};

mod html;
mod service;
use service::{NativeService, ServiceProof};

const LABEL: &str = "seuoffice-engine-host";
const HOST: &str = "engine-host";
const WAIT: Duration = Duration::from_secs(120);
type Reply = Result<Value, HostError>;

/// The engine page tells the shell what it is doing with `GET REPORT_PATH?m=<text>` on its own
/// scheme (`engine-host/boot.ts`). The request needs nothing of the IPC that readiness depends on,
/// so a page that cannot reach the IPC still leaves its reason in the trace and in the error of a
/// readiness that never came.
const REPORT_PATH: &str = "/__engine-host-log";
/// The longest report taken, in characters
const REPORT_CHARS: usize = 600;
/// How many reports are kept, and how many of them the error of a missed readiness shows
const REPORTS_KEPT: usize = 16;
const REPORTS_SHOWN: usize = 6;

/// What the engine page reported about itself, oldest first
#[derive(Default)]
struct PageReports(VecDeque<String>);

impl PageReports {
    fn note(&mut self, text: String) {
        if self.0.len() == REPORTS_KEPT {
            self.0.pop_front();
        }
        self.0.push_back(text);
    }

    /// What to add to the error of a readiness that never came
    fn status(&self) -> String {
        if self.0.is_empty() {
            return "the engine page reported nothing: its boot script did not run, or its \
                    requests did not reach the shell"
                .to_owned();
        }
        let shown: Vec<&str> = self
            .0
            .iter()
            .skip(self.0.len().saturating_sub(REPORTS_SHOWN))
            .map(String::as_str)
            .collect();
        format!("the engine page reported: {}", shown.join(" | "))
    }
}

struct ServiceState {
    ready: Result<bool, String>,
    pending: HashMap<u64, PendingRequest>,
}
#[derive(Default)]
struct CallbackDrain(Mutex<CallbackDrainState>);
#[derive(Default)]
struct CallbackDrainState {
    active: usize,
    waiters: Vec<Box<dyn FnOnce() + Send>>,
}
struct CallbackWork(Arc<CallbackDrain>);
impl CallbackDrain {
    fn begin(self: &Arc<Self>) -> CallbackWork {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).active += 1;
        CallbackWork(self.clone())
    }
    fn after_idle(&self, callback: impl FnOnce() + Send + 'static) {
        let mut callback: Option<Box<dyn FnOnce() + Send>> = Some(Box::new(callback));
        {
            let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
            if state.active != 0 {
                state.waiters.push(callback.take().unwrap());
            }
        }
        if let Some(callback) = callback {
            callback();
        }
    }
}
impl Drop for CallbackWork {
    fn drop(&mut self) {
        let waiters = {
            let mut state = self.0.0.lock().unwrap_or_else(|p| p.into_inner());
            state.active -= 1;
            if state.active == 0 {
                std::mem::take(&mut state.waiters)
            } else {
                Vec::new()
            }
        };
        for waiter in waiters {
            waiter();
        }
    }
}

struct PendingRequest {
    service: String,
    nonce: String,
    input: Arc<Value>,
    slides_owner: Option<shell_ipc_d1::slides::engine::RenderOwner>,
    origin: Option<OwnedRequest>,
    service_owner: Option<ServiceProof>,
    evaluation: Option<DocumentEvaluation>,
    cancel: CancelToken,
    callbacks: HashSet<u64>,
    callback_drain: Arc<CallbackDrain>,
    progress: Option<mpsc::Sender<(f64, f64)>>,
    reply: mpsc::SyncSender<Reply>,
}

#[derive(Clone)]
enum OwnedRequest {
    Interactive {
        owner: NativeRequestOwner,
        destination: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
    },
    InteractiveMany {
        owner: NativeRequestOwner,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
    },
    NativeInvocation {
        target: shell_contract::ipc::NativeInvocationTarget,
        owner: NativeRequestOwner,
        cancel: CancelToken,
        pending: CancelToken,
    },
    Recovery {
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: NativeRequestOwner,
        cancel: CancelToken,
    },
    NativeCommandPreparation {
        operation: shell_contract::ipc::NativeInvocationOperation,
        expected: shell_contract::ipc::NativeEditorCommand,
        startup: Option<(
            shell_contract::ipc::NativeInvocationOperation,
            NativeRequestOwner,
        )>,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
    },
    ExistingDocumentSave {
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        participants: Vec<NativeRequestOwner>,
        cancel: CancelToken,
        pending: CancelToken,
        additional: Vec<CancelToken>,
    },
}

impl OwnedRequest {
    fn with_active<T>(&self, effect: impl FnOnce() -> T) -> Option<T> {
        match self {
            Self::Interactive {
                owner,
                destination,
                cancel,
                pending,
            } => crate::ipc::native_owner::with_paired_effect(
                owner,
                destination,
                cancel,
                pending,
                effect,
            ),
            Self::InteractiveMany {
                owner,
                participants,
                cancel,
                pending,
            } => crate::ipc::native_owner::with_many_owner_transition(
                owner,
                participants,
                cancel,
                pending,
                |_| effect(),
            ),
            Self::NativeInvocation {
                target,
                owner,
                cancel,
                pending,
            } => crate::ipc::native_owner::with_native_invocation_effect(
                target, owner, cancel, pending, effect,
            ),
            Self::Recovery {
                receipt,
                receiver,
                cancel,
            } => crate::ipc::native_owner::with_recovery_open_effect(
                receipt, receiver, cancel, effect,
            ),
            Self::NativeCommandPreparation {
                operation,
                expected,
                startup,
                participants,
                cancel,
                pending,
            } => match startup {
                Some((root, shell)) => {
                    crate::ipc::native_owner::with_startup_command_preparation_effect(
                        root,
                        shell,
                        operation,
                        expected,
                        participants,
                        cancel,
                        pending,
                        effect,
                    )
                }
                None => crate::ipc::native_owner::with_native_command_preparation_effect(
                    operation,
                    expected,
                    participants,
                    cancel,
                    pending,
                    effect,
                ),
            },
            Self::ExistingDocumentSave {
                binding,
                participants,
                cancel,
                pending,
                additional,
            } => {
                let additional: Vec<_> = additional.iter().collect();
                crate::ipc::native_owner::with_existing_document_save_many_effect(
                    binding,
                    participants,
                    cancel,
                    pending,
                    &additional,
                    effect,
                )
            }
        }
    }

    fn with_service<T>(&self, service: &ServiceProof, effect: impl FnOnce() -> T) -> Option<T> {
        match self {
            Self::Interactive {
                owner,
                destination,
                cancel,
                pending,
            } => crate::ipc::native_owner::with_service_pair_effect(
                owner,
                destination,
                &service.origin,
                service.generation(),
                cancel,
                pending,
                effect,
            ),
            Self::InteractiveMany {
                owner,
                participants,
                cancel,
                pending,
            } => crate::ipc::native_owner::with_service_many_effect(
                owner,
                participants,
                &service.origin,
                service.generation(),
                cancel,
                pending,
                effect,
            ),
            Self::NativeInvocation {
                target,
                owner,
                cancel,
                pending,
            } => crate::ipc::native_owner::with_service_native_invocation_effect(
                target,
                owner,
                &service.origin,
                service.generation(),
                cancel,
                pending,
                effect,
            ),
            Self::Recovery {
                receipt,
                receiver,
                cancel,
            } => crate::ipc::native_owner::with_service_recovery_open_effect(
                receipt,
                receiver,
                &service.origin,
                service.generation(),
                cancel,
                effect,
            ),
            Self::NativeCommandPreparation {
                operation,
                expected,
                startup,
                participants,
                cancel,
                pending,
            } => crate::ipc::native_owner::with_command_preparation_service_effect(
                operation,
                expected,
                startup.as_ref().map(|(root, shell)| (root, shell)),
                participants,
                &service.origin,
                service.generation(),
                cancel,
                pending,
                effect,
            ),
            Self::ExistingDocumentSave {
                binding,
                participants,
                cancel,
                pending,
                additional,
            } => {
                let additional: Vec<_> = additional.iter().collect();
                crate::ipc::native_owner::with_existing_document_service_effect(
                    binding,
                    participants,
                    &service.origin,
                    service.generation(),
                    cancel,
                    pending,
                    &additional,
                    effect,
                )
            }
        }
    }

    fn cancellation_tokens(&self, external: Option<&CancelToken>) -> Vec<CancelToken> {
        let mut tokens: Vec<CancelToken> = external.cloned().into_iter().collect();
        match self {
            Self::Interactive {
                owner,
                destination,
                cancel,
                pending,
            } => {
                tokens.extend(
                    [
                        Some(cancel.clone()),
                        Some(pending.clone()),
                        owner.loss_token(),
                        destination.loss_token(),
                    ]
                    .into_iter()
                    .flatten(),
                );
            }
            Self::InteractiveMany {
                owner,
                participants,
                cancel,
                pending,
            } => {
                tokens.extend([cancel.clone(), pending.clone()]);
                tokens.extend(owner.loss_token());
                tokens.extend(
                    participants
                        .iter()
                        .filter_map(NativeRequestOwner::loss_token),
                );
            }
            Self::NativeInvocation {
                target,
                owner,
                cancel,
                pending,
            } => {
                tokens.extend(
                    [
                        Some(cancel.clone()),
                        Some(pending.clone()),
                        owner.loss_token(),
                        Some(target.operation().cancel().clone()),
                        Some(target.operation().shutdown().clone()),
                        Some(target.operation().loss_token()),
                    ]
                    .into_iter()
                    .flatten(),
                );
            }
            Self::Recovery {
                receipt,
                receiver,
                cancel,
            } => {
                tokens.push(cancel.clone());
                tokens.extend(receiver.loss_token());
                tokens.push(receipt.target().created().loss_token());
                if let shell_contract::ipc::RecoveryOpenContext::NativeStartup(operation) =
                    receipt.target().context()
                {
                    tokens.extend([
                        operation.cancel().clone(),
                        operation.shutdown().clone(),
                        operation.loss_token(),
                    ]);
                }
            }
            Self::NativeCommandPreparation {
                operation,
                startup,
                participants,
                cancel,
                pending,
                ..
            } => {
                tokens.extend([
                    cancel.clone(),
                    pending.clone(),
                    operation.cancel().clone(),
                    operation.shutdown().clone(),
                    operation.loss_token(),
                ]);
                tokens.extend(
                    participants
                        .iter()
                        .filter_map(NativeRequestOwner::loss_token),
                );
                if let Some((root, shell)) = startup {
                    tokens.extend([
                        root.cancel().clone(),
                        root.shutdown().clone(),
                        root.loss_token(),
                    ]);
                    tokens.extend(shell.loss_token());
                }
            }
            Self::ExistingDocumentSave {
                binding,
                participants,
                cancel,
                pending,
                additional,
            } => {
                let operation = binding.operation();
                tokens.extend([
                    cancel.clone(),
                    pending.clone(),
                    operation.cancel().clone(),
                    operation.shutdown().clone(),
                    operation.loss_token(),
                ]);
                tokens.extend(binding.owner().loss_token());
                tokens.extend(additional.iter().cloned());
                tokens.extend(
                    participants
                        .iter()
                        .filter_map(NativeRequestOwner::loss_token),
                );
            }
        }
        tokens
    }
}

struct SlidesRequest {
    owner: shell_ipc_d1::slides::engine::RenderOwner,
    env: crate::host::d1::slides_fonts::FontEnvironment,
    renderer: Box<dyn crate::host::d1::slides_fonts::FontRenderer>,
}

struct SlidesCleanup(u64);
impl Drop for SlidesCleanup {
    fn drop(&mut self) {
        crate::host::d1::slides_fonts::close_request(self.0);
    }
}

fn error(name: &str, message: impl Into<String>) -> HostError {
    HostError {
        name: name.to_owned(),
        message: message.into(),
    }
}

fn next_request_id(next: &AtomicU64) -> Result<u64, HostError> {
    next.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
        (id <= 9_007_199_254_740_991).then(|| id + 1)
    })
    .map_err(|_| error("HostUnavailable", "engine request ids exhausted"))
}

fn cancel_script(proof: Option<&ServiceProof>, id: u64) {
    if let Some(proof) = proof {
        service::cancel(proof, id);
    }
}

fn close_dependencies(id: u64) {
    html::close_request(id);
    crate::host::d2::pdf::native_callbacks::close_request(id);
    crate::host::d2::pdf::pdfium_session::close_request(id);
    crate::host::d2::pdf::native_engine::document_edits::session::close_request(id);
    crate::host::d1::slides_fonts::close_request(id);
}

pub struct EngineHost {
    app: AppHandle,
    ui: crate::TauriUi,
    token: String,
    service: NativeService,
    state: (Mutex<ServiceState>, Condvar),
    next: AtomicU64,
    page_reports: Mutex<PageReports>,
}

impl EngineHost {
    pub fn install(app: &AppHandle) -> Result<Arc<Self>, String> {
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|e| format!("engine host identity: {e}"))?;
        let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let host = Arc::new(Self {
            app: app.clone(),
            ui: crate::TauriUi {
                app: app.clone(),
                main: std::thread::current().id(),
            },
            token,
            service: NativeService::new(),
            state: (
                Mutex::new(ServiceState {
                    ready: Ok(false),
                    pending: HashMap::new(),
                }),
                Condvar::new(),
            ),
            next: AtomicU64::new(1),
            page_reports: Mutex::default(),
        });
        if !app.manage(host.clone()) {
            return Err("engine host was installed twice".to_owned());
        }
        crate::host::d2::pdf::native_callbacks::configure(crate::paths::resources_path(app))?;
        let url = crate::app_scheme::webview_url("genoffice-app://engine-host/index.html")
            .ok_or("invalid engine service URL")?;
        let allowed = url.clone();
        let bootstrap = format!(
            "window.__SEUOFFICE_ENGINE_TOKEN__={};",
            serde_json::to_string(&host.token).map_err(|e| e.to_string())?
        );
        let crashed = host.clone();
        // The service page navigates exactly once: top-level navigations other than its own
        // URL are refused; its sub-resources are not the policy's business.
        let policy = RequestPolicy::new(move |request| {
            !request.main_frame || request.url == allowed.as_str()
        });
        let spec = NativeSpec::new()
            .bridge(BridgeMode::Invoke)
            .request_policy(policy)
            .native_events(Arc::new(move |event: &NativeEvent| {
                if matches!(event.kind, NativeEventKind::WebviewProcessFailed { .. }) {
                    let host = crashed.clone();
                    std::thread::spawn(move || host.close());
                }
            }));
        let native = app
            .try_state::<Native>()
            .ok_or("the native module is not installed")?;
        let (builder, browser) = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(url))
            .native(&native, LABEL, spec)?;
        let view = builder
            .title("SeuOffice engine service")
            .visible(false)
            .focused(false)
            .skip_taskbar(true)
            .initialization_script(bootstrap)
            .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
            .build()
            .map_err(|e| format!("opening the engine service: {e}"))?;
        native
            .attach(AsRef::<tauri::Webview>::as_ref(&view), &browser)
            .map_err(|error| format!("attaching the engine service: {error}"))?;
        let native = host.clone();
        seu_host::register_native_host(move |name, input| native.call(name, input))
            .map_err(|e| e.to_string())?;
        Ok(host)
    }

    fn wait_ready_until(
        &self,
        deadline: Option<Instant>,
        cancel: Option<&CancelToken>,
    ) -> Result<(), HostError> {
        if self.ui.is_current() {
            return Err(error(
                "HostUnavailable",
                "the engine service cannot wait on its UI thread",
            ));
        }
        let (lock, changed) = &self.state;
        let mut state = lock
            .lock()
            .map_err(|_| error("HostUnavailable", "engine service state was poisoned"))?;
        loop {
            if let Err(reason) = &state.ready {
                return Err(error("HostUnavailable", reason.clone()));
            }
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(error("AbortError", "the engine operation was cancelled"));
            }
            if matches!(&state.ready, Ok(true)) {
                return Ok(());
            }
            let remaining = deadline.map(|end| end.saturating_duration_since(Instant::now()));
            if remaining.is_some_and(|duration| duration.is_zero()) {
                return Err(error(
                    "TimeoutError",
                    format!(
                        "the engine operation timed out while waiting for readiness; {}",
                        self.page_status()
                    ),
                ));
            }
            let wait = remaining
                .unwrap_or(Duration::from_secs(3600))
                .min(Duration::from_millis(50));
            let (next, _) = changed
                .wait_timeout(state, wait)
                .map_err(|_| error("HostUnavailable", "engine service state was poisoned"))?;
            state = next;
        }
    }

    pub fn wait_ready(&self) -> Result<(), String> {
        self.wait_ready_until(Some(Instant::now() + WAIT), None)
            .map_err(|e| e.message)
    }

    fn page_status(&self) -> String {
        self.page_reports
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status()
    }

    /// Why the engine is not ready yet, or `None` once its page said it is
    pub fn waiting_for(&self) -> Option<String> {
        let ready = self
            .state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ready
            .clone();
        match ready {
            Ok(true) => None,
            Ok(false) => Some(self.page_status()),
            Err(reason) => Some(reason),
        }
    }

    pub fn call(self: &Arc<Self>, name: &str, input: &Value) -> Reply {
        self.call_with_options(name, input, Some(WAIT), None)
    }

    pub fn call_cancellable(
        self: &Arc<Self>,
        name: &str,
        input: &Value,
        cancel: Option<&CancelToken>,
    ) -> Reply {
        self.call_with_options(name, input, Some(WAIT), cancel)
    }

    pub fn call_with_options(
        self: &Arc<Self>,
        name: &str,
        input: &Value,
        timeout: Option<Duration>,
        cancel: Option<&CancelToken>,
    ) -> Reply {
        self.call_with_progress_inner(name, input, timeout, cancel, None, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn call_with_progress_inner(
        self: &Arc<Self>,
        name: &str,
        input: &Value,
        timeout: Option<Duration>,
        cancel: Option<&CancelToken>,
        progress: Option<&(dyn Fn(f64, f64) + Sync)>,
        slides: Option<SlidesRequest>,
        origin: Option<OwnedRequest>,
    ) -> Reply {
        let deadline = timeout
            .map(|timeout| {
                Instant::now()
                    .checked_add(timeout)
                    .ok_or_else(|| error("Error", "engine timeout is too large"))
            })
            .transpose()?;
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(error("AbortError", "the engine operation was cancelled"));
        }
        if let Some(origin) = &origin {
            origin.with_active(|| ()).ok_or_else(|| {
                error(
                    "AbortError",
                    "the engine request owner is no longer current",
                )
            })?;
        }
        let id = next_request_id(&self.next)?;
        let mut random = [0u8; 24];
        getrandom::fill(&mut random)
            .map_err(|e| error("HostUnavailable", format!("engine request nonce: {e}")))?;
        let nonce: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let script = format!(
            "void window.__seuofficeEngineRequest({id},{},{},{})",
            serde_json::to_string(name).map_err(|e| error("Error", e.to_string()))?,
            serde_json::to_string(input).map_err(|e| error("Error", e.to_string()))?,
            serde_json::to_string(&nonce).map_err(|e| error("Error", e.to_string()))?
        );
        let (send, receive) = mpsc::sync_channel(1);
        let (progress_tx, progress_rx) = mpsc::channel();
        let request_cancel = CancelToken::default();
        let _slides_cleanup = slides.as_ref().map(|_| SlidesCleanup(id));
        let slides_owner = if let Some(slides) = slides {
            let deck = input
                .get("deckId")
                .and_then(Value::as_u64)
                .ok_or_else(|| error("HostUnavailable", "slide deck identity missing"))?;
            let generation = input
                .get("generation")
                .and_then(Value::as_u64)
                .ok_or_else(|| error("HostUnavailable", "slide generation missing"))?;
            let view = input
                .get("ownerViewId")
                .and_then(Value::as_u64)
                .ok_or_else(|| error("HostUnavailable", "slide owner missing"))?;
            let authority = crate::host::d1::slides_engine::owner_authority(&slides.owner)
                .map_err(|failure| error("AbortError", failure.0))?;
            if view != u64::from(slides.owner.view.0) {
                return Err(error(
                    "AbortError",
                    "slide document authorization is no longer current",
                ));
            }
            crate::host::d1::slides_fonts::install_request(
                id,
                shell_ipc_d1::slides::engine::DeckId(deck),
                generation,
                authority.authorization.clone(),
                slides.env,
                slides.renderer,
            )
            .map_err(|message| error("HostUnavailable", message))?;
            Some(slides.owner)
        } else {
            None
        };
        {
            let mut state = self
                .state
                .0
                .lock()
                .map_err(|_| error("HostUnavailable", "engine request state was poisoned"))?;
            if slides_owner.as_ref().is_some_and(|owner| {
                crate::host::d1::slides_engine::owner_authority(owner).is_err()
            }) {
                return Err(error(
                    "AbortError",
                    "slide document authorization is no longer current",
                ));
            }
            if name == "docs.htmlToDocx" {
                use shell_contract::runtime::Views;
                let owner = input
                    .get("ownerViewId")
                    .and_then(Value::as_u64)
                    .and_then(|id| u32::try_from(id).ok())
                    .ok_or_else(|| {
                        error("HostUnavailable", "HTML conversion has no owning view")
                    })?;
                let ipc = self
                    .app
                    .try_state::<crate::ipc::Ipc>()
                    .ok_or_else(|| error("HostUnavailable", "view table is unavailable"))?;
                if ipc.views.sender_of(shell_contract::ViewId(owner)).is_none() {
                    return Err(error("AbortError", "the owning view was destroyed"));
                }
            }
            match &state.ready {
                Ok(_) => {
                    state.pending.insert(
                        id,
                        PendingRequest {
                            service: name.to_owned(),
                            nonce,
                            input: Arc::new(input.clone()),
                            slides_owner,
                            origin: origin.clone(),
                            service_owner: None,
                            evaluation: None,
                            cancel: request_cancel.clone(),
                            reply: send,
                            callbacks: HashSet::new(),
                            callback_drain: Arc::default(),
                            progress: progress.map(|_| progress_tx),
                        },
                    );
                }
                Err(reason) => return Err(error("HostUnavailable", reason.clone())),
            }
        }
        let tokens = origin.as_ref().map_or_else(
            || cancel.cloned().into_iter().collect(),
            |origin| origin.cancellation_tokens(cancel),
        );
        let subscriptions: Vec<_> = tokens
            .iter()
            .enumerate()
            .filter_map(|(index, token)| {
                if tokens[..index]
                    .iter()
                    .any(|previous| previous.same_token(token))
                {
                    return None;
                }
                let host = self.clone();
                let request_cancel = request_cancel.clone();
                Some(token.on_cancel(move || {
                    request_cancel.cancel();
                    std::thread::spawn(move || {
                        host.abort_request(
                            id,
                            error("AbortError", "the engine operation was cancelled"),
                        );
                    });
                }))
            })
            .collect();
        if let Err(error) = self.wait_ready_until(deadline, Some(&request_cancel)) {
            let pending = self
                .state
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pending
                .remove(&id);
            if let Some(pending) = pending {
                pending.cancel.cancel();
            }
            close_dependencies(id);
            drop(subscriptions);
            return Err(error);
        }
        let service_owner = match self.service.capture() {
            Some(proof) => proof,
            None => {
                self.reject_request(
                    id,
                    error(
                        "AbortError",
                        "the native engine service owner is unavailable",
                    ),
                );
                return Err(error(
                    "AbortError",
                    "the native engine service owner is unavailable",
                ));
            }
        };
        {
            let mut state = self
                .state
                .0
                .lock()
                .map_err(|_| error("HostUnavailable", "engine request state was poisoned"))?;
            let request = state
                .pending
                .get_mut(&id)
                .ok_or_else(|| error("AbortError", "the engine request was cancelled"))?;
            request.service_owner = Some(service_owner.clone());
        }
        let host = self.clone();
        let sent = self
            .app
            .run_on_main_thread(move || {
                let request = host.state.0.lock().ok().and_then(|state| {
                    state.pending.get(&id).map(|request| {
                        (
                            request_may_start(&state, id),
                            request.origin.clone(),
                            request.cancel.clone(),
                            request.service_owner.clone(),
                        )
                    })
                });
                let Some((true, origin, pending_cancel, Some(proof))) = request else {
                    return;
                };
                // Admit this exact request/service pair, then release all gates
                // before asking the captured native evaluator to enter the webview.
                let action = || {
                    let completion = host.clone();
                    let evaluation = proof
                        .origin
                        .evaluate(script, move |result| {
                            if let Err(failure) = result {
                                completion.reject_request(
                                    id,
                                    error(
                                        "HostUnavailable",
                                        format!("native engine evaluation failed: {failure:?}"),
                                    ),
                                );
                            }
                        })
                        .map_err(|failure| {
                            error(
                                "HostUnavailable",
                                format!("native engine evaluation failed: {failure:?}"),
                            )
                        })?;
                    let mut evaluation = Some(evaluation);
                    {
                        let mut state = host.state.0.lock().map_err(|_| {
                            error("HostUnavailable", "engine request state was poisoned")
                        })?;
                        if let Some(request) = state.pending.get_mut(&id).filter(|request| {
                            !request.cancel.is_cancelled()
                                && request
                                    .service_owner
                                    .as_ref()
                                    .is_some_and(|expected| expected.same_context(&proof))
                        }) {
                            request.evaluation = evaluation.take();
                        }
                    }
                    drop(evaluation);
                    Ok(())
                };
                let accepted = match origin {
                    Some(origin) => origin
                        .with_service(&proof, || pending_cancel.with_active(|| action))
                        .flatten(),
                    None => proof
                        .with_active(|| pending_cancel.with_active(|| action))
                        .flatten(),
                };
                let result = match accepted {
                    Some(action) => action(),
                    None => Err(error(
                        "AbortError",
                        "the engine request or service owner is no longer current",
                    )),
                };
                if let Err(error) = result {
                    host.reject_request(id, error);
                }
            })
            .map_err(|e| error("HostUnavailable", e.to_string()));
        let result = match sent {
            Ok(()) => {
                if let Some(progress) = progress {
                    loop {
                        while let Ok((done, total)) = progress_rx.try_recv() {
                            progress(done, total);
                        }
                        let remaining =
                            deadline.map(|end| end.saturating_duration_since(Instant::now()));
                        if remaining.is_some_and(|duration| duration.is_zero()) {
                            self.abort_request(
                                id,
                                error("TimeoutError", "the engine operation timed out"),
                            );
                            break receive.recv().unwrap_or_else(|_| {
                                Err(error("HostUnavailable", "engine reply channel closed"))
                            });
                        }
                        let interval = remaining
                            .unwrap_or(Duration::from_millis(50))
                            .min(Duration::from_millis(50));
                        match receive.recv_timeout(interval) {
                            Ok(result) => {
                                while let Ok((done, total)) = progress_rx.try_recv() {
                                    progress(done, total);
                                }
                                break result;
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                break Err(error("HostUnavailable", "engine reply channel closed"));
                            }
                        }
                    }
                } else {
                    match deadline {
                        Some(end) => match receive
                            .recv_timeout(end.saturating_duration_since(Instant::now()))
                        {
                            Ok(result) => result,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                self.abort_request(
                                    id,
                                    error("TimeoutError", "the engine operation timed out"),
                                );
                                receive.recv().unwrap_or_else(|_| {
                                    Err(error("HostUnavailable", "engine reply channel closed"))
                                })
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => {
                                Err(error("HostUnavailable", "engine reply channel closed"))
                            }
                        },
                        None => receive.recv().unwrap_or_else(|_| {
                            Err(error("HostUnavailable", "engine reply channel closed"))
                        }),
                    }
                }
            }
            Err(e) => Err(e),
        };
        let pending = self
            .state
            .0
            .lock()
            .map_err(|_| error("HostUnavailable", "engine request state was poisoned"))?
            .pending
            .remove(&id);
        if let Some(pending) = pending {
            pending.cancel.cancel();
        }
        request_cancel.cancel();
        close_dependencies(id);
        drop(subscriptions);
        let result = result?;
        match origin {
            Some(origin) => origin
                .with_service(&service_owner, || Ok(result))
                .ok_or_else(|| {
                    error(
                        "AbortError",
                        "the engine request or service owner is no longer current",
                    )
                })?,
            None => service_owner.with_active(|| Ok(result)).ok_or_else(|| {
                error(
                    "AbortError",
                    "the native engine service owner is no longer current",
                )
            })?,
        }
    }

    /// The request must carry the service token and the nonce of the engine page's registered
    /// document. The first such document is bound as the service owner; a second one never is.
    fn authorized(
        self: &Arc<Self>,
        webview: &tauri::Webview,
        request: &tauri::ipc::Request<'_>,
        token: &str,
    ) -> Result<ServiceProof, String> {
        if token != self.token || webview.label() != LABEL {
            return Err("untrusted engine service sender".into());
        }
        let origin = crate::native::redeem(webview, request.headers())
            .ok_or("engine service native attestation is unavailable")?;
        if let Some(proof) = self.service.authenticate(origin.clone()) {
            return Ok(proof);
        }
        let captured = origin.clone();
        let loss = Arc::downgrade(self);
        if !self.service.bind(&origin, move || {
            if let Some(host) = loss.upgrade() {
                host.close_for_context(&captured);
            }
        }) {
            return Err("engine service native owner is no longer current".into());
        }
        self.service
            .authenticate(origin)
            .ok_or_else(|| "engine service native owner is no longer current".into())
    }

    /// Remove the exact request before queueing cancellation on the same UI
    /// sequence as dispatch. Its receiver and evaluator live through the
    /// cancellation acknowledgement.
    fn abort_request(self: &Arc<Self>, id: u64, failure: HostError) {
        let pending = self
            .state
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .remove(&id);
        let Some(pending) = pending else { return };
        pending.cancel.cancel();
        self.state.1.notify_all();
        let proof = pending.service_owner.clone();
        let settlement = Arc::new(Mutex::new(Some((pending, failure))));
        let Some(proof) = proof else {
            finish_cancel(&settlement, id, Ok(()));
            return;
        };
        let callback = settlement.clone();
        let queued = self.app.run_on_main_thread(move || {
            service::cancel_complete(&proof, id, move |result| {
                finish_cancel(&callback, id, result)
            });
        });
        if let Err(error) = queued {
            finish_cancel(
                &settlement,
                id,
                Err(format!("queueing engine cancellation failed: {error}")),
            );
        }
    }

    fn reject_request(self: &Arc<Self>, id: u64, failure: HostError) {
        self.abort_request(id, failure);
    }

    fn request_effect<T>(
        &self,
        proof: &ServiceProof,
        id: u64,
        effect: impl FnOnce(&mut ServiceState) -> Result<T, String>,
    ) -> Result<T, String> {
        let (origin, cancel) = {
            let state = self
                .state
                .0
                .lock()
                .map_err(|_| "engine request state was poisoned")?;
            let request = state
                .pending
                .get(&id)
                .ok_or("engine request is no longer pending")?;
            if !request
                .service_owner
                .as_ref()
                .is_some_and(|expected| expected.same_context(proof))
            {
                return Err("engine request belongs to a different native service context".into());
            }
            (request.origin.clone(), request.cancel.clone())
        };
        let work = || {
            cancel.with_active(|| {
                let mut state = self
                    .state
                    .0
                    .lock()
                    .map_err(|_| "engine request state was poisoned")?;
                effect(&mut state)
            })
        };
        match origin {
            Some(origin) => origin.with_service(proof, work),
            None => proof.with_active(work),
        }
        .flatten()
        .ok_or("engine request or service owner is no longer current")?
    }

    fn close_for_context(&self, origin: &DocumentContext) {
        if self.service.close_if_matches(origin) {
            self.close();
        }
    }

    fn close(&self) {
        self.service.close();
        let pending = self.state.0.lock().map(|mut state| {
            state.ready = Err("engine service was destroyed".to_owned());
            self.state.1.notify_all();
            std::mem::take(&mut state.pending)
        });
        if let Ok(pending) = pending {
            for (id, request) in pending {
                request.cancel.cancel();
                close_dependencies(id);
                let _ = request.reply.send(Err(error(
                    "HostUnavailable",
                    "engine service was destroyed",
                )));
            }
        }
    }
}

fn finish_cancel(
    settlement: &Mutex<Option<(PendingRequest, HostError)>>,
    id: u64,
    result: Result<(), String>,
) {
    let Some((pending, failure)) = settlement
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    else {
        return;
    };
    close_dependencies(id);
    let result = match result {
        Ok(()) => Err(failure),
        Err(message) => Err(error("HostUnavailable", message)),
    };
    if pending.service == "docs.htmlToDocx" {
        let drain = pending.callback_drain.clone();
        drain.after_idle(move || {
            html::after_close(
                id,
                Box::new(move |closed| {
                    let result = match closed {
                        Ok(()) => result,
                        Err(message) => Err(error("HostUnavailable", message)),
                    };
                    let _ = pending.reply.send(result);
                }),
            )
        });
    } else {
        let _ = pending.reply.send(result);
    }
}

fn request_may_start(state: &ServiceState, id: u64) -> bool {
    state
        .pending
        .get(&id)
        .is_some_and(|request| !request.cancel.is_cancelled())
}

pub fn owner_destroyed(view: shell_contract::ViewId) {
    let Some(host) = HOST_INSTANCE.get() else {
        return;
    };
    let pending = {
        let Ok(mut state) = host.state.0.lock() else {
            return;
        };
        let ids: Vec<u64> = state
            .pending
            .iter()
            .filter_map(|(id, request)| {
                ((request.service == "docs.htmlToDocx"
                    && request.input.get("ownerViewId").and_then(Value::as_u64)
                        == Some(u64::from(view.0)))
                    || request
                        .slides_owner
                        .as_ref()
                        .is_some_and(|owner| owner.view == view))
                .then_some(*id)
            })
            .collect();
        ids.into_iter()
            .filter_map(|id| state.pending.remove(&id).map(|request| (id, request)))
            .collect::<Vec<_>>()
    };
    for (id, request) in pending {
        request.cancel.cancel();
        let proof = request.service_owner.clone();
        std::thread::spawn(move || {
            cancel_script(proof.as_ref(), id);
            close_dependencies(id);
            let _ = request
                .reply
                .send(Err(error("AbortError", "the owning view was destroyed")));
        });
    }
}

fn target_crashed(id: u64) {
    std::thread::spawn(move || {
        let Some(host) = HOST_INSTANCE.get() else {
            return;
        };
        let request = host
            .state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pending
            .remove(&id);
        if let Some(request) = request {
            request.cancel.cancel();
            cancel_script(request.service_owner.as_ref(), id);
            close_dependencies(id);
            let _ = request.reply.send(Err(error(
                "HostUnavailable",
                "HTML conversion renderer terminated",
            )));
        }
    });
}

#[tauri::command]
pub fn engine_host_ready(
    webview: tauri::Webview,
    request: tauri::ipc::Request<'_>,
    state: State<'_, Arc<EngineHost>>,
    token: String,
) -> Result<(), String> {
    let proof = state.authorized(&webview, &request, &token)?;
    proof
        .with_active(|| {
            let mut ready = state
                .state
                .0
                .lock()
                .map_err(|_| "engine service state was poisoned")?;
            if ready.ready.is_err() {
                return Err("engine service is closed".to_owned());
            }
            ready.ready = Ok(true);
            state.state.1.notify_all();
            Ok(())
        })
        .ok_or("engine service native owner is no longer current")?
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn engine_host_reply(
    webview: tauri::Webview,
    request: tauri::ipc::Request<'_>,
    state: State<'_, Arc<EngineHost>>,
    token: String,
    id: u64,
    name: String,
    nonce: String,
    reply: Value,
) -> Result<(), String> {
    let proof = state.authorized(&webview, &request, &token)?;
    let result = decode_reply(reply)?;
    let pending = state.request_effect(&proof, id, |state| {
        let request = state
            .pending
            .get(&id)
            .ok_or("engine request is no longer pending")?;
        if request.service != name || request.nonce != nonce {
            return Err("reply does not belong to this request".into());
        }
        state
            .pending
            .remove(&id)
            .ok_or_else(|| "engine request is no longer pending".into())
    })?;
    let result = if pending
        .slides_owner
        .as_ref()
        .is_some_and(|owner| crate::host::d1::slides_engine::owner_authority(owner).is_err())
    {
        Err(error("AbortError", "slide document is no longer current"))
    } else {
        result
    };
    pending.cancel.cancel();
    close_dependencies(id);
    if pending.service == "docs.htmlToDocx" {
        let drain = pending.callback_drain.clone();
        drain.after_idle(move || {
            html::after_close(
                id,
                Box::new(move |closed| {
                    let result = match closed {
                        Ok(()) => result,
                        Err(message) => Err(error("HostUnavailable", message)),
                    };
                    let _ = deliver_engine_reply(pending, &proof, result);
                }),
            )
        });
        Ok(())
    } else {
        deliver_engine_reply(pending, &proof, result)
    }
}

fn deliver_engine_reply(
    pending: PendingRequest,
    proof: &ServiceProof,
    result: Reply,
) -> Result<(), String> {
    let delivery = || pending.reply.send(result);
    let sent = match &pending.origin {
        Some(origin) => origin.with_service(proof, delivery),
        None => proof.with_active(delivery),
    };
    match sent {
        Some(sent) => sent.map_err(|_| "engine request receiver is closed".to_owned()),
        None => pending
            .reply
            .send(Err(error(
                "AbortError",
                "the engine request or service owner is no longer current",
            )))
            .map_err(|_| "engine request receiver is closed".to_owned()),
    }
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn engine_host_callback(
    webview: tauri::Webview,
    request: tauri::ipc::Request<'_>,
    state: State<'_, Arc<EngineHost>>,
    token: String,
    id: u64,
    name: String,
    nonce: String,
    callback_id: u64,
    operation: String,
    input: Value,
) -> Result<Value, String> {
    let proof = state.authorized(&webview, &request, &token)?;
    if name == "pdf.structureOperation" && operation == "progress" {
        let done = input
            .get("done")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite())
            .ok_or("invalid progress done")?;
        let total = input
            .get("total")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite())
            .ok_or("invalid progress total")?;
        return state.request_effect(&proof, id, |inner| {
            let (_, _, progress) =
                claim_callback(inner, id, &name, &nonce, callback_id, &operation)?;
            progress
                .ok_or("this request has no progress observer")?
                .send((done, total))
                .map_err(|_| "progress receiver is closed")?;
            Ok(Value::Null)
        });
    }
    let (request_input, cancel, slides_owner, origin, work) =
        state.request_effect(&proof, id, |inner| {
            let (request_input, cancel, _) =
                claim_callback(inner, id, &name, &nonce, callback_id, &operation)?;
            let request = inner
                .pending
                .get(&id)
                .ok_or("engine request is no longer pending")?;
            let work = (name == "docs.htmlToDocx").then(|| request.callback_drain.begin());
            Ok((
                request_input,
                cancel,
                request.slides_owner.clone(),
                request.origin.clone(),
                work,
            ))
        })?;
    let app = state.app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _work = work;
        if name == "pdf.structureOperation" {
            crate::host::d2::pdf::native_callbacks::dispatch(
                id,
                &operation,
                &input,
                &request_input,
                &cancel,
            )
        } else if name == "pdf.pdfiumSession" {
            crate::host::d2::pdf::pdfium_session::dispatch(
                id,
                &operation,
                &input,
                &request_input,
                &cancel,
            )
        } else if name == "pdf.documentEditSession" {
            crate::host::d2::pdf::native_engine::document_edits::session::dispatch(
                id,
                &operation,
                &input,
                &request_input,
                &cancel,
            )
        } else if matches!(
            name.as_str(),
            "slides.renderAll"
                | "slides.renderSlide"
                | "slides.measurePageText"
                | "slides.measureAutofit"
        ) {
            let owner =
                slides_owner.ok_or("slide request has no captured document authorization")?;
            let authority =
                crate::host::d1::slides_engine::owner_authority(&owner).map_err(|error| error.0)?;
            crate::host::d1::slides_fonts::dispatch(
                id,
                &operation,
                &input,
                &request_input,
                &authority.authorization.lease(),
                &cancel,
            )
        } else {
            html::dispatch(&app, id, &request_input, &operation, &input, &cancel)
        }
    })
    .await
    .map_err(|error| error.to_string())?;
    match origin {
        Some(origin) => origin.with_service(&proof, || result),
        None => proof.with_active(|| result),
    }
    .ok_or("engine request or service owner is no longer current")?
}

type CallbackClaim = (Arc<Value>, CancelToken, Option<mpsc::Sender<(f64, f64)>>);

fn claim_callback(
    state: &mut ServiceState,
    id: u64,
    name: &str,
    nonce: &str,
    callback_id: u64,
    operation: &str,
) -> Result<CallbackClaim, String> {
    if callback_id == 0 || callback_id > 9_007_199_254_740_991 {
        return Err("invalid callback identity".into());
    }
    let request = state
        .pending
        .get_mut(&id)
        .ok_or("engine request is no longer pending")?;
    if request.service != name || request.nonce != nonce {
        return Err("callback does not belong to this request".into());
    }
    let allowed = (name == "docs.htmlToDocx"
        && matches!(
            operation,
            "html.open"
                | "html.viewport"
                | "html.goto"
                | "html.evaluate"
                | "html.script"
                | "html.initScript"
                | "html.screenshot"
                | "html.close"
        ))
        || (name == "pdf.structureOperation" && matches!(operation, "progress" | "pdf.ocr"))
        || (name == "pdf.pdfiumSession" && operation == "pdfium.next")
        || (name == "pdf.documentEditSession" && operation == "pdfDocument.next")
        || (matches!(
            name,
            "slides.renderAll"
                | "slides.renderSlide"
                | "slides.measurePageText"
                | "slides.measureAutofit"
        ) && matches!(operation, "slides.fontMetrics" | "slides.mediaResolve")
            && request.slides_owner.is_some());
    if !allowed {
        return Err("callback capability is not permitted for this service".into());
    }
    if request.cancel.is_cancelled() {
        return Err("engine request is no longer pending".into());
    }
    if !request.callbacks.insert(callback_id) {
        return Err("callback was already used".into());
    }
    Ok((
        request.input.clone(),
        request.cancel.clone(),
        request.progress.clone(),
    ))
}

fn decode_reply(mut reply: Value) -> Result<Reply, String> {
    let object = reply
        .as_object_mut()
        .ok_or("engine reply is not an object")?;
    if object.len() != 1 {
        return Err("engine reply must have one result".to_owned());
    }
    if let Some(value) = object.remove("ok") {
        return Ok(Ok(value));
    }
    let mut failure = object.remove("error").ok_or("engine reply has no result")?;
    let failure = failure
        .as_object_mut()
        .ok_or("engine error is not an object")?;
    if failure.len() != 2 {
        return Err("engine error must have a name and message".to_owned());
    }
    let Value::String(name) = failure.remove("name").ok_or("engine error has no name")? else {
        return Err("engine error has no name".to_owned());
    };
    let Value::String(message) = failure
        .remove("message")
        .ok_or("engine error has no message")?
    else {
        return Err("engine error has no message".to_owned());
    };
    Ok(Err(HostError { name, message }))
}

pub fn window_destroyed(app: &AppHandle, label: &str) {
    if label == LABEL
        && let Some(host) = app.try_state::<Arc<EngineHost>>()
    {
        let host = host.inner().clone();
        std::thread::spawn(move || host.close());
    }
}

pub(crate) fn allocate_request_id() -> Result<u64, String> {
    let host = HOST_INSTANCE
        .get()
        .ok_or("engine service is not installed")?;
    next_request_id(&host.next).map_err(|error| error.message)
}

pub fn call(name: &str, input: &Value) -> Reply {
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    host.call(name, input)
}

pub(crate) fn call_with_slides_owner(
    name: &str,
    input: &Value,
    owner: shell_ipc_d1::slides::engine::RenderOwner,
    env: crate::host::d1::slides_fonts::FontEnvironment,
    renderer: Box<dyn crate::host::d1::slides_fonts::FontRenderer>,
) -> Reply {
    if !matches!(
        name,
        "slides.renderAll"
            | "slides.renderSlide"
            | "slides.measurePageText"
            | "slides.measureAutofit"
    ) || crate::host::d1::slides_engine::owner_authority(&owner).is_err()
    {
        return Err(error("HostUnavailable", "invalid slide render authority"));
    }
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    host.call_with_progress_inner(
        name,
        input,
        Some(WAIT),
        None,
        None,
        Some(SlidesRequest {
            owner,
            env,
            renderer,
        }),
        None,
    )
}

pub fn call_cancellable(name: &str, input: &Value, cancel: &CancelToken) -> Reply {
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    host.call_cancellable(name, input, Some(cancel))
}

/// Request-owned dependency execution. Only a worker may wait for this reply.
pub fn call_owned(
    name: &str,
    input: &Value,
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
) -> Reply {
    call_owned_pair(name, input, owner, owner, cancel, cancel)
}

/// Both original identities and operation tokens survive the engine wait.
pub fn call_owned_pair(
    name: &str,
    input: &Value,
    origin: &NativeRequestOwner,
    destination: &NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Reply {
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    let ipc = host.app.try_state::<crate::ipc::Ipc>();
    require_owned_worker(&host.ui, ipc.as_ref().map(|ipc| ipc.core.as_ref()))?;
    host.call_with_progress_inner(
        name,
        input,
        Some(WAIT),
        Some(cancel),
        None,
        None,
        Some(OwnedRequest::Interactive {
            owner: origin.clone(),
            destination: destination.clone(),
            cancel: cancel.clone(),
            pending: pending.clone(),
        }),
    )
}

/// Renderer-owned diagram parsing retains every original participant.
pub(crate) fn call_owned_many(
    name: &str,
    input: &Value,
    owner: &NativeRequestOwner,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<Value, shell_contract::IpcError> {
    let run = || -> Reply {
        if name != "diagram.open" {
            return Err(error("HostUnavailable", "invalid owned diagram dependency"));
        }
        let host = HOST_INSTANCE
            .get()
            .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
        let ipc = host.app.try_state::<crate::ipc::Ipc>();
        require_owned_worker(&host.ui, ipc.as_ref().map(|ipc| ipc.core.as_ref()))?;
        host.call_with_progress_inner(
            name,
            input,
            Some(WAIT),
            Some(cancel),
            None,
            None,
            Some(OwnedRequest::InteractiveMany {
                owner: owner.clone(),
                participants: participants.to_vec(),
                cancel: cancel.clone(),
                pending: pending.clone(),
            }),
        )
    };
    run().map_err(|failure| shell_contract::IpcError {
        name: Some(failure.name),
        message: failure.message,
    })
}

/// The exact native-created invocation and first registered owner share the
/// same service admission at dispatch and reply; this never mints a sender.
pub fn call_native_invocation(
    name: &str,
    input: &Value,
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Reply {
    call_native_invocation_with_timeout(name, input, target, owner, cancel, pending, WAIT)
}

/// Like `call_native_invocation`, with the original caller's explicit deadline.
pub fn call_native_invocation_with_timeout(
    name: &str,
    input: &Value,
    target: &shell_contract::ipc::NativeInvocationTarget,
    owner: &NativeRequestOwner,
    cancel: &CancelToken,
    pending: &CancelToken,
    timeout: Duration,
) -> Reply {
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    let ipc = host.app.try_state::<crate::ipc::Ipc>();
    require_owned_worker(&host.ui, ipc.as_ref().map(|ipc| ipc.core.as_ref()))?;
    host.call_with_progress_inner(
        name,
        input,
        Some(timeout),
        Some(cancel),
        None,
        None,
        Some(OwnedRequest::NativeInvocation {
            target: target.clone(),
            owner: owner.clone(),
            cancel: cancel.clone(),
            pending: pending.clone(),
        }),
    )
}

/// Parse the exact native command before any editor target exists.
#[allow(clippy::too_many_arguments)]
pub(crate) fn call_native_command_preparation(
    name: &str,
    input: &Value,
    operation: &shell_contract::ipc::NativeInvocationOperation,
    expected: &shell_contract::ipc::NativeEditorCommand,
    startup: Option<(
        &shell_contract::ipc::NativeInvocationOperation,
        &NativeRequestOwner,
    )>,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
) -> Result<Value, shell_contract::IpcError> {
    let run = || -> Reply {
        if name != "diagram.open"
            || !matches!(
                expected,
                shell_contract::ipc::NativeEditorCommand::OpenFile {
                    surface: shell_contract::Surface::Diagram,
                    ..
                }
            )
        {
            return Err(error(
                "HostUnavailable",
                "invalid native command dependency",
            ));
        }
        let host = HOST_INSTANCE
            .get()
            .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
        let ipc = host.app.try_state::<crate::ipc::Ipc>();
        require_owned_worker(&host.ui, ipc.as_ref().map(|ipc| ipc.core.as_ref()))?;
        host.call_with_progress_inner(
            name,
            input,
            Some(WAIT),
            Some(cancel),
            None,
            None,
            Some(OwnedRequest::NativeCommandPreparation {
                operation: operation.clone(),
                expected: expected.clone(),
                startup: startup.map(|(root, shell)| (root.clone(), shell.clone())),
                participants: participants.to_vec(),
                cancel: cancel.clone(),
                pending: pending.clone(),
            }),
        )
    };
    run().map_err(|failure| shell_contract::IpcError {
        name: Some(failure.name),
        message: failure.message,
    })
}

/// Save an already-existing native document through its captured service.
#[allow(clippy::too_many_arguments)]
pub(crate) fn call_existing_document_save_with_timeout(
    name: &str,
    input: &Value,
    binding: &shell_contract::ipc::NativeExistingDocumentBinding,
    participants: &[NativeRequestOwner],
    cancel: &CancelToken,
    pending: &CancelToken,
    additional: &[CancelToken],
    timeout: Duration,
) -> Reply {
    match name {
        "docs.htmlToDocx" => {
            let view = input.get("ownerViewId").and_then(Value::as_u64);
            if !std::iter::once(binding.owner())
                .chain(participants.iter())
                .any(|owner| view == Some(u64::from(owner.view.0)))
            {
                return Err(error(
                    "AbortError",
                    "HTML conversion owner is not a captured save participant",
                ));
            }
        }
        "pdf.pageCount" | "pdf.stitchPages" => {}
        _ => {
            return Err(error(
                "HostUnavailable",
                "invalid existing-document save dependency",
            ));
        }
    }
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    let ipc = host.app.try_state::<crate::ipc::Ipc>();
    require_owned_worker(&host.ui, ipc.as_ref().map(|ipc| ipc.core.as_ref()))?;
    host.call_with_progress_inner(
        name,
        input,
        Some(timeout),
        Some(cancel),
        None,
        None,
        Some(OwnedRequest::ExistingDocumentSave {
            binding: binding.clone(),
            participants: participants.to_vec(),
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
        }),
    )
}

/// Execute for the exact committed recovery seed and registered receiver.
pub(crate) fn call_recovery_seed(
    name: &str,
    input: &Value,
    receipt: &shell_contract::ipc::RecoverySeedReceipt,
    receiver: &NativeRequestOwner,
    cancel: &CancelToken,
) -> Reply {
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    let ipc = host.app.try_state::<crate::ipc::Ipc>();
    require_owned_worker(&host.ui, ipc.as_ref().map(|ipc| ipc.core.as_ref()))?;
    host.call_with_progress_inner(
        name,
        input,
        Some(WAIT),
        Some(cancel),
        None,
        None,
        Some(OwnedRequest::Recovery {
            receipt: receipt.clone(),
            receiver: receiver.clone(),
            cancel: cancel.clone(),
        }),
    )
}

fn require_owned_worker(ui: &dyn Thread, core: Option<&dyn Thread>) -> Result<(), HostError> {
    if ui.is_current() || core.is_some_and(|core| core.is_current()) {
        return Err(error(
            "HostUnavailable",
            "an owned engine request must run on a worker",
        ));
    }
    Ok(())
}

pub fn call_with_timeout_cancellable(
    name: &str,
    input: &Value,
    timeout: Duration,
    cancel: &CancelToken,
) -> Reply {
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    host.call_with_options(name, input, Some(timeout), Some(cancel))
}

pub fn call_without_timeout_cancellable(name: &str, input: &Value, cancel: &CancelToken) -> Reply {
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    host.call_with_options(name, input, None, Some(cancel))
}

pub fn call_with_progress(
    name: &str,
    input: &Value,
    cancel: &CancelToken,
    progress: &(dyn Fn(f64, f64) + Sync),
) -> Reply {
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    host.call_with_progress_inner(name, input, None, Some(cancel), Some(progress), None, None)
}

pub fn call_without_timeout(name: &str, input: &Value) -> Reply {
    let host = HOST_INSTANCE
        .get()
        .ok_or_else(|| error("HostUnavailable", "engine service is not installed"))?;
    host.call_with_options(name, input, None, None)
}

static HOST_INSTANCE: std::sync::OnceLock<Arc<EngineHost>> = std::sync::OnceLock::new();
pub fn install(app: &AppHandle) -> Result<(), String> {
    let host = EngineHost::install(app)?;
    HOST_INSTANCE
        .set(host)
        .map_err(|_| "engine host was installed twice".to_owned())
}

pub const SCHEME_HOST: &str = HOST;

/// The text of an engine page report, or `None` when the request is not one: only the engine
/// page's own GET of `REPORT_PATH` is. The text is the page's, so it is cut and stripped of
/// control characters before it reaches a log line.
fn report_text(host: &str, label: &str, request: &tauri::http::Request<Vec<u8>>) -> Option<String> {
    if host != HOST
        || label != LABEL
        || request.method() != tauri::http::Method::GET
        || request.uri().path() != REPORT_PATH
    {
        return None;
    }
    let url = tauri::Url::parse(&request.uri().to_string()).ok()?;
    let (_, text) = url.query_pairs().find(|pair| pair.0 == "m")?;
    Some(
        text.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(REPORT_CHARS)
            .collect(),
    )
}

/// The answer to an engine page report, `None` for every other request of the scheme
pub fn page_report(
    app: &AppHandle,
    host: &str,
    label: &str,
    request: &tauri::http::Request<Vec<u8>>,
) -> Option<tauri::http::Response<Vec<u8>>> {
    let text = report_text(host, label, request)?;
    crate::trace::trace!("engine", "{text}");
    if let Some(engine) = app.try_state::<Arc<EngineHost>>() {
        engine
            .page_reports
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .note(text);
    }
    let mut response = tauri::http::Response::new(Vec::new());
    *response.status_mut() = tauri::http::StatusCode::NO_CONTENT;
    response.headers_mut().insert(
        tauri::http::header::CACHE_CONTROL,
        tauri::http::HeaderValue::from_static("no-store"),
    );
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn html_completion_waits_for_all_admitted_callbacks() {
        let drain = Arc::new(CallbackDrain::default());
        let first = drain.begin();
        let second = drain.begin();
        let (sent, received) = mpsc::channel();
        drain.after_idle(move || {
            sent.send(()).unwrap();
        });
        drop(first);
        assert!(received.try_recv().is_err());
        drop(second);
        assert!(received.try_recv().is_ok());
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn command_preparation_retains_exact_selection_and_cancellation() {
        use shell_contract::ipc::{NativeEditorCommand, NativeInvocationIssuer};
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let issuer = NativeInvocationIssuer::new(CancelToken::default());
        let selected = NativeEditorCommand::OpenFile {
            surface: shell_contract::Surface::Diagram,
            path: "/tmp/one.drawio".into(),
        };
        let operation = issuer.issue_command(selected.clone(), cancel.clone());
        let request = OwnedRequest::NativeCommandPreparation {
            operation: operation.clone(),
            expected: selected,
            startup: None,
            participants: Vec::new(),
            cancel: cancel.clone(),
            pending: pending.clone(),
        };
        assert!(request.with_active(|| ()).is_some());
        let other = OwnedRequest::NativeCommandPreparation {
            operation,
            expected: NativeEditorCommand::OpenFile {
                surface: shell_contract::Surface::Diagram,
                path: "/tmp/two.drawio".into(),
            },
            startup: None,
            participants: Vec::new(),
            cancel,
            pending: pending.clone(),
        };
        assert!(other.with_active(|| ()).is_none());
        pending.cancel();
        assert!(request.with_active(|| ()).is_none());
    }

    #[test]
    fn owned_parser_subscribes_to_every_captured_owner_loss() {
        use shell_contract::{ViewId, ipc::NativeContext, runtime::RendererGeneration};
        let source_loss = CancelToken::default();
        let participant_loss = CancelToken::default();
        let owner = NativeRequestOwner {
            view: ViewId(1),
            context: NativeContext::with_owner_loss(
                (),
                RendererGeneration::new(),
                source_loss.clone(),
            ),
        };
        let participant = NativeRequestOwner {
            view: ViewId(2),
            context: NativeContext::with_owner_loss(
                (),
                RendererGeneration::new(),
                participant_loss.clone(),
            ),
        };
        let request = OwnedRequest::InteractiveMany {
            owner,
            participants: vec![participant],
            cancel: CancelToken::default(),
            pending: CancelToken::default(),
        };
        let tokens = request.cancellation_tokens(None);
        assert!(tokens.iter().any(|token| token.same_token(&source_loss)));
        assert!(
            tokens
                .iter()
                .any(|token| token.same_token(&participant_loss))
        );
    }

    #[test]
    fn existing_save_subscribes_to_document_and_operation_loss() {
        use shell_contract::{
            Surface, ViewId,
            ipc::{NativeContext, NativeInvocationIssuer},
            runtime::RendererGeneration,
        };
        let owner = NativeRequestOwner {
            view: ViewId(4),
            context: NativeContext::with_owner_loss(
                (),
                RendererGeneration::new(),
                CancelToken::default(),
            )
            .with_document_lease(RendererGeneration::new()),
        };
        let issuer = NativeInvocationIssuer::new(CancelToken::default());
        let binding = issuer
            .issue_existing_document_save(
                Surface::Html,
                owner.view,
                shell_contract::ipc::NativeSaveSelection::ExactPath("/tmp/page.html".into()),
                owner,
                CancelToken::default(),
            )
            .unwrap();
        let owner_loss = binding.owner().loss_token().unwrap();
        let operation_loss = binding.operation().loss_token();
        let callback_cancel = CancelToken::default();
        let request = OwnedRequest::ExistingDocumentSave {
            binding,
            participants: Vec::new(),
            cancel: CancelToken::default(),
            pending: CancelToken::default(),
            additional: vec![callback_cancel.clone()],
        };
        let tokens = request.cancellation_tokens(None);
        assert!(tokens.iter().any(|token| token.same_token(&owner_loss)));
        assert!(tokens.iter().any(|token| token.same_token(&operation_loss)));
        assert!(
            tokens
                .iter()
                .any(|token| token.same_token(&callback_cancel))
        );
    }

    #[test]
    fn request_ids_stop_at_the_javascript_integer_boundary() {
        let next = AtomicU64::new(9_007_199_254_740_991);
        assert_eq!(next_request_id(&next).unwrap(), 9_007_199_254_740_991);
        assert!(next_request_id(&next).is_err());
        assert!(next_request_id(&next).is_err());
    }

    #[test]
    fn concurrent_engine_and_font_requests_never_share_an_id() {
        let next = Arc::new(AtomicU64::new(1));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let next = next.clone();
                std::thread::spawn(move || {
                    (0..32)
                        .map(|_| next_request_id(&next).unwrap())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let ids: Vec<_> = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect();
        let unique: HashSet<_> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len());
        assert_eq!(unique, (1..=128).collect());
    }

    #[test]
    fn public_engine_errors_are_data_not_diagnostics() {
        let result =
            decode_reply(json!({"error":{"name":"RuntimeArchiveError","message":"bad archive"}}))
                .unwrap();
        assert_eq!(result.unwrap_err().name, "RuntimeArchiveError");
    }

    #[test]
    fn conflicting_or_malformed_replies_are_refused() {
        for value in [
            json!({"ok":1,"error":{"name":"Error","message":"x"}}),
            json!({"error":{"message":"x"}}),
            json!({"anything":true}),
        ] {
            assert!(decode_reply(value).is_err());
        }
    }

    #[test]
    fn callbacks_require_live_request_service_nonce_and_fresh_identity() {
        let (reply, _receiver) = mpsc::sync_channel(1);
        let mut state = ServiceState {
            ready: Ok(true),
            pending: HashMap::new(),
        };
        let cancel = CancelToken::default();
        state.pending.insert(
            7,
            PendingRequest {
                service: "docs.htmlToDocx".into(),
                nonce: "secret".into(),
                input: Arc::new(json!({"path":"document.html"})),
                cancel: cancel.clone(),
                reply,
                callbacks: HashSet::new(),
                callback_drain: Arc::default(),
                progress: None,
                slides_owner: None,
                origin: None,
                service_owner: None,
                evaluation: None,
            },
        );
        assert!(
            claim_callback(&mut state, 8, "docs.htmlToDocx", "secret", 1, "html.open").is_err()
        );
        assert!(claim_callback(&mut state, 7, "reader.pdf", "secret", 1, "html.open").is_err());
        assert!(claim_callback(&mut state, 7, "docs.htmlToDocx", "other", 1, "html.open").is_err());
        assert!(claim_callback(&mut state, 7, "docs.htmlToDocx", "secret", 1, "pdf.sign").is_err());
        let (input, _, _) =
            claim_callback(&mut state, 7, "docs.htmlToDocx", "secret", 1, "html.open").unwrap();
        assert_eq!(input["path"], "document.html");
        assert!(
            claim_callback(&mut state, 7, "docs.htmlToDocx", "secret", 1, "html.open").is_err()
        );
        assert!(request_may_start(&state, 7));
        cancel.cancel();
        assert!(!request_may_start(&state, 7));
        assert!(
            claim_callback(
                &mut state,
                7,
                "docs.htmlToDocx",
                "secret",
                2,
                "html.evaluate"
            )
            .is_err()
        );
        state.pending.remove(&7);
        assert!(
            claim_callback(&mut state, 7, "docs.htmlToDocx", "secret", 3, "html.open").is_err()
        );
    }

    #[test]
    fn progress_is_confined_to_its_request_and_cannot_be_replayed() {
        let (reply, _receiver) = mpsc::sync_channel(1);
        let (progress, received) = mpsc::channel();
        let mut state = ServiceState {
            ready: Ok(true),
            pending: HashMap::new(),
        };
        state.pending.insert(
            9,
            PendingRequest {
                service: "pdf.structureOperation".into(),
                nonce: "pdf-secret".into(),
                input: Arc::new(json!({})),
                cancel: CancelToken::default(),
                reply,
                callbacks: HashSet::new(),
                callback_drain: Arc::default(),
                progress: Some(progress),
                slides_owner: None,
                origin: None,
                service_owner: None,
                evaluation: None,
            },
        );
        assert!(
            claim_callback(
                &mut state,
                9,
                "pdf.structureOperation",
                "pdf-secret",
                1,
                "html.open"
            )
            .is_err()
        );
        let (_, _, progress) = claim_callback(
            &mut state,
            9,
            "pdf.structureOperation",
            "pdf-secret",
            1,
            "progress",
        )
        .unwrap();
        progress.unwrap().send((2.0, 5.0)).unwrap();
        assert_eq!(received.recv().unwrap(), (2.0, 5.0));
        assert!(
            claim_callback(
                &mut state,
                9,
                "pdf.structureOperation",
                "pdf-secret",
                1,
                "progress"
            )
            .is_err()
        );
    }
    #[test]
    fn pdfium_session_only_permits_its_own_request_bound_operation() {
        let (reply, _) = mpsc::sync_channel(1);
        let mut state = ServiceState {
            ready: Ok(true),
            pending: HashMap::new(),
        };
        state.pending.insert(
            10,
            PendingRequest {
                service: "pdf.pdfiumSession".into(),
                nonce: "secret".into(),
                input: Arc::new(json!({"document":"original"})),
                slides_owner: None,
                origin: None,
                service_owner: None,
                evaluation: None,
                cancel: CancelToken::default(),
                callbacks: HashSet::new(),
                callback_drain: Arc::default(),
                progress: None,
                reply,
            },
        );
        assert!(
            claim_callback(&mut state, 10, "pdf.pdfiumSession", "secret", 1, "pdf.sign").is_err()
        );
        assert!(
            claim_callback(&mut state, 10, "pdf.pdfiumSession", "secret", 1, "pdf.ocr").is_err()
        );
        let (original, _, _) = claim_callback(
            &mut state,
            10,
            "pdf.pdfiumSession",
            "secret",
            1,
            "pdfium.next",
        )
        .unwrap();
        assert_eq!(original["document"], "original");
        assert!(
            claim_callback(
                &mut state,
                10,
                "pdf.pdfiumSession",
                "secret",
                1,
                "pdfium.next"
            )
            .is_err()
        );
    }
    #[test]
    fn pdf_document_session_cannot_cross_callback_capabilities() {
        let (reply, _) = mpsc::sync_channel(1);
        let mut state = ServiceState {
            ready: Ok(true),
            pending: HashMap::new(),
        };
        state.pending.insert(
            11,
            PendingRequest {
                service: "pdf.documentEditSession".into(),
                nonce: "secret".into(),
                input: Arc::new(json!({"session":42})),
                slides_owner: None,
                origin: None,
                service_owner: None,
                evaluation: None,
                cancel: CancelToken::default(),
                callbacks: HashSet::new(),
                callback_drain: Arc::default(),
                progress: None,
                reply,
            },
        );
        assert!(
            claim_callback(
                &mut state,
                11,
                "pdf.documentEditSession",
                "secret",
                1,
                "pdf.sign"
            )
            .is_err()
        );
        assert!(
            claim_callback(
                &mut state,
                11,
                "pdf.documentEditSession",
                "secret",
                1,
                "pdfium.next"
            )
            .is_err()
        );
        let (original, _, _) = claim_callback(
            &mut state,
            11,
            "pdf.documentEditSession",
            "secret",
            1,
            "pdfDocument.next",
        )
        .unwrap();
        assert_eq!(original["session"], 42);
        assert!(
            claim_callback(
                &mut state,
                11,
                "pdf.documentEditSession",
                "secret",
                1,
                "pdfDocument.next"
            )
            .is_err()
        );
    }

    #[test]
    fn slide_callbacks_cannot_be_claimed_without_captured_owner() {
        let (reply, _) = mpsc::sync_channel(1);
        let mut state = ServiceState {
            ready: Ok(true),
            pending: HashMap::new(),
        };
        state.pending.insert(
            12,
            PendingRequest {
                service: "slides.renderSlide".into(),
                nonce: "secret".into(),
                input: Arc::new(json!({"deckId":1,"generation":1,"ownerViewId":1})),
                slides_owner: None,
                origin: None,
                service_owner: None,
                evaluation: None,
                cancel: CancelToken::default(),
                callbacks: HashSet::new(),
                callback_drain: Arc::default(),
                progress: None,
                reply,
            },
        );
        assert!(
            claim_callback(
                &mut state,
                12,
                "slides.renderSlide",
                "secret",
                1,
                "slides.fontMetrics"
            )
            .is_err()
        );
        assert!(
            claim_callback(
                &mut state,
                12,
                "slides.renderSlide",
                "secret",
                2,
                "slides.mediaResolve"
            )
            .is_err()
        );
        assert!(
            claim_callback(&mut state, 12, "slides.renderSlide", "secret", 3, "pdf.ocr").is_err()
        );
    }

    struct ActualThread(std::thread::ThreadId);
    impl Thread for ActualThread {
        fn post(&self, _: Box<dyn FnOnce() + Send>) {
            panic!("worker admission must never dispatch")
        }
        fn is_current(&self) -> bool {
            std::thread::current().id() == self.0
        }
    }

    #[test]
    fn owned_engine_boundary_rejects_real_ui_and_core_threads_before_waiting() {
        let current = ActualThread(std::thread::current().id());
        assert!(require_owned_worker(&current, None).is_err());
        let other_id = std::thread::spawn(|| std::thread::current().id())
            .join()
            .unwrap();
        let other = ActualThread(other_id);
        assert!(require_owned_worker(&other, Some(&current)).is_err());
    }

    #[test]
    fn owned_engine_boundary_admits_an_actual_worker_only() {
        let ui = std::thread::current().id();
        std::thread::spawn(move || {
            assert!(require_owned_worker(&ActualThread(ui), Some(&ActualThread(ui))).is_ok());
        })
        .join()
        .unwrap();
    }

    fn report_request(method: &str, uri: &str) -> tauri::http::Request<Vec<u8>> {
        tauri::http::Request::builder()
            .method(method)
            .uri(uri)
            .body(Vec::new())
            .unwrap()
    }

    #[test]
    fn only_the_engine_pages_own_get_of_the_reserved_path_is_a_report() {
        let mac = "genoffice-app-engine-host://localhost/__engine-host-log?m=boot%3A%20up%2B1";
        let windows =
            "http://genoffice-app-engine-host.localhost/__engine-host-log?m=boot%3A%20up%2B1";
        for uri in [mac, windows] {
            let get = report_request("GET", uri);
            assert_eq!(
                report_text(HOST, LABEL, &get).as_deref(),
                Some("boot: up+1")
            );
            // another webview of the same scheme, another host's scheme, another method
            assert_eq!(report_text(HOST, "probe-page", &get), None);
            assert_eq!(report_text("docs", LABEL, &get), None);
            assert_eq!(report_text(HOST, LABEL, &report_request("POST", uri)), None);
        }
        // an ordinary file of the page is not a report
        let file = report_request("GET", "genoffice-app-engine-host://localhost/main.js?m=x");
        assert_eq!(report_text(HOST, LABEL, &file), None);
    }

    #[test]
    fn a_report_is_one_bounded_line_of_text() {
        let long = "a".repeat(REPORT_CHARS * 2);
        let uri =
            format!("genoffice-app-engine-host://localhost{REPORT_PATH}?m=one%0Atwo%00{long}");
        let text = report_text(HOST, LABEL, &report_request("GET", &uri)).unwrap();
        assert!(text.starts_with("one two "));
        assert!(!text.chars().any(char::is_control));
        assert_eq!(text.chars().count(), REPORT_CHARS);
    }

    #[test]
    fn a_missed_readiness_names_what_the_page_reported() {
        let mut reports = PageReports::default();
        assert!(reports.status().contains("reported nothing"));
        let total = REPORTS_KEPT + 4;
        for n in 0..total {
            reports.note(format!("report {n}"));
        }
        let shown: Vec<String> = (total - REPORTS_SHOWN..total)
            .map(|n| format!("report {n}"))
            .collect();
        assert_eq!(
            reports.status(),
            format!("the engine page reported: {}", shown.join(" | "))
        );
        assert_eq!(reports.0.len(), REPORTS_KEPT);
    }

    #[test]
    fn the_boot_script_reports_to_the_path_the_shell_answers() {
        let script = include_str!("../engine-host/boot.ts");
        assert!(script.contains(&format!("'{REPORT_PATH}'")));
    }
}
