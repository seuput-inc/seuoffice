//! hwp-main.ts and the recovery channels as a renderer and the shell observe
//! them: replies, events, the files written, and what the shell was asked.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract;
use main_core::packages::electron_utils::recovery_store::{RecoveryIdentity, SharedRecoveryState};
use serde_json::json;
use shell_contract::ipc::{Sender, Target};
use shell_contract::{
    BytesKind, DocumentEditors, IpcError, IpcValue, SaveMode, Surface, ViewId, editor_for,
};

use shell_contract::editors::Then;

use super::{HEADLESS_TIMEOUT_MS, HwpEditors, UntitledOption};
use crate::hwp::fake::FakeShell;
use crate::hwp::{HwpShellHost, KeyInput};
use crate::kit::{IntoIpc, as_str, field, obj};
use crate::testing::{HostCall, Rt};

/// OLE compound-file magic: what `bytesMatchFormat(bytes, 'hwp')` checks
const HWP: &[u8] = &[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1, 1, 2, 3];
/// ZIP magic: an HWPX container
const HWPX: &[u8] = &[0x50, 0x4b, 0x03, 0x04, 9, 9];

struct T {
    rt: Rt,
    shell: Arc<FakeShell>,
    editors: Rc<HwpEditors>,
}

fn t(case: serde_json::Value) -> T {
    let shell = Arc::new(FakeShell::new(&case, Default::default()));
    let host = Arc::clone(&shell);
    let mut rt = Rt::new(move |h| {
        crate::hwp::fakes(h, &case);
        h.insert::<HwpShellHost>(host);
        h.insert::<Arc<Mutex<SharedRecoveryState>>>(Arc::new(Mutex::new(
            SharedRecoveryState::new("0123456789abcdef"),
        )));
    });
    rt.register(crate::hwp::entries).unwrap();
    let editors = rt.provided().all::<HwpEditors>().remove(0);
    T { rt, shell, editors }
}

fn s(text: &str) -> IpcValue {
    text.into_ipc()
}

fn bytes(b: &[u8]) -> IpcValue {
    IpcValue::Bytes(BytesKind::Uint8Array, b.to_vec())
}

fn text(v: &IpcValue, key: &str) -> Option<String> {
    as_str(field(v, key)).map(str::to_owned)
}

fn file(dir: &Path, name: &str, content: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path.to_string_lossy().into_owned()
}

fn channels(t: &T) -> Vec<(&'static str, Vec<IpcValue>)> {
    t.rt.events()
        .into_iter()
        .map(|e| (e.channel, e.args))
        .collect()
}

/// a Hangul tab on `path`, as `createHwpView` makes it
fn tab(t: &mut T, id: u32, path: Option<&str>) -> Sender {
    let from = t.rt.view(id, Surface::Hwp);
    t.editors
        .grant_and_track(from.view, path, UntitledOption::No);
    from
}

type Slot<T> = Rc<RefCell<Option<Result<T, IpcError>>>>;

fn slot<T: 'static>() -> (Slot<T>, Then<T>) {
    let slot: Slot<T> = Rc::default();
    let set = Rc::clone(&slot);
    (slot, Box::new(move |r| *set.borrow_mut() = Some(r)))
}

fn owned_origin(t: &T, view: ViewId) -> shell_contract::ipc::NativeRequestOwner {
    let generation = t.rt.services.views.renderer_generation(view).unwrap();
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap();
    t.editors
        .capture_document_owner(view, effects.capture_owner(view, &generation).unwrap())
        .unwrap()
}

#[test]
fn native_hwp_recovery_refuses_a_different_selected_file() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision,
        RecoveryOpenContext, RecoveryOpenTarget,
    };
    let mut t = t(json!({}));
    let original = file(&t.rt.dir, "original.hwpx", HWPX);
    let selected = file(&t.rt.dir, "selected.hwpx", HWPX);
    let from = tab(&mut t, 1, Some(&original));
    let generation = t.rt.services.views.renderer_generation(from.view).unwrap();
    let operation = NativeInvocationIssuer::new(Default::default()).issue(
        NativeInvocationSelection::RecoveryFile {
            surface: Surface::Hwp,
            path: PathBuf::from(&selected),
            decision: NativeRecoveryDecision::Restore,
        },
        Default::default(),
    );
    let (_issuer, closure, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(from.view, generation.clone());
    let target = RecoveryOpenTarget::bind(
        RecoveryOpenContext::NativeStartup(operation),
        from.view,
        generation,
        closure,
        browser,
    )
    .unwrap();
    let result = Rc::new(RefCell::new(None));
    let answer = result.clone();
    t.editors.open_file_with_decision(
        &from.view,
        &RecoveryOpenFileRequest {
            file_path: original.clone(),
            decision: RecoveryOpenDecision::Restore,
        },
        &target,
        Box::new(move |value| *answer.borrow_mut() = Some(value)),
    );
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    assert_eq!(
        t.editors.document_path(from.view),
        Some(PathBuf::from(original))
    );
}

