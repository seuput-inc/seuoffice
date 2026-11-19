//! The CLI hosts the ordinary Docs renderer and its existing export handshake.

use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::native::{
    BrowserCloseSubscription, BrowserId, DocumentContext, NativeEvent, NativeEventKind, WebviewExt,
};
use main_core::packages::electron_utils::headless_export::HeadlessExportRequest;
use main_core::shell::headless_export::HeadlessExportProduct;
use shell_contract::ipc::{
    NativeInvocationOperation, NativeInvocationSelection, NativeInvocationTarget,
};
use shell_contract::{
    Surface, ViewId, WindowId,
    runtime::{RendererGeneration, Views},
};
use shell_ipc_d2::docs::editors::DocsEditors;

use crate::host::{
    HostContext,
    window::{Bridge, ViewSpec, WindowOptions, WindowSpec},
};

type Done = Box<dyn FnOnce(Result<Option<HeadlessExportProduct>, String>) + Send>;
type Candidate =
    std::rc::Rc<std::cell::RefCell<Option<shell_ipc_d2::docs::editors::DocsActorDisposal>>>;
type Receipt = std::rc::Rc<shell_ipc_d2::docs::editors::DocsRegisteredView>;
const TIMEOUT: Duration = Duration::from_millis(300_000);
const URL: &str = "genoffice-app://docs/index.html";

struct Created {
    window: WindowId,
    view: ViewId,
    generation: RendererGeneration,
    target: NativeInvocationTarget,
    proof: crate::host::window::NativeClosureProof,
    owner: shell_contract::ipc::NativeBrowserOwner,
    webview: Option<tauri::Webview>,
    construction_error: Option<String>,
}

#[derive(Clone)]
struct ReservedCreation {
    window: WindowId,
    view: ViewId,
    generation: RendererGeneration,
    target: NativeInvocationTarget,
    proof: crate::host::window::NativeClosureProof,
    owner: shell_contract::ipc::NativeBrowserOwner,
}

struct Construction {
    job: Arc<Export>,
    result: Option<Result<Created, String>>,
}

impl Drop for Construction {
    fn drop(&mut self) {
        let Some(result) = self.result.take() else {
            return;
        };
        let error = match result {
            Ok(created) => {
                let _ = self.job.created.set(created);
                "the Docs construction core continuation was discarded".to_owned()
            }
            Err(error) => format!("{error}; the Docs construction core continuation was discarded"),
        };
        self.job.creation_finished.store(true, Ordering::Release);
        self.job.watch_installing.store(false, Ordering::Release);
        if self.job.settled.load(Ordering::Acquire) {
            self.job.close_created();
        } else {
            self.job.finish(Err(error));
        }
        self.job
            .retry_failed("The Docs construction continuation was discarded".into());
    }
}
struct ConstructionDispatch {
    job: Arc<Export>,
    ready: Option<Box<dyn FnOnce(Construction) + Send>>,
}

impl Drop for ConstructionDispatch {
    fn drop(&mut self) {
        if self.ready.is_some() {
            if let Some(reserved) = self.job.reserved.get() {
                let _ = self.job.created.set(Created {
                    window: reserved.window,
                    view: reserved.view,
                    generation: reserved.generation.clone(),
                    target: reserved.target.clone(),
                    proof: reserved.proof.clone(),
                    owner: reserved.owner.clone(),
                    webview: None,
                    construction_error: Some("The Docs constructor dispatch was discarded".into()),
                });
            }
            self.job.creation_finished.store(true, Ordering::Release);
            self.job.watch_installing.store(false, Ordering::Release);
            if self.job.settled.load(Ordering::Acquire) {
                self.job.close_created();
            } else {
                self.job
                    .finish(Err("The Docs constructor dispatch was discarded".into()));
            }
            self.job
                .retry_failed("The Docs constructor dispatch was discarded".into());
        }
    }
}

struct CloseDispatch {
    job: Arc<Export>,
    proof: crate::host::window::NativeClosureProof,
    permit: Option<shell_contract::ipc::NativeBrowserClosePermit>,
}

impl CloseDispatch {
    fn run(mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        let outcome = self.proof.dispatch_close(permit);
        let failure = {
            let mut retry = self
                .job
                .retry_permit
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let failure = match outcome {
                Ok(_) => None,
                Err((error, permit)) => {
                    *retry = Some((permit, error.clone()));
                    Some(error)
                }
            };
            self.job.dispatch_finished.store(true, Ordering::Release);
            failure
        };
        if let Some(error) = failure {
            self.job.retry_failed(error);
        }
        if self.job.closed.load(Ordering::Acquire) && self.job.actor_done.load(Ordering::Acquire) {
            self.job.complete_closed();
        }
    }
}

impl Drop for CloseDispatch {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            let error = "The Docs native close dispatch was discarded".to_owned();
            {
                let mut retry = self
                    .job
                    .retry_permit
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *retry = Some((permit, error.clone()));
                self.job.dispatch_finished.store(true, Ordering::Release);
            }
            self.job.retry_failed(error);
        }
    }
}

