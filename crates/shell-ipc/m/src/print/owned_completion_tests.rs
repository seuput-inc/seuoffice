//! Exercises the real completion helper against the existing synthetic owner
//! gates. These tests are not native ingress or modal-loop execution evidence.
use super::*;
use crate::testkit::{Probe, sender};
use shell_contract::{Surface, hosts::OwnedEffectsHost, runtime::CancelToken};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

fn success() -> Reply {
    Ok(IpcValue::Object(vec![("ok".into(), IpcValue::Bool(true))]))
}

#[test]
fn owned_reply_can_reenter_the_same_owner_gates_after_terminal_state_commits() {
    let probe = Probe::new(json!({}), vec![sender(1, Surface::Hwp)]);
    let state = probe.services.hosts.get::<MStateRef>().unwrap().clone();
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let renderer = probe.services.views.renderer_generation(ViewId(1)).unwrap();
    let owner = effects.capture_owner(ViewId(1), &renderer).unwrap();
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let replies: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let completions: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let reentered = Arc::new(AtomicBool::new(false));
    let (join_tx, join_rx) = mpsc::channel::<std::thread::JoinHandle<()>>();
    let (reply_state, reply_effects, reply_owner, reply_cancel, reply_pending) = (
        state.clone(),
        effects.clone(),
        owner.clone(),
        cancel.clone(),
        pending.clone(),
    );
    let replied = replies.clone();
    let entered = reentered.clone();
    let reply = Responder::new(move |answer| {
        assert!(
            reply_state.begin_print(true, ViewId(1)),
            "the old busy marker must already be terminal"
        );
        reply_state.end_print(true, ViewId(1));
        let (notify, observed) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = reply_effects.with_owned_effect(
                &reply_owner,
                &reply_owner,
                &reply_cancel,
                &reply_pending,
                &mut || Ok(()),
            );
            let _ = notify.send(result);
        });
        // A regression which replies under the gate times out here, then returns
        // so the worker can unwind cleanly. It cannot hang the test process.
        entered.store(
            observed
                .recv_timeout(Duration::from_secs(5))
                .is_ok_and(|result| result.is_ok()),
            Ordering::Release,
        );
        join_tx.send(worker).unwrap();
        replied.lock().unwrap().push(answer);
    });
    let completed = completions.clone();
    let grant = OwnedHwpCompletion {
        authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(owner.clone()),
        destination: owner,
        cancel,
        pending,
        effects,
        additional: Vec::new(),
        accepted_losses: Arc::default(),
        complete: Box::new(move |result| completed.lock().unwrap().push(result)),
    };
    assert!(state.begin_print(true, ViewId(1)));
    finish_owned_hwp_print(&state, ViewId(1), grant, reply, success(), None);
    join_rx.recv().unwrap().join().unwrap();
    assert!(
        reentered.load(Ordering::Acquire),
        "Responder must run after all ownership gates release"
    );
    let replies = replies.lock().unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        field(replies[0].as_ref().unwrap(), "ok"),
        &IpcValue::Bool(true)
    );
    assert_eq!(&*completions.lock().unwrap(), &[Ok(())]);
}

#[test]
fn rejected_owned_completion_releases_only_busy_state_and_replies_once() {
    let probe = Probe::new(json!({}), vec![sender(1, Surface::Hwp)]);
    let state = probe.services.hosts.get::<MStateRef>().unwrap().clone();
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let renderer = probe.services.views.renderer_generation(ViewId(1)).unwrap();
    let owner = effects.capture_owner(ViewId(1), &renderer).unwrap();
    let replies: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let completions: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let reply_state = state.clone();
    let replied = replies.clone();
    let reply = Responder::new(move |answer| {
        assert!(
            reply_state.begin_print(true, ViewId(1)),
            "revocation must not leave the old busy marker stuck"
        );
        reply_state.end_print(true, ViewId(1));
        replied.lock().unwrap().push(answer);
    });
    let completed = completions.clone();
    let grant = OwnedHwpCompletion {
        authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(owner.clone()),
        destination: owner,
        cancel: CancelToken::default(),
        pending: CancelToken::default(),
        effects,
        additional: Vec::new(),
        accepted_losses: Arc::default(),
        complete: Box::new(move |result| completed.lock().unwrap().push(result)),
    };
    assert!(state.begin_print(true, ViewId(1)));
    renderer.revoke();
    finish_owned_hwp_print(&state, ViewId(1), grant, reply, success(), None);
    let replies = replies.lock().unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        field(replies[0].as_ref().unwrap(), "ok"),
        &IpcValue::Bool(false)
    );
    let completions = completions.lock().unwrap();
    assert_eq!(completions.len(), 1);
    assert!(completions[0].is_err());
}

