//! Prepare the original recovery document before its real native page can load.
use std::sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, Ordering}};
use std::rc::Rc;

use main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone;
use main_core::shell::tab_manager::TabKind;
use shell_contract::{Provided, ViewId, ipc::{RecoveryOpenContext, RecoveryOpenTarget, RecoveryRequestConstraint}, runtime::{CancelSubscription, Services, Views}};
use tauri_runtime_cef::{FrameEventKind, NativeBrowserClose, NativeBrowserCloseSubscription, NativeContextToken};

use crate::host::HostContext;
use crate::ipc::native_owner::{self, NativeOwnerAuthority};

struct PdfRetirement(shell_ipc_d2::pdf::editor::editors::PdfRecoveryRetirement);
impl shell_ipc_w::tabs::host::EditorSeedRollback for PdfRetirement {
    fn retire_closed(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) {
        self.0.retire_closed(proof, done);
    }
    fn retire_adopted(&self, proof: shell_contract::ipc::NativeClosureReceipt, done: Box<dyn FnOnce(Result<(), String>)>) {
        self.0.retire_adopted(proof, done);
    }
    fn retire_superseded(&self, proof: shell_contract::ipc::NativeBrowserSuperseded, done: Box<dyn FnOnce(Result<(), String>)>) {
        self.0.retire_superseded(proof, done);
    }
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) { self.0.rollback(done); }
}

type BeforeLoad = Box<dyn FnOnce(ViewId, &RecoveryOpenTarget, RecoveryOpenDone)>;
pub(crate) type RecoveryCleanup = shell_ipc_w::tabs::host::ViewCreationCleanup;
pub(crate) struct RecoveryFailure {
    pub message: String,
    pub cleanup: shell_ipc_w::tabs::host::ViewCreationCleanupState,
}
type Done = Box<dyn FnOnce(Result<RecoveryReady, RecoveryFailure>)>;
type SendDone = Box<dyn FnOnce(Result<RecoveryReady, RecoveryFailure>) + Send>;

pub(crate) struct RecoveryReady {
    owner: shell_contract::ipc::NativeRequestOwner,
    _target: RecoveryOpenTarget,
    browser: Browser,
    cx: HostContext,
    core: Arc<dyn shell_contract::runtime::Thread>,
}
impl RecoveryReady {
    pub(crate) fn view(&self) -> ViewId { self.owner.view }
    pub(crate) fn invocation_document(&self) -> Result<shell_contract::ipc::NativeInvocationDocument, String> {
        self._target.invocation_target().ok_or_else(|| "The recovery target is not a native invocation".to_owned())?.registered_document()
    }
    pub(crate) fn into_owner(self) -> shell_contract::ipc::NativeRequestOwner { self.owner }
    pub(crate) fn into_parts(self) -> (
        shell_contract::ipc::NativeRequestOwner,
        RecoveryCleanup,
        shell_ipc_w::tabs::host::ViewClosureBinding,
    ) {
        let owner = self.owner.clone();
        let cx = self.cx;
        let core = self.core;
        let view = self.owner.view;
        let target = self._target;
        let browser = self.browser;
        let reservation = browser.proof.reservation().clone();
        let browser_owner = browser.proof.owner().clone();
        let producer = Arc::new(browser.proof.clone());
        let registration = shell_ipc_w::tabs::host::NativeBrowserRegistration::Registered(owner.clone());
        let proof = browser.proof.clone();
        let watch_target = target.clone();
        let watch_core = core.clone();
        let closure = Arc::new(move |done| {
            let target = watch_target.clone();
            let core = watch_core.clone();
            proof.watch(Box::new(move |receipt| {
                if let Err(error) = target.drain_work(core, Box::new(move || done(shell_contract::ipc::NativeBrowserRetirement::Closed(receipt)))) {
                    eprintln!("[recovery] closure work drain failed: {error}");
                }
            }))
        });
        (owner, Arc::new(move |done| discard_captured(cx.clone(), core.clone(), view, target.clone(), browser.clone(), false,
            Box::new(move |result| done(result.map(shell_contract::ipc::NativeBrowserRetirement::Closed))))),
            shell_ipc_w::tabs::host::ViewClosureBinding { reservation, owner: browser_owner, producer, closure, registration })
    }
    pub(crate) fn discard(self, done: Box<dyn FnOnce(Result<shell_contract::ipc::NativeClosureReceipt, String>) + Send>) {
        discard_captured(self.cx, self.core, self.owner.view, self._target, self.browser, false, done);
    }
}