#[test]
fn recovery_receipt_waits_for_the_matching_initialized_document() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision,
        RecoveryOpenContext, RecoveryOpenTarget,
    };
    use shell_contract::runtime::CancelToken;

    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "recovery.hwpx", HWPX);
    let from = tab(&mut t, 1, Some(&path));
    let generation = t.rt.services.views.renderer_generation(from.view).unwrap();
    let owner = t.rt.native_root(from.view);
    let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
        NativeInvocationSelection::RecoveryFile {
            surface: Surface::Hwp,
            path: PathBuf::from(&path),
            decision: NativeRecoveryDecision::Original,
        },
        CancelToken::default(),
    );
    let (_issuer, closure, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(from.view, generation.clone());
    let target = RecoveryOpenTarget::bind(
        RecoveryOpenContext::NativeStartup(operation),
        from.view,
        generation,
        closure,
        browser,
    )
    .unwrap();
    let result = Rc::new(RefCell::new(None));
    let answer = result.clone();
    t.editors.open_file_with_decision(
        &from.view,
        &RecoveryOpenFileRequest {
            file_path: path.clone(),
            decision: RecoveryOpenDecision::Original,
        },
        &target,
        Box::new(move |value| *answer.borrow_mut() = Some(value)),
    );
    assert!(matches!(&*result.borrow(), Some(Ok(()))));
    let receipt = target.committed_seed().unwrap();
    assert!(receipt.settlement().is_none());
    target
        .invocation_target()
        .unwrap()
        .register_from_native_admitted(owner.clone())
        .unwrap();
    target.retain_native_registration(owner.clone()).unwrap();
    let seed =
        t.rt.invoke_native(
            "hwp:consume-recovery",
            from,
            vec![],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap();
    assert_eq!(text(&seed, "path").as_deref(), Some(path.as_str()));
    let token = text(&seed, "token").unwrap();
    let wrong =
        t.rt.invoke_native(
            "hwp:recovery-initialized",
            from,
            vec![s("old"), obj([("ok", true.into_ipc())])],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap();
    assert!(wrong.is_err());
    assert!(receipt.settlement().is_none());
    let premature =
        t.rt.invoke_native(
            "hwp:recovery-initialized",
            from,
            vec![s(&token), obj([("ok", true.into_ipc())])],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap();
    assert!(premature.is_err());
    assert!(receipt.settlement().is_none());
    let read =
        t.rt.invoke_native(
            "hwp:read-file",
            from,
            vec![s(&path)],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap();
    assert_eq!(read, bytes(HWPX));
    assert!(receipt.settlement().is_none());
    let ack =
        t.rt.invoke_native(
            "hwp:recovery-initialized",
            from,
            vec![s(&token), obj([("ok", true.into_ipc())])],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap();
    assert!(ack.is_ok());
    assert!(matches!(receipt.settlement(), Some(Ok(()))));
}

#[test]
fn recovery_rename_rejects_revoked_participant_before_state_change() {
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision,
        RecoveryOpenContext, RecoveryOpenTarget,
    };
    use shell_contract::runtime::CancelToken;
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "recovery-rename.hwpx", HWPX);
    let from = tab(&mut t, 1, Some(&path));
    let other = tab(&mut t, 2, None);
    let generation = t.rt.services.views.renderer_generation(from.view).unwrap();
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap();
    let receiver = effects.capture_owner(from.view, &generation).unwrap();
    let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
        NativeInvocationSelection::RecoveryFile {
            surface: Surface::Hwp,
            path: PathBuf::from(&path),
            decision: NativeRecoveryDecision::Original,
        },
        CancelToken::default(),
    );
    let (_issuer, closure, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(from.view, generation.clone());
    let target = RecoveryOpenTarget::bind(
        RecoveryOpenContext::NativeStartup(operation),
        from.view,
        generation,
        closure,
        browser,
    )
    .unwrap();
    let receipt = effects
        .commit_recovery_seed(&target, &mut || Ok(()))
        .unwrap();
    target
        .invocation_target()
        .unwrap()
        .register_from_native_admitted(receiver.clone())
        .unwrap();
    target.retain_native_registration(receiver.clone()).unwrap();
    let destination = owned_origin(&t, from.view);
    let participant = owned_origin(&t, other.view);
    participant.context.document_lease().unwrap().revoke();
    let new = t.rt.dir.join("renamed.hwpx");
    let (result, done) = slot();
    t.editors.file_renamed_recovery(
        from.view,
        Path::new(&path),
        &new,
        &receipt,
        &receiver,
        &destination,
        &[participant],
        &CancelToken::default(),
        &CancelToken::default(),
        done,
    );
    t.rt.drain();
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    assert_eq!(
        t.editors.document_path(from.view),
        Some(PathBuf::from(path))
    );
}

#[test]
fn hwp_rename_requires_every_captured_document_participant() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "before.hwpx", HWPX);
    let other_path = file(&t.rt.dir, "other.hwpx", HWPX);
    let from = tab(&mut t, 1, Some(&path));
    let other = tab(&mut t, 2, Some(&other_path));
    let owner = owned_origin(&t, from.view);
    let participant = owned_origin(&t, other.view);
    participant.context.document_lease().unwrap().revoke();
    let next = t.rt.dir.join("after.hwpx");
    let (result, then) = slot();
    t.editors.file_renamed_many_owned_extra(
        from.view,
        Path::new(&path),
        &next,
        &owner,
        &owner,
        &[participant],
        &Default::default(),
        &Default::default(),
        &[],
        then,
    );
    t.rt.drain();
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    assert_eq!(
        t.editors.document_path(from.view),
        Some(PathBuf::from(&path))
    );
    assert_eq!(std::fs::read(&path).unwrap(), HWPX);
    assert!(!next.exists());
}

#[test]
fn hwp_rename_rejects_cancelled_additional_scope() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "before-extra.hwpx", HWPX);
    let from = tab(&mut t, 1, Some(&path));
    let owner = owned_origin(&t, from.view);
    let extra = shell_contract::runtime::CancelToken::default();
    extra.cancel();
    let next = t.rt.dir.join("after-extra.hwpx");
    let (result, then) = slot();
    t.editors.file_renamed_many_owned_extra(
        from.view,
        Path::new(&path),
        &next,
        &owner,
        &owner,
        &[],
        &Default::default(),
        &Default::default(),
        &[extra],
        then,
    );
    t.rt.drain();
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    assert_eq!(
        t.editors.document_path(from.view),
        Some(PathBuf::from(&path))
    );
    assert_eq!(std::fs::read(&path).unwrap(), HWPX);
    assert!(!next.exists());
}

#[test]
fn native_hwp_rename_rejects_cancelled_additional_scope() {
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeSaveDestination, NativeSaveRevisionAuthority,
        NativeSaveSelection,
    };
    use shell_contract::runtime::CancelToken;
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "native-before.hwpx", HWPX);
    let from = tab(&mut t, 1, Some(&path));
    let owner = owned_origin(&t, from.view);
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer
        .issue_existing_document_save(
            Surface::Hwp,
            from.view,
            NativeSaveSelection::ExactPath(PathBuf::from(&path)),
            owner.clone(),
            CancelToken::default(),
        )
        .unwrap();
    let authority = NativeSaveRevisionAuthority::new(
        NativeSaveDestination::from_exact_selection(binding).unwrap(),
        Vec::new(),
    );
    let wrong = t.rt.dir.join("wrong-selection.hwpx");
    let (mismatch, mismatch_done) = slot();
    t.editors.file_renamed_existing_document(
        from.view,
        Path::new(&path),
        &wrong,
        &authority,
        &wrong,
        &owner,
        &[],
        &CancelToken::default(),
        &CancelToken::default(),
        &[],
        mismatch_done,
    );
    t.rt.drain();
    assert!(matches!(&*mismatch.borrow(), Some(Err(_))));
    assert_eq!(
        t.editors.document_path(from.view),
        Some(PathBuf::from(&path))
    );
    let extra = CancelToken::default();
    extra.cancel();
    let next = t.rt.dir.join("native-after.hwpx");
    let (result, then) = slot();
    t.editors.file_renamed_existing_document(
        from.view,
        Path::new(&path),
        &next,
        &authority,
        Path::new(&path),
        &owner,
        &[],
        &CancelToken::default(),
        &CancelToken::default(),
        &[extra],
        then,
    );
    t.rt.drain();
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    assert_eq!(
        t.editors.document_path(from.view),
        Some(PathBuf::from(&path))
    );
    assert_eq!(std::fs::read(&path).unwrap(), HWPX);
    assert!(!next.exists());
}

#[test]
fn owned_hwp_save_rejects_another_reply_and_cancellation_before_file_publication() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "owned.hwpx", HWPX);
    let from = tab(&mut t, 1, Some(&path));
    let origin = owned_origin(&t, from.view);
    let generation = origin.context.renderer().unwrap().clone();
    let cancel = shell_contract::runtime::CancelToken::default();
    let (result, then) = slot();
    t.editors
        .request_save_owned(from.view, SaveMode::Save, origin, cancel.clone(), then);
    let events = channels(&t);
    let token = events
        .iter()
        .find(|(channel, _)| *channel == "hwp:save-request")
        .and_then(|(_, args)| args.get(1))
        .and_then(as_str)
        .unwrap()
        .to_owned();
    let native = || shell_contract::ipc::NativeContext::with_renderer((), generation.clone());
    let wrong =
        t.rt.invoke_native(
            "hwp:begin-save",
            from,
            vec![s("save"), s("another-request")],
            native(),
            Default::default(),
        )
        .take()
        .unwrap()
        .unwrap();
    assert_eq!(field(&wrong, "ok"), &IpcValue::Bool(false));
    assert!(result.borrow().is_none());
    let begun =
        t.rt.invoke_native(
            "hwp:begin-save",
            from,
            vec![s("save"), s(&token)],
            native(),
            Default::default(),
        )
        .take()
        .unwrap()
        .unwrap();
    assert_eq!(text(&begun, "path"), Some(path.clone()));
    cancel.cancel();
    t.rt.drain();
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    let committed =
        t.rt.invoke_native(
            "hwp:commit-save",
            from,
            vec![bytes(HWPX), IpcValue::Array(vec![]), obj([]), s(&token)],
            native(),
            Default::default(),
        )
        .take()
        .unwrap()
        .unwrap();
    assert_eq!(field(&committed, "ok"), &IpcValue::Bool(false));
    assert_eq!(std::fs::read(&path).unwrap(), HWPX);
    assert!(!PathBuf::from(format!("{path}.bak")).exists());
}

#[test]
fn document_replacement_invalidates_captured_hwp_save_authority() {
    let mut t = t(json!({}));
    let old = file(&t.rt.dir, "old.hwpx", HWPX);
    let next = file(&t.rt.dir, "next.hwpx", HWPX);
    let from = tab(&mut t, 1, Some(&old));
    let old_owner = owned_origin(&t, from.view);
    let (result, then) = slot();
    t.editors.request_save_owned(
        from.view,
        SaveMode::Save,
        old_owner.clone(),
        Default::default(),
        then,
    );
    t.editors
        .grant_and_track(from.view, Some(&next), UntitledOption::No);
    t.rt.drain();
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    assert!(
        t.editors
            .capture_document_owner(from.view, old_owner)
            .is_err()
    );
    assert_eq!(
        t.editors.document_path(from.view),
        Some(PathBuf::from(next))
    );
    assert_eq!(std::fs::read(old).unwrap(), HWPX);
}