#[test]
fn loaded_target_receipt_commits_terminal_state_before_callback() {
    let probe = Probe::new(
        json!({}),
        vec![sender(1, Surface::Hwp), sender(2, Surface::Pdf)],
    );
    let state = probe.services.hosts.get::<MStateRef>().unwrap().clone();
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let capture = |view| {
        effects
            .capture_owner(
                view,
                &probe.services.views.renderer_generation(view).unwrap(),
            )
            .unwrap()
    };
    let source = capture(ViewId(1));
    let target = capture(ViewId(2));
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let receipt = shell_contract::hosts::CommittedGeneratedPath::from_loaded_target(
        source.clone(),
        target,
        source.clone(),
        cancel.clone(),
        pending.clone(),
        effects.clone(),
    )
    .unwrap();
    let replies: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let completions: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let replied = replies.clone();
    let completed = completions.clone();
    let callback_state = state.clone();
    let reply = Responder::new(move |answer| {
        assert!(callback_state.begin_print(true, ViewId(1)));
        callback_state.end_print(true, ViewId(1));
        replied.lock().unwrap().push(answer);
    });
    let grant = OwnedHwpCompletion {
        authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(source.clone()),
        destination: source,
        cancel,
        pending,
        effects,
        additional: Vec::new(),
        accepted_losses: Arc::default(),
        complete: Box::new(move |result| completed.lock().unwrap().push(result)),
    };
    assert!(state.begin_print(true, ViewId(1)));
    finish_owned_hwp_print(
        &state,
        ViewId(1),
        grant,
        reply,
        success(),
        Some(HwpPublicationTerminal::Opened(&receipt)),
    );
    assert_eq!(replies.lock().unwrap().len(), 1);
    assert_eq!(&*completions.lock().unwrap(), &[Ok(())]);
}

#[test]
fn target_revocation_after_receipt_never_reports_success() {
    let probe = Probe::new(
        json!({}),
        vec![sender(1, Surface::Hwp), sender(2, Surface::Pdf)],
    );
    let state = probe.services.hosts.get::<MStateRef>().unwrap().clone();
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let capture = |view| {
        effects
            .capture_owner(
                view,
                &probe.services.views.renderer_generation(view).unwrap(),
            )
            .unwrap()
    };
    let source = capture(ViewId(1));
    let target = capture(ViewId(2));
    let target_epoch = target.context.renderer().unwrap().clone();
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let receipt = shell_contract::hosts::CommittedGeneratedPath::from_loaded_target(
        source.clone(),
        target,
        source.clone(),
        cancel.clone(),
        pending.clone(),
        effects.clone(),
    )
    .unwrap();
    let replies: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let completions: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let replied = replies.clone();
    let completed = completions.clone();
    let reply = Responder::new(move |answer| replied.lock().unwrap().push(answer));
    let grant = OwnedHwpCompletion {
        authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(source.clone()),
        destination: source,
        cancel,
        pending,
        effects,
        additional: Vec::new(),
        accepted_losses: Arc::default(),
        complete: Box::new(move |result| completed.lock().unwrap().push(result)),
    };
    assert!(state.begin_print(true, ViewId(1)));
    target_epoch.revoke();
    finish_owned_hwp_print(
        &state,
        ViewId(1),
        grant,
        reply,
        success(),
        Some(HwpPublicationTerminal::Opened(&receipt)),
    );
    assert_eq!(
        field(replies.lock().unwrap()[0].as_ref().unwrap(), "ok"),
        &IpcValue::Bool(false)
    );
    assert!(completions.lock().unwrap()[0].is_err());
    assert!(state.begin_print(true, ViewId(1)));
    state.end_print(true, ViewId(1));
}

