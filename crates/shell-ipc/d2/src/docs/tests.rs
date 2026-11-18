//! What a docs renderer sees: answers, events and files, across channels.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision;
use main_core::packages::electron_utils::recovery_store::SharedRecoveryState;
use serde_json::json;
use shell_contract::editors::{DocumentEditors, SaveMode, editor_for};
use shell_contract::ipc::{Sender, Target};
use shell_contract::{BytesKind, IpcValue, Surface, ViewId};

use super::editors::DocsEditors;
use super::hosts::{DocsEngineHost, DocsShellHost, Fake};
use crate::kit::{self, IntoIpc};
use crate::testing::{HostCall, Rt};

fn s(text: &str) -> IpcValue {
    IpcValue::String(text.into())
}

fn ab(bytes: &[u8]) -> IpcValue {
    IpcValue::Bytes(BytesKind::ArrayBuffer, bytes.to_vec())
}

fn get<'a>(v: &'a IpcValue, key: &str) -> &'a IpcValue {
    kit::field(v, key)
}

fn obj(entries: &[(&str, IpcValue)]) -> IpcValue {
    IpcValue::Object(
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

struct BackgroundWorkers;

impl shell_contract::runtime::Workers for BackgroundWorkers {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
        let _ = std::thread::spawn(job);
    }
}

struct DiscardWorkers;

impl shell_contract::runtime::Workers for DiscardWorkers {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
        drop(job);
    }
}

#[derive(Default)]
struct HeldWorkers {
    hold: std::sync::atomic::AtomicBool,
    jobs: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

impl shell_contract::runtime::Workers for HeldWorkers {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
        if self.hold.load(std::sync::atomic::Ordering::SeqCst) {
            self.jobs.lock().unwrap().push(job);
        } else {
            let _ = std::thread::spawn(job);
        }
    }
}

struct T {
    rt: Rt,
    fake: Fake,
    a: Sender,
}

impl T {
    fn new(case: serde_json::Value) -> Self {
        Self::with_workers(case, None)
    }

    fn with_workers(
        case: serde_json::Value,
        workers: Option<Arc<dyn shell_contract::runtime::Workers>>,
    ) -> Self {
        let fake = Fake::new(&case);
        let f = fake.clone();
        let mut rt = Rt::new(move |h| {
            h.insert::<DocsEngineHost>(Arc::new(f.clone()));
            h.insert::<DocsShellHost>(Arc::new(f));
            h.insert::<Arc<Mutex<SharedRecoveryState>>>(Arc::new(Mutex::new(
                SharedRecoveryState::new("run"),
            )));
        });
        if let Some(workers) = workers {
            rt.services.workers = workers;
        }
        fake.bind_query_runtime(&rt.services);
        for sub in ["userData", "documents", "temp", "home"] {
            std::fs::create_dir_all(rt.dir.join(sub)).unwrap();
        }
        rt.register(super::entries).unwrap();
        let a = rt.view(1, Surface::Docs);
        rt.provided().all::<DocsEditors>()[0]
            .register_view(a.view)
            .unwrap();
        T { rt, fake, a }
    }

    fn editor(&self) -> Rc<DocsEditors> {
        self.rt.provided().all::<DocsEditors>().remove(0)
    }

    fn call(&mut self, channel: &str, args: Vec<IpcValue>) -> IpcValue {
        let a = self.a;
        self.rt.invoke_now(channel, a, args).unwrap()
    }

    fn file(&self, name: &str, bytes: &[u8]) -> String {
        let p = self.rt.dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn open(&mut self, path: &str) -> IpcValue {
        self.call("docs:open-path", vec![s(path)])
    }

    /// the hosts' log lines, as compact JSON
    fn calls(&self) -> Vec<String> {
        self.fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|l| l.to_string())
            .collect()
    }

    fn channels(&self) -> Vec<&'static str> {
        self.rt.events().into_iter().map(|e| e.channel).collect()
    }
}

#[test]
fn prepared_view_registration_is_admitted_before_replacing_the_lease() {
    let t = T::new(json!({}));
    let editor = t.editor();
    let view = t.a.view;
    let generation = t.rt.services.views.renderer_generation(view).unwrap();
    let (_issuer, reservation, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, generation.clone());
    editor.0.revoke_media_view(view);
    let mut prepared = editor
        .prepare_view_registration(view, generation, None, &reservation, &browser)
        .unwrap();
    reservation
        .with_initial_owner(&browser, |transition| {
            prepared.commit_admitted(transition, None)
        })
        .unwrap();
    let receipt = prepared.finish().unwrap();
    let permit = receipt.lease().current().unwrap();
    assert!(editor.media_permit(view).unwrap().same_session(&permit));
    assert!(receipt.discard().unwrap());
    assert!(!permit.is_active());
    assert!(editor.media_permit(view).is_none());
}

#[test]
fn prepared_view_discard_does_not_revoke_a_successor() {
    let t = T::new(json!({}));
    let editor = t.editor();
    let view = t.a.view;
    let generation = t.rt.services.views.renderer_generation(view).unwrap();
    let (_issuer, reservation, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, generation.clone());
    editor.0.revoke_media_view(view);
    let mut prepared = editor
        .prepare_view_registration(view, generation, None, &reservation, &browser)
        .unwrap();
    reservation
        .with_initial_owner(&browser, |transition| {
            prepared.commit_admitted(transition, None)
        })
        .unwrap();
    let receipt = prepared.finish().unwrap();
    let successor = receipt.lease().stage_document().unwrap();
    assert!(!receipt.discard().unwrap());
    assert!(successor.is_active());
}

#[test]
fn prepared_open_rolls_back_only_its_exact_queue_entry() {
    let t = T::new(json!({}));
    let editor = t.editor();
    let view = t.a.view;
    let generation = t.rt.services.views.renderer_generation(view).unwrap();
    let (_issuer, reservation, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, generation.clone());
    editor.queue_open(view, "old.docx".into());
    let mut open = editor
        .prepare_queue_open(view, "same.docx".into(), generation.clone(), &reservation)
        .unwrap();
    reservation
        .with_initial_owner(&browser, |transition| {
            open.commit_admitted(transition, &browser)
        })
        .unwrap();
    let receipt = open.finish().unwrap();
    receipt.discard().unwrap();
    assert_eq!(editor.0.window_opens.get(view).as_deref(), Some("old.docx"));

    let mut open = editor
        .prepare_queue_open(view, "same.docx".into(), generation, &reservation)
        .unwrap();
    reservation
        .with_owner(&browser, |transition| {
            open.commit_admitted(transition, &browser)
        })
        .unwrap();
    let receipt = open.finish().unwrap();
    editor.queue_open(view, "same.docx".into());
    assert!(receipt.discard().is_err());
    assert_eq!(
        editor.0.window_opens.get(view).as_deref(),
        Some("same.docx")
    );
}

#[test]
fn destroyed_view_without_exact_close_proof_does_not_acknowledge_cleanup() {
    let mut t = T::new(json!({}));
    let editor = t.editor();
    let view = t.a.view;
    let generation = t.rt.services.views.renderer_generation(view).unwrap();
    let (issuer, reservation, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, generation.clone());
    let mut open = editor
        .prepare_queue_open(
            view,
            "pending.docx".into(),
            generation.clone(),
            &reservation,
        )
        .unwrap();
    reservation
        .with_initial_owner(&browser, |transition| {
            open.commit_admitted(transition, &browser)
        })
        .unwrap();
    let receipt = open.finish().unwrap();
    t.rt.destroy(view);
    assert!(receipt.discard().is_err());
    let (forged, _) = shell_contract::ipc::NativeClosureIssuer::new(view, generation);
    assert!(receipt.retire_closed(&forged.closed()).is_err());
    receipt.retire_closed(&issuer.closed()).unwrap();
    assert!(receipt.discard().is_ok());
}

#[test]
fn registration_receipt_rejects_unproven_removal_and_replacement() {
    let t = T::new(json!({}));
    let editor = t.editor();
    let view = t.a.view;
    let generation = t.rt.services.views.renderer_generation(view).unwrap();
    let (_issuer, reservation, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, generation.clone());
    editor.0.revoke_media_view(view);
    let mut registration = editor
        .prepare_view_registration(view, generation.clone(), None, &reservation, &browser)
        .unwrap();
    reservation
        .with_initial_owner(&browser, |transition| {
            registration.commit_admitted(transition, None)
        })
        .unwrap();
    let receipt = registration.finish().unwrap();
    editor.0.revoke_media_view(view);
    assert!(!receipt.discard().unwrap());

    let mut registration = editor
        .prepare_view_registration(view, generation, None, &reservation, &browser)
        .unwrap();
    reservation
        .with_owner(&browser, |transition| {
            registration.commit_admitted(transition, None)
        })
        .unwrap();
    let receipt = registration.finish().unwrap();
    let successor = editor.register_view(view).unwrap();
    assert!(!receipt.discard().unwrap());
    assert!(successor.current().is_some());
}

#[test]
fn blank_seed_receipt_preserves_same_generation_successor() {
    let t = T::new(json!({}));
    let editor = t.editor();
    let view = t.a.view;
    let generation = t.rt.services.views.renderer_generation(view).unwrap();
    let (_issuer, reservation, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, generation.clone());
    let mut first = editor
        .prepare_blank_seed(view, generation.clone(), &reservation)
        .unwrap();
    reservation
        .with_initial_owner(&browser, |transition| {
            first.commit_admitted(transition, &browser)
        })
        .unwrap();
    let receipt = first.finish().unwrap();
    let mut successor = editor
        .prepare_blank_seed(view, generation, &reservation)
        .unwrap();
    reservation
        .with_owner(&browser, |transition| {
            successor.commit_admitted(transition, &browser)
        })
        .unwrap();
    let _successor = successor.finish().unwrap();
    assert!(receipt.rollback().is_err());
    assert!(editor.0.owned_new_blank.get(view).is_some());
}

#[test]
fn adopted_blank_requires_the_original_closed_reservation() {
    let t = T::new(json!({}));
    let editor = t.editor();
    let view = t.a.view;
    let generation = t.rt.services.views.renderer_generation(view).unwrap();
    let (issuer, reservation, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, generation.clone());
    let mut blank = editor
        .prepare_blank_seed(view, generation.clone(), &reservation)
        .unwrap();
    reservation
        .with_initial_owner(&browser, |transition| {
            blank.commit_admitted(transition, &browser)
        })
        .unwrap();
    let receipt = blank.finish().unwrap();
    let (forged, _) = shell_contract::ipc::NativeClosureIssuer::new(view, generation);
    assert!(receipt.retire_adopted(&forged.closed()).is_err());
    assert!(editor.0.owned_new_blank.get(view).is_some());
    receipt.retire_adopted(&issuer.closed()).unwrap();
    assert!(editor.0.owned_new_blank.get(view).is_none());
}

#[test]
fn opening_a_file_grants_its_save_and_lists_it_as_recent() {
    let mut t = T::new(json!({}));
    let path = t.file("a.docx", b"plain docx");
    let opened = t.open(&path);
    assert_eq!(get(&opened, "path"), &s(&path));
    assert_eq!(get(&opened, "name"), &s("a.docx"));
    assert_eq!(get(&opened, "dataUrl"), &s("handoff:706c61696e20646f6378"));
    assert_eq!(get(&opened, "hash"), &s(&super::sha256_hex(b"plain docx")));
    assert_eq!(get(&opened, "encrypted"), &IpcValue::Bool(false));
    assert!(
        matches!(&opened, IpcValue::Object(e) if e.iter().any(|(k, v)| k == "recovered" && *v == IpcValue::Undefined))
    );
    assert_eq!(
        t.call("docs:recent", vec![]),
        IpcValue::Array(vec![s(&path)])
    );
    assert!(
        t.calls()
            .contains(&serde_json::json!(["fileOpenedHook", 1, path]).to_string())
    );
    // the original is archived under its hash
    assert!(
        t.rt.dir
            .join("userData/originals")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );

    let saved = t.call("docs:save", vec![s(&path), ab(b"new body")]);
    assert_eq!(get(&saved, "ok"), &IpcValue::Bool(true));
    assert_eq!(get(&saved, "passwordIntentPending"), &IpcValue::Bool(false));
    assert_eq!(std::fs::read(&path).unwrap(), b"new body");
}

#[test]
fn failed_byte_handoff_rejects_open_without_granting_the_file() {
    let mut t = T::new(json!({ "hand_off_bytes": { "error": "media worker stopped" } }));
    let path = t.file("a.docx", b"plain docx");
    let error =
        t.rt.invoke_now("docs:open-path", t.a, vec![s(&path)])
            .unwrap_err();
    assert_eq!(error.to_string(), "Error: media worker stopped");
    assert_eq!(t.call("docs:recent", vec![]), IpcValue::Array(vec![]));
    assert!(
        !t.calls()
            .contains(&json!(["fileOpenedHook", 1, path]).to_string())
    );
}

#[test]
fn failed_open_keeps_the_current_media_generation() {
    let mut t = T::new(json!({}));
    let path = t.file("open.docx", b"original document");
    t.open(&path);
    let original = t.editor().media_permit(t.a.view).unwrap();
    let missing = t.rt.dir.join("missing.docx").to_string_lossy().into_owned();
    assert_eq!(t.open(&missing), IpcValue::Null);
    let current = t.editor().media_permit(t.a.view).unwrap();
    assert!(original.is_active());
    assert!(current.same_session(&original));
}

#[test]
fn lazy_worker_failure_rejects_open_instead_of_falling_back_to_disk() {
    let mut t = T::new(json!({ "open_lazy_docx": { "error": "media worker stopped" } }));
    let path = t.file("a.docx", b"plain docx");
    let error =
        t.rt.invoke_now("docs:open-path", t.a, vec![s(&path)])
            .unwrap_err();
    assert_eq!(error.to_string(), "Error: media worker stopped");
    assert_eq!(t.call("docs:recent", vec![]), IpcValue::Array(vec![]));
}

#[test]
fn lazy_worker_failure_after_save_is_reported_without_undoing_the_write() {
    let mut t = T::new(json!({ "point_lazy_media_at": { "error": "media worker stopped" } }));
    let path = t.file("a.docx", b"plain docx");
    t.open(&path);
    let error =
        t.rt.invoke_now("docs:save", t.a, vec![s(&path), ab(b"new body")])
            .unwrap_err();
    assert_eq!(error.to_string(), "Error: media worker stopped");
    assert_eq!(std::fs::read(path).unwrap(), b"new body");
}

#[test]
fn a_path_that_is_no_word_document_opens_nothing() {
    let mut t = T::new(json!({}));
    let path = t.file("a.txt", b"x");
    assert_eq!(t.open(&path), IpcValue::Null);
    assert_eq!(
        t.open(&t.rt.dir.join("gone.docx").to_string_lossy()),
        IpcValue::Null
    );
    assert_eq!(
        t.call("docs:open-path", vec![1.0.into_ipc()]),
        IpcValue::Null
    );
}

#[test]
fn the_open_dialog_cancelled_answers_null_and_picked_opens() {
    let mut t = T::new(json!({}));
    let path = t.file("b.odt", b"odt bytes");
    t.rt.script().open.push_back(None);
    assert_eq!(t.call("docs:open", vec![]), IpcValue::Null);
    t.rt.script()
        .open
        .push_back(Some(vec![PathBuf::from(&path)]));
    let opened = t.call("docs:open", vec![]);
    assert_eq!(get(&opened, "name"), &s("b.odt"));
}

#[test]
fn a_file_too_large_to_open_shows_an_error_and_answers_null() {
    let mut t = T::new(json!({ "open_lazy_docx": { "bytes": "x", "hash": "h" } }));
    let path = t.file("big.docx", b"x");
    // a lazily opened document carries its own hash
    let opened = t.open(&path);
    assert_eq!(get(&opened, "hash"), &s("h"));
}

#[test]
fn a_legacy_doc_is_converted_into_the_picked_docx_copy() {
    let mut t = T::new(json!({ "convert_legacy_doc": "converted" }));
    let doc = t.file("old.doc", b"word97");
    let target = t.rt.dir.join("old.docx");
    t.rt.script().message.push_back(0);
    t.rt.script().save.push_back(Some(target.clone()));
    let opened = t.open(&doc);
    assert_eq!(get(&opened, "path"), &s(&target.to_string_lossy()));
    assert_eq!(std::fs::read(&target).unwrap(), b"converted");
    assert_eq!(std::fs::read(&doc).unwrap(), b"word97");

    // cancelling the conversion box opens nothing
    t.rt.script().message.push_back(1);
    assert_eq!(t.open(&doc), IpcValue::Null);
}

#[test]
fn save_refuses_a_path_this_view_did_not_open() {
    let mut t = T::new(json!({}));
    let path = t.file("c.docx", b"x");
    let r = t.call("docs:save", vec![s(&path), ab(b"y")]);
    assert_eq!(
        r,
        obj(&[
            ("ok", false.into_ipc()),
            ("error", s("save target is not an opened document"))
        ])
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"x");
}

#[test]
fn a_file_changed_on_disk_is_not_overwritten_without_asking() {
    let mut t = T::new(json!({}));
    let path = t.file("d.docx", b"original");
    t.open(&path);
    std::fs::write(&path, b"edited elsewhere!").unwrap();
    let external = obj(&[("ok", false.into_ipc()), ("reason", s("external-modified"))]);
    assert_eq!(
        t.call("docs:save", vec![s(&path), ab(b"mine"), true.into_ipc()]),
        external
    );
    t.rt.script().message.push_back(1);
    assert_eq!(t.call("docs:save", vec![s(&path), ab(b"mine")]), external);
    assert_eq!(std::fs::read(&path).unwrap(), b"edited elsewhere!");
    t.rt.script().message.push_back(0);
    assert_eq!(
        get(&t.call("docs:save", vec![s(&path), ab(b"mine")]), "ok"),
        &IpcValue::Bool(true)
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"mine");
}

#[test]
fn save_as_writes_only_the_picked_path_once() {
    let mut t = T::new(json!({ "file_saved": "/moved/e.docx" }));
    let target = t.rt.dir.join("e");
    t.rt.script().save.push_back(Some(target.clone()));
    let picked = t.call("docs:save-as-pick", vec![s("e.docx")]);
    let path = format!("{}.docx", target.to_string_lossy());
    assert_eq!(picked, obj(&[("path", s(&path)), ("kind", s("docx"))]));
    let other = t.call("docs:save-as", vec![s("/elsewhere.docx"), ab(b"z")]);
    assert_eq!(get(&other, "error"), &s("save target was not picked"));
    let saved = t.call("docs:save-as", vec![s(&path), ab(b"body")]);
    assert_eq!(get(&saved, "path"), &s("/moved/e.docx"));
    assert_eq!(std::fs::read(&path).unwrap(), b"body");
    // the pick is used up
    let again = t.call("docs:save-as", vec![s(&path), ab(b"body")]);
    assert_eq!(get(&again, "error"), &s("save target was not picked"));
    // and the saved file is this view's document now
    assert_eq!(
        t.editor().document_path(ViewId(1)),
        Some(PathBuf::from(&path))
    );
}

#[test]
fn save_as_pick_of_page_images_authorizes_a_temp_pdf_and_the_folder() {
    let mut t = T::new(json!({}));
    let out = t.rt.dir.join("pages.png");
    t.rt.script().save.push_back(Some(out));
    let picked = t.call(
        "docs:save-as-pick",
        vec![s("doc.docx"), IpcValue::Null, s("docx"), true.into_ipc()],
    );
    assert_eq!(get(&picked, "kind"), &s("png"));
    let dir = kit::as_str(get(&picked, "dir")).unwrap().to_owned();
    let pdf = kit::as_str(get(&picked, "pdfPath")).unwrap().to_owned();

    assert_eq!(
        get(
            &t.call(
                "docs:write-export-image",
                vec![s(&dir), s("../x.png"), s("AA==")]
            ),
            "error"
        ),
        &s("invalid image file name")
    );
    let wrote = t.call(
        "docs:write-export-image",
        vec![s(&dir), s("p1.png"), s("aGk=")],
    );
    assert_eq!(
        std::fs::read(Path::new(&dir).join("p1.png")).unwrap(),
        b"hi"
    );
    assert_eq!(get(&wrote, "ok"), &IpcValue::Bool(true));

    std::fs::write(&pdf, b"%PDF").unwrap();
    let taken = t.call("docs:take-export-pdf", vec![s(&pdf)]);
    assert_eq!(
        taken,
        obj(&[("ok", true.into_ipc()), ("base64", s("JVBERg=="))])
    );
    assert!(!Path::new(&pdf).exists());
    assert_eq!(
        get(&t.call("docs:take-export-pdf", vec![s(&pdf)]), "error"),
        &s("not an image-export temp file")
    );
}