#[test]
fn owned_hwp_close_waits_for_its_save_and_ignores_legacy_boolean_ack() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "close.hwpx", HWPX);
    let from = tab(&mut t, 1, Some(&path));
    t.rt.send("hwp:dirty-changed", from, vec![true.into_ipc()]);
    t.rt.script().message.push_back(0);
    let origin = owned_origin(&t, from.view);
    let cancel = shell_contract::runtime::CancelToken::default();
    let (result, then) = slot();
    t.editors
        .request_close_owned(from.view, None, origin, cancel.clone(), then);
    t.rt.drain();
    assert!(result.borrow().is_none());
    assert!(
        channels(&t)
            .iter()
            .any(|(channel, args)| *channel == "hwp:save-request" && args.len() == 2)
    );
    t.rt.send("hwp:close-save-result", from, vec![true.into_ipc()]);
    assert!(result.borrow().is_none());
    cancel.cancel();
    t.rt.drain();
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    assert_eq!(std::fs::read(path).unwrap(), HWPX);
}

#[test]
fn completing_a_clean_owned_close_does_not_cancel_its_callers_token() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "clean.hwpx", HWPX);
    let from = tab(&mut t, 1, Some(&path));
    let origin = owned_origin(&t, from.view);
    let cancel = shell_contract::runtime::CancelToken::default();
    let (result, then) = slot();
    t.editors
        .request_close_owned(from.view, None, origin, cancel.clone(), then);
    t.rt.drain();
    assert_eq!(*result.borrow(), Some(Ok(true)));
    assert!(!cancel.is_cancelled());
    assert!(channels(&t).is_empty());
}

#[test]
fn claiming_owned_hwp_pdf_transfers_only_the_renderer_waiter_deadline() {
    for claim in [false, true] {
        let mut t = t(json!({}));
        let path = file(&t.rt.dir, "source.hwpx", HWPX);
        let from = tab(&mut t, 1, Some(&path));
        let target = t.rt.dir.join("copy.pdf");
        t.rt.script().save.push_back(Some(target.clone()));
        let origin = owned_origin(&t, from.view);
        let generation = origin.context.renderer().unwrap().clone();
        let (result, then) = slot();
        t.editors.request_save_owned(
            from.view,
            SaveMode::SaveAs,
            origin.clone(),
            Default::default(),
            then,
        );
        let events = channels(&t);
        let token = events
            .iter()
            .find(|(channel, _)| *channel == "hwp:save-request")
            .and_then(|(_, args)| args.get(1))
            .and_then(as_str)
            .unwrap()
            .to_owned();
        let begin =
            t.rt.invoke_native(
                "hwp:begin-save",
                from,
                vec![s("saveAs"), s(&token)],
                shell_contract::ipc::NativeContext::with_renderer((), generation.clone()),
                Default::default(),
            )
            .take()
            .unwrap()
            .unwrap();
        assert_eq!(text(&begin, "export").as_deref(), Some("pdf"));
        let destination = t.editors.0.pending_save_destination(from.view).unwrap();
        let claim_cancel = shell_contract::runtime::CancelToken::default();
        assert!(
            t.editors
                .take_owned_pdf_grant(
                    from.view,
                    &token,
                    &shell_contract::runtime::RendererGeneration::new(),
                    Some(&destination),
                    &claim_cancel
                )
                .is_none()
        );
        assert!(
            t.editors
                .take_owned_pdf_grant(
                    from.view,
                    "foreign",
                    &generation,
                    Some(&destination),
                    &claim_cancel
                )
                .is_none()
        );
        assert!(
            t.editors
                .take_owned_pdf_grant(from.view, &token, &generation, None, &claim_cancel)
                .is_none()
        );
        let foreign = shell_contract::ipc::NativeRequestOwner {
            view: from.view,
            context: shell_contract::ipc::NativeContext::with_renderer((), generation.clone()),
        };
        assert!(
            t.editors
                .take_owned_pdf_grant(
                    from.view,
                    &token,
                    &generation,
                    Some(&foreign),
                    &claim_cancel
                )
                .is_none()
        );
        let cancelled = shell_contract::runtime::CancelToken::default();
        cancelled.cancel();
        assert!(
            t.editors
                .take_owned_pdf_grant(
                    from.view,
                    &token,
                    &generation,
                    Some(&destination),
                    &cancelled
                )
                .is_none()
        );
        if !claim {
            t.rt.advance(Duration::from_secs(121));
            assert!(matches!(&*result.borrow(), Some(Err(_))));
            continue;
        }
        let grant = t
            .editors
            .take_owned_pdf_grant(
                from.view,
                &token,
                &generation,
                Some(&destination),
                &claim_cancel,
            )
            .unwrap();
        assert_eq!(grant.path, target);
        assert!(
            t.editors
                .take_owned_pdf_grant(
                    from.view,
                    &token,
                    &generation,
                    Some(&destination),
                    &claim_cancel
                )
                .is_none()
        );
        t.rt.advance(Duration::from_secs(121));
        assert!(result.borrow().is_none());
        assert!(!grant.pending.is_cancelled());
        (grant.complete)(Ok(()));
        t.rt.drain();
        assert_eq!(*result.borrow(), Some(Ok(false)));
        assert_eq!(
            t.editors.document_path(from.view),
            Some(PathBuf::from(path))
        );
    }
}

#[test]
fn a_view_reads_only_the_file_it_was_given() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    assert_eq!(
        t.rt.invoke_now("hwp:consume-pending", from, vec![]),
        Ok(s(&path))
    );
    // a reload consumes it again
    assert_eq!(
        t.rt.invoke_now("hwp:consume-pending", from, vec![]),
        Ok(s(&path))
    );
    assert_eq!(
        t.rt.invoke_now("hwp:untitled-copy", from, vec![]),
        Ok(IpcValue::Null)
    );

    let other = file(&t.rt.dir, "b.hwp", HWP);
    let refused = t.rt.invoke_now("hwp:read-file", from, vec![s(&other)]);
    assert_eq!(
        refused.unwrap_err().message,
        "hwp: path not granted to this view"
    );
    assert_eq!(
        t.rt.invoke_now("hwp:read-file", from, vec![s(&path)]),
        Ok(IpcValue::Bytes(BytesKind::Uint8Array, HWP.to_vec()))
    );
    assert_eq!(
        t.editors.document_path(from.view),
        Some(PathBuf::from(&path))
    );
}

#[test]
fn a_file_past_the_limit_or_gone_is_refused_with_the_node_message() {
    let mut t = t(json!({}));
    let path = t.rt.dir.join("big.hwp");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(512 * 1024 * 1024 + 1)
        .unwrap();
    let path = path.to_string_lossy().into_owned();
    let from = tab(&mut t, 1, Some(&path));
    let err =
        t.rt.invoke_now("hwp:read-file", from, vec![s(&path)])
            .unwrap_err();
    assert_eq!(err.message, "hwp: file is too large to open");
    std::fs::remove_file(&path).unwrap();
    let err =
        t.rt.invoke_now("hwp:read-file", from, vec![s(&path)])
            .unwrap_err();
    assert!(
        err.message
            .starts_with("ENOENT: no such file or directory, stat"),
        "{}",
        err.message
    );
}

#[test]
fn an_untitled_copy_is_read_like_a_file_and_saved_like_a_new_document() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "template.hwpx", HWPX);
    let from = t.rt.view(1, Surface::Hwp);
    t.editors
        .grant_and_track(from.view, Some(&path), UntitledOption::Copy);
    assert_eq!(
        t.rt.invoke_now("hwp:untitled-copy", from, vec![]),
        Ok(obj([("name", s("template.hwpx")), ("format", s("hwpx"))]))
    );
    assert_eq!(t.editors.document_path(from.view), None);
}

