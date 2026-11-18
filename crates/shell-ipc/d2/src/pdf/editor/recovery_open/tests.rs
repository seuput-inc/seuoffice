use super::super::{
    editors::PdfEditors,
    fakes,
    hosts::{PdfEngineHost, PdfShellHost},
    recovery::file_identity,
};
use crate::testing::{Pending, Rt};
use main_core::packages::electron_utils::{
    recovery_contract::{RecoveryEditorContract, RecoveryOpenUntitledRequest},
    recovery_store::recovery_copy_path,
};
use shell_contract::{
    IpcValue, Surface,
    editors::DocumentEditors,
    hosts::OwnedEffectsHost,
    ipc::{RecoveryOpenContext, RecoveryOpenTarget},
    runtime::{CancelToken, Workers},
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Deferred(Mutex<Vec<Box<dyn FnOnce() + Send>>>);
impl Workers for Deferred {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
        self.0.lock().unwrap().push(job);
    }
}

/// The message of the refusal a handler answered with
fn refusal(pending: &Pending) -> String {
    pending.take().unwrap().unwrap_err().message
}

/// Records `scenarios` of `channel` with what the test observed for each
fn record(channel: &str, seen: &serde_json::Map<String, serde_json::Value>, scenarios: &[&str]) {
    let observed = scenarios.iter().map(|scenario| {
        let seen = seen
            .get(*scenario)
            .unwrap_or_else(|| panic!("scenario {scenario} was never driven"));
        ((*scenario).to_owned(), seen.clone())
    });
    let observed = serde_json::Value::Object(observed.collect()).to_string();
    shell_contract::native_evidence::record(channel, scenarios, &observed);
}