#[test]
fn native_pdf_grant_keeps_callback_cancellation_and_document_lease() {
    // Synthetic ownership only; this does not authenticate a CEF renderer.
    for revoke_document in [false, true] {
        let probe = Probe::new(json!({}), vec![sender(1, Surface::Hwp)]);
        let effects = probe
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .unwrap()
            .clone();
        let renderer = probe.services.views.renderer_generation(ViewId(1)).unwrap();
        let mut owner = effects.capture_owner(ViewId(1), &renderer).unwrap();
        let document = shell_contract::runtime::RendererGeneration::new();
        owner.context = owner.context.with_document_lease(document.clone());
        let operation_cancel = CancelToken::default();
        let binding = shell_contract::ipc::NativeInvocationIssuer::new(CancelToken::default())
            .issue_existing_document_save(
                Surface::Hwp,
                ViewId(1),
                shell_contract::ipc::NativeSaveSelection::ExactPath("output.pdf".into()),
                owner.clone(),
                operation_cancel.clone(),
            )
            .unwrap();
        let selection =
            shell_contract::ipc::NativeSaveDestination::from_exact_selection(binding).unwrap();
        let authority = shell_contract::editors::OwnedPdfGrantAuthority::ExistingDocument {
            selection,
            participants: Vec::new(),
        };
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let callback = CancelToken::default();
        let mut calls = 0;
        pdf_grant_effect(
            &authority,
            &owner,
            &effects,
            &cancel,
            &pending,
            std::slice::from_ref(&callback),
            &mut || {
                calls += 1;
                Ok(())
            },
        )
        .unwrap();
        if revoke_document {
            document.revoke();
        } else {
            callback.cancel();
        }
        assert!(
            pdf_grant_effect(
                &authority,
                &owner,
                &effects,
                &cancel,
                &pending,
                std::slice::from_ref(&callback),
                &mut || {
                    calls += 1;
                    Ok(())
                }
            )
            .is_err()
        );
        assert_eq!(calls, 1);
    }
}

#[test]
fn renderer_pdf_grant_rejects_a_cancelled_callback_owner() {
    let probe = Probe::new(json!({}), vec![sender(1, Surface::Hwp)]);
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let renderer = probe.services.views.renderer_generation(ViewId(1)).unwrap();
    let owner = effects.capture_owner(ViewId(1), &renderer).unwrap();
    let authority = shell_contract::editors::OwnedPdfGrantAuthority::Renderer(owner.clone());
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let callback = CancelToken::default();
    let mut calls = 0;
    pdf_grant_effect(
        &authority,
        &owner,
        &effects,
        &cancel,
        &pending,
        std::slice::from_ref(&callback),
        &mut || {
            calls += 1;
            Ok(())
        },
    )
    .unwrap();
    callback.cancel();
    assert!(
        pdf_grant_effect(
            &authority,
            &owner,
            &effects,
            &cancel,
            &pending,
            std::slice::from_ref(&callback),
            &mut || {
                calls += 1;
                Ok(())
            }
        )
        .is_err()
    );
    assert_eq!(calls, 1);
}

struct InlineCore;
impl shell_contract::runtime::Thread for InlineCore {
    fn post(&self, job: Box<dyn FnOnce() + Send>) {
        job();
    }
    fn is_current(&self) -> bool {
        true
    }
}