#[test]
fn tracked_changes_are_dropped_or_block_the_save() {
    // the engine can take them out: the editor gets the plain document
    let mut t1 = t(json!({ "tracked_changes_of": "tracked", "without_tracked_changes": "PLAIN" }));
    let path = file(&t1.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t1, 1, Some(&path));
    assert_eq!(
        t1.rt.invoke_now("hwp:read-file", from, vec![s(&path)]),
        Ok(bytes(b"PLAIN"))
    );
    assert_eq!(
        t1.rt.invoke_now("hwp:tracking-dropped", from, vec![]),
        Ok(true.into_ipc())
    );

    // it cannot: no save under any name
    let mut t2 = t(json!({ "tracked_changes_of": "tracked" }));
    let path = file(&t2.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t2, 1, Some(&path));
    t2.rt
        .invoke_now("hwp:read-file", from, vec![s(&path)])
        .unwrap();
    assert_eq!(
        t2.rt.invoke_now("hwp:tracking-dropped", from, vec![]),
        Ok(false.into_ipc())
    );
    let begun = t2
        .rt
        .invoke_now("hwp:begin-save", from, vec![s("saveAs")])
        .unwrap();
    assert_eq!(text(&begun, "reason").as_deref(), Some("tracked"));
    assert!(t2.rt.script().calls.is_empty(), "no dialog");
}

#[test]
fn an_unreadable_structure_saves_to_another_name_only() {
    let mut t = t(json!({ "tracked_changes_of": "unreadable" }));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    t.rt.invoke_now("hwp:read-file", from, vec![s(&path)])
        .unwrap();
    assert_eq!(
        t.rt.invoke_now("hwp:structure-unverified", from, vec![]),
        Ok(true.into_ipc())
    );
    let begun =
        t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
            .unwrap();
    assert_eq!(text(&begun, "reason").as_deref(), Some("unverified"));
    // Save As to another file goes ahead
    let other = t.rt.dir.join("copy.hwp");
    t.rt.script().save.push_back(Some(other.clone()));
    let begun =
        t.rt.invoke_now("hwp:begin-save", from, vec![s("saveAs")])
            .unwrap();
    assert_eq!(
        text(&begun, "path"),
        Some(other.to_string_lossy().into_owned())
    );
}

#[test]
fn save_writes_once_with_a_backup_and_the_view_follows_its_file() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    t.rt.invoke_now("hwp:read-file", from, vec![s(&path)])
        .unwrap();
    t.rt.send("hwp:dirty-changed", from, vec![true.into_ipc()]);
    assert!(t.editors.is_dirty(from.view));

    // no save begun
    let none =
        t.rt.invoke_now("hwp:commit-save", from, vec![bytes(HWP)])
            .unwrap();
    assert_eq!(
        text(&none, "error").as_deref(),
        Some("hwp: no save in progress")
    );

    let begun =
        t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
            .unwrap();
    assert_eq!(
        begun,
        obj([
            ("ok", true.into_ipc()),
            ("path", s(&path)),
            ("format", s("hwp"))
        ])
    );
    let saved: Vec<u8> = [HWP, b"v2"].concat();
    let done =
        t.rt.invoke_now("hwp:commit-save", from, vec![bytes(&saved)])
            .unwrap();
    assert_eq!(
        done,
        obj([
            ("ok", true.into_ipc()),
            ("path", s(&path)),
            ("backupPath", s(&format!("{path}.bak"))),
        ])
    );
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    assert_eq!(std::fs::read(format!("{path}.bak")).unwrap(), HWP);
    assert!(!t.editors.is_dirty(from.view));

    // the second save replaces bytes this view wrote: no backup
    t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
        .unwrap();
    let again =
        t.rt.invoke_now("hwp:commit-save", from, vec![bytes(HWP)])
            .unwrap();
    assert_eq!(field(&again, "backupPath"), &IpcValue::Undefined);
    // the same path: no new-path hook
    assert!(
        !t.shell
            .take_calls()
            .iter()
            .any(|c| c.starts_with("file_saved"))
    );
}

#[test]
fn commit_refuses_bytes_that_are_not_the_target_format() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
        .unwrap();
    let wrong =
        t.rt.invoke_now("hwp:commit-save", from, vec![bytes(HWPX)])
            .unwrap();
    assert_eq!(
        text(&wrong, "error").as_deref(),
        Some("hwp: exported bytes are not a hwp file")
    );
    // the pending save was taken: the next commit has none
    let next =
        t.rt.invoke_now("hwp:commit-save", from, vec![bytes(HWP)])
            .unwrap();
    assert_eq!(
        text(&next, "error").as_deref(),
        Some("hwp: no save in progress")
    );
    t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
        .unwrap();
    let empty =
        t.rt.invoke_now("hwp:commit-save", from, vec![bytes(&[])])
            .unwrap();
    assert_eq!(
        text(&empty, "error").as_deref(),
        Some("hwp: bad save request")
    );
    assert_eq!(std::fs::read(&path).unwrap(), HWP);
}

#[test]
fn an_automatic_save_stands_down_when_a_sibling_changed_the_file() {
    let mut t = t(json!({ "is_path_open_elsewhere": true }));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    t.rt.invoke_now("hwp:read-file", from, vec![s(&path)])
        .unwrap();
    // the other pane wrote it
    std::fs::write(&path, [HWP, b"sibling!"].concat()).unwrap();
    t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
        .unwrap();
    let auto = obj([("auto", true.into_ipc())]);
    let done =
        t.rt.invoke_now(
            "hwp:commit-save",
            from,
            vec![bytes(HWP), IpcValue::Undefined, auto],
        )
        .unwrap();
    assert_eq!(
        text(&done, "error").as_deref(),
        Some("hwp: external-modified")
    );
    // a manual save asks; "keep the disk copy" stands down too
    t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
        .unwrap();
    t.rt.script().message.push_back(1);
    let done =
        t.rt.invoke_now("hwp:commit-save", from, vec![bytes(HWP)])
            .unwrap();
    assert_eq!(
        text(&done, "error").as_deref(),
        Some("hwp: external-modified")
    );
    assert_eq!(std::fs::read(&path).unwrap(), [HWP, b"sibling!"].concat());
}

#[test]
fn a_new_document_asks_where_and_save_as_may_pick_a_pdf_once() {
    let mut t = t(json!({}));
    let from = tab(&mut t, 1, None);
    // cancelled
    t.rt.script().save.push_back(None);
    assert_eq!(
        t.rt.invoke_now("hwp:begin-save", from, vec![s("save")]),
        Ok(obj([
            ("ok", true.into_ipc()),
            ("canceled", true.into_ipc())
        ]))
    );
    let asked = match t.rt.script().calls.pop() {
        Some(HostCall::Save(_, options)) => options,
        other => panic!("{other:?}"),
    };
    assert_eq!(asked.title.as_deref(), Some("Save HWP Document"));
    assert!(asked.default_path.unwrap().ends_with("Untitled.hwp"));
    assert_eq!(asked.filters[0].extensions, vec!["hwp".to_owned()]);
    assert_eq!(asked.filters.len(), 2);

    // a bare name takes the format's extension
    let bare = t.rt.dir.join("report");
    t.rt.script().save.push_back(Some(bare.clone()));
    let begun =
        t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
            .unwrap();
    assert_eq!(
        text(&begun, "path"),
        Some(format!("{}.hwp", bare.display()))
    );

    // Save As lists PDF, without the extension in the suggestion
    let pdf = t.rt.dir.join("out.pdf");
    t.rt.script().save.push_back(Some(pdf.clone()));
    let begun =
        t.rt.invoke_now("hwp:begin-save", from, vec![s("saveAs")])
            .unwrap();
    assert_eq!(text(&begun, "export").as_deref(), Some("pdf"));
    let asked = match t.rt.script().calls.pop() {
        Some(HostCall::Save(_, options)) => options,
        other => panic!("{other:?}"),
    };
    assert!(asked.default_path.unwrap().ends_with("Untitled"));
    assert_eq!(asked.filters.len(), 3);
    // no save is pending: the PDF export writes, once
    let commit =
        t.rt.invoke_now("hwp:commit-save", from, vec![bytes(HWP)])
            .unwrap();
    assert_eq!(
        text(&commit, "error").as_deref(),
        Some("hwp: no save in progress")
    );
    assert_eq!(t.editors.take_pdf_grant(from.view), Some(pdf));
    assert_eq!(t.editors.take_pdf_grant(from.view), None);
}

