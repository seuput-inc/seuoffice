use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::*;
use crate::ipc::{Sender, Target};
use crate::{Surface, WindowId};

fn closure_reservation(view: ViewId, created: &RendererGeneration) -> (crate::ipc::NativeClosureReservation, crate::ipc::NativeBrowserOwner) {
    let (_, reservation, owner) = crate::ipc::NativeClosureIssuer::new_with_owner(view, created.clone());
    (reservation, owner)
}

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queue(Mutex<VecDeque<Job>>);
impl Thread for Queue {
    fn post(&self, job: Job) { self.0.lock().unwrap().push_back(job); }
    fn is_current(&self) -> bool { false }
}
impl Queue {
    fn drain(&self) {
        loop {
            let next = self.0.lock().unwrap().pop_front();
            match next { Some(job) => job(), None => break }
        }
    }
}

#[derive(Default)]
struct TestViews {
    current: Mutex<HashMap<ViewId, (Sender, RendererGeneration)>>,
    destroyed: Mutex<Vec<Box<dyn Fn(ViewId) + Send + Sync>>>,
    replaced: Mutex<Vec<Box<dyn Fn(ViewId, RendererGeneration) + Send + Sync>>>,
}
impl TestViews {
    fn add(&self, view: ViewId, generation: RendererGeneration) {
        self.current.lock().unwrap().insert(view, (Sender { view, window: WindowId(view.0), surface: Surface::Shell }, generation));
    }
    fn replace(&self, view: ViewId) {
        let old = {
            let mut current = self.current.lock().unwrap();
            let (_, generation) = current.get_mut(&view).unwrap();
            std::mem::replace(generation, RendererGeneration::new())
        };
        old.revoke();
        for listener in self.replaced.lock().unwrap().iter() { listener(view, old.clone()); }
    }
}
impl Views for TestViews {
    fn sender_of(&self, view: ViewId) -> Option<Sender> { self.current.lock().unwrap().get(&view).map(|(sender, _)| *sender) }
    fn all(&self) -> Vec<Sender> { self.current.lock().unwrap().values().map(|(sender, _)| *sender).collect() }
    fn renderer_generation(&self, view: ViewId) -> Option<RendererGeneration> { self.current.lock().unwrap().get(&view).map(|(_, generation)| generation.clone()) }
    fn on_destroyed(&self, listener: Box<dyn Fn(ViewId) + Send + Sync>) { self.destroyed.lock().unwrap().push(listener); }
    fn on_renderer_replaced(&self, listener: Box<dyn Fn(ViewId, RendererGeneration) + Send + Sync>) { self.replaced.lock().unwrap().push(listener); }
}

