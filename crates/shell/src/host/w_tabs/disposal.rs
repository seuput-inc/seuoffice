//! One-shot disposal of the actor, renderer and browser captured before the dirty query.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::native::{
    BrowserClose, BrowserCloseOutcome, BrowserCloseSubscription, DocumentContext,
    DocumentEvaluation, WebviewExt,
};
use shell_contract::{ViewId, ipc::NativeRequestOwner, runtime::CancelToken};
use shell_ipc_w::tabs::{DocsDisposal, DocsDisposalOperation};

use super::Hooks;
use crate::host::{
    HostContext,
    renderer_disposal::{self, CapturedRenderer},
};
use crate::ipc::native_owner::{self, NativeOwnerAuthority};

type Done = Box<dyn FnOnce(Result<DocsDisposal, String>)>;
type SendDone = Box<dyn FnOnce(Result<DocsDisposal, String>) + Send>;
pub(super) type Prepared = Rc<dyn DocsDisposalOperation>;

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare(
    hooks: &Hooks,
    view: ViewId,
    origin: NativeRequestOwner,
    destination: NativeRequestOwner,
    mut participants: Vec<NativeRequestOwner>,
    target: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
) -> Result<Prepared, String> {
    if target.view != view {
        return Err("The disposal target does not match its captured document".into());
    }
    let native = target
        .context
        .authority_ref::<NativeOwnerAuthority>()
        .ok_or("The disposal target has no native authority")?
        .origin
        .clone();
    let docs = hooks
        .editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs")
        .map_err(|error| error.0)?;
    let actor = docs.capture_disposal(view, &target)?;
    let webview = hooks
        .cx
        .windows
        .webview(view)
        .ok_or("The original document browser is unavailable")?;
    let closed = webview
        .native_browser_close()
        .map_err(|error| error.to_string())?;
    let renderer = renderer_disposal::capture(view, &native)?;
    participants.push(destination);
    participants.push(target.clone());
    native_owner::with_many_owner_transition(&origin, &participants, &cancel, &pending, |_| ())
        .ok_or("The document disposal request is no longer authorized")?;
    let native_disposal = Arc::new(Disposal {
        cx: hooks.cx.clone(),
        events: hooks.services.events.clone(),
        target: target.clone(),
        native,
        renderer,
        closed,
        actor_error: Mutex::new(None),
        done: Mutex::new(None),
        evaluation: Mutex::new(None),
        closing: Mutex::new(None),
        unloaded: AtomicBool::new(false),
        close_started: AtomicBool::new(false),
        browser_closed: AtomicBool::new(false),
        teardown_done: AtomicBool::new(false),
        teardown_uncertain: AtomicBool::new(false),
        attempt: AtomicU64::new(0),
    });
    Ok(Rc::new(PreparedDisposal {
        actor: RefCell::new(Some(actor)),
        native: native_disposal,
        origin,
        participants,
        target,
        cancel,
        pending,
        busy: Cell::new(false),
        retired: Cell::new(false),
        finished: Cell::new(false),
        core: hooks.services.core.clone(),
    }))
}

struct PreparedDisposal {
    actor: RefCell<Option<shell_ipc_d2::docs::editors::DocsActorDisposal>>,
    native: Arc<Disposal>,
    origin: NativeRequestOwner,
    participants: Vec<NativeRequestOwner>,
    target: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    busy: Cell<bool>,
    retired: Cell<bool>,
    finished: Cell<bool>,
    core: Arc<dyn shell_contract::runtime::Thread>,
}

impl DocsDisposalOperation for PreparedDisposal {
    fn run(self: Rc<Self>, done: Done) {
        if self.finished.get() {
            return done(Err("The captured document was already disposed".into()));
        }
        if self.busy.replace(true) {
            return done(Err(
                "The captured document disposal is already running".into()
            ));
        }
        if self.retired.get() {
            return self.start_native(done);
        }
        let Some(actor) = self.actor.borrow_mut().take() else {
            self.busy.set(false);
            return done(Err("The captured document actor is unavailable".into()));
        };
        let origin = self.origin.clone();
        let participants = self.participants.clone();
        let cancel = self.cancel.clone();
        let pending = self.pending.clone();
        let target = self.target.clone();
        let operation = self.clone();
        actor.run(
            Box::new(move |effect| {
                native_owner::with_many_owner_transition(
                    &origin,
                    &participants,
                    &cancel,
                    &pending,
                    |retirement| {
                        let owner = native_owner::authenticated_browser_owner(&target.context)
                            .ok_or("The disposal target has no adopted browser owner")?;
                        owner
                            .reservation()
                            .with_owner(owner, |transition| effect(transition))?;
                        if let Some(document) = target.context.document_lease() {
                            retirement.retire(document)?;
                        }
                        Ok::<(), String>(())
                    },
                )
                .ok_or_else(|| "The original document disposal authority expired".to_owned())?
            }),
            Box::new(move |result| match result {
                shell_ipc_d2::docs::editors::DocsDisposalOutcome::NotRetired { actor, error } => {
                    *operation.actor.borrow_mut() = Some(*actor);
                    operation.busy.set(false);
                    done(Err(error));
                }
                shell_ipc_d2::docs::editors::DocsDisposalOutcome::Retired { cleanup } => {
                    operation.retired.set(true);
                    *operation
                        .native
                        .actor_error
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = cleanup.err();
                    operation.start_native(done);
                }
            }),
        );
    }
}