struct DroppedCore;
impl shell_contract::runtime::Thread for DroppedCore {
    fn post(&self, _job: Box<dyn FnOnce() + Send>) {}
    fn is_current(&self) -> bool {
        false
    }
}

struct PendingReceipt {
    reject: AtomicBool,
    observer: Mutex<Option<shell_contract::hosts::GeneratedPathObserver>>,
    wake: Mutex<Option<shell_contract::hosts::GeneratedPathWake>>,
    restored: Arc<Mutex<Option<Result<shell_contract::hosts::GeneratedPathOutcome, String>>>>,
    cancelling: AtomicBool,
}
impl shell_contract::hosts::GeneratedPathPending for PendingReceipt {
    fn observe(
        &self,
        observer: shell_contract::hosts::GeneratedPathObserver,
        wake: shell_contract::hosts::GeneratedPathWake,
    ) -> Result<(), shell_contract::hosts::GeneratedObserveFailure> {
        if self.reject.load(Ordering::Acquire) {
            Err(shell_contract::hosts::GeneratedObserveFailure {
                error: "observer rejected".into(),
                observer,
                wake,
            })
        } else {
            *self.observer.lock().unwrap() = Some(observer);
            *self.wake.lock().unwrap() = Some(wake);
            Ok(())
        }
    }
    fn wake_observer(&self) -> Result<(), String> {
        let wake = self
            .wake
            .lock()
            .unwrap()
            .clone()
            .ok_or("observer unavailable")?;
        wake();
        Ok(())
    }
    fn take_redelivery(
        &self,
    ) -> Result<Option<shell_contract::hosts::GeneratedTerminalDelivery>, String> {
        let result = self.restored.lock().unwrap().take();
        Ok(result.map(|result| {
            shell_contract::hosts::GeneratedTerminalDelivery::new(
                result,
                Arc::new(PendingAck {
                    calls: std::sync::atomic::AtomicUsize::new(0),
                    fail_once: AtomicBool::new(false),
                    restored: self.restored.clone(),
                }),
            )
        }))
    }
    fn is_cancelling(&self) -> bool {
        self.cancelling.load(Ordering::Acquire)
    }
    fn retry(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        done(Err("not requested".into()));
    }
    fn cancel_and_drain(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        self.cancelling.store(true, Ordering::Release);
        let wake = self.wake.lock().unwrap().clone();
        if let Some(wake) = wake {
            wake();
        }
        done(Err("the synthetic task has not settled".into()));
    }
}

struct FixturePrepared {
    task: Arc<dyn shell_contract::hosts::GeneratedPathPending>,
    started: Arc<AtomicBool>,
}
impl shell_contract::hosts::PreparedGeneratedPath for FixturePrepared {
    fn task(&self) -> Arc<dyn shell_contract::hosts::GeneratedPathPending> {
        self.task.clone()
    }
    fn start(self: Box<Self>) -> Result<(), shell_contract::hosts::GeneratedStartFailure> {
        self.started.store(true, Ordering::Release);
        Ok(())
    }
}

struct RetryPrepared {
    task: Arc<dyn shell_contract::hosts::GeneratedPathPending>,
    attempts: Arc<std::sync::atomic::AtomicUsize>,
}
impl shell_contract::hosts::PreparedGeneratedPath for RetryPrepared {
    fn task(&self) -> Arc<dyn shell_contract::hosts::GeneratedPathPending> {
        self.task.clone()
    }
    fn start(self: Box<Self>) -> Result<(), shell_contract::hosts::GeneratedStartFailure> {
        if self.attempts.fetch_add(1, Ordering::AcqRel) == 0 {
            Err(shell_contract::hosts::GeneratedStartFailure {
                error: "temporary start failure".into(),
                prepared: self,
            })
        } else {
            Ok(())
        }
    }
}

enum FixtureOutcome {
    Pending(Arc<dyn shell_contract::hosts::GeneratedPathPending>),
    Terminal(shell_contract::hosts::GeneratedTerminalDelivery),
}