#[derive(Default)]
struct Events {
    sent: Mutex<Vec<(Target, &'static str, Vec<IpcValue>)>>,
    failure: Mutex<Option<String>>,
    failed: Mutex<Vec<String>>,
}
impl EventSink for Events {
    fn emit(&self, target: Target, channel: &'static str, args: Vec<IpcValue>) { self.sent.lock().unwrap().push((target, channel, args)); }
    fn emit_to_owner_checked(&self, owner: &NativeRequestOwner, channel: &'static str, args: Vec<IpcValue>) -> Result<(), String> {
        if let Some(error) = self.failure.lock().unwrap().take() {
            self.failed.lock().unwrap().push(error.clone());
            return Err(error);
        }
        self.emit(Target::View(owner.view), channel, args);
        Ok(())
    }
}

fn model() -> (SyntheticOwnedEffects, Arc<Queue>, Arc<TestViews>, Arc<Events>, NativeRequestOwner) {
    let ui = Arc::new(Queue::default());
    let views = Arc::new(TestViews::default());
    let events = Arc::new(Events::default());
    let generation = RendererGeneration::new();
    views.add(ViewId(1), generation.clone());
    let effects = SyntheticOwnedEffects::new(ui.clone(), views.clone(), events.clone());
    let owner = effects.capture_owner(ViewId(1), &generation).unwrap();
    (effects, ui, views, events, owner)
}

pub(crate) fn editor_effects_fixture() -> crate::hosts::OwnedEffectsHost {
    Arc::new(model().0)
}

#[test]
fn capture_requires_the_exact_current_generation_and_never_grants_native_authority() {
    let (effects, _, views, _, owner) = model();
    assert!(owner.context.authority_ref::<()>().is_none());
    assert!(effects.capture_owner(ViewId(1), &RendererGeneration::new()).is_err());
    views.replace(ViewId(1));
    assert!(effects.capture_owner(ViewId(1), owner.context.renderer().unwrap()).is_err());
    let cancel = CancelToken::default();
    let mut called = false;
    assert!(effects.with_owned_effect(&owner, &owner, &cancel, &cancel, &mut || { called = true; Ok(()) }).is_err());
    assert!(!called);
}

#[test]
fn exact_browser_admission_rejects_replacement_and_foreign_context() {
    let (effects, _, _, _, owner) = model();
    let created = owner.context.renderer().unwrap().clone();
    let (_, reservation, first) = crate::ipc::NativeClosureIssuer::new_with_owner(owner.view, created);
    reservation.with_initial_owner(&first, |transition| effects.bind_browser_fixture(&owner, &first, transition)).unwrap();
    let cancel = CancelToken::default();
    let mut calls = 0;
    effects.with_exact_browser_owner_effect(&first, &owner, &cancel, &mut || { calls += 1; Ok(()) }).unwrap();
    let foreign = effects.capture_owner(owner.view, owner.context.renderer().unwrap()).unwrap();
    assert!(effects.with_exact_browser_owner_effect(&first, &foreign, &cancel, &mut || { calls += 1; Ok(()) }).is_err());
    let (next, _, ()) = reservation.prepare_replacement(&first).unwrap().commit(|transition, next| {
        effects.bind_browser_fixture(&owner, next, transition)
    }).unwrap();
    assert!(effects.with_exact_browser_owner_effect(&first, &owner, &cancel, &mut || { calls += 1; Ok(()) }).is_err());
    effects.with_exact_browser_owner_effect(&next, &owner, &cancel, &mut || { calls += 1; Ok(()) }).unwrap();
    cancel.cancel();
    assert!(effects.with_exact_browser_owner_effect(&next, &owner, &cancel, &mut || { calls += 1; Ok(()) }).is_err());
    assert_eq!(calls, 2);
}

#[test]
fn aliased_generations_document_leases_pdf_leases_and_cancel_tokens_lock_once() {
    let (effects, _, _, _, mut owner) = model();
    let renderer = owner.context.renderer().unwrap().clone();
    let pdf = PdfDocumentLease::new();
    owner.context = owner.context.with_document_lease(renderer.clone()).with_pdf_document(pdf.clone()).with_pdf_revision(pdf);
    let cancel = owner.loss_token().unwrap();
    let mut count = 0;
    effects.with_owned_effect(&owner, &owner, &cancel, &cancel, &mut || { count += 1; Ok(()) }).unwrap();
    effects.with_created_effect(&owner, &renderer, &cancel, &cancel, &mut || { count += 1; Ok(()) }).unwrap();
    assert_eq!(count, 2);
    assert_eq!(effects.with_owned_effect(&owner, &owner, &cancel, &cancel, &mut || Err("actual effect failed".into())), Err("actual effect failed".into()));
}

#[test]
fn canceled_pending_scope_and_replaced_pdf_revision_never_run_effects() {
    let (effects, _, _, _, mut owner) = model();
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    pending.cancel();
    let mut count = 0;
    assert!(effects.with_owned_effect(&owner, &owner, &cancel, &pending, &mut || { count += 1; Ok(()) }).is_err());
    let revision = PdfDocumentLease::new();
    owner.context = owner.context.with_pdf_revision(revision.clone());
    revision.revoke_deferred().cancel();
    assert!(effects.with_created_effect(&owner, &RendererGeneration::new(), &cancel, &cancel, &mut || { count += 1; Ok(()) }).is_err());
    assert_eq!(count, 0);
}

#[test]
fn docs_identity_is_captured_and_document_loss_rejects_later_effects() {
    let (effects, _, _, _, owner) = model();
    let lease = RendererGeneration::new();
    let docs = effects.capture_docs_owner(owner.view, owner.context.renderer().unwrap(), 41, lease.clone()).unwrap();
    let other = effects.capture_docs_owner(owner.view, owner.context.renderer().unwrap(), 42, RendererGeneration::new()).unwrap();
    assert_eq!(effects.docs_document_identity(&docs), Some((ViewId(1), 41)));
    assert_eq!(effects.docs_document_identity(&other), Some((ViewId(1), 42)));
    lease.revoke();
    let cancel = CancelToken::default();
    assert!(effects.with_owned_effect(&owner, &docs, &cancel, &cancel, &mut || Ok(())).is_err());
    assert_eq!(effects.docs_document_identity(&docs), Some((ViewId(1), 41)));
}

#[test]
fn emit_delivers_real_arguments_and_preserves_sink_failure() {
    let (effects, _, _, events, owner) = model();
    let cancel = CancelToken::default();
    effects.emit_owned(&owner, &owner, &cancel, &cancel, "generate:state", vec![IpcValue::Undefined, IpcValue::Bytes(crate::BytesKind::ArrayBuffer, vec![])]).unwrap();
    assert_eq!(events.sent.lock().unwrap().as_slice(), &[(Target::View(owner.view), "generate:state", vec![IpcValue::Undefined, IpcValue::Bytes(crate::BytesKind::ArrayBuffer, vec![])])]);
    *events.failure.lock().unwrap() = Some("delivery failed".into());
    effects.emit_owned(&owner, &owner, &cancel, &cancel, "generate:state", vec![]).unwrap();
    assert_eq!(events.failed.lock().unwrap().as_slice(), &["delivery failed"]);
    assert!(effects.emit_owned(&owner, &owner, &cancel, &cancel, "not-a-contract-event", vec![]).is_err());
    assert_eq!(events.sent.lock().unwrap().len(), 1);
}

#[test]
fn synthetic_stream_reentrant_enqueue_is_ordered_and_does_not_hold_its_mutex() {
    struct Reentrant {
        stream: Arc<SyntheticEventStream>,
        owner: NativeRequestOwner,
        seen: Mutex<Vec<String>>,
    }
    impl EventSink for Reentrant {
        fn emit(&self, _: Target, _: &'static str, _: Vec<IpcValue>) { panic!("unchecked synthetic event"); }
        fn emit_to_owner_checked(&self, _: &NativeRequestOwner, _: &'static str, args: Vec<IpcValue>) -> Result<(), String> {
            let IpcValue::String(value) = &args[0] else { return Err("unexpected event".into()); };
            self.seen.lock().unwrap().push(value.clone());
            if value == "first" {
                let mut next = Some(Box::new(SyntheticEventNode { owner: self.owner.clone(), channel: "generate:state",
                    args: vec![IpcValue::String("second".into())], next: None }));
                assert!(!self.stream.enqueue(&mut next)?);
            }
            Ok(())
        }
    }
    let (effects, _, _, _, owner) = model();
    let (stream, first) = effects.event_streams.prepare(&owner, "generate:state",
        vec![IpcValue::String("first".into())]).unwrap();
    let sink = Reentrant { stream: stream.clone(), owner, seen: Mutex::new(Vec::new()) };
    assert!(stream.enqueue(&mut Some(first)).unwrap());
    stream.drain(&sink);
    assert_eq!(*sink.seen.lock().unwrap(), ["first", "second"]);
}

#[test]
fn accepted_ui_action_and_completion_run_after_admission_gates_drop() {
    let (effects, ui, _, _, owner) = model();
    let cancel = CancelToken::default();
    let cancel_in_action = cancel.clone();
    let renderer = owner.context.renderer().unwrap().clone();
    let result = Arc::new(Mutex::new(None));
    let done = result.clone();
    effects.post_ui_owned(owner.clone(), owner, cancel.clone(), cancel.clone(), Box::new(move || {
        renderer.revoke();
        cancel_in_action.cancel();
        Err("actual constructor failed".into())
    }), Box::new(move |answer| *done.lock().unwrap() = Some(answer)));
    assert!(result.lock().unwrap().is_none());
    ui.drain();
    assert!(cancel.is_cancelled());
    assert_eq!(*result.lock().unwrap(), Some(Err("actual constructor failed".into())));
}

#[test]
fn ui_admission_rechecks_pending_cancellation_and_replacement_at_actual_execution() {
    for replace in [false, true] {
        let (effects, ui, views, _, owner) = model();
        let pending = CancelToken::default();
        let ran = Arc::new(Mutex::new(false));
        let action = ran.clone();
        let result = Arc::new(Mutex::new(None));
        let done = result.clone();
        effects.post_ui_owned(owner.clone(), owner, CancelToken::default(), pending.clone(), Box::new(move || {
            *action.lock().unwrap() = true;
            Ok(())
        }), Box::new(move |answer| *done.lock().unwrap() = Some(answer)));
        if replace { views.replace(ViewId(1)); } else { pending.cancel(); }
        ui.drain();
        assert!(!*ran.lock().unwrap());
        assert!(result.lock().unwrap().as_ref().unwrap().is_err());
    }
}

#[test]
fn admitted_transition_retires_only_captured_documents_and_notifies_after_all_gates() {
    let (effects, _, _, _, mut owner) = model();
    let document = RendererGeneration::new();
    owner.context = owner.context.with_document_lease(document.clone());
    let renderer = owner.context.renderer().unwrap().clone();
    let observed = Arc::new(Mutex::new(false));
    let notified = observed.clone();
    let pending = CancelToken::default();
    let callback_pending = pending.clone();
    let _subscription = document.loss_token().on_cancel(move || {
        renderer.with_active(|| *notified.lock().unwrap() = true).unwrap();
        callback_pending.cancel();
    });
    let cancel = CancelToken::default();
    let unrelated = RendererGeneration::new();
    let result = effects.with_owned_transition(&owner, &owner, &cancel, &pending, &mut |retirement| {
        assert!(retirement.retire(&unrelated).is_err());
        retirement.retire(&document)?;
        assert!(!document.is_active());
        assert!(!*observed.lock().unwrap());
        Err("terminal effect failed after retirement".into())
    });
    assert_eq!(result, Err("terminal effect failed after retirement".into()));
    assert!(unrelated.is_active());
    assert!(*observed.lock().unwrap());
    assert!(pending.is_cancelled());
    assert!(owner.loss_token().unwrap().is_cancelled());
}

#[test]
fn many_owner_transition_admits_the_whole_set_and_deduplicates_aliases() {
    let (effects, _, views, _, mut source) = model();
    let mut documents = vec![RendererGeneration::new()];
    source.context = source.context.with_document_lease(documents[0].clone());
    let mut destinations = Vec::new();
    for id in 2..=4 {
        let generation = RendererGeneration::new();
        views.add(ViewId(id), generation.clone());
        let mut owner = effects.capture_owner(ViewId(id), &generation).unwrap();
        let document = RendererGeneration::new();
        owner.context = owner.context.with_document_lease(document.clone());
        documents.push(document);
        destinations.push(owner);
    }
    destinations.push(source.clone());
    destinations.push(destinations[0].clone());
    let notified = Arc::new(Mutex::new(0));
    let subscriptions = documents.iter().map(|document| {
        let notified = notified.clone();
        let renderer = source.context.renderer().unwrap().clone();
        document.loss_token().on_cancel(move || {
            renderer.with_active(|| *notified.lock().unwrap() += 1).unwrap();
        })
    }).collect::<Vec<_>>();
    let cancel = CancelToken::default();
    effects.with_owned_transition_many(&source, &destinations, &cancel, &cancel, &mut |admitted| {
        for document in &documents { admitted.retire(document)?; }
        assert_eq!(*notified.lock().unwrap(), 0);
        Ok(())
    }).unwrap();
    assert!(documents.iter().all(|document| !document.is_active()));
    assert_eq!(*notified.lock().unwrap(), 4);
    drop(subscriptions);
}

#[test]
fn one_lost_destination_rejects_the_entire_many_owner_transition() {
    let (effects, _, views, _, source) = model();
    let generation = RendererGeneration::new();
    views.add(ViewId(2), generation.clone());
    let target = effects.capture_owner(ViewId(2), &generation).unwrap();
    let cancel = CancelToken::default();
    target.loss_token().unwrap().cancel();
    let mut ran = false;
    assert!(effects.with_owned_transition_many(&source, &[source.clone(), target], &cancel, &cancel, &mut |_| {
        ran = true;
        Ok(())
    }).is_err());
    assert!(!ran);
}

#[test]
fn published_docs_identity_is_exact_and_old_binding_drop_cannot_remove_its_successor() {
    let (effects, _, _, _, owner) = model();
    let renderer = owner.context.renderer().unwrap();
    let old_document = RendererGeneration::new();
    let old_binding = effects.bind_docs_document(owner.view, renderer, 41, old_document.clone()).unwrap();
    let captured = effects.capture_owner(owner.view, renderer).unwrap();
    assert_eq!(effects.docs_document_identity(&captured), Some((owner.view, 41)));
    let document = RendererGeneration::new();
    let binding = effects.bind_docs_document(owner.view, renderer, 42, document.clone()).unwrap();
    drop(old_binding);
    assert!(old_document.is_active());
    let successor = effects.capture_owner(owner.view, renderer).unwrap();
    assert_eq!(effects.docs_document_identity(&successor), Some((owner.view, 42)));
    old_document.revoke();
    let cancel = CancelToken::default();
    assert!(effects.with_owned_effect(&captured, &successor, &cancel, &cancel, &mut || Ok(())).is_err());
    assert!(effects.with_owned_effect(&successor, &successor, &cancel, &cancel, &mut || Ok(())).is_ok());
    document.revoke();
    assert!(effects.capture_owner(owner.view, renderer).is_err());
    drop(binding);
    assert_eq!(effects.docs_document_identity(&effects.capture_owner(owner.view, renderer).unwrap()), None);
}

#[test]
fn many_owner_event_uses_the_exact_recipient_and_requires_every_captured_owner() {
    let (effects, _, views, events, source) = model();
    let mut destinations = Vec::new();
    for id in [2, 3] {
        let generation = RendererGeneration::new();
        views.add(ViewId(id), generation.clone());
        destinations.push(effects.capture_owner(ViewId(id), &generation).unwrap());
    }
    let cancel = CancelToken::default();
    effects.emit_owned_many(&source, &destinations, 1, &cancel, &cancel, "generate:state", vec![IpcValue::Undefined]).unwrap();
    assert_eq!(events.sent.lock().unwrap().as_slice(), &[(Target::View(ViewId(3)), "generate:state", vec![IpcValue::Undefined])]);
    assert!(effects.emit_owned_many(&source, &destinations, 2, &cancel, &cancel, "generate:state", vec![]).is_err());
    destinations[0].loss_token().unwrap().cancel();
    assert!(effects.emit_owned_many(&source, &destinations, 1, &cancel, &cancel, "generate:state", vec![]).is_err());
    assert_eq!(events.sent.lock().unwrap().len(), 1);
}

#[test]
fn many_owner_ui_action_runs_after_all_gates_and_returns_its_real_error() {
    let (effects, ui, views, _, source) = model();
    let generation = RendererGeneration::new();
    views.add(ViewId(2), generation.clone());
    let target = effects.capture_owner(ViewId(2), &generation).unwrap();
    let cancel = CancelToken::default();
    let target_loss = target.loss_token().unwrap();
    let action_loss = target_loss.clone();
    let result = Arc::new(Mutex::new(None));
    let answer = result.clone();
    effects.post_ui_owned_many(source.clone(), vec![source, target], cancel.clone(), cancel,
        Box::new(move || {
            action_loss.cancel();
            Err("actual native action failed".into())
        }),
        Box::new(move |value| *answer.lock().unwrap() = Some(value)));
    assert!(result.lock().unwrap().is_none());
    ui.drain();
    assert!(target_loss.is_cancelled());
    assert_eq!(*result.lock().unwrap(), Some(Err("actual native action failed".into())));
}

#[test]
fn many_owner_ui_action_rechecks_every_owner_after_posting() {
    let (effects, ui, views, _, source) = model();
    let generation = RendererGeneration::new();
    views.add(ViewId(2), generation.clone());
    let target = effects.capture_owner(ViewId(2), &generation).unwrap();
    let cancel = CancelToken::default();
    let called = Arc::new(Mutex::new(false));
    let action_called = called.clone();
    let result = Arc::new(Mutex::new(None));
    let answer = result.clone();
    effects.post_ui_owned_many(source.clone(), vec![source, target], cancel.clone(), cancel,
        Box::new(move || { *action_called.lock().unwrap() = true; Ok(()) }),
        Box::new(move |value| *answer.lock().unwrap() = Some(value)));
    views.replace(ViewId(2));
    ui.drain();
    assert!(!*called.lock().unwrap());
    assert!(result.lock().unwrap().as_ref().unwrap().is_err());
}

#[test]
fn standalone_many_admission_requires_every_lifetime_and_releases_before_action() {
    let (effects, _, views, _, origin) = model();
    let renderer = RendererGeneration::new();
    views.add(ViewId(2), renderer.clone());
    let mut target = effects.capture_owner(ViewId(2), &renderer).unwrap();
    let document = RendererGeneration::new();
    target.context = target.context.with_document_lease(document.clone());
    let cancel = CancelToken::default();
    let result = admit_many(&origin, &[origin.clone(), target.clone()], &cancel, &cancel, || {
        document.revoke();
        Err::<(), _>("actual dialog failed")
    }).unwrap();
    assert_eq!(result, Err("actual dialog failed"));
    let mut called = false;
    assert!(admit_many(&origin, &[target], &cancel, &cancel, || {
        called = true;
    }).is_err());
    assert!(!called);
}

#[test]
fn invocation_admission_requires_registered_identity_and_every_lifetime() {
    use crate::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    let (effects, _, views, events, owner) = model();
    let cancel = CancelToken::default();
    let operation = NativeInvocationIssuer::new(cancel.clone()).issue(
        NativeInvocationSelection::HeadlessExport {
            surface: Surface::Shell, input: "in".into(), output: "out".into(),
            format: "pdf".into(), page: None, ops: None, batch: None,
        }, cancel.clone());
    let target = operation.reserve_target(owner.view, owner.context.renderer().unwrap().clone()).unwrap();
    let document = register_native_invocation_fixture(&target, owner.clone()).unwrap();
    assert!(register_native_invocation_fixture(&target, owner.clone()).is_err());
    let mut count = 0;
    effects.with_native_invocation_effect(&document, &owner, &[owner.clone()], &cancel, &cancel,
        &mut || { count += 1; Ok(()) }).unwrap();
    assert_eq!(count, 1);
    effects.emit_native_invocation_many(&document, &owner, &[owner.clone()], 0, &cancel, &cancel,
        "generate:state", vec![IpcValue::Undefined]).unwrap();
    assert_eq!(events.sent.lock().unwrap().len(), 1);
    assert!(effects.emit_native_invocation_many(&document, &owner, &[owner.clone()], 1, &cancel, &cancel,
        "generate:state", vec![]).is_err());
    assert_eq!(effects.with_native_invocation_effect(&document, &owner, &[], &cancel, &cancel,
        &mut || Err("actual failure".into())), Err("actual failure".into()));
    let different = effects.capture_owner(owner.view, owner.context.renderer().unwrap()).unwrap();
    assert!(effects.with_native_invocation_effect(&document, &different, &[], &cancel, &cancel,
        &mut || { count += 1; Ok(()) }).is_err());
    views.replace(owner.view);
    assert!(effects.with_native_invocation_effect(&document, &owner, &[], &cancel, &cancel,
        &mut || { count += 1; Ok(()) }).is_err());
    assert!(effects.emit_native_invocation_many(&document, &owner, &[owner.clone()], 0, &cancel, &cancel,
        "generate:state", vec![]).is_err());
    assert_eq!(count, 1);
}

#[test]
fn recovery_pdf_revision_transition_retains_successor_and_notifies_after_cleanup() {
    use crate::ipc::{RecoveryOpenContext, RecoveryOpenTarget};
    let (effects, _, _, _, receiver) = model();
    let created = receiver.context.renderer().unwrap().clone();
    let cancel = CancelToken::default();
    let (reservation, browser) = closure_reservation(receiver.view, &created);
    let target = RecoveryOpenTarget::bind(RecoveryOpenContext::Renderer {
        source: receiver.clone(), participants: vec![], cancel: cancel.clone(), pending: cancel.clone(),
    }, receiver.view, created.clone(), reservation, browser).unwrap();
    let receipt = effects.commit_recovery_seed(&target, &mut || Ok(())).unwrap();
    target.retain_native_registration(receiver.clone()).unwrap();
    let old = PdfDocumentLease::new();
    let next = PdfDocumentLease::new();
    let destination = NativeRequestOwner { view: receiver.view, context: receiver.context.clone().with_pdf_revision(old.clone()) };
    let stale = target.authority_snapshot().unwrap();
    let mut renewal = stale.prepare_pdf_revision_renewal(&[destination.clone()], &old, &next).unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let observed = order.clone();
    let _loss = old.loss_token().on_cancel(move || observed.lock().unwrap().push("notify"));
    let cleanup = order.clone();
    let mut successor = None;
    effects.with_recovery_pdf_revision_transition(&receipt, &receiver, &[destination.clone()], &cancel, &cancel,
        &old, &next, &mut renewal, &mut || cleanup.lock().unwrap().push("cleanup"), &mut |admitted, renewal| {
            renewal.commit(admitted)?;
            successor = renewal.successor_for(&destination).cloned();
            Ok(())
        }).unwrap();
    assert!(stale.with_current(|| ()).is_none());
    assert_eq!(target.authority_snapshot().unwrap().with_current(|| ()), Some(()));
    assert_eq!(*order.lock().unwrap(), ["cleanup", "notify"]);
    assert!(successor.unwrap().pdf_revision().unwrap().same_document(&next));
    assert!(!old.is_live());
}

#[test]
fn recovery_pdf_revision_transition_rejects_wrong_or_lost_participants_and_unwinds_safely() {
    use crate::ipc::{RecoveryOpenContext, RecoveryOpenTarget};
    let (effects, _, views, _, receiver) = model();
    let created = receiver.context.renderer().unwrap().clone();
    let cancel = CancelToken::default();
    let (reservation, browser) = closure_reservation(receiver.view, &created);
    let target = RecoveryOpenTarget::bind(RecoveryOpenContext::Renderer {
        source: receiver.clone(), participants: vec![], cancel: cancel.clone(), pending: cancel.clone(),
    }, receiver.view, created.clone(), reservation, browser).unwrap();
    let receipt = effects.commit_recovery_seed(&target, &mut || Ok(())).unwrap();
    target.retain_native_registration(receiver.clone()).unwrap();
    let old = PdfDocumentLease::new();
    let next = PdfDocumentLease::new();
    let destination = NativeRequestOwner { view: receiver.view, context: receiver.context.clone().with_pdf_revision(old.clone()) };
    let snapshot = target.authority_snapshot().unwrap();
    let mut renewal = snapshot.prepare_pdf_revision_renewal(&[destination.clone()], &old, &next).unwrap();
    let (reservation, browser) = closure_reservation(receiver.view, &created);
    let other_target = RecoveryOpenTarget::bind(RecoveryOpenContext::Renderer {
        source: receiver.clone(), participants: vec![], cancel: cancel.clone(), pending: cancel.clone(),
    }, receiver.view, created.clone(), reservation, browser).unwrap();
    let other_receipt = effects.commit_recovery_seed(&other_target, &mut || Ok(())).unwrap();
    other_target.retain_native_registration(receiver.clone()).unwrap();
    assert!(effects.with_recovery_pdf_revision_transition(&other_receipt, &receiver, &[destination.clone()], &cancel, &cancel,
        &old, &next, &mut renewal, &mut || {}, &mut |admitted, renewal| renewal.commit(admitted)).is_err());
    let wrong = PdfDocumentLease::new();
    assert!(effects.with_recovery_pdf_revision_transition(&receipt, &receiver, &[destination.clone()], &cancel, &cancel,
        &wrong, &next, &mut renewal, &mut || {}, &mut |_, _| Ok(())).is_err());
    let mut wrong_successor = snapshot.prepare_pdf_revision_renewal(&[destination.clone()], &old, &PdfDocumentLease::new()).unwrap();
    assert!(effects.with_recovery_pdf_revision_transition(&receipt, &receiver, &[destination.clone()], &cancel, &cancel,
        &old, &next, &mut wrong_successor, &mut || {}, &mut |admitted, renewal| renewal.commit(admitted)).is_err());
    assert!(old.is_live());
    let lost_generation = RendererGeneration::new();
    views.add(ViewId(2), lost_generation.clone());
    let lost = effects.capture_owner(ViewId(2), &lost_generation).unwrap();
    lost_generation.revoke();
    assert!(effects.with_recovery_pdf_revision_transition(&receipt, &receiver, &[destination.clone(), lost], &cancel, &cancel,
        &old, &next, &mut renewal, &mut || {}, &mut |_, _| Ok(())).is_err());
    let order = Arc::new(Mutex::new(Vec::new()));
    let observed = order.clone();
    let _loss = old.loss_token().on_cancel(move || observed.lock().unwrap().push("notify"));
    let cleanup = order.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = effects.with_recovery_pdf_revision_transition(&receipt, &receiver, &[destination], &cancel, &cancel,
            &old, &next, &mut renewal, &mut || cleanup.lock().unwrap().push("cleanup"), &mut |admitted, renewal| {
                renewal.commit(admitted)?;
                panic!("after PDF retirement");
            });
    }));
    assert!(result.is_err());
    assert_eq!(*order.lock().unwrap(), ["cleanup", "notify"]);
}