#[test]
fn save_as_to_a_new_path_tells_the_shell() {
    let mut t = t(json!({}));
    let from = tab(&mut t, 1, None);
    let target = t.rt.dir.join("new.hwpx");
    t.rt.script().save.push_back(Some(target.clone()));
    let begun =
        t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
            .unwrap();
    assert_eq!(text(&begun, "format").as_deref(), Some("hwpx"));
    let done =
        t.rt.invoke_now("hwp:commit-save", from, vec![bytes(HWPX)])
            .unwrap();
    assert_eq!(field(&done, "ok"), &true.into_ipc());
    let target = target.to_string_lossy().into_owned();
    assert_eq!(
        t.shell.take_calls(),
        vec![
            "runtime.newDocumentDir 1".to_owned(),
            format!("openPathProbe {target}"),
            format!("fileSavedHook 1 {target}"),
        ]
    );
    assert_eq!(
        t.rt.invoke_now("hwp:consume-pending", from, vec![]),
        Ok(s(&target))
    );
}

#[test]
fn new_tabs_need_a_pending_save_and_matching_bytes() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    let empty = Ok(IpcValue::Array(vec![]));
    assert_eq!(
        t.rt.invoke_now("hwp:new-tabs", from, vec![bytes(HWP), s("hwp")]),
        empty
    );
    t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
        .unwrap();
    assert_eq!(
        t.rt.invoke_now("hwp:new-tabs", from, vec![bytes(HWP), s("hwpx")]),
        empty
    );
    assert_eq!(
        t.rt.invoke_now("hwp:new-tabs", from, vec![bytes(HWP), s("hwp")]),
        empty
    );
}

#[test]
fn a_memo_editor_blocks_saves_until_it_closes_or_its_page_goes() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    let stranger = t.rt.view(2, Surface::Hwp);
    // only a Hangul view takes the lock
    t.rt.send("hwp:memo-modal-open", stranger, vec![true.into_ipc()]);
    assert_eq!(
        t.rt.invoke_now("hwp:memo-modal-state", stranger, vec![]),
        Ok(false.into_ipc())
    );
    t.rt.send("hwp:memo-modal-open", from, vec![true.into_ipc()]);
    // any view sees it
    assert_eq!(
        t.rt.invoke_now("hwp:memo-modal-state", stranger, vec![]),
        Ok(true.into_ipc())
    );
    let begun =
        t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
            .unwrap();
    assert_eq!(text(&begun, "reason").as_deref(), Some("memo"));
    assert_eq!(channels(&t), vec![("hwp:memo-blocked", vec![])]);
    t.rt.send("hwp:file-command", from, vec![s("save")]);
    assert_eq!(channels(&t), vec![("hwp:memo-blocked", vec![])]);

    t.editors.did_start_navigation(from.view, true, true);
    assert!(t.editors.memo_modal_open(from.view));
    t.editors.did_start_navigation(from.view, true, false);
    assert!(!t.editors.memo_modal_open(from.view));
    t.rt.send("hwp:memo-modal-open", from, vec![true.into_ipc()]);
    t.editors.render_process_gone(from.view, "crashed");
    assert!(!t.editors.memo_modal_open(from.view));
}

#[test]
fn a_menu_save_waits_for_the_renderer_one_at_a_time() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    let editor = editor_for(&t.rt.provided(), from.view).unwrap();
    let (first, then) = slot();
    editor.request_save(from.view, SaveMode::SaveAs, then);
    assert_eq!(channels(&t), vec![("hwp:save-request", vec![s("saveAs")])]);
    // a second request while the first is in flight is declined
    let (second, then) = slot();
    editor.request_save(from.view, SaveMode::Save, then);
    assert_eq!(*second.borrow(), Some(Ok(false)));
    assert!(channels(&t).is_empty());
    // the renderer's save lands
    t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
        .unwrap();
    t.rt.invoke_now("hwp:commit-save", from, vec![bytes(HWP)])
        .unwrap();
    assert_eq!(*first.borrow(), Some(Ok(true)));

    // an export that fails ends the wait; a silent renderer times out
    let (aborted, then) = slot();
    editor.request_save(from.view, SaveMode::Save, then);
    t.rt.send("hwp:abort-save", from, vec![]);
    assert_eq!(*aborted.borrow(), Some(Ok(false)));
    let (silent, then) = slot();
    editor.request_save(from.view, SaveMode::Save, then);
    t.rt.advance(Duration::from_millis(119_999));
    assert_eq!(*silent.borrow(), None);
    t.rt.advance(Duration::from_millis(1));
    assert_eq!(*silent.borrow(), Some(Ok(false)));
}

#[test]
fn the_file_menu_sends_the_same_requests_as_the_keys() {
    let mut t = t(json!({}));
    let from = tab(&mut t, 1, None);
    t.rt.send("hwp:file-command", from, vec![s("preview")]);
    t.rt.send("hwp:file-command", from, vec![s("print")]);
    t.rt.send("hwp:file-command", from, vec![s("pdf")]);
    t.rt.send("hwp:file-command", from, vec![s("bogus")]);
    assert_eq!(
        channels(&t),
        vec![
            ("hwp:preview-request", vec![s("open")]),
            ("hwp:preview-request", vec![s("print")]),
            ("hwp:print-request", vec![s("pdf")]),
        ]
    );
    t.rt.send("hwp:file-command", from, vec![s("importPdf")]);
    let stranger = t.rt.view(2, Surface::Hwp);
    t.rt.send("hwp:file-command", stranger, vec![s("importPdf")]);
    assert_eq!(t.shell.take_calls(), vec!["runtime.importPdf".to_owned()]);
}

#[test]
fn closing_a_dirty_tab_asks_and_saves_when_told() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    let editor = editor_for(&t.rt.provided(), from.view).unwrap();
    // clean: closes without asking
    let (clean, then) = slot();
    editor.request_close(from.view, None, then);
    assert_eq!(*clean.borrow(), Some(Ok(true)));
    assert!(t.rt.script().calls.is_empty());

    t.rt.send("hwp:dirty-changed", from, vec![true.into_ipc()]);
    t.rt.script().message.extend([2, 1, 0, 0]);
    let (cancel, then) = slot();
    editor.request_close(from.view, None, then);
    assert_eq!(*cancel.borrow(), Some(Ok(false)));
    let (dont, then) = slot();
    editor.request_close(from.view, None, then);
    assert_eq!(*dont.borrow(), Some(Ok(true)));

    let (save, then) = slot();
    editor.request_close(from.view, Some(shell_contract::WindowId(7)), then);
    assert_eq!(channels(&t), vec![("hwp:close-save-request", vec![])]);
    t.rt.send("hwp:close-save-result", from, vec![true.into_ipc()]);
    assert_eq!(*save.borrow(), Some(Ok(true)));
    match &t.rt.script().calls[2] {
        HostCall::Message(Some(shell_contract::WindowId(7)), b) => {
            assert_eq!(b.buttons, vec!["Save", "Don't Save", "Cancel"])
        }
        other => panic!("{other:?}"),
    }

    // the tab goes while the renderer saves: the close does not happen
    let (gone, then) = slot();
    editor.request_close(from.view, None, then);
    t.rt.destroy(from.view);
    assert_eq!(*gone.borrow(), Some(Ok(false)));
}

