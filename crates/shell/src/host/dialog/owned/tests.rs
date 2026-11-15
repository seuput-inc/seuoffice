use super::*;
use std::sync::atomic::AtomicUsize;
use shell_contract::hosts::FileFilter;

fn tokens() -> [CancelToken; 4] { std::array::from_fn(|_| CancelToken::default()) }

#[test]
fn every_original_lifetime_cancels_the_same_native_session() {
    for expired in 0..4 {
        let tokens = tokens();
        let session = Session::with_tokens(tokens.clone()).unwrap();
        assert!(session.check().is_ok());
        tokens[expired].cancel();
        assert!(session.cancelled());
        assert!(session.check().is_err());
        tokens[expired].cancel();
        assert!(session.cancelled());
    }
}

#[test]
fn loss_before_subscribe_cannot_open_a_native_dialog() {
    for expired in 0..4 {
        let tokens = tokens();
        tokens[expired].cancel();
        let session = Session::with_tokens(tokens).unwrap();
        assert!(session.check().is_err());
    }
}

#[test]
fn cancellation_subscription_does_not_keep_a_completed_session_alive() {
    let tokens = tokens();
    let session = Session::with_tokens(tokens.clone()).unwrap();
    let weak = Arc::downgrade(&session);
    drop(session);
    assert!(weak.upgrade().is_none());
    for token in &tokens { token.cancel(); }
    assert!(weak.upgrade().is_none());
}

#[test]
fn late_cancellation_cannot_close_a_new_dialog_reusing_the_native_address() {
    let mut active = NativeSessions::new();
    let old = Session::with_tokens(tokens()).unwrap();
    let new = Session::with_tokens(tokens()).unwrap();
    assert_ne!(old.id, new.id);
    active.insert(old.id, 42usize).unwrap();
    let queued_cancellation = old.id;
    assert_eq!(active.remove(&old.id), Some(42));
    active.insert(new.id, 42).unwrap();
    assert_eq!(active.get(&queued_cancellation), None);
    assert_eq!(active.get(&new.id), Some(&42));
    assert_eq!(active.remove(&old.id), None, "repeated close acknowledgment is inert");
    assert_eq!(active.remove(&new.id), Some(42));
    assert_eq!(active.len(), 0);
}

#[test]
fn registering_another_target_cannot_replace_the_retained_native_identity() {
    let mut active = NativeSessions::new();
    active.insert(1, 42).unwrap();
    assert!(active.insert(1, 99).is_err());
    assert_eq!(active.get(&1), Some(&42));
}

struct NativeHandle(Arc<AtomicUsize>);
impl Drop for NativeHandle {
    fn drop(&mut self) { self.0.fetch_add(1, Ordering::Relaxed); }
}

#[test]
fn native_handles_are_retained_through_close_and_not_accumulated() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut active = NativeSessions::new();
    for id in 1..=1024 {
        active.insert(id, NativeHandle(drops.clone())).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(drops.load(Ordering::Relaxed), id as usize - 1);
        let native_closed = active.remove(&id).unwrap();
        assert_eq!(active.len(), 0);
        drop(native_closed);
        assert_eq!(drops.load(Ordering::Relaxed), id as usize);
    }
}

#[test]
fn bounded_register_cancel_finish_late_delivery_orders_keep_exact_identity() {
    // These are the same insert/get/remove transitions used by all three
    // native producers, not a native modal-loop execution claim.
    for cancel_before_register in [false, true] {
        for retire_before_delivery in [false, true] {
            let tokens = tokens();
            let session = Session::with_tokens(tokens.clone()).unwrap();
            let mut active = NativeSessions::new();
            if cancel_before_register { tokens[0].cancel(); }
            if session.check().is_ok() { active.insert(session.id, "owned native handle").unwrap(); }
            tokens[0].cancel();
            let queued = session.id;
            if retire_before_delivery { active.remove(&session.id); }
            let cancel_target = active.get(&queued).copied();
            assert_eq!(cancel_target.is_some(), !cancel_before_register && !retire_before_delivery);
            active.remove(&session.id);
            assert!(active.get(&queued).is_none());
            assert_eq!(active.len(), 0);
        }
    }
}

