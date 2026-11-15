use super::*;
use std::sync::atomic::{AtomicI32, Ordering};
use shell_contract::{Surface, ViewId, ipc::{NativeContext, NativeInvocationIssuer, NativeInvocationSelection}, runtime::RendererGeneration};
use tauri_runtime_cef::DevToolsProtocol;

static NEXT: AtomicI32 = AtomicI32::new(-1000);

#[test]
fn every_native_invocation_and_print_loss_wakes_the_actual_pending_slot_once() {
    // Only the cancellation transport is synthetic. No CEF document token or
    // native-origin proof is manufactured in this test.
    for expired in 0..10 {
        let shutdown = CancelToken::default();
        let operation_cancel = CancelToken::default();
        let issuer = NativeInvocationIssuer::new(shutdown.clone());
        let operation = issuer.issue(NativeInvocationSelection::HeadlessExport {
            surface: Surface::Sheets, input: "input.xlsx".into(), output: "output.pdf".into(),
            format: "pdf".into(), page: None, ops: None, batch: None,
        }, operation_cancel.clone());
        let created = RendererGeneration::new();
        let invocation = operation.reserve_target(ViewId(901), created.clone()).unwrap();
        let registered = RendererGeneration::new();
        let owner = NativeRequestOwner { view: ViewId(901), context: NativeContext::with_owner_loss((), registered.clone(), registered.loss_token()) };
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let actor = CancelToken::default();
        let stop = CancelToken::default();
        let print = RendererGeneration::new();
        let authority = Authority::Native { invocation, owner, participants: Vec::new(), stop: stop.clone() };
        let id = NEXT.fetch_sub(1, Ordering::Relaxed);
        let (sender, received) = std::sync::mpsc::sync_channel(1);
        super::super::PENDING.0.lock().unwrap().insert(id, super::super::PendingCall { sender, owner: None });
        let _subscriptions = subscribe_losses(authority.losses(&cancel, &pending, &actor, &print).unwrap(), id);
        match expired {
            0 => cancel.cancel(),
            1 => pending.cancel(),
            2 => actor.cancel(),
            3 => print.revoke(),
            4 => registered.revoke(),
            5 => created.revoke(),
            6 => operation_cancel.cancel(),
            7 => shutdown.cancel(),
            8 => operation.finish(),
            _ => stop.cancel(),
        }
        assert!(received.try_recv().unwrap().is_err());
        assert!(!super::super::PENDING.0.lock().unwrap().contains_key(&id));
        super::super::PENDING.complete(DevToolsProtocol::MethodResult { message_id: id, success: true, result: b"{}".to_vec() });
        assert!(received.try_recv().is_err());
    }
}

#[test]
fn already_cancelled_signal_removes_pending_slot_during_subscription() {
    let id = NEXT.fetch_sub(1, Ordering::Relaxed);
    let (sender, received) = std::sync::mpsc::sync_channel(1);
    super::super::PENDING.0.lock().unwrap().insert(id, super::super::PendingCall { sender, owner: None });
    let loss = CancelToken::default();
    loss.cancel();
    let _subscriptions = subscribe_losses(vec![loss], id);
    assert!(received.try_recv().unwrap().is_err());
    assert!(!super::super::PENDING.0.lock().unwrap().contains_key(&id));
}