fn discard_captured(
    cx: HostContext, core: Arc<dyn shell_contract::runtime::Thread>, view: ViewId,
    target: RecoveryOpenTarget, browser: Browser, allow_not_created: bool,
    done: Box<dyn FnOnce(Result<shell_contract::ipc::NativeClosureReceipt, String>) + Send>,
) {
    let created = target.created().clone();
    let completion = Arc::new(Mutex::new(Some(done)));
    let completed = completion.clone();
    let drain_core = core.clone();
    let done = shell_ipc_w::home_files::back_to_core(core, move |result| {
        let receipt = match result {
            Ok(receipt) => receipt,
            Err(error) => {
                if let Some(done) = completed.lock().unwrap_or_else(|error| error.into_inner()).take() { done(Err(error)); }
                return;
            }
        };
        let drained = completed.clone();
        if let Err(error) = target.drain_work(drain_core, Box::new(move || {
            if let Some(done) = drained.lock().unwrap_or_else(|error| error.into_inner()).take() { done(Ok(receipt)); }
        })) {
            if let Some(done) = completed.lock().unwrap_or_else(|error| error.into_inner()).take() { done(Err(error)); }
        }
    });
    let state = Arc::new(Discard {
        done: Mutex::new(Some(Box::new(done))), subscription: Mutex::new(None),
        cx, view, created,
    });
    let closed = state.clone();
    let subscription = browser.proof.when_closed(move |receipt| {
        if receipt.outcome() == shell_contract::ipc::NativeClosureOutcome::NotCreated && !allow_not_created {
            return closed.finish(Err("The ready recovery browser was reported as never created".into()));
        }
        closed.cx.views.remove_view_if_generation(closed.view, &closed.created);
        closed.finish(Ok(receipt));
    });
    match subscription {
        Ok(subscription) => *state.subscription.lock().unwrap_or_else(|error| error.into_inner()) = Some(subscription),
        Err(error) => return state.finish(Err(error)),
    }
    let close = browser.closed;
    let webview = browser.webview;
    let ui = state.cx.ui.clone();
    let posted = DiscardPost(Some(state.clone()));
    ui.post(Box::new(move || {
        let mut posted = posted;
        crate::host::contents::forget_webview(webview.label());
        if let Err(error) = close.request_close() { state.finish(Err(error)); }
        drop(posted.0.take());
    }));
}

struct Discard {
    done: Mutex<Option<Box<dyn FnOnce(Result<shell_contract::ipc::NativeClosureReceipt, String>) + Send>>>,
    subscription: Mutex<Option<NativeBrowserCloseSubscription>>,
    cx: HostContext,
    view: ViewId,
    created: shell_contract::runtime::RendererGeneration,
}
impl Discard {
    fn finish(&self, result: Result<shell_contract::ipc::NativeClosureReceipt, String>) {
        let subscription = self.subscription.lock().unwrap_or_else(|error| error.into_inner()).take();
        drop(subscription);
        let done = self.done.lock().unwrap_or_else(|error| error.into_inner()).take();
        if let Some(done) = done { done(result); }
    }
}

struct DiscardPost(Option<Arc<Discard>>);
impl Drop for DiscardPost {
    fn drop(&mut self) {
        if let Some(state) = self.0.take() {
            state.finish(Err("The captured recovery cleanup task was discarded".into()));
        }
    }
}

#[derive(Clone)]
struct Browser {
    webview: tauri::Webview,
    closed: NativeBrowserClose,
    proof: crate::host::window::NativeClosureProof,
}

struct Opening {
    cx: HostContext,
    core: Arc<dyn shell_contract::runtime::Thread>,
    target: RecoveryOpenTarget,
    proof: crate::host::window::NativeClosureProof,
    closed_receipt: Mutex<Option<shell_contract::ipc::NativeClosureReceipt>>,
    browser: OnceLock<Browser>,
    failure_close: OnceLock<NativeBrowserClose>,
    origin: OnceLock<NativeContextToken>,
    constructed: AtomicBool,
    construction_admitted: AtomicBool,
    settled: AtomicBool,
    document_ready: AtomicBool,
    initialized: AtomicBool,
    closing: AtomicBool,
    closed: AtomicBool,
    draining: AtomicBool,
    drained: AtomicBool,
    subscription: Mutex<Option<NativeBrowserCloseSubscription>>,
    losses: Mutex<Vec<CancelSubscription>>,
    result: Mutex<Option<Result<RecoveryReady, RecoveryFailure>>>,
    done: Mutex<Option<SendDone>>,
}

