use super::super::{
    editors::PdfEditors,
    fakes,
    hosts::{PdfEngineHost, PdfShellHost},
};
use crate::{kit::field, testing::Rt};
use shell_contract::{
    Surface,
    hosts::{OwnedEffectsHost, SavedSessionReceipt},
    ipc::Responder,
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
fn adopted_pdf_publishes_only_after_actor_ack_and_never_after_cancellation() {
    for cancelled in [false, true] {
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
        let sender = rt.view(1, Surface::Pdf);
        let old = rt.dir.join("untitled.pdf").to_string_lossy().into_owned();
        let target = rt.dir.join("saved.pdf").to_string_lossy().into_owned();
        let editor = rt.provided().all::<PdfEditors>().pop().unwrap().0.clone();
        editor.replace_document(sender.view, Some(old.clone()));
        editor.st.borrow_mut().untitled_copy.insert(old.clone());
        editor
            .st
            .borrow_mut()
            .unsaved_untitled_copy
            .insert(old.clone());
        let old_revision = editor.st.borrow().views[&sender.view]
            .document_revision
            .clone()
            .unwrap();
        let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
        let origin = rt
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .unwrap()
            .capture_owner(sender.view, &renderer)
            .unwrap();
        let cancel = CancelToken::default();
        let authority = editor
            .capture_save_authority(sender.view, origin, cancel.clone())
            .unwrap();
        let publications = Rc::new(RefCell::new(0));
        let published = publications.clone();
        let observed = editor.clone();
        let final_path = target.clone();
        let original = old.clone();
        let session = SavedSessionReceipt::Pending(Box::new(move |done| {
            assert_eq!(
                observed.open_path(sender.view).as_deref(),
                Some(final_path.as_str())
            );
            assert!(!observed.is_untitled_copy(&original));
            assert!(
                !observed
                    .st
                    .borrow()
                    .unsaved_untitled_copy
                    .contains(&original)
            );
            assert!(!old_revision.is_live());
            *published.borrow_mut() += 1;
            done(Ok(()));
        }));
        let answer = Arc::new(Mutex::new(None));
        let answered = answer.clone();
        editor.after_adopt_owned_ready(
            sender.view,
            old,
            target.clone(),
            authority,
            session,
            Responder::new(move |result| *answered.lock().unwrap() = Some(result)),
        );
        assert_eq!(*publications.borrow(), 0);
        assert!(answer.lock().unwrap().is_none());
        if cancelled {
            cancel.cancel();
        }
        let jobs = std::mem::take(&mut *workers.0.lock().unwrap());
        assert_eq!(jobs.len(), 1);
        for job in jobs {
            job();
        }
        rt.drain();
        let reply = answer.lock().unwrap().take().unwrap().unwrap();
        assert_eq!(
            field(&reply, "ok"),
            &shell_contract::IpcValue::Bool(!cancelled)
        );
        assert_eq!(*publications.borrow(), usize::from(!cancelled));
        if !cancelled {
            assert_eq!(
                field(&reply, "path"),
                &shell_contract::IpcValue::String(target)
            );
        }
    }
}