struct Export {
    self_ref: std::sync::Weak<Export>,
    registration: Mutex<Option<crate::host::headless_cleanup::Registration>>,
    retry_waiter: Mutex<Option<crate::host::headless_cleanup::Done>>,
    cleanup_failure: Mutex<Option<String>>,
    watch_action: Mutex<Option<shell_ipc_d2::docs::editors::CoreReplay>>,
    terminal_action: Mutex<Option<shell_ipc_d2::docs::editors::CoreReplay>>,
    terminal_queued: AtomicBool,
    terminal_processed: AtomicBool,
    cx: HostContext,
    created: OnceLock<Created>,
    reserved: OnceLock<ReservedCreation>,
    creation_finished: AtomicBool,
    committed: AtomicBool,
    registered: OnceLock<DocumentContext>,
    browser: OnceLock<BrowserId>,
    cleanup: OnceLock<shell_contract::ipc::NativeHeadlessCleanup>,
    watch_ready: AtomicBool,
    watch_installing: AtomicBool,
    watch_error: Mutex<Option<String>>,
    closing_started: AtomicBool,
    dispatch_finished: AtomicBool,
    close_completion_started: AtomicBool,
    actor_done: AtomicBool,
    actor_registered: AtomicBool,
    close_action: Mutex<Option<shell_ipc_d2::docs::editors::CoreReplay>>,
    retry_permit: Mutex<Option<(shell_contract::ipc::NativeBrowserClosePermit, String)>>,
    actor_error: Mutex<Option<String>>,
    closing: Mutex<Option<BrowserCloseSubscription>>,
    cancellation: Mutex<Vec<shell_contract::runtime::CancelSubscription>>,
    closed: AtomicBool,
    terminal_receipt: Mutex<Option<shell_contract::ipc::NativeClosureReceipt>>,
    settled: AtomicBool,
    result: Mutex<Option<Result<(), String>>>,
    done: Mutex<Option<Done>>,
}