impl Opening {
    fn arm_authority(self: &Arc<Self>) -> Result<(), String> {
        if self.settled.load(Ordering::Acquire) { return Ok(()); }
        let snapshot = self.target.authority_snapshot()?;
        let mut losses = vec![(self.target.created().loss_token(), false)];
        match &snapshot.context {
            RecoveryOpenContext::Renderer { source, participants, cancel, pending } => {
                losses.push((source.loss_token().ok_or("The recovery source lifetime is unavailable")?, true));
                for owner in participants { losses.push((owner.loss_token().ok_or("A recovery participant lifetime is unavailable")?, true)); }
                losses.extend([(cancel.clone(), true), (pending.clone(), true)]);
            }
            RecoveryOpenContext::NativeStartup(operation) =>
                losses.extend([(operation.loss_token(), false), (operation.cancel().clone(), false), (operation.shutdown().clone(), false)]),
        }
        if let Some(request) = &snapshot.request {
            losses.push((request.source.loss_token().ok_or("The recovery request source lifetime is unavailable")?, true));
            for owner in &request.participants { losses.push((owner.loss_token().ok_or("A recovery request participant lifetime is unavailable")?, true)); }
            losses.extend([(request.cancel.clone(), true), (request.pending.clone(), true)]);
        }
        if let Some(startup) = &snapshot.startup {
            losses.push((startup.shell.loss_token().ok_or("The startup Shell lifetime is unavailable")?, false));
            for owner in &startup.participants { losses.push((owner.loss_token().ok_or("A startup participant lifetime is unavailable")?, false)); }
            losses.extend([(startup.startup.loss_token(), false), (startup.startup.cancel().clone(), false),
                (startup.startup.shutdown().clone(), false), (startup.cancel.clone(), false), (startup.pending.clone(), false)]);
        }
        if let Some(command) = &snapshot.command {
            for owner in &command.participants { losses.push((owner.loss_token().ok_or("A command participant lifetime is unavailable")?, false)); }
            losses.extend([(command.command.loss_token(), false), (command.command.cancel().clone(), false),
                (command.command.shutdown().clone(), false), (command.cancel.clone(), false), (command.pending.clone(), false)]);
        }
        let mut subscriptions = Vec::with_capacity(losses.len() + 1);
        let weak = Arc::downgrade(self);
        subscriptions.push(snapshot.revision_loss_token().on_cancel(move || {
            if let Some(opening) = weak.upgrade() {
                if let Err(error) = opening.arm_authority() { opening.fail(error); }
            }
        }));
        for (loss, precommit_only) in losses {
            let weak = Arc::downgrade(self);
            let captured = snapshot.clone();
            subscriptions.push(loss.on_cancel(move || {
                if let Some(opening) = weak.upgrade() {
                    let claimed = captured.with_authority_current(|| {
                        (!precommit_only || !opening.target.is_seed_committed())
                            && !opening.settled.swap(true, Ordering::AcqRel)
                    }).unwrap_or(false);
                    if claimed { opening.fail_claimed("The recovery opening authority expired".into()); }
                }
            }));
        }
        if !self.settled.load(Ordering::Acquire) {
            self.losses.lock().unwrap_or_else(|error| error.into_inner()).extend(subscriptions);
        }
        Ok(())
    }

    fn fail(self: &Arc<Self>, error: String) {
        if self.settled.swap(true, Ordering::AcqRel) { return; }
        self.fail_claimed(error);
    }

    fn fail_claimed(self: &Arc<Self>, error: String) {
        if let Some(receipt) = self.target.committed_seed() {
            if let Ok(notification) = receipt.fail_after_loss(error.clone()) { notification.notify(); }
        }
        *self.result.lock().unwrap_or_else(|error| error.into_inner()) = Some(Err(RecoveryFailure { message: error, cleanup: shell_ipc_w::tabs::host::ViewCreationCleanupState::Complete }));
        self.target.created().revoke();
        self.close_created();
    }