#[test]
fn shared_file_options_keep_default_directory_name_and_wildcard_semantics() {
    let directory = std::env::current_dir().unwrap();
    let options = FileDialog {
        title: Some("Save document".into()),
        default_path: Some(directory.join("owned-dialog-fixture.docx")),
        filters: vec![
            FileFilter { name: "Any".into(), extensions: vec!["*".into()] },
            FileFilter { name: "Empty".into(), extensions: vec![] },
            FileFilter { name: "Document".into(), extensions: vec!["docx".into(), "odt".into()] },
        ],
        properties: vec!["createDirectory".into(), "multiSelections".into(), "openDirectory".into()],
        ..Default::default()
    };
    let spec = FileSpec::of(&options);
    assert_eq!(spec.title.as_deref(), Some("Save document"));
    assert_eq!(spec.directory, Some(directory));
    assert_eq!(spec.name.as_deref(), Some("owned-dialog-fixture.docx"));
    assert_eq!(spec.filters, vec![("Document".into(), vec!["docx".into(), "odt".into()])]);
    assert!(spec.folders && spec.multiple);
    assert_eq!(spec.create_directories, Some(true));
}

fn subscription_owner(id: u32) -> (NativeRequestOwner, shell_contract::runtime::RendererGeneration) {
    // Subscription metadata only. This deliberately carries no native admission
    // capability and is never passed to a native UI effect in these tests.
    let generation = shell_contract::runtime::RendererGeneration::new();
    let owner = NativeRequestOwner {
        view: shell_contract::ViewId(id),
        context: shell_contract::ipc::NativeContext::with_owner_loss((), generation.clone(), generation.loss_token()),
    };
    (owner, generation)
}

#[test]
fn every_participant_beyond_the_original_pair_retains_its_loss_subscription() {
    for expired in 0..13 {
        let (origin, source_generation) = subscription_owner(1);
        let (participants, generations): (Vec<_>, Vec<_>) = (2..14).map(subscription_owner).unzip();
        let session = Session::new_many(&origin, &participants, &CancelToken::default(), &CancelToken::default()).unwrap();
        assert!(session.check().is_ok());
        if expired == 0 { source_generation.revoke(); }
        else { generations[expired - 1].revoke(); }
        assert!(session.cancelled());
        assert!(session.check().is_err());
    }
}

#[test]
fn an_unobservable_last_participant_is_rejected_instead_of_omitted() {
    let (origin, _) = subscription_owner(1);
    let (mut participants, _generations): (Vec<_>, Vec<_>) = (2..14).map(subscription_owner).unzip();
    participants.push(NativeRequestOwner { view: shell_contract::ViewId(14), context: shell_contract::ipc::NativeContext::new(()) });
    assert!(Session::new_many(&origin, &participants, &CancelToken::default(), &CancelToken::default()).is_err());
}

#[test]
fn batch_document_revocation_cancels_the_original_native_session() {
    let (origin, _) = subscription_owner(1);
    let (mut participants, _generations): (Vec<_>, Vec<_>) = (2..14).map(subscription_owner).unzip();
    let document = shell_contract::runtime::RendererGeneration::new();
    let last = participants.last_mut().unwrap();
    last.context = last.context.clone().with_document_lease(document.clone());
    let session = Session::new_many(&origin, &participants, &CancelToken::default(), &CancelToken::default()).unwrap();
    document.revoke();
    assert!(session.cancelled());
}

#[test]
fn parent_closing_cancels_the_original_dialog_session() {
    let session = Session::with_tokens(Vec::new()).unwrap();
    let parent_closing = CancelToken::default();
    session.observe_parent(&parent_closing);
    assert!(session.check().is_ok());
    parent_closing.cancel();
    assert!(session.check().is_err());
}

#[test]
fn renderer_dialog_extra_request_cancellation_closes_the_original_session() {
    let (origin, _) = subscription_owner(1);
    let (participant, _) = subscription_owner(2);
    let extra = CancelToken::default();
    let session = Session::new_many_extra(&origin, &[participant], &CancelToken::default(), &CancelToken::default(), &[extra.clone()]).unwrap();
    assert!(session.check().is_ok());
    extra.cancel();
    assert!(session.check().is_err());
}