#[test]
fn save_new_lands_in_the_default_folder_under_a_free_name() {
    let mut t = T::new(json!({}));
    let first = t.call("docs:save-new", vec![s("Doc.docx"), ab(b"1")]);
    let second = t.call("docs:save-new", vec![s("Doc.docx"), ab(b"2")]);
    let (p1, p2) = (
        kit::as_str(get(&first, "path")).unwrap(),
        kit::as_str(get(&second, "path")).unwrap(),
    );
    assert!(p1.ends_with("Doc.docx"), "{p1}");
    assert!(p2.ends_with("Doc-2.docx"), "{p2}");
    assert_eq!(std::fs::read(p2).unwrap(), b"2");
    // a saved document may be saved again
    assert_eq!(
        get(&t.call("docs:save", vec![s(p1), ab(b"3")]), "ok"),
        &IpcValue::Bool(true)
    );
}

#[test]
fn save_to_needs_the_mcp_grant_and_respects_existing_files() {
    let mut t = T::new(json!({}));
    assert_eq!(
        get(
            &t.call(
                "docs:save-to",
                vec![s("rel.docx"), ab(b"x"), false.into_ipc()]
            ),
            "error"
        ),
        &s("path must be absolute")
    );
    let path = t.rt.dir.join("sub/f.docx").to_string_lossy().into_owned();
    assert_eq!(
        get(
            &t.call("docs:save-to", vec![s(&path), ab(b"x"), false.into_ipc()]),
            "error"
        ),
        &s("save target was not authorized")
    );
    t.editor().authorize_mcp_doc_write(ViewId(1), &path);
    let saved = t.call("docs:save-to", vec![s(&path), ab(b"x"), false.into_ipc()]);
    assert_eq!(get(&saved, "path"), &s(&path));
    assert_eq!(std::fs::read(&path).unwrap(), b"x");
    let exists = t.call("docs:save-to", vec![s(&path), ab(b"y"), false.into_ipc()]);
    assert_eq!(
        get(&exists, "error"),
        &s(&format!(
            "file already exists: {path} (pass overwrite:true to replace it)"
        ))
    );
    t.call("docs:save-to", vec![s(&path), ab(b"y"), true.into_ipc()]);
    assert_eq!(std::fs::read(&path).unwrap(), b"y");
}

#[test]
fn save_into_project_goes_where_the_shell_puts_it_or_is_skipped() {
    let skipped = obj(&[("ok", false.into_ipc()), ("skipped", true.into_ipc())]);
    let mut t = T::new(json!({}));
    assert_eq!(
        t.call("docs:save-into-project", vec![s("n"), s("docx"), ab(b"x")]),
        skipped
    );

    // the shell files the document into the project folder
    let target = std::env::temp_dir()
        .join(format!("docs-project-{}.docx", kit::unique()))
        .to_string_lossy()
        .into_owned();
    let mut p = T::new(json!({ "write_new_document": target.clone() }));
    assert_eq!(
        get(
            &p.call(
                "docs:save-into-project",
                vec![s("n"), s("docx"), s("not bytes")]
            ),
            "error"
        ),
        &s("bad save request")
    );
    let saved = p.call(
        "docs:save-into-project",
        vec![s("dir/My: Doc.docx"), s("docx"), ab(b"body")],
    );
    assert_eq!(get(&saved, "path"), &s(&target));
    assert!(
        p.calls()
            .iter()
            .any(|c| c == r#"["writeNewDocument",1,"My_ Doc.docx","626f6479"]"#)
    );
    // the document has its file now
    assert_eq!(
        p.call("docs:save-into-project", vec![s("n"), s("docx"), ab(b"x")]),
        skipped
    );
    let _ = std::fs::remove_file(&target);
}

#[test]
fn a_password_set_for_the_document_encrypts_its_next_save() {
    let mut t = T::new(json!({}));
    let path = t.file("g.docx", b"plain");
    assert_eq!(
        t.call("docs:set-password", vec![s(&path), s("pw")]),
        obj(&[("ok", false.into_ipc())])
    );
    t.open(&path);
    assert_eq!(
        t.call("docs:set-password", vec![s(&path), s("")]),
        obj(&[("ok", false.into_ipc())])
    );
    assert_eq!(
        t.call("docs:set-password", vec![s(&path), s("pw")]),
        obj(&[("ok", true.into_ipc())])
    );
    assert_eq!(
        t.call("docs:password-intent-revision", vec![]),
        1.0.into_ipc()
    );
    t.call("docs:save", vec![s(&path), ab(b"text")]);
    assert_eq!(std::fs::read(&path).unwrap(), b"ENC:text");
    assert!(
        t.calls()
            .contains(&r#"["officeCrypto.encrypt","pw"]"#.to_owned())
    );
}

#[test]
fn password_intents_are_discarded_through_a_revision() {
    let mut t = T::new(json!({}));
    assert_eq!(
        t.call("docs:discard-password-intents", vec![(-1.0).into_ipc()]),
        obj(&[("ok", false.into_ipc())])
    );
    assert_eq!(
        t.call("docs:discard-password-intents", vec![1.5.into_ipc()]),
        obj(&[("ok", false.into_ipc())])
    );
    t.call("docs:set-password", vec![IpcValue::Null, s("pw")]);
    assert_eq!(
        t.call("docs:discard-password-intents", vec![1.0.into_ipc()]),
        obj(&[("ok", true.into_ipc())])
    );
    // the pending password is gone: a first save is plain
    let saved = t.call("docs:save-new", vec![s("h.docx"), ab(b"x")]);
    let path = kit::as_str(get(&saved, "path")).unwrap().to_owned();
    assert_eq!(std::fs::read(path).unwrap(), b"x");
}

#[test]
fn open_decrypt_reports_why_it_did_not_open() {
    let mut t = T::new(json!({}));
    let bad = obj(&[
        ("ok", false.into_ipc()),
        ("reason", s("error")),
        ("error", s("invalid arguments")),
    ]);
    assert_eq!(t.call("docs:open-decrypt", vec![s("/x.docx"), s("")]), bad);
    let path = t.rt.dir.join("none.docx").to_string_lossy().into_owned();
    assert_eq!(
        get(
            &t.call("docs:open-decrypt", vec![s(&path), s("pw")]),
            "error"
        ),
        &s("file not found")
    );
    let plain = t.file("plain.docx", b"zip");
    assert_eq!(
        get(&t.call("docs:open-decrypt", vec![s(&plain), s("pw")]), "ok"),
        &IpcValue::Bool(true)
    );
}

#[test]
fn the_close_guard_saves_discards_or_cancels_by_the_answer() {
    let mut t = T::new(json!({}));
    let a = t.a;
    let dirty = obj(&[
        ("dirty", true.into_ipc()),
        ("autoSave", false.into_ipc()),
        ("filePath", IpcValue::Null),
    ]);

    // clean: closes without asking
    let p = t.rt.invoke("docs:confirm-document-replace", a, vec![]);
    assert_eq!(t.channels(), vec!["docs:close-check"]);
    t.rt.send("docs:close-check-result", a, vec![false.into_ipc()]);
    assert_eq!(p.take(), Some(Ok(true.into_ipc())));

    // dirty, Cancel
    t.rt.script().message.push_back(2);
    let p = t.rt.invoke("docs:confirm-document-replace", a, vec![]);
    t.rt.send("docs:close-check-result", a, vec![dirty.clone()]);
    assert_eq!(p.take(), Some(Ok(false.into_ipc())));

    // dirty, Don't Save: the recovery copies go and the renderer hears it
    t.rt.events();
    t.rt.script().message.push_back(1);
    let p = t.rt.invoke("docs:confirm-document-replace", a, vec![]);
    t.rt.send("docs:close-check-result", a, vec![dirty.clone()]);
    assert_eq!(p.take(), Some(Ok(true.into_ipc())));
    assert_eq!(
        t.channels(),
        vec!["docs:close-check", "docs:recovery-copy-dropped"]
    );

    // dirty, Save: the renderer saves and reports
    t.rt.script().message.push_back(0);
    let p = t.rt.invoke("docs:confirm-document-replace", a, vec![]);
    t.rt.send("docs:close-check-result", a, vec![dirty]);
    assert_eq!(p.take(), None);
    assert_eq!(
        t.channels(),
        vec!["docs:close-check", "docs:close-save-request"]
    );
    t.rt.send("docs:close-save-result", a, vec![true.into_ipc()]);
    assert_eq!(p.take(), Some(Ok(true.into_ipc())));
}

#[test]
fn dirty_query_returns_renderer_state_and_rejects_timeout_or_missing_view() {
    let mut t = T::new(json!({}));
    let editor = t.editor();
    let answers = Rc::new(std::cell::RefCell::new(Vec::new()));
    let output = answers.clone();
    editor.query_dirty(
        t.a.view,
        Box::new(move |result| output.borrow_mut().push(result)),
    );
    assert!(answers.borrow().is_empty());
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![obj(&[("dirty", true.into_ipc())])],
    );
    assert_eq!(answers.borrow_mut().pop().unwrap(), Ok(true));

    let output = answers.clone();
    editor.query_dirty(
        t.a.view,
        Box::new(move |result| output.borrow_mut().push(result)),
    );
    t.rt.advance(Duration::from_millis(2_000));
    assert!(answers.borrow_mut().pop().unwrap().is_err());

    t.rt.destroy(t.a.view);
    let output = answers.clone();
    editor.query_dirty(
        t.a.view,
        Box::new(move |result| output.borrow_mut().push(result)),
    );
    assert!(answers.borrow_mut().pop().unwrap().is_err());
}

fn owned_dirty_query(
    t: &T,
    source: Sender,
    cancel: shell_contract::runtime::CancelToken,
    answers: Rc<RefCell<Vec<Result<bool, shell_contract::IpcError>>>>,
) -> String {
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let owner = shell_contract::ipc::NativeRequestOwner {
        view: source.view,
        context: shell_contract::ipc::NativeContext::with_owner_loss(
            (),
            generation.clone(),
            generation.loss_token(),
        ),
    };
    t.editor().query_dirty_owned(
        t.a.view,
        owner,
        cancel,
        Box::new(move |answer| answers.borrow_mut().push(answer)),
    );
    t.rt.events()
        .iter()
        .rev()
        .find(|event| event.channel == "docs:close-check")
        .and_then(|event| event.args.first())
        .and_then(kit::as_str)
        .unwrap()
        .to_owned()
}

#[test]
fn owned_dirty_query_requires_exact_token_sender_and_renderer() {
    let mut t = T::new(json!({}));
    let source = t.rt.view(2, Surface::Shell);
    let other = t.rt.view(3, Surface::Docs);
    let answers = Rc::new(RefCell::new(Vec::new()));
    let cancel = shell_contract::runtime::CancelToken::default();
    let token = owned_dirty_query(&t, source, cancel, answers.clone());
    let state = obj(&[("dirty", true.into_ipc())]);
    t.rt.send(
        "docs:close-check-result",
        other,
        vec![state.clone(), s(&token)],
    );
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![state.clone(), s("wrong")],
    );
    assert!(answers.borrow().is_empty());
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![state.clone(), s(&token)],
    );
    assert_eq!(*answers.borrow(), vec![Ok(true)]);
    t.rt.send("docs:close-check-result", t.a, vec![state, s(&token)]);
    assert_eq!(answers.borrow().len(), 1);
}

#[test]
fn owned_dirty_queries_reject_source_loss_target_replacement_cancellation_and_timeout() {
    let mut t = T::new(json!({}));
    let source = t.rt.view(2, Surface::Shell);
    let answers = Rc::new(RefCell::new(Vec::new()));
    let token = owned_dirty_query(
        &t,
        source,
        shell_contract::runtime::CancelToken::default(),
        answers.clone(),
    );
    t.rt.replace_renderer(source.view);
    assert!(answers.borrow_mut().pop().unwrap().is_err());
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![obj(&[("dirty", false.into_ipc())]), s(&token)],
    );
    assert!(answers.borrow().is_empty());

    let old = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let token = owned_dirty_query(
        &t,
        source,
        shell_contract::runtime::CancelToken::default(),
        answers.clone(),
    );
    t.rt.replace_renderer(t.a.view);
    assert!(answers.borrow_mut().pop().unwrap().is_err());
    t.rt.send_from_generation(
        "docs:close-check-result",
        t.a,
        vec![obj(&[("dirty", false.into_ipc())]), s(&token)],
        old,
    );
    assert!(answers.borrow().is_empty());
    t.editor().register_view(t.a.view).unwrap();

    let cancel = shell_contract::runtime::CancelToken::default();
    owned_dirty_query(&t, source, cancel.clone(), answers.clone());
    cancel.cancel();
    t.rt.drain();
    assert!(answers.borrow_mut().pop().unwrap().is_err());

    owned_dirty_query(
        &t,
        source,
        shell_contract::runtime::CancelToken::default(),
        answers.clone(),
    );
    t.rt.advance(Duration::from_millis(2000));
    assert!(answers.borrow_mut().pop().unwrap().is_err());
}

#[test]
fn owned_dirty_queries_are_independent_and_allow_same_source_target() {
    let mut t = T::new(json!({}));
    let first = Rc::new(RefCell::new(Vec::new()));
    let second = Rc::new(RefCell::new(Vec::new()));
    let a = owned_dirty_query(
        &t,
        t.a,
        shell_contract::runtime::CancelToken::default(),
        first.clone(),
    );
    let b = owned_dirty_query(
        &t,
        t.a,
        shell_contract::runtime::CancelToken::default(),
        second.clone(),
    );
    assert_ne!(a, b);
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![obj(&[("dirty", false.into_ipc())]), s(&b)],
    );
    assert!(first.borrow().is_empty());
    assert_eq!(*second.borrow(), vec![Ok(false)]);
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![obj(&[("dirty", true.into_ipc())]), s(&a)],
    );
    assert_eq!(*first.borrow(), vec![Ok(true)]);
}

#[test]
fn owned_dirty_query_rejects_owner_loss_before_ack_without_waiting_for_core_notification() {
    let mut t = T::new(json!({}));
    let source = t.rt.view(2, Surface::Shell);
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let loss = shell_contract::runtime::CancelToken::default();
    let owner = shell_contract::ipc::NativeRequestOwner {
        view: source.view,
        context: shell_contract::ipc::NativeContext::with_owner_loss((), generation, loss.clone()),
    };
    let answers = Rc::new(RefCell::new(Vec::new()));
    let output = answers.clone();
    t.editor().query_dirty_owned(
        t.a.view,
        owner,
        shell_contract::runtime::CancelToken::default(),
        Box::new(move |result| output.borrow_mut().push(result)),
    );
    let token =
        t.rt.events()
            .iter()
            .rev()
            .find(|event| event.channel == "docs:close-check")
            .and_then(|event| event.args.first())
            .and_then(kit::as_str)
            .unwrap()
            .to_owned();
    loss.cancel();
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![obj(&[("dirty", false.into_ipc())]), s(&token)],
    );
    assert_eq!(answers.borrow().len(), 1);
    assert!(answers.borrow()[0].is_err());
}

#[test]
fn a_renderer_that_never_answers_is_asked_about_and_a_save_times_out() {
    let mut t = T::new(json!({}));
    let a = t.a;
    t.rt.script().message.push_back(1);
    let p = t.rt.invoke("docs:confirm-document-replace", a, vec![]);
    t.rt.advance(Duration::from_millis(1999));
    assert_eq!(p.take(), None);
    t.rt.advance(Duration::from_millis(1));
    assert_eq!(p.take(), Some(Ok(false.into_ipc())));
    let asked =
        t.rt.script()
            .calls
            .iter()
            .any(|c| matches!(c, HostCall::Message(_, m) if m.buttons.len() == 2));
    assert!(asked);

    // autosave on: a save that never reports falls back to the prompt after 120 s
    t.rt.script().message.push_back(2);
    let p = t.rt.invoke("docs:confirm-document-replace", a, vec![]);
    t.rt.send(
        "docs:close-check-result",
        a,
        vec![obj(&[
            ("dirty", true.into_ipc()),
            ("autoSave", true.into_ipc()),
        ])],
    );
    t.rt.advance(Duration::from_millis(119_999));
    assert_eq!(p.take(), None);
    t.rt.advance(Duration::from_millis(1));
    assert_eq!(p.take(), Some(Ok(false.into_ipc())));
}

#[test]
fn closing_twice_joins_one_check_and_a_destroyed_view_closes() {
    let mut t = T::new(json!({}));
    let a = t.a;
    let editor = editor_for(&t.rt.provided(), ViewId(1)).unwrap();
    let got = Rc::new(std::cell::RefCell::new(Vec::new()));
    for _ in 0..2 {
        let g = got.clone();
        editor.request_close(
            ViewId(1),
            None,
            Box::new(move |r| g.borrow_mut().push(r.unwrap())),
        );
    }
    assert_eq!(t.channels(), vec!["docs:close-check"]);
    retire_actor(&mut t);
    t.rt.destroy(a.view);
    assert_eq!(*got.borrow(), vec![true, true]);
}

#[test]
fn owned_close_does_not_join_or_settle_a_legacy_close() {
    let mut t = T::new(json!({}));
    let path = t.file("owned-close.docx", b"plain docx");
    t.open(&path);
    let source = t.rt.view(22, Surface::Shell);
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let origin =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .capture_owner(source.view, &generation)
            .unwrap();
    let editor = editor_for(&t.rt.provided(), t.a.view).unwrap();
    let owned = Rc::new(RefCell::new(Vec::new()));
    let legacy = Rc::new(RefCell::new(Vec::new()));
    let output = legacy.clone();
    editor.request_close(
        t.a.view,
        None,
        Box::new(move |result| output.borrow_mut().push(result)),
    );
    let output = owned.clone();
    editor.request_close_owned(
        t.a.view,
        None,
        origin,
        shell_contract::runtime::CancelToken::default(),
        Box::new(move |result| output.borrow_mut().push(result)),
    );
    let token =
        t.rt.events()
            .iter()
            .rev()
            .find(|event| {
                event.channel == "docs:close-check"
                    && event.args.first().and_then(kit::as_str).is_some()
            })
            .and_then(|event| event.args.first())
            .and_then(kit::as_str)
            .unwrap()
            .to_owned();
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![obj(&[("dirty", false.into_ipc())]), s(&token)],
    );
    assert_eq!(*owned.borrow(), vec![Ok(true)]);
    t.rt.script().message.push_back(2);
    assert!(legacy.borrow().is_empty());
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![obj(&[("dirty", true.into_ipc())])],
    );
    assert_eq!(*legacy.borrow(), vec![Ok(false)]);
}

#[test]
fn owned_close_source_cancellation_rejects_late_dirty_result() {
    let mut t = T::new(json!({}));
    let path = t.file("cancel-close.docx", b"plain docx");
    t.open(&path);
    let source = t.rt.view(22, Surface::Shell);
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let origin =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .capture_owner(source.view, &generation)
            .unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    let answers = Rc::new(RefCell::new(Vec::new()));
    let sink = answers.clone();
    editor_for(&t.rt.provided(), t.a.view)
        .unwrap()
        .request_close_owned(
            t.a.view,
            None,
            origin,
            cancel.clone(),
            Box::new(move |result| sink.borrow_mut().push(result)),
        );
    let token =
        t.rt.events()
            .iter()
            .rev()
            .find(|event| event.channel == "docs:close-check")
            .and_then(|event| event.args.first())
            .and_then(kit::as_str)
            .unwrap()
            .to_owned();
    cancel.cancel();
    t.rt.drain();
    assert_eq!(answers.borrow().len(), 1);
    assert!(answers.borrow()[0].is_err());
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![obj(&[("dirty", false.into_ipc())]), s(&token)],
    );
    assert_eq!(answers.borrow().len(), 1);
}

