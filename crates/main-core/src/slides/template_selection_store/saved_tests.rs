use super::*;
use serde_json::json;
use std::{cell::Cell, path::PathBuf, rc::Rc};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::var_os("SEUOFFICE_TEST_SCRATCH")
            .expect("hosted synthetic scratch must be explicitly configured");
        let root = PathBuf::from(root).join(format!("template-move-{}", jscompat::random_uuid()));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_owned()
    }
    fn store(&self, clock: Option<Box<dyn Fn() -> f64>>) -> TemplateSelectionStore {
        TemplateSelectionStore::new(
            self.0.to_str().unwrap(),
            TemplateSelectionStoreOptions {
                max_documents: Some(2.0),
                now: clock,
            },
        )
    }
    fn seed(&self, old: &str, new: &str) {
        let raw = json!({"version":1,"documents":{
            (old):{"kind":"builtin","key":"aurora","adjustments":["blue"],"updatedAt":10,"usedAt":20},
            (new):{"kind":"builtin","key":"displaced","adjustments":[],"updatedAt":11,"usedAt":21}
        }});
        std::fs::write(
            self.path(TEMPLATE_SELECTION_FILE),
            serde_json::to_vec(&raw).unwrap(),
        )
        .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn immutable_move_uses_original_displacement_order_and_calls_clock_only_when_needed() {
    let fixture = Fixture::new();
    let old = fixture.path("old.pptx");
    let new = fixture.path("new.pptx");
    fixture.seed(&old, &new);
    let store = fixture.store(None);
    let clock_calls = Rc::new(Cell::new(0));
    let called = clock_calls.clone();
    let mut payload = None;
    store
        .move_plan()
        .apply(
            &old,
            &new,
            Some(Box::new(move || {
                called.set(called.get() + 1);
                42.0
            })),
            &mut |_, value| {
                payload = Some(jscompat::json_value_to_serde(value));
                Ok(())
            },
        )
        .unwrap();
    let payload = payload.unwrap();
    let documents = payload["documents"].as_object().unwrap();
    assert!(!documents.contains_key(&canonical_path_key(&old)));
    assert_eq!(documents[&canonical_path_key(&new)]["key"], "aurora");
    assert_eq!(documents[&canonical_path_key(&new)]["updatedAt"], 10.0);
    assert_eq!(documents[&canonical_path_key(&new)]["usedAt"], 42.0);
    assert_eq!(clock_calls.get(), 1);
    let bytes = std::fs::read(fixture.path(TEMPLATE_SELECTION_FILE)).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(raw["documents"].as_object().unwrap().contains_key(&old));
}

#[test]
fn empty_move_keeps_original_no_write_and_no_clock_policy() {
    let fixture = Fixture::new();
    let store = fixture.store(None);
    store
        .move_plan()
        .apply(
            &fixture.path("absent.pptx"),
            &fixture.path("new.pptx"),
            Some(Box::new(|| panic!("no record must not read the clock"))),
            &mut |_, _| panic!("no record must not create a file"),
        )
        .unwrap();
    assert!(!fixture.0.join(TEMPLATE_SELECTION_FILE).exists());
}

#[test]
fn original_borrowed_clock_runs_on_its_caller_inside_the_same_file_fifo() {
    let fixture = Fixture::new();
    let old = fixture.path("old.pptx");
    let new = fixture.path("new.pptx");
    fixture.seed(&old, &new);
    let thread = std::thread::current().id();
    let calls = Rc::new(Cell::new(0));
    let invoked = calls.clone();
    let store = fixture.store(Some(Box::new(move || {
        assert_eq!(std::thread::current().id(), thread);
        invoked.set(invoked.get() + 1);
        50.0
    })));
    store.move_record(&old, &new).unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(store.get(&new).unwrap().unwrap().key, "aurora");
    assert_eq!(calls.get(), 2);
}

#[test]
fn denied_staged_write_is_an_error_without_replaying_the_original_transfer() {
    let fixture = Fixture::new();
    let old = fixture.path("old.pptx");
    let new = fixture.path("new.pptx");
    fixture.seed(&old, &new);
    let before = std::fs::read(fixture.path(TEMPLATE_SELECTION_FILE)).unwrap();
    let calls = Cell::new(0);
    let result = fixture
        .store(None)
        .move_plan()
        .apply(&old, &new, None, &mut |_, _| {
            calls.set(calls.get() + 1);
            Err(NodeError {
                code: None,
                message: "synthetic lost authority".into(),
            })
        });
    assert!(result.is_err());
    assert_eq!(calls.get(), 1);
    assert_eq!(
        std::fs::read(fixture.path(TEMPLATE_SELECTION_FILE)).unwrap(),
        before
    );
}