impl PreparedDisposal {
    fn start_native(self: Rc<Self>, done: Done) {
        let operation = self.clone();
        let completion = shell_ipc_w::home_files::back_to_core(
            self.core.clone(),
            move |result: Result<DocsDisposal, String>| {
                operation.busy.set(false);
                if result.is_ok() {
                    operation.finished.set(true);
                }
                done(result);
            },
        );
        self.native.clone().start_attempt(completion);
    }
}

struct Disposal {
    cx: HostContext,
    events: Arc<dyn shell_contract::ipc::EventSink>,
    target: NativeRequestOwner,
    native: DocumentContext,
    renderer: CapturedRenderer,
    closed: BrowserClose,
    actor_error: Mutex<Option<String>>,
    done: Mutex<Option<SendDone>>,
    evaluation: Mutex<Option<DocumentEvaluation>>,
    closing: Mutex<Option<BrowserCloseSubscription>>,
    unloaded: AtomicBool,
    close_started: AtomicBool,
    browser_closed: AtomicBool,
    teardown_done: AtomicBool,
    teardown_uncertain: AtomicBool,
    attempt: AtomicU64,
}

#[derive(Debug, PartialEq, Eq)]
enum NativePhase {
    Teardown,
    Renderer,
    Browser,
    Uncertain,
}

fn native_phase(unloaded: bool, teardown_done: bool, teardown_uncertain: bool) -> NativePhase {
    if unloaded {
        NativePhase::Browser
    } else if teardown_done {
        NativePhase::Renderer
    } else if teardown_uncertain {
        NativePhase::Uncertain
    } else {
        NativePhase::Teardown
    }
}

impl Disposal {
    fn active(&self, attempt: u64) -> bool {
        self.attempt.load(Ordering::Acquire) == attempt
    }

    fn start_attempt(self: Arc<Self>, done: SendDone) {
        let Some(next_terminal) = self.attempt.load(Ordering::Acquire).checked_add(2) else {
            return done(Err(
                "The document disposal attempt counter was exhausted".into()
            ));
        };
        let attempt = next_terminal - 1;
        *self.done.lock().unwrap_or_else(|error| error.into_inner()) = Some(done);
        self.attempt.store(attempt, Ordering::Release);
        self.clone().start(attempt);
    }

    fn start(self: Arc<Self>, attempt: u64) {
        let closed = self.clone();
        let subscription = self.closed.when_closed(move |outcome| {
            if !closed.active(attempt) {
                return;
            }
            match outcome {
                BrowserCloseOutcome::Retired => {
                    closed.browser_closed.store(true, Ordering::Release);
                    closed.complete_if_closed(attempt);
                }
                BrowserCloseOutcome::NotCreated => closed.finish(
                    attempt,
                    Err("An existing Docs browser cannot be acknowledged as never created".into()),
                ),
            }
        });
        match subscription {
            Ok(subscription) => {
                *self
                    .closing
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(subscription)
            }
            Err(error) => return self.finish(attempt, Err(error)),
        }
        match native_phase(
            self.unloaded.load(Ordering::Acquire),
            self.teardown_done.load(Ordering::Acquire),
            self.teardown_uncertain.load(Ordering::Acquire),
        ) {
            NativePhase::Browser => return self.close_browser(attempt),
            NativePhase::Renderer => return self.release_renderer(attempt),
            NativePhase::Uncertain => return self.finish(
                attempt,
                Err(
                    "The captured Docs teardown outcome is unknown; it cannot be repeated safely"
                        .into(),
                ),
            ),
            NativePhase::Teardown => {}
        }
        let ui = self.cx.ui.clone();
        ui.post(Box::new(move || {
            if !self.active(attempt) {
                return;
            }
            if let Err(error) =
                self.events
                    .emit_to_owner_checked(&self.target, "docs:teardown", Vec::new())
            {
                return self.finish(
                    attempt,
                    Err(format!(
                        "The captured Docs teardown event was not delivered: {error}"
                    )),
                );
            }
            // The wake that drains the event is ordered before this round trip in the page.
            let script = "true".to_owned();
            let completed = self.clone();
            self.teardown_uncertain.store(true, Ordering::Release);
            match self.native.evaluate(script, move |result| {
                if !completed.active(attempt) {
                    return;
                }
                let evaluation = completed
                    .evaluation
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                drop(evaluation);
                match result {
                    Ok(_) => {
                        completed.teardown_done.store(true, Ordering::Release);
                        completed.teardown_uncertain.store(false, Ordering::Release);
                        completed.release_renderer(attempt);
                    }
                    Err(error) => completed.finish(
                        attempt,
                        Err(format!(
                            "The captured Docs teardown event failed: {error:?}"
                        )),
                    ),
                }
            }) {
                Ok(evaluation) => {
                    *self
                        .evaluation
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(evaluation);
                    if !self.active(attempt) {
                        let evaluation = self
                            .evaluation
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .take();
                        drop(evaluation);
                    }
                }
                Err(error) => {
                    self.teardown_uncertain.store(false, Ordering::Release);
                    self.finish(
                        attempt,
                        Err(format!(
                            "The captured Docs renderer is unavailable: {error:?}"
                        )),
                    );
                }
            }
        }));
    }