#[test]
fn existing_document_dialog_tracks_original_operation_and_all_document_lifetimes() {
    for expired in 0..7 {
        let shutdown = CancelToken::default();
        let operation_cancel = CancelToken::default();
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let extra = CancelToken::default();
        let (mut owner, _) = subscription_owner(1);
        let document = shell_contract::runtime::RendererGeneration::new();
        owner.context = owner.context.clone().with_document_lease(document.clone());
        let (participant, generation) = subscription_owner(2);
        let issuer = shell_contract::ipc::NativeInvocationIssuer::new(shutdown.clone());
        let binding = issuer.issue_existing_document_save(shell_contract::Surface::Docs, owner.view,
            shell_contract::ipc::NativeSaveSelection::ExactPath("document.docx".into()), owner, operation_cancel.clone()).unwrap();
        let session = Session::new_existing_document(&binding, &[participant], &cancel, &pending, &[extra.clone()]).unwrap();
        assert!(session.check().is_ok());
        match expired {
            0 => shutdown.cancel(),
            1 => operation_cancel.cancel(),
            2 => cancel.cancel(),
            3 => pending.cancel(),
            4 => document.revoke(),
            5 => generation.revoke(),
            _ => extra.cancel(),
        }
        assert!(session.check().is_err());
    }
}

#[test]
fn native_revision_dialog_retains_snapshot_and_persistent_alias_lifetimes() {
    for expire_revision in [false, true] {
        let (mut owner, _) = subscription_owner(1);
        let document = shell_contract::runtime::RendererGeneration::new();
        owner.context = owner.context.clone().with_document_lease(document);
        let (alias, alias_generation) = subscription_owner(2);
        let issuer = shell_contract::ipc::NativeInvocationIssuer::new(CancelToken::default());
        let binding = issuer.issue_existing_document_save(shell_contract::Surface::Docs, owner.view,
            shell_contract::ipc::NativeSaveSelection::ExactPath("document.docx".into()), owner, CancelToken::default()).unwrap();
        let selection = shell_contract::ipc::NativeSaveDestination::from_exact_selection(binding).unwrap();
        let authority = shell_contract::ipc::NativeSaveRevisionAuthority::new(selection, vec![alias]);
        let snapshot = authority.snapshot().unwrap();
        let session = Session::new_save_revision(&snapshot, &[], &CancelToken::default(), &CancelToken::default(), &[]).unwrap();
        assert!(session.check().is_ok());
        if expire_revision { snapshot.loss_token().cancel(); } else { alias_generation.revoke(); }
        assert!(session.check().is_err());
    }
}

#[test]
fn recovery_dialog_retains_every_startup_and_creation_lifetime() {
    use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision, RecoveryOpenContext, RecoveryOpenTarget};
    use shell_contract::runtime::RendererGeneration;
    for expired in 0..5 {
        let shutdown = CancelToken::default();
        let cancel = CancelToken::default();
        let created = RendererGeneration::new();
        let (_, closure) = shell_contract::ipc::NativeClosureIssuer::new(shell_contract::ViewId(71), created.clone());
        let issuer = NativeInvocationIssuer::new(shutdown.clone());
        let operation = issuer.issue(NativeInvocationSelection::RecoveryFile {
            surface: shell_contract::Surface::Slides,
            path: "recovery.ppt".into(),
            decision: NativeRecoveryDecision::Restore,
        }, cancel.clone());
        let target = RecoveryOpenTarget::bind(RecoveryOpenContext::NativeStartup(operation.clone()),
            shell_contract::ViewId(71), created.clone(), closure).unwrap();
        let session = Session::new_recovery(&target, &target.authority_snapshot().unwrap()).unwrap();
        assert!(session.check().is_ok());
        match expired {
            0 => shutdown.cancel(),
            1 => cancel.cancel(),
            2 => created.revoke(),
            3 => operation.finish(),
            _ => target.preparation_loss_token().cancel(),
        }
        assert!(session.cancelled());
    }
}

#[test]
fn recovery_dialog_retains_all_renderer_participant_losses() {
    use shell_contract::ipc::{RecoveryOpenContext, RecoveryOpenTarget};
    use shell_contract::runtime::RendererGeneration;
    for expired in 0..5 {
        let (source, source_generation) = subscription_owner(1);
        let (other, other_generation) = subscription_owner(2);
        let created = RendererGeneration::new();
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let (_, closure) = shell_contract::ipc::NativeClosureIssuer::new(shell_contract::ViewId(3), created.clone());
        let target = RecoveryOpenTarget::bind(RecoveryOpenContext::Renderer {
            source, participants: vec![other], cancel: cancel.clone(), pending: pending.clone(),
        }, shell_contract::ViewId(3), created.clone(), closure).unwrap();
        let session = Session::new_recovery(&target, &target.authority_snapshot().unwrap()).unwrap();
        match expired {
            0 => source_generation.revoke(),
            1 => other_generation.revoke(),
            2 => created.revoke(),
            3 => cancel.cancel(),
            _ => pending.cancel(),
        }
        assert!(session.cancelled());
    }
}