#[test]
fn native_evidence_captured_recovery_completes_before_navigation_and_revocation_blocks_seed() {
    let mut consume_seen = serde_json::Map::new();
    let mut init_seen = serde_json::Map::new();
    for (revoke, failed, replaced, renamed, new_document) in [
        (false, false, false, false, false),
        (false, true, false, false, false),
        (false, false, true, false, false),
        (false, false, false, true, false),
        (false, false, false, false, true),
        (true, false, false, false, false),
    ] {
        let plain = !(revoke || failed || replaced || renamed || new_document);
        let mut rt = Rt::new(|hosts| {
            hosts.insert::<PdfEngineHost>(fakes::engine(&serde_json::Value::Null));
            hosts.insert::<PdfShellHost>(fakes::shell(&serde_json::Value::Null));
            hosts.insert(Arc::new(Mutex::new(
                main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new(
                    "test",
                ),
            )));
            crate::pdf::tools::fakes(hosts, &serde_json::Value::Null);
        });
        let workers = Arc::new(Deferred::default());
        rt.services.workers = workers.clone();
        rt.register(crate::pdf::entries).unwrap();
        let source = rt.view(1, Surface::Shell);
        let destination = rt.view(2, Surface::Pdf);
        assert_eq!(
            rt.invoke_now("pdf:consume-recovery-pending", destination, vec![])
                .unwrap(),
            shell_contract::IpcValue::Null
        );
        consume_seen.insert(
            "ordinary-empty".into(),
            serde_json::json!({"answer": "null"}),
        );
        let source_generation = rt.services.views.renderer_generation(source.view).unwrap();
        let source_owner = rt
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .unwrap()
            .capture_owner(source.view, &source_generation)
            .unwrap();
        let created = rt
            .services
            .views
            .renderer_generation(destination.view)
            .unwrap();
        let (issuer, closure, browser_owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(
                destination.view,
                created.clone(),
            );
        let target = RecoveryOpenTarget::bind(
            RecoveryOpenContext::Renderer {
                source: source_owner,
                participants: vec![],
                cancel: CancelToken::default(),
                pending: CancelToken::default(),
            },
            destination.view,
            created.clone(),
            closure,
            browser_owner,
        )
        .unwrap();
        let identity = file_identity("/missing.pdf");
        let copy =
            recovery_copy_path(&rt.dir.join("userData").to_string_lossy(), &identity).unwrap();
        std::fs::create_dir_all(std::path::Path::new(&copy).parent().unwrap()).unwrap();
        std::fs::write(&copy, b"%PDF recovered").unwrap();
        let request = RecoveryOpenUntitledRequest {
            copy: identity,
            title: "Recovered".into(),
            save_as_default_path: Some("/missing.pdf".into()),
        };
        let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
        let result = Rc::new(RefCell::new(None));
        let completed = result.clone();
        <PdfEditors as RecoveryEditorContract<_, RecoveryOpenTarget>>::open_untitled_copy(
            &editor,
            &destination.view,
            &request,
            &target,
            Box::new(move |answer| *completed.borrow_mut() = Some(answer)),
        );
        assert!(result.borrow().is_none());
        let first = workers.0.lock().unwrap().remove(0);
        first();
        if revoke {
            source_generation.revoke();
        }
        rt.drain();
        loop {
            let job = {
                let mut pending = workers.0.lock().unwrap();
                if pending.is_empty() {
                    None
                } else {
                    Some(pending.remove(0))
                }
            };
            let Some(job) = job else { break };
            job();
            rt.drain();
        }
        let answer = result.borrow_mut().take().unwrap();
        assert_eq!(answer.is_ok(), !revoke);
        let path = editor.0.open_path(destination.view);
        if revoke {
            assert!(path.is_none());
            let after = rt.invoke_now("pdf:consume-recovery-pending", destination, vec![]);
            assert!(!matches!(after, Ok(IpcValue::Object(_))));
            consume_seen.insert(
                "preparation-loss".into(),
                serde_json::json!({
                    "open": format!("{:?}", answer.as_ref().err()),
                    "document_path": null,
                    "consume_after_loss": format!("{after:?}"),
                }),
            );
        } else {
            let path = path.unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"%PDF recovered");
            assert!(editor.0.is_untitled_copy(&path));
            let receipt = target.committed_seed().unwrap();
            assert!(receipt.settlement().is_none());
            let synthetic = rt
                .services
                .hosts
                .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
                .unwrap()
                .clone();
            let provenance = shell_contract::hosts::synthetic::SyntheticNativeOrigin::new();
            let receiver = synthetic
                .capture_invocation_root(destination.view, &created, &provenance)
                .unwrap();
            target.retain_native_registration(receiver.clone()).unwrap();
            let mut before_consume = String::new();
            if plain {
                let early = rt.invoke_native(
                    "pdf:read-file",
                    destination,
                    vec![IpcValue::String(path.clone())],
                    receiver.context.clone(),
                    CancelToken::default(),
                );
                before_consume = refusal(&early);
                assert!(before_consume.contains("captured seed"), "{before_consume}");
            }
            let foreign = shell_contract::ipc::NativeContext::with_renderer(
                (),
                shell_contract::runtime::RendererGeneration::new(),
            );
            let foreign_consume = refusal(&rt.invoke_native(
                "pdf:consume-recovery-pending",
                destination,
                vec![],
                foreign,
                CancelToken::default(),
            ));
            consume_seen.insert(
                "foreign-renderer".into(),
                serde_json::json!({"error": foreign_consume}),
            );
            assert!(receipt.settlement().is_none());
            let pending = rt
                .invoke_native(
                    "pdf:consume-recovery-pending",
                    destination,
                    vec![],
                    receiver.context.clone(),
                    CancelToken::default(),
                )
                .take()
                .unwrap()
                .unwrap();
            let shell_contract::IpcValue::Object(fields) = pending else {
                panic!("expected a recovery seed")
            };
            let token = fields
                .iter()
                .find(|(key, _)| key == "token")
                .unwrap()
                .1
                .clone();
            let IpcValue::String(token_text) = &token else {
                panic!("the recovery token is a string")
            };
            assert!(!token_text.is_empty());
            assert_eq!(
                fields.iter().find(|(key, _)| key == "path").unwrap().1,
                IpcValue::String(path.clone())
            );
            consume_seen.insert(
                "captured-consume".into(),
                serde_json::json!({"path": path, "token": token_text}),
            );
            let duplicate_consume = refusal(&rt.invoke_native(
                "pdf:consume-recovery-pending",
                destination,
                vec![],
                receiver.context.clone(),
                CancelToken::default(),
            ));
            consume_seen.insert(
                "duplicate-consume".into(),
                serde_json::json!({"error": duplicate_consume, "settled": false}),
            );
            assert!(receipt.settlement().is_none());
            let missing_token = refusal(&rt.invoke_native(
                "pdf:recovery-initialized",
                destination,
                vec![],
                receiver.context.clone(),
                CancelToken::default(),
            ));
            init_seen.insert(
                "missing-token".into(),
                serde_json::json!({"error": missing_token, "settled": false}),
            );
            assert!(receipt.settlement().is_none());
            let foreign_token = refusal(&rt.invoke_native(
                "pdf:recovery-initialized",
                destination,
                vec![IpcValue::String("foreign".into()), IpcValue::Bool(true)],
                receiver.context.clone(),
                CancelToken::default(),
            ));
            init_seen.insert(
                "foreign-token".into(),
                serde_json::json!({"error": foreign_token, "settled": false}),
            );
            assert!(receipt.settlement().is_none());
            let foreign = shell_contract::ipc::NativeContext::with_renderer(
                (),
                shell_contract::runtime::RendererGeneration::new(),
            );
            let foreign_ack = refusal(&rt.invoke_native(
                "pdf:recovery-initialized",
                destination,
                vec![token.clone(), IpcValue::Bool(true)],
                foreign,
                CancelToken::default(),
            ));
            init_seen.insert(
                "foreign-renderer".into(),
                serde_json::json!({"error": foreign_ack, "settled": false}),
            );
            assert!(receipt.settlement().is_none());
            if plain {
                let reading = rt.invoke_native(
                    "pdf:read-file",
                    destination,
                    vec![IpcValue::String(path.clone())],
                    receiver.context.clone(),
                    CancelToken::default(),
                );
                assert!(reading.take().is_none());
                for job in std::mem::take(&mut *workers.0.lock().unwrap()) {
                    job();
                }
                rt.drain();
                let Ok(IpcValue::Bytes(_, applied)) = reading.take().unwrap() else {
                    panic!("the renderer's read of its recovered file was refused")
                };
                assert_eq!(applied, b"%PDF recovered");
                assert!(receipt.settlement().is_none());
                consume_seen.insert(
                    "renderer-apply".into(),
                    serde_json::json!({
                        "read_before_consume": before_consume,
                        "read_after_consume": "%PDF recovered",
                        "settled_by_read": false,
                    }),
                );
                init_seen.insert(
                    "renderer-apply".into(),
                    serde_json::json!({
                        "read_before_ack": "%PDF recovered",
                        "settled_by_read": false,
                    }),
                );
            }
            if renamed {
                let destination_owner = editor
                    .capture_document_owner(destination.view, receiver.clone())
                    .unwrap();
                let independent_alias = editor
                    .capture_document_owner(destination.view, receiver.clone())
                    .unwrap();
                let moved = rt.dir.join("renamed-recovery.pdf");
                std::fs::rename(&path, &moved).unwrap();
                let outcome = Rc::new(RefCell::new(None));
                let completed = outcome.clone();
                editor.file_renamed_recovery(
                    destination.view,
                    std::path::Path::new(&path),
                    &moved,
                    &receipt,
                    &receiver,
                    &destination_owner,
                    &[independent_alias],
                    &CancelToken::default(),
                    &CancelToken::default(),
                    Box::new(move |result| *completed.borrow_mut() = Some(result)),
                );
                assert!(
                    rt.invoke_native(
                        "pdf:read-file",
                        destination,
                        vec![shell_contract::IpcValue::String(path.clone())],
                        receiver.context.clone(),
                        CancelToken::default()
                    )
                    .take()
                    .unwrap()
                    .is_err()
                );
                let reading = rt.invoke_native(
                    "pdf:read-file",
                    destination,
                    vec![shell_contract::IpcValue::String(
                        moved.to_string_lossy().into_owned(),
                    )],
                    receiver.context.clone(),
                    CancelToken::default(),
                );
                assert!(reading.take().is_none());
                for job in std::mem::take(&mut *workers.0.lock().unwrap()) {
                    job();
                }
                rt.drain();
                assert!(reading.take().unwrap().is_ok());
                assert!(outcome.borrow_mut().take().unwrap().is_ok());
                assert!(receipt.settlement().is_none());
                assert_eq!(
                    editor.document_path(destination.view).as_deref(),
                    Some(moved.as_path())
                );
            }
            if new_document {
                let replacement = rt.dir.join("replacement.pdf");
                std::fs::write(&replacement, b"%PDF replacement").unwrap();
                editor.0.replace_document(
                    destination.view,
                    Some(replacement.to_string_lossy().into_owned()),
                );
                assert!(receipt.settlement().is_some_and(|result| result.is_err()));
                let retirement = editor.recovery_retirement(target.clone()).unwrap();
                let closed = Rc::new(RefCell::new(None));
                let closed_out = closed.clone();
                retirement.retire_closed(
                    issuer.closed(),
                    Box::new(move |result| *closed_out.borrow_mut() = Some(result)),
                );
                rt.drain();
                for job in std::mem::take(&mut *workers.0.lock().unwrap()) {
                    job();
                }
                rt.drain();
                assert!(closed.borrow().as_ref().unwrap().is_ok());
                assert_eq!(
                    editor.document_path(destination.view).as_deref(),
                    Some(replacement.as_path())
                );
                assert_eq!(std::fs::read(&replacement).unwrap(), b"%PDF replacement");
            } else if replaced {
                editor.0.advance_document_revision(destination.view);
                assert!(receipt.settlement().is_some_and(|result| result.is_err()));
                let loss = receipt.settlement().unwrap().unwrap_err();
                let late_ack = refusal(&rt.invoke_native(
                    "pdf:recovery-initialized",
                    destination,
                    vec![token.clone(), IpcValue::Bool(true)],
                    receiver.context.clone(),
                    CancelToken::default(),
                ));
                init_seen.insert(
                    "revision-loss".into(),
                    serde_json::json!({"settlement": {"err": loss}, "late_ack_error": late_ack}),
                );
            } else if failed {
                let answer = rt
                    .invoke_native(
                        "pdf:recovery-initialized",
                        destination,
                        vec![
                            token.clone(),
                            IpcValue::Bool(false),
                            IpcValue::String("parse failed".into()),
                        ],
                        receiver.context.clone(),
                        CancelToken::default(),
                    )
                    .take()
                    .unwrap()
                    .unwrap();
                assert_eq!(answer, IpcValue::Null);
                assert_eq!(receipt.settlement(), Some(Err("parse failed".into())));
                rt.destroy(destination.view);
                assert_eq!(std::fs::read(&path).unwrap(), b"%PDF recovered");
                assert_eq!(std::fs::read(&copy).unwrap(), b"%PDF recovered");
                init_seen.insert(
                    "parse-failure".into(),
                    serde_json::json!({
                        "answer": "null",
                        "settlement": {"err": "parse failed"},
                        "recovery_copy_kept": true,
                    }),
                );
            } else {
                let answer = rt
                    .invoke_native(
                        "pdf:recovery-initialized",
                        destination,
                        vec![token.clone(), IpcValue::Bool(true)],
                        receiver.context.clone(),
                        CancelToken::default(),
                    )
                    .take()
                    .unwrap()
                    .unwrap();
                assert_eq!(answer, IpcValue::Null);
                assert_eq!(receipt.settlement(), Some(Ok(())));
                let expected = editor
                    .document_path(destination.view)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                assert_eq!(
                    rt.invoke_now("pdf:consume-pending", destination, vec![])
                        .unwrap(),
                    IpcValue::String(expected.clone())
                );
                if plain {
                    init_seen.insert(
                        "successful-ack".into(),
                        serde_json::json!({
                            "answer": "null",
                            "settlement": "ok",
                            "navigation_path": expected,
                        }),
                    );
                }
            }
            let second_ack = refusal(&rt.invoke_native(
                "pdf:recovery-initialized",
                destination,
                vec![token, IpcValue::Bool(true)],
                receiver.context,
                CancelToken::default(),
            ));
            if plain {
                init_seen.insert(
                    "duplicate-ack".into(),
                    serde_json::json!({"error": second_ack, "settlement": "ok"}),
                );
            }
        }
        assert!(
            !rt.script()
                .calls
                .iter()
                .any(|call| matches!(call, crate::testing::HostCall::ReloadPdf(_)))
        );
    }
    record(
        "pdf:consume-recovery-pending",
        &consume_seen,
        &[
            "ordinary-empty",
            "captured-consume",
            "duplicate-consume",
            "foreign-renderer",
            "preparation-loss",
            "renderer-apply",
        ],
    );
    record(
        "pdf:recovery-initialized",
        &init_seen,
        &[
            "successful-ack",
            "missing-token",
            "foreign-token",
            "foreign-renderer",
            "duplicate-ack",
            "parse-failure",
            "revision-loss",
            "renderer-apply",
        ],
    );
}