#[test]
fn recovery_seed_and_final_effect_require_original_lineage() {
    use crate::ipc::{RecoveryOpenContext, RecoveryOpenTarget};
    let (effects, _, views, events, source) = model();
    let created = RendererGeneration::new();
    views.add(ViewId(2), created.clone());
    let receiver = effects.capture_owner(ViewId(2), &created).unwrap();
    let cancel = CancelToken::default();
    let (reservation, browser) = closure_reservation(receiver.view, &created);
    let target = RecoveryOpenTarget::bind(RecoveryOpenContext::Renderer {
        source: source.clone(), participants: vec![source.clone()],
        cancel: cancel.clone(), pending: cancel.clone(),
    }, receiver.view, created.clone(), reservation, browser).unwrap();
    let mut count = 0;
    effects.with_recovery_seed_effect(&target, &mut || { count += 1; Ok(()) }).unwrap();
    assert!(effects.with_recovery_dialog_effect(&target, &receiver, &RendererGeneration::new(),
        &cancel, &cancel, &mut || { count += 1; Ok(()) }).is_err());
    effects.with_recovery_dialog_effect(&target, &receiver, receiver.context.renderer().unwrap(),
        &cancel, &cancel, &mut || { count += 1; Ok(()) }).unwrap();
    effects.emit_recovery_dialog(&target, &receiver, receiver.context.renderer().unwrap(), &cancel, &cancel,
        "generate:state", vec![IpcValue::Undefined]).unwrap();
    assert_eq!(events.sent.lock().unwrap().as_slice(), &[(Target::View(receiver.view), "generate:state", vec![IpcValue::Undefined])]);
    *events.failure.lock().unwrap() = Some("delivery failed".into());
    effects.emit_recovery_dialog(&target, &receiver, receiver.context.renderer().unwrap(),
        &cancel, &cancel, "generate:state", vec![]).unwrap();
    assert_eq!(events.failed.lock().unwrap().as_slice(), &["delivery failed"]);
    assert!(effects.commit_recovery_seed(&target, &mut || Err("enqueue failed".into())).is_err());
    let receipt = effects.commit_recovery_seed(&target, &mut || Ok(())).unwrap();
    assert!(effects.commit_recovery_seed(&target, &mut || Ok(())).is_err());
    target.retain_native_registration(receiver.clone()).unwrap();
    assert!(effects.with_recovery_dialog_effect(&target, &receiver, receiver.context.renderer().unwrap(),
        &cancel, &cancel, &mut || { count += 1; Ok(()) }).is_err());
    assert!(effects.emit_recovery_dialog(&target, &receiver, receiver.context.renderer().unwrap(),
        &cancel, &cancel, "generate:state", vec![]).is_err());
    assert!(effects.with_recovery_seed_effect(&target, &mut || { count += 1; Ok(()) }).is_err());
    let other = effects.capture_owner(receiver.view, receiver.context.renderer().unwrap()).unwrap();
    let request = CancelToken::default();
    assert!(effects.with_recovery_open_effect(&receipt, &other, &request, &mut || { count += 1; Ok(()) }).is_err());
    cancel.cancel();
    effects.with_recovery_open_effect(&receipt, &receiver, &request, &mut || { count += 1; Ok(()) }).unwrap();
    let extra = CancelToken::default();
    extra.cancel();
    let fresh_pending = CancelToken::default();
    assert!(effects.with_recovery_open_effect_many_extra(&receipt, &receiver, &[], &request, &fresh_pending,
        &[&extra], &mut || { count += 1; Ok(()) }).is_err());
    let pending = CancelToken::default();
    pending.cancel();
    assert!(effects.with_recovery_open_effect_pending(&receipt, &receiver, &request, &pending,
        &mut || { count += 1; Ok(()) }).is_err());
    source.context.renderer().unwrap().revoke();
    let active = CancelToken::default();
    assert!(effects.with_recovery_open_effect_many(&receipt, &receiver, &[source], &request, &active,
        &mut || { count += 1; Ok(()) }).is_err());
    request.cancel();
    assert!(effects.with_recovery_open_effect(&receipt, &receiver, &request, &mut || { count += 1; Ok(()) }).is_err());
    assert_eq!(count, 3);
}

#[test]
fn native_recovery_uses_the_reserved_target_and_operation_shutdown() {
    use crate::ipc::{NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision, RecoveryOpenContext, RecoveryOpenTarget};
    let (effects, _, _, _, owner) = model();
    let shutdown = CancelToken::default();
    let operation = NativeInvocationIssuer::new(shutdown.clone()).issue(
        NativeInvocationSelection::RecoveryFile {
            surface: Surface::Shell, path: "recovery".into(), decision: NativeRecoveryDecision::Restore,
        }, CancelToken::default());
    let (reservation, browser) = closure_reservation(owner.view, owner.context.renderer().unwrap());
    let target = RecoveryOpenTarget::bind(RecoveryOpenContext::NativeStartup(operation),
        owner.view, owner.context.renderer().unwrap().clone(), reservation, browser).unwrap();
    let mut count = 0;
    effects.with_recovery_seed_effect(&target, &mut || { count += 1; Ok(()) }).unwrap();
    let receipt = effects.commit_recovery_seed(&target, &mut || Ok(())).unwrap();
    register_recovery_fixture(&target, owner.clone()).unwrap();
    effects.with_recovery_open_effect(&receipt, &owner, &CancelToken::default(), &mut || { count += 1; Ok(()) }).unwrap();
    shutdown.cancel();
    assert!(effects.with_recovery_open_effect(&receipt, &owner, &CancelToken::default(), &mut || { count += 1; Ok(()) }).is_err());
    assert_eq!(count, 2);
}

#[test]
fn startup_constraint_survives_seed_and_final_recovery_admission() {
    use crate::ipc::{NativeInvocationIssuer, NativeInvocationSelection, RecoveryOpenContext, RecoveryOpenTarget, StartupRecoveryConstraint};
    let (effects, _, _, _, plain) = model();
    let created = plain.context.renderer().unwrap().clone();
    let owner = effects.capture_invocation_root(plain.view, &created, &SyntheticNativeOrigin::new()).unwrap();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let startup = issuer.issue_startup(vec![], false, owner.clone(), CancelToken::default());
    let selection = NativeInvocationSelection::RecoveryBlank { surface: Surface::Docs, title: "Recovery".into() };
    let cancelled = CancelToken::default();
    let first = issuer.issue_recovery_child(&startup, selection.clone(), CancelToken::default()).unwrap();
    let (reservation, browser) = closure_reservation(owner.view, &created);
    let target = RecoveryOpenTarget::bind_startup(RecoveryOpenContext::NativeStartup(first), owner.view, created.clone(), reservation, browser,
        StartupRecoveryConstraint { startup: startup.clone(), shell: owner.clone(), participants: vec![owner.clone()],
            cancel: cancelled.clone(), pending: CancelToken::default() }, None).unwrap();
    cancelled.cancel();
    assert!(effects.commit_recovery_seed(&target, &mut || Ok(())).is_err());

    let child = issuer.issue_recovery_child(&startup, selection, CancelToken::default()).unwrap();
    let pending = CancelToken::default();
    let (reservation, browser) = closure_reservation(owner.view, &created);
    let target = RecoveryOpenTarget::bind_startup(RecoveryOpenContext::NativeStartup(child), owner.view, created.clone(), reservation, browser,
        StartupRecoveryConstraint { startup, shell: owner.clone(), participants: vec![owner.clone()],
            cancel: CancelToken::default(), pending: pending.clone() }, None).unwrap();
    let receipt = effects.commit_recovery_seed(&target, &mut || Ok(())).unwrap();
    register_recovery_fixture(&target, owner.clone()).unwrap();
    effects.with_recovery_open_effect(&receipt, &owner, &CancelToken::default(), &mut || Ok(())).unwrap();
    pending.cancel();
    assert!(effects.with_recovery_open_effect(&receipt, &owner, &CancelToken::default(), &mut || Ok(())).is_err());
    assert!(effects.settle_recovery_seed(&receipt, &owner, &CancelToken::default(), Ok(())).is_err());
}

#[test]
fn command_recovery_keeps_command_authority_through_seed_and_acknowledgement() {
    use crate::ipc::{CommandRecoveryConstraint, NativeEditorCommand, NativeInvocationIssuer, NativeInvocationSelection, RecoveryOpenContext, RecoveryOpenTarget};
    let (effects, _, _, _, owner) = model();
    let generation = owner.context.renderer().unwrap().clone();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let root = issuer.issue_command(NativeEditorCommand::OpenFile { surface: Surface::Docs, path: "recovery.docx".into() }, CancelToken::default());
    let child = issuer.issue_command_recovery_child(&root, NativeInvocationSelection::RecoveryBlank { surface: Surface::Docs, title: "Recovery".into() }, CancelToken::default()).unwrap();
    let cancel = CancelToken::default();
    let (reservation, browser) = closure_reservation(owner.view, &generation);
    let target = RecoveryOpenTarget::bind_command(RecoveryOpenContext::NativeStartup(child), owner.view, generation.clone(), reservation, browser,
        CommandRecoveryConstraint { command: root.clone(), participants: vec![], cancel: cancel.clone(), pending: cancel.clone() }).unwrap();
    let receipt = effects.commit_recovery_seed(&target, &mut || Ok(())).unwrap();
    register_recovery_fixture(&target, owner.clone()).unwrap();
    let document = target.invocation_target().unwrap().registered_document().unwrap();
    let wrong_root = issuer.issue_command(NativeEditorCommand::OpenHome, CancelToken::default());
    assert!(effects.with_command_recovery_target_effect(&wrong_root, &document, &owner, &[], &cancel, &cancel, &mut || Ok(())).is_err());
    effects.with_command_recovery_target_effect(&root, &document, &owner, &[], &cancel, &cancel, &mut || Ok(())).unwrap();
    effects.with_recovery_open_effect(&receipt, &owner, &cancel, &mut || Ok(())).unwrap();
    root.finish();
    assert!(effects.with_command_recovery_target_effect(&root, &document, &owner, &[], &cancel, &cancel, &mut || Ok(())).is_err());
    assert!(effects.with_recovery_open_effect(&receipt, &owner, &cancel, &mut || Ok(())).is_err());
    assert!(effects.settle_recovery_seed(&receipt, &owner, &cancel, Ok(())).is_err());
}