    fn close_created(self: &Arc<Self>) {
        let Some(browser) = self.browser.get() else {
            if self.constructed.load(Ordering::Acquire) {
                if !self.construction_admitted.load(Ordering::Acquire) {
                    self.cx.views.remove_view_if_generation(self.target.view(), self.target.created());
                }
                self.complete();
            }
            return;
        };
        if self.closed.load(Ordering::Acquire) { self.complete(); return; }
        if self.closing.swap(true, Ordering::AcqRel) { return; }
        let original = browser.webview.clone();
        let close = browser.closed.clone();
        let opening = self.clone();
        let ui = self.cx.ui.clone();
        let posted = ClosePost(Some(opening.clone()));
        ui.post(Box::new(move || {
            let mut posted = posted;
            if opening.closed.load(Ordering::Acquire) {
                drop(posted.0.take());
                opening.complete(); return;
            }
            crate::host::contents::forget_webview(original.label());
            if let Err(error) = close.request_close() {
                let mut result = opening.result.lock().unwrap_or_else(|error| error.into_inner());
                if let Some(Err(first)) = result.as_mut() { first.message.push_str(&format!("; recovery browser cleanup failed: {error}")); }
                drop(result);
                opening.complete();
            }
            drop(posted.0.take());
        }));
    }

    fn ready(self: &Arc<Self>, origin: &NativeContextToken) {
        if self.settled.load(Ordering::Acquire) { return; }
        if self.origin.get() != Some(origin) { return self.fail("The recovery ready document does not match its constructor".into()); }
        self.document_ready.store(true, Ordering::Release);
        self.finish_if_ready();
    }

