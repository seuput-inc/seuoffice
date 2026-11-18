use super::*;
use std::{cell::RefCell, sync::atomic::{AtomicBool, Ordering}};
use shell_contract::{editors::DocumentEditors, hosts::PdfReloadHost, ipc::{NativeSavedInput, NativeSavedPath}};

#[cfg(unix)]
fn saved_input(rt: &Rt, existing: &NativeRequestOwner, path: &std::path::Path, bytes: &[u8]) -> (NativeSavedInput, NativeSavedPath) {
    use std::ffi::CString;
    let effects = rt.services.hosts.get::<OwnedEffectsHost>().unwrap();
    let issuer = shell_contract::ipc::NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer.issue_existing_document_save(Surface::Pdf, existing.view, shell_contract::ipc::NativeSaveSelection::ExactPath(path.to_owned()), existing.clone(), CancelToken::default()).unwrap();
    let selection = shell_contract::ipc::NativeSaveDestination::from_exact_selection(binding).unwrap();
    let parent = std::fs::File::open(path.parent().unwrap()).unwrap();
    let directory = file_commit::PrivateDirectory::create(&parent).unwrap();
    let stage = directory.create_stage(0o600).unwrap().quarantine().ok().unwrap();
    let stage = stage.write_owned(bytes.to_vec()).ok().unwrap();
    let name = CString::new(path.file_name().unwrap().to_str().unwrap()).unwrap();
    let stage = stage.prepare(&parent, &name, path, file_commit::PublicationMode::Replace).ok().unwrap();
    let (publication, snapshot) = effects.publish_native_staged_write(&selection, &[], &CancelToken::default(), &CancelToken::default(), &[], stage).ok().unwrap();
    let input = NativeSavedInput::from_written_snapshot(publication.clone(), snapshot).unwrap();
    (input, NativeSavedPath::Selected(publication))
}