#[test]
fn command_recovery_registration_rejects_root_revoked_after_seed() {
    use crate::ipc::{CommandRecoveryConstraint, NativeEditorCommand, NativeInvocationIssuer, NativeInvocationSelection, RecoveryOpenContext, RecoveryOpenTarget};
    let (effects, _, _, _, owner) = model();
    let generation = owner.context.renderer().unwrap().clone();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let root = issuer.issue_command(NativeEditorCommand::OpenHome, CancelToken::default());
    let child = issuer.issue_command_recovery_child(&root,
        NativeInvocationSelection::RecoveryBlank { surface: Surface::Docs, title: "Recovery".into() }, CancelToken::default()).unwrap();
    let control = CancelToken::default();
    let (reservation, browser) = closure_reservation(owner.view, &generation);
    let target = RecoveryOpenTarget::bind_command(RecoveryOpenContext::NativeStartup(child), owner.view,
        generation.clone(), reservation, browser,
        CommandRecoveryConstraint { command: root.clone(), participants: vec![], cancel: control.clone(), pending: control }).unwrap();
    effects.commit_recovery_seed(&target, &mut || Ok(())).unwrap();
    root.finish();
    assert!(register_recovery_fixture(&target, owner).is_err());
    assert!(target.registered_owner().is_none());
}

#[test]
fn command_recovery_retains_participant_until_final_admission() {
    use crate::ipc::{CommandRecoveryConstraint, NativeEditorCommand, NativeInvocationIssuer, NativeInvocationSelection, RecoveryOpenContext, RecoveryOpenTarget};
    let (effects, _, views, _, owner) = model();
    let other_generation = RendererGeneration::new();
    views.add(ViewId(2), other_generation.clone());
    let participant = effects.capture_owner(ViewId(2), &other_generation).unwrap();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let root = issuer.issue_command(NativeEditorCommand::OpenFile { surface: Surface::Docs, path: "copy.docx".into() }, CancelToken::default());
    let child = issuer.issue_command_recovery_child(&root, NativeInvocationSelection::RecoveryBlank { surface: Surface::Docs, title: "Recovery".into() }, CancelToken::default()).unwrap();
    let cancel = CancelToken::default();
    let (reservation, browser) = closure_reservation(owner.view, owner.context.renderer().unwrap());
    let target = RecoveryOpenTarget::bind_command(RecoveryOpenContext::NativeStartup(child), owner.view, owner.context.renderer().unwrap().clone(), reservation, browser,
        CommandRecoveryConstraint { command: root, participants: vec![participant], cancel: cancel.clone(), pending: cancel.clone() }).unwrap();
    let receipt = effects.commit_recovery_seed(&target, &mut || Ok(())).unwrap();
    register_recovery_fixture(&target, owner.clone()).unwrap();
    other_generation.revoke();
    assert!(effects.with_recovery_open_effect(&receipt, &owner, &cancel, &mut || Ok(())).is_err());
}

#[test]
fn native_command_event_enqueues_only_for_the_selected_live_generation() {
    use crate::ipc::{NativeEditorCommand, NativeInvocationIssuer};
    let (effects, _, views, events, owner) = model();
    let created = owner.context.renderer().unwrap().clone();
    let cancel = CancelToken::default();
    let command = NativeEditorCommand::OpenFile { surface: Surface::Sheets, path: "book.xlsx".into() };
    let operation = NativeInvocationIssuer::new(CancelToken::default()).issue_command(command.clone(), CancelToken::default());
    effects.emit_native_command_creation_many(&operation, &command, &created, &[owner.clone()], 0,
        &cancel, &cancel, "menu:action", vec![IpcValue::String("open".into())]).unwrap();
    assert_eq!(events.sent.lock().unwrap().len(), 1);
    views.replace(owner.view);
    assert!(effects.emit_native_command_creation_many(&operation, &command, &created, &[owner], 0,
        &cancel, &cancel, "menu:action", vec![]).is_err());
    assert_eq!(events.sent.lock().unwrap().len(), 1);
}

#[test]
fn native_command_preparation_requires_the_selected_root_or_exact_startup_parent() {
    use crate::ipc::{NativeEditorCommand, NativeInvocationIssuer};
    let (effects, _, _, _, plain) = model();
    let generation = plain.context.renderer().unwrap().clone();
    let owner = effects.capture_invocation_root(plain.view, &generation, &SyntheticNativeOrigin::new()).unwrap();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let command = NativeEditorCommand::OpenFile { surface: Surface::Docs, path: "selected.docx".into() };
    let other = NativeEditorCommand::OpenFile { surface: Surface::Docs, path: "other.docx".into() };
    let cancel = CancelToken::default();
    let root = issuer.issue_command(command.clone(), CancelToken::default());
    let startup = issuer.issue_startup(vec![], false, owner.clone(), CancelToken::default());
    let other_startup = issuer.issue_startup(vec![], false, owner.clone(), CancelToken::default());
    let child = issuer.issue_command_child(&startup, command.clone(), CancelToken::default()).unwrap();
    let mut calls = 0;
    effects.with_native_command_preparation_effect(&root, &command, &[], &cancel, &cancel, &mut || { calls += 1; Ok(()) }).unwrap();
    assert!(effects.with_native_command_preparation_effect(&child, &command, &[], &cancel, &cancel, &mut || { calls += 1; Ok(()) }).is_err());
    assert!(effects.with_startup_command_preparation_effect(&startup, &owner, &child, &other, &[], &cancel, &cancel, &mut || { calls += 1; Ok(()) }).is_err());
    effects.with_startup_command_preparation_effect(&startup, &owner, &child, &command, &[], &cancel, &cancel, &mut || { calls += 1; Ok(()) }).unwrap();
    let target = child.reserve_target(owner.view, generation).unwrap();
    let document = register_native_invocation_fixture(&target, owner.clone()).unwrap();
    effects.with_startup_command_target_effect(&startup, &owner, &child, &command, &document, &owner, &[], &cancel, &cancel, &mut || { calls += 1; Ok(()) }).unwrap();
    assert!(effects.with_startup_command_target_effect(&other_startup, &owner, &child, &command, &document, &owner, &[], &cancel, &cancel, &mut || { calls += 1; Ok(()) }).is_err());
    assert!(effects.with_startup_command_target_effect(&startup, &owner, &child, &other, &document, &owner, &[], &cancel, &cancel, &mut || { calls += 1; Ok(()) }).is_err());
    startup.finish();
    assert!(effects.with_startup_command_target_effect(&startup, &owner, &child, &command, &document, &owner, &[], &cancel, &cancel, &mut || { calls += 1; Ok(()) }).is_err());
    assert_eq!(calls, 3);
}

#[test]
fn startup_command_target_rejects_lost_child_and_replaced_renderer() {
    use crate::ipc::{NativeEditorCommand, NativeInvocationIssuer};
    let (effects, _, views, _, plain) = model();
    let generation = plain.context.renderer().unwrap().clone();
    let owner = effects.capture_invocation_root(plain.view, &generation, &SyntheticNativeOrigin::new()).unwrap();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let startup = issuer.issue_startup(vec![], false, owner.clone(), CancelToken::default());
    let command = NativeEditorCommand::OpenFile { surface: Surface::Docs, path: "selected.docx".into() };
    let child = issuer.issue_command_child(&startup, command.clone(), CancelToken::default()).unwrap();
    let target = child.reserve_target(owner.view, generation.clone()).unwrap();
    let document = register_native_invocation_fixture(&target, owner.clone()).unwrap();
    let cancel = CancelToken::default();
    child.finish();
    assert!(effects.with_startup_command_target_effect(&startup, &owner, &child, &command, &document, &owner, &[], &cancel, &cancel, &mut || Ok(())).is_err());
    let child = issuer.issue_command_child(&startup, command.clone(), CancelToken::default()).unwrap();
    let target = child.reserve_target(owner.view, generation).unwrap();
    let document = register_native_invocation_fixture(&target, owner.clone()).unwrap();
    views.replace(owner.view);
    assert!(effects.with_startup_command_target_effect(&startup, &owner, &child, &command, &document, &owner, &[], &cancel, &cancel, &mut || Ok(())).is_err());
}

#[test]
fn invocation_fixture_preserves_explicit_lineage_across_docs_publication() {
    use crate::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    let (effects, _, _, _, original) = model();
    let generation = original.context.renderer().unwrap().clone();
    let provenance = SyntheticNativeOrigin::new();
    let root = effects.capture_invocation_root(original.view, &generation, &provenance).unwrap();
    let cancel = CancelToken::default();
    let operation = NativeInvocationIssuer::new(cancel.clone()).issue(
        NativeInvocationSelection::HeadlessExport {
            surface: Surface::Shell, input: "in".into(), output: "out".into(),
            format: "pdf".into(), page: None, ops: None, batch: None,
        }, cancel.clone());
    let target = operation.reserve_target(root.view, generation.clone()).unwrap();
    let document = register_native_invocation_fixture(&target, root).unwrap();
    let document_lease = RendererGeneration::new();
    let _binding = effects.bind_docs_document(original.view, &generation, 7, document_lease.clone()).unwrap();
    let candidate = effects.capture_invocation_docs(original.view, &generation, &provenance).unwrap();
    let foreign = effects.capture_invocation_docs(original.view, &generation, &SyntheticNativeOrigin::new()).unwrap();
    let mut count = 0;
    effects.with_native_invocation_effect(&document, &candidate, &[], &cancel, &cancel,
        &mut || { count += 1; Ok(()) }).unwrap();
    effects.with_same_native_origin_effect(document.owner(), &candidate, &[], &cancel, &cancel, &[&cancel],
        &mut || Ok(())).unwrap();
    assert!(effects.with_same_native_origin_effect(document.owner(), &foreign, &[], &cancel, &cancel, &[],
        &mut || Ok(())).is_err());
    let extra = CancelToken::default();
    extra.cancel();
    assert!(effects.with_same_native_origin_effect(document.owner(), &candidate, &[], &cancel, &cancel, &[&extra],
        &mut || Ok(())).is_err());
    assert!(effects.with_native_invocation_effect(&document, &foreign, &[], &cancel, &cancel,
        &mut || { count += 1; Ok(()) }).is_err());
    document_lease.revoke();
    assert!(effects.with_native_invocation_effect(&document, &candidate, &[], &cancel, &cancel,
        &mut || { count += 1; Ok(()) }).is_err());
    assert_eq!(count, 1);
}

#[test]
fn prepared_dialog_fixture_binds_selection_and_refuses_expired_target() {
    use crate::ipc::{NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision, RecoveryOpenContext, RecoveryOpenTarget};
    let cancel = CancelToken::default();
    let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
        NativeInvocationSelection::RecoveryFile {
            surface: Surface::Slides, path: "old.ppt".into(), decision: NativeRecoveryDecision::Restore,
        }, cancel.clone());
    let created = RendererGeneration::new();
    let (reservation, browser) = closure_reservation(ViewId(830), &created);
    let target = RecoveryOpenTarget::bind(RecoveryOpenContext::NativeStartup(operation),
        ViewId(830), created.clone(), reservation, browser).unwrap();
    let snapshot = target.authority_snapshot().unwrap();
    let selected = recovery_save(&target, &snapshot, || Some("selected.pptx".into())).unwrap().unwrap();
    assert_eq!(selected.path(), std::path::Path::new("selected.pptx"));
    assert_eq!(selected.target().view(), target.view());
    assert!(recovery_save(&target, &snapshot, || None).unwrap().is_none());
    cancel.cancel();
    let called = std::cell::Cell::new(false);
    assert!(recovery_save(&target, &snapshot, || { called.set(true); Some("forbidden".into()) }).is_err());
    assert!(!called.get());
}

#[test]
fn recovery_seed_commit_requires_original_request_constraint() {
    use crate::ipc::{RecoveryOpenContext, RecoveryOpenTarget, RecoveryRequestConstraint};
    let (effects, _, views, _, source) = model();
    let created = RendererGeneration::new();
    views.add(ViewId(2), created.clone());
    let control = CancelToken::default();
    let request = CancelToken::default();
    let (reservation, browser) = closure_reservation(ViewId(2), &created);
    let target = RecoveryOpenTarget::bind_request(
        RecoveryOpenContext::Renderer { source: source.clone(), participants: vec![],
            cancel: control.clone(), pending: control },
        ViewId(2), created.clone(), reservation, browser,
        RecoveryRequestConstraint { source, participants: vec![], cancel: request.clone(), pending: request.clone() },
    ).unwrap();
    request.cancel();
    let mut called = false;
    assert!(effects.commit_recovery_seed(&target, &mut || { called = true; Ok(()) }).is_err());
    assert!(!called);
    assert!(!target.is_seed_committed());
}

#[test]
fn recovery_commit_and_settlement_notify_after_admission_gates_drop() {
    use crate::ipc::{RecoveryOpenContext, RecoveryOpenTarget};
    use std::sync::atomic::{AtomicBool, Ordering};
    let (effects, _, views, _, source) = model();
    let source_generation = source.context.renderer().unwrap().clone();
    let created = RendererGeneration::new();
    views.add(ViewId(2), created.clone());
    let receiver = effects.capture_owner(ViewId(2), &created).unwrap();
    let cancel = CancelToken::default();
    let (reservation, browser) = closure_reservation(receiver.view, &created);
    let target = RecoveryOpenTarget::bind(RecoveryOpenContext::Renderer {
        source, participants: vec![], cancel: cancel.clone(), pending: cancel.clone(),
    }, receiver.view, created.clone(), reservation, browser).unwrap();
    let notified = Arc::new(AtomicBool::new(false));
    let observed = notified.clone();
    let _preparation = target.preparation_loss_token().on_cancel(move || {
        source_generation.revoke();
        observed.store(true, Ordering::SeqCst);
    });
    let receipt = effects.commit_recovery_seed(&target, &mut || Ok(())).unwrap();
    assert!(notified.load(Ordering::SeqCst));
    target.retain_native_registration(receiver.clone()).unwrap();
    let settled = Arc::new(AtomicBool::new(false));
    let observed = settled.clone();
    let _subscription = receipt.on_settled(move |result| {
        assert_eq!(result, Err("load failed".into()));
        created.revoke();
        observed.store(true, Ordering::SeqCst);
    });
    effects.settle_recovery_seed(&receipt, &receiver, &cancel, Err("load failed".into())).unwrap();
    assert!(settled.load(Ordering::SeqCst));
    assert_eq!(receipt.settlement(), Some(Err("load failed".into())));
    assert!(effects.settle_recovery_seed(&receipt, &receiver, &cancel, Ok(())).is_err());
}