    fn finish_if_ready(self: &Arc<Self>) {
        if self.settled.load(Ordering::Acquire) || !self.constructed.load(Ordering::Acquire)
            || !self.document_ready.load(Ordering::Acquire) || !self.initialized.load(Ordering::Acquire) { return; }
        let snapshot = match self.target.authority_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return self.fail(error),
        };
        let Some(owner) = snapshot.registered.clone() else { return self.fail("The recovery document did not register".into()); };
        let Some(browser) = self.browser.get().cloned() else { return self.fail("The original recovery browser is unavailable".into()); };
        let Some(receipt) = self.target.committed_seed() else { return self.fail("The recovery seed was not committed".into()); };
        let cancel = self.target.created().loss_token();
        let ready = RecoveryReady { owner: owner.clone(), _target: self.target.clone(), browser, cx: self.cx.clone(), core: self.core.clone() };
        let admitted = native_owner::with_recovery_open_service_snapshot(&receipt, &snapshot, &owner, &[], None, None, &cancel, None, &[], || {
            if self.closed.load(Ordering::Acquire) || self.settled.swap(true, Ordering::AcqRel) { return false; }
            *self.result.lock().unwrap_or_else(|error| error.into_inner()) = Some(Ok(ready));
            true
        });
        match admitted {
            Some(true) => {}
            Some(false) if self.settled.load(Ordering::Acquire) => return,
            _ => return self.fail("The recovery request expired before its document became ready".into()),
        }
        self.complete();
    }

    fn complete(self: &Arc<Self>) {
        if !self.drained.load(Ordering::Acquire) {
            if self.draining.swap(true, Ordering::AcqRel) { return; }
            let opening = self.clone();
            if let Err(error) = self.target.drain_work(self.core.clone(), Box::new(move || {
                opening.drained.store(true, Ordering::Release);
                opening.complete();
            })) {
                self.draining.store(false, Ordering::Release);
                eprintln!("[recovery] work drain failed: {error}");
            }
            return;
        }
        let has_ready = matches!(self.result.lock().unwrap_or_else(|error| error.into_inner()).as_ref(), Some(Ok(_)));
        if has_ready {
            let snapshot = match self.target.authority_snapshot() {
                Ok(snapshot) => snapshot,
                Err(error) => { eprintln!("[recovery] final authority snapshot failed: {error}"); return; }
            };
            let Some(owner) = snapshot.registered.clone() else {
                eprintln!("[recovery] final document registration is unavailable");
                return;
            };
            let exact_origin = self.origin.get().is_some_and(|origin| {
                owner.context.authority_ref::<NativeOwnerAuthority>().is_some_and(|authority| &authority.origin == origin)
            });
            let Some(receipt) = self.target.committed_seed() else { return; };
            let cancel = self.target.created().loss_token();
            let delivered = exact_origin && native_owner::with_recovery_open_service_snapshot(
                &receipt, &snapshot, &owner, &[], None, None, &cancel, None, &[], || {
                    let mut result = self.result.lock().unwrap_or_else(|error| error.into_inner());
                    if let Some(Ok(ready)) = result.as_mut() { ready.owner = owner.clone(); true } else { false }
                },
            ).unwrap_or(false);
            if !delivered {
                eprintln!("[recovery] final document authority changed during work drain");
                return;
            }
        }
        let mut result = self.result.lock().unwrap_or_else(|error| error.into_inner()).take();
        if let Some(Err(error)) = result.as_mut() {
            error.cleanup = if let Some(receipt) = self.closed_receipt.lock().unwrap_or_else(|poison| poison.into_inner()).clone() {
                shell_ipc_w::tabs::host::ViewCreationCleanupState::Acknowledged(
                    shell_contract::ipc::NativeBrowserRetirement::Closed(receipt))
            } else {
                let cx = self.cx.clone();
                let core = self.core.clone();
                let view = self.target.view();
                let created = self.target.created().clone();
                let proof = self.proof.clone();
                shell_ipc_w::tabs::host::ViewCreationCleanupState::Pending(Arc::new(move |done| {
                    let done = shell_ipc_w::home_files::back_to_core(core.clone(), done);
                    super::accepted::cleanup_proof(cx.clone(), view, created.clone(), proof.clone(),
                        Box::new(move |result| done(result.map(shell_contract::ipc::NativeBrowserRetirement::Closed))));
                }))
            };
        }
        let subscription = self.subscription.lock().unwrap_or_else(|error| error.into_inner()).take();
        let losses = std::mem::take(&mut *self.losses.lock().unwrap_or_else(|error| error.into_inner()));
        drop((subscription, losses));
        let Some(result) = result else { return; };
        let done = self.done.lock().unwrap_or_else(|error| error.into_inner()).take();
        if let Some(done) = done { done(result); }
    }

    fn frame(self: &Arc<Self>, view: ViewId, event: &tauri_runtime_cef::FrameEvent) {
        if self.settled.load(Ordering::Acquire) || !event.is_main { return; }
        if view != self.target.view() { return self.fail("The recovery browser does not match its reservation".into()); }
        match &event.kind {
            FrameEventKind::NativeContextRegistered { origin }
                if event.browser_id == origin.browser_id() && event.frame_id == origin.frame_id() => {
                if let Some(first) = self.origin.get() {
                    if first != origin { self.fail("The recovery native document was replaced".into()); }
                    return;
                }
                let owner = match native_owner::capture_owner(&self.cx.views, view, self.target.created()) {
                    Ok(owner) => owner,
                    Err(error) => return self.fail(error),
                };
                if owner.context.authority_ref::<NativeOwnerAuthority>().is_none_or(|proof| &proof.origin != origin) {
                    return self.fail("The recovery event does not match its attested native owner".into());
                }
                if let Err(error) = native_owner::register_recovery_open_target(&self.target, owner) { return self.fail(error); }
                let _ = self.origin.set(origin.clone());
            }
            FrameEventKind::NativeDocumentReady { origin } => self.ready(origin),
            FrameEventKind::NativeContextRevoked { origin } if self.origin.get() == Some(origin) =>
                self.fail("The recovery native document was revoked before load completed".into()),
            FrameEventKind::NavigationFailed { .. } => self.fail("The recovery page did not load".into()),
            FrameEventKind::NativeRendererTerminated { origin, process }
                if self.origin.get() == Some(origin) && origin.renderer_process_identity().as_ref() == Some(process) =>
                self.fail("The recovery renderer stopped before load completed".into()),
            _ => {}
        }
    }
}

struct ClosePost(Option<Arc<Opening>>);
impl Drop for ClosePost {
    fn drop(&mut self) {
        if let Some(opening) = self.0.take() {
            let mut result = opening.result.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(Err(error)) = result.as_mut() { error.message.push_str("; recovery cleanup task was discarded"); }
            drop(result);
            opening.complete();
        }
    }
}