#[test]
fn exact_closed_receipt_waits_for_preparation_and_removes_only_its_work_file() {
    let mut rt = Rt::new(|hosts| {
        hosts.insert::<PdfEngineHost>(fakes::engine(&serde_json::Value::Null));
        hosts.insert::<PdfShellHost>(fakes::shell(&serde_json::Value::Null));
        hosts.insert(Arc::new(Mutex::new(
            main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"),
        )));
        crate::pdf::tools::fakes(hosts, &serde_json::Value::Null);
    });
    let workers = Arc::new(Deferred::default());
    rt.services.workers = workers.clone();
    rt.register(crate::pdf::entries).unwrap();
    let source = rt.view(1, Surface::Shell);
    let destination = rt.view(2, Surface::Pdf);
    let effects = rt.services.hosts.get::<OwnedEffectsHost>().unwrap();
    let source_generation = rt.services.views.renderer_generation(source.view).unwrap();
    let owner = effects
        .capture_owner(source.view, &source_generation)
        .unwrap();
    let created = rt
        .services
        .views
        .renderer_generation(destination.view)
        .unwrap();
    let (issuer, reservation, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(destination.view, created.clone());
    let target = RecoveryOpenTarget::bind(
        RecoveryOpenContext::Renderer {
            source: owner,
            participants: vec![],
            cancel: CancelToken::default(),
            pending: CancelToken::default(),
        },
        destination.view,
        created.clone(),
        reservation,
        browser_owner,
    )
    .unwrap();
    let identity = file_identity("/missing-closed.pdf");
    let copy = recovery_copy_path(&rt.dir.join("userData").to_string_lossy(), &identity).unwrap();
    std::fs::create_dir_all(std::path::Path::new(&copy).parent().unwrap()).unwrap();
    std::fs::write(&copy, b"%PDF preserved").unwrap();
    let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
    let retirement = editor.recovery_retirement(target.clone()).unwrap();
    let opened = Rc::new(RefCell::new(None));
    let opened_out = opened.clone();
    let request = RecoveryOpenUntitledRequest {
        copy: identity,
        title: "Recovery".into(),
        save_as_default_path: None,
    };
    <PdfEditors as RecoveryEditorContract<_, RecoveryOpenTarget>>::open_untitled_copy(
        &editor,
        &destination.view,
        &request,
        &target,
        Box::new(move |result| *opened_out.borrow_mut() = Some(result)),
    );
    let job = workers.0.lock().unwrap().remove(0);
    job();
    let work_dir = rt
        .services
        .paths
        .path(shell_contract::runtime::PathName::Temp)
        .join("seuoffice-pdf-recovery");
    let work = std::fs::read_dir(work_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let (foreign, _) =
        shell_contract::ipc::NativeClosureIssuer::new(destination.view, created.clone());
    let refused = Rc::new(RefCell::new(None));
    let refused_out = refused.clone();
    retirement.retire_closed(
        foreign.closed(),
        Box::new(move |result| *refused_out.borrow_mut() = Some(result)),
    );
    assert!(refused.borrow().as_ref().unwrap().is_err());
    assert!(work.exists());
    let (_foreign_issuer, foreign_reservation, foreign_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(destination.view, created.clone());
    foreign_reservation
        .with_initial_owner(&foreign_owner, |_| Ok(()))
        .unwrap();
    let (_, foreign_supersession, ()) = foreign_reservation
        .prepare_replacement(&foreign_owner)
        .unwrap()
        .commit(|_, _| Ok(()))
        .unwrap();
    let rejected = Rc::new(RefCell::new(None));
    let rejected_out = rejected.clone();
    retirement.retire_superseded(
        foreign_supersession,
        Box::new(move |result| *rejected_out.borrow_mut() = Some(result)),
    );
    assert!(rejected.borrow().as_ref().unwrap().is_err());
    assert!(work.exists());
    let claim = target
        .closure_reservation()
        .claim_close(target.browser_owner(), |transition| {
            retirement.claim_close_in_transition(transition, target.browser_owner())
        })
        .unwrap();
    let mut permit = match claim {
        shell_contract::ipc::NativeBrowserCloseClaim::Won { permit, .. } => permit,
        shell_contract::ipc::NativeBrowserCloseClaim::Superseded(_) => {
            panic!("original recovery slot unexpectedly superseded")
        }
    };
    assert!(
        target
            .closure_reservation()
            .prepare_replacement(target.browser_owner())
            .unwrap()
            .commit(|_, _| Ok(()))
            .is_err()
    );
    permit.begin_dispatch().unwrap();
    created.revoke();
    let closed = Rc::new(RefCell::new(None));
    let closed_out = closed.clone();
    let proof = issuer.closed();
    retirement.retire_closed(
        proof.clone(),
        Box::new(move |result| *closed_out.borrow_mut() = Some(result)),
    );
    assert!(closed.borrow().is_none());
    rt.drain();
    assert!(opened.borrow().as_ref().unwrap().is_err());
    assert!(work.exists());
    std::fs::remove_file(&work).unwrap();
    std::fs::create_dir(&work).unwrap();
    for job in std::mem::take(&mut *workers.0.lock().unwrap()) {
        job();
    }
    rt.drain();
    assert!(closed.borrow().as_ref().unwrap().is_err());
    std::fs::remove_dir(&work).unwrap();
    std::fs::write(&work, b"%PDF retry").unwrap();
    let retried = Rc::new(RefCell::new(None));
    let retried_out = retried.clone();
    retirement.retire_closed(
        proof,
        Box::new(move |result| *retried_out.borrow_mut() = Some(result)),
    );
    rt.drain();
    for job in std::mem::take(&mut *workers.0.lock().unwrap()) {
        job();
    }
    rt.drain();
    assert!(retried.borrow().as_ref().unwrap().is_ok());
    assert!(!work.exists());
    assert_eq!(std::fs::read(copy).unwrap(), b"%PDF preserved");
}

#[test]
fn native_evidence_recovery_routes_reject_foreign_surface_in_the_real_dispatcher() {
    let rt = Rt::new(|hosts| {
        hosts.insert::<PdfEngineHost>(fakes::engine(&serde_json::Value::Null));
        hosts.insert::<PdfShellHost>(fakes::shell(&serde_json::Value::Null));
        hosts.insert(Arc::new(Mutex::new(
            main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"),
        )));
        crate::pdf::tools::fakes(hosts, &serde_json::Value::Null);
    });
    let foreign = rt.view(5, Surface::Docs);
    let mut table = shell_core::table::Table::default();
    let provided = shell_contract::ipc::Registry::provided(&table);
    for entry in crate::pdf::entries(&rt.services, &provided).unwrap() {
        shell_contract::ipc::Registry::register(
            &mut table,
            shell_contract::Batch::D2,
            entry.channel,
            entry.kind,
            entry.handler,
        )
        .unwrap();
    }
    let dispatcher = shell_core::dispatch::Dispatcher::new(table);
    let channels = ["pdf:consume-recovery-pending", "pdf:recovery-initialized"];
    let refusals = channels.map(|channel| {
        let (send, receive) = std::sync::mpsc::channel();
        dispatcher.dispatch(
            foreign,
            shell_contract::ipc::ChannelKind::Invoke,
            Some(shell_contract::RequestId(1)),
            None,
            channel,
            vec![],
            Box::new(move |result| send.send(result).unwrap()),
        );
        let message = receive.try_recv().unwrap().unwrap_err().message;
        assert!(message.contains("not open to Docs"), "{message}");
        message
    });
    for (channel, message) in channels.into_iter().zip(refusals) {
        let observed =
            serde_json::json!({"unauthorized-surface": {"from": "docs", "error": message}});
        shell_contract::native_evidence::record(
            channel,
            &["unauthorized-surface"],
            &observed.to_string(),
        );
    }
}