#[test]
fn deferred_recovery_notifications_survive_unwind_after_admission() {
    use crate::ipc::{RecoveryOpenContext, RecoveryOpenTarget};
    use std::sync::atomic::{AtomicBool, Ordering};
    let (_, _, _, _, source) = model();
    let generation = source.context.renderer().unwrap().clone();
    let cancel = CancelToken::default();
    let created = RendererGeneration::new();
    let (reservation, browser) = closure_reservation(ViewId(2), &created);
    let target = RecoveryOpenTarget::bind(RecoveryOpenContext::Renderer {
        source, participants: vec![], cancel: cancel.clone(), pending: cancel,
    }, ViewId(2), created.clone(), reservation, browser).unwrap();
    let notified = Arc::new(AtomicBool::new(false));
    let observed = notified.clone();
    let reenter = generation.clone();
    let _sub = target.preparation_loss_token().on_cancel(move || {
        reenter.revoke();
        observed.store(true, Ordering::SeqCst);
    });
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut notification = DeferredPreparation(None);
        generation.with_active(|| {
            target.commit_seed_from_admitted(|| Ok(())).unwrap();
            notification.0 = Some(target.preparation_loss_token());
            panic!("after committed seed");
        });
    }));
    assert!(unwind.is_err());
    assert!(notified.load(Ordering::SeqCst));
    let receipt = target.committed_seed().unwrap();
    let notified = Arc::new(AtomicBool::new(false));
    let observed = notified.clone();
    let generation = RendererGeneration::new();
    let reenter = generation.clone();
    let _sub = receipt.on_settled(move |_| {
        reenter.revoke();
        observed.store(true, Ordering::SeqCst);
    });
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut notification = DeferredSettlement(None);
        generation.with_active(|| {
            notification.0 = Some(receipt.settle_from_admitted(Ok(())).unwrap());
            panic!("after committed settlement");
        });
    }));
    assert!(unwind.is_err());
    assert!(notified.load(Ordering::SeqCst));
    assert_eq!(receipt.settlement(), Some(Ok(())));
}