#[test]
fn captured_many_close_rejects_extra_participant_loss_before_dirty_reply() {
    let mut t = T::new(json!({}));
    let path = t.file("many-close.docx", b"plain docx");
    t.open(&path);
    let source = t.rt.view(22, Surface::Shell);
    let extra = t.rt.view(23, Surface::Shell);
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let capture = |view| {
        let generation = t.rt.services.views.renderer_generation(view).unwrap();
        effects.capture_owner(view, &generation).unwrap()
    };
    let origin = capture(source.view);
    let participant = capture(extra.view);
    let destination = capture(t.a.view);
    let answers = Rc::new(RefCell::new(Vec::new()));
    let output = answers.clone();
    t.editor().request_close_captured_many(
        t.a.view,
        None,
        origin,
        vec![participant],
        destination,
        shell_contract::runtime::CancelToken::default(),
        shell_contract::runtime::CancelToken::default(),
        Box::new(move |result| output.borrow_mut().push(result)),
    );
    let token =
        t.rt.events()
            .iter()
            .rev()
            .find(|event| event.channel == "docs:close-check")
            .and_then(|event| event.args.first())
            .and_then(kit::as_str)
            .unwrap()
            .to_owned();
    t.rt.destroy(extra.view);
    t.rt.drain();
    assert_eq!(answers.borrow().len(), 1);
    assert!(answers.borrow()[0].is_err());
    t.rt.send(
        "docs:close-check-result",
        t.a,
        vec![obj(&[("dirty", false.into_ipc())]), s(&token)],
    );
    assert_eq!(answers.borrow().len(), 1);
}

#[test]
fn crash_copies_are_written_and_a_save_drops_them() {
    let mut t = T::new(json!({}));
    let path = t.file("r.docx", b"x");
    let autosave = t.rt.dir.join("userData/docs-autosave");
    let copies = |dir: &Path| {
        std::fs::read_dir(dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "docx"))
                    .count()
            })
            .unwrap_or(0)
    };
    assert_eq!(
        t.call("docs:write-recovery", vec![s(&path), ab(b"draft")]),
        obj(&[("ok", false.into_ipc())])
    );
    t.open(&path);
    assert_eq!(
        t.call("docs:write-recovery", vec![s(&path), s("not bytes")]),
        obj(&[("ok", false.into_ipc())])
    );
    assert_eq!(
        t.call("docs:write-recovery", vec![s(&path), ab(b"draft")]),
        obj(&[("ok", true.into_ipc())])
    );
    assert_eq!(copies(&autosave), 1);
    t.call("docs:save", vec![s(&path), ab(b"saved")]);
    assert_eq!(copies(&autosave), 0);

    // an untitled document writes to its slot
    let meta = obj(&[("title", s("Draft")), ("format", s("docx"))]);
    assert_eq!(
        t.call("docs:write-recovery", vec![IpcValue::Null, ab(b"u"), meta]),
        obj(&[("ok", true.into_ipc())])
    );
    assert_eq!(copies(&autosave.join("untitled")), 1);
    assert!(t.editor().recovery_identity(ViewId(1)).is_some());
}

#[test]
fn a_copy_left_from_a_crash_is_offered_when_its_file_opens() {
    let mut t = T::new(json!({}));
    let path = t.file("k.docx", b"on disk");
    t.open(&path);
    t.call("docs:write-recovery", vec![s(&path), ab(b"unsaved edits")]);
    // another tab opens the file: restore
    let b = t.rt.view(2, Surface::Docs);
    t.editor().register_view(b.view).unwrap();
    t.rt.script().message.push_back(0);
    let opened =
        t.rt.invoke_now("docs:open-path", b, vec![s(&path)])
            .unwrap();
    assert_eq!(
        get(&opened, "dataUrl"),
        &s("handoff:756e7361766564206564697473")
    );
    assert_eq!(get(&opened, "recovered"), &IpcValue::Bool(true));
}

#[test]
fn failed_lazy_adoption_rejects_recovery_open() {
    let mut t = T::new(json!({ "adopt_lazy_media_hashes": { "error": "media worker stopped" } }));
    let path = t.file("k.docx", b"on disk");
    t.open(&path);
    t.call("docs:write-recovery", vec![s(&path), ab(b"unsaved edits")]);
    let view = t.rt.view(2, Surface::Docs);
    t.editor().register_view(view.view).unwrap();
    t.rt.script().message.push_back(0);
    let error =
        t.rt.invoke_now("docs:open-path", view, vec![s(&path)])
            .unwrap_err();
    assert_eq!(error.to_string(), "Error: media worker stopped");
}

#[test]
fn session_end_asks_every_booted_renderer_for_a_copy() {
    let mut t = T::new(json!({}));
    let a = t.a;
    let b = t.rt.view(2, Surface::Docs);
    t.editor().register_view(b.view).unwrap();
    t.call("docs:consume-copy", vec![]);
    t.rt.invoke_now("docs:consume-copy", b, vec![]).unwrap();
    let done = Rc::new(RefCell::new(None));
    let d = done.clone();
    t.editor()
        .write_copies_now(Box::new(move |result| *d.borrow_mut() = Some(result)));
    let events = t.rt.events();
    assert_eq!(events.len(), 2);
    let token_of = |view: ViewId| {
        events
            .iter()
            .find(|e| e.target == Target::View(view) && e.channel == "docs:write-recovery-now")
            .map(|e| e.args[0].clone())
            .unwrap()
    };
    t.rt.send("docs:write-recovery-now-done", b, vec![token_of(ViewId(1))]);
    assert!(
        done.borrow().is_none(),
        "another view cannot acknowledge the request"
    );
    t.rt.send("docs:write-recovery-now-done", a, vec![token_of(ViewId(1))]);
    assert!(done.borrow().is_none());
    t.rt.advance(Duration::from_millis(5000));
    assert!(done.borrow().as_ref().unwrap().is_err());
}

#[test]
fn replaced_docs_recovery_ignores_old_completion() {
    let mut t = T::new(json!({}));
    let view = t.a;
    t.call("docs:consume-copy", vec![]);
    let first = Rc::new(RefCell::new(None));
    let done = first.clone();
    t.editor()
        .write_copies_now(Box::new(move |result| *done.borrow_mut() = Some(result)));
    let old = t.rt.events()[0].args[0].clone();
    let second = Rc::new(RefCell::new(None));
    let done = second.clone();
    t.editor()
        .write_copies_now(Box::new(move |result| *done.borrow_mut() = Some(result)));
    assert!(matches!(&*first.borrow(), Some(Err(_))));
    let current = t.rt.events()[0].args[0].clone();
    t.rt.send("docs:write-recovery-now-done", view, vec![old]);
    assert!(second.borrow().is_none());
    t.rt.send("docs:write-recovery-now-done", view, vec![current]);
    assert!(matches!(&*second.borrow(), Some(Ok(()))));
}

#[test]
fn docs_recovery_reports_dispatch_failure() {
    let mut t = T::new(json!({}));
    t.call("docs:consume-copy", vec![]);
    t.rt.fail_next_checked_event("stream unavailable");
    let result = Rc::new(RefCell::new(None));
    let done = result.clone();
    t.editor()
        .write_copies_now(Box::new(move |value| *done.borrow_mut() = Some(value)));
    assert!(matches!(&*result.borrow(), Some(Err(error)) if error.0 == "stream unavailable"));
    assert!(t.rt.events().is_empty());
}

#[test]
fn docs_recovery_fails_on_renderer_generation_replacement() {
    let mut t = T::new(json!({}));
    let view = t.a;
    t.call("docs:consume-copy", vec![]);
    let first = Rc::new(RefCell::new(None));
    let done = first.clone();
    t.editor()
        .write_copies_now(Box::new(move |result| *done.borrow_mut() = Some(result)));
    let old = t.rt.events()[0].args[0].clone();
    let previous_generation = t.rt.services.views.renderer_generation(view.view).unwrap();
    t.rt.replace_renderer(view.view);
    assert!(matches!(&*first.borrow(), Some(Err(_))));
    let second = Rc::new(RefCell::new(None));
    let done = second.clone();
    t.editor()
        .write_copies_now(Box::new(move |result| *done.borrow_mut() = Some(result)));
    let current = t.rt.events()[0].args[0].clone();
    t.rt.send_from_generation(
        "docs:write-recovery-now-done",
        view,
        vec![current.clone()],
        previous_generation,
    );
    assert!(second.borrow().is_none());
    t.rt.send("docs:write-recovery-now-done", view, vec![old]);
    assert!(second.borrow().is_none());
    t.rt.send("docs:write-recovery-now-done", view, vec![current]);
    assert!(matches!(&*second.borrow(), Some(Ok(()))));
}

#[test]
fn first_document_queues_are_taken_once() {
    let mut t = T::new(json!({}));
    let editor = t.editor();
    assert_eq!(t.call("docs:consume-new-blank", vec![]), false.into_ipc());
    editor.mark_new_blank(ViewId(1));
    assert_eq!(t.call("docs:consume-new-blank", vec![]), true.into_ipc());
    assert_eq!(t.call("docs:consume-new-blank", vec![]), false.into_ipc());

    let copy = t.file("template.docx", b"tpl");
    editor.queue_copy(ViewId(1), copy, "Report.docx".into());
    let got = t.call("docs:consume-copy", vec![]);
    assert_eq!(
        got,
        obj(&[
            ("path", s("Report.docx")),
            ("name", s("Report.docx")),
            ("dataUrl", s("handoff:74706c")),
            ("hash", s("")),
            ("untitledCopy", true.into_ipc()),
        ])
    );
    assert_eq!(t.call("docs:consume-copy", vec![]), IpcValue::Null);

    let doc = t.file("queued.docx", b"q");
    assert_eq!(t.call("docs:consume-pending-open", vec![]), IpcValue::Null);
    editor.queue_open(ViewId(1), doc.clone());
    assert_eq!(
        get(&t.call("docs:consume-pending-open", vec![]), "path"),
        &s(&doc)
    );
}

#[test]
fn a_decided_open_waits_for_boot_then_goes_out_as_docs_opened() {
    let mut t = T::new(json!({}));
    let editor = t.editor();
    let doc = t.file("dec.docx", b"d");
    editor.open_file_with_decision(
        ViewId(1),
        doc.clone(),
        RecoveryOpenDecision::Original,
        captured_docs_owner(&t),
        shell_contract::runtime::CancelToken::default(),
    );
    assert!(t.channels().is_empty());
    assert_eq!(
        get(&t.call("docs:consume-pending-open", vec![]), "path"),
        &s(&doc)
    );
    t.call("docs:consume-copy", vec![]);
    t.rt.events();
    editor.open_file_with_decision(
        ViewId(1),
        doc.clone(),
        RecoveryOpenDecision::Original,
        captured_docs_owner(&t),
        shell_contract::runtime::CancelToken::default(),
    );
    // the shell's call is no channel call: its worker's answer waits on the core queue
    t.rt.drain();
    let events = t.rt.events();
    assert_eq!(events[0].channel, "docs:opened");
    assert_eq!(get(&events[0].args[0], "path"), &s(&doc));
}