fn handle_hwp_publication(
    state: &MStateRef,
    view: ViewId,
    grant: OwnedHwpCompletion,
    reply: Responder,
    answer: Reply,
    outcome: Result<FixtureOutcome, String>,
    core: Arc<dyn shell_contract::runtime::Thread>,
) {
    let (_, done) = prepare_hwp_publication(state, view, grant, reply, answer, core);
    match outcome {
        Ok(FixtureOutcome::Pending(task)) => done(Ok(Box::new(FixturePrepared {
            task,
            started: Arc::new(AtomicBool::new(false)),
        }))),
        Ok(FixtureOutcome::Terminal(delivery)) => {
            let task = Arc::new(PendingReceipt {
                reject: AtomicBool::new(false),
                observer: Mutex::new(None),
                wake: Mutex::new(None),
                restored: Arc::default(),
                cancelling: AtomicBool::new(false),
            });
            done(Ok(Box::new(FixturePrepared {
                task: task.clone(),
                started: Arc::new(AtomicBool::new(false)),
            })));
            let observer = task.observer.lock().unwrap().take().unwrap();
            observer(delivery);
        }
        Err(error) => done(Err(error)),
    }
}

struct PendingAck {
    calls: std::sync::atomic::AtomicUsize,
    fail_once: AtomicBool,
    restored: Arc<Mutex<Option<Result<shell_contract::hosts::GeneratedPathOutcome, String>>>>,
}
impl shell_contract::hosts::GeneratedTerminalAck for PendingAck {
    fn complete(&self) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if self.fail_once.swap(false, Ordering::AcqRel) {
            Err("ack temporarily failed".into())
        } else {
            Ok(())
        }
    }
    fn restore(&self, result: Result<shell_contract::hosts::GeneratedPathOutcome, String>) {
        *self.restored.lock().unwrap() = Some(result);
    }
}

#[test]
fn cleanup_pending_retains_completion_until_actual_terminal_callback() {
    let probe = Probe::new(json!({}), vec![sender(1, Surface::Hwp)]);
    let state = probe.services.hosts.get::<MStateRef>().unwrap().clone();
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let renderer = probe.services.views.renderer_generation(ViewId(1)).unwrap();
    let owner = effects.capture_owner(ViewId(1), &renderer).unwrap();
    let replies: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let completions: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let replied = replies.clone();
    let completed = completions.clone();
    let grant = OwnedHwpCompletion {
        authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(owner.clone()),
        destination: owner,
        cancel: CancelToken::default(),
        pending: CancelToken::default(),
        additional: Vec::new(),
        effects,
        accepted_losses: Arc::default(),
        complete: Box::new(move |result| completed.lock().unwrap().push(result)),
    };
    let receipt = Arc::new(PendingReceipt {
        reject: AtomicBool::new(false),
        observer: Mutex::new(None),
        wake: Mutex::new(None),
        restored: Arc::default(),
        cancelling: AtomicBool::new(false),
    });
    let erased: Arc<dyn shell_contract::hosts::GeneratedPathPending> = receipt.clone();
    assert!(state.begin_print(true, ViewId(1)));
    handle_hwp_publication(
        &state,
        ViewId(1),
        grant,
        Responder::new(move |answer| replied.lock().unwrap().push(answer)),
        success(),
        Ok(FixtureOutcome::Pending(erased.clone())),
        Arc::new(DroppedCore),
    );
    assert!(replies.lock().unwrap().is_empty());
    assert!(completions.lock().unwrap().is_empty());
    assert!(!state.begin_print(true, ViewId(1)));
    let ack = Arc::new(PendingAck {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_once: AtomicBool::new(true),
        restored: receipt.restored.clone(),
    });
    let observer = receipt.observer.lock().unwrap().take().unwrap();
    observer(shell_contract::hosts::GeneratedTerminalDelivery::new(
        Err("cancelled after cleanup".into()),
        ack.clone(),
    ));
    assert!(replies.lock().unwrap().is_empty());
    assert!(completions.lock().unwrap().is_empty());
    assert_eq!(ack.calls.load(Ordering::Acquire), 0);
    state.wake_hwp_publication(ViewId(1), &erased, Arc::new(InlineCore));
    assert_eq!(replies.lock().unwrap().len(), 1);
    assert!(completions.lock().unwrap()[0].is_err());
    assert_eq!(ack.calls.load(Ordering::Acquire), 1);
    state.wake_hwp_publication(ViewId(1), &erased, Arc::new(InlineCore));
    assert_eq!(ack.calls.load(Ordering::Acquire), 2);
    assert_eq!(replies.lock().unwrap().len(), 1);
    assert_eq!(completions.lock().unwrap().len(), 1);
    assert!(state.begin_print(true, ViewId(1)));
    state.end_print(true, ViewId(1));
}