struct Preparation(Option<Arc<Opening>>);
impl Drop for Preparation {
    fn drop(&mut self) {
        if let Some(opening) = self.0.take() {
            opening.constructed.store(true, Ordering::Release);
            if opening.settled.load(Ordering::Acquire) { opening.close_created(); }
            else { opening.fail("The recovery preparation completion was discarded".into()); }
        }
    }
}

struct Construction(Option<Arc<Opening>>);
impl Drop for Construction {
    fn drop(&mut self) {
        if let Some(opening) = self.0.take() {
            opening.constructed.store(true, Ordering::Release);
            if opening.settled.load(Ordering::Acquire) { opening.close_created(); }
            else { opening.fail("The recovery constructor task was discarded".into()); }
        }
    }
}

pub(crate) fn create_recovery_view_owned(
    services: &Services, provided: Rc<Provided>, kind: TabKind, path: Option<&str>, untitled: bool,
    context: RecoveryOpenContext, request: Option<RecoveryRequestConstraint>,
    startup: Option<shell_contract::ipc::StartupRecoveryConstraint>,
    command: Option<shell_contract::ipc::CommandRecoveryConstraint>,
    rollback: shell_ipc_w::tabs::host::EditorSeedRollbackStore, before_load: BeforeLoad, done: Done,
) -> Result<(), String> {
    let mut docs = None;
    let mut sheets = None;
    let mut actors = super::OrdinaryActorStages::default();
    let pdf = if kind == TabKind::Pdf {
        Some(provided.all::<shell_ipc_d2::pdf::editor::editors::PdfEditors>().into_iter().next().ok_or("PDF editor is unavailable")?)
    } else { None };
    let constructor = || super::prepare_recovery_view(services, provided, kind, None, untitled || path.is_some(), &mut docs, &mut sheets, &mut actors);
    let mut constructor = match (&context, &startup, &command) {
        (RecoveryOpenContext::NativeStartup(child), Some(startup), Some(command)) =>
            native_owner::with_startup_command_recovery_preparation_effect(startup, command, child,
                request.as_ref(), || constructor),
        (RecoveryOpenContext::NativeStartup(child), Some(startup), None) =>
            native_owner::with_startup_recovery_preparation_effect(&startup.startup, &startup.shell, child,
                &startup.participants, &startup.cancel, &startup.pending, || constructor),
        (RecoveryOpenContext::NativeStartup(child), None, Some(command)) if request.is_none() =>
            native_owner::with_command_recovery_preparation_effect(&command.command, child,
                &command.participants, &command.cancel, &command.pending, || constructor),
        (RecoveryOpenContext::Renderer { source, participants, cancel, pending }, None, None) =>
            native_owner::with_many_owner_transition(source, participants, cancel, pending, |_| constructor),
        (RecoveryOpenContext::NativeStartup(operation), None, None) => match &request {
            Some(request) => native_owner::with_native_invocation_operation_effect(operation, &request.source,
                &request.participants, &request.cancel, &request.pending, || constructor),
            None => operation.with_active(|| constructor),
        },
        _ => None,
    }.ok_or("The recovery request expired before reservation")?;
    let mut prepared = constructor()?;
    let view = prepared.view;
    let created = prepared.generation.clone();
    if let RecoveryOpenContext::NativeStartup(operation) = &context {
        if operation.selection().surface() != prepared.surface {
            prepared.cx.views.remove_view_if_generation(view, &created);
            return Err("The native recovery selection belongs to another editor".into());
        }
    }
    let closure = prepared.proof.reservation().clone();
    let owner = prepared.proof.owner().clone();
    let target = match (startup, command, request) {
        (Some(startup), Some(command), request) =>
            RecoveryOpenTarget::bind_startup_command(context, view, created.clone(), closure, owner, startup, command, request),
        (Some(startup), None, request) => RecoveryOpenTarget::bind_startup(context, view, created.clone(), closure, owner, startup, request),
        (None, Some(command), None) => RecoveryOpenTarget::bind_command(context, view, created.clone(), closure, owner, command),
        (None, None, Some(request)) => RecoveryOpenTarget::bind_request(context, view, created.clone(), closure, owner, request),
        (None, None, None) => RecoveryOpenTarget::bind(context, view, created.clone(), closure, owner),
        _ => Err("Conflicting recovery authority constraints".into()),
    };
    let target = match target {
        Ok(target) => target,
        Err(error) => { prepared.cx.views.remove_view_if_generation(view, &created); return Err(error); }
    };
    let done = shell_ipc_w::home_files::back_to_core(services.core.clone(), done);
    let opening = Arc::new(Opening {
        cx: prepared.cx.clone(), target, proof: prepared.proof.clone(), closed_receipt: Mutex::new(None), browser: OnceLock::new(), failure_close: OnceLock::new(), origin: OnceLock::new(),
        document_ready: AtomicBool::new(false), initialized: AtomicBool::new(false),
        core: services.core.clone(),
        construction_admitted: AtomicBool::new(false),
        constructed: AtomicBool::new(false), settled: AtomicBool::new(false), closing: AtomicBool::new(false), closed: AtomicBool::new(false),
        draining: AtomicBool::new(false), drained: AtomicBool::new(false),
        subscription: Mutex::new(None), losses: Mutex::new(Vec::new()), result: Mutex::new(None), done: Mutex::new(Some(Box::new(done))),
    });
    let observe_close = opening.clone();
    match opening.proof.when_closed(move |receipt| {
        *observe_close.closed_receipt.lock().unwrap_or_else(|error| error.into_inner()) = Some(receipt);
        observe_close.closed.store(true, Ordering::Release);
        observe_close.cx.views.remove_view_if_generation(observe_close.target.view(), observe_close.target.created());
        if observe_close.settled.load(Ordering::Acquire) { observe_close.complete(); }
        else { observe_close.fail("The recovery browser closed before its document became ready".into()); }
    }) {
        Ok(subscription) => *opening.subscription.lock().unwrap_or_else(|error| error.into_inner()) = Some(subscription),
        Err(error) => {
            opening.constructed.store(true, Ordering::Release);
            opening.fail(error);
            return Ok(());
        }
    }
    let browser_owner = match prepared.cx.views.prepare_browser_owner(view, &prepared.generation, prepared.proof.owner()) {
        Ok(owner) => owner,
        Err(error) => {
            opening.constructed.store(true, Ordering::Release);
            opening.fail(error);
            return Ok(());
        }
    };
    let registered = (|| -> Result<(), String> {
        native_owner::with_recovery_seed_effect(&opening.target, || -> Result<(), String> {
            prepared.proof.reservation().with_initial_owner(prepared.proof.owner(), |transition| {
                if docs.is_some() && sheets.is_some() { return Err("Multiple recovery actors were prepared for one view".into()); }
                let mut browser = Some(browser_owner.validate(transition)?);
                actors.with_validated(transition, &mut || {
                    let browser = browser.take().ok_or("The native browser guard was consumed")?;
                    if let Some(token) = docs.as_mut() { token.commit_admitted(transition, None)?; }
                    if let Some(token) = sheets.as_mut() { token.commit_admitted(transition)?; }
                    browser.commit();
                    Ok(())
                })
            })
        }).ok_or("The recovery actor preparation authority expired")??;
        if let Some(pdf) = pdf {
            let receipt = pdf.recovery_retirement(opening.target.clone())?;
            rollback.borrow_mut().push(Rc::new(PdfRetirement(receipt)));
        }
        if let Some(token) = docs {
            let receipt = token.finish()?;
            let lease = receipt.lease();
            rollback.borrow_mut().push(Rc::new(super::hooks::DocsRegistrationRollback(receipt)));
            prepared.cx.views.set_docs_media_lease(view, lease.clone())?;
            let session = crate::host::document_assets::register_session(view, crate::host::document_assets::AssetKind::Docs)?;
            prepared.cx.views.set_docs_asset_session(view, session)?;
            prepared.docs_lease = Some(lease);
        }
        if let Some(token) = sheets {
            let receipt = token.finish()?;
            rollback.borrow_mut().push(Rc::new(super::hooks::SheetsRegistrationRollback(receipt)));
        }
        Ok(())
    })();
    if let Err(error) = registered {
        opening.constructed.store(true, Ordering::Release);
        opening.fail(error);
        return Ok(());
    }
    let observed = opening.clone();
    prepared.invocation_on_frame = Some(Arc::new(move |view, event| observed.frame(view, event)));
    let navigating = opening.clone();
    let url = prepared.url.clone();
    prepared.invocation_navigate = Some(Arc::new(move |bound| {
        if let Err(error) = bound { navigating.fail(error); return; }
        let opening = navigating.clone();
        let url = url.clone();
        let ui = opening.cx.ui.clone();
        ui.post(Box::new(move || {
            if opening.settled.load(Ordering::Acquire) { return; }
            let Some(receipt) = opening.target.committed_seed() else { return opening.fail("The recovery seed was not committed".into()); };
            let loaded = native_owner::run_recovery_seeded_ui(opening.cx.ui.as_ref(), &receipt,
                || opening.cx.windows.load_view(opening.target.view(), &url))
                .and_then(|result| result);
            if let Err(error) = loaded { opening.fail(error); }
        }));
    }));
    if let Err(error) = opening.arm_authority() {
        opening.constructed.store(true, Ordering::Release);
        opening.fail(error);
        return Ok(());
    }
    if opening.settled.load(Ordering::Acquire) {
        opening.constructed.store(true, Ordering::Release);
        opening.close_created();
        return Ok(());
    }
    let preparation = Preparation(Some(opening.clone()));
    let prepared_opening = opening.clone();
    let prepare_done: RecoveryOpenDone = Box::new(move |result| {
        let mut preparation = preparation;
        drop(preparation.0.take());
        if let Err(error) = result {
            prepared_opening.constructed.store(true, Ordering::Release);
            prepared_opening.fail(error.0);
            return;
        }
        let Some(receipt) = prepared_opening.target.committed_seed() else {
            prepared_opening.constructed.store(true, Ordering::Release);
            prepared_opening.fail("The recovery preparation did not commit its seed".into());
            return;
        };
        let initialized = Arc::downgrade(&prepared_opening);
        let settlement = receipt.on_settled(move |result| {
            if let Some(opening) = initialized.upgrade() {
                match result {
                    Ok(()) => { opening.initialized.store(true, Ordering::Release); opening.finish_if_ready(); }
                    Err(error) => opening.fail(error),
                }
            }
        });
        prepared_opening.losses.lock().unwrap_or_else(|error| error.into_inner()).push(settlement);
        if prepared_opening.settled.load(Ordering::Acquire) {
            prepared_opening.constructed.store(true, Ordering::Release);
            prepared_opening.close_created();
            return;
        }
        let ui = prepared_opening.cx.ui.clone();
        let construction = Construction(Some(prepared_opening.clone()));
        ui.post(Box::new(move || {
            let result = native_owner::run_recovery_seeded_ui(prepared_opening.cx.ui.as_ref(), &receipt, || {
                prepared_opening.construction_admitted.store(true, Ordering::Release);
                move || prepared.install()
            }).map(|action| action()).map_err(|error| super::accepted::InstallError {
                error: main_core::shell::tab_manager::TabError(error), close: None, proof: None,
            }).and_then(|result| result);
            match result {
                Err(error) => {
                    if let Some(close) = error.close {
                        let _ = prepared_opening.failure_close.set(close);
                    } else {
                        prepared_opening.construction_admitted.store(false, Ordering::Release);
                    }
                    prepared_opening.constructed.store(true, Ordering::Release);
                    prepared_opening.fail(error.error.0);
                    drop(construction);
                }
                Ok(child) => {
                    let closed = child.close.clone();
                    let _ = prepared_opening.browser.set(Browser { webview: child.webview.clone(), closed, proof: prepared_opening.proof.clone() });
                    let opening = prepared_opening.clone();
                    super::accepted::await_creation(child, Box::new(move |result| {
                        let mut construction = construction;
                        opening.constructed.store(true, Ordering::Release);
                        match result {
                            Ok(_) if opening.settled.load(Ordering::Acquire) => opening.close_created(),
                            Ok(_) => opening.finish_if_ready(),
                            Err(error) => opening.fail(error.error.0),
                        }
                        drop(construction.0.take());
                    }));
                }
            }
        }));
    });
    let request_target = opening.target.clone();
    let action = native_owner::with_recovery_seed_effect(&opening.target,
        || move || before_load(view, &request_target, prepare_done));
    match action {
        Some(action) => action(),
        None => {
            opening.constructed.store(true, Ordering::Release);
            if opening.settled.load(Ordering::Acquire) { opening.close_created(); }
            else { opening.fail("The recovery request expired before input preparation".into()); }
        }
    }
    Ok(())
}