#[test]
fn recovery_contract_is_discoverable_and_opens_the_requested_document() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenFileRequest,
    };
    let mut t = T::with_workers(json!({}), Some(Arc::new(BackgroundWorkers)));
    let path = t.file("recovery.docx", b"saved document");
    let source = t.rt.view(22, Surface::Shell);
    let source_generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let origin =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .capture_owner(source.view, &source_generation)
            .unwrap();
    let created = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(t.a.view, created.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: origin,
            participants: Vec::new(),
            cancel: shell_contract::runtime::CancelToken::default(),
            pending: shell_contract::runtime::CancelToken::default(),
        },
        t.a.view,
        created.clone(),
        closure,
        browser_owner,
    )
    .unwrap();
    let contract =
        t.rt.provided()
            .all::<dyn RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget>>()
            .into_iter()
            .find(|editor| editor.kind() == "docs")
            .unwrap();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    contract.open_file_with_decision(
        &ViewId(1),
        &RecoveryOpenFileRequest {
            file_path: path.clone(),
            decision: RecoveryOpenDecision::Original,
        },
        &target,
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while answer.borrow().is_none() {
        t.rt.drain();
        assert!(
            std::time::Instant::now() < deadline,
            "captured recovery did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(answer.borrow().as_ref().unwrap().is_ok());
    std::fs::rename(&path, format!("{path}.before-replacement")).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    let synthetic =
        t.rt.services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .unwrap()
            .clone();
    let provenance = shell_contract::hosts::synthetic::SyntheticNativeOrigin::new();
    target
        .retain_native_registration(
            synthetic
                .capture_invocation_root(t.a.view, &created, &provenance)
                .unwrap(),
        )
        .unwrap();
    let receiver = synthetic
        .capture_invocation_docs(t.a.view, &created, &provenance)
        .unwrap();
    let pending = t.rt.invoke_native(
        "docs:consume-pending-open",
        t.a,
        vec![],
        receiver.context,
        shell_contract::runtime::CancelToken::default(),
    );
    let result = await_owned_reply(&mut t, &pending).unwrap();
    assert_eq!(get(&result, "path"), &s(&path));
    assert_eq!(
        get(&result, "dataUrl"),
        &s("handoff:736176656420646f63756d656e74")
    );
    assert!(target.committed_seed().unwrap().settlement().is_none());
}

#[test]
fn a_headless_export_reads_its_target_and_settles_on_the_report() {
    let mut t = T::new(json!({}));
    let a = t.a;
    let report = Rc::new(std::cell::RefCell::new(None));
    let r = report.clone();
    t.editor().begin_headless_export(
        ViewId(1),
        "/in.docx".into(),
        "/out.pdf".into(),
        "pdf",
        Duration::from_secs(300),
        Box::new(move |res| *r.borrow_mut() = Some(res)),
    );
    assert_eq!(
        t.call("docs:consume-headless-export", vec![]),
        obj(&[("outPath", s("/out.pdf")), ("format", s("pdf"))])
    );
    assert_eq!(
        t.call("docs:consume-headless-export", vec![]),
        IpcValue::Null
    );
    t.rt.send(
        "docs:headless-export-done",
        a,
        vec![obj(&[("ok", false.into_ipc()), ("error", s("boom"))])],
    );
    assert_eq!(*report.borrow(), Some(Err("boom".to_owned())));
}

#[test]
fn only_the_active_tab_patches_the_view_menu() {
    let mut t = T::new(json!({ "active_docs_view": 1 }));
    let (a, b) = (t.a, t.rt.view(2, Surface::Docs));
    t.editor().register_view(b.view).unwrap();
    let state = obj(&[
        ("aiSidebar", true.into_ipc()),
        ("darkCanvas", false.into_ipc()),
        ("odt", true.into_ipc()),
    ]);
    t.rt.send("docs:view-menu-state", b, vec![state.clone()]);
    assert!(t.calls().is_empty());
    t.rt.send("docs:view-menu-state", a, vec![state]);
    assert_eq!(
        t.calls(),
        vec![
            r#"["MenuItem.checked","docs-menu-ai-sidebar",true]"#,
            r#"["MenuItem.checked","docs-menu-dark-mode",false]"#,
            r#"["MenuItem.enabled","docs-menu-insert-equation",false]"#,
            r#"["MenuItem.enabled","docs-menu-insert-comment",false]"#,
        ]
    );
    // the background tab's state is kept for the menu to read when it is focused
    let editor = t.editor();
    assert_eq!(editor.view_menu_state(b.view), Some([true, false, true]));
    assert_eq!(editor.view_menu_state(ViewId(9)), None);
}

#[test]
fn a_torn_down_tab_loses_its_writes() {
    let mut t = T::new(json!({}));
    let path = t.file("t.docx", b"x");
    t.open(&path);
    t.editor().teardown(ViewId(1), false);
    assert_eq!(t.channels(), vec!["docs:teardown"]);
    assert!(
        editor_for(&t.rt.provided(), ViewId(1))
            .unwrap()
            .torn_down(ViewId(1))
    );
    assert_eq!(
        get(&t.call("docs:save", vec![s(&path), ab(b"y")]), "ok"),
        &IpcValue::Bool(false)
    );
    assert_eq!(
        t.call("docs:password-intent-revision", vec![]),
        (-1.0).into_ipc()
    );
    assert_eq!(
        t.call("docs:save-as-pick", vec![s("n.docx")]),
        IpcValue::Null
    );
    assert_eq!(
        t.call("docs:confirm-marked-export", vec![s("odt")]),
        s("cancel")
    );
}

#[test]
fn failed_lazy_forget_does_not_interrupt_teardown() {
    let mut t = T::new(json!({ "forget_lazy_media_owner": { "error": "media worker stopped" } }));
    let path = t.file("t.docx", b"on disk");
    t.open(&path);
    t.editor().teardown(ViewId(1), false);
    assert_eq!(t.channels(), vec!["docs:teardown"]);
    assert_eq!(
        get(&t.call("docs:save", vec![s(&path), ab(b"changed")]), "ok"),
        &IpcValue::Bool(false)
    );
    assert_eq!(std::fs::read(path).unwrap(), b"on disk");
}

#[test]
fn a_renamed_file_keeps_its_grant_under_the_new_name() {
    let mut t = T::new(json!({}));
    let old = t.file("old.docx", b"x");
    t.open(&old);
    let new = t.rt.dir.join("new.docx").to_string_lossy().into_owned();
    std::fs::rename(&old, &new).unwrap();
    t.rt.events();
    let editor = editor_for(&t.rt.provided(), ViewId(1)).unwrap();
    editor.file_renamed(ViewId(1), Path::new(&old), Path::new(&new));
    let events = t.rt.events();
    assert_eq!(events[0].channel, "docs:renamed");
    assert_eq!(
        events[0].args[0],
        obj(&[("oldPath", s(&old)), ("newPath", s(&new))])
    );
    assert_eq!(
        get(&t.call("docs:save", vec![s(&new), ab(b"y")]), "ok"),
        &IpcValue::Bool(true)
    );
    assert_eq!(editor.document_path(ViewId(1)), Some(PathBuf::from(&new)));
}

#[test]
fn failed_lazy_media_move_disables_saving_without_discarding_recovery() {
    let mut t = T::new(json!({ "move_lazy_media_source": { "error": "media worker stopped" } }));
    let old = t.file("old.docx", b"on disk");
    t.open(&old);
    t.call("docs:write-recovery", vec![s(&old), ab(b"unsaved edits")]);
    let autosave = t.rt.dir.join("userData/docs-autosave");
    let count = || std::fs::read_dir(&autosave).unwrap().count();
    let before = count();
    assert!(before > 0);
    let new = t.rt.dir.join("new.docx").to_string_lossy().into_owned();
    std::fs::rename(&old, &new).unwrap();
    let editor = editor_for(&t.rt.provided(), ViewId(1)).unwrap();
    editor.file_renamed(ViewId(1), Path::new(&old), Path::new(&new));
    assert!(editor.torn_down(ViewId(1)));
    assert_eq!(
        get(&t.call("docs:save", vec![s(&new), ab(b"changed")]), "ok"),
        &IpcValue::Bool(false)
    );
    assert_eq!(std::fs::read(&new).unwrap(), b"on disk");
    assert_eq!(count(), before);
}

#[test]
fn the_editor_saves_and_exports_by_menu_command() {
    let t = T::new(json!({}));
    let editor = editor_for(&t.rt.provided(), ViewId(1)).unwrap();
    assert!(editor.owns(ViewId(1)));
    let other = t.rt.view(3, Surface::Pdf);
    assert!(!editor.owns(other.view));
    let got = Rc::new(std::cell::RefCell::new(None));
    let g = got.clone();
    editor.request_save(
        ViewId(1),
        SaveMode::Save,
        Box::new(move |r| *g.borrow_mut() = Some(r)),
    );
    // the tab menu's save is the renderer's own flow: the command goes out
    assert_eq!(*got.borrow(), Some(Ok(true)));
    editor.export_request(ViewId(1), "pdf");
    let events: Vec<_> =
        t.rt.events()
            .into_iter()
            .map(|e| (e.channel, e.args[0].clone()))
            .collect();
    assert_eq!(
        events,
        vec![
            ("menu:command", s("save")),
            ("menu:command", s("export-pdf"))
        ]
    );
}

#[test]
fn export_pdf_writes_an_authorized_target_and_reveals_it() {
    let mut t = T::new(json!({}));
    let geometry = vec![s("doc.docx"), 11906.0.into_ipc(), 16838.0.into_ipc()];
    let bad = t.call(
        "docs:export-pdf",
        vec![s("doc.docx"), 0.0.into_ipc(), 1.0.into_ipc()],
    );
    assert_eq!(get(&bad, "error"), &s("invalid page size or scale"));
    let mut named = geometry.clone();
    named.push(s("/tmp/not-authorized.pdf"));
    assert_eq!(
        get(&t.call("docs:export-pdf", named), "error"),
        &s("export target is not an authorized path")
    );
    t.rt.script().save.push_back(None);
    assert_eq!(
        t.call("docs:export-pdf", geometry.clone()),
        obj(&[("ok", false.into_ipc())])
    );

    let out = t.rt.dir.join("doc.pdf");
    t.rt.script().save.push_back(Some(out.clone()));
    t.rt.script().pdf = Some(b"%PDF-1.7".to_vec());
    let r = t.call("docs:export-pdf", geometry.clone());
    assert_eq!(
        r,
        obj(&[("ok", true.into_ipc()), ("path", s(&out.to_string_lossy()))])
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"%PDF-1.7");
    assert!(
        t.rt.script()
            .calls
            .contains(&HostCall::ShowItemInFolder(out.clone()))
    );

    // the dialog's pick stays authorized for the renderer's retry; a print failure names it
    t.rt.script().pdf = None;
    let mut retry = geometry;
    retry.push(s(&out.to_string_lossy()));
    let failed = t.call("docs:export-pdf", retry);
    assert_eq!(get(&failed, "error"), &s("Error: printToPDF failed"));
    assert_eq!(get(&failed, "path"), &s(&out.to_string_lossy()));
}

#[test]
fn export_html_and_markdown_write_their_files() {
    let mut t = T::new(json!({}));
    assert_eq!(
        get(&t.call("docs:export-html", vec![s("d"), s("")]), "error"),
        &s("empty document")
    );
    let html = t.rt.dir.join("d.html");
    t.rt.script().save.push_back(Some(html.clone()));
    t.call("docs:export-html", vec![s("d.docx"), s("<p>hi</p>")]);
    assert_eq!(std::fs::read_to_string(&html).unwrap(), "<p>hi</p>");

    assert_eq!(
        get(
            &t.call("docs:export-markdown", vec![s("d"), s("# x"), s("no")]),
            "error"
        ),
        &s("bad image list")
    );
    let md = t.rt.dir.join("note (1).md");
    t.rt.script().save.push_back(Some(md.clone()));
    let images = IpcValue::Array(vec![
        obj(&[
            ("name", s("image1.png")),
            ("src", s("data:image/png;base64,aGk=")),
        ]),
        obj(&[
            ("name", s("../evil.png")),
            ("src", s("data:image/png;base64,aGk=")),
        ]),
    ]);
    let r = t.call(
        "docs:export-markdown",
        vec![
            s("d.docx"),
            s("![](__seuoffice_media__/image1.png)"),
            images,
        ],
    );
    assert_eq!(get(&r, "imagesDropped"), &1.0.into_ipc());
    assert_eq!(
        std::fs::read_to_string(&md).unwrap(),
        "![](note%20%281%29_files/image1.png)"
    );
    assert_eq!(
        std::fs::read(t.rt.dir.join("note (1)_files/image1.png")).unwrap(),
        b"hi"
    );
}

#[test]
fn export_hwp_converts_confirms_losses_and_writes() {
    let mut t = T::new(json!({
        "convert_to_hwp": [{}, { "losses": [{ "feature": "chart", "kind": "dropped", "count": 2 }] }]
    }));
    let bad = obj(&[
        ("ok", false.into_ipc()),
        ("code", s("failed")),
        ("error", s("bad export request")),
    ]);
    assert_eq!(t.call("docs:export-hwp", vec![s("d"), s("x")]), bad);
    let unauthorized = t.call("docs:export-hwp", vec![s("d"), ab(b"x"), s("/x.hwp")]);
    assert_eq!(get(&unauthorized, "code"), &s("unauthorized"));

    let out = t.rt.dir.join("d.hwp");
    t.rt.script().save.push_back(Some(out.clone()));
    let r = t.call("docs:export-hwp", vec![s("d.docx"), ab(b"docx")]);
    assert_eq!(
        r,
        obj(&[("ok", true.into_ipc()), ("path", s(&out.to_string_lossy()))])
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"hwp:docx");

    // a loss asks first; Cancel writes nothing
    std::fs::remove_file(&out).unwrap();
    t.rt.script().message.push_back(1);
    let r = t.call(
        "docs:export-hwp",
        vec![s("d.docx"), ab(b"docx"), s(&out.to_string_lossy())],
    );
    assert_eq!(
        r,
        obj(&[("ok", false.into_ipc()), ("canceled", true.into_ipc())])
    );
    assert!(!out.exists());
}

#[test]
fn merged_pdf_parts_land_in_one_file() {
    let mut t = T::new(json!({}));
    let out = t.rt.dir.join("m.pdf");
    t.rt.script().save.push_back(Some(out.clone()));
    let r = t.call(
        "docs:save-merged-pdf",
        vec![s("m.docx"), IpcValue::Array(vec![s("YQ=="), s("Yg==")])],
    );
    assert_eq!(get(&r, "ok"), &IpcValue::Bool(true));
    assert_eq!(std::fs::read(&out).unwrap(), b"ab");
}

#[test]
fn pick_image_reads_the_picked_picture() {
    let mut t = T::new(json!({}));
    let png = t.file("pic.PNG", b"img");
    t.rt.script()
        .open
        .push_back(Some(vec![PathBuf::from(&png)]));
    assert_eq!(
        t.call("docs:pick-image", vec![]),
        obj(&[
            ("base64", s("aW1n")),
            ("mime", s("image/png")),
            ("name", s("pic.PNG"))
        ])
    );
    let bmp = t.file("pic.bmp", b"img");
    t.rt.script()
        .open
        .push_back(Some(vec![PathBuf::from(&bmp)]));
    assert_eq!(t.call("docs:pick-image", vec![]), IpcValue::Null);
}

#[test]
fn mail_merge_reads_a_source_and_writes_one_file_per_record() {
    let mut t =
        T::new(json!({ "read_merge_data_source": [{ "fields": ["a"] }, { "error": "bad csv" }] }));
    t.rt.script()
        .open
        .push_back(Some(vec![PathBuf::from("/list.csv")]));
    assert_eq!(
        get(&t.call("docs:merge-data-source", vec![]), "ok"),
        &IpcValue::Bool(true)
    );
    t.rt.script()
        .open
        .push_back(Some(vec![PathBuf::from("/list.csv")]));
    assert_eq!(
        t.call("docs:merge-data-source", vec![]),
        obj(&[("ok", false.into_ipc()), ("error", s("bad csv"))])
    );

    assert_eq!(
        get(&t.call("docs:merge-output", vec![obj(&[])]), "error"),
        &s("nothing to write")
    );
    let folder = t.rt.dir.join("merged");
    std::fs::create_dir_all(&folder).unwrap();
    t.rt.script().open.push_back(Some(vec![folder.clone()]));
    let files = IpcValue::Array(vec![
        obj(&[("name", s("Kim")), ("data", ab(b"1"))]),
        obj(&[("name", s("Kim")), ("data", ab(b"2"))]),
    ]);
    let r = t.call(
        "docs:merge-output",
        vec![obj(&[("mode", s("folder")), ("files", files)])],
    );
    assert_eq!(get(&r, "count"), &2.0.into_ipc());
    assert_eq!(std::fs::read(folder.join("Kim-2.docx")).unwrap(), b"2");
}

#[test]
fn copying_a_picture_puts_its_bitmap_and_sized_html_on_the_clipboard() {
    let mut t = T::new(json!({ "image_size": [10, 20] }));
    assert_eq!(
        t.call(
            "docs:copy-image-to-clipboard",
            vec![s("data:text/html;base64,AA==")]
        ),
        false.into_ipc()
    );
    let meta = r#"{"imageWidthPx":5,"x":"<"}"#;
    let ok = t.call(
        "docs:copy-image-to-clipboard",
        vec![s("data:image/png;base64,aGk="), s(meta)],
    );
    assert_eq!(ok, true.into_ipc());
    let html = serde_json::json!(["clipboard.write", {
        "html": "<img src=\"data:image/png;base64,aGk=\" width=\"5\" height=\"20\" data-image-meta=\"{&quot;imageWidthPx&quot;:5,&quot;x&quot;:&quot;&lt;&quot;}\">",
        "text": null
    }])
    .to_string();
    assert!(t.calls().contains(&html), "{:?}", t.calls());
}

#[test]
fn capture_rect_checks_its_rectangle() {
    let mut t = T::new(json!({ "capture_rect": "png" }));
    assert_eq!(
        t.call("docs:capture-rect", vec![obj(&[("x", 0.0.into_ipc())])]),
        IpcValue::Null
    );
    let rect = obj(&[
        ("x", (-3.5).into_ipc()),
        ("y", 1.2.into_ipc()),
        ("width", 10.2.into_ipc()),
        ("height", 5.0.into_ipc()),
    ]);
    assert_eq!(
        t.call("docs:capture-rect", vec![rect]),
        kit::u8_array(b"png".to_vec())
    );
    assert!(t.calls().contains(
        &r#"["webContents.capturePage",1,{"x":0,"y":1,"width":11,"height":5}]"#.to_owned()
    ));
}

#[test]
fn ai_doc_creation_reports_tab_open_failure() {
    let mut t = T::new(json!({"open_ai_doc_tab": "tab creation failed"}));
    let result = t.call(
        "docs:create-document",
        vec![obj(&[
            ("type", s("docx")),
            ("title", s("T")),
            ("content", s("<p>content</p>")),
        ])],
    );
    assert_eq!(get(&result, "ok"), &IpcValue::Bool(false));
    assert_eq!(get(&result, "error"), &s("tab creation failed"));
}

#[test]
fn ai_documents_open_in_a_tab_or_are_written_beside_the_others() {
    let mut t = T::new(json!({}));
    assert_eq!(
        t.call(
            "docs:create-document",
            vec![obj(&[("type", s("md")), ("content", s("  "))])]
        ),
        obj(&[
            ("ok", false.into_ipc()),
            ("error", s("content must not be empty"))
        ])
    );
    let docx = t.call(
        "docs:create-document",
        vec![obj(&[
            ("type", s("docx")),
            ("title", s("T")),
            ("content", s("<p>x</p>")),
        ])],
    );
    assert_eq!(docx, obj(&[("ok", true.into_ipc())]));
    assert!(
        t.calls()
            .iter()
            .any(|c| c.starts_with(r#"["openAiDocTabFor""#))
    );
    let md = t.call(
        "docs:create-document",
        vec![obj(&[
            ("type", s("md")),
            ("title", s("a/b")),
            ("content", s("# hi")),
        ])],
    );
    let path = kit::as_str(get(&md, "path")).unwrap();
    assert!(path.ends_with("a_b.md"), "{path}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "# hi");
    assert_eq!(
        get(
            &t.call(
                "docs:create-document",
                vec![obj(&[("type", s("xls")), ("content", s("x"))])]
            ),
            "error"
        ),
        &s("unsupported document type: xls")
    );
}

#[test]
fn altchunk_html_becomes_docx_bytes() {
    let mut t = T::new(json!({ "html_to_docx": "DOCX" }));
    assert_eq!(
        t.call("docs:altchunk-html-to-docx", vec![s("   ")]),
        IpcValue::Null
    );
    assert_eq!(
        t.call("docs:altchunk-html-to-docx", vec![s("<p>x</p>")]),
        kit::u8_array(b"DOCX".to_vec())
    );
}

#[test]
fn analyze_media_rejects_a_missing_request_and_answers_the_tool() {
    let mut t = T::new(json!({ "analyze_media": { "text": "a cat" } }));
    let a = t.a;
    let err =
        t.rt.invoke_now("docs:analyze-media", a, vec![])
            .unwrap_err();
    assert_eq!(
        err.to_string(),
        "TypeError: Cannot read properties of undefined (reading 'mediaUrls')"
    );
    let r = t.call(
        "docs:analyze-media",
        vec![obj(&[("mediaUrls", IpcValue::Array(vec![s("u"), s("")]))])],
    );
    assert_eq!(r, obj(&[("text", s("a cat"))]));
    assert!(
        t.calls()
            .iter()
            .any(|c| c.starts_with(r#"["analyzeMediaTool""#) && c.contains(r#"["u"]"#))
    );
}

#[test]
fn marked_odt_answers_the_button_pressed() {
    let mut t = T::new(json!({}));
    t.rt.script().message.push_back(1);
    assert_eq!(
        t.call("docs:confirm-marked-export", vec![s("odt")]),
        s("clear")
    );
    t.rt.script().message.push_back(0);
    assert_eq!(
        t.call("docs:confirm-marked-export", vec![s("odt")]),
        s("docx")
    );
}

#[test]
fn materialize_takes_only_an_array_buffer() {
    let mut t = T::new(json!({}));
    assert_eq!(
        t.call("docs:materialize-docx", vec![kit::u8_array(b"x".to_vec())]),
        IpcValue::Null
    );
    assert_eq!(
        t.call("docs:materialize-docx", vec![ab(b"x")]),
        obj(&[("dataUrl", s("handoff:78"))])
    );
}

#[test]
fn save_image_as_writes_the_fetched_picture() {
    let mut t = T::new(json!({}));
    assert_eq!(
        t.call("docs:save-image-as", vec![1.0.into_ipc()]),
        obj(&[("ok", false.into_ipc())])
    );
    let out = t.rt.dir.join("pic.png");
    t.rt.script().save.push_back(Some(out.clone()));
    let r = t.call("docs:save-image-as", vec![s("data:image/png;base64,aGk=")]);
    assert_eq!(
        r,
        obj(&[("ok", true.into_ipc()), ("path", s(&out.to_string_lossy()))])
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"hi");
}

#[test]
fn pick_export_images_target_authorizes_its_folder() {
    let mut t = T::new(json!({}));
    t.rt.script().open.push_back(None);
    assert_eq!(
        t.call("docs:pick-export-images-target", vec![]),
        IpcValue::Null
    );
    let dir = t.rt.dir.join("imgs");
    std::fs::create_dir_all(&dir).unwrap();
    t.rt.script().open.push_back(Some(vec![dir.clone()]));
    let r = t.call("docs:pick-export-images-target", vec![]);
    let d = dir.to_string_lossy().into_owned();
    assert_eq!(get(&r, "dir"), &s(&d));
    assert_eq!(
        get(
            &t.call(
                "docs:write-export-image",
                vec![s(&d), s("a.png"), s("aGk=")]
            ),
            "ok"
        ),
        &IpcValue::Bool(true)
    );
    assert_eq!(
        get(
            &t.call(
                "docs:write-export-image",
                vec![s("/other"), s("a.png"), s("aGk=")]
            ),
            "error"
        ),
        &s("export target is not an authorized folder")
    );
}

#[test]
fn a_destroyed_view_drops_its_state() {
    let mut t = T::new(json!({}));
    let path = t.file("z.docx", b"x");
    t.open(&path);
    retire_actor(&mut t);
    t.rt.destroy(ViewId(1));
    let editor = editor_for(&t.rt.provided(), ViewId(1));
    assert!(editor.is_none());
    assert!(t.editor().torn_down(ViewId(1)));
    assert_eq!(t.editor().document_path(ViewId(1)), None);
    assert!(
        t.calls()
            .contains(&r#"["forgetLazyMediaOwner",1]"#.to_owned())
    );
}

#[test]
fn an_untitled_copy_reopens_in_a_new_tab_that_keeps_writing_it() {
    let mut t = T::new(json!({}));
    let meta = obj(&[("title", s("Notes")), ("format", s("docx"))]);
    t.call(
        "docs:write-recovery",
        vec![IpcValue::Null, ab(b"draft"), meta],
    );
    let copy = t.editor().recovery_identity(ViewId(1)).unwrap();
    // the crashed tab is gone; a new one restores its copy before it booted
    t.editor().teardown(ViewId(1), true);
    let b = t.rt.view(2, Surface::Docs);
    t.editor().register_view(b.view).unwrap();
    t.editor().open_untitled_copy(
        ViewId(2),
        main_core::packages::electron_utils::recovery_store::RecoveryIdentity::Untitled(
            copy.clone(),
        ),
        String::new(),
        None,
    );
    let restored = t.rt.invoke_now("docs:consume-copy", b, vec![]).unwrap();
    assert_eq!(get(&restored, "name"), &s("Notes.docx"));
    assert_eq!(get(&restored, "dataUrl"), &s("handoff:6472616674"));
    assert_eq!(get(&restored, "recovered"), &IpcValue::Bool(true));
    assert_eq!(t.editor().recovery_identity(ViewId(2)), Some(copy));
}

#[test]
fn discarding_a_tab_drops_its_copy_and_tells_the_renderer() {
    let mut t = T::new(json!({}));
    let path = t.file("x.docx", b"x");
    t.open(&path);
    t.call("docs:write-recovery", vec![s(&path), ab(b"edit")]);
    let autosave = t.rt.dir.join("userData/docs-autosave");
    let count = || {
        std::fs::read_dir(&autosave)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_file())
            .count()
    };
    assert!(count() > 0);
    t.rt.events();
    t.editor().discard_recovery(ViewId(1));
    assert_eq!(count(), 0);
    assert_eq!(t.channels(), vec!["docs:recovery-copy-dropped"]);
    // the "Don't Save" holds: the next tick writes nothing
    assert_eq!(
        t.call("docs:write-recovery", vec![s(&path), ab(b"edit")]),
        obj(&[("ok", false.into_ipc())])
    );
}

#[test]
fn other_apps_create_ai_documents_through_the_editor() {
    let t = T::new(json!({}));
    let got = Rc::new(std::cell::RefCell::new(None));
    let g = got.clone();
    let request = obj(&[
        ("type", s("html")),
        ("title", s("Page")),
        ("content", s("<b>x</b>")),
    ]);
    t.editor().create_ai_document(
        &request,
        ViewId(1),
        Box::new(move |v| *g.borrow_mut() = Some(v)),
    );
    let mut t = t;
    t.rt.drain();
    let reply = got.borrow().clone().unwrap();
    let path = kit::as_str(get(&reply, "path")).unwrap().to_owned();
    assert!(path.ends_with("Page.html"), "{path}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "<b>x</b>");
}

#[test]
fn save_to_path_waits_for_renderer_and_real_file() {
    use jscompat::JsonValue;
    use main_core::shell::mcp::tools::document_tools::McpEditorCommandName;
    use main_core::shell::mcp::tools::sheets_tools::SheetsCommandName;
    use shell_contract::editors::Then;
    use shell_ipc_a::mcp::McpBridgeControl;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Bridge {
        pending: RefCell<Option<Then<Option<JsonValue>>>>,
        request: RefCell<Option<(i64, McpEditorCommandName, JsonValue)>>,
    }
    impl McpBridgeControl for Bridge {
        fn docs_open_blank_tab(&self, _: Then<Option<JsonValue>>) {
            unreachable!()
        }
        fn docs_run_command(
            &self,
            id: i64,
            command: McpEditorCommandName,
            payload: Option<JsonValue>,
            then: Then<Option<JsonValue>>,
        ) {
            *self.request.borrow_mut() = Some((id, command, payload.unwrap()));
            *self.pending.borrow_mut() = Some(then);
        }
        fn sheets_open_blank_tab(&self, _: Then<Option<JsonValue>>) {
            unreachable!()
        }
        fn sheets_run_command(
            &self,
            _: i64,
            _: SheetsCommandName,
            _: Option<JsonValue>,
            _: Then<Option<JsonValue>>,
        ) {
            unreachable!()
        }
    }

    let mut t = T::new(json!({}));
    let bridge = Rc::new(Bridge::default());
    t.rt.provided().add::<dyn McpBridgeControl>(bridge.clone());
    let path = t.rt.dir.join("created.docx");
    let result = Rc::new(RefCell::new(None));
    let sink = result.clone();
    t.editor().save_to_path(
        ViewId(1),
        path.clone(),
        Box::new(move |answer| *sink.borrow_mut() = Some(answer)),
    );
    assert!(result.borrow().is_none());
    let request = bridge.request.borrow();
    let (id, command, payload) = request.as_ref().unwrap();
    assert_eq!(*id, 1);
    assert_eq!(*command, McpEditorCommandName::SaveDocument);
    assert_eq!(
        payload.get("overwrite").as_deref(),
        Some(&JsonValue::Bool(true))
    );
    drop(request);
    std::fs::write(&path, b"docx").unwrap();
    let mut reply = jscompat::JsObject::new();
    reply.set("ok", JsonValue::Bool(true));
    reply.set(
        "path",
        JsonValue::String(path.to_string_lossy().into_owned()),
    );
    bridge.pending.borrow_mut().take().unwrap()(Ok(Some(JsonValue::Object(reply))));
    assert!(result.borrow().is_none());
    t.rt.drain();
    assert_eq!(*result.borrow(), Some(Ok(())));

    let missing = t.rt.dir.join("not-written.docx");
    *result.borrow_mut() = None;
    let sink = result.clone();
    t.editor().save_to_path(
        ViewId(1),
        missing.clone(),
        Box::new(move |answer| *sink.borrow_mut() = Some(answer)),
    );
    let mut reply = jscompat::JsObject::new();
    reply.set("ok", JsonValue::Bool(true));
    reply.set(
        "path",
        JsonValue::String(missing.to_string_lossy().into_owned()),
    );
    bridge.pending.borrow_mut().take().unwrap()(Ok(Some(JsonValue::Object(reply))));
    t.rt.drain();
    assert!(result.borrow().as_ref().unwrap().is_err());

    *result.borrow_mut() = None;
    let sink = result.clone();
    t.editor().save_to_path(
        ViewId(1),
        path.clone(),
        Box::new(move |answer| *sink.borrow_mut() = Some(answer)),
    );
    let mut reply = jscompat::JsObject::new();
    reply.set("ok", JsonValue::Bool(true));
    reply.set(
        "path",
        JsonValue::String(path.to_string_lossy().into_owned()),
    );
    bridge.pending.borrow_mut().take().unwrap()(Ok(Some(JsonValue::Object(reply))));
    t.rt.destroy(ViewId(1));
    t.rt.drain();
    assert!(result.borrow().as_ref().unwrap().is_err());
}

#[test]
fn save_to_path_fails_without_bridge_or_for_a_closed_view() {
    let mut t = T::new(json!({}));
    let path = t.rt.dir.join("missing.docx");
    let answer = Rc::new(std::cell::RefCell::new(None));
    let sink = answer.clone();
    t.editor().save_to_path(
        ViewId(1),
        path.clone(),
        Box::new(move |result| *sink.borrow_mut() = Some(result)),
    );
    assert!(answer.borrow().as_ref().unwrap().is_err());
    t.rt.destroy(ViewId(1));
    let sink = answer.clone();
    t.editor().save_to_path(
        ViewId(1),
        path,
        Box::new(move |result| *sink.borrow_mut() = Some(result)),
    );
    assert!(answer.borrow().as_ref().unwrap().is_err());
}

#[test]
fn owned_ai_creation_writes_real_markdown_before_opening() {
    let mut t = T::with_workers(
        json!({ "open_generated_path": true }),
        Some(Arc::new(BackgroundWorkers)),
    );
    let source_path = t.file("source.docx", b"plain docx");
    let opened = t.rt.invoke("docs:open-path", t.a, vec![s(&source_path)]);
    assert!(await_owned_reply(&mut t, &opened).is_ok());
    let source = t.a;
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap();
    let native = effects.capture_owner(source.view, &generation).unwrap();
    let owner = t
        .editor()
        .capture_document_owner(source.view, native)
        .unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    let pending = shell_contract::runtime::CancelToken::default();
    let admission = t
        .editor()
        .0
        .capture_create_admission(source.view, &owner, &owner, &cancel, &pending)
        .unwrap();
    let answers = Arc::new(Mutex::new(None));
    let completed = answers.clone();
    t.editor().create_ai_document_owned(
        obj(&[
            ("type", s("md")),
            ("title", s("Owned markdown")),
            ("content", s("# 제목\n\nreal bytes\n")),
        ]),
        source.view,
        owner.clone(),
        owner,
        cancel,
        pending,
        admission,
        Box::new(move |result| *completed.lock().unwrap() = Some(result)),
    );
    let answer = await_ai_creation(&mut t, &answers).unwrap();
    assert_eq!(get(&answer, "ok"), &IpcValue::Bool(true));
    let path = kit::as_str(get(&answer, "path")).unwrap();
    assert_eq!(
        std::fs::read(path).unwrap(),
        "# 제목\n\nreal bytes\n".as_bytes()
    );
    assert!(
        t.calls()
            .iter()
            .any(|call| call.contains("openGeneratedPath") && call.contains("Owned markdown"))
    );
}

#[test]
fn revoked_ai_creator_cannot_write_or_open_a_new_document() {
    let (mut t, _) = background_document();
    let source = t.a;
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap();
    let native = effects.capture_owner(source.view, &generation).unwrap();
    let owner = t
        .editor()
        .capture_document_owner(source.view, native)
        .unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    let pending = shell_contract::runtime::CancelToken::default();
    let admission = t
        .editor()
        .0
        .capture_create_admission(source.view, &owner, &owner, &cancel, &pending)
        .unwrap();
    cancel.cancel();
    let answers = Arc::new(Mutex::new(None));
    let completed = answers.clone();
    t.editor().create_ai_document_owned(
        obj(&[
            ("type", s("html")),
            ("title", s("Revoked")),
            ("content", s("<p>must not land</p>")),
        ]),
        source.view,
        owner.clone(),
        owner,
        cancel,
        pending,
        admission,
        Box::new(move |result| *completed.lock().unwrap() = Some(result)),
    );
    assert!(await_ai_creation(&mut t, &answers).is_err());
    assert!(
        !t.calls()
            .iter()
            .any(|call| call.contains("writeNewDocument") || call.contains("openGeneratedPath"))
    );
}

fn await_ai_creation(
    t: &mut T,
    answer: &Arc<Mutex<Option<Result<IpcValue, String>>>>,
) -> Result<IpcValue, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        t.rt.drain();
        if let Some(answer) = answer.lock().unwrap().take() {
            return answer;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the owned creation did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn await_owned_reply(t: &mut T, pending: &crate::testing::Pending) -> shell_contract::ipc::Reply {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        t.rt.drain();
        if let Some(reply) = pending.take() {
            return reply;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the owned save did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn background_document() -> (T, String) {
    let mut t = T::with_workers(json!({}), Some(Arc::new(BackgroundWorkers)));
    let path = t.file("owned-original.docx", b"original bytes");
    let open = t.rt.invoke("docs:open-path", t.a, vec![s(&path)]);
    assert_eq!(
        get(&await_owned_reply(&mut t, &open).unwrap(), "path"),
        &s(&path)
    );
    (t, path)
}

#[allow(clippy::type_complexity)]
fn owned_save_request(
    t: &mut T,
    target: PathBuf,
) -> (
    String,
    shell_contract::ipc::NativeRequestOwner,
    shell_contract::runtime::CancelToken,
    Rc<RefCell<Option<Result<(), shell_contract::IpcError>>>>,
) {
    let source = t.rt.view(22, Surface::Shell);
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let origin = effects.capture_owner(source.view, &generation).unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    let answers = Rc::new(RefCell::new(None));
    let completed = answers.clone();
    t.editor().save_to_path_owned(
        t.a.view,
        target,
        origin,
        cancel.clone(),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    t.rt.drain();
    let token =
        t.rt.events()
            .into_iter()
            .find(|event| event.channel == "docs:close-save-request")
            .and_then(|event| {
                event
                    .args
                    .first()
                    .map(|value| get(value, "requestId").clone())
            })
            .and_then(|value| kit::as_str(&value).map(str::to_owned))
            .unwrap();
    let destination = t.editor().0.pending_save_destination(&token).unwrap();
    (token, destination, cancel, answers)
}

fn acknowledge_owned_save(
    t: &mut T,
    token: &str,
    destination: &shell_contract::ipc::NativeRequestOwner,
    ok: bool,
) {
    t.rt.send_native(
        "docs:close-save-result",
        t.a,
        vec![ok.into_ipc(), s(token)],
        destination.context.clone(),
        shell_contract::runtime::CancelToken::default(),
    );
}

#[test]
fn owned_save_waits_for_real_file_write_and_matching_renderer_ack() {
    let (mut t, original) = background_document();
    let target = t.rt.dir.join("nested/owned-save.docx");
    let (token, destination, _, answers) = owned_save_request(&mut t, target.clone());
    assert!(answers.borrow().is_none());
    assert!(!target.exists());
    let pending = t.rt.invoke_native(
        "docs:save-to",
        t.a,
        vec![
            s(&target.to_string_lossy()),
            ab(b"actual owned bytes"),
            true.into_ipc(),
            s(&token),
        ],
        destination.context.clone(),
        shell_contract::runtime::CancelToken::default(),
    );
    let saved = await_owned_reply(&mut t, &pending).unwrap();
    assert_eq!(get(&saved, "ok"), &IpcValue::Bool(true));
    assert_eq!(std::fs::read(&target).unwrap(), b"actual owned bytes");
    assert_eq!(std::fs::read(original).unwrap(), b"original bytes");
    assert!(answers.borrow().is_none());
    acknowledge_owned_save(&mut t, "another-request", &destination, true);
    assert!(answers.borrow().is_none());
    acknowledge_owned_save(&mut t, &token, &destination, true);
    assert!(answers.borrow().as_ref().unwrap().is_ok());
}

#[test]
fn renderer_success_without_native_owned_write_is_rejected() {
    let (mut t, _) = background_document();
    let target = t.rt.dir.join("never-written.docx");
    let (token, destination, _, answers) = owned_save_request(&mut t, target.clone());
    acknowledge_owned_save(&mut t, &token, &destination, true);
    assert!(answers.borrow().as_ref().unwrap().is_err());
    assert!(!target.exists());
}

#[test]
fn canceled_owned_save_cannot_publish_from_a_late_renderer() {
    let (mut t, _) = background_document();
    let target = t.rt.dir.join("canceled.docx");
    let (token, destination, cancel, answers) = owned_save_request(&mut t, target.clone());
    cancel.cancel();
    t.rt.drain();
    assert!(answers.borrow().as_ref().unwrap().is_err());
    let pending = t.rt.invoke_native(
        "docs:save-to",
        t.a,
        vec![
            s(&target.to_string_lossy()),
            ab(b"must not land"),
            true.into_ipc(),
            s(&token),
        ],
        destination.context.clone(),
        shell_contract::runtime::CancelToken::default(),
    );
    assert!(await_owned_reply(&mut t, &pending).is_err());
    assert!(!target.exists());
}

#[test]
fn final_owned_save_commit_rechecks_the_current_write_grant() {
    use main_core::packages::electron_utils::atomic_write::owned::{self, Admission};
    let (mut t, path) = background_document();
    let view = t.a.view;
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let generation = t.rt.services.views.renderer_generation(view).unwrap();
    let owner = effects.capture_owner(view, &generation).unwrap();
    let docs = t.editor().0.clone();
    let scope = super::owned_save::capture_close(
        &docs,
        view,
        None,
        owner,
        shell_contract::runtime::CancelToken::default(),
    )
    .unwrap();
    let admit = super::owned_write::admission(
        &docs,
        scope,
        &path,
        super::owned_write::Permission::ExistingDocument,
    );
    let (ready, waiting) = std::sync::mpsc::sync_channel(1);
    let once = std::sync::atomic::AtomicBool::new(false);
    let admission: Admission = Arc::new(move |effect| {
        if !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
            ready.send(()).unwrap();
        }
        admit(effect)
    });
    let target = path.clone();
    let (send, result) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        send.send(owned::write_file(&target, b"must not replace", admission))
            .unwrap();
    });
    waiting.recv_timeout(Duration::from_secs(10)).unwrap();
    docs.doc_writable.remove(view);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let result = loop {
        t.rt.drain();
        if let Ok(result) = result.try_recv() {
            break result;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "final grant rejection did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    worker.join().unwrap();
    assert!(
        result
            .unwrap_err()
            .message
            .contains("not an opened document")
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"original bytes");
    assert!(!t.rt.dir.join("owned-original.docx.unsaved.tmp").exists());
}

#[test]
fn revoked_actual_media_permit_rejects_a_late_owned_save() {
    let (mut t, original) = background_document();
    let target = t.rt.dir.join("revoked-document.docx");
    let (token, destination, _, answers) = owned_save_request(&mut t, target.clone());
    t.editor().media_permit(t.a.view).unwrap().revoke();
    t.rt.drain();
    assert!(answers.borrow().as_ref().unwrap().is_err());
    let pending = t.rt.invoke_native(
        "docs:save-to",
        t.a,
        vec![
            s(&target.to_string_lossy()),
            ab(b"must not land"),
            true.into_ipc(),
            s(&token),
        ],
        destination.context,
        shell_contract::runtime::CancelToken::default(),
    );
    assert!(await_owned_reply(&mut t, &pending).is_err());
    assert!(!target.exists());
    assert_eq!(std::fs::read(original).unwrap(), b"original bytes");
}

fn captured_docs_owner(t: &T) -> shell_contract::ipc::NativeRequestOwner {
    let generation = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    t.rt.services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap()
        .capture_owner(t.a.view, &generation)
        .unwrap()
}

fn disposal_admission(
    t: &T,
    owner: shell_contract::ipc::NativeRequestOwner,
    cancel: shell_contract::runtime::CancelToken,
) -> super::editors::DisposalAdmission {
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let browser = t
        .editor()
        .0
        .media_views
        .borrow()
        .get(&t.a.view)
        .unwrap()
        .owner
        .clone();
    Box::new(move |effect| {
        effects.with_owned_effect(&owner, &owner, &cancel, &cancel, &mut || {
            browser
                .reservation()
                .with_owner(&browser, |transition| effect(transition))
        })
    })
}

fn await_disposal(
    t: &mut T,
    answer: &Rc<RefCell<Option<super::editors::DocsDisposalOutcome>>>,
) -> super::editors::DocsDisposalOutcome {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        t.rt.drain();
        if let Some(answer) = answer.borrow_mut().take() {
            return answer;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "captured Docs disposal did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The shell's actor retirement of the view's current document, which a
/// destroyed view with a live media lease waits for.
fn retire_actor(t: &mut T) {
    let owner = captured_docs_owner(t);
    let disposal = t.editor().capture_disposal(t.a.view, &owner).unwrap();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    disposal.run(
        disposal_admission(t, owner, shell_contract::runtime::CancelToken::default()),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    assert!(matches!(
        await_disposal(t, &answer),
        super::editors::DocsDisposalOutcome::Retired { cleanup: Ok(()) }
    ));
}

#[test]
fn captured_disposal_retires_only_the_original_actor_and_acknowledges_cleanup() {
    let (mut t, path) = background_document();
    let editor = t.editor();
    let permit = editor.media_permit(t.a.view).unwrap();
    let owner = captured_docs_owner(&t);
    let disposal = editor.capture_disposal(t.a.view, &owner).unwrap();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    disposal.run(
        disposal_admission(&t, owner, shell_contract::runtime::CancelToken::default()),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    assert!(!permit.is_active());
    assert!(editor.torn_down(t.a.view));
    assert!(matches!(
        await_disposal(&mut t, &answer),
        super::editors::DocsDisposalOutcome::Retired { cleanup: Ok(()) }
    ));
    assert_eq!(std::fs::read(path).unwrap(), b"original bytes");
    assert!(!editor.0.can_doc_write(
        t.a.view,
        &editor.document_path(t.a.view).unwrap().to_string_lossy()
    ));
}

#[test]
fn captured_cleanup_retires_exact_actor_after_request_owner_is_lost() {
    let (mut t, _) = background_document();
    let editor = t.editor();
    let permit = editor.media_permit(t.a.view).unwrap();
    let owner = captured_docs_owner(&t);
    let actor = editor.capture_disposal(t.a.view, &owner).unwrap();
    let browser = editor
        .0
        .media_views
        .borrow()
        .get(&t.a.view)
        .unwrap()
        .owner
        .clone();
    owner.loss_token().unwrap().cancel();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    actor.run_cleanup(
        Box::new(move |effect| {
            browser
                .reservation()
                .with_owner(&browser, |transition| effect(transition))
        }),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    assert!(!permit.is_active());
    assert!(matches!(
        await_disposal(&mut t, &answer),
        super::editors::DocsDisposalOutcome::Retired { cleanup: Ok(()) }
    ));
}

#[test]
fn terminal_browser_receipt_retires_only_captured_actor() {
    let (mut t, _) = background_document();
    let editor = t.editor();
    let generation = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let (issuer, reservation, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(t.a.view, generation.clone());
    reservation
        .with_initial_owner(&browser, |_| {
            editor
                .0
                .media_views
                .borrow_mut()
                .get_mut(&t.a.view)
                .unwrap()
                .owner = browser.clone();
            Ok(())
        })
        .unwrap();
    let permit = editor.media_permit(t.a.view).unwrap();
    let owner = captured_docs_owner(&t);
    let actor = editor.capture_disposal(t.a.view, &owner).unwrap();
    let (foreign, _, _) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(t.a.view, generation);
    let rejected = Rc::new(RefCell::new(None));
    let reply = rejected.clone();
    actor.run_closed(
        foreign.closed(),
        Box::new(move |result| *reply.borrow_mut() = Some(result)),
    );
    let actor = match rejected.borrow_mut().take().unwrap() {
        super::editors::DocsDisposalOutcome::NotRetired { actor, .. } => *actor,
        _ => panic!("foreign browser retired the Docs actor"),
    };
    assert!(permit.is_active());
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    owner.loss_token().unwrap().cancel();
    actor.run_closed(
        issuer.closed(),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    assert!(matches!(
        await_disposal(&mut t, &answer),
        super::editors::DocsDisposalOutcome::Retired { cleanup: Ok(()) }
    ));
    assert!(!permit.is_active());
}

#[test]
fn captured_disposal_cannot_retire_a_document_opened_after_capture() {
    let mut t = T::new(json!({}));
    let original = t.file("before-disposal.docx", b"before");
    t.open(&original);
    let owner = captured_docs_owner(&t);
    let disposal = t.editor().capture_disposal(t.a.view, &owner).unwrap();
    let replacement = t.file("after-disposal.docx", b"after");
    t.open(&replacement);
    let current = t.editor().media_permit(t.a.view).unwrap();
    let invoked = Rc::new(std::cell::Cell::new(false));
    let attempt = invoked.clone();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    disposal.run(
        Box::new(move |_| {
            attempt.set(true);
            Ok(())
        }),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    assert!(matches!(
        answer.borrow().as_ref().unwrap(),
        super::editors::DocsDisposalOutcome::NotRetired { .. }
    ));
    assert!(!invoked.get());
    assert!(current.is_active());
    assert!(!t.editor().torn_down(t.a.view));
    assert_eq!(
        get(
            &t.call("docs:save", vec![s(&replacement), ab(b"still writable")]),
            "ok"
        ),
        &IpcValue::Bool(true)
    );
    assert_eq!(std::fs::read(replacement).unwrap(), b"still writable");
}

#[test]
fn disposal_rechecks_the_exact_actor_inside_admission() {
    let t = T::new(json!({}));
    let editor = t.editor();
    let view = t.a.view;
    let owner = captured_docs_owner(&t);
    let disposal = editor.capture_disposal(view, &owner).unwrap();
    let successor = Rc::new(RefCell::new(None));
    let created = successor.clone();
    let replace = editor.clone();
    let browser = editor
        .0
        .media_views
        .borrow()
        .get(&view)
        .unwrap()
        .owner
        .clone();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    disposal.run(
        Box::new(move |effect| {
            *created.borrow_mut() = Some(replace.register_view(view)?);
            browser
                .reservation()
                .with_owner(&browser, |transition| effect(transition))
        }),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    assert!(matches!(
        answer.borrow().as_ref().unwrap(),
        super::editors::DocsDisposalOutcome::NotRetired { .. }
    ));
    assert!(successor.borrow().as_ref().unwrap().current().is_some());
    assert!(!editor.torn_down(view));
}

#[test]
fn canceled_disposal_admission_does_not_retire_the_captured_actor() {
    let (mut t, _) = background_document();
    let owner = captured_docs_owner(&t);
    let editor = t.editor();
    let permit = editor.media_permit(t.a.view).unwrap();
    let disposal = editor.capture_disposal(t.a.view, &owner).unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    cancel.cancel();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    disposal.run(
        disposal_admission(&t, owner.clone(), cancel),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    let actor = match await_disposal(&mut t, &answer) {
        super::editors::DocsDisposalOutcome::NotRetired { actor, .. } => *actor,
        _ => panic!("canceled admission retired the actor"),
    };
    assert!(permit.is_active());
    assert!(!editor.torn_down(t.a.view));
    let completed = answer.clone();
    actor.run(
        disposal_admission(&t, owner, shell_contract::runtime::CancelToken::default()),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    assert!(matches!(
        await_disposal(&mut t, &answer),
        super::editors::DocsDisposalOutcome::Retired { cleanup: Ok(()) }
    ));
}

#[test]
fn discarded_disposal_worker_reports_failure_after_actor_retirement() {
    let mut t = T::with_workers(json!({}), Some(Arc::new(DiscardWorkers)));
    let owner = captured_docs_owner(&t);
    let editor = t.editor();
    let actor = editor.capture_disposal(t.a.view, &owner).unwrap();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    actor.run(
        disposal_admission(&t, owner, shell_contract::runtime::CancelToken::default()),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    assert!(editor.torn_down(t.a.view));
    match await_disposal(&mut t, &answer) {
        super::editors::DocsDisposalOutcome::Retired {
            cleanup: Err(error),
        } => assert!(error.contains("discarded")),
        _ => panic!("discarded cleanup did not report a retired failure"),
    }
}

#[test]
fn disposal_reports_actual_recovery_errors_after_terminal_actor_retirement() {
    let (mut t, _) = background_document();
    let editor = t.editor();
    let docs = editor.0.clone();
    let failed = docs.with_store(|store| {
        docs.book.borrow_mut().begin_copy(
            store,
            t.a.view,
            &super::recovery::CopyTarget::Untitled("../outside"),
            &|| "00000000-0000-4000-8000-000000000999".into(),
        )
    });
    assert!(failed.is_err());
    docs.book.borrow_mut().note_closed_saved(t.a.view);
    let owner = captured_docs_owner(&t);
    let disposal = editor.capture_disposal(t.a.view, &owner).unwrap();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    disposal.run(
        disposal_admission(&t, owner, shell_contract::runtime::CancelToken::default()),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    match await_disposal(&mut t, &answer) {
        super::editors::DocsDisposalOutcome::Retired {
            cleanup: Err(error),
        } => assert!(error.contains("invalid recovery copy extension")),
        _ => panic!("the recovery cleanup failure was not reported"),
    }
    assert!(editor.torn_down(t.a.view));
    assert!(editor.recovery_identity(t.a.view).is_none());
}

#[test]
fn owned_rename_retains_all_participants_at_the_actual_state_and_notification_effects() {
    let mut t = T::new(json!({}));
    let old = t.file("rename-owned.docx", b"document");
    t.open(&old);
    let destination = captured_docs_owner(&t);
    let extra = t.rt.view(22, Surface::Shell);
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let generation = t.rt.services.views.renderer_generation(extra.view).unwrap();
    let participant = effects.capture_owner(extra.view, &generation).unwrap();
    let new = t.rt.dir.join("renamed-owned.docx");
    std::fs::rename(&old, &new).unwrap();
    t.rt.events();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    t.editor().file_renamed_many_owned_extra(
        t.a.view,
        Path::new(&old),
        &new,
        &destination,
        &destination,
        &[participant],
        &shell_contract::runtime::CancelToken::default(),
        &shell_contract::runtime::CancelToken::default(),
        &[],
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    t.rt.drain();
    assert!(answer.borrow().as_ref().unwrap().is_ok());
    assert_eq!(t.editor().document_path(t.a.view).as_ref(), Some(&new));
    assert!(t.rt.events().iter().any(|event| {
        event.channel == "docs:renamed"
            && event
                .args
                .first()
                .is_some_and(|value| get(value, "newPath") == &s(&new.to_string_lossy()))
    }));
    assert!(t.editor().media_permit(t.a.view).unwrap().is_active());
}

#[test]
fn revoked_extra_participant_blocks_owned_rename_state_and_notification() {
    let mut t = T::new(json!({}));
    let old = t.file("rename-blocked.docx", b"document");
    t.open(&old);
    let destination = captured_docs_owner(&t);
    let extra = t.rt.view(22, Surface::Shell);
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let generation = t.rt.services.views.renderer_generation(extra.view).unwrap();
    let participant = effects.capture_owner(extra.view, &generation).unwrap();
    generation.revoke();
    let new = t.rt.dir.join("must-not-be-acknowledged.docx");
    t.rt.events();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    t.editor().file_renamed_many_owned_extra(
        t.a.view,
        Path::new(&old),
        &new,
        &destination,
        &destination,
        &[participant],
        &shell_contract::runtime::CancelToken::default(),
        &shell_contract::runtime::CancelToken::default(),
        &[],
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    t.rt.drain();
    assert!(answer.borrow().as_ref().unwrap().is_err());
    assert_eq!(
        t.editor().document_path(t.a.view),
        Some(PathBuf::from(&old))
    );
    assert!(
        !t.rt
            .events()
            .iter()
            .any(|event| event.channel == "docs:renamed")
    );
    assert_eq!(std::fs::read(old).unwrap(), b"document");
}

#[test]
fn cancelled_callback_blocks_owned_rename_state_and_notification() {
    let mut t = T::new(json!({}));
    let old = t.file("rename-cancelled.docx", b"document");
    t.open(&old);
    let destination = captured_docs_owner(&t);
    let extra = shell_contract::runtime::CancelToken::default();
    extra.cancel();
    let new = t.rt.dir.join("must-not-rename.docx");
    t.rt.events();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    t.editor().file_renamed_many_owned_extra(
        t.a.view,
        Path::new(&old),
        &new,
        &destination,
        &destination,
        &[],
        &shell_contract::runtime::CancelToken::default(),
        &shell_contract::runtime::CancelToken::default(),
        &[extra],
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    t.rt.drain();
    assert!(answer.borrow().as_ref().unwrap().is_err());
    assert_eq!(
        t.editor().document_path(t.a.view),
        Some(PathBuf::from(&old))
    );
    assert!(
        !t.rt
            .events()
            .iter()
            .any(|event| event.channel == "docs:renamed")
    );
}

#[test]
fn native_save_rename_rejects_cancelled_callback_without_changing_document() {
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeSaveDestination, NativeSaveRevisionAuthority,
        NativeSaveSelection,
    };
    let mut t = T::new(json!({}));
    let old = t.file("native-rename-cancelled.docx", b"document");
    t.open(&old);
    let destination = captured_docs_owner(&t);
    let mut source = destination.clone();
    source.context = source
        .context
        .with_document_lease(shell_contract::runtime::RendererGeneration::new());
    let binding = NativeInvocationIssuer::new(shell_contract::runtime::CancelToken::default())
        .issue_existing_document_save(
            Surface::Docs,
            source.view,
            NativeSaveSelection::ExactPath(PathBuf::from(&old)),
            source,
            shell_contract::runtime::CancelToken::default(),
        )
        .unwrap();
    let authority = NativeSaveRevisionAuthority::new(
        NativeSaveDestination::from_exact_selection(binding).unwrap(),
        Vec::new(),
    );
    let extra = shell_contract::runtime::CancelToken::default();
    extra.cancel();
    let new = t.rt.dir.join("native-rename-must-not-publish.docx");
    t.rt.events();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    t.editor().file_renamed_existing_document(
        t.a.view,
        Path::new(&old),
        &new,
        &authority,
        Path::new(&old),
        &destination,
        &[],
        &shell_contract::runtime::CancelToken::default(),
        &shell_contract::runtime::CancelToken::default(),
        &[extra],
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    t.rt.drain();
    assert!(answer.borrow().as_ref().unwrap().is_err());
    assert_eq!(
        t.editor().document_path(t.a.view),
        Some(PathBuf::from(&old))
    );
    assert!(
        !t.rt
            .events()
            .iter()
            .any(|event| event.channel == "docs:renamed")
    );
}

#[test]
fn recovery_rename_rejects_revoked_captured_participant_without_changing_document() {
    let mut t = T::new(json!({}));
    let old = t.file("recovery-rename.docx", b"document");
    t.open(&old);
    let extra = t.rt.view(22, Surface::Shell);
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let extra_generation = t.rt.services.views.renderer_generation(extra.view).unwrap();
    let participant = effects
        .capture_owner(extra.view, &extra_generation)
        .unwrap();
    let created = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let source = captured_docs_owner(&t);
    let cancel = shell_contract::runtime::CancelToken::default();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(t.a.view, created.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: source.clone(),
            participants: Vec::new(),
            cancel: cancel.clone(),
            pending: cancel.clone(),
        },
        t.a.view,
        created.clone(),
        closure,
        browser_owner,
    )
    .unwrap();
    let receipt = effects
        .commit_recovery_seed(&target, &mut || Ok(()))
        .unwrap();
    let synthetic =
        t.rt.services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .unwrap()
            .clone();
    let provenance = shell_contract::hosts::synthetic::SyntheticNativeOrigin::new();
    let root = synthetic
        .capture_invocation_root(t.a.view, &created, &provenance)
        .unwrap();
    target.retain_native_registration(root).unwrap();
    let receiver = synthetic
        .capture_invocation_docs(t.a.view, &created, &provenance)
        .unwrap();
    let destination = receiver.clone();
    extra_generation.revoke();
    let new = t.rt.dir.join("recovery-rename-next.docx");
    t.rt.events();
    let result = Rc::new(RefCell::new(None));
    let output = result.clone();
    t.editor().file_renamed_recovery(
        t.a.view,
        Path::new(&old),
        &new,
        &receipt,
        &receiver,
        &destination,
        &[participant],
        &cancel,
        &cancel,
        Box::new(move |value| *output.borrow_mut() = Some(value)),
    );
    assert!(result.borrow().as_ref().unwrap().is_err());
    assert_eq!(
        t.editor().document_path(t.a.view),
        Some(PathBuf::from(&old))
    );
    assert!(
        !t.rt
            .events()
            .iter()
            .any(|event| event.channel == "docs:renamed")
    );
}

#[test]
fn recovery_rename_updates_the_captured_document_and_notifies_it() {
    let mut t = T::new(json!({}));
    let old = t.file("recovery-rename-ok.docx", b"document");
    t.open(&old);
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let created = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let source = captured_docs_owner(&t);
    let cancel = shell_contract::runtime::CancelToken::default();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(t.a.view, created.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source,
            participants: Vec::new(),
            cancel: cancel.clone(),
            pending: cancel.clone(),
        },
        t.a.view,
        created.clone(),
        closure,
        browser_owner,
    )
    .unwrap();
    let receipt = effects
        .commit_recovery_seed(&target, &mut || Ok(()))
        .unwrap();
    let synthetic =
        t.rt.services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .unwrap()
            .clone();
    let provenance = shell_contract::hosts::synthetic::SyntheticNativeOrigin::new();
    target
        .retain_native_registration(
            synthetic
                .capture_invocation_root(t.a.view, &created, &provenance)
                .unwrap(),
        )
        .unwrap();
    let receiver = synthetic
        .capture_invocation_docs(t.a.view, &created, &provenance)
        .unwrap();
    let new = t.rt.dir.join("recovery-renamed.docx");
    std::fs::rename(&old, &new).unwrap();
    t.rt.events();
    let result = Rc::new(RefCell::new(None));
    let output = result.clone();
    t.editor().file_renamed_recovery(
        t.a.view,
        Path::new(&old),
        &new,
        &receipt,
        &receiver,
        &receiver,
        &[],
        &cancel,
        &cancel,
        Box::new(move |value| *output.borrow_mut() = Some(value)),
    );
    assert!(result.borrow().as_ref().unwrap().is_ok());
    assert_eq!(t.editor().document_path(t.a.view).as_ref(), Some(&new));
    assert!(
        t.rt.events()
            .iter()
            .any(|event| event.channel == "docs:renamed")
    );
}

#[test]
fn headless_abort_drains_the_existing_target_input_and_timer_once() {
    let mut t = T::new(json!({ "is_headless": true }));
    let path = t.file("headless-input.docx", b"document");
    let out =
        t.rt.dir
            .join("headless-output.html")
            .to_string_lossy()
            .into_owned();
    let results = Rc::new(RefCell::new(Vec::new()));
    let completed = results.clone();
    let editor = t.editor();
    editor.begin_headless_export(
        t.a.view,
        path,
        out.clone(),
        "html",
        Duration::from_secs(1),
        Box::new(move |result| completed.borrow_mut().push(result)),
    );
    assert!(editor.fail_headless_export(t.a.view, "native navigation failed".into()));
    assert!(!editor.fail_headless_export(t.a.view, "another failure".into()));
    assert!(
        t.rt.invoke_now("docs:consume-headless-export", t.a, vec![])
            .is_err()
    );
    assert!(
        t.rt.invoke_now("docs:consume-pending-open", t.a, vec![])
            .is_err()
    );
    t.rt.advance(Duration::from_secs(2));
    t.rt.send(
        "docs:headless-export-done",
        t.a,
        vec![obj(&[("ok", true.into_ipc())])],
    );
    assert_eq!(
        *results.borrow(),
        vec![Err("native navigation failed".to_owned())]
    );
    assert!(!Path::new(&out).exists());
}

#[test]
fn canceled_native_headless_reservation_cannot_seed_or_authorize_an_export() {
    use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    let mut t = T::new(json!({ "is_headless": true }));
    let path = t.file("native-headless-input.docx", b"document");
    let out = t.rt.dir.join("native-headless-output.pdf");
    let cancel = shell_contract::runtime::CancelToken::default();
    let issuer = NativeInvocationIssuer::new(shell_contract::runtime::CancelToken::default());
    let operation = issuer.issue(
        NativeInvocationSelection::HeadlessExport {
            surface: Surface::Docs,
            input: path.clone().into(),
            output: out.clone(),
            format: "pdf".into(),
            page: None,
            ops: None,
            batch: None,
        },
        cancel.clone(),
    );
    let generation = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let target = operation.reserve_target(t.a.view, generation).unwrap();
    cancel.cancel();
    let results = Rc::new(RefCell::new(Vec::new()));
    let completed = results.clone();
    t.editor().begin_headless_export_native(
        t.a.view,
        path,
        out.to_string_lossy().into_owned(),
        "pdf",
        Duration::from_secs(1),
        &target,
        Box::new(move |result| completed.borrow_mut().push(result)),
    );
    assert!(results.borrow().first().unwrap().is_err());
    assert!(
        t.rt.invoke_now("docs:consume-headless-export", t.a, vec![])
            .is_err()
    );
    assert!(
        t.rt.invoke_now("docs:consume-pending-open", t.a, vec![])
            .is_err()
    );
    assert!(!out.exists());
    t.rt.advance(Duration::from_secs(2));
    assert_eq!(results.borrow().len(), 1);
}

#[test]
fn native_headless_reservation_cannot_authorize_another_output() {
    use shell_contract::ipc::{NativeInvocationIssuer, NativeInvocationSelection};
    let mut t = T::new(json!({ "is_headless": true }));
    let path = t.file("selected-headless.docx", b"document");
    let selected = t.rt.dir.join("selected.pdf");
    let other = t.rt.dir.join("other.pdf");
    let issuer = NativeInvocationIssuer::new(shell_contract::runtime::CancelToken::default());
    let operation = issuer.issue(
        NativeInvocationSelection::HeadlessExport {
            surface: Surface::Docs,
            input: path.clone().into(),
            output: selected,
            format: "pdf".into(),
            page: None,
            ops: None,
            batch: None,
        },
        shell_contract::runtime::CancelToken::default(),
    );
    let generation = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let target = operation.reserve_target(t.a.view, generation).unwrap();
    let results = Rc::new(RefCell::new(Vec::new()));
    let completed = results.clone();
    t.editor().begin_headless_export_native(
        t.a.view,
        path,
        other.to_string_lossy().into_owned(),
        "pdf",
        Duration::from_secs(1),
        &target,
        Box::new(move |result| completed.borrow_mut().push(result)),
    );
    assert!(results.borrow().first().unwrap().is_err());
    assert!(
        t.rt.invoke_now("docs:consume-headless-export", t.a, vec![])
            .is_err()
    );
    assert!(
        t.rt.invoke_now("docs:consume-pending-open", t.a, vec![])
            .is_err()
    );
    assert!(!other.exists());
}

#[test]
fn captured_recovery_waits_for_real_copy_preparation_before_navigation() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenUntitledRequest,
    };
    use main_core::packages::electron_utils::recovery_store::RecoveryIdentity;
    let mut t = T::with_workers(json!({}), Some(Arc::new(BackgroundWorkers)));
    let metadata = obj(&[("title", s("Recovered notes")), ("format", s("docx"))]);
    let write = t.rt.invoke(
        "docs:write-recovery",
        t.a,
        vec![IpcValue::Null, ab(b"saved recovery"), metadata],
    );
    assert_eq!(
        get(&await_owned_reply(&mut t, &write).unwrap(), "ok"),
        &IpcValue::Bool(true)
    );
    let identity = t.editor().recovery_identity(t.a.view).unwrap();
    t.editor().teardown(t.a.view, true);
    let restored_view = t.rt.view(2, Surface::Docs);
    t.editor().register_view(restored_view.view).unwrap();
    let source = t.rt.view(22, Surface::Shell);
    let source_generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let origin =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .capture_owner(source.view, &source_generation)
            .unwrap();
    let created =
        t.rt.services
            .views
            .renderer_generation(restored_view.view)
            .unwrap();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(
            restored_view.view,
            created.clone(),
        );
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: origin,
            participants: Vec::new(),
            cancel: shell_contract::runtime::CancelToken::default(),
            pending: shell_contract::runtime::CancelToken::default(),
        },
        restored_view.view,
        created.clone(),
        closure,
        browser_owner,
    )
    .unwrap();
    let editor =
        t.rt.provided()
            .all::<dyn RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget>>()
            .into_iter()
            .find(|editor| editor.kind() == "docs")
            .unwrap();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    editor.open_untitled_copy(
        &restored_view.view,
        &RecoveryOpenUntitledRequest {
            copy: RecoveryIdentity::Untitled(identity.clone()),
            title: String::new(),
            save_as_default_path: None,
        },
        &target,
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while answer.borrow().is_none() {
        t.rt.drain();
        assert!(
            std::time::Instant::now() < deadline,
            "captured recovery did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(answer.borrow().as_ref().unwrap().is_ok());
    let synthetic =
        t.rt.services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .unwrap()
            .clone();
    let provenance = shell_contract::hosts::synthetic::SyntheticNativeOrigin::new();
    let root = synthetic
        .capture_invocation_root(restored_view.view, &created, &provenance)
        .unwrap();
    target.retain_native_registration(root).unwrap();
    let receiver = synthetic
        .capture_invocation_docs(restored_view.view, &created, &provenance)
        .unwrap();
    let restored = t.rt.invoke_native(
        "docs:consume-copy",
        restored_view,
        vec![],
        receiver.context,
        shell_contract::runtime::CancelToken::default(),
    );
    let restored = await_owned_reply(&mut t, &restored).unwrap();
    assert_eq!(get(&restored, "name"), &s("Recovered notes.docx"));
    assert_eq!(
        get(&restored, "dataUrl"),
        &s("handoff:7361766564207265636f76657279")
    );
    let receipt = target.committed_seed().unwrap();
    assert!(receipt.settlement().is_none());
    let token = get(&restored, "recoveryReceipt").clone();
    let receiver = synthetic
        .capture_invocation_docs(restored_view.view, &created, &provenance)
        .unwrap();
    let ack = t.rt.invoke_native(
        "docs:recovery-initialized",
        restored_view,
        vec![token, IpcValue::Bool(true)],
        receiver.context,
        shell_contract::runtime::CancelToken::default(),
    );
    assert!(ack.take().unwrap().is_ok());
    assert!(receipt.settlement().unwrap().is_ok());
    assert_eq!(
        t.editor().recovery_identity(restored_view.view),
        Some(identity)
    );
}

#[test]
fn canceled_captured_recovery_does_not_seed_the_target() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenFileRequest,
    };
    let mut t = T::with_workers(json!({}), Some(Arc::new(BackgroundWorkers)));
    let path = t.file("canceled-recovery.docx", b"unchanged");
    let source = t.rt.view(22, Surface::Shell);
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let origin =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .capture_owner(source.view, &generation)
            .unwrap();
    let cancel = shell_contract::runtime::CancelToken::default();
    let created = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(t.a.view, created.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: origin,
            participants: Vec::new(),
            cancel: cancel.clone(),
            pending: shell_contract::runtime::CancelToken::default(),
        },
        t.a.view,
        created,
        closure,
        browser_owner,
    )
    .unwrap();
    let contract =
        t.rt.provided()
            .all::<dyn RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget>>()
            .into_iter()
            .find(|editor| editor.kind() == "docs")
            .unwrap();
    cancel.cancel();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    contract.open_file_with_decision(
        &t.a.view,
        &RecoveryOpenFileRequest {
            file_path: path.clone(),
            decision: RecoveryOpenDecision::Original,
        },
        &target,
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while answer.borrow().is_none() {
        t.rt.drain();
        assert!(
            std::time::Instant::now() < deadline,
            "canceled recovery did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(answer.borrow().as_ref().unwrap().is_err());
    let pending = t.rt.invoke("docs:consume-pending-open", t.a, vec![]);
    assert_eq!(await_owned_reply(&mut t, &pending).unwrap(), IpcValue::Null);
    assert_eq!(std::fs::read(path).unwrap(), b"unchanged");
}

#[test]
fn discarded_recovery_worker_settles_the_core_callback_once() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenFileRequest,
    };
    let mut t = T::with_workers(json!({}), Some(Arc::new(DiscardWorkers)));
    let path = t.file("discarded-recovery.docx", b"unchanged");
    let source = t.rt.view(22, Surface::Shell);
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let origin =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .capture_owner(source.view, &generation)
            .unwrap();
    let created = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(t.a.view, created.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: origin,
            participants: Vec::new(),
            cancel: shell_contract::runtime::CancelToken::default(),
            pending: shell_contract::runtime::CancelToken::default(),
        },
        t.a.view,
        created,
        closure,
        browser_owner,
    )
    .unwrap();
    let contract =
        t.rt.provided()
            .all::<dyn RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget>>()
            .into_iter()
            .find(|editor| editor.kind() == "docs")
            .unwrap();
    let results = Rc::new(RefCell::new(Vec::new()));
    let completed = results.clone();
    contract.open_file_with_decision(
        &t.a.view,
        &RecoveryOpenFileRequest {
            file_path: path.clone(),
            decision: RecoveryOpenDecision::Original,
        },
        &target,
        Box::new(move |result| completed.borrow_mut().push(result)),
    );
    t.rt.drain();
    assert_eq!(results.borrow().len(), 1);
    assert!(results.borrow()[0].is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"unchanged");
}

/// A blank recovery tab whose renderer consumed its seed: the native receipt,
/// the authenticated renderer's native context and the receipt it must acknowledge
struct BlankRecovery {
    target: shell_contract::ipc::RecoveryOpenTarget,
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    context: shell_contract::ipc::NativeContext,
    token: IpcValue,
}

fn blank_recovery(t: &mut T) -> BlankRecovery {
    let source = t.rt.view(22, Surface::Shell);
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let origin = effects.capture_owner(source.view, &generation).unwrap();
    let created = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(t.a.view, created.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: origin,
            participants: Vec::new(),
            cancel: shell_contract::runtime::CancelToken::default(),
            pending: shell_contract::runtime::CancelToken::default(),
        },
        t.a.view,
        created.clone(),
        closure,
        browser_owner,
    )
    .unwrap();
    let result = Rc::new(RefCell::new(None));
    let completed = result.clone();
    t.editor().queue_blank_recovery(
        t.a.view,
        &target,
        "Original tab title".into(),
        Box::new(move |value| *completed.borrow_mut() = Some(value)),
    );
    assert!(result.borrow().as_ref().unwrap().is_ok());
    let receipt = target.committed_seed().unwrap();
    assert!(receipt.settlement().is_none());
    let synthetic =
        t.rt.services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .unwrap()
            .clone();
    let provenance = shell_contract::hosts::synthetic::SyntheticNativeOrigin::new();
    target
        .retain_native_registration(
            synthetic
                .capture_invocation_root(t.a.view, &created, &provenance)
                .unwrap(),
        )
        .unwrap();
    let wrong = shell_contract::ipc::NativeContext::with_renderer(
        (),
        shell_contract::runtime::RendererGeneration::new(),
    );
    let refused =
        t.rt.invoke_native(
            "docs:consume-new-blank",
            t.a,
            vec![],
            wrong,
            shell_contract::runtime::CancelToken::default(),
        )
        .take()
        .unwrap();
    assert!(refused.is_err());
    assert!(receipt.settlement().is_none());
    let context = synthetic
        .capture_invocation_docs(t.a.view, &created, &provenance)
        .unwrap()
        .context;
    let blank =
        t.rt.invoke_native(
            "docs:consume-new-blank",
            t.a,
            vec![],
            context.clone(),
            shell_contract::runtime::CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap();
    assert!(receipt.settlement().is_none());
    let token = get(&blank, "recoveryReceipt").clone();
    BlankRecovery {
        target,
        receipt,
        context,
        token,
    }
}

/// `docs:recovery-initialized` as the registered renderer's native call
fn acknowledge_recovery(
    t: &mut T,
    args: Vec<IpcValue>,
    context: shell_contract::ipc::NativeContext,
) -> shell_contract::ipc::Reply {
    t.rt.invoke_native(
        "docs:recovery-initialized",
        t.a,
        args,
        context,
        shell_contract::runtime::CancelToken::default(),
    )
    .take()
    .unwrap()
}

#[test]
fn native_evidence_docs_recovery_initialized_failed_ack_settles_failure_once() {
    let mut t = T::new(json!({}));
    let seed = blank_recovery(&mut t);
    let drained = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let notified = drained.clone();
    seed.target
        .drain_work(
            t.rt.services.core.clone(),
            Box::new(move || notified.store(true, std::sync::atomic::Ordering::SeqCst)),
        )
        .unwrap();
    t.rt.drain();
    assert!(!drained.load(std::sync::atomic::Ordering::SeqCst));
    let failed = acknowledge_recovery(
        &mut t,
        vec![seed.token.clone(), IpcValue::Bool(false), s("blank failed")],
        seed.context.clone(),
    );
    assert_eq!(failed.unwrap(), IpcValue::Undefined);
    t.rt.drain();
    assert!(drained.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(seed.receipt.settlement(), Some(Err("blank failed".into())));
    let duplicate = acknowledge_recovery(
        &mut t,
        vec![seed.token.clone(), IpcValue::Bool(true)],
        seed.context.clone(),
    )
    .unwrap_err();
    assert!(duplicate.message.contains("no longer pending"));
    assert_eq!(seed.receipt.settlement(), Some(Err("blank failed".into())));
    shell_contract::native_evidence::record(
        "docs:recovery-initialized",
        &["failed-apply", "duplicate-ack"],
        &json!({
            "failed-apply": {
                "settlement": "Err(blank failed)",
                "work-drained-after-ack": true,
            },
            "duplicate-ack": {
                "error": duplicate.message,
                "settlement-unchanged": "Err(blank failed)",
            },
        })
        .to_string(),
    );
}

#[test]
fn native_evidence_docs_recovery_initialized_settles_only_the_exact_authenticated_ack() {
    let mut t = T::new(json!({}));
    let seed = blank_recovery(&mut t);
    let missing = acknowledge_recovery(&mut t, vec![], seed.context.clone()).unwrap_err();
    assert!(missing.message.contains("receipt is missing"));
    let valueless = acknowledge_recovery(
        &mut t,
        vec![IpcValue::Bool(true), IpcValue::Bool(true)],
        seed.context.clone(),
    )
    .unwrap_err();
    assert!(valueless.message.contains("receipt is missing"));
    assert!(seed.receipt.settlement().is_none());
    let foreign = acknowledge_recovery(
        &mut t,
        vec![s("foreign-recovery-receipt"), IpcValue::Bool(true)],
        seed.context.clone(),
    )
    .unwrap_err();
    assert!(foreign.message.contains("no longer pending"));
    assert!(seed.receipt.settlement().is_none());
    let other_renderer = shell_contract::ipc::NativeContext::with_renderer(
        (),
        shell_contract::runtime::RendererGeneration::new(),
    );
    let wrong_renderer = acknowledge_recovery(
        &mut t,
        vec![seed.token.clone(), IpcValue::Bool(true)],
        other_renderer,
    )
    .unwrap_err();
    assert!(seed.receipt.settlement().is_none());
    let pdf_view = t.rt.view(2, Surface::Pdf);
    let mut table = shell_core::table::Table::default();
    let provided = shell_contract::ipc::Registry::provided(&table);
    for entry in super::entries(&t.rt.services, &provided).unwrap() {
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
    let (send, receive) = std::sync::mpsc::channel();
    dispatcher.dispatch(
        pdf_view,
        shell_contract::ipc::ChannelKind::Invoke,
        Some(shell_contract::RequestId(1)),
        None,
        "docs:recovery-initialized",
        vec![seed.token.clone(), IpcValue::Bool(true)],
        Box::new(move |result| send.send(result).unwrap()),
    );
    let unauthorized = receive.try_recv().unwrap().unwrap_err();
    assert!(unauthorized.message.contains("not open to Pdf"));
    assert!(seed.receipt.settlement().is_none());
    let acknowledged = acknowledge_recovery(
        &mut t,
        vec![seed.token.clone(), IpcValue::Bool(true)],
        seed.context.clone(),
    );
    assert_eq!(acknowledged.unwrap(), IpcValue::Undefined);
    assert_eq!(seed.receipt.settlement(), Some(Ok(())));
    shell_contract::native_evidence::record(
        "docs:recovery-initialized",
        &[
            "successful-ack",
            "missing-receipt",
            "foreign-receipt",
            "foreign-renderer",
            "unauthorized-surface",
        ],
        &json!({
            "missing-receipt": {
                "no-arguments": missing.message,
                "non-string-receipt": valueless.message,
                "settled-by-either": false,
            },
            "foreign-receipt": {
                "error": foreign.message,
                "pending-settled": false,
            },
            "foreign-renderer": {
                "error": wrong_renderer.message,
                "pending-settled": false,
            },
            "unauthorized-surface": {
                "error": unauthorized.message,
                "pending-settled": false,
            },
            "successful-ack": {
                "answer": "undefined",
                "settlement": "Ok",
            },
        })
        .to_string(),
    );
}

#[test]
fn blank_recovery_rejects_a_different_native_selection() {
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeInvocationSelection, RecoveryOpenContext, RecoveryOpenTarget,
    };
    let t = T::new(json!({}));
    let issuer = NativeInvocationIssuer::new(shell_contract::runtime::CancelToken::default());
    let operation = issuer.issue(
        NativeInvocationSelection::RecoveryBlank {
            surface: Surface::Docs,
            title: "Selected".into(),
        },
        shell_contract::runtime::CancelToken::default(),
    );
    let created = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(t.a.view, created.clone());
    let target = RecoveryOpenTarget::bind(
        RecoveryOpenContext::NativeStartup(operation),
        t.a.view,
        created,
        closure,
        browser_owner,
    )
    .unwrap();
    let result = Rc::new(RefCell::new(None));
    let completed = result.clone();
    t.editor().queue_blank_recovery(
        t.a.view,
        &target,
        "Other".into(),
        Box::new(move |value| *completed.borrow_mut() = Some(value)),
    );
    assert!(result.borrow().as_ref().unwrap().is_err());
    assert!(target.committed_seed().is_none());
}

#[test]
fn recovery_file_uses_the_copy_pinned_before_navigation() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenFileRequest,
    };
    use main_core::packages::electron_utils::recovery_store::recovery_copy_path;
    let mut t = T::with_workers(json!({}), Some(Arc::new(BackgroundWorkers)));
    let path = t.file("pinned-recovery.docx", b"original");
    let opened = t.rt.invoke("docs:open-path", t.a, vec![s(&path)]);
    assert!(await_owned_reply(&mut t, &opened).is_ok());
    let write = t.rt.invoke(
        "docs:write-recovery",
        t.a,
        vec![s(&path), ab(b"captured draft")],
    );
    assert_eq!(
        get(&await_owned_reply(&mut t, &write).unwrap(), "ok"),
        &IpcValue::Bool(true)
    );
    let copy = recovery_copy_path(
        &t.editor().0.user_data(&[]),
        &super::recovery::file_identity(&path),
    )
    .unwrap();
    let restored = t.rt.view(2, Surface::Docs);
    t.editor().register_view(restored.view).unwrap();
    let source = t.rt.view(22, Surface::Shell);
    let source_generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let origin = effects
        .capture_owner(source.view, &source_generation)
        .unwrap();
    let created =
        t.rt.services
            .views
            .renderer_generation(restored.view)
            .unwrap();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(restored.view, created.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: origin,
            participants: Vec::new(),
            cancel: shell_contract::runtime::CancelToken::default(),
            pending: shell_contract::runtime::CancelToken::default(),
        },
        restored.view,
        created.clone(),
        closure,
        browser_owner,
    )
    .unwrap();
    let contract =
        t.rt.provided()
            .all::<dyn RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget>>()
            .into_iter()
            .find(|editor| editor.kind() == "docs")
            .unwrap();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    contract.open_file_with_decision(
        &restored.view,
        &RecoveryOpenFileRequest {
            file_path: path,
            decision: RecoveryOpenDecision::Restore,
        },
        &target,
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while answer.borrow().is_none() {
        t.rt.drain();
        assert!(
            std::time::Instant::now() < deadline,
            "captured recovery did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(answer.borrow().as_ref().unwrap().is_ok());
    std::fs::write(copy, b"replacement draft").unwrap();
    let synthetic =
        t.rt.services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .unwrap()
            .clone();
    let provenance = shell_contract::hosts::synthetic::SyntheticNativeOrigin::new();
    target
        .retain_native_registration(
            synthetic
                .capture_invocation_root(restored.view, &created, &provenance)
                .unwrap(),
        )
        .unwrap();
    let receiver = synthetic
        .capture_invocation_docs(restored.view, &created, &provenance)
        .unwrap();
    let pending = t.rt.invoke_native(
        "docs:consume-pending-open",
        restored,
        vec![],
        receiver.context,
        shell_contract::runtime::CancelToken::default(),
    );
    let opened = await_owned_reply(&mut t, &pending).unwrap();
    assert_eq!(
        get(&opened, "dataUrl"),
        &s("handoff:6361707475726564206472616674")
    );
    assert!(target.committed_seed().unwrap().settlement().is_none());
}

#[test]
fn an_unusable_recovery_copy_is_left_for_a_later_open() {
    use main_core::packages::electron_utils::recovery_store::{
        RecoveryCopyState, RecoveryPinnedRead,
    };
    let mut t = T::with_workers(json!({}), Some(Arc::new(BackgroundWorkers)));
    let path = t.file("unusable-recovery.docx", b"original");
    let opened = t.rt.invoke("docs:open-path", t.a, vec![s(&path)]);
    assert!(await_owned_reply(&mut t, &opened).is_ok());
    let write = t.rt.invoke(
        "docs:write-recovery",
        t.a,
        vec![s(&path), ab(b"captured draft")],
    );
    assert_eq!(
        get(&await_owned_reply(&mut t, &write).unwrap(), "ok"),
        &IpcValue::Bool(true)
    );
    let root = t.editor().0.user_data(&[]);
    let identity = super::recovery::file_identity(&path);
    let snapshot = || {
        RecoveryPinnedRead::open_saved(&root, &identity)
            .unwrap()
            .read()
            .unwrap()
    };
    let admit: super::saved_load::Admit = Arc::new(|effect| {
        effect();
        Ok(())
    });
    super::saved_load::write_copy_state(&snapshot(), RecoveryCopyState::Undecided, &admit).unwrap();
    assert!(snapshot().copy_info(None).unwrap().state == RecoveryCopyState::Undecided);
    super::saved_load::write_copy_state(&snapshot(), RecoveryCopyState::Later, &admit).unwrap();
    assert!(snapshot().copy_info(None).unwrap().state == RecoveryCopyState::Later);
}

#[test]
fn synthetic_media_transition_rejects_cancellation_and_retains_the_exact_successor() {
    let mut t = T::new(json!({}));
    let editor = t.editor();
    let lease = editor
        .0
        .media_views
        .borrow()
        .get(&t.a.view)
        .unwrap()
        .lease
        .clone();
    let old = lease.current().unwrap();
    let candidate = lease.prepare_document().unwrap();
    let successor = candidate.permit().clone();
    let stage = super::media_transition::DocsMediaStageAdapter {
        lease: lease.clone(),
        old: old.clone(),
        prior: None,
        candidate,
    };
    let generation = t.rt.services.views.renderer_generation(t.a.view).unwrap();
    let synthetic =
        t.rt.services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .unwrap()
            .clone();
    let provenance = shell_contract::hosts::synthetic::SyntheticNativeOrigin::new();
    let owner = synthetic
        .capture_invocation_docs(t.a.view, &generation, &provenance)
        .unwrap();
    let cleanup = Rc::new(RefCell::new(Some(
        editor.capture_disposal(t.a.view, &owner).unwrap(),
    )));
    let browser = editor
        .0
        .media_views
        .borrow()
        .get(&t.a.view)
        .unwrap()
        .owner
        .clone();
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let canceled = shell_contract::runtime::CancelToken::default();
    canceled.cancel();
    assert!(
        effects
            .stage_docs_media(&owner, &[], &canceled, &canceled, &stage, None)
            .is_err()
    );
    assert!(lease.pending().is_none());
    let pending = shell_contract::runtime::CancelToken::default();
    let foreign_lease = main_core::docs::byte_handoff::MediaViewLease::new(t.a.view.0).unwrap();
    let foreign = super::media_transition::DocsMediaStageAdapter {
        lease: lease.clone(),
        old: old.clone(),
        prior: None,
        candidate: foreign_lease.prepare_document().unwrap(),
    };
    assert!(
        effects
            .stage_docs_media(&owner, &[], &pending, &pending, &foreign, None)
            .is_err()
    );
    assert!(lease.pending().is_none());
    effects
        .stage_docs_media(&owner, &[], &pending, &pending, &stage, None)
        .unwrap();
    assert!(lease.pending().unwrap().same_session(&successor));
    let transition = super::media_transition::DocsMediaTransitionAdapter {
        lease: lease.clone(),
        old: old.clone(),
        successor: successor.clone(),
    };
    assert!(
        effects
            .transition_docs_media(
                &owner,
                &[],
                &canceled,
                &canceled,
                &transition,
                None,
                &mut |_, commit| commit()
            )
            .is_err()
    );
    assert!(old.is_active());
    assert!(lease.current().unwrap().same_session(&old));
    assert!(
        super::disposal::retain_successor(
            Some(&cleanup),
            &lease,
            &old,
            &successor,
            &owner,
            &mut || Err("commit rejected".into())
        )
        .is_err()
    );
    assert!(lease.current().unwrap().same_session(&old));
    let accepted = effects
        .transition_docs_media(
            &owner,
            &[],
            &pending,
            &pending,
            &transition,
            None,
            &mut |next, commit| {
                super::disposal::retain_successor(
                    Some(&cleanup),
                    &lease,
                    &old,
                    &successor,
                    next,
                    commit,
                )
            },
        )
        .unwrap();
    editor.0.retain_synthetic_successor(t.a.view, &accepted);
    assert!(!old.is_active());
    assert!(lease.current().unwrap().same_session(&successor));
    assert_eq!(
        effects.docs_document_identity(&accepted),
        Some((t.a.view, successor.owner().generation))
    );
    assert!(
        effects
            .transition_docs_media(
                &owner,
                &[],
                &pending,
                &pending,
                &transition,
                None,
                &mut |_, commit| commit()
            )
            .is_err()
    );
    let recaptured = synthetic
        .capture_invocation_docs(t.a.view, &generation, &provenance)
        .unwrap();
    assert_eq!(
        effects.docs_document_identity(&recaptured),
        Some((t.a.view, successor.owner().generation))
    );
    let actor = cleanup.borrow_mut().take().unwrap();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    actor.run_cleanup(
        Box::new(move |effect| {
            browser
                .reservation()
                .with_owner(&browser, |transition| effect(transition))
        }),
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    assert!(matches!(
        await_disposal(&mut t, &answer),
        super::editors::DocsDisposalOutcome::Retired { cleanup: Ok(()) }
    ));
    assert!(!successor.is_active());
}

#[test]
fn canceled_recovery_drain_waits_for_copy_worker_and_core_continuation() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenUntitledRequest,
    };
    use main_core::packages::electron_utils::recovery_store::RecoveryIdentity;
    use std::sync::atomic::{AtomicBool, Ordering};
    let workers = Arc::new(HeldWorkers::default());
    let mut t = T::with_workers(json!({}), Some(workers.clone()));
    let metadata = obj(&[("title", s("Recovery")), ("format", s("docx"))]);
    let write = t.rt.invoke(
        "docs:write-recovery",
        t.a,
        vec![IpcValue::Null, ab(b"draft"), metadata],
    );
    assert!(await_owned_reply(&mut t, &write).is_ok());
    let identity = t.editor().recovery_identity(t.a.view).unwrap();
    t.editor().teardown(t.a.view, true);
    let restored = t.rt.view(2, Surface::Docs);
    t.editor().register_view(restored.view).unwrap();
    let source = t.rt.view(22, Surface::Shell);
    let generation =
        t.rt.services
            .views
            .renderer_generation(source.view)
            .unwrap();
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let origin = effects.capture_owner(source.view, &generation).unwrap();
    let created =
        t.rt.services
            .views
            .renderer_generation(restored.view)
            .unwrap();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(restored.view, created.clone());
    let target = shell_contract::ipc::RecoveryOpenTarget::bind(
        shell_contract::ipc::RecoveryOpenContext::Renderer {
            source: origin,
            participants: Vec::new(),
            cancel: shell_contract::runtime::CancelToken::default(),
            pending: shell_contract::runtime::CancelToken::default(),
        },
        restored.view,
        created.clone(),
        closure,
        browser_owner,
    )
    .unwrap();
    let contract =
        t.rt.provided()
            .all::<dyn RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget>>()
            .into_iter()
            .find(|editor| editor.kind() == "docs")
            .unwrap();
    let answer = Rc::new(RefCell::new(None));
    let completed = answer.clone();
    contract.open_untitled_copy(
        &restored.view,
        &RecoveryOpenUntitledRequest {
            copy: RecoveryIdentity::Untitled(identity),
            title: String::new(),
            save_as_default_path: None,
        },
        &target,
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while answer.borrow().is_none() {
        t.rt.drain();
        assert!(
            std::time::Instant::now() < deadline,
            "recovery preparation did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(answer.borrow().as_ref().unwrap().is_ok());
    let synthetic =
        t.rt.services
            .hosts
            .get::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .unwrap()
            .clone();
    let provenance = shell_contract::hosts::synthetic::SyntheticNativeOrigin::new();
    target
        .retain_native_registration(
            synthetic
                .capture_invocation_root(restored.view, &created, &provenance)
                .unwrap(),
        )
        .unwrap();
    let receiver = synthetic
        .capture_invocation_docs(restored.view, &created, &provenance)
        .unwrap();
    workers.hold.store(true, Ordering::SeqCst);
    let _reply = t.rt.invoke_native(
        "docs:consume-copy",
        restored,
        vec![],
        receiver.context,
        shell_contract::runtime::CancelToken::default(),
    );
    created.revoke();
    let drained = Arc::new(AtomicBool::new(false));
    let notified = drained.clone();
    target
        .drain_work(
            t.rt.services.core.clone(),
            Box::new(move || notified.store(true, Ordering::SeqCst)),
        )
        .unwrap();
    t.rt.drain();
    assert!(!drained.load(Ordering::SeqCst));
    let job = workers
        .jobs
        .lock()
        .unwrap()
        .pop()
        .expect("copy worker was not queued");
    job();
    t.rt.drain();
    assert!(drained.load(Ordering::SeqCst));
}

#[cfg(unix)]
#[test]
fn saved_disposal_rejects_other_browser_and_settles_after_commit() {
    use shell_contract::ipc::{
        CallContext, NativeClosureIssuer, NativeContext, NativeInvocationIssuer,
        NativeRequestOwner, NativeSaveDestination, NativeSaveSelection, NativeSavedInput,
        NativeSavedPath, NativeSavedTarget, Responder,
    };
    use shell_contract::runtime::{CancelToken, RendererGeneration};
    use std::ffi::CString;

    let mut t = T::new(json!({}));
    let parent = std::fs::File::open(&t.rt.dir).unwrap();
    let path = t.rt.dir.join("saved.docx");
    let effects =
        t.rt.services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .unwrap()
            .clone();
    let source_view = t.a.view;
    let source_generation =
        t.rt.services
            .views
            .renderer_generation(source_view)
            .unwrap();
    let source = effects
        .capture_owner(source_view, &source_generation)
        .unwrap();
    let source = t
        .editor()
        .capture_document_owner(source_view, source)
        .unwrap();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer
        .issue_existing_document_save(
            Surface::Docs,
            source_view,
            NativeSaveSelection::ExactPath(path.clone()),
            source,
            CancelToken::default(),
        )
        .unwrap();
    let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
    let staging = file_commit::PrivateDirectory::create(&parent).unwrap();
    let stage = staging
        .create_stage(0o600)
        .unwrap()
        .quarantine()
        .ok()
        .unwrap();
    let stage = stage.write_owned(b"saved bytes".to_vec()).ok().unwrap();
    let stage = stage
        .prepare(
            &parent,
            &CString::new("saved.docx").unwrap(),
            &path,
            file_commit::PublicationMode::Exclusive,
        )
        .ok()
        .unwrap();
    let cancel = CancelToken::default();
    let pending = CancelToken::default();
    let (output, snapshot) = effects
        .publish_native_staged_write(&selection, &[], &cancel, &pending, &[], stage)
        .ok()
        .unwrap();
    let input = NativeSavedInput::from_written_snapshot(output.clone(), snapshot).unwrap();
    let generation = RendererGeneration::new();
    let (_, reservation, owner) = NativeClosureIssuer::new_with_owner(t.a.view, generation.clone());
    let target = NativeSavedTarget::prepare(
        input,
        NativeSavedPath::Selected(output),
        Surface::Docs,
        t.a.view,
        generation.clone(),
        reservation.clone(),
        owner.clone(),
        vec![],
        CancelToken::default(),
        CancelToken::default(),
        vec![],
    )
    .unwrap();
    reservation
        .with_initial_owner(&owner, |transition| {
            target.validate_in_transition(transition)?.commit();
            Ok(())
        })
        .unwrap();
    let native_owner = NativeRequestOwner {
        view: t.a.view,
        context: NativeContext::with_owner_loss((), generation, CancelToken::default()),
    };
    let document = target.register_from_native_admitted(native_owner).unwrap();
    std::fs::rename(&path, t.rt.dir.join("saved-original.docx")).unwrap();
    std::fs::write(&path, b"replacement bytes").unwrap();
    let lease = main_core::docs::byte_handoff::MediaViewLease::new(t.a.view.0).unwrap();
    let decoded = super::saved_load::decode_saved(
        &document,
        &t.fake,
        &t.fake,
        lease.current().unwrap(),
        CancelToken::default(),
    )
    .unwrap();
    assert_eq!(decoded.bytes.as_slice(), b"saved bytes");
    let content = super::saved_load::prepare_content(
        &document,
        &decoded,
        main_core::packages::electron_utils::recovery_store::SavedRecoveryOpenResult::Original,
        &t.fake,
        lease.current().unwrap(),
        CancelToken::default(),
    )
    .unwrap();
    assert_eq!(content.bytes, b"saved bytes");
    assert!(!content.recovered && !content.recovery_failed);
    let (_, _, foreign) = NativeClosureIssuer::new_with_owner(t.a.view, RendererGeneration::new());
    let replied = Arc::new(Mutex::new(None));
    let reply_slot = replied.clone();
    let responder = Responder::new(move |result| *reply_slot.lock().unwrap() = Some(result));
    let finished = Rc::new(RefCell::new(None));
    let completion = finished.clone();
    let cx = CallContext {
        sender: t.a,
        request: None,
        cancel: CancelToken::default(),
        native: None,
    };
    let docs = t.editor().0.clone();
    docs.saved_loads.borrow_mut().insert(
        t.a.view,
        super::saved_load::SavedLoad {
            target: target.clone(),
            document: None,
            waiter: Some((cx, responder)),
            done: Some(Box::new(move |result| {
                *completion.borrow_mut() = Some(result)
            })),
            token: "saved-token".into(),
            started: false,
            applying: false,
            recovered: false,
            recovery: None,
            staged: Default::default(),
            failure: None,
            owner_loss: None,
            callbacks: Default::default(),
            _losses: vec![],
        },
    );
    let received = Rc::new(std::cell::Cell::new(0));
    let captured = received.clone();
    let worker =
        super::saved_load::worker_callback(&docs, &target, move |value: u32| captured.set(value))
            .unwrap();
    worker(7);
    assert_eq!(received.get(), 0);
    t.rt.drain();
    assert_eq!(received.get(), 7);
    assert!(super::saved_load::prepare_disposal(&docs, t.a.view, &foreign).is_err());
    assert!(docs.saved_loads.borrow().contains_key(&t.a.view));
    let detached = super::saved_load::prepare_disposal(&docs, t.a.view, &owner)
        .unwrap()
        .commit()
        .unwrap();
    assert!(replied.lock().unwrap().is_none());
    assert!(finished.borrow().is_none());
    detached.fail("closed");
    assert!(replied.lock().unwrap().as_ref().unwrap().is_err());
    assert!(finished.borrow().as_ref().unwrap().is_err());
}

#[test]
fn generated_cleanup_ack_waits_for_original_completion() {
    use shell_contract::hosts::{
        GeneratedObserveFailure, GeneratedPathObserver, GeneratedPathOutcome, GeneratedPathPending,
        GeneratedPathWake, GeneratedStartFailure, GeneratedTerminalAck, GeneratedTerminalDelivery,
        PreparedGeneratedPath,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Ack(Arc<AtomicUsize>);
    impl GeneratedTerminalAck for Ack {
        fn complete(&self) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn restore(&self, _: Result<GeneratedPathOutcome, String>) {
            panic!("unacknowledged test delivery was dropped");
        }
    }
    #[derive(Default)]
    struct Pending(
        Mutex<Option<(GeneratedPathObserver, GeneratedPathWake)>>,
        AtomicBool,
    );
    impl GeneratedPathPending for Pending {
        fn observe(
            &self,
            observer: GeneratedPathObserver,
            wake: GeneratedPathWake,
        ) -> Result<(), GeneratedObserveFailure> {
            let mut slot = self.0.lock().unwrap();
            if slot.is_some() {
                return Err(GeneratedObserveFailure {
                    error: "already observing".into(),
                    observer,
                    wake,
                });
            }
            *slot = Some((observer, wake));
            Ok(())
        }
        fn wake_observer(&self) -> Result<(), String> {
            if let Some((_, wake)) = self.0.lock().unwrap().as_ref() {
                wake();
            }
            Ok(())
        }
        fn take_redelivery(&self) -> Result<Option<GeneratedTerminalDelivery>, String> {
            Ok(None)
        }
        fn retry(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
            done(Ok(()));
        }
        fn is_cancelling(&self) -> bool {
            self.1.load(Ordering::SeqCst)
        }
        fn cancel_and_drain(&self, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
            self.1.store(true, Ordering::SeqCst);
            done(Ok(()));
        }
    }
    struct Prepared(Arc<Pending>);
    impl PreparedGeneratedPath for Prepared {
        fn task(&self) -> Arc<dyn GeneratedPathPending> {
            self.0.clone()
        }
        fn start(self: Box<Self>) -> Result<(), GeneratedStartFailure> {
            assert!(self.0.0.lock().unwrap().is_some());
            Ok(())
        }
    }
    let mut t = T::new(json!({}));
    let docs = t.editor().0.clone();
    let pending = Arc::new(Pending::default());
    let acked = Arc::new(AtomicUsize::new(0));
    let completed: Rc<RefCell<Option<crate::generated_mailbox::Finished>>> =
        Rc::new(RefCell::new(None));
    let captured = completed.clone();
    let ticket = super::cleanup_mailbox::begin(
        &docs,
        Box::new(move |result, done| {
            assert!(matches!(result, Ok(GeneratedPathOutcome::NotOpened)));
            *captured.borrow_mut() = Some(done);
        }),
    )
    .unwrap();
    ticket.attach(&docs, Box::new(Prepared(pending.clone())));
    let (observer, _) = pending.0.lock().unwrap().take().unwrap();
    observer(GeneratedTerminalDelivery::new(
        Ok(GeneratedPathOutcome::NotOpened),
        Arc::new(Ack(acked.clone())),
    ));
    t.rt.drain();
    assert_eq!(acked.load(Ordering::SeqCst), 0);
    completed.borrow_mut().take().unwrap().complete();
    assert_eq!(acked.load(Ordering::SeqCst), 1);
    super::cleanup_mailbox::wake(&docs);
    t.rt.drain();
    assert_eq!(acked.load(Ordering::SeqCst), 1);
}

#[test]
fn generated_completion_survives_a_discarded_core_post() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Ack;
    impl shell_contract::hosts::GeneratedTerminalAck for Ack {
        fn complete(&self) -> Result<(), String> {
            Ok(())
        }
        fn restore(&self, _: Result<shell_contract::hosts::GeneratedPathOutcome, String>) {
            panic!("unacknowledged test delivery was dropped");
        }
    }
    #[derive(Default)]
    struct Core {
        discard: AtomicBool,
        jobs: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    }
    impl shell_contract::runtime::Thread for Core {
        fn post(&self, job: Box<dyn FnOnce() + Send>) {
            if self.discard.swap(false, Ordering::SeqCst) {
                drop(job);
            } else {
                self.jobs.lock().unwrap().push(job);
            }
        }
        fn is_current(&self) -> bool {
            true
        }
    }
    let fake = Fake::new(&json!({}));
    let host = fake.clone();
    let mut rt = Rt::new(move |hosts| {
        hosts.insert::<DocsEngineHost>(Arc::new(host.clone()));
        hosts.insert::<DocsShellHost>(Arc::new(host));
        hosts.insert::<Arc<Mutex<SharedRecoveryState>>>(Arc::new(Mutex::new(
            SharedRecoveryState::new("run"),
        )));
    });
    let core = Arc::new(Core::default());
    rt.services.core = core.clone();
    fake.bind_query_runtime(&rt.services);
    rt.register(super::entries).unwrap();
    let docs = rt.provided().all::<DocsEditors>()[0].0.clone();
    let calls = Rc::new(std::cell::Cell::new(0));
    let counted = calls.clone();
    let ticket = super::cleanup_mailbox::begin(
        &docs,
        Box::new(move |result, done| {
            assert!(matches!(
                result,
                Ok(shell_contract::hosts::GeneratedPathOutcome::NotOpened)
            ));
            counted.set(counted.get() + 1);
            done.complete();
        }),
    )
    .unwrap();
    core.discard.store(true, Ordering::SeqCst);
    ticket.deliver(
        &docs,
        shell_contract::hosts::GeneratedTerminalDelivery::new(
            Ok(shell_contract::hosts::GeneratedPathOutcome::NotOpened),
            Arc::new(Ack),
        ),
    );
    assert_eq!(calls.get(), 0);
    super::cleanup_mailbox::wake(&docs);
    let job = core.jobs.lock().unwrap().pop().unwrap();
    job();
    assert_eq!(calls.get(), 1);
    super::cleanup_mailbox::wake(&docs);
    assert_eq!(calls.get(), 1);
}

#[test]
fn generated_ack_failure_keeps_actor_task_for_retry() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Ack(AtomicUsize);
    impl shell_contract::hosts::GeneratedTerminalAck for Ack {
        fn complete(&self) -> Result<(), String> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("ack unavailable".into())
            } else {
                Ok(())
            }
        }
        fn restore(&self, _: Result<shell_contract::hosts::GeneratedPathOutcome, String>) {
            panic!("unacknowledged test delivery was dropped");
        }
    }
    let mut t = T::new(json!({}));
    let docs = t.editor().0.clone();
    let replies = Rc::new(std::cell::Cell::new(0));
    let captured = replies.clone();
    let ticket = super::cleanup_mailbox::begin(
        &docs,
        Box::new(move |result, done| {
            assert!(matches!(
                result,
                Ok(shell_contract::hosts::GeneratedPathOutcome::NotOpened)
            ));
            captured.set(captured.get() + 1);
            done.complete();
        }),
    )
    .unwrap();
    let ack = Arc::new(Ack(AtomicUsize::new(0)));
    ticket.deliver(
        &docs,
        shell_contract::hosts::GeneratedTerminalDelivery::new(
            Ok(shell_contract::hosts::GeneratedPathOutcome::NotOpened),
            ack.clone(),
        ),
    );
    t.rt.drain();
    assert_eq!(replies.get(), 1);
    assert_eq!(ack.0.load(Ordering::SeqCst), 1);
    assert!(
        !t.fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == &json!(["generatedActorCleanupComplete"]))
    );
    super::cleanup_mailbox::wake(&docs);
    assert_eq!(replies.get(), 1);
    assert_eq!(ack.0.load(Ordering::SeqCst), 2);
    assert!(
        t.fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == &json!(["generatedActorCleanupComplete"]))
    );
}