#[test]
fn failed_observer_registration_keeps_receipt_and_original_completion() {
    let probe = Probe::new(json!({}), vec![sender(1, Surface::Hwp)]);
    let state = probe.services.hosts.get::<MStateRef>().unwrap().clone();
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let renderer = probe.services.views.renderer_generation(ViewId(1)).unwrap();
    let owner = effects.capture_owner(ViewId(1), &renderer).unwrap();
    let replies: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let completions: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let completed = completions.clone();
    let replied = replies.clone();
    let grant = OwnedHwpCompletion {
        authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(owner.clone()),
        destination: owner,
        cancel: CancelToken::default(),
        pending: CancelToken::default(),
        additional: Vec::new(),
        effects,
        accepted_losses: Arc::default(),
        complete: Box::new(move |result| completed.lock().unwrap().push(result)),
    };
    let concrete = Arc::new(PendingReceipt {
        reject: AtomicBool::new(true),
        observer: Mutex::new(None),
        wake: Mutex::new(None),
        restored: Arc::default(),
        cancelling: AtomicBool::new(false),
    });
    let receipt: Arc<dyn shell_contract::hosts::GeneratedPathPending> = concrete.clone();
    assert!(state.begin_print(true, ViewId(1)));
    handle_hwp_publication(
        &state,
        ViewId(1),
        grant,
        Responder::new(move |answer| replied.lock().unwrap().push(answer)),
        success(),
        Ok(FixtureOutcome::Pending(receipt.clone())),
        Arc::new(InlineCore),
    );
    assert!(replies.lock().unwrap().is_empty());
    assert!(completions.lock().unwrap().is_empty());
    assert!(!state.begin_print(true, ViewId(1)));
    concrete.reject.store(false, Ordering::Release);
    state.wake_hwp_publications(Arc::new(InlineCore));
    let ack = Arc::new(PendingAck {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_once: AtomicBool::new(false),
        restored: concrete.restored.clone(),
    });
    let observer = concrete.observer.lock().unwrap().take().unwrap();
    observer(shell_contract::hosts::GeneratedTerminalDelivery::new(
        Err("cancelled after cleanup".into()),
        ack.clone(),
    ));
    assert_eq!(replies.lock().unwrap().len(), 1);
    assert!(completions.lock().unwrap()[0].is_err());
    assert_eq!(ack.calls.load(Ordering::Acquire), 1);
}