#[test]
fn committed_recovery_dialog_tracks_receiver_document_and_pending_loss() {
    use shell_contract::ipc::{RecoveryOpenContext, RecoveryOpenTarget};
    for expired in 0..4 {
        let (source, _) = subscription_owner(1);
        let (mut receiver, generation) = subscription_owner(2);
        let document = shell_contract::runtime::RendererGeneration::new();
        receiver.context = receiver.context.clone().with_document_lease(document.clone());
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let (_, closure) = shell_contract::ipc::NativeClosureIssuer::new(receiver.view, generation.clone());
        let target = RecoveryOpenTarget::bind(RecoveryOpenContext::Renderer {
            source, participants: vec![], cancel: CancelToken::default(), pending: CancelToken::default(),
        }, receiver.view, generation.clone(), closure).unwrap();
        let receipt = target.commit_seed_from_admitted(|| Ok(())).unwrap();
        let session = Session::new_committed(&receipt, &target.authority_snapshot().unwrap(), &receiver, &cancel, &pending).unwrap();
        assert!(session.check().is_ok());
        match expired {
            0 => generation.revoke(),
            1 => document.revoke(),
            2 => cancel.cancel(),
            _ => pending.cancel(),
        }
        assert!(session.check().is_err());
    }
}

#[test]
fn startup_recovery_dialog_retains_request_constraint_losses() {
    use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision, RecoveryOpenContext, RecoveryOpenTarget, RecoveryRequestConstraint};
    use shell_contract::runtime::RendererGeneration;
    for expired in 0..4 {
        let (source, source_generation) = subscription_owner(11);
        let (participant, participant_generation) = subscription_owner(12);
        let cancel = CancelToken::default();
        let pending = CancelToken::default();
        let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
            NativeInvocationSelection::RecoveryFile {
                surface: shell_contract::Surface::Slides, path: "old.ppt".into(),
                decision: NativeRecoveryDecision::Restore,
            }, CancelToken::default());
        let created = RendererGeneration::new();
        let (_, closure) = shell_contract::ipc::NativeClosureIssuer::new(shell_contract::ViewId(13), created.clone());
        let target = RecoveryOpenTarget::bind_request(RecoveryOpenContext::NativeStartup(operation),
            shell_contract::ViewId(13), created, closure, RecoveryRequestConstraint {
                source, participants: vec![participant], cancel: cancel.clone(), pending: pending.clone(),
            }).unwrap();
        let session = Session::new_recovery(&target, &target.authority_snapshot().unwrap()).unwrap();
        match expired {
            0 => source_generation.revoke(),
            1 => participant_generation.revoke(),
            2 => cancel.cancel(),
            _ => pending.cancel(),
        }
        assert!(session.cancelled());
    }
}

#[test]
fn recovery_request_document_loss_cancels_native_session() {
    use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision, RecoveryOpenContext, RecoveryOpenTarget, RecoveryRequestConstraint};
    use shell_contract::runtime::RendererGeneration;
    let (mut source, _) = subscription_owner(21);
    let document = RendererGeneration::new();
    source.context = source.context.clone().with_document_lease(document.clone());
    let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
        NativeInvocationSelection::RecoveryFile {
            surface: shell_contract::Surface::Slides, path: "old.ppt".into(),
            decision: NativeRecoveryDecision::Restore,
        }, CancelToken::default());
    let created = RendererGeneration::new();
    let (_, closure) = shell_contract::ipc::NativeClosureIssuer::new(shell_contract::ViewId(22), created.clone());
    let target = RecoveryOpenTarget::bind_request(RecoveryOpenContext::NativeStartup(operation),
        shell_contract::ViewId(22), created, closure, RecoveryRequestConstraint {
            source, participants: Vec::new(), cancel: CancelToken::default(), pending: CancelToken::default(),
        }).unwrap();
    let session = Session::new_recovery(&target, &target.authority_snapshot().unwrap()).unwrap();
    document.revoke();
    assert!(session.cancelled());
}