#[cfg(unix)]
#[test]
fn retained_reload_waits_for_exact_renderer_ack_and_preserves_literal_cancel() {
    for scenario in ["success", "parse-failure", "load-failure", "cancel-at-acceptance", "revoke-before-registration", "cancel-before-ack", "destroy-before-ack"] {
        let mut rt = Rt::new(|hosts| {
            crate::fakes(hosts, &serde_json::Value::Null);
            hosts.insert(Arc::new(Mutex::new(main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"))));
        });
        rt.register(crate::pdf::entries).unwrap();
        let sender = rt.view(1, Surface::Pdf);
        let path = rt.dir.join("generated.pdf");
        std::fs::write(&path, b"%PDF previous").unwrap();
        let editor = rt.provided().all::<crate::pdf::editor::editors::PdfEditors>().pop().unwrap();
        editor.open_view(sender.view, path.to_str());
        rt.send("pdf:dirty-changed", sender, vec![IpcValue::Bool(true)]);
        let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
        let native = rt.services.hosts.get::<OwnedEffectsHost>().unwrap().capture_owner(sender.view, &renderer).unwrap();
        let native = NativeRequestOwner { view: sender.view, context: native.context.with_document_lease(RendererGeneration::new()) };
        let existing = editor.capture_document_owner(sender.view, native).unwrap();
        let expected = b"%PDF malformed but retained";
        let (input, saved_path) = saved_input(&rt, &existing, &path, expected);
        std::fs::write(&path, b"%PDF replaced after commit").unwrap();
        let admission = Arc::new(AtomicBool::new(false));
        let admitted = admission.clone();
        let loss_seen = Arc::new(AtomicBool::new(false));
        let loss_observed = loss_seen.clone();
        let admitted_on_loss = admission.clone();
        let _loss = renderer.loss_token().on_cancel(move || {
            assert!(admitted_on_loss.load(Ordering::Acquire));
            loss_observed.store(true, Ordering::Release);
        });
        let source = existing.clone();
        let cancel = CancelToken::default();
        let callback_cancel = CancelToken::default();
        let cancel_on_acceptance = callback_cancel.clone();
        let accepted = Box::new(move |marker: AcceptedPdfReload| {
            assert!(marker.retires(&source));
            admitted.store(true, Ordering::Release);
            if scenario == "cancel-at-acceptance" { cancel_on_acceptance.cancel(); }
        });
        let answer = Rc::new(RefCell::new(None));
        let output = answer.clone();
        rt.script().pdf_reload.push_back(if scenario == "load-failure" { Err("actual synthetic load failed".into()) } else { Ok(()) });
        let service = rt.services.hosts.get::<PdfReloadHost>().unwrap().clone();
        service.reload_saved_document(&rt.provided(), &rt.services, input, saved_path, existing, vec![], cancel.clone(), CancelToken::default(), vec![callback_cancel.clone()], accepted, Box::new(move |result| *output.borrow_mut() = Some(result)));
        let early = Arc::new(Mutex::new(Vec::new()));
        let early_owner = rt.script().pdf_reload_owner.clone();
        if let Some(owner) = &early_owner {
            let (_, handler) = rt.table.handlers.get("pdf:consume-recovery-pending").unwrap();
            let handler = Rc::clone(handler);
            let captured = early.clone();
            let cx = shell_contract::ipc::CallContext {
                sender, request: Some(shell_contract::RequestId(1)), cancel: CancelToken::default(),
                native: Some(owner.context.clone()),
            };
            handler.call(&cx, vec![], shell_contract::ipc::Responder::new(move |reply| captured.lock().unwrap().push(reply)));
            assert!(early.lock().unwrap().is_empty(), "consume must wait for the registered successor");
            if scenario == "revoke-before-registration" { owner.context.renderer().unwrap().revoke(); }
        }
        rt.drain();
        assert!(admission.load(Ordering::Acquire));
        assert_eq!(loss_seen.load(Ordering::Acquire), scenario != "cancel-at-acceptance");
        assert_eq!(rt.script().calls.iter().filter(|call| **call == HostCall::ReloadPdf(sender.view)).count(), usize::from(scenario != "cancel-at-acceptance"));
        if scenario == "revoke-before-registration" {
            assert!(answer.borrow_mut().take().unwrap().is_err());
            let replies = early.lock().unwrap();
            assert_eq!(replies.len(), 1);
            assert!(replies[0].is_err());
            continue;
        }
        if matches!(scenario, "load-failure" | "cancel-at-acceptance") {
            assert!(answer.borrow_mut().take().unwrap().is_err());
            assert_eq!(callback_cancel.is_cancelled(), scenario == "cancel-at-acceptance");
            continue;
        }
        assert!(answer.borrow().is_none(), "native context registration is not parser completion");
        if scenario == "cancel-before-ack" {
            callback_cancel.cancel();
            rt.drain();
            assert!(answer.borrow_mut().take().unwrap().is_err());
            continue;
        }
        if scenario == "destroy-before-ack" {
            rt.destroy(sender.view);
            assert!(answer.borrow_mut().take().unwrap().is_err());
            continue;
        }
        let owner = early_owner.unwrap();
        let mut replies = early.lock().unwrap();
        assert_eq!(replies.len(), 1, "activation must answer the parked consume exactly once");
        let pending = replies.pop().unwrap().unwrap();
        drop(replies);
        let token = crate::kit::field(&pending, "token").clone();
        let path_value = crate::kit::field(&pending, "path").clone();
        let chunk = rt.invoke_native("pdf:read-file-chunk", sender, vec![path_value, IpcValue::Number(0.0), IpcValue::Number(1024.0)], owner.context.clone(), CancelToken::default()).take().unwrap().unwrap();
        assert_eq!(crate::kit::field(&chunk, "data"), &IpcValue::Bytes(shell_contract::BytesKind::Uint8Array, expected.to_vec()));
        assert!(rt.invoke_native("pdf:recovery-initialized", sender, vec![IpcValue::String("foreign".into()), IpcValue::Bool(true)], owner.context.clone(), CancelToken::default()).take().unwrap().is_err());
        let foreign = shell_contract::ipc::NativeContext::with_renderer((), RendererGeneration::new());
        assert!(rt.invoke_native("pdf:recovery-initialized", sender, vec![token.clone(), IpcValue::Bool(true)], foreign, CancelToken::default()).take().unwrap().is_err());
        assert!(answer.borrow().is_none());
        let ok = scenario == "success";
        let ack = rt.invoke_native("pdf:recovery-initialized", sender, vec![token.clone(), IpcValue::Bool(ok), IpcValue::String("parse failed".into())], owner.context.clone(), CancelToken::default()).take().unwrap();
        assert!(ack.is_ok());
        rt.drain();
        let result = answer.borrow_mut().take().unwrap();
        assert_eq!(result.is_ok(), ok);
        if let Ok(receipt) = result {
            assert_eq!(receipt.input().bytes(), expected);
            assert_eq!(receipt.path().path(), path.as_path());
            assert!(!editor.is_dirty(sender.view));
        }
        assert!(rt.invoke_native("pdf:recovery-initialized", sender, vec![token, IpcValue::Bool(true)], owner.context.clone(), CancelToken::default()).take().unwrap().is_err());
        assert!(!cancel.is_cancelled());
    }
}

#[cfg(unix)]
#[test]
fn reload_rejects_a_path_from_another_committed_publication_before_admission() {
    let mut rt = Rt::new(|hosts| {
        crate::fakes(hosts, &serde_json::Value::Null);
        hosts.insert(Arc::new(Mutex::new(main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"))));
    });
    rt.register(crate::pdf::entries).unwrap();
    let sender = rt.view(1, Surface::Pdf);
    let first = rt.dir.join("first.pdf");
    let second = rt.dir.join("second.pdf");
    std::fs::write(&first, b"old first").unwrap();
    std::fs::write(&second, b"old second").unwrap();
    let editor = rt.provided().all::<crate::pdf::editor::editors::PdfEditors>().pop().unwrap();
    editor.open_view(sender.view, first.to_str());
    let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
    let native = rt.services.hosts.get::<OwnedEffectsHost>().unwrap().capture_owner(sender.view, &renderer).unwrap();
    let native = NativeRequestOwner { view: sender.view, context: native.context.with_document_lease(RendererGeneration::new()) };
    let existing = editor.capture_document_owner(sender.view, native).unwrap();
    let (input, first_path) = saved_input(&rt, &existing, &first, b"%PDF one");
    let (_, other_path) = saved_input(&rt, &existing, &second, b"%PDF two");
    let foreign_read = other_path.publication().commit().read_bounded(1024, || false).unwrap();
    let foreign_input = NativeSavedInput::from_drive_committed_read(first_path.publication().clone(), foreign_read);
    let admitted = Arc::new(AtomicBool::new(false));
    let flag = admitted.clone();
    let result = Rc::new(RefCell::new(None));
    let output = result.clone();
    rt.services.hosts.get::<PdfReloadHost>().unwrap().reload_saved_document(
        &rt.provided(), &rt.services, input, other_path, existing.clone(), vec![], CancelToken::default(),
        CancelToken::default(), vec![], Box::new(move |_| { flag.store(true, Ordering::Release); }),
        Box::new(move |answer| *output.borrow_mut() = Some(answer)),
    );
    assert!(result.borrow_mut().take().unwrap().is_err());
    assert!(!admitted.load(Ordering::Acquire));
    let flag = admitted.clone();
    let output = result.clone();
    rt.services.hosts.get::<PdfReloadHost>().unwrap().reload_saved_document(
        &rt.provided(), &rt.services, foreign_input, first_path, existing, vec![], CancelToken::default(),
        CancelToken::default(), vec![], Box::new(move |_| { flag.store(true, Ordering::Release); }),
        Box::new(move |answer| *output.borrow_mut() = Some(answer)),
    );
    assert!(result.borrow_mut().take().unwrap().is_err());
    assert!(!admitted.load(Ordering::Acquire));
    assert!(renderer.is_active());
    assert!(rt.script().calls.iter().all(|call| *call != HostCall::ReloadPdf(sender.view)));
}

#[test]
fn discarded_reload_callback_rejects_once_without_reissuing() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let recorded = calls.clone();
    let guard = ReloadCompletion(Rc::new(RefCell::new(Some(Box::new(move |result| {
        recorded.borrow_mut().push(result.err());
    })))));
    let pending = guard.clone();
    drop(guard);
    assert!(calls.borrow().is_empty());
    drop(pending);
    assert_eq!(calls.borrow().len(), 1);
    assert!(calls.borrow()[0].as_deref().unwrap().contains("discarded"));
}