#[test]
fn reading_the_text_retries_until_the_editor_answers() {
    let mut t = t(json!({ "hwp_to_text": ["hello", { "error": "bad document" }] }));
    let from = tab(&mut t, 1, None);
    let editor = editor_for(&t.rt.provided(), from.view).unwrap();
    let (first, then) = slot();
    editor.read_text(from.view, then);
    let (shared, then) = slot();
    editor.read_text(from.view, then);
    assert_eq!(channels(&t).len(), 1);
    t.rt.advance(Duration::from_millis(250));
    assert_eq!(channels(&t), vec![("hwp:read-bytes-request", vec![])]);
    let answer = obj([("ok", true.into_ipc()), ("bytes", bytes(HWP))]);
    t.rt.send("hwp:read-bytes-result", from, vec![answer.clone()]);
    assert_eq!(*first.borrow(), Some(Ok("hello".to_owned())));
    assert_eq!(*shared.borrow(), Some(Ok("hello".to_owned())));
    // answered: no more requests
    t.rt.advance(Duration::from_millis(1000));
    assert!(channels(&t).is_empty());

    let (failed, then) = slot();
    editor.read_text(from.view, then);
    t.rt.send(
        "hwp:read-bytes-result",
        from,
        vec![obj([("ok", false.into_ipc())])],
    );
    assert_eq!(
        failed.borrow().clone().unwrap().unwrap_err().message,
        "the editor returned no document"
    );
    let (engine, then) = slot();
    editor.read_text(from.view, then);
    t.rt.send("hwp:read-bytes-result", from, vec![answer]);
    assert_eq!(
        engine.borrow().clone().unwrap().unwrap_err().message,
        "bad document"
    );

    let (late, then) = slot();
    editor.read_text(from.view, then);
    t.rt.advance(Duration::from_millis(30_000));
    assert_eq!(
        late.borrow().clone().unwrap().unwrap_err().message,
        "timed out reading the document"
    );
}

#[test]
fn native_headless_grant_requires_its_sender_and_is_single_use() {
    use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    use shell_contract::runtime::CancelToken;

    let mut t = t(json!({}));
    let sender = t.rt.view(901, Surface::Hwp);
    let foreign = t.rt.view(902, Surface::Hwp);
    let generation =
        t.rt.services
            .views
            .renderer_generation(sender.view)
            .unwrap();
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap();
    let owner = effects.capture_owner(sender.view, &generation).unwrap();
    let other_generation =
        t.rt.services
            .views
            .renderer_generation(foreign.view)
            .unwrap();
    let other = effects
        .capture_owner(foreign.view, &other_generation)
        .unwrap();
    let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
        NativeInvocationSelection::HeadlessExport {
            surface: Surface::Hwp,
            input: "/in.hwp".into(),
            output: "/out.pdf".into(),
            format: "pdf".into(),
            page: None,
            ops: None,
            batch: None,
        },
        CancelToken::default(),
    );
    let target = operation.reserve_target(sender.view, generation).unwrap();
    target.register_from_native_admitted(owner.clone()).unwrap();
    t.editors
        .0
        .change(sender.view, |state| state.native_headless = true);
    let timer = crate::kit::after(&t.rt.services, Duration::from_secs(180), || {});
    let (finished, then) = slot();
    t.editors.0.headless.borrow_mut().insert(
        sender.view,
        super::Headless {
            timer,
            then,
            native: Some(super::NativeHeadless {
                target,
                pdf: Some(PathBuf::from("/out.pdf")),
                committed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                subscriptions: Vec::new(),
            }),
        },
    );
    assert!(
        t.editors
            .take_native_headless_pdf_grant(sender.view, None)
            .is_err()
    );
    assert!(
        t.editors
            .take_native_headless_pdf_grant(sender.view, Some(&other.context))
            .is_err()
    );
    assert!(matches!(
        t.editors.take_native_headless_pdf_grant(foreign.view, None),
        Ok(None)
    ));
    let grant = t
        .editors
        .take_native_headless_pdf_grant(sender.view, Some(&owner.context))
        .unwrap()
        .unwrap();
    assert_eq!(grant.path, PathBuf::from("/out.pdf"));
    assert!(
        t.editors
            .take_native_headless_pdf_grant(sender.view, Some(&owner.context))
            .is_err()
    );
    assert_eq!(t.editors.take_pdf_grant(sender.view), None);
    let premature = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.rt.send_native(
            "hwp:headless-export-done",
            sender,
            vec![obj([("ok", true.into_ipc())])],
            owner.context.clone(),
            CancelToken::default(),
        );
    }));
    assert!(premature.is_err());
    assert!(finished.borrow().is_none());
    (grant.commit)();
    t.rt.send_native(
        "hwp:headless-export-done",
        sender,
        vec![obj([("ok", true.into_ipc())])],
        owner.context.clone(),
        CancelToken::default(),
    );
    assert_eq!(*finished.borrow(), Some(Ok(())));
    assert!(
        t.editors
            .take_native_headless_pdf_grant(sender.view, Some(&owner.context))
            .is_err()
    );
}

#[test]
fn a_headless_export_hands_its_path_once_and_ends_with_the_report() {
    let mut t = t(json!({ "create_headless_view": [900, 901, 902] }));
    let input = file(&t.rt.dir, "in.hwp", HWP);
    for id in [900, 901, 902] {
        t.rt.view(id, Surface::Hwp);
    }
    let (ok, then) = slot();
    t.editors
        .export_hwp_pdf_headless(&input, "/out/a.pdf", HEADLESS_TIMEOUT_MS, then);
    let hidden = Sender {
        view: ViewId(900),
        window: shell_contract::WindowId(900),
        surface: Surface::Hwp,
    };
    assert_eq!(
        t.rt.invoke_now("hwp:consume-pending", hidden, vec![]),
        Ok(s(&input))
    );
    assert_eq!(
        t.rt.invoke_now("hwp:consume-headless-export", hidden, vec![]),
        Ok(s("/out/a.pdf"))
    );
    assert_eq!(
        t.rt.invoke_now("hwp:consume-headless-export", hidden, vec![]),
        Ok(IpcValue::Null)
    );
    // the PDF may write the command line's path, once
    assert_eq!(
        t.editors.take_pdf_grant(ViewId(900)),
        Some(PathBuf::from("/out/a.pdf"))
    );
    t.rt.send(
        "hwp:headless-export-done",
        hidden,
        vec![obj([("ok", true.into_ipc())])],
    );
    assert_eq!(*ok.borrow(), Some(Ok(())));
    assert_eq!(
        t.shell.take_calls(),
        vec![
            "new BrowserWindow",
            "webContents.loadURL 900",
            "BrowserWindow.destroy 900"
        ]
    );

    let (failed, then) = slot();
    t.editors
        .export_hwp_pdf_headless(&input, "/out/b.pdf", HEADLESS_TIMEOUT_MS, then);
    let hidden = Sender {
        view: ViewId(901),
        ..hidden
    };
    t.rt.send(
        "hwp:headless-export-done",
        hidden,
        vec![obj([("ok", false.into_ipc()), ("error", s("no pages"))])],
    );
    assert_eq!(
        failed.borrow().clone().unwrap().unwrap_err().message,
        "no pages"
    );

    let (late, then) = slot();
    t.editors
        .export_hwp_pdf_headless(&input, "/out/c.pdf", 1000, then);
    t.rt.advance(Duration::from_millis(1000));
    assert_eq!(
        late.borrow().clone().unwrap().unwrap_err().message,
        "hwp export timed out after 1000ms (a password-protected document cannot be exported this way)"
    );
}

#[test]
fn headless_navigation_failure_finishes_and_destroys_the_view() {
    let t = t(json!({ "load_headless_view": "navigation failed" }));
    let input = file(&t.rt.dir, "in.hwp", HWP);
    let (answer, then) = slot();
    t.editors
        .export_hwp_pdf_headless(&input, "/out/a.pdf", HEADLESS_TIMEOUT_MS, then);
    assert_eq!(
        answer.borrow().clone().unwrap().unwrap_err().message,
        "navigation failed"
    );
    assert_eq!(
        t.shell.take_calls(),
        vec![
            "new BrowserWindow",
            "webContents.loadURL 900",
            "BrowserWindow.destroy 900"
        ]
    );
}