impl Export {
    fn fail(self: &Arc<Self>, error: String) {
        let job = self.clone();
        let unavailable = self.clone();
        let unavailable_error = error.clone();
        crate::host::w::post_core_checked(
            move |provided, _| {
                if let Some(created) = job.created.get()
                    && let Some(editor) = provided.all::<DocsEditors>().into_iter().next()
                    && editor.fail_headless_export(created.view, error.clone())
                {
                    return;
                }
                job.finish(Err(error));
            },
            move || {
                unavailable.finish(Err(format!(
                    "{unavailable_error}; Docs core services are unavailable"
                )))
            },
        );
    }

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
                        Ok(())
                    } else {
                        Err("the Docs exporter has no matching registered native document".into())
                    }
                }
                _ => Err("the Docs export renderer was replaced".into()),
            };
        }
        *self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(result);
        self.close_created();
    }

    fn close_created(self: &Arc<Self>) {
        if self.created.get().is_none() {
            if self.creation_finished.load(Ordering::Acquire) {
                self.complete(None);
            }
            return;
        }
        if self.closed.load(Ordering::Acquire) {
            if self.actor_done.load(Ordering::Acquire)
                && self.dispatch_finished.load(Ordering::Acquire)
            {
                self.complete_closed();
                return;
            }
            if self.closing_started.load(Ordering::Acquire) {
                return;
            }
        }
        if !self.watch_ready.load(Ordering::Acquire) {
            return;
        }
        let action = self
            .close_action
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(action) = action {
            if self.closing_started.swap(true, Ordering::AcqRel) {
                return;
            }
            let job = self.clone();
            action.post(Box::new(move |result| {
                if let Err(error) = result {
                    job.closing_started.store(false, Ordering::Release);
                    job.retry_failed(error);
                }
            }));
        }
    }
    fn retry_failed(&self, error: String) {
        let waiter = self
            .retry_waiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(waiter) = waiter {
            waiter(Err(error));
        }
    }
    fn post_terminal(self: &Arc<Self>) {
        if self.terminal_processed.load(Ordering::Acquire)
            || self.terminal_queued.swap(true, Ordering::AcqRel)
        {
            return;
        }
        if self.terminal_processed.load(Ordering::Acquire) {
            self.terminal_queued.store(false, Ordering::Release);
            return;
        }
        let action = self
            .terminal_action
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(action) = action else {
            self.terminal_queued.store(false, Ordering::Release);
            return self.retry_failed("The Docs terminal continuation is unavailable".into());
        };
        let job = self.clone();
        action.post(Box::new(move |result| {
            if let Err(error) = result {
                job.terminal_queued.store(false, Ordering::Release);
                job.retry_failed(error);
            }
        }));
    }

    fn install_watcher(
        self: &Arc<Self>,
        services: &shell_contract::runtime::Services,
        candidate: Option<Candidate>,
        receipt: Option<Receipt>,
    ) {
        self.watch_installing.store(true, Ordering::Release);
        let Some(created) = self.created.get() else {
            self.watch_installing.store(false, Ordering::Release);
            return self.retry_failed("The captured Docs creation is unavailable".into());
        };
        let retry_job = self.clone();
        let retry_candidate = candidate.clone();
        let retry_receipt = receipt.clone();
        let retry_services = services.clone();
        let retry = shell_ipc_d2::docs::editors::replay_on_core(services, move || {
            retry_job.install_watcher(&retry_services, retry_candidate, retry_receipt);
        });
        let closed_job = self.clone();
        let terminal_job = self.clone();
        let terminal = shell_ipc_d2::docs::editors::replay_on_core(services, move || {
            terminal_job
                .terminal_processed
                .store(true, Ordering::Release);
            let proof = terminal_job
                .terminal_receipt
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let Some(proof) = proof else {
                terminal_job.terminal_queued.store(false, Ordering::Release);
                return terminal_job
                    .retry_failed("The Docs terminal receipt is unavailable".into());
            };
            let cleanup = receipt
                .as_ref()
                .map(|receipt| receipt.retire_closed(&proof))
                .unwrap_or(Ok(()));
            if let Err(error) = cleanup {
                if !terminal_job.settled.load(Ordering::Acquire) {
                    terminal_job.finish(Err(error.clone()));
                }
                terminal_job.terminal_queued.store(false, Ordering::Release);
                return terminal_job.complete(Some(error));
            }
            let already_closing = terminal_job.closing_started.swap(true, Ordering::AcqRel);
            if !terminal_job.settled.load(Ordering::Acquire) {
                terminal_job.finish(Err("docs renderer stopped (destroyed)".into()));
            }
            if !already_closing {
                terminal_job
                    .dispatch_finished
                    .store(true, Ordering::Release);
                terminal_job.retire_terminal(candidate, receipt, proof);
            } else {
                terminal_job.close_created();
            }
            terminal_job.terminal_queued.store(false, Ordering::Release);
        });
        let previous = self
            .terminal_action
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(terminal);
        if let Some(previous) = previous {
            previous.cancel();
        }
        let closed = move |proof: shell_contract::ipc::NativeClosureReceipt| {
            {
                let _permit = closed_job
                    .retry_permit
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *closed_job
                    .terminal_receipt
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(proof);
                closed_job.closed.store(true, Ordering::Release);
            }
            closed_job.post_terminal();
        };
        match created.proof.when_closed(closed) {
            Ok(closing) => {
                retry.cancel();
                *self.closing.lock().unwrap_or_else(|e| e.into_inner()) = Some(closing);
                self.watch_ready.store(true, Ordering::Release);
                self.watch_installing.store(false, Ordering::Release);
                let _ = self
                    .watch_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if self.settled.load(Ordering::Acquire) {
                    self.close_created();
                }
            }
            Err(error) => {
                *self.watch_action.lock().unwrap_or_else(|e| e.into_inner()) = Some(retry);
                *self.watch_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
                if !self.settled.swap(true, Ordering::AcqRel) {
                    *self.result.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(Err(error.clone()));
                }
                self.watch_installing.store(false, Ordering::Release);
                self.retry_failed(error);
            }
        }
    }

    fn retain_close_retry(
        self: &Arc<Self>,
        candidate: Option<
            std::rc::Rc<std::cell::RefCell<Option<shell_ipc_d2::docs::editors::DocsActorDisposal>>>,
        >,
        receipt: Option<std::rc::Rc<shell_ipc_d2::docs::editors::DocsRegisteredView>>,
        error: String,
    ) {
        let retry_error = error.clone();
        *self.actor_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
        self.actor_done.store(false, Ordering::Release);
        let job = self.clone();
        let installed = crate::host::w::on_core_services_local(|_, services| {
            let action = shell_ipc_d2::docs::editors::replay_on_core(services, move || {
                job.retire_created(candidate, receipt)
            });
            *self.close_action.lock().unwrap_or_else(|e| e.into_inner()) = Some(action);
        });
        if installed {
            self.closing_started.store(false, Ordering::Release);
        }
        self.retry_failed(retry_error);
    }

    fn retire_terminal(
        self: &Arc<Self>,
        candidate: Option<Candidate>,
        receipt: Option<Receipt>,
        proof: shell_contract::ipc::NativeClosureReceipt,
    ) {
        if !self.actor_registered.load(Ordering::Acquire) {
            let Some(created) = self.created.get() else {
                return self.complete(Some("The original Docs reservation is unavailable".into()));
            };
            match proof.with_terminal_owner(&created.owner, |_| Ok(())) {
                Ok(()) => {
                    self.actor_done.store(true, Ordering::Release);
                    if self.dispatch_finished.load(Ordering::Acquire) {
                        self.complete_closed();
                    }
                }
                Err(error) => self.complete(Some(error)),
            }
            return;
        }
        let Some(candidate) = candidate else {
            return self.complete(Some("The captured Docs actor is unavailable".into()));
        };
        let actor = match candidate.borrow_mut().take() {
            Some(actor) => actor,
            None => {
                return self.complete(Some("The captured Docs actor was already claimed".into()));
            }
        };
        let job = self.clone();
        let restore = candidate.clone();
        actor.run_closed(
            proof,
            Box::new(move |outcome| match outcome {
                shell_ipc_d2::docs::editors::DocsDisposalOutcome::NotRetired { actor, error } => {
                    *restore.borrow_mut() = Some(*actor);
                    job.retain_close_retry(Some(restore), receipt, error);
                }
                shell_ipc_d2::docs::editors::DocsDisposalOutcome::Retired { cleanup } => {
                    *job.actor_error.lock().unwrap_or_else(|e| e.into_inner()) = cleanup.err();
                    job.actor_done.store(true, Ordering::Release);
                    if job.dispatch_finished.load(Ordering::Acquire) {
                        job.complete_closed();
                    }
                }
            }),
        );
    }

    fn retire_created(
        self: &Arc<Self>,
        candidate: Option<
            std::rc::Rc<std::cell::RefCell<Option<shell_ipc_d2::docs::editors::DocsActorDisposal>>>,
        >,
        receipt: Option<std::rc::Rc<shell_ipc_d2::docs::editors::DocsRegisteredView>>,
    ) {
        let job = self.clone();
        let terminal = job
            .terminal_receipt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(terminal) = terminal {
            job.dispatch_finished.store(true, Ordering::Release);
            return job.retire_terminal(candidate, receipt, terminal);
        }
        let Some(created) = job.created.get() else {
            return job.complete(Some("the captured Docs creation is unavailable".into()));
        };
        let Some(cleanup) = job.cleanup.get().cloned() else {
            return job.complete(Some(
                "the captured Docs cleanup authority is unavailable".into(),
            ));
        };
        let proof = created.proof.clone();
        let permit = if job.actor_registered.load(Ordering::Acquire) {
            let Some(candidate) = candidate else {
                return job.complete(Some(
                    "the captured Docs cleanup candidate is unavailable".into(),
                ));
            };
            let actor = match candidate.borrow_mut().take() {
                Some(actor) => actor,
                None => {
                    return job.complete(Some(
                        "the captured Docs cleanup candidate was already claimed".into(),
                    ));
                }
            };
            let claimed = std::rc::Rc::new(std::cell::RefCell::new(None));
            let claim_result = claimed.clone();
            let retired_job = job.clone();
            let restore = candidate.clone();
            let retry_claim = cleanup.clone();
            actor.run_cleanup(
                Box::new(move |retire| {
                    match cleanup.claim_close(|transition| retire(transition))? {
                        shell_contract::ipc::NativeBrowserCloseClaim::Won { permit, .. } => {
                            *claim_result.borrow_mut() = Some(permit);
                            Ok(())
                        }
                        shell_contract::ipc::NativeBrowserCloseClaim::Superseded(_) => {
                            Err("the captured Docs browser was superseded".into())
                        }
                    }
                }),
                Box::new(move |outcome| {
                    let error = match outcome {
                        shell_ipc_d2::docs::editors::DocsDisposalOutcome::NotRetired {
                            actor,
                            error,
                        } => {
                            *restore.borrow_mut() = Some(*actor);
                            Some(error)
                        }
                        shell_ipc_d2::docs::editors::DocsDisposalOutcome::Retired { cleanup } => {
                            cleanup.err()
                        }
                    };
                    *retired_job
                        .actor_error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = error;
                    retired_job.actor_done.store(true, Ordering::Release);
                    if retired_job.closed.load(Ordering::Acquire)
                        && retired_job.dispatch_finished.load(Ordering::Acquire)
                    {
                        retired_job.complete_closed();
                    }
                }),
            );
            let permit = claimed.borrow_mut().take();
            match permit {
                Some(permit) => permit,
                None => {
                    if let Ok(shell_contract::ipc::NativeBrowserCloseClaim::Superseded(successor)) =
                        retry_claim.claim_close(|_| {
                            Err::<(), _>("the captured Docs actor was not retired".into())
                        })
                    {
                        let result = receipt
                            .as_ref()
                            .ok_or_else(|| {
                                "the original Docs registration receipt is unavailable".to_owned()
                            })
                            .and_then(|receipt| receipt.retire_superseded(&successor));
                        if result.is_ok() {
                            let retired = candidate.borrow_mut().take();
                            drop(retired);
                        }
                        return job.complete(result.err());
                    }
                    let error = job
                        .actor_error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .unwrap_or_else(|| "the captured Docs retirement was not admitted".into());
                    job.retain_close_retry(Some(candidate), receipt, error);
                    return;
                }
            }
        } else {
            match cleanup.claim_close(|_| Ok(())) {
                Ok(shell_contract::ipc::NativeBrowserCloseClaim::Won { permit, .. }) => {
                    job.actor_done.store(true, Ordering::Release);
                    permit
                }
                Ok(shell_contract::ipc::NativeBrowserCloseClaim::Superseded(_)) => {
                    return job.complete(None);
                }
                Err(error) => {
                    job.retain_close_retry(None, receipt, error);
                    return;
                }
            }
        };
        job.dispatch_close(proof, permit);
    }
    fn dispatch_close(
        self: &Arc<Self>,
        proof: crate::host::window::NativeClosureProof,
        permit: shell_contract::ipc::NativeBrowserClosePermit,
    ) {
        let dispatch = CloseDispatch {
            job: self.clone(),
            proof,
            permit: Some(permit),
        };
        self.cx.ui.post(Box::new(move || dispatch.run()));
    }

    fn complete_closed(&self) {
        if self
            .result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return;
        }
        if self.close_completion_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let actor = self
            .actor_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let dispatch = self
            .retry_permit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .map(|(_, error)| error);
        if let Some(error) = actor {
            self.complete(Some(match dispatch {
                Some(dispatch) => format!("{error}; native close dispatch failed: {dispatch}"),
                None => error,
            }));
            return;
        }
        if let Some(error) = dispatch {
            let mut result = self.result.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(previous) = result.take() {
                *result = Some(Err(match previous {
                    Ok(()) => format!("the Docs native close dispatch failed: {error}"),
                    Err(first) => format!("{first}; Docs native close dispatch failed: {error}"),
                }));
            }
        }
        self.complete(None);
    }

    fn complete(&self, cleanup_error: Option<String>) {
        if let Some(error) = cleanup_error {
            *self
                .cleanup_failure
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
            self.retry_failed(error);
            return;
        }
        let result = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let Some(result) = result else {
            return;
        };
        let action = self
            .close_action
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(action) = action {
            action.cancel();
        }
        let watch_action = self
            .watch_action
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(action) = watch_action {
            action.cancel();
        }
        let terminal_action = self
            .terminal_action
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(action) = terminal_action {
            action.cancel();
        }
        let subscription = self
            .closing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        drop(subscription);
        let cancellation = std::mem::take(
            &mut *self
                .cancellation
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        drop(cancellation);
        if self.closed.load(Ordering::Acquire)
            && let Some(created) = self.created.get()
        {
            self.cx.views.remove_window(created.window);
        }
        let mut registration = self.registration.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(registration) = registration.take() {
            registration.complete();
        }
        drop(registration);
        let waiter = self
            .retry_waiter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(waiter) = waiter {
            waiter(Ok(()));
        }
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

impl crate::host::headless_cleanup::CleanupTask for Export {
    fn retry(&self, done: crate::host::headless_cleanup::Done) {
        let registration = self.registration.lock().unwrap_or_else(|e| e.into_inner());
        if registration.is_none() {
            drop(registration);
            done(Ok(()));
            return;
        }
        let mut waiter = self.retry_waiter.lock().unwrap_or_else(|e| e.into_inner());
        if waiter.is_some() {
            drop(waiter);
            drop(registration);
            done(Err("The Docs cleanup retry is already active".into()));
            return;
        }
        *waiter = Some(done);
        drop(waiter);
        drop(registration);
        let Some(job) = self.self_ref.upgrade() else {
            return self.retry_failed("The Docs cleanup was released".into());
        };
        let cleanup_failure = self
            .cleanup_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(error) = cleanup_failure {
            return self.retry_failed(error);
        }
        if !self.settled.load(Ordering::Acquire) {
            job.fail("Docs headless cleanup was requested".into());
            return;
        }
        if self.closed.load(Ordering::Acquire) {
            if self.terminal_queued.load(Ordering::Acquire) {
                return;
            }
            if !self.terminal_processed.load(Ordering::Acquire) {
                job.post_terminal();
                return;
            }
        }
        if !self.watch_ready.load(Ordering::Acquire) {
            if self.watch_installing.swap(true, Ordering::AcqRel) {
                return;
            }
            let action = self
                .watch_action
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(action) = action {
                let failed = job.clone();
                action.post(Box::new(move |result| {
                    if let Err(error) = result {
                        failed.watch_installing.store(false, Ordering::Release);
                        failed.retry_failed(error);
                    }
                }));
                return;
            }
            if self.created.get().is_none() {
                self.watch_installing.store(false, Ordering::Release);
                if self.creation_finished.load(Ordering::Acquire) {
                    self.complete(None);
                }
                return;
            }
            if self.actor_registered.load(Ordering::Acquire) {
                self.watch_installing.store(false, Ordering::Release);
                return self
                    .retry_failed("The captured Docs watcher continuation is unavailable".into());
            }
            let ready = job.clone();
            let unavailable = job.clone();
            crate::host::w::post_core_checked(
                move |_, services| {
                    let close_job = ready.clone();
                    let action = shell_ipc_d2::docs::editors::replay_on_core(services, move || {
                        close_job.retire_created(None, None)
                    });
                    *ready.close_action.lock().unwrap_or_else(|e| e.into_inner()) = Some(action);
                    ready.install_watcher(services, None, None);
                },
                move || {
                    unavailable.watch_installing.store(false, Ordering::Release);
                    unavailable.retry_failed("The Docs core is unavailable during cleanup".into());
                },
            );
            return;
        }
        let permit = {
            let mut retry = self.retry_permit.lock().unwrap_or_else(|e| e.into_inner());
            if self.closed.load(Ordering::Acquire) {
                None
            } else {
                if retry.is_some() {
                    self.dispatch_finished.store(false, Ordering::Release);
                }
                retry.take().map(|(permit, _)| permit)
            }
        };
        if let Some(permit) = permit {
            let Some(created) = self.created.get() else {
                return self.retry_failed("The Docs close reservation is unavailable".into());
            };
            job.dispatch_close(created.proof.clone(), permit);
            return;
        }
        job.close_created();
    }
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
            surface: Surface::Docs,
            input: selected_input,
            output: selected_out,
            format: selected_format,
            page,
            ops,
            batch,
        } if selected_input == std::path::Path::new(input)
            && selected_out == std::path::Path::new(out)
            && selected_format == format
            && request.target_format == format
            && *page == request.page
            && ops == &request.ops
            && batch.as_deref() == request.batch => {}
        _ => return Err("the native Docs export does not match the selected invocation".into()),
    }
    let operation = operation.clone();
    let mut accepted = Err("Docs headless export requires the registered core thread".to_owned());
    let cx = cx.clone();
    let input = input.to_owned();
    let out = out.to_owned();
    let format = format.to_owned();
    let expected_url =
        crate::app_scheme::webview_url(URL).ok_or("the Docs renderer URL is unavailable")?;
    let mut done = Some(done);
    crate::host::w::on_core_services_local(|provided, services| {
        let Some(editor) = provided.all::<DocsEditors>().into_iter().next() else {
            accepted = Err("the Docs editor is unavailable".into());
            return;
        };
        let job = Arc::new_cyclic(|weak| Export {
            self_ref: weak.clone(),
            registration: Mutex::new(None),
            retry_waiter: Mutex::new(None),
            cleanup_failure: Mutex::new(None),
            watch_action: Mutex::new(None),
            terminal_action: Mutex::new(None),
            terminal_queued: AtomicBool::new(false),
            terminal_processed: AtomicBool::new(false),
            cx: cx.clone(),
            created: OnceLock::new(),
            reserved: OnceLock::new(),
            registered: OnceLock::new(),
            browser: OnceLock::new(),
            cleanup: OnceLock::new(),
            closing_started: AtomicBool::new(false),
            actor_done: AtomicBool::new(false),
            dispatch_finished: AtomicBool::new(false),
            close_completion_started: AtomicBool::new(false),
            watch_ready: AtomicBool::new(false),
            watch_installing: AtomicBool::new(true),
            watch_error: Mutex::new(None),
            actor_registered: AtomicBool::new(false),
            actor_error: Mutex::new(None),
            close_action: Mutex::new(None),
            retry_permit: Mutex::new(None),
            creation_finished: AtomicBool::new(false),
            committed: AtomicBool::new(false),
            closing: Mutex::new(None),
            closed: AtomicBool::new(false),
            terminal_receipt: Mutex::new(None),
            settled: AtomicBool::new(false),
            result: Mutex::new(None),
            done: Mutex::new(done.take()),
            cancellation: Mutex::new(Vec::new()),
        });
        let registration = match cx.headless_cleanup.register(job.clone()) {
            Ok(registration) => registration,
            Err(error) => {
                accepted = Err(error);
                return;
            }
        };
        *job.registration.lock().unwrap_or_else(|e| e.into_inner()) = Some(registration);
        let cancelled = job.clone();
        let mut cancellation = vec![
            operation
                .cancel()
                .on_cancel(move || cancelled.fail("Docs headless export was cancelled".into())),
        ];
        if !operation.shutdown().same_token(operation.cancel()) {
            let cancelled = job.clone();
            cancellation.push(
                operation
                    .shutdown()
                    .on_cancel(move || cancelled.fail("Docs headless startup was stopped".into())),
            );
        }
        let cancelled = job.clone();
        cancellation.push(
            operation
                .loss_token()
                .on_cancel(move || cancelled.fail("the Docs headless invocation was ended".into())),
        );
        *job.cancellation
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = cancellation;
        let prepared_job = job.clone();
        let callback_services = services.clone();
        let ready = shell_ipc_d2::docs::editors::back_on_core(
            services,
            move |mut construction: Construction| {
                let created = match construction.result.take().expect("one construction result") {
                    Ok(created) => created,
                    Err(error) => {
                        prepared_job
                            .creation_finished
                            .store(true, Ordering::Release);
                        prepared_job
                            .watch_installing
                            .store(false, Ordering::Release);
                        if prepared_job.settled.load(Ordering::Acquire) {
                            prepared_job.close_created();
                        } else {
                            prepared_job.finish(Err(error));
                        }
                        return;
                    }
                };
                let view = created.view;
                if prepared_job.created.set(created).is_err() {
                    return prepared_job
                        .finish(Err("the hidden Docs browser was constructed twice".into()));
                }
                prepared_job
                    .creation_finished
                    .store(true, Ordering::Release);
                let Some(created) = prepared_job.created.get() else {
                    return prepared_job
                        .finish(Err("the hidden Docs construction was released".into()));
                };
                let mut candidate = None;
                let registration = if !prepared_job.settled.load(Ordering::Acquire)
                    && created.construction_error.is_none()
                {
                    let result = (|| {
                        let mut registration = editor.prepare_view_registration(
                            view,
                            created.generation.clone(),
                            None,
                            created.proof.reservation(),
                            &created.owner,
                        )?;
                        candidate = Some(registration.prepare_headless_cleanup()?);
                        let browser_owner = prepared_job.cx.views.prepare_browser_owner(
                            view,
                            &created.generation,
                            &created.owner,
                        )?;
                        created
                            .target
                            .with_active(|| {
                                created.proof.reservation().with_initial_owner(
                                    &created.owner,
                                    |transition| {
                                        let actor = registration
                                            .validate_in_transition(transition, None)?;
                                        let browser_owner = browser_owner.validate(transition)?;
                                        actor.commit();
                                        browser_owner.commit();
                                        Ok(())
                                    },
                                )
                            })
                            .ok_or("the Docs invocation was cancelled before registration")??;
                        prepared_job.actor_registered.store(true, Ordering::Release);
                        let receipt = registration.finish()?;
                        let lease = receipt.lease();
                        Ok::<_, String>((receipt, lease))
                    })();
                    match result {
                        Ok(registration) => Some(registration),
                        Err(error) => {
                            prepared_job.finish(Err(error));
                            None
                        }
                    }
                } else {
                    None
                };
                let lease = registration.as_ref().map(|(_, lease)| lease.clone());
                let receipt = registration.map(|(receipt, _)| std::rc::Rc::new(receipt));
                let close_job = prepared_job.clone();
                let close_candidate = candidate.clone();
                let close_receipt = receipt.clone();
                let action =
                    shell_ipc_d2::docs::editors::replay_on_core(&callback_services, move || {
                        close_job.retire_created(close_candidate, close_receipt)
                    });
                *prepared_job
                    .close_action
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(action);
                prepared_job.install_watcher(&callback_services, candidate, receipt);
                if !prepared_job.watch_ready.load(Ordering::Acquire) {
                    return;
                }
                if let Some(error) = &created.construction_error {
                    if !prepared_job.settled.load(Ordering::Acquire) {
                        prepared_job.finish(Err(error.clone()));
                    } else {
                        prepared_job.close_created();
                    }
                    return;
                }
                if prepared_job.settled.load(Ordering::Acquire) {
                    prepared_job.close_created();
                    return;
                }
                let Some(lease) = lease else {
                    return prepared_job
                        .finish(Err("the hidden Docs registration is unavailable".into()));
                };
                if let Err(error) = (|| {
                    prepared_job
                        .cx
                        .views
                        .set_docs_media_lease(view, lease.clone())?;
                    let session = crate::host::document_assets::register_session(
                        view,
                        crate::host::document_assets::AssetKind::Docs,
                    )?;
                    prepared_job
                        .cx
                        .views
                        .set_docs_asset_session(view, session)?;
                    Ok::<(), String>(())
                })() {
                    return prepared_job.finish(Err(error));
                }
                let finished = prepared_job.clone();
                let Some(created) = prepared_job.created.get() else {
                    return prepared_job
                        .finish(Err("the hidden Docs construction was released".into()));
                };
                editor.begin_headless_export_native(
                    view,
                    input,
                    out,
                    &format,
                    TIMEOUT,
                    &created.target,
                    Box::new(move |result| finished.finish(result)),
                );
                let Some(webview) = created.webview.clone() else {
                    return prepared_job
                        .finish(Err("the hidden Docs webview is unavailable".into()));
                };
                let navigation_job = prepared_job.clone();
                let browser = match webview.native_browser() {
                    Ok(browser) => browser,
                    Err(error) => return prepared_job.fail(error),
                };
                let ready_browser = browser.clone();
                browser.navigator().when_ready(move |result| {
                    if let Err(error) = result {
                        navigation_job.fail(error);
                        return;
                    }
                    let bind_job = navigation_job.clone();
                    if bind_job.settled.load(Ordering::Acquire) {
                        return;
                    }
                    let bound = bind_job.cx.views.bind_docs_media_browser(
                        view,
                        &lease,
                        ready_browser.id(),
                        ready_browser.clone(),
                    );
                    if let Err(error) = bound {
                        bind_job.fail(error);
                        return;
                    }
                    if bind_job.browser.set(ready_browser.id()).is_err() {
                        bind_job.fail("the hidden Docs browser identity was bound twice".into());
                        return;
                    }
                    let load_job = bind_job.clone();
                    bind_job.cx.ui.post(Box::new(move || {
                        let Some(created) = load_job.created.get() else {
                            return;
                        };
                        let result = crate::ipc::native_owner::run_native_invocation_created_ui(
                            load_job.cx.ui.as_ref(),
                            &created.target,
                            || load_job.cx.windows.load_view(view, URL),
                        )
                        .and_then(|result| result);
                        if let Err(error) = result {
                            load_job.fail(error);
                        }
                    }));
                });
            },
        );
        let ui = cx.ui.clone();
        let mut construction = ConstructionDispatch {
            job: job.clone(),
            ready: Some(ready),
        };
        ui.post(Box::new(move || {
            let job = construction.job.clone();
            let frame_job = job.clone();
            let on_frame = Arc::new(move |_: ViewId, event: &NativeEvent| match &event.kind {
                NativeEventKind::DocumentCommitted { url, .. } => {
                    if frame_job.committed.load(Ordering::Acquire) {
                        frame_job.fail("the hidden Docs document was replaced".into());
                    } else if url == &expected_url {
                        frame_job.committed.store(true, Ordering::Release);
                    }
                }
                NativeEventKind::DocumentRegistered { context: origin }
                    if frame_job.committed.load(Ordering::Acquire)
                        && event.browser.is_same_browser(origin.browser()) =>
                {
                    if let Some(original) = frame_job.registered.get() {
                        if original != origin {
                            frame_job.fail("the hidden Docs native document was replaced".into());
                        }
                    } else {
                        let registered = (|| {
                            let created = frame_job
                                .created
                                .get()
                                .ok_or("the hidden Docs reservation is unavailable")?;
                            if frame_job.browser.get() != Some(&origin.browser().id()) {
                                return Err(
                                    "the registered Docs browser does not match its reservation"
                                        .to_owned(),
                                );
                            }
                            let owner = crate::ipc::native_owner::owner_with_browser(
                                created.view,
                                origin.clone(),
                                created.generation.clone(),
                                (),
                                Some(created.owner.clone()),
                            )
                            .ok_or("the native Docs registration was revoked")?;
                            crate::ipc::native_owner::register_native_invocation_target(
                                &created.target,
                                owner,
                            )?;
                            frame_job.registered.set(origin.clone()).map_err(|_| {
                                "the native Docs document was registered twice".to_owned()
                            })?;
                            Ok::<(), String>(())
                        })();
                        if let Err(error) = registered {
                            frame_job.fail(error);
                        }
                    }
                }
                NativeEventKind::DocumentRevoked { document, .. }
                    if frame_job
                        .registered
                        .get()
                        .is_some_and(|original| original.identity() == document) =>
                {
                    frame_job.fail("docs renderer stopped (document revoked)".into());
                }
                NativeEventKind::WebviewProcessFailed { .. } => {
                    frame_job.fail("docs renderer stopped (terminated)".into())
                }
                _ => {}
            });
            let spec = WindowSpec {
                title: "Docs".into(),
                size: (1360.0, 900.0),
                min_size: None,
                visible: false,
                decorations: true,
                transparent: false,
                resizable: true,
                parent: None,
                focus: false,
                main: false,
                view: ViewSpec {
                    url: URL.into(),
                    bridge: Bridge::Surface(Surface::Docs),
                },
            };
            let options = WindowOptions {
                background_throttling: Some(
                    tauri::utils::config::BackgroundThrottlingPolicy::Disabled,
                ),
                on_frame: Some(on_frame),
                ..Default::default()
            };
            let result = (|| {
                let reserved = crate::ipc::native_owner::run_native_invocation_ui(
                    job.cx.ui.as_ref(),
                    &operation,
                    || job.cx.windows.reserve_deferred_window(Surface::Docs),
                )
                .and_then(|result| result)?;
                let window = reserved.window;
                let view = reserved.view;
                let generation = reserved.created.clone();
                let proof = reserved.proof.clone();
                let owner = proof.owner().clone();
                let target = operation.reserve_target(view, generation.clone())?;
                let cleanup = shell_contract::ipc::NativeHeadlessCleanup::capture(
                    &target,
                    proof.reservation(),
                    &owner,
                    job.cx.cleanup_shutdown.clone(),
                )?;
                job.cleanup
                    .set(cleanup)
                    .map_err(|_| "the Docs cleanup authority was captured twice".to_owned())?;
                job.reserved
                    .set(ReservedCreation {
                        window,
                        view,
                        generation: generation.clone(),
                        target: target.clone(),
                        proof: proof.clone(),
                        owner: owner.clone(),
                    })
                    .map_err(|_| "the Docs creation reservation was captured twice".to_owned())?;
                let opened = crate::ipc::native_owner::run_native_invocation_ui(
                    job.cx.ui.as_ref(),
                    &operation,
                    || {
                        job.cx
                            .windows
                            .open_reserved_deferred_window(spec, options, reserved)
                    },
                )
                .and_then(|result| result);
                let webview = if opened.is_ok() {
                    job.cx.windows.webview(view)
                } else {
                    None
                };
                let construction_error = opened.err().or_else(|| {
                    webview
                        .is_none()
                        .then(|| "the hidden Docs webview was not constructed".to_owned())
                });
                Ok(Created {
                    window,
                    view,
                    generation,
                    target,
                    proof,
                    owner,
                    webview,
                    construction_error,
                })
            })();
            if let Some(ready) = construction.ready.take() {
                ready(Construction {
                    job: job.clone(),
                    result: Some(result),
                });
            }
        }));
        accepted = Ok(());
    });
    accepted
}
