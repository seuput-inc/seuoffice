use super::*;
use std::sync::{Mutex, mpsc};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::var_os("SEUOFFICE_TEST_SCRATCH")
            .expect("hosted synthetic fixture scratch must be explicitly configured");
        let path =
            PathBuf::from(root).join(format!("owned-project-rename-{}", jscompat::random_uuid()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn store(&self, broken_source: bool) -> NativeProjectStore {
        let store = NativeProjectStore::new(&self.0);
        let source = if broken_source {
            json!({"id":"a","files":"broken"})
        } else {
            json!({"id":"a","files":["/docs/old.docx"]})
        };
        write_json(&store.base.join("a/project.json"), &source).unwrap();
        write_json(
            &store.base.join("b/project.json"),
            &json!({"id":"b","files":["/docs/new.docx"]}),
        )
        .unwrap();
        let old = canonical_path_key("/docs/old.docx");
        let new = canonical_path_key("/docs/new.docx");
        let mut index = json!({"projects":[],"fileMap":{},"chatIdByPath":{}});
        index["fileMap"][&old] = json!("a");
        index["fileMap"][&new] = json!("b");
        index["chatIdByPath"][&old] = json!("source");
        index["chatIdByPath"][&new] = json!("displaced");
        write_json(&store.base.join("index.json"), &index).unwrap();
        let chat = store.base.join("b/chats/displaced.jsonl");
        fs::create_dir_all(chat.parent().unwrap()).unwrap();
        fs::write(chat, b"synthetic displaced chat\n").unwrap();
        store
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn allow(effect: &mut dyn FnMut()) -> Result<(), String> {
    effect();
    Ok(())
}

#[test]
fn prepared_rename_does_not_publish_until_admitted_and_preserves_cache_scope() {
    let fixture = Fixture::new();
    let mut store = fixture.store(false);
    store
        .sequences
        .insert(("b".into(), "displaced".into()), 7.0);
    store
        .sequences
        .insert(("a".into(), "unrelated".into()), 9.0);
    let before = fs::read(store.base.join("index.json")).unwrap();
    let mut plan =
        store
            .prepare_rename_context()
            .prepare("/docs/old.docx", "/docs/new.docx", &mut allow);
    assert_eq!(fs::read(store.base.join("index.json")).unwrap(), before);
    assert!(matches!(
        plan.commit(&mut store, &mut allow),
        RenameCommit::Applied(Ok(()))
    ));
    let index = store.index();
    assert_eq!(index["fileMap"][canonical_path_key("/docs/new.docx")], "a");
    assert!(
        index["fileMap"]
            .get(canonical_path_key("/docs/old.docx"))
            .is_none()
    );
    assert_eq!(
        store.project("a").unwrap().unwrap()["files"],
        json!(["/docs/new.docx"])
    );
    assert_eq!(store.project("b").unwrap().unwrap()["files"], json!([]));
    assert!(!store.base.join("b/chats/displaced.jsonl").exists());
    assert!(
        !store
            .sequences
            .contains_key(&("b".into(), "displaced".into()))
    );
    assert_eq!(
        store.sequences.get(&("a".into(), "unrelated".into())),
        Some(&9.0)
    );
    assert!(matches!(
        plan.commit(&mut store, &mut allow),
        RenameCommit::Applied(Err(_))
    ));
}

#[test]
fn stale_preparation_retries_only_before_any_namespace_prefix_was_published() {
    let fixture = Fixture::new();
    let mut store = fixture.store(false);
    let before = fs::read(store.base.join("index.json")).unwrap();
    let mut plan =
        store
            .prepare_rename_context()
            .prepare("/docs/old.docx", "/docs/new.docx", &mut allow);
    store.revision.fetch_add(1, Ordering::AcqRel);
    assert!(matches!(
        plan.commit(&mut store, &mut allow),
        RenameCommit::Retry
    ));
    assert_eq!(fs::read(store.base.join("index.json")).unwrap(), before);
    drop(plan);
}

#[test]
fn a_later_algorithm_error_keeps_the_same_already_written_prefix_as_legacy() {
    let legacy_fixture = Fixture::new();
    let staged_fixture = Fixture::new();
    let mut legacy = legacy_fixture.store(true);
    let mut staged = staged_fixture.store(true);
    assert!(
        legacy
            .file_renamed("/docs/old.docx", "/docs/new.docx")
            .is_err()
    );
    let mut plan =
        staged
            .prepare_rename_context()
            .prepare("/docs/old.docx", "/docs/new.docx", &mut allow);
    assert!(matches!(
        plan.commit(&mut staged, &mut allow),
        RenameCommit::Applied(Err(_))
    ));
    assert_eq!(legacy.project("b").unwrap().unwrap()["files"], json!([]));
    assert_eq!(staged.project("b").unwrap().unwrap()["files"], json!([]));
    assert_eq!(legacy.index(), staged.index());
    assert!(staged.base.join("b/chats/displaced.jsonl").exists());
}

#[test]
fn authority_refusal_is_not_io_fallback_or_a_repeatable_plan() {
    let fixture = Fixture::new();
    let mut store = fixture.store(false);
    let before = fs::read(store.base.join("index.json")).unwrap();
    let mut plan =
        store
            .prepare_rename_context()
            .prepare("/docs/old.docx", "/docs/new.docx", &mut allow);
    let mut denied = |_: &mut dyn FnMut()| Err("request ended".into());
    assert!(
        matches!(plan.commit(&mut store,&mut denied),RenameCommit::Applied(Err(message)) if message == "request ended")
    );
    assert_eq!(fs::read(store.base.join("index.json")).unwrap(), before);
    assert!(matches!(
        plan.commit(&mut store, &mut allow),
        RenameCommit::Applied(Err(_))
    ));
}

#[test]
fn contention_releases_owner_admission_before_waiting_for_the_store() {
    let store = Arc::new(Mutex::new(NativeProjectStore::new("synthetic-only-no-io")));
    let owner = Arc::new(Mutex::new(()));
    let held_store = store.lock().unwrap();
    let (released, wait_released) = mpsc::sync_channel(1);
    let worker_store = store.clone();
    let worker_owner = owner.clone();
    let worker = std::thread::spawn(move || {
        let admitted = worker_owner.lock().unwrap();
        assert!(matches!(
            worker_store.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        drop(admitted);
        released.send(()).unwrap();
        drop(worker_store.lock().unwrap());
    });
    wait_released.recv().unwrap();
    let can_revoke = owner
        .try_lock()
        .expect("revocation cannot be trapped behind a store wait");
    drop(can_revoke);
    drop(held_store);
    worker.join().unwrap();
}