#[test]
fn a_destroyed_view_leaves_no_state() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    t.rt.send("hwp:dirty-changed", from, vec![true.into_ipc()]);
    let editor = editor_for(&t.rt.provided(), from.view).unwrap();
    let (pending, then) = slot();
    editor.request_save(from.view, SaveMode::Save, then);
    t.rt.destroy(from.view);
    assert_eq!(*pending.borrow(), Some(Ok(false)));
    assert!(!t.editors.is_dirty(from.view));
    assert!(!editor.owns(from.view));
    assert_eq!(editor.document_path(from.view), None);
}

#[test]
fn a_renamed_file_keeps_its_view_and_its_grant() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    let renamed = t.rt.dir.join("b.hwp");
    std::fs::rename(&path, &renamed).unwrap();
    let editor = editor_for(&t.rt.provided(), from.view).unwrap();
    editor.file_renamed(from.view, Path::new(&path), &renamed);
    assert_eq!(editor.document_path(from.view), Some(renamed.clone()));
    let renamed = renamed.to_string_lossy().into_owned();
    assert_eq!(
        t.rt.invoke_now("hwp:consume-pending", from, vec![]),
        Ok(s(&renamed))
    );
    assert!(
        t.rt.invoke_now("hwp:read-file", from, vec![s(&renamed)])
            .is_ok()
    );
}

#[test]
fn ctrl_s_and_hangul_alt_keys_ask_for_a_save() {
    let mut t = t(json!({}));
    let from = tab(&mut t, 1, None);
    let mac = cfg!(target_os = "macos");
    let key = |code: &str, shift: bool| KeyInput {
        kind: "keyDown".into(),
        key: "s".into(),
        code: code.into(),
        shift,
        control: !mac,
        meta: mac,
        ..KeyInput::default()
    };
    assert!(t.editors.before_input(from.view, &key("KeyS", false)));
    assert_eq!(channels(&t), vec![("hwp:save-request", vec![s("save")])]);
    // Ctrl+Shift+S: the editor did not take it, so it is Save As (declined
    // here: the first save is still in flight)
    assert!(t.editors.before_input(from.view, &key("KeyS", true)));
    assert!(
        t.shell
            .take_calls()
            .contains(&"editorTakes 1 ctrl-shift-s".to_owned())
    );
    assert!(t.editors.before_input(from.view, &key("KeyP", false)));
    assert_eq!(
        channels(&t),
        vec![("hwp:preview-request", vec![s("print")])]
    );
    let plain = KeyInput {
        control: false,
        meta: false,
        ..key("KeyS", false)
    };
    assert!(!t.editors.before_input(from.view, &plain));
    let alt = KeyInput {
        alt: true,
        control: false,
        meta: false,
        ..key("KeyP", false)
    };
    // Hangul's Alt keys are not a Mac's
    assert_eq!(t.editors.before_input(from.view, &alt), !mac);
}

#[test]
fn a_pdf_to_import_is_named_as_the_document_it_becomes() {
    let mut t = t(json!({}));
    let pdf = file(&t.rt.dir, "scan.pdf", b"%PDF-");
    let from = t.rt.view(1, Surface::Hwp);
    t.editors.grant_and_track(
        from.view,
        Some(&pdf),
        UntitledOption::Pdf {
            base_name: "scan".into(),
        },
    );
    assert_eq!(
        t.rt.invoke_now("hwp:untitled-copy", from, vec![]),
        Ok(obj([("name", s("scan.hwp")), ("format", s("hwp"))]))
    );
    // a path that is not there is not granted
    let gone = t.rt.view(2, Surface::Hwp);
    t.editors
        .grant_and_track(gone.view, Some("/no/such.hwp"), UntitledOption::No);
    assert_eq!(
        t.rt.invoke_now("hwp:consume-pending", gone, vec![]),
        Ok(IpcValue::Null)
    );
}

#[test]
fn links_the_page_opens_go_to_the_browser_when_safe() {
    let t = t(json!({}));
    t.editors.window_open("https://example.test/a");
    t.editors.window_open("mailto:a@example.test");
    t.editors.window_open("file:///etc/passwd");
    t.editors.window_open("javascript:alert(1)");
    assert_eq!(
        t.rt.script().calls,
        vec![
            HostCall::OpenExternal("https://example.test/a".into()),
            HostCall::OpenExternal("mailto:a@example.test".into()),
        ]
    );
}

#[test]
fn the_shell_reads_and_sets_a_tab_s_recovery_slot() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let saved = tab(&mut t, 1, Some(&path));
    let untitled = tab(&mut t, 2, None);
    let stranger = t.rt.view(3, Surface::Hwp);
    assert!(matches!(
        t.editors.recovery_identity(saved.view),
        Some(RecoveryIdentity::File(f)) if f.file_path == path
    ));
    let Some(RecoveryIdentity::Untitled(slot)) = t.editors.recovery_identity(untitled.view) else {
        panic!("an untitled slot")
    };
    assert_eq!(slot.ext, ".hwp");
    // the slot stays the view's
    assert_eq!(
        t.editors.recovery_identity(untitled.view),
        Some(RecoveryIdentity::Untitled(slot))
    );
    assert_eq!(t.editors.recovery_identity(stranger.view), None);

    // the untitled copy is listed under the tab's title
    t.editors.set_recovery_title(untitled.view, "Minutes");
    t.rt.invoke_now("hwp:recovery-begin", untitled, vec![])
        .unwrap();
    t.rt.invoke_now("hwp:recovery-write", untitled, vec![bytes(HWP), s("hwp")])
        .unwrap();
    let meta = std::fs::read_dir(t.rt.dir.join("userData/hwp-autosave/untitled"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with(".meta.json"))
        .unwrap();
    let meta = std::fs::read_to_string(meta.path()).unwrap();
    assert!(meta.contains("\"title\": \"Minutes\""), "{meta}");

    // a tool closed the file without saving: its copy goes
    t.rt.invoke_now("hwp:recovery-begin", saved, vec![])
        .unwrap();
    t.rt.invoke_now("hwp:recovery-write", saved, vec![bytes(HWP), s("hwp")])
        .unwrap();
    assert_eq!(copies(&t), 1);
    t.editors.discard_recovery_copy(saved.view);
    assert_eq!(copies(&t), 0);
    t.editors.request_preview(saved.view, "open");
    assert_eq!(channels(&t), vec![("hwp:preview-request", vec![s("open")])]);
}

// ---- recovery ----

fn copies(t: &T) -> usize {
    let dir = t.rt.dir.join("userData").join("hwp-autosave");
    std::fs::read_dir(dir).map_or(0, |d| {
        d.filter_map(Result::ok)
            .filter(|e| !e.file_name().to_string_lossy().ends_with(".json"))
            .filter(|e| e.path().is_file())
            .count()
    })
}

#[test]
fn a_turn_writes_one_copy_and_the_save_removes_it() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    // a write without a turn is older than what the editor holds
    assert_eq!(
        t.rt.invoke_now("hwp:recovery-write", from, vec![bytes(HWP), s("hwp")]),
        Ok(s("superseded"))
    );
    assert_eq!(
        t.rt.invoke_now("hwp:recovery-begin", from, vec![]),
        Ok(obj([("ok", true.into_ipc()), ("format", s("hwp"))]))
    );
    assert_eq!(
        t.rt.invoke_now("hwp:recovery-new-tabs", from, vec![bytes(HWP), s("hwpx")]),
        Ok(IpcValue::Array(vec![]))
    );
    // another format than the turn's is refused
    assert_eq!(
        t.rt.invoke_now("hwp:recovery-write", from, vec![bytes(HWPX), s("hwpx")]),
        Ok(s("refused"))
    );
    t.rt.invoke_now("hwp:recovery-begin", from, vec![]).unwrap();
    assert_eq!(
        t.rt.invoke_now("hwp:recovery-write", from, vec![bytes(HWP), s("hwp")]),
        Ok(s("written"))
    );
    assert_eq!(copies(&t), 1);
    t.rt.invoke_now("hwp:begin-save", from, vec![s("save")])
        .unwrap();
    t.rt.invoke_now("hwp:commit-save", from, vec![bytes(HWP)])
        .unwrap();
    assert_eq!(copies(&t), 0);
}