#[test]
fn immediate_terminal_delivery_survives_a_dropped_core_wake_and_ack_retry() {
    let probe = Probe::new(json!({}), vec![sender(1, Surface::Hwp)]);
    let state = probe.services.hosts.get::<MStateRef>().unwrap().clone();
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let renderer = probe.services.views.renderer_generation(ViewId(1)).unwrap();
    let owner = effects.capture_owner(ViewId(1), &renderer).unwrap();
    let replies: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let completions: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let replied = replies.clone();
    let completed = completions.clone();
    let grant = OwnedHwpCompletion {
        authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(owner.clone()),
        destination: owner,
        cancel: CancelToken::default(),
        pending: CancelToken::default(),
        additional: Vec::new(),
        effects,
        accepted_losses: Arc::default(),
        complete: Box::new(move |result| completed.lock().unwrap().push(result)),
    };
    let ack = Arc::new(PendingAck {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_once: AtomicBool::new(true),
        restored: Arc::default(),
    });
    assert!(state.begin_print(true, ViewId(1)));
    handle_hwp_publication(
        &state,
        ViewId(1),
        grant,
        Responder::new(move |answer| replied.lock().unwrap().push(answer)),
        success(),
        Ok(FixtureOutcome::Terminal(
            shell_contract::hosts::GeneratedTerminalDelivery::new(
                Err("open failed".into()),
                ack.clone(),
            ),
        )),
        Arc::new(DroppedCore),
    );
    assert!(replies.lock().unwrap().is_empty());
    assert_eq!(ack.calls.load(Ordering::Acquire), 0);
    state.wake_hwp_publications(Arc::new(InlineCore));
    assert_eq!(replies.lock().unwrap().len(), 1);
    assert_eq!(completions.lock().unwrap().len(), 1);
    assert_eq!(ack.calls.load(Ordering::Acquire), 1);
    state.wake_hwp_publications(Arc::new(InlineCore));
    assert_eq!(replies.lock().unwrap().len(), 1);
    assert_eq!(completions.lock().unwrap().len(), 1);
    assert_eq!(ack.calls.load(Ordering::Acquire), 2);
}

#[test]
fn early_supervisor_wake_keeps_the_original_responder_until_dispatch_completes() {
    let probe = Probe::new(json!({}), vec![sender(1, Surface::Hwp)]);
    let state = probe.services.hosts.get::<MStateRef>().unwrap().clone();
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let renderer = probe.services.views.renderer_generation(ViewId(1)).unwrap();
    let owner = effects.capture_owner(ViewId(1), &renderer).unwrap();
    let replies: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let completions: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let replied = replies.clone();
    let completed = completions.clone();
    let grant = OwnedHwpCompletion {
        authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(owner.clone()),
        destination: owner,
        cancel: CancelToken::default(),
        pending: CancelToken::default(),
        additional: Vec::new(),
        effects,
        accepted_losses: Arc::default(),
        complete: Box::new(move |result| completed.lock().unwrap().push(result)),
    };
    assert!(state.begin_print(true, ViewId(1)));
    let (wake, done) = prepare_hwp_publication(
        &state,
        ViewId(1),
        grant,
        Responder::new(move |answer| replied.lock().unwrap().push(answer)),
        success(),
        Arc::new(InlineCore),
    );
    wake();
    assert!(replies.lock().unwrap().is_empty());
    let ack = Arc::new(PendingAck {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_once: AtomicBool::new(false),
        restored: Arc::default(),
    });
    let task = Arc::new(PendingReceipt {
        reject: AtomicBool::new(false),
        observer: Mutex::new(None),
        wake: Mutex::new(None),
        restored: Arc::default(),
        cancelling: AtomicBool::new(false),
    });
    done(Ok(Box::new(FixturePrepared {
        task: task.clone(),
        started: Arc::new(AtomicBool::new(false)),
    })));
    let observer = task.observer.lock().unwrap().take().unwrap();
    observer(shell_contract::hosts::GeneratedTerminalDelivery::new(
        Err("cancelled".into()),
        ack.clone(),
    ));
    assert_eq!(replies.lock().unwrap().len(), 1);
    assert_eq!(completions.lock().unwrap().len(), 1);
    assert_eq!(ack.calls.load(Ordering::Acquire), 1);
}

