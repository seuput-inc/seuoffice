use std::sync::mpsc;

use shell_contract::ipc::{
    NativeContext, NativeInvocationIssuer, NativeInvocationSelection, NativeRequestOwner,
};
use shell_contract::runtime::{CancelToken, RendererGeneration};
use shell_contract::{Surface, ViewId};

use super::*;

type Answers = mpsc::Receiver<Result<(), PageError>>;

fn waiter() -> (mpsc::SyncSender<Result<(), PageError>>, Answers) {
    mpsc::sync_channel(1)
}

fn owner_of(view: u32) -> (NativeRequestOwner, RendererGeneration) {
    let epoch = RendererGeneration::new();
    let context = NativeContext::with_owner_loss((), epoch.clone(), epoch.loss_token());
    (
        NativeRequestOwner {
            view: ViewId(view),
            context,
        },
        epoch,
    )
}

fn headless_export(
    issuer: &NativeInvocationIssuer,
    surface: Surface,
    cancel: CancelToken,
) -> shell_contract::ipc::NativeInvocationOperation {
    issuer.issue(
        NativeInvocationSelection::HeadlessExport {
            surface,
            input: "input".into(),
            output: "output.pdf".into(),
            format: "pdf".into(),
            page: None,
            ops: None,
            batch: None,
        },
        cancel,
    )
}

#[test]
fn every_native_invocation_and_print_loss_wakes_the_waiting_call_once() {
    // Only the cancellation transport is synthetic. No document identity or native-origin
    // proof is manufactured in this test.
    for expired in 0..10 {
        let shutdown = CancelToken::default();
        let operation_cancel = CancelToken::default();
        let issuer = NativeInvocationIssuer::new(shutdown.clone());
        let operation = headless_export(&issuer, Surface::Sheets, operation_cancel.clone());
        let created = RendererGeneration::new();
        let invocation = operation
            .reserve_target(ViewId(901), created.clone())
            .unwrap();
        let (owner, registered) = owner_of(901);
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let actor = CancelToken::default();
        let stop = CancelToken::default();
        let print = RendererGeneration::new();
        let authority = Authority::Native {
            invocation,
            owner,
            participants: Vec::new(),
            stop: stop.clone(),
        };
        let (send, received) = waiter();
        let _subscriptions = subscribe_losses(
            authority.losses(&cancel, &pending, &actor, &print).unwrap(),
            &send,
        );
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
        assert_eq!(
            received.try_recv().unwrap(),
            Err(PageError::Cancelled),
            "loss {expired}"
        );
        assert!(received.try_recv().is_err(), "loss {expired} answers once");
    }
}

#[test]
fn an_already_lost_token_answers_during_the_subscription() {
    let (send, received) = waiter();
    let loss = CancelToken::default();
    loss.cancel();
    let _subscriptions = subscribe_losses(vec![loss], &send);
    assert_eq!(received.try_recv().unwrap(), Err(PageError::Cancelled));
}

#[test]
fn every_additional_captured_participant_wakes_the_waiting_call() {
    for expired in 0..12 {
        let issuer = NativeInvocationIssuer::new(CancelToken::default());
        let operation = headless_export(&issuer, Surface::Docs, CancelToken::default());
        let invocation = operation
            .reserve_target(ViewId(900), RendererGeneration::new())
            .unwrap();
        let (owner, _root) = owner_of(900);
        let (participants, epochs): (Vec<_>, Vec<_>) = (1..13).map(owner_of).unzip();
        let authority = Authority::Native {
            invocation,
            owner,
            participants,
            stop: CancelToken::default(),
        };
        let (send, received) = waiter();
        let losses = authority
            .losses(
                &CancelToken::default(),
                &CancelToken::default(),
                &CancelToken::default(),
                &RendererGeneration::new(),
            )
            .unwrap();
        let _subscriptions = subscribe_losses(losses, &send);
        epochs[expired].revoke();
        assert_eq!(
            received.try_recv().unwrap(),
            Err(PageError::Cancelled),
            "owner {expired}"
        );
    }
}

#[test]
fn existing_document_print_losses_wake_the_waiting_call() {
    for expired in 0..9 {
        let shutdown = CancelToken::default();
        let operation_cancel = CancelToken::default();
        let renderer = RendererGeneration::new();
        let document = RendererGeneration::new();
        let owner = NativeRequestOwner {
            view: ViewId(902),
            context: NativeContext::with_owner_loss((), renderer.clone(), renderer.loss_token())
                .with_document_lease(document.clone()),
        };
        let binding = NativeInvocationIssuer::new(shutdown.clone())
            .issue_existing_document_save(
                Surface::Html,
                owner.view,
                shell_contract::ipc::NativeSaveSelection::Mode(
                    shell_contract::editors::SaveMode::SaveAs,
                ),
                owner,
                operation_cancel.clone(),
            )
            .unwrap();
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let additional = CancelToken::default();
        let stop = CancelToken::default();
        let print = RendererGeneration::new();
        let authority = Authority::ExistingDocument {
            binding: binding.clone(),
            participants: Vec::new(),
            additional: vec![additional.clone()],
            stop: stop.clone(),
        };
        let (send, received) = waiter();
        let _subscriptions = subscribe_losses(
            authority.losses(&cancel, &pending, &stop, &print).unwrap(),
            &send,
        );
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
        assert_eq!(
            received.try_recv().unwrap(),
            Err(PageError::Cancelled),
            "loss {expired}"
        );
    }
}

#[test]
fn multi_owner_print_callback_cancel_wakes_the_waiting_call() {
    let (origin, _renderer) = owner_of(903);
    let callback = CancelToken::default();
    let stop = CancelToken::default();
    let authority = Authority::OwnedMany {
        origin,
        participants: Vec::new(),
        additional: vec![callback.clone()],
        stop: stop.clone(),
    };
    let (send, received) = waiter();
    let losses = authority
        .losses(
            &CancelToken::default(),
            &CancelToken::default(),
            &stop,
            &RendererGeneration::new(),
        )
        .unwrap();
    let _subscriptions = subscribe_losses(losses, &send);
    callback.cancel();
    assert_eq!(received.try_recv().unwrap(), Err(PageError::Cancelled));
}

#[test]
fn a_print_caller_reads_cancel_and_stale_as_its_own_messages() {
    assert_eq!(owned_error(PageError::Cancelled), "Print request expired.");
    assert_eq!(
        owned_error(PageError::Stale),
        "Print request is no longer available."
    );
}