#[test]
fn native_operation_fixture_requires_explicit_origin_and_every_lifetime() {
    use crate::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    let (effects, _, _, events, plain) = model();
    let generation = plain.context.renderer().unwrap().clone();
    let origin = effects.capture_invocation_root(plain.view, &generation, &SyntheticNativeOrigin::new()).unwrap();
    let shutdown = CancelToken::default();
    let issuer = NativeInvocationIssuer::new(shutdown.clone());
    let operation = issuer.issue_startup(vec![], false, origin.clone(), CancelToken::default());
    let unbound = issuer.issue(NativeInvocationSelection::StartupLaunch { paths: vec![], reopened_by_installer: false }, CancelToken::default());
    let recovery = issuer.issue(NativeInvocationSelection::RecoveryFile {
        surface: Surface::Shell, path: "copy".into(), decision: crate::ipc::NativeRecoveryDecision::Restore,
    }, CancelToken::default());
    let cancel = CancelToken::default();
    let mut calls = 0;
    assert!(effects.with_native_operation_effect(&unbound, &origin, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    assert!(effects.with_native_operation_effect(&recovery, &origin, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    let foreign = effects.capture_invocation_root(plain.view, &generation, &SyntheticNativeOrigin::new()).unwrap();
    assert!(effects.with_native_operation_effect(&operation, &foreign, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    let mut different_document = origin.clone();
    different_document.context = different_document.context.with_document_lease(RendererGeneration::new());
    assert!(effects.with_native_operation_effect(&operation, &different_document, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    assert!(effects.with_native_operation_effect(&operation, &plain, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    effects.with_native_operation_effect(&operation, &origin, &[origin.clone()], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).unwrap();
    effects.emit_native_operation_many(&operation, &origin, &[origin.clone()], 0, &cancel, &cancel,
        "generate:state", vec![IpcValue::Undefined]).unwrap();
    assert_eq!(events.sent.lock().unwrap().len(), 1);
    shutdown.cancel();
    assert!(effects.with_native_operation_effect(&operation, &origin, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    assert_eq!(calls, 1);
}

#[test]
fn startup_recovery_requires_both_operation_lifetimes_and_registered_child() {
    use crate::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    let (effects, _, _, events, plain) = model();
    let generation = plain.context.renderer().unwrap().clone();
    let owner = effects.capture_invocation_root(plain.view, &generation, &SyntheticNativeOrigin::new()).unwrap();
    let shutdown = CancelToken::default();
    let issuer = NativeInvocationIssuer::new(shutdown);
    let startup_cancel = CancelToken::default();
    let startup = issuer.issue_startup(vec![], false, owner.clone(), startup_cancel.clone());
    let child_cancel = CancelToken::default();
    let selection = NativeInvocationSelection::RecoveryFile {
        surface: Surface::Shell, path: "copy".into(), decision: crate::ipc::NativeRecoveryDecision::Restore,
    };
    let operation = issuer.issue_recovery_child(&startup, selection.clone(), child_cancel.clone()).unwrap();
    let unrelated = issuer.issue(selection, CancelToken::default());
    let unrelated_target = unrelated.reserve_target(owner.view, generation.clone()).unwrap();
    let unrelated_child = register_native_invocation_fixture(&unrelated_target, owner.clone()).unwrap();
    let target = operation.reserve_target(owner.view, generation).unwrap();
    let child = register_native_invocation_fixture(&target, owner.clone()).unwrap();
    let cancel = CancelToken::default();
    let mut calls = 0;
    assert!(effects.with_startup_recovery_effect(&startup, &owner, &unrelated_child, &owner, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    effects.with_startup_recovery_effect(&startup, &owner, &child, &owner, &[owner.clone(), owner.clone()], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).unwrap();
    effects.emit_startup_recovery_many(&startup, &owner, &child, &owner, &[owner.clone()], 0, &cancel, &cancel,
        "generate:state", vec![IpcValue::Undefined]).unwrap();
    assert_eq!(events.sent.lock().unwrap().len(), 1);
    assert!(effects.emit_startup_recovery_many(&startup, &owner, &child, &owner, &[owner.clone()], 1, &cancel, &cancel,
        "generate:state", vec![]).is_err());
    child_cancel.cancel();
    assert!(effects.with_startup_recovery_effect(&startup, &owner, &child, &owner, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    assert_eq!(calls, 1);
}

#[test]
fn startup_recovery_creation_and_seed_require_the_selected_parent_and_reservation() {
    use crate::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    let (effects, _, _, _, plain) = model();
    let created = plain.context.renderer().unwrap().clone();
    let owner = effects.capture_invocation_root(plain.view, &created, &SyntheticNativeOrigin::new()).unwrap();
    let shutdown = CancelToken::default();
    let issuer = NativeInvocationIssuer::new(shutdown);
    let cancel = CancelToken::default();
    let startup = issuer.issue_startup(vec![], false, owner.clone(), cancel.clone());
    let other = issuer.issue_startup(vec![], false, owner.clone(), cancel.clone());
    let selection = NativeInvocationSelection::RecoveryBlank { surface: Surface::Docs, title: "Recovery".into() };
    let child = issuer.issue_recovery_child(&startup, selection.clone(), CancelToken::default()).unwrap();
    let unrelated = issuer.issue_recovery_child(&other, selection, CancelToken::default()).unwrap();
    let mut effects_seen = 0;
    assert!(effects.with_startup_recovery_creation_effect(&startup, &owner, &unrelated, &created, &[], &cancel, &cancel,
        &mut || { effects_seen += 1; Ok(()) }).is_err());
    effects.with_startup_recovery_creation_effect(&startup, &owner, &child, &created, &[], &cancel, &cancel,
        &mut || { effects_seen += 1; Ok(()) }).unwrap();
    let target = child.reserve_target(owner.view, created).unwrap();
    effects.with_startup_recovery_seed_effect(&startup, &owner, &target, &[], &cancel, &cancel,
        &mut || { effects_seen += 1; Ok(()) }).unwrap();
    let pending = CancelToken::default();
    pending.cancel();
    assert!(effects.with_startup_recovery_seed_effect(&startup, &owner, &target, &[], &cancel, &pending,
        &mut || { effects_seen += 1; Ok(()) }).is_err());
    child.finish();
    assert!(effects.with_startup_recovery_seed_effect(&startup, &owner, &target, &[], &cancel, &cancel,
        &mut || { effects_seen += 1; Ok(()) }).is_err());
    assert_eq!(effects_seen, 2);
}

#[test]
fn created_many_rejects_lost_created_lifetime_without_running_effect() {
    let (effects, _, _, _, owner) = model();
    let created = RendererGeneration::new();
    let cancel = CancelToken::default();
    let mut calls = 0;
    effects.with_created_many_effect(&owner, &[owner.clone()], &created, &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).unwrap();
    created.revoke();
    assert!(effects.with_created_many_effect(&owner, &[owner.clone()], &created, &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    assert_eq!(calls, 1);
}

#[test]
fn native_command_gates_the_exact_selection_target_and_pending_lifetime() {
    use crate::ipc::{NativeEditorCommand, NativeInvocationIssuer, NativeInvocationSelection};
    let (effects, _, _, _, owner) = model();
    let generation = owner.context.renderer().unwrap().clone();
    let shutdown = CancelToken::default();
    let cancel = CancelToken::default();
    let issuer = NativeInvocationIssuer::new(shutdown.clone());
    let command = NativeEditorCommand::OpenFile { surface: crate::Surface::Docs, path: "selected.docx".into() };
    let other = NativeEditorCommand::OpenFile { surface: crate::Surface::Docs, path: "other.docx".into() };
    let operation = issuer.issue_command(command.clone(), cancel.clone());
    let forged = issuer.issue(NativeInvocationSelection::NativeCommand(command.clone()), cancel.clone());
    let mut calls = 0;
    assert!(effects.with_native_command_creation_effect(&forged, &command, &generation, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    assert!(effects.with_native_command_creation_effect(&operation, &other, &generation, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    effects.with_native_command_creation_effect(&operation, &command, &generation, &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).unwrap();
    let target = operation.reserve_target(owner.view, generation).unwrap();
    let document = register_native_invocation_fixture(&target, owner.clone()).unwrap();
    let pending = CancelToken::default();
    effects.with_native_command_target_effect(&operation, &command, &document, &owner, &[], &cancel, &pending,
        &mut || { calls += 1; Ok(()) }).unwrap();
    pending.cancel();
    assert!(effects.with_native_command_target_effect(&operation, &command, &document, &owner, &[], &cancel, &pending,
        &mut || { calls += 1; Ok(()) }).is_err());
    shutdown.cancel();
    assert!(effects.with_native_command_creation_effect(&operation, &command, target.created(), &[], &cancel, &cancel,
        &mut || { calls += 1; Ok(()) }).is_err());
    assert_eq!(calls, 2);
}

#[test]
fn existing_document_save_uses_the_exact_document_and_live_controls() {
    use crate::ipc::{NativeEditorCommand, NativeInvocationIssuer, NativeSaveSelection};
    let (effects, ui, views, events, mut owner) = model();
    let document = RendererGeneration::new();
    owner.context = owner.context.with_document_lease(document.clone());
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer.issue_existing_document_save(Surface::Docs, owner.view, NativeSaveSelection::ExactPath("chosen.docx".into()), owner.clone(), CancelToken::default()).unwrap();
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let mut count = 0;
    effects.with_existing_document_save_effect(&binding, &[], &cancel, &pending, &mut || { count += 1; Ok(()) }).unwrap();
    effects.emit_existing_document_save_many(&binding, &[owner.clone()], 0, &cancel, &pending, "generate:state", vec![IpcValue::String("saved".into())]).unwrap();
    assert_eq!(events.sent.lock().unwrap().len(), 1);
    let rejected = CancelToken::default();
    rejected.cancel();
    assert!(effects.with_existing_document_save_effect(&binding, &[], &rejected, &pending, &mut || { count += 1; Ok(()) }).is_err());
    assert!(effects.with_existing_document_save_effect(&binding, &[], &cancel, &rejected, &mut || { count += 1; Ok(()) }).is_err());
    let command = NativeEditorCommand::SaveDocument { surface: Surface::Docs, view: owner.view, selection: NativeSaveSelection::ExactPath("chosen.docx".into()) };
    assert!(effects.with_native_command_preparation_effect(binding.operation(), &command, &[], &cancel, &pending, &mut || { count += 1; Ok(()) }).is_err());
    assert!(effects.with_native_command_creation_effect(binding.operation(), &command, owner.context.renderer().unwrap(), &[], &cancel, &pending, &mut || { count += 1; Ok(()) }).is_err());
    let result = Arc::new(Mutex::new(None));
    let completed = result.clone();
    effects.post_ui_existing_document_save(binding.clone(), vec![], cancel.clone(), pending.clone(), Box::new(|| Ok(())),
        Box::new(move |value| *completed.lock().unwrap() = Some(value)));
    document.revoke();
    ui.drain();
    assert!(result.lock().unwrap().as_ref().unwrap().is_err());
    assert!(effects.with_existing_document_save_effect(&binding, &[], &cancel, &pending, &mut || { count += 1; Ok(()) }).is_err());
    views.replace(owner.view);
    assert!(effects.emit_existing_document_save_many(&binding, &[owner], 0, &cancel, &pending, "generate:state", vec![]).is_err());
    binding.operation().finish();
    assert!(effects.with_existing_document_save_effect(&binding, &[], &cancel, &pending, &mut || { count += 1; Ok(()) }).is_err());
    assert_eq!(count, 1);
    assert_eq!(events.sent.lock().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn stale_actor_revision_cannot_truncate_native_save() {
    use crate::ipc::{NativeInvocationIssuer, NativeSaveDestination, NativeSaveSelection};
    use std::{fs::{self, File}, time::{SystemTime, UNIX_EPOCH}};
    let dir = std::env::temp_dir().join(format!("synthetic-save-revision-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("output.pdf");
    fs::write(&path, b"original").unwrap();
    let (effects, _, _, _, mut owner) = model();
    owner.context = owner.context.with_document_lease(RendererGeneration::new());
    let binding = NativeInvocationIssuer::new(CancelToken::default()).issue_existing_document_save(
        Surface::Pdf, owner.view, NativeSaveSelection::ExactPath(path.clone()), owner, CancelToken::default()).unwrap();
    let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
    let parent = File::open(&dir).unwrap();
    let prepared = file_commit::prepare_direct(&parent, &path).unwrap();
    let mut validated = false;
    let rejected = effects.begin_native_direct_write_validated(&selection, &[], &CancelToken::default(),
        &CancelToken::default(), &[], prepared, &mut || { validated = true; Err("stale revision".into()) });
    assert!(validated);
    assert!(matches!(rejected, Err(crate::ipc::DirectWriteStartError::Admission(_))));
    assert_eq!(fs::read(&path).unwrap(), b"original");
    let directory = file_commit::PrivateDirectory::create(&parent).unwrap();
    let pending = directory.create_stage(0o600).unwrap();
    let written = match pending.write_owned(b"replacement".to_vec()) {
        Ok(written) => written, Err((error, _, _)) => panic!("{error}"),
    };
    let written = match written.quarantine() { Ok(written) => written, Err((error, _)) => panic!("{error}") };
    let stage = match written.prepare(&parent, &std::ffi::CString::new("output.pdf").unwrap(),
        &path, file_commit::PublicationMode::Replace) {
        Ok(stage) => stage, Err((error, _)) => panic!("{error}"),
    };
    let mut validated = false;
    let rejected = effects.publish_native_staged_write_validated(&selection, &[], &CancelToken::default(),
        &CancelToken::default(), &[], stage, &mut || { validated = true; Err("stale revision".into()) });
    assert!(validated);
    let stage = match rejected {
        Err(crate::ipc::StagedWriteError::Admission { stage, .. }) => stage,
        _ => panic!("stale stage was not retained"),
    };
    assert_eq!(fs::read(&path).unwrap(), b"original");
    let (stage, _) = stage.into_parts();
    assert!(stage.remove().is_ok());
    directory.remove().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn native_reload_retires_both_pdf_leases_before_deferred_notification() {
    use crate::hosts::{CommittedPdfReload, PreparedAcceptedPdfReload};
    use crate::ipc::{NativeInvocationIssuer, NativeSaveDestination, NativeSaveSelection};
    use std::{fs::{self, File}, time::{SystemTime, UNIX_EPOCH}};
    let dir = std::env::temp_dir().join(format!("synthetic-reload-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("output.pdf");
    use std::sync::atomic::{AtomicBool, Ordering};
    let (effects, _, views, _, mut old) = model();
    let slot = PdfDocumentLease::new();
    let revision = PdfDocumentLease::new();
    old.context = old.context.with_document_lease(RendererGeneration::new())
        .with_pdf_document(slot.clone()).with_pdf_revision(revision.clone());
    let binding = NativeInvocationIssuer::new(CancelToken::default()).issue_existing_document_save(
        Surface::Pdf, old.view, NativeSaveSelection::ExactPath(path.clone()), old.clone(), CancelToken::default()).unwrap();
    let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
    let write_cancel = CancelToken::default();
    let write_pending = CancelToken::default();
    let write = effects.begin_native_direct_write(&selection, &[], &write_cancel, &write_pending, &[],
        file_commit::prepare_direct(&File::open(&dir).unwrap(), &path).unwrap()).unwrap();
    let written = match write.write_owned(b"synthetic pdf".to_vec()) { Ok(written) => written, Err(_) => panic!("fixture write failed") };
    let (publication, snapshot) = match written.finish(&effects) {
        crate::ipc::DirectWriteCompletion::Committed { publication, snapshot } => (publication, snapshot),
        _ => panic!("fixture publication failed"),
    };
    let input = crate::ipc::NativeSavedInput::from_written_snapshot(publication.clone(), snapshot).unwrap();
    let next_generation = RendererGeneration::new();
    let saved_path = crate::ipc::NativeSavedPath::Selected(publication);
    let make_prepared = || PreparedAcceptedPdfReload::new(
        input.clone(), saved_path.clone(), old.clone(), next_generation.clone(),
        CancelToken::default(), CancelToken::default(), Arc::new(|_| true),
        vec![old.clone()], CancelToken::default(), CancelToken::default(), Vec::new(),
    ).unwrap();
    let mut prepared = make_prepared();
    let mut accepted = None;
    effects.with_saved_reload_acceptance(&input, &saved_path, &[old.clone()], &CancelToken::default(),
        &CancelToken::default(), &[], &mut |permit| {
            accepted = Some(prepared.commit(permit)?);
            Ok(())
        }).unwrap();
    let (marker, handoff) = accepted.unwrap();
    let mut other_prepared = make_prepared();
    let mut other_accepted = None;
    effects.with_saved_reload_acceptance(&input, &saved_path, &[old.clone()], &CancelToken::default(),
        &CancelToken::default(), &[], &mut |permit| {
            other_accepted = Some(other_prepared.commit(permit)?);
            Ok(())
        }).unwrap();
    let (_, unchecked) = other_accepted.unwrap();
    views.add(old.view, next_generation.clone());
    let mut successor = effects.capture_owner(old.view, &next_generation).unwrap();
    successor.context = successor.context.with_document_lease(RendererGeneration::new())
        .with_pdf_document(PdfDocumentLease::new()).with_pdf_revision(PdfDocumentLease::new());
    let loaded = CommittedPdfReload::from_completed_native_load(successor.clone(), marker.clone()).unwrap();
    assert!(unchecked.complete(loaded).is_err());
    let notified = Arc::new(AtomicBool::new(false));
    let observed = notified.clone();
    let _subscription = slot.loss_token().on_cancel(move || { observed.store(true, Ordering::SeqCst); });
    let before = notified.clone();
    effects.with_native_save_reload_completion_effect(&handoff, &successor,
        &mut || assert!(!before.load(Ordering::SeqCst)),
        &mut |retirement| retirement.retire()).unwrap();
    let mut parser_calls = 0;
    let stopped = CancelToken::default();
    stopped.cancel();
    assert!(effects.with_native_save_reload_parser_effect(&handoff, &successor, &stopped, &mut |_| { parser_calls += 1; Ok(()) }).is_err());
    effects.with_native_save_reload_parser_effect(&handoff, &successor, &CancelToken::default(), &mut |input| {
        assert_eq!(input.bytes(), b"synthetic pdf");
        parser_calls += 1;
        Ok(())
    }).unwrap();
    assert_eq!(parser_calls, 1);
    let loaded = CommittedPdfReload::from_completed_native_load(successor.clone(), marker).unwrap();
    assert!(handoff.complete(loaded).is_ok());
    assert!(!slot.is_live());
    assert!(!revision.is_live());
    assert!(notified.load(Ordering::SeqCst));
    assert!(slot.with_live(|| ()).is_none());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_selection_swap_fences_old_snapshot_before_loss_notification() {
    use crate::editors::SaveMode;
    use crate::ipc::{NativeInvocationIssuer, NativeSaveDestination, NativeSaveRevisionAuthority, NativeSaveSelection};
    let (effects, _, _, events, mut owner) = model();
    owner.context = owner.context.with_document_lease(RendererGeneration::new());
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer.issue_existing_document_save(Surface::Docs, owner.view,
        NativeSaveSelection::Mode(SaveMode::SaveAs), owner.clone(), CancelToken::default()).unwrap();
    let first = NativeSaveDestination::from_native_save_dialog(binding.clone(), "first.docx".into()).unwrap();
    let next = NativeSaveDestination::from_native_save_dialog(binding, "second.docx".into()).unwrap();
    let authority = NativeSaveRevisionAuthority::new(first, Vec::new());
    let snapshot = authority.snapshot().unwrap();
    let foreign = issuer.issue_existing_document_save(Surface::Docs, owner.view,
        NativeSaveSelection::Mode(SaveMode::SaveAs), owner.clone(), CancelToken::default()).unwrap();
    let foreign = NativeSaveDestination::from_native_save_dialog(foreign, "other.docx".into()).unwrap();
    assert!(snapshot.prepare_selection(foreign).is_err());
    let mut prepared = snapshot.prepare_selection(next).unwrap();
    let cancel = CancelToken::default();
    let rejected = CancelToken::default();
    rejected.cancel();
    assert!(effects.with_native_save_selection_transition(&mut prepared, &[], &cancel, &cancel, &[&rejected],
        &mut || {}, &mut |permit, prepared| { prepared.commit(permit)?; Ok(()) }).is_err());
    assert!(!prepared.is_committed());
    let mut before_called = false;
    effects.with_native_save_selection_transition(&mut prepared, &[], &cancel, &cancel, &[],
        &mut || {
            before_called = true;
            assert!(!snapshot.loss_token().is_cancelled());
            assert!(effects.with_native_save_revision_snapshot_effect(&snapshot, &[], &cancel, &cancel, &[], &mut || Ok(())).is_err());
            assert!(effects.emit_native_save_revision_snapshot(&snapshot, &[owner.clone()], 0, &cancel, &cancel, &[],
                "generate:state", vec![]).is_err());
        }, &mut |permit, prepared| { prepared.commit(permit)?; Ok(()) }).unwrap();
    assert!(before_called);
    assert!(snapshot.loss_token().is_cancelled());
    assert!(events.sent.lock().unwrap().is_empty());
    assert!(effects.with_native_save_selection_transition(&mut prepared, &[], &cancel, &cancel, &[],
        &mut || {}, &mut |permit, prepared| { prepared.commit(permit)?; Ok(()) }).is_err());
}

#[test]
fn native_save_revision_retires_aliases_and_non_renderer_pairs_atomically() {
    use crate::ipc::{NativeInvocationIssuer, NativeSaveDestination, NativeSaveRevisionAuthority, NativeSaveSelection};
    use std::sync::atomic::{AtomicBool, Ordering};
    let (effects, _, views, _, mut owner) = model();
    let old = PdfDocumentLease::new();
    let raw_old = PdfDocumentLease::new();
    let next = PdfDocumentLease::new();
    let raw_next = PdfDocumentLease::new();
    owner.context = owner.context.with_document_lease(RendererGeneration::new()).with_pdf_revision(old.clone());
    let callback_generation = RendererGeneration::new();
    let callback_view = ViewId(22);
    views.add(callback_view, callback_generation.clone());
    let mut callback = effects.capture_owner(callback_view, &callback_generation).unwrap();
    callback.context = callback.context.with_pdf_revision(old.clone());
    let binding = NativeInvocationIssuer::new(CancelToken::default()).issue_existing_document_save(
        Surface::Pdf, owner.view, NativeSaveSelection::ExactPath("output.pdf".into()), owner.clone(), CancelToken::default()).unwrap();
    let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
    let authority = NativeSaveRevisionAuthority::new(selection, vec![owner.clone(), owner.clone()]);
    let snapshot = authority.snapshot().unwrap();
    let pairs = vec![(old.clone(), next.clone()), (raw_old.clone(), raw_next.clone())];
    let mut prepared = snapshot.prepare_with_request_aliases(pairs.clone(), &[callback.clone()]).unwrap();
    let mut stale = snapshot.prepare(pairs).unwrap();
    let notified = Arc::new(AtomicBool::new(false));
    let observed = notified.clone();
    let _subscription = snapshot.loss_token().on_cancel(move || { observed.store(true, Ordering::SeqCst); });
    let before = notified.clone();
    let cancel = CancelToken::default();
    effects.with_native_save_revision_transition(&mut prepared, &[owner.clone()], &cancel, &cancel, &[],
        &mut || assert!(!before.load(Ordering::SeqCst)),
        &mut |admitted, prepared| { prepared.commit(admitted)?; Ok(()) }).unwrap();
    let renewed = prepared.renewed_request_owner(&callback).unwrap();
    assert!(renewed.pdf_revision().unwrap().same_document(&next));
    assert!(authority.snapshot().unwrap().aliases().iter().all(|owner| owner.view != callback_view));
    assert!(!old.is_live());
    assert!(!raw_old.is_live());
    assert!(next.is_live());
    assert!(raw_next.is_live());
    assert!(notified.load(Ordering::SeqCst));
    assert!(effects.with_native_save_revision_transition(&mut stale, &[], &cancel, &cancel, &[],
        &mut || {}, &mut |admitted, prepared| { prepared.commit(admitted)?; Ok(()) }).is_err());
}

#[test]
fn native_save_revision_rejects_cancelled_extra_before_retirement() {
    use crate::ipc::{NativeInvocationIssuer, NativeSaveDestination, NativeSaveRevisionAuthority, NativeSaveSelection};
    let (effects, _, _, _, mut owner) = model();
    let old = PdfDocumentLease::new();
    owner.context = owner.context.with_document_lease(RendererGeneration::new()).with_pdf_revision(old.clone());
    let binding = NativeInvocationIssuer::new(CancelToken::default()).issue_existing_document_save(
        Surface::Pdf, owner.view, NativeSaveSelection::ExactPath("output.pdf".into()), owner, CancelToken::default()).unwrap();
    let authority = NativeSaveRevisionAuthority::new(NativeSaveDestination::from_exact_selection(binding).unwrap(), Vec::new());
    let mut prepared = authority.snapshot().unwrap().prepare(vec![(old.clone(), PdfDocumentLease::new())]).unwrap();
    let cancel = CancelToken::default();
    let extra = CancelToken::default();
    extra.cancel();
    let mut called = false;
    assert!(effects.with_native_save_revision_transition(&mut prepared, &[], &cancel, &cancel, &[&extra],
        &mut || {}, &mut |admitted, prepared| { called = true; prepared.commit(admitted)?; Ok(()) }).is_err());
    assert!(!called);
    assert!(old.is_live());
}

#[test]
fn native_save_revision_rejects_revoked_request_alias_without_retiring() {
    use crate::ipc::{NativeInvocationIssuer, NativeSaveDestination, NativeSaveRevisionAuthority, NativeSaveSelection};
    let (effects, _, views, _, mut owner) = model();
    let old = PdfDocumentLease::new();
    owner.context = owner.context.with_document_lease(RendererGeneration::new()).with_pdf_revision(old.clone());
    let binding = NativeInvocationIssuer::new(CancelToken::default()).issue_existing_document_save(
        Surface::Pdf, owner.view, NativeSaveSelection::ExactPath("output.pdf".into()), owner, CancelToken::default()).unwrap();
    let authority = NativeSaveRevisionAuthority::new(NativeSaveDestination::from_exact_selection(binding).unwrap(), Vec::new());
    let generation = RendererGeneration::new();
    views.add(ViewId(22), generation.clone());
    let callback = effects.capture_owner(ViewId(22), &generation).unwrap();
    let mut prepared = authority.snapshot().unwrap()
        .prepare_with_request_aliases(vec![(old.clone(), PdfDocumentLease::new())], &[callback]).unwrap();
    generation.revoke();
    let cancel = CancelToken::default();
    let mut called = false;
    assert!(effects.with_native_save_revision_transition(&mut prepared, &[], &cancel, &cancel, &[],
        &mut || {}, &mut |admitted, prepared| { called = true; prepared.commit(admitted)?; Ok(()) }).is_err());
    assert!(!called);
    assert!(old.is_live());
}

#[test]
fn native_save_revision_notifies_after_committed_callback_panics() {
    use crate::ipc::{NativeInvocationIssuer, NativeSaveDestination, NativeSaveRevisionAuthority, NativeSaveSelection};
    use std::sync::atomic::{AtomicBool, Ordering};
    let (effects, _, _, _, mut owner) = model();
    let old = PdfDocumentLease::new();
    owner.context = owner.context.with_document_lease(RendererGeneration::new()).with_pdf_revision(old.clone());
    let binding = NativeInvocationIssuer::new(CancelToken::default()).issue_existing_document_save(
        Surface::Pdf, owner.view, NativeSaveSelection::ExactPath("output.pdf".into()), owner, CancelToken::default()).unwrap();
    let authority = NativeSaveRevisionAuthority::new(NativeSaveDestination::from_exact_selection(binding).unwrap(), Vec::new());
    let snapshot = authority.snapshot().unwrap();
    let mut prepared = snapshot.prepare(vec![(old.clone(), PdfDocumentLease::new())]).unwrap();
    let notified = Arc::new(AtomicBool::new(false));
    let observed = notified.clone();
    let _subscription = snapshot.loss_token().on_cancel(move || { observed.store(true, Ordering::SeqCst); });
    let cleaned = Arc::new(AtomicBool::new(false));
    let cleanup = cleaned.clone();
    let cancel = CancelToken::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        effects.with_native_save_revision_transition(&mut prepared, &[], &cancel, &cancel, &[],
            &mut || { cleanup.store(true, Ordering::SeqCst); },
            &mut |admitted, prepared| { prepared.commit(admitted)?; panic!("synthetic actor panic"); })
    }));
    assert!(result.is_err());
    assert!(cleaned.load(Ordering::SeqCst));
    assert!(notified.load(Ordering::SeqCst));
    assert!(!old.is_live());
}

#[test]
fn existing_save_rejects_a_different_native_origin_on_the_same_renderer() {
    use crate::ipc::{NativeInvocationIssuer, NativeSaveSelection};
    let (effects, _, _, _, original) = model();
    let generation = original.context.renderer().unwrap().clone();
    let provenance = SyntheticNativeOrigin::new();
    let mut owner = effects.capture_invocation_root(original.view, &generation, &provenance).unwrap();
    owner.context = owner.context.with_document_lease(RendererGeneration::new());
    let foreign = effects.capture_invocation_root(original.view, &generation, &SyntheticNativeOrigin::new()).unwrap();
    let cancel = CancelToken::default();
    let binding = NativeInvocationIssuer::new(CancelToken::default())
        .issue_existing_document_save(Surface::Docs, owner.view, NativeSaveSelection::Mode(crate::editors::SaveMode::Save), owner.clone(), CancelToken::default()).unwrap();
    let mut calls = 0;
    effects.with_existing_document_same_origin_effect(&binding, &owner, &[], &cancel, &cancel, &[], &mut || { calls += 1; Ok(()) }).unwrap();
    assert!(effects.with_existing_document_same_origin_effect(&binding, &foreign, &[], &cancel, &cancel, &[], &mut || { calls += 1; Ok(()) }).is_err());
    assert_eq!(calls, 1);
}

#[test]
fn existing_document_save_stops_when_only_the_native_operation_finishes() {
    use crate::ipc::{NativeInvocationIssuer, NativeSaveSelection};
    let (effects, _, _, _, mut owner) = model();
    owner.context = owner.context.with_document_lease(RendererGeneration::new());
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer.issue_existing_document_save(Surface::Docs, owner.view, NativeSaveSelection::ExactPath("chosen.docx".into()), owner, CancelToken::default()).unwrap();
    let cancel = CancelToken::default();
    effects.with_existing_document_save_effect(&binding, &[], &cancel, &cancel, &mut || Ok(())).unwrap();
    binding.operation().finish();
    assert!(effects.with_existing_document_save_effect(&binding, &[], &cancel, &cancel, &mut || Ok(())).is_err());
}

#[test]
fn native_generated_path_receipt_retains_selected_binding_and_participants() {
    use crate::ipc::{NativeInvocationIssuer, NativeSaveDestination, NativeSaveSelection};
    let (effects, _, views, _, mut owner) = model();
    let document = RendererGeneration::new();
    owner.context = owner.context.with_document_lease(document.clone());
    let other_generation = RendererGeneration::new();
    views.add(ViewId(2), other_generation.clone());
    let participant = effects.capture_owner(ViewId(2), &other_generation).unwrap();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer.issue_existing_document_save(
        Surface::Docs, owner.view, NativeSaveSelection::ExactPath("chosen.docx".into()),
        owner.clone(), CancelToken::default(),
    ).unwrap();
    let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
    let host: crate::hosts::OwnedEffectsHost = Arc::new(effects);
    let document_receipt = crate::hosts::CommittedGeneratedPath::from_existing_document_save(
        selection.clone(), owner.clone(), Vec::new(), CancelToken::default(), CancelToken::default(), Vec::new(), host.clone(),
    ).unwrap();
    let additional = CancelToken::default();
    let callback_receipt = crate::hosts::CommittedGeneratedPath::from_existing_document_save(
        selection.clone(), owner.clone(), Vec::new(), CancelToken::default(), CancelToken::default(),
        vec![additional.clone()], host.clone(),
    ).unwrap();
    let receipt = crate::hosts::CommittedGeneratedPath::from_existing_document_save(
        selection, owner, vec![participant], CancelToken::default(), CancelToken::default(), Vec::new(), host,
    ).unwrap();
    let mut committed = 0;
    receipt.with_terminal_commit(&mut || { committed += 1; Ok(()) }).unwrap();
    views.replace(ViewId(2));
    assert!(receipt.with_terminal_commit(&mut || { committed += 1; Ok(()) }).is_err());
    additional.cancel();
    assert!(callback_receipt.with_terminal_commit(&mut || Ok(())).is_err());
    assert_eq!(committed, 1);
    document.revoke();
    assert!(document_receipt.with_terminal_commit(&mut || Ok(())).is_err());
}

#[cfg(unix)]
#[test]
fn native_saved_path_admits_only_live_selected_save_controls() {
    use crate::ipc::{NativeInvocationIssuer, NativeSaveDestination, NativeSaveSelection, NativeSavedPath};
    use std::{fs::{self, File}, time::{SystemTime, UNIX_EPOCH}};
    let dir = std::env::temp_dir().join(format!("synthetic-path-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    fs::create_dir(&dir).unwrap();
    let selected = dir.join("chosen.docx");
    let parent = File::open(&dir).unwrap();
    let (effects, _, views, _, mut owner) = model();
    owner.context = owner.context.with_document_lease(RendererGeneration::new());
    let generation = RendererGeneration::new();
    views.add(ViewId(2), generation.clone());
    let participant = effects.capture_owner(ViewId(2), &generation).unwrap();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer.issue_existing_document_save(
        Surface::Docs, owner.view, NativeSaveSelection::ExactPath(selected.clone()),
        owner, CancelToken::default(),
    ).unwrap();
    let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
    let save_cancel = CancelToken::default();
    let save_pending = CancelToken::default();
    let write = effects.begin_native_direct_write(&selection, &[], &save_cancel, &save_pending, &[],
        file_commit::prepare_direct(&parent, &selected).unwrap()).unwrap();
    let written = match write.write_owned(b"saved".to_vec()) { Ok(written) => written, Err(_) => panic!("fixture write failed") };
    let output = match written.finish(&effects) {
        crate::ipc::DirectWriteCompletion::Committed { publication, .. } => publication,
        _ => panic!("fixture publication failed"),
    };
    let path = NativeSavedPath::Selected(output);
    let host: crate::hosts::OwnedEffectsHost = Arc::new(effects);
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let extra = CancelToken::default();
    let mut calls = 0;
    path.with_effect(&host, &[participant.clone()], &cancel, &pending, &[extra.clone()], &mut || {
        calls += 1;
        Ok(())
    }).unwrap();
    let mut plan_calls = 0;
    host.with_prepared_saved_publication(path.publication(), &[participant.clone()], &cancel, &pending, &[],
        &mut |admit| {
            admit(&mut || { plan_calls += 1; Ok(()) })?;
            assert!(admit(&mut || { plan_calls += 1; Ok(()) }).is_err());
            Ok(())
        }).unwrap();
    assert_eq!(plan_calls, 1);
    extra.cancel();
    assert!(path.with_effect(&host, &[participant.clone()], &cancel, &pending, &[extra], &mut || {
        calls += 1;
        Ok(())
    }).is_err());
    save_pending.cancel();
    assert!(path.with_effect(&host, &[participant.clone()], &cancel, &pending, &[], &mut || Ok(())).is_err());
    views.replace(ViewId(2));
    assert!(path.with_effect(&host, &[participant], &cancel, &pending, &[], &mut || {
        calls += 1;
        Ok(())
    }).is_err());
    assert_eq!(calls, 1);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn saved_document_admits_retained_bytes_only_for_exact_live_origin() {
    use std::{fs::{self, File}, time::{SystemTime, UNIX_EPOCH}};
    use crate::ipc::{NativeClosureIssuer, NativeInvocationIssuer, NativeSaveDestination,
        NativeSavedInput, NativeSavedPath, NativeSavedTarget, NativeSaveSelection};
    let dir = std::env::temp_dir().join(format!("synthetic-saved-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("saved.pdf");
    let parent = File::open(&dir).unwrap();
    let (effects, _, views, _, source) = model();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer.issue_existing_document_save(Surface::Docs, source.view,
        NativeSaveSelection::ExactPath(path.clone()), source, CancelToken::default()).unwrap();
    let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
    let save_cancel = CancelToken::default();
    let save_pending = CancelToken::default();
    let write = effects.begin_native_direct_write(&selection, &[], &save_cancel, &save_pending, &[],
        file_commit::prepare_direct(&parent, &path).unwrap()).unwrap();
    let written = match write.write_owned(b"committed".to_vec()) { Ok(written) => written, Err(_) => panic!("fixture write failed") };
    let (output, snapshot) = match written.finish(&effects) {
        crate::ipc::DirectWriteCompletion::Committed { publication, snapshot } => (publication, snapshot),
        _ => panic!("fixture publication failed"),
    };
    let input = NativeSavedInput::from_written_snapshot(output.clone(), snapshot).unwrap();
    fs::rename(&path, dir.join("old.pdf")).unwrap();
    fs::write(&path, b"replacement").unwrap();
    let generation = RendererGeneration::new();
    let view = ViewId(2);
    views.add(view, generation.clone());
    let origin = SyntheticNativeOrigin::new();
    let docs_generation = RendererGeneration::new();
    let _binding = effects.bind_docs_document(view, &generation, 41, docs_generation.clone()).unwrap();
    let owner = effects.capture_invocation_docs(view, &generation, &origin).unwrap();
    let foreign = effects.capture_invocation_root(view, &generation, &SyntheticNativeOrigin::new()).unwrap();
    let (_issuer, reservation, browser) = NativeClosureIssuer::new_with_owner(view, generation.clone());
    let cancel = CancelToken::default();
    let target = NativeSavedTarget::prepare(input, NativeSavedPath::Selected(output), Surface::Pdf, view,
        generation, reservation.clone(), browser.clone(), vec![], cancel.clone(), CancelToken::default(), vec![]).unwrap();
    reservation.with_initial_owner(&browser, |transition| {
        effects.bind_browser_fixture(&owner, &browser, transition)?;
        target.validate_in_transition(transition)?.commit();
        Ok(())
    }).unwrap();
    let document = target.register_from_native_admitted(owner.clone()).unwrap();
    let mut calls = 0;
    effects.with_native_saved_document_same_origin(&document, &owner, &cancel, &[], &cancel, &cancel, &[],
        &mut |input| { assert_eq!(input.bytes(), b"committed"); calls += 1; Ok(()) }).unwrap();
    assert!(effects.with_native_saved_document_same_origin(&document, &foreign, &cancel, &[], &cancel, &cancel, &[],
        &mut |_| { calls += 1; Ok(()) }).is_err());
    let stopped = CancelToken::default();
    stopped.cancel();
    assert!(effects.with_native_saved_document_input(&document, &[], &stopped, &cancel, &[],
        &mut |_| { calls += 1; Ok(()) }).is_err());
    let wrong_stage = SavedStageFixture { identity: (view, 99) };
    assert!(effects.stage_saved_docs_media(&document, &owner, &cancel, &owner, &[], &cancel, &cancel, &[], &wrong_stage).is_err());
    let stage = SavedStageFixture { identity: (view, 41) };
    assert!(effects.stage_saved_docs_media(&document, &owner, &stopped, &owner, &[], &cancel, &cancel, &[], &stage).is_err());
    effects.stage_saved_docs_media(&document, &owner, &cancel, &owner, &[], &cancel, &cancel, &[], &stage).unwrap();
    let transition = SavedMediaFixture { old: docs_generation, id: 41, events: Mutex::new(Vec::new()) };
    let (actual, reply) = SyntheticReplyAuthority::attach(owner.clone(), &cancel);
    assert!(effects.transition_saved_docs_media(&document, &foreign, &cancel, &owner, &[], &cancel, &cancel, &[],
        &transition, &mut |_, commit| commit(), &mut |_, _| Ok(())).is_err());
    assert!(effects.transition_saved_docs_media(&document, &actual, &CancelToken::default(), &owner, &[], &cancel, &cancel, &[],
        &transition, &mut |_, commit| commit(), &mut |_, _| Ok(())).is_err());
    let (renewed, next_actual) = effects.transition_saved_docs_media(&document, &actual, &cancel, &owner, &[], &cancel, &cancel, &[],
        &transition, &mut |_, commit| commit(), &mut |next, next_actual| {
            assert_eq!(effects.docs_document_identity(next.owner()), Some((view, 42)));
            assert_eq!(effects.docs_document_identity(next_actual), Some((view, 42)));
            transition.events.lock().unwrap().push("notified");
            Ok(())
        }).unwrap();
    assert_eq!(*transition.events.lock().unwrap(), vec!["commit", "notified", "loss"]);
    let mut delivered = false;
    reply.deliver(&effects, || delivered = true).unwrap();
    assert!(delivered);
    assert!(reply.deliver(&effects, || panic!("duplicate reply")).is_err());
    assert!(next_actual.context.request_authority::<SyntheticReplyAuthority>().is_some());
    assert!(effects.with_native_saved_document_input(&document, &[], &cancel, &cancel, &[], &mut |_| Ok(())).is_err());
    effects.with_native_saved_document_input(&renewed, &[], &cancel, &cancel, &[], &mut |_| Ok(())).unwrap();
    let second = SavedMediaFixture { old: renewed.owner().context.document_lease().unwrap().clone(), id: 42, events: Mutex::new(Vec::new()) };
    assert!(effects.transition_saved_docs_media(&renewed, &next_actual, &cancel, renewed.owner(), &[], &cancel, &cancel, &[],
        &second, &mut |_, commit| commit(), &mut |_, _| Ok(())).is_err());
    let mut after_error = None;
    let second_cancel = CancelToken::default();
    let (second_actual, second_reply) = SyntheticReplyAuthority::attach(renewed.owner().clone(), &second_cancel);
    assert!(effects.transition_saved_docs_media(&renewed, &second_actual, &second_cancel, renewed.owner(), &[], &cancel, &cancel, &[],
        &second, &mut |_, commit| { commit()?; Err("after commit".into()) }, &mut |next, _| {
            second.events.lock().unwrap().push("notified");
            after_error = Some(next.clone());
            Ok(())
        }).is_err());
    assert_eq!(*second.events.lock().unwrap(), vec!["commit", "notified", "loss"]);
    let after_error = after_error.unwrap();
    second_cancel.cancel();
    assert!(second_reply.deliver(&effects, || panic!("cancelled reply delivered")).is_err());
    let (third_actual, _third_reply) = SyntheticReplyAuthority::attach(after_error.owner().clone(), &cancel);
    let third = SavedMediaFixture { old: after_error.owner().context.document_lease().unwrap().clone(), id: 43, events: Mutex::new(Vec::new()) };
    let mut after_panic = None;
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = effects.transition_saved_docs_media(&after_error, &third_actual, &cancel, after_error.owner(), &[], &cancel, &cancel, &[],
            &third, &mut |_, commit| { commit()?; panic!("after commit") }, &mut |next, _| {
                third.events.lock().unwrap().push("notified");
                after_panic = Some(next.clone());
                Ok(())
            });
    })).is_err());
    assert_eq!(*third.events.lock().unwrap(), vec!["commit", "notified", "loss"]);
    let after_panic = after_panic.unwrap();
    views.replace(view);
    assert!(effects.with_native_saved_document_input(&after_panic, &[], &cancel, &cancel, &[],
        &mut |_| { calls += 1; Ok(()) }).is_err());
    assert_eq!(calls, 1);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
struct SavedMediaFixture {
    old: RendererGeneration,
    id: u64,
    events: Mutex<Vec<&'static str>>,
}

#[cfg(unix)]
impl crate::hosts::DocsMediaTransition for SavedMediaFixture {
    fn lease(&self) -> &(dyn std::any::Any + Send + Sync) { self }
    fn old(&self) -> &(dyn std::any::Any + Send + Sync) { self }
    fn successor(&self) -> &(dyn std::any::Any + Send + Sync) { self }
    fn old_identity(&self) -> (ViewId, u64) { (ViewId(2), self.id) }
    fn successor_identity(&self) -> (ViewId, u64) { (ViewId(2), self.id + 1) }
    fn synthetic_successor(&self) -> Result<(RendererGeneration, Arc<dyn std::any::Any + Send + Sync>), String> {
        Ok((RendererGeneration::new(), Arc::new(())))
    }
    fn synthetic_transition(&self,
        admit: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
        after_unlock: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut committed = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
            admit(&mut || { committed = true; self.events.lock().unwrap().push("commit"); Ok(()) })));
        let notified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(after_unlock));
        if committed {
            self.events.lock().unwrap().push("loss");
            self.old.revoke();
        }
        match (result, notified) {
            (Err(panic), _) | (Ok(_), Err(panic)) => std::panic::resume_unwind(panic),
            (Ok(result), Ok(notified)) => result.and(notified),
        }
    }
}

#[cfg(unix)]
struct SavedStageFixture { identity: (ViewId, u64) }

#[cfg(unix)]
impl crate::hosts::DocsMediaStage for SavedStageFixture {
    fn lease(&self) -> &(dyn std::any::Any + Send + Sync) { self }
    fn old(&self) -> &(dyn std::any::Any + Send + Sync) { self }
    fn old_identity(&self) -> (ViewId, u64) { self.identity }
    fn prior(&self) -> Option<&(dyn std::any::Any + Send + Sync)> { None }
    fn candidate(&self) -> &(dyn std::any::Any + Send + Sync) { self }
    fn synthetic_stage(&self, admit: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>) -> Result<(), String> {
        admit(&mut || Ok(()))
    }
}

#[test]
fn generated_cleanup_fixture_retains_dropped_registrations_until_real_completion() {
    struct Mailbox {
        result: Mutex<Option<Result<crate::hosts::GeneratedPathOutcome, String>>>,
        wake: Mutex<Option<crate::hosts::GeneratedPathWake>>,
        acknowledged: std::sync::atomic::AtomicBool,
    }
    impl crate::hosts::GeneratedTerminalAck for Mailbox {
        fn complete(&self) -> Result<(), String> {
            if self.acknowledged.swap(true, std::sync::atomic::Ordering::SeqCst) { Err("Already acknowledged".into()) } else { Ok(()) }
        }
        fn restore(&self, result: Result<crate::hosts::GeneratedPathOutcome, String>) {
            *self.result.lock().unwrap() = Some(result);
        }
    }
    struct Pending { calls: std::sync::atomic::AtomicUsize, cancelling: std::sync::atomic::AtomicBool, mailbox: Arc<Mailbox> }
    impl crate::hosts::GeneratedPathPending for Pending {
        fn observe(&self, observer: crate::hosts::GeneratedPathObserver, wake: crate::hosts::GeneratedPathWake) -> Result<(), crate::hosts::GeneratedObserveFailure> {
            let mut installed = self.mailbox.wake.lock().unwrap();
            if installed.is_some() { return Err(crate::hosts::GeneratedObserveFailure { error: "Observer already installed".into(), observer, wake }); }
            *installed = Some(wake);
            drop(installed);
            let result = self.mailbox.result.lock().unwrap().take();
            if let Some(result) = result {
                observer(crate::hosts::GeneratedTerminalDelivery::new(result, self.mailbox.clone()));
            }
            Ok(())
        }
        fn wake_observer(&self) -> Result<(), String> {
            let wake = self.mailbox.wake.lock().unwrap().clone().ok_or("Observer is not installed")?;
            wake();
            Ok(())
        }
        fn take_redelivery(&self) -> Result<Option<crate::hosts::GeneratedTerminalDelivery>, String> {
            Ok(self.mailbox.result.lock().unwrap().take().map(|result|
                crate::hosts::GeneratedTerminalDelivery::new(result, self.mailbox.clone())))
        }
        fn retry(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            done(Ok(()));
        }
        fn is_cancelling(&self) -> bool {
            self.cancelling.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn cancel_and_drain(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
            self.cancelling.store(true, std::sync::atomic::Ordering::SeqCst);
            self.calls.fetch_add(10, std::sync::atomic::Ordering::SeqCst);
            done(Ok(()));
        }
    }
    let registry = SyntheticGeneratedCleanupRegistry::default();
    let pending = Arc::new(Pending { calls: std::sync::atomic::AtomicUsize::new(0), cancelling: std::sync::atomic::AtomicBool::new(false), mailbox: Arc::new(Mailbox {
        result: Mutex::new(Some(Ok(crate::hosts::GeneratedPathOutcome::NotOpened))),
        wake: Mutex::new(None), acknowledged: std::sync::atomic::AtomicBool::new(false),
    }) });
    registry.reject_next("unavailable".into());
    assert!(registry.register(pending.clone()).is_err());
    assert!(registry.retained().is_empty());
    let dropped = registry.register(pending.clone()).unwrap();
    let completed = registry.register(pending.clone()).unwrap();
    drop(dropped);
    completed.complete();
    let retained = registry.retained();
    assert_eq!(retained.len(), 1);
    retained[0].observe(Box::new(|delivery| drop(delivery)), Arc::new(|| {})).unwrap_or_else(|_| panic!("observer rejected"));
    let mut delivery = retained[0].take_redelivery().unwrap().expect("dropped delivery restored");
    assert!(matches!(delivery.result(), Ok(crate::hosts::GeneratedPathOutcome::NotOpened)));
    delivery.acknowledge().unwrap();
    assert!(retained[0].take_redelivery().unwrap().is_none());
    retained[0].retry(Box::new(|result| assert!(result.is_ok())));
    retained[0].cancel_and_drain(Box::new(|result| assert!(result.is_ok())));
    assert_eq!(pending.calls.load(std::sync::atomic::Ordering::SeqCst), 11);
}

#[test]
fn prepared_docs_publication_is_invisible_until_exact_adoption() {
    let (effects, _, _, _, owner) = model();
    let renderer = owner.context.renderer().unwrap().clone();
    let mut prepared = effects.prepare_docs_document(owner.view, &renderer, 73, RendererGeneration::new()).unwrap();
    assert_eq!(effects.docs_document_identity(&effects.capture_owner(owner.view, &renderer).unwrap()), None);
    let (_issuer, reservation, browser) = crate::ipc::NativeClosureIssuer::new_with_owner(owner.view, renderer.clone());
    let (_wrong_issuer, wrong, wrong_owner) = crate::ipc::NativeClosureIssuer::new_with_owner(owner.view, renderer.clone());
    assert!(wrong.with_initial_owner(&wrong_owner, |transition| prepared.validate(transition, &reservation).map(|_| ())).is_err());
    let mut retained = None;
    reservation.with_initial_owner(&browser, |transition| {
        retained = Some(prepared.validate(transition, &reservation)?.commit());
        Ok(())
    }).unwrap();
    assert_eq!(effects.docs_document_identity(&effects.capture_owner(owner.view, &renderer).unwrap()), Some((owner.view, 73)));
    drop(retained);
    assert_eq!(effects.docs_document_identity(&effects.capture_owner(owner.view, &renderer).unwrap()), None);
}