#[test]
fn restored_terminal_lease_is_recovered_from_the_original_pending_task() {
    let probe = Probe::new(json!({}), vec![sender(1, Surface::Hwp)]);
    let state = probe.services.hosts.get::<MStateRef>().unwrap().clone();
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let renderer = probe.services.views.renderer_generation(ViewId(1)).unwrap();
    let owner = effects.capture_owner(ViewId(1), &renderer).unwrap();
    let replies: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let completions: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let replied = replies.clone();
    let completed = completions.clone();
    let grant = OwnedHwpCompletion {
        authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(owner.clone()),
        destination: owner,
        cancel: CancelToken::default(),
        pending: CancelToken::default(),
        additional: Vec::new(),
        effects,
        accepted_losses: Arc::default(),
        complete: Box::new(move |result| completed.lock().unwrap().push(result)),
    };
    let task = Arc::new(PendingReceipt {
        reject: AtomicBool::new(false),
        observer: Mutex::new(None),
        wake: Mutex::new(None),
        restored: Arc::default(),
        cancelling: AtomicBool::new(false),
    });
    let erased: Arc<dyn shell_contract::hosts::GeneratedPathPending> = task.clone();
    assert!(state.begin_print(true, ViewId(1)));
    handle_hwp_publication(
        &state,
        ViewId(1),
        grant,
        Responder::new(move |answer| replied.lock().unwrap().push(answer)),
        success(),
        Ok(FixtureOutcome::Pending(erased.clone())),
        Arc::new(InlineCore),
    );
    let ack = Arc::new(PendingAck {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_once: AtomicBool::new(false),
        restored: task.restored.clone(),
    });
    drop(shell_contract::hosts::GeneratedTerminalDelivery::new(
        Err("cancelled".into()),
        ack,
    ));
    state.wake_hwp_publication(ViewId(1), &erased, Arc::new(InlineCore));
    assert_eq!(replies.lock().unwrap().len(), 1);
    assert_eq!(completions.lock().unwrap().len(), 1);
    assert!(completions.lock().unwrap()[0].is_err());
    assert!(task.restored.lock().unwrap().is_none());
}

#[test]
fn failed_prepared_start_retains_same_task_until_wake_retries_it() {
    let probe = Probe::new(json!({}), vec![sender(1, Surface::Hwp)]);
    let state = probe.services.hosts.get::<MStateRef>().unwrap().clone();
    let effects = probe
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .clone();
    let renderer = probe.services.views.renderer_generation(ViewId(1)).unwrap();
    let owner = effects.capture_owner(ViewId(1), &renderer).unwrap();
    let replies: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let replied = replies.clone();
    let completions: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let completed = completions.clone();
    let grant = OwnedHwpCompletion {
        authority: shell_contract::editors::OwnedPdfGrantAuthority::Renderer(owner.clone()),
        destination: owner,
        cancel: CancelToken::default(),
        pending: CancelToken::default(),
        additional: Vec::new(),
        effects,
        accepted_losses: Arc::default(),
        complete: Box::new(move |result| completed.lock().unwrap().push(result)),
    };
    let task = Arc::new(PendingReceipt {
        reject: AtomicBool::new(false),
        observer: Mutex::new(None),
        wake: Mutex::new(None),
        restored: Arc::default(),
        cancelling: AtomicBool::new(false),
    });
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    assert!(state.begin_print(true, ViewId(1)));
    let (wake, done) = prepare_hwp_publication(
        &state,
        ViewId(1),
        grant,
        Responder::new(move |answer| replied.lock().unwrap().push(answer)),
        success(),
        Arc::new(InlineCore),
    );
    done(Ok(Box::new(RetryPrepared {
        task: task.clone(),
        attempts: attempts.clone(),
    })));
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    assert!(replies.lock().unwrap().is_empty());
    wake();
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    let ack = Arc::new(PendingAck {
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_once: AtomicBool::new(false),
        restored: task.restored.clone(),
    });
    let observer = task.observer.lock().unwrap().take().unwrap();
    observer(shell_contract::hosts::GeneratedTerminalDelivery::new(
        Err("cancelled".into()),
        ack.clone(),
    ));
    assert_eq!(replies.lock().unwrap().len(), 1);
    assert_eq!(completions.lock().unwrap().len(), 1);
    assert_eq!(ack.calls.load(Ordering::Acquire), 1);
}
