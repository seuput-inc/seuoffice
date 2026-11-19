use super::super::{
    editors::PdfEditors,
    fakes,
    hosts::{PdfEngineHost, PdfShellHost},
};
use crate::testing::Rt;
use shell_contract::{
    Surface,
    editors::DocumentEditors,
    hosts::OwnedEffectsHost,
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

#[test]
fn rename_retains_every_captured_participant_through_worker_and_renderer_publication() {
    for scenario in 0..3 {
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
        let target = rt.view(1, Surface::Pdf);
        let source = rt.view(2, Surface::Shell);
        let extra = [
            rt.view(3, Surface::Shell),
            rt.view(4, Surface::Shell),
            rt.view(5, Surface::Shell),
        ];
        let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
        let old = rt.dir.join("before.pdf");
        let new = rt.dir.join("after.pdf");
        std::fs::write(&old, b"%PDF old").unwrap();
        std::fs::write(&new, b"%PDF moved").unwrap();
        editor.open_view(target.view, old.to_str());
        let effects = rt.services.hosts.get::<OwnedEffectsHost>().unwrap().clone();
        let capture = |view| {
            effects
                .capture_owner(view, &rt.services.views.renderer_generation(view).unwrap())
                .unwrap()
        };
        let origin = capture(source.view);
        let destination = editor
            .capture_document_owner(target.view, capture(target.view))
            .unwrap();
        let mut participants = extra
            .iter()
            .map(|sender| capture(sender.view))
            .collect::<Vec<_>>();
        participants.push(origin.clone());
        participants.push(destination.clone());
        let extra_cancel = CancelToken::default();
        let result = Rc::new(RefCell::new(None));
        let completed = result.clone();
        editor.file_renamed_many_owned_extra(
            target.view,
            &old,
            &new,
            &origin,
            &destination,
            &participants,
            &CancelToken::default(),
            &CancelToken::default(),
            std::slice::from_ref(&extra_cancel),
            Box::new(move |answer| *completed.borrow_mut() = Some(answer)),
        );
        assert!(result.borrow().is_none());
        if scenario == 1 {
            rt.destroy(extra[2].view);
        }
        if scenario == 2 {
            extra_cancel.cancel();
        }
        let jobs = std::mem::take(&mut *workers.0.lock().unwrap());
        assert_eq!(jobs.len(), 1);
        for job in jobs {
            job();
        }
        rt.drain();
        assert_eq!(result.borrow_mut().take().unwrap().is_ok(), scenario == 0);
        let renamed = rt
            .events()
            .into_iter()
            .filter(|event| event.channel == "pdf:file-renamed")
            .collect::<Vec<_>>();
        assert_eq!(renamed.len(), usize::from(scenario == 0));
        if scenario == 0 {
            assert_eq!(
                editor.document_path(target.view).as_deref(),
                Some(new.as_path())
            );
            assert_eq!(
                renamed[0].args,
                vec![shell_contract::IpcValue::String(
                    new.to_string_lossy().into_owned()
                )]
            );
        }
    }
}

#[test]
fn recovery_rename_uses_the_captured_receiver_and_every_participant() {
    for revoked in [false, true] {
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
        let pdf = rt.view(1, Surface::Pdf);
        let source = rt.view(2, Surface::Docs);
        let home = rt.view(3, Surface::Shell);
        let extra = rt.view(4, Surface::Shell);
        let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
        let old = rt.dir.join("recovery-before.pdf");
        let new = rt.dir.join("recovery-after.pdf");
        std::fs::write(&old, b"%PDF before").unwrap();
        std::fs::write(&new, b"%PDF after").unwrap();
        editor.open_view(pdf.view, old.to_str());
        let effects = rt.services.hosts.get::<OwnedEffectsHost>().unwrap().clone();
        let capture = |view| {
            effects
                .capture_owner(view, &rt.services.views.renderer_generation(view).unwrap())
                .unwrap()
        };
        let origin = capture(home.view);
        let created = rt.services.views.renderer_generation(source.view).unwrap();
        let (_issuer, closure, browser_owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(source.view, created.clone());
        let target = shell_contract::ipc::RecoveryOpenTarget::bind(
            shell_contract::ipc::RecoveryOpenContext::Renderer {
                source: origin,
                participants: vec![],
                cancel: CancelToken::default(),
                pending: CancelToken::default(),
            },
            source.view,
            created.clone(),
            closure,
            browser_owner,
        )
        .unwrap();
        let mut enqueued = false;
        let receipt = effects
            .commit_recovery_seed(&target, &mut || {
                enqueued = true;
                Ok(())
            })
            .unwrap();
        assert!(enqueued);
        let synthetic = rt
            .services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .unwrap();
        let provenance = shell_contract::hosts::synthetic::SyntheticNativeOrigin::new();
        let receiver = synthetic
            .capture_invocation_root(source.view, &created, &provenance)
            .unwrap();
        target.retain_native_registration(receiver.clone()).unwrap();
        let destination = editor
            .capture_document_owner(pdf.view, capture(pdf.view))
            .unwrap();
        let participant = capture(extra.view);
        let result = Rc::new(RefCell::new(None));
        let completed = result.clone();
        editor.file_renamed_recovery(
            pdf.view,
            &old,
            &new,
            &receipt,
            &receiver,
            &destination,
            &[participant],
            &CancelToken::default(),
            &CancelToken::default(),
            Box::new(move |answer| *completed.borrow_mut() = Some(answer)),
        );
        if revoked {
            rt.destroy(extra.view);
        }
        for job in std::mem::take(&mut *workers.0.lock().unwrap()) {
            job();
        }
        rt.drain();
        assert_eq!(result.borrow_mut().take().unwrap().is_ok(), !revoked);
        let events = rt
            .events()
            .into_iter()
            .filter(|event| event.channel == "pdf:file-renamed")
            .count();
        assert_eq!(events, usize::from(!revoked));
    }
}

#[test]
fn native_rename_delivers_exact_revision_receipt_before_old_snapshot_loss() {
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeSaveDestination, NativeSaveRevisionAuthority,
        NativeSaveSelection,
    };
    for scenario in 0..3 {
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
        let target = rt.view(1, Surface::Pdf);
        let editor = rt.provided().all::<PdfEditors>().pop().unwrap();
        let old = rt.dir.join("before.pdf");
        let new = rt.dir.join("after.pdf");
        std::fs::write(&old, b"%PDF old").unwrap();
        std::fs::write(&new, b"%PDF moved").unwrap();
        editor.open_view(target.view, old.to_str());
        let effects = rt.services.hosts.get::<OwnedEffectsHost>().unwrap();
        let mut owner = effects
            .capture_owner(
                target.view,
                &rt.services.views.renderer_generation(target.view).unwrap(),
            )
            .unwrap();
        owner.context = owner
            .context
            .with_document_lease(shell_contract::runtime::RendererGeneration::new());
        let owner = editor.capture_document_owner(target.view, owner).unwrap();
        let binding = NativeInvocationIssuer::new(CancelToken::default())
            .issue_existing_document_save(
                Surface::Pdf,
                target.view,
                NativeSaveSelection::ExactPath(new.clone()),
                owner.clone(),
                CancelToken::default(),
            )
            .unwrap();
        let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
        let cell = NativeSaveRevisionAuthority::new(selection, vec![owner.clone()]);
        let snapshot = cell.snapshot().unwrap();
        let cancel = CancelToken::default();
        let answer = Rc::new(RefCell::new(None));
        let recorded = answer.clone();
        let callback_owner = owner.clone();
        let old_loss = snapshot.loss_token();
        let expected = if scenario == 1 {
            old.as_path()
        } else {
            new.as_path()
        };
        editor.file_renamed_existing_document(
            target.view,
            &old,
            &new,
            &cell,
            expected,
            &owner,
            std::slice::from_ref(&owner),
            &cancel,
            &CancelToken::default(),
            &[],
            Box::new(move |result| {
                let observed = result.map(|receipt| {
                    receipt.map(|receipt| {
                        let successor = receipt.renewed_request_owner(&callback_owner).unwrap();
                        (
                            !old_loss.is_cancelled(),
                            successor
                                .pdf_revision()
                                .unwrap()
                                .same_document(callback_owner.pdf_revision().unwrap()),
                        )
                    })
                });
                *recorded.borrow_mut() = Some(observed);
            }),
        );
        if scenario == 2 {
            cancel.cancel();
        }
        for job in std::mem::take(&mut *workers.0.lock().unwrap()) {
            job();
        }
        rt.drain();
        let result = answer.borrow_mut().take().unwrap();
        if scenario == 0 {
            assert_eq!(result.unwrap(), Some((true, false)));
            assert!(snapshot.loss_token().is_cancelled());
        } else {
            assert!(result.is_err());
            assert!(!snapshot.loss_token().is_cancelled());
        }
    }
}
