use super::*;
use std::cell::RefCell;

#[derive(Clone)]
struct Deps {
    loaded: Rc<Cell<usize>>,
    saved: Rc<RefCell<Vec<DocumentLinksFile>>>,
    now: f64,
}
impl Deps {
    fn new() -> Self {
        Self {
            loaded: Rc::new(Cell::new(0)),
            saved: Rc::new(RefCell::new(Vec::new())),
            now: 1_700_000_000_000.0,
        }
    }
}
impl DocumentLinksDeps for Deps {
    fn load(&mut self) -> Vec<DocumentLink> {
        self.loaded.set(self.loaded.get() + 1);
        Vec::new()
    }
    fn save(&mut self, file: &DocumentLinksFile) -> Result<(), String> {
        self.saved.borrow_mut().push(file.clone());
        Ok(())
    }
    fn events(&mut self) -> Vec<MakeEvent> {
        vec![MakeEvent {
            web_contents_id: 7.0,
            mode: DocumentLinkVia::Ai,
            sources: vec![Some("/source/😀.txt".into())],
            at: 10.0,
        }]
    }
    fn now_ms(&mut self) -> f64 {
        self.now
    }
    fn warn(&mut self, _: &str, _: &str) {}
}
struct Contents(f64);
impl LinkedWebContents for Contents {
    fn id(&self) -> f64 {
        self.0
    }
    fn once_destroyed(&mut self) {}
}

#[test]
fn worker_snapshot_reuses_the_original_join_and_defers_live_listeners() {
    fn assert_send<T: Send>() {}
    assert_send::<DocumentLinksSnapshot>();
    let deps = Deps::new();
    let mut live = DocumentLinks::new(deps.clone());
    let changes = Rc::new(Cell::new(0));
    let count = changes.clone();
    live.on_change(Box::new(move || count.set(count.get() + 1)));
    let base = live.snapshot();
    let revision = base.revision();
    assert_eq!(deps.loaded.get(), 0);
    let mut worker = base.into_model(deps.clone());
    worker
        .note_tab_path(&mut Contents(7.0), "/output/book.docx", Some(true))
        .unwrap();
    assert_eq!(deps.loaded.get(), 1);
    assert_eq!(changes.get(), 0);
    let expected = worker.links();
    let commits = live
        .apply_snapshot(revision, worker.snapshot())
        .ok()
        .unwrap();
    assert_eq!(changes.get(), 0);
    assert_eq!(live.links(), expected);
    live.publish_snapshot_changes(commits);
    assert_eq!(changes.get(), 1);
    let mut original = DocumentLinks::new(Deps::new());
    original
        .note_tab_path(&mut Contents(7.0), "/output/book.docx", Some(true))
        .unwrap();
    assert_eq!(live.links(), original.links());
    assert_eq!(deps.saved.borrow().len(), 1);
}

#[test]
fn intervening_tab_lifetime_change_refuses_a_stale_prepared_join() {
    let mut live = DocumentLinks::new(Deps::new());
    let base = live.snapshot();
    let revision = base.revision();
    let mut worker = base.into_model(Deps::new());
    worker
        .note_tab_path(&mut Contents(7.0), "/output/a.docx", Some(true))
        .unwrap();
    live.tab_destroyed(7.0);
    assert!(live.apply_snapshot(revision, worker.snapshot()).is_err());
    assert!(live.links().is_empty());
}

#[test]
fn invalid_clock_keeps_original_partial_tracking_without_inventing_a_record() {
    let mut deps = Deps::new();
    deps.now = f64::NAN;
    let mut live = DocumentLinks::new(deps.clone());
    let base = live.snapshot();
    let revision = base.revision();
    let mut worker = base.into_model(deps.clone());
    assert!(
        worker
            .note_tab_path(&mut Contents(7.0), "/output/a.docx", Some(true))
            .is_err()
    );
    assert_eq!(
        live.apply_snapshot(revision, worker.snapshot())
            .ok()
            .unwrap(),
        0
    );
    assert!(live.links().is_empty());
    assert!(!live.sync().unwrap());
    assert!(deps.saved.borrow().is_empty());
}