#[test]
fn a_protected_document_gets_no_copy_and_one_notice() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    let first =
        t.rt.invoke_now("hwp:recovery-opened", from, vec![true.into_ipc()])
            .unwrap();
    assert_eq!(field(&first, "restored"), &false.into_ipc());
    assert!(text(&first, "notice").is_some_and(|n| !n.is_empty()));
    let again =
        t.rt.invoke_now("hwp:recovery-opened", from, vec![true.into_ipc()])
            .unwrap();
    assert_eq!(field(&again, "notice"), &IpcValue::Null);
    assert_eq!(
        t.rt.invoke_now("hwp:recovery-begin", from, vec![]),
        Ok(obj([("ok", false.into_ipc())]))
    );
    assert!(t.editors.is_unprotectable(&from.view));
}

#[test]
fn the_os_session_end_asks_every_dirty_tab_for_its_copy() {
    let mut t = t(json!({}));
    let (pa, pb) = (file(&t.rt.dir, "a.hwp", HWP), file(&t.rt.dir, "b.hwp", HWP));
    let a = tab(&mut t, 1, Some(&pa));
    let other = tab(&mut t, 2, Some(&pb));
    t.rt.send("hwp:dirty-changed", a, vec![true.into_ipc()]);
    let result = Rc::new(RefCell::new(None));
    let completed = Rc::clone(&result);
    t.editors
        .write_copies_now(Box::new(move |value| *completed.borrow_mut() = Some(value)));
    let events = t.rt.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].target, Target::View(a.view));
    assert_eq!(events[0].channel, "hwp:recovery-flush");
    let token = events[0].args[0].clone();
    t.rt.send("hwp:recovery-flushed", other, vec![token.clone()]);
    assert!(result.borrow().is_none());
    t.rt.send(
        "hwp:recovery-flushed",
        a,
        vec![token.clone(), s("copy failed")],
    );
    assert!(matches!(&*result.borrow(), Some(Err(error)) if error.0 == "copy failed"));
    t.rt.send("hwp:recovery-flushed", a, vec![token]);
    t.rt.send("hwp:recovery-flushed", a, vec![s("1")]);
    t.rt.advance(Duration::from_millis(10_000));
    assert!(t.rt.events().is_empty());
}

#[test]
fn hwp_flush_replacement_rejects_late_reply_and_timeout() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let view = tab(&mut t, 1, Some(&path));
    t.rt.send("hwp:dirty-changed", view, vec![true.into_ipc()]);
    let first = Rc::new(RefCell::new(None));
    let done = Rc::clone(&first);
    t.editors
        .write_copies_now(Box::new(move |result| *done.borrow_mut() = Some(result)));
    let old = t.rt.events()[0].args[0].clone();
    let second = Rc::new(RefCell::new(None));
    let done = Rc::clone(&second);
    t.editors
        .write_copies_now(Box::new(move |result| *done.borrow_mut() = Some(result)));
    assert!(matches!(&*first.borrow(), Some(Err(_))));
    let current = t.rt.events()[0].args[0].clone();
    t.rt.send("hwp:recovery-flushed", view, vec![old]);
    assert!(second.borrow().is_none());
    t.rt.advance(Duration::from_millis(10_000));
    assert!(matches!(&*second.borrow(), Some(Err(_))));
    t.rt.send("hwp:recovery-flushed", view, vec![current]);
    assert!(matches!(&*second.borrow(), Some(Err(_))));
}

#[test]
fn hwp_flush_fails_when_the_renderer_is_destroyed() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let view = tab(&mut t, 1, Some(&path));
    t.rt.send("hwp:dirty-changed", view, vec![true.into_ipc()]);
    let result = Rc::new(RefCell::new(None));
    let done = Rc::clone(&result);
    t.editors
        .write_copies_now(Box::new(move |value| *done.borrow_mut() = Some(value)));
    let token = t.rt.events()[0].args[0].clone();
    t.rt.destroy(view.view);
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    t.rt.send("hwp:recovery-flushed", view, vec![token]);
    assert!(matches!(&*result.borrow(), Some(Err(_))));
}

#[test]
fn hwp_flush_fails_on_new_document_navigation() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let view = tab(&mut t, 1, Some(&path));
    t.rt.send("hwp:dirty-changed", view, vec![true.into_ipc()]);
    let result = Rc::new(RefCell::new(None));
    let done = Rc::clone(&result);
    t.editors
        .write_copies_now(Box::new(move |value| *done.borrow_mut() = Some(value)));
    let token = t.rt.events()[0].args[0].clone();
    t.editors.did_start_navigation(view.view, true, true);
    assert!(result.borrow().is_none());
    t.editors.did_start_navigation(view.view, true, false);
    assert!(result.borrow().is_none());
    t.rt.replace_renderer(view.view);
    assert!(matches!(&*result.borrow(), Some(Err(_))));
    t.rt.send("hwp:recovery-flushed", view, vec![token]);
    assert!(matches!(&*result.borrow(), Some(Err(_))));
}

#[test]
fn hwp_flush_reports_dispatch_failure() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let view = tab(&mut t, 1, Some(&path));
    t.rt.send("hwp:dirty-changed", view, vec![true.into_ipc()]);
    t.rt.fail_next_checked_event("stream unavailable");
    let result = Rc::new(RefCell::new(None));
    let done = Rc::clone(&result);
    t.editors
        .write_copies_now(Box::new(move |value| *done.borrow_mut() = Some(value)));
    assert!(matches!(&*result.borrow(), Some(Err(error)) if error.0 == "stream unavailable"));
    assert!(t.rt.events().is_empty());
}

#[test]
fn hwp_renderer_replacement_fails_only_old_generation() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let view = tab(&mut t, 1, Some(&path));
    t.rt.send("hwp:dirty-changed", view, vec![true.into_ipc()]);
    let old = Rc::new(RefCell::new(None));
    let done = old.clone();
    t.editors
        .write_copies_now(Box::new(move |value| *done.borrow_mut() = Some(value)));
    let token = t.rt.events()[0].args[0].clone();
    let previous_generation = t.rt.services.views.renderer_generation(view.view).unwrap();
    t.rt.replace_renderer(view.view);
    assert!(matches!(&*old.borrow(), Some(Err(_))));
    let current = Rc::new(RefCell::new(None));
    let done = current.clone();
    t.editors
        .write_copies_now(Box::new(move |value| *done.borrow_mut() = Some(value)));
    let current_token = t.rt.events()[0].args[0].clone();
    t.rt.send_from_generation(
        "hwp:recovery-flushed",
        view,
        vec![current_token.clone()],
        previous_generation,
    );
    assert!(current.borrow().is_none());
    t.rt.send("hwp:recovery-flushed", view, vec![token]);
    assert!(current.borrow().is_none());
    t.rt.send("hwp:recovery-flushed", view, vec![current_token]);
    assert!(matches!(&*current.borrow(), Some(Ok(()))));
}

#[test]
fn dont_save_drops_the_copy_and_asks_again_if_the_tab_stays() {
    let mut t = t(json!({}));
    let path = file(&t.rt.dir, "a.hwp", HWP);
    let from = tab(&mut t, 1, Some(&path));
    t.rt.invoke_now("hwp:recovery-begin", from, vec![]).unwrap();
    t.rt.invoke_now("hwp:recovery-write", from, vec![bytes(HWP), s("hwp")])
        .unwrap();
    assert_eq!(copies(&t), 1);
    t.rt.send("hwp:dirty-changed", from, vec![true.into_ipc()]);
    t.rt.script().message.push_back(1);
    let (closed, then) = slot();
    t.editors
        .0
        .request_close(from.view, None, Box::new(move |ok| then(Ok(ok))));
    assert_eq!(*closed.borrow(), Some(Ok(true)));
    assert_eq!(copies(&t), 0);
    // no copy while the tab closes
    assert_eq!(
        t.rt.invoke_now("hwp:recovery-begin", from, vec![]),
        Ok(obj([("ok", false.into_ipc())]))
    );
    t.rt.advance(Duration::from_millis(5_000));
    assert_eq!(channels(&t), vec![("hwp:recovery-reset", vec![])]);
}
