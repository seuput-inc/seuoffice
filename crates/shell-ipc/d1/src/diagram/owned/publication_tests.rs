use super::*;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use shell_contract::Surface;
use shell_contract::hosts::{SavedFile, SavedSessionReceipt};
use crate::testkit::{Kit, temp_dir};

fn fixture(name: &str) -> (Kit, DiagramViews, ViewId, OwnedSave) {
    let root = temp_dir(name);
    let kit = Kit::new(&root, crate::insert_fakes);
    let sender = kit.view(101, Surface::Diagram);
    let views = DiagramViews::new(&kit.services).unwrap();
    views.open_view(sender.view, None);
    let effects = kit.services.hosts.need::<OwnedEffectsHost>(Batch::D1).unwrap();
    let renderer = kit.services.views.renderer_generation(sender.view).unwrap();
    let destination = effects.capture_owner(sender.view, &renderer).unwrap();
    let document = views.with_state(|state| state.view(sender.view).document.clone()).unwrap();
    let scope = OwnedSave::new(destination.clone(), destination, CancelToken::default(), document, effects).unwrap();
    (kit, views, sender.view, scope)
}

#[test]
fn publication_stays_core_only_and_follows_final_editor_and_recovery_ack() {
    let (kit, views, view, scope) = fixture("diagram-publication-order");
    let path = kit.root.join("saved.drawio").to_str().unwrap().to_owned();
    std::fs::write(&path, b"saved").unwrap();
    views.with_state(|state| state.recovery_of(view).title = Some("untitled".into()));
    let published = Rc::new(Cell::new(false));
    let observed = published.clone();
    let owner = views.clone();
    let final_path = path.clone();
    let session = SavedSessionReceipt::Pending(Box::new(move |done| {
        assert_eq!(owner.file_path(view).as_deref(), Some(final_path.as_str()));
        assert!(owner.with_state(|state| state.recovery_of(view).title.is_none()));
        observed.set(true);
        done(Ok(()));
    }));
    let answer = Arc::new(Mutex::new(None));
    let output = answer.clone();
    kit.hold_workers();
    views.complete_owned_diagram(view, None, scope, SavedFile { path, session }, None,
        Responder::new(move |result| *output.lock().unwrap() = Some(result)));
    assert!(!published.get());
    assert!(answer.lock().unwrap().is_none());
    kit.release_workers();
    assert!(published.get());
    assert!(answer.lock().unwrap().as_ref().unwrap().is_ok());
}

#[test]
fn replaced_document_drops_pending_publication_instead_of_publishing_a_new_view_identity() {
    let (kit, views, view, scope) = fixture("diagram-publication-replaced");
    let path = kit.root.join("saved.drawio").to_str().unwrap().to_owned();
    std::fs::write(&path, b"saved").unwrap();
    let published = Rc::new(Cell::new(false));
    let observed = published.clone();
    let session = SavedSessionReceipt::Pending(Box::new(move |done| {
        observed.set(true);
        done(Ok(()));
    }));
    let answer = Arc::new(Mutex::new(None));
    let output = answer.clone();
    kit.hold_workers();
    views.complete_owned_diagram(view, None, scope, SavedFile { path, session }, None,
        Responder::new(move |result| *output.lock().unwrap() = Some(result)));
    views.open_view(view, None);
    kit.release_workers();
    assert!(!published.get());
    assert!(answer.lock().unwrap().as_ref().unwrap().is_err());
    assert!(views.file_path(view).is_none());
}