#[test]
fn every_additional_captured_participant_cancels_pending_transport() {
    for expired in 0..12 {
        let issuer = NativeInvocationIssuer::new(CancelToken::default());
        let operation = issuer.issue(NativeInvocationSelection::HeadlessExport {
            surface: Surface::Docs, input: "input.docx".into(), output: "output.pdf".into(),
            format: "pdf".into(), page: None, ops: None, batch: None,
        }, CancelToken::default());
        let created = RendererGeneration::new();
        let invocation = operation.reserve_target(ViewId(900), created).unwrap();
        let make_owner = |view| {
            let epoch = RendererGeneration::new();
            let owner = NativeRequestOwner { view: ViewId(view), context: NativeContext::with_owner_loss((), epoch.clone(), epoch.loss_token()) };
            (owner, epoch)
        };
        let (owner, _root) = make_owner(900);
        let (participants, epochs): (Vec<_>, Vec<_>) = (1..13).map(make_owner).unzip();
        let authority = Authority::Native { invocation, owner, participants, stop: CancelToken::default() };
        let id = NEXT.fetch_sub(1, Ordering::Relaxed);
        let (sender, received) = std::sync::mpsc::sync_channel(1);
        super::super::PENDING.0.lock().unwrap().insert(id, super::super::PendingCall { sender, owner: None });
        let _subscriptions = subscribe_losses(authority.losses(&CancelToken::default(), &CancelToken::default(),
            &CancelToken::default(), &RendererGeneration::new()).unwrap(), id);
        epochs[expired].revoke();
        assert!(received.try_recv().unwrap().is_err());
        assert!(!super::super::PENDING.0.lock().unwrap().contains_key(&id));
    }
}

#[test]
fn existing_document_print_losses_wake_pending_transport() {
    // Tests cancellation plumbing only; these contexts do not authenticate CEF.
    for expired in 0..9 {
        let shutdown = CancelToken::new();
        let operation_cancel = CancelToken::new();
        let renderer = RendererGeneration::new();
        let document = RendererGeneration::new();
        let owner = NativeRequestOwner {
            view: ViewId(902),
            context: NativeContext::with_owner_loss((), renderer.clone(), renderer.loss_token())
                .with_document_lease(document.clone()),
        };
        let binding = NativeInvocationIssuer::new(shutdown.clone()).issue_existing_document_save(
            Surface::Html, owner.view, shell_contract::ipc::NativeSaveSelection::Mode(shell_contract::editors::SaveMode::SaveAs),
            owner, operation_cancel.clone()).unwrap();
        let cancel = CancelToken::new();
        let pending = CancelToken::new();
        let additional = CancelToken::new();
        let stop = CancelToken::new();
        let print = RendererGeneration::new();
        let authority = Authority::ExistingDocument { binding: binding.clone(), participants: Vec::new(), additional: vec![additional.clone()], stop: stop.clone() };
        let id = NEXT.fetch_sub(1, Ordering::Relaxed);
        let (sender, received) = std::sync::mpsc::sync_channel(1);
        super::super::PENDING.0.lock().unwrap().insert(id, super::super::PendingCall { sender, owner: None });
        let _subscriptions = subscribe_losses(authority.losses(&cancel, &pending, &stop, &print).unwrap(), id);
        match expired {
            0 => cancel.cancel(),
            1 => pending.cancel(),
            2 => additional.cancel(),
            3 => stop.cancel(),
            4 => print.revoke(),
            5 => renderer.revoke(),
            6 => document.revoke(),
            7 => operation_cancel.cancel(),
            _ => shutdown.cancel(),
        }
        assert!(received.try_recv().unwrap().is_err());
        assert!(!super::super::PENDING.0.lock().unwrap().contains_key(&id));
    }
}

#[test]
fn multi_owner_print_callback_cancel_wakes_pending_transport() {
    let renderer = RendererGeneration::new();
    let origin = NativeRequestOwner { view: ViewId(903), context: NativeContext::with_owner_loss((), renderer.clone(), renderer.loss_token()) };
    let callback = CancelToken::new();
    let stop = CancelToken::new();
    let authority = Authority::OwnedMany { origin, participants: Vec::new(), additional: vec![callback.clone()], stop: stop.clone() };
    let id = NEXT.fetch_sub(1, Ordering::Relaxed);
    let (sender, received) = std::sync::mpsc::sync_channel(1);
    super::super::PENDING.0.lock().unwrap().insert(id, super::super::PendingCall { sender, owner: None });
    let _subscriptions = subscribe_losses(authority.losses(&CancelToken::new(), &CancelToken::new(), &stop, &RendererGeneration::new()).unwrap(), id);
    callback.cancel();
    assert!(received.try_recv().unwrap().is_err());
    assert!(!super::super::PENDING.0.lock().unwrap().contains_key(&id));
}