    fn release_renderer(self: Arc<Self>, attempt: u64) {
        if !self.active(attempt) {
            return;
        }
        let ready = self.clone();
        let released = renderer_disposal::release_owned(
            &self.renderer,
            Box::new(move |result| {
                if !ready.active(attempt) {
                    return;
                }
                match result {
                    Ok(()) => {
                        ready.unloaded.store(true, Ordering::Release);
                        ready.close_browser(attempt);
                    }
                    Err(error) => ready.finish(attempt, Err(error)),
                }
            }),
        );
        if let Err(error) = released {
            self.finish(attempt, Err(error));
        }
    }

    fn close_browser(self: Arc<Self>, attempt: u64) {
        if !self.active(attempt) {
            return;
        }
        if self.browser_closed.load(Ordering::Acquire) {
            return self.complete_if_closed(attempt);
        }
        if self.close_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let ui = self.cx.ui.clone();
        ui.post(Box::new(move || {
            if self.browser_closed.load(Ordering::Acquire) {
                return;
            }
            if !renderer_disposal::can_close_owned(&self.renderer) {
                return self.close_failed(
                    "The captured renderer changed before its browser could close".into(),
                );
            }
            if let Err(error) = self.closed.request_close() {
                self.close_failed(error);
            }
        }));
    }

    fn close_failed(self: Arc<Self>, error: String) {
        self.close_started.store(false, Ordering::Release);
        let current = self.attempt.load(Ordering::Acquire);
        if current % 2 == 1 {
            self.finish(current, Err(error));
        }
    }

    fn complete_if_closed(self: Arc<Self>, attempt: u64) {
        if self.unloaded.load(Ordering::Acquire) && self.browser_closed.load(Ordering::Acquire) {
            self.finish(attempt, Ok(()));
        }
    }

    fn finish(self: Arc<Self>, attempt: u64, result: Result<(), String>) {
        if self
            .attempt
            .compare_exchange(attempt, attempt + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        renderer_disposal::cancel_owned(&self.renderer);
        let evaluation = self
            .evaluation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let closing = self
            .closing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        drop((evaluation, closing));
        if self.unloaded.load(Ordering::Acquire)
            && self.browser_closed.load(Ordering::Acquire)
            && let Some(generation) = self.target.context.renderer()
        {
            self.cx
                .views
                .remove_view_if_generation(self.target.view, generation);
        }
        let actor_error = self
            .actor_error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let result = match (&actor_error, result) {
            (None, result) => result,
            (Some(actor), Ok(())) => Err(actor.clone()),
            (Some(actor), Err(native)) => Err(format!("{actor}; native disposal failed: {native}")),
        }
        .and_then(|()| DocsDisposal::closed(self.target.clone()));
        let done = self
            .done
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(done) = done {
            done(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NativePhase, native_phase};

    #[test]
    fn retry_resumes_only_the_unfinished_native_phase() {
        assert_eq!(native_phase(false, false, false), NativePhase::Teardown);
        assert_eq!(native_phase(false, true, false), NativePhase::Renderer);
        assert_eq!(native_phase(true, true, false), NativePhase::Browser);
        assert_eq!(native_phase(false, false, true), NativePhase::Uncertain);
        assert_eq!(native_phase(true, false, true), NativePhase::Browser);
    }

    #[test]
    fn late_attempt_cannot_finish_the_new_attempt() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let attempt = AtomicU64::new(1);
        assert!(
            attempt
                .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        );
        attempt.store(3, Ordering::Release);
        assert!(
            attempt
                .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        );
        assert_eq!(attempt.load(Ordering::Acquire), 3);
    }
}
