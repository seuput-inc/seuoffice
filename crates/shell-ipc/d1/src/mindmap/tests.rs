//! Tests of the mind map handlers: what a renderer observes.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shell_contract::editors::{DocumentEditors, SaveMode};
use shell_contract::ipc::{Sender, Target};
use shell_contract::{IpcError, IpcValue, Surface, ViewId};

use super::hosts::ProbeLog;
use super::{MindmapApp, Seed, handle};
use crate::testkit::{Kit, TestRegistry, call, registered, temp_dir};
use crate::value::{buffer, field, obj, string};

struct H {
    kit: Kit,
    reg: TestRegistry,
    log: ProbeLog,
    sender: Sender,
    dir: PathBuf,
    app: MindmapApp,
}

fn harness(name: &str, case: impl FnOnce(&Path) -> Value) -> H {
    let dir = temp_dir(name);
    let case = case(&dir);
    let log: ProbeLog = Arc::default();
    let kit = Kit::new(&dir, |hosts| {
        // the other apps of the batch need their fakes to register
        crate::insert_fakes(hosts);
        hosts.insert::<ProbeLog>(log.clone());
        super::insert_fakes(hosts, &case);
    });
    // the native host gives every capture of one renderer the same origin
    kit.host::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
        .share_capture_origins();
    let reg = registered(&kit);
    let sender = kit.view(1, Surface::Mindmap);
    let app = handle().expect("routes ran");
    H {
        kit,
        reg,
        log,
        sender,
        dir,
        app,
    }
}

fn plain(name: &str) -> H {
    harness(name, |_| Value::Null)
}

impl H {
    fn call(&self, channel: &str, args: Vec<IpcValue>) -> Result<IpcValue, IpcError> {
        call(&self.reg, self.sender, channel, args)
    }

    fn call_as(&self, sender: Sender, channel: &str, args: Vec<IpcValue>) -> IpcValue {
        call(&self.reg, sender, channel, args).expect("answered")
    }

    fn ok(&self, channel: &str, args: Vec<IpcValue>) -> IpcValue {
        self.call(channel, args).expect("answered")
    }

    fn pick_save(&self, path: Option<&Path>) {
        self.kit
            .dialogs
            .saves
            .lock()
            .unwrap()
            .push_back(path.map(Path::to_path_buf));
    }

    fn answer_message(&self, response: usize) {
        self.kit
            .dialogs
            .messages
            .lock()
            .unwrap()
            .push_back(response);
    }

    fn events(&self, channel: &str) -> Vec<Vec<IpcValue>> {
        self.kit
            .take_events()
            .into_iter()
            .filter(|(_, name, _)| *name == channel)
            .map(|(_, _, args)| args)
            .collect()
    }

    fn probe_log(&self) -> Vec<Value> {
        self.log.lock().unwrap().clone()
    }

    fn logged(&self, name: &str) -> Vec<Value> {
        self.probe_log()
            .into_iter()
            .filter(|line| line[0] == name)
            .collect()
    }

    fn userdata(&self) -> PathBuf {
        self.dir.join("userdata")
    }

    /// recovery copies on disk, without their metadata
    fn copies(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if !path.to_string_lossy().ends_with(".meta.json") {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.userdata().join("mindmap-autosave"), &mut out);
        out
    }

    /// the mind map's own editor: another app's editor may own the same view id in this registry
    fn editor(&self, view: u32) -> Rc<dyn DocumentEditors> {
        let editor: Rc<dyn DocumentEditors> =
            Rc::new(super::editors::MindmapEditors(self.app.clone()));
        assert!(editor.owns(ViewId(view)), "a mind map view");
        editor
    }
}

fn bytes(text: &str) -> IpcValue {
    buffer(text.as_bytes().to_vec())
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn save_request(text: &str, mode: &str) -> IpcValue {
    obj([("bytes", bytes(text)), ("mode", string(mode))])
}

fn is_ok(value: &IpcValue) -> bool {
    field(value, "ok") == &IpcValue::Bool(true)
}

fn error_of(value: &IpcValue) -> String {
    match field(value, "error") {
        IpcValue::String(text) => text.clone(),
        other => panic!("no error in {value:?} ({other:?})"),
    }
}

type Slot<T> = Rc<RefCell<Option<Result<T, IpcError>>>>;
type Then<T> = Box<dyn FnOnce(Result<T, IpcError>)>;

fn slot<T: 'static>() -> (Slot<T>, Then<T>) {
    let slot: Slot<T> = Rc::default();
    let keep = slot.clone();
    (
        slot,
        Box::new(move |result| *keep.borrow_mut() = Some(result)),
    )
}

fn open_granted_file(h: &H, name: &str, content: &str) -> PathBuf {
    let file = h.dir.join(name);
    fs::write(&file, content).unwrap();
    h.app.create_view(h.sender.view, Some(&path_str(&file)));
    let opened = h.ok("mindmap:open-file", vec![string(path_str(&file))]);
    assert_eq!(field(&opened, "status"), &string("ok"));
    file
}

#[test]
fn recovery_flush_rejects_stale_ack_and_propagates_failure() {
    let h = plain("mindmap-flush-result");
    h.app.create_view(h.sender.view, None);
    h.ok("mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    let first = Rc::new(RefCell::new(None));
    let sink = first.clone();
    h.app.write_copies_now(
        Duration::from_secs(5),
        Box::new(move |result| *sink.borrow_mut() = Some(result)),
    );
    let stale = h.events("mindmap:recovery-copy-now")[0][0].clone();
    let second = Rc::new(RefCell::new(None));
    let sink = second.clone();
    h.app.write_copies_now(
        Duration::from_secs(5),
        Box::new(move |result| *sink.borrow_mut() = Some(result)),
    );
    let current = h.events("mindmap:recovery-copy-now")[0][0].clone();
    assert!(matches!(&*first.borrow(), Some(Err(_))));
    h.ok(
        "mindmap:recovery-copy-now-done",
        vec![stale, IpcValue::Null],
    );
    assert!(second.borrow().is_none());
    h.ok(
        "mindmap:recovery-copy-now-done",
        vec![current, string("disk failed")],
    );
    assert_eq!(
        second.borrow().as_ref().unwrap().as_ref().unwrap_err().0,
        "disk failed"
    );
}

#[test]
fn engine_failure_rejects_open_without_classifying_the_file_as_unreadable() {
    let h = harness(
        "engine-open-failure",
        |_| json!({"mindmap": {"openXmind": [{"error": "engine unavailable"}]}}),
    );
    let file = h.dir.join("map.xmind");
    fs::write(&file, "XMIND-1").unwrap();
    h.app.create_view(h.sender.view, Some(&path_str(&file)));
    let error = h
        .call("mindmap:open-file", vec![string(path_str(&file))])
        .unwrap_err();
    assert_eq!(error.message, "engine unavailable");
    assert_eq!(fs::read_to_string(file).unwrap(), "XMIND-1");
}

#[test]
fn engine_failure_rejects_save_and_settles_the_waiter_without_writing() {
    let h = harness(
        "engine-save-failure",
        |_| json!({"mindmap": {"openXmind": [{"status": "ok"}, {"error": "engine unavailable"}]}}),
    );
    let file = open_granted_file(&h, "map.xmind", "XMIND-1");
    let (waiting, then) = slot::<bool>();
    h.editor(1).request_save(ViewId(1), SaveMode::SaveAs, then);
    let error = h
        .call("mindmap:save", vec![save_request("XMIND-2", "saveAs")])
        .unwrap_err();
    assert_eq!(error.message, "engine unavailable");
    assert_eq!(waiting.borrow().clone(), Some(Ok(false)));
    assert_eq!(fs::read_to_string(file).unwrap(), "XMIND-1");
}

#[test]
fn engine_failure_rejects_project_save_without_creating_a_file() {
    let h = harness("engine-project-failure", |dir| {
        json!({"mindmap": {
            "openXmind": [{"error": "engine unavailable"}],
            "writeNewDocument": {"path": path_str(&dir.join("project").join("map.xmind"))}
        }})
    });
    h.app.create_view(h.sender.view, None);
    let error = h
        .call(
            "mindmap:save-into-project",
            vec![save_request("XMIND-1", "save")],
        )
        .unwrap_err();
    assert_eq!(error.message, "engine unavailable");
    assert!(!h.dir.join("project").join("map.xmind").exists());
}

#[test]
fn a_granted_file_opens_and_saves_and_a_bad_one_is_never_written() {
    let h = plain("open-save");
    let file = h.dir.join("a.xmind");
    fs::write(&file, "XMIND-1").unwrap();
    h.app.create_view(ViewId(1), Some(&path_str(&file)));
    assert_eq!(
        h.ok("mindmap:consume-pending", vec![]),
        string(path_str(&file))
    );
    // a path that was not granted to the view is refused
    let refused = h
        .call(
            "mindmap:open-file",
            vec![string(path_str(&h.dir.join("other.xmind")))],
        )
        .unwrap_err();
    assert_eq!(refused.message, "mindmap: path not granted to this view");

    let opened = h.ok("mindmap:open-file", vec![string(path_str(&file))]);
    assert_eq!(
        opened,
        obj([("status", string("ok")), ("bytes", bytes("XMIND-1"))])
    );
    assert_eq!(h.app.file_path(ViewId(1)), Some(path_str(&file)));

    let saved = h.ok("mindmap:save", vec![save_request("XMIND-2", "save")]);
    assert_eq!(
        saved,
        obj([
            ("ok", IpcValue::Bool(true)),
            ("path", string(path_str(&file)))
        ])
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "XMIND-2");

    // bytes that would not open as a map never replace the file
    let refused = h.ok("mindmap:save", vec![save_request("junk", "save")]);
    assert_eq!(
        error_of(&refused),
        "mindmap: refusing to write a file that would not open (rejected)"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "XMIND-2");

    // a file that did not open is a view nothing is written for
    let broken = h.dir.join("b.xmind");
    fs::write(&broken, "junk").unwrap();
    let second = h.kit.view(2, Surface::Mindmap);
    h.app.create_view(ViewId(2), Some(&path_str(&broken)));
    let status = h.call_as(second, "mindmap:open-file", vec![string(path_str(&broken))]);
    assert_eq!(status, obj([("status", string("rejected"))]));
    let refused = h.call_as(
        second,
        "mindmap:save",
        vec![save_request("XMIND-3", "save")],
    );
    assert_eq!(
        error_of(&refused),
        "mindmap: the file did not open, nothing is written"
    );
    assert_eq!(fs::read_to_string(&broken).unwrap(), "junk");
    h.call_as(second, "mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    assert!(!h.app.is_dirty(ViewId(2)));
}

#[test]
fn an_untitled_map_asks_where_and_later_saves_go_to_that_file() {
    let h = plain("first-save");
    h.app.create_view(ViewId(1), None);
    assert_eq!(h.ok("mindmap:consume-pending", vec![]), IpcValue::Null);
    // a cancelled dialog writes nothing
    h.pick_save(None);
    let canceled = h.ok("mindmap:save", vec![save_request("XMIND-1", "save")]);
    assert_eq!(
        canceled,
        obj([
            ("ok", IpcValue::Bool(true)),
            ("canceled", IpcValue::Bool(true))
        ])
    );

    let target = h.dir.join("first");
    h.pick_save(Some(&target));
    let request = obj([
        ("bytes", bytes("XMIND-1")),
        ("mode", string("save")),
        ("defaultName", string("Plan/A")),
    ]);
    let saved = h.ok("mindmap:save", vec![request]);
    let written = h.dir.join("first.xmind");
    assert_eq!(
        saved,
        obj([
            ("ok", IpcValue::Bool(true)),
            ("path", string(path_str(&written)))
        ])
    );
    assert_eq!(fs::read_to_string(&written).unwrap(), "XMIND-1");
    assert_eq!(
        h.logged("mindmap-main.fileSavedHook"),
        vec![json!(["mindmap-main.fileSavedHook", 1, path_str(&written)])]
    );
    // the dialog suggested the sanitized name in the default folder
    let dialog_log = h.kit.take_log().join("\n");
    assert!(dialog_log.contains("Plan_A.xmind"), "{dialog_log}");

    // the next save has a target: no dialog
    let again = h.ok("mindmap:save", vec![save_request("XMIND-2", "save")]);
    assert!(is_ok(&again));
    assert!(
        !h.kit
            .take_log()
            .iter()
            .any(|line| line.starts_with("save "))
    );
    assert_eq!(fs::read_to_string(&written).unwrap(), "XMIND-2");
}

#[test]
fn save_as_an_export_type_grants_that_one_path_to_the_export() {
    let h = plain("export");
    h.app.create_view(ViewId(1), None);
    let target = h.dir.join("pic.png");
    h.pick_save(Some(&target));
    let picked = h.ok("mindmap:save", vec![save_request("XMIND-1", "saveAs")]);
    let request_id = field(field(&picked, "export"), "ownedSaveRequestId").clone();
    assert!(matches!(request_id, IpcValue::String(_)));
    assert_eq!(
        picked,
        obj([
            ("ok", IpcValue::Bool(true)),
            (
                "export",
                obj([
                    ("format", string("png")),
                    ("path", string(path_str(&target))),
                    ("ownedSaveRequestId", request_id.clone()),
                ])
            ),
        ])
    );
    // Save As lists the export types after .xmind, and nothing was written yet
    let dialog_log = h.kit.take_log().join("\n");
    assert!(dialog_log.contains("PNG Image") && dialog_log.contains("Mind Maps"));
    assert!(!target.exists());

    let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    png.extend_from_slice(b"data");
    let export = |path: IpcValue| {
        obj([
            ("bytes", buffer(png.clone())),
            ("suggestedName", string("m")),
            ("path", path),
            ("ownedSaveRequestId", request_id.clone()),
        ])
    };
    // another path is not the one Save As picked
    let other = h
        .call(
            "mindmap:export-png",
            vec![export(string(path_str(&h.dir.join("x.png"))))],
        )
        .unwrap_err();
    assert_eq!(
        other.message,
        "This export was not selected by this save request"
    );
    let done = h.ok(
        "mindmap:export-png",
        vec![export(string(path_str(&target)))],
    );
    assert_eq!(
        done,
        obj([
            ("ok", IpcValue::Bool(true)),
            ("path", string(path_str(&target)))
        ])
    );
    assert_eq!(fs::read(&target).unwrap(), png);
    // taken once
    let twice = h
        .call(
            "mindmap:export-png",
            vec![export(string(path_str(&target)))],
        )
        .unwrap_err();
    assert_eq!(
        twice.message,
        "This export was not selected by this save request"
    );
}

#[test]
fn an_export_without_a_granted_path_asks_with_its_own_dialog() {
    let h = plain("export-dialog");
    let file = open_granted_file(&h, "m.xmind", "XMIND-1");
    let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\"><g/></svg>";
    let request = |svg: &str| obj([("svg", string(svg)), ("suggestedName", string("sheet"))]);

    // the dialog starts beside the map's file and takes the kind's extension
    h.pick_save(Some(&h.dir.join("out")));
    let done = h.ok("mindmap:export-svg", vec![request(svg)]);
    let out = h.dir.join("out.svg");
    assert_eq!(
        done,
        obj([
            ("ok", IpcValue::Bool(true)),
            ("path", string(path_str(&out)))
        ])
    );
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{svg}")
    );
    let dialog_log = h.kit.take_log().join("\n");
    assert!(
        dialog_log.contains(&path_str(&file.parent().unwrap().join("sheet.svg"))),
        "{dialog_log}"
    );

    // cancelled, and a page that is not a map's SVG
    assert_eq!(
        h.ok("mindmap:export-svg", vec![request(svg)]),
        obj([
            ("ok", IpcValue::Bool(true)),
            ("canceled", IpcValue::Bool(true))
        ])
    );
    let bad = h.ok(
        "mindmap:export-svg",
        vec![request("<svg><script>x</script></svg>")],
    );
    assert_eq!(error_of(&bad), "mindmap: bad SVG export request");

    // markdown and PDF write what the renderer sent (the PDF through the page renderer)
    h.pick_save(Some(&h.dir.join("outline.md")));
    let md = h.ok(
        "mindmap:export-markdown",
        vec![obj([
            ("text", string("# a")),
            ("suggestedName", string("o")),
        ])],
    );
    assert!(is_ok(&md));
    assert_eq!(fs::read_to_string(h.dir.join("outline.md")).unwrap(), "# a");
    h.pick_save(Some(&h.dir.join("doc")));
    let pdf = h.ok(
        "mindmap:export-pdf",
        vec![obj([
            ("html", string("<p>x</p>")),
            ("suggestedName", string("d")),
        ])],
    );
    assert_eq!(
        field(&pdf, "path"),
        &string(path_str(&h.dir.join("doc.pdf")))
    );
    assert_eq!(fs::read(h.dir.join("doc.pdf")).unwrap(), b"%PDF-1.7");
    assert_eq!(
        h.logged("export-main.pageToPdf"),
        vec![json!(["export-main.pageToPdf", "<p>x</p>"])]
    );
}

#[test]
fn a_menu_save_waits_for_the_renderer_and_its_ack_resumes_it() {
    let h = plain("save-round-trip");
    let file = open_granted_file(&h, "m.xmind", "XMIND-1");
    let editor = h.editor(1);

    // a clean map with a file has nothing to save: answered at once, nothing asked
    let (clean, then) = slot::<bool>();
    editor.request_save(ViewId(1), SaveMode::Save, then);
    assert_eq!(clean.borrow().clone(), Some(Ok(true)));
    assert!(h.events("mindmap:save-request").is_empty());

    // Save As asks the renderer; the ack of a save it declined resumes the wait
    let (waiting, then) = slot::<bool>();
    editor.request_save(ViewId(1), SaveMode::SaveAs, then);
    assert!(waiting.borrow().is_none());
    assert_eq!(
        h.events("mindmap:save-request"),
        vec![vec![string("saveAs")]]
    );
    h.ok("mindmap:save-request-ack", vec![IpcValue::Bool(false)]);
    assert_eq!(waiting.borrow().clone(), Some(Ok(false)));

    // a dirty map's save call is what answers: only a written map is true
    h.ok("mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    let (saving, then) = slot::<bool>();
    editor.request_save(ViewId(1), SaveMode::Save, then);
    assert_eq!(h.events("mindmap:save-request"), vec![vec![string("save")]]);
    h.ok("mindmap:save", vec![save_request("XMIND-2", "save")]);
    assert_eq!(saving.borrow().clone(), Some(Ok(true)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "XMIND-2");
    assert!(!h.app.is_dirty(ViewId(1)));

    // a Save As that ends in the dialog's cancel is no save
    let (cancelled, then) = slot::<bool>();
    editor.request_save(ViewId(1), SaveMode::SaveAs, then);
    h.pick_save(None);
    h.ok("mindmap:save", vec![save_request("XMIND-3", "saveAs")]);
    assert_eq!(cancelled.borrow().clone(), Some(Ok(false)));

    // a renderer that never answers is given up on after 120 s
    let (silent, then) = slot::<bool>();
    editor.request_save(ViewId(1), SaveMode::SaveAs, then);
    h.app.run_due(Instant::now() + Duration::from_secs(60));
    assert!(silent.borrow().is_none());
    h.app.run_due(Instant::now() + Duration::from_secs(121));
    assert_eq!(silent.borrow().clone(), Some(Ok(false)));

    // the MCP server's save to a path points the target there, and gives it back when the save fails
    let (to_path, then) = slot::<()>();
    let target = h.dir.join("elsewhere.xmind");
    editor.save_to_path(ViewId(1), target.clone(), then);
    assert_eq!(h.app.file_path(ViewId(1)), Some(path_str(&target)));
    h.ok("mindmap:save-request-ack", vec![IpcValue::Bool(false)]);
    assert_eq!(
        to_path.borrow().clone().unwrap().unwrap_err().message,
        "could not save the map"
    );
    assert_eq!(h.app.file_path(ViewId(1)), Some(path_str(&file)));
}

#[test]
fn the_close_prompt_cancels_discards_or_saves_through_the_renderer() {
    let h = plain("close");
    let file = open_granted_file(&h, "m.xmind", "XMIND-1");
    let editor = h.editor(1);
    let parent = Some(h.sender.window);

    // clean: closes without a question
    let (clean, then) = slot::<bool>();
    editor.request_close(ViewId(1), parent, then);
    assert_eq!(clean.borrow().clone(), Some(Ok(true)));
    assert!(
        h.kit
            .take_log()
            .iter()
            .all(|line| !line.starts_with("message"))
    );

    h.ok("mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    // Cancel
    h.answer_message(2);
    let (cancel, then) = slot::<bool>();
    editor.request_close(ViewId(1), parent, then);
    assert_eq!(cancel.borrow().clone(), Some(Ok(false)));
    let prompt = h.kit.take_log().join("\n");
    assert!(
        prompt.contains("message Some(WindowId(1))") && prompt.contains("Don't Save"),
        "{prompt}"
    );

    // Don't Save: closes at once, the work is dropped
    h.answer_message(1);
    let (dropped, then) = slot::<bool>();
    editor.request_close(ViewId(1), parent, then);
    assert_eq!(dropped.borrow().clone(), Some(Ok(true)));

    // Save: the renderer is asked, and its result answers
    h.answer_message(0);
    let (saving, then) = slot::<bool>();
    editor.request_close(ViewId(1), parent, then);
    assert!(saving.borrow().is_none());
    assert_eq!(
        h.events("mindmap:close-save-request"),
        vec![Vec::<IpcValue>::new()]
    );
    h.ok("mindmap:close-save-result", vec![IpcValue::Bool(true)]);
    assert_eq!(saving.borrow().clone(), Some(Ok(true)));

    h.answer_message(0);
    let (failed, then) = slot::<bool>();
    editor.request_close(ViewId(1), parent, then);
    h.ok("mindmap:close-save-result", vec![IpcValue::Bool(false)]);
    assert_eq!(failed.borrow().clone(), Some(Ok(false)));
    drop(file);
}

#[test]
fn recovery_copies_are_kept_only_for_unsaved_maps_that_opened() {
    let h = plain("recovery-gate");
    h.app.create_view(ViewId(1), None);
    let copy = |name: &str| obj([("bytes", bytes("XMIND-copy")), ("name", string(name))]);
    // nothing unsaved: skipped
    assert_eq!(
        h.ok("mindmap:recovery-copy", vec![copy("My  map")]),
        string("skipped")
    );
    h.ok("mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    // no bytes: skipped
    assert_eq!(
        h.ok("mindmap:recovery-copy", vec![obj([("name", string("x"))])]),
        string("skipped")
    );
    assert_eq!(
        h.ok("mindmap:recovery-copy", vec![copy("My  map")]),
        string("written")
    );
    let copies = h.copies();
    assert_eq!(copies.len(), 1);
    assert!(copies[0].to_string_lossy().contains("untitled"));
    let meta = fs::read_to_string(format!("{}.meta.json", copies[0].display())).unwrap();
    assert!(meta.contains("My map"), "{meta}");

    // a view whose file did not open takes no copy, whatever it says
    let broken = h.dir.join("b.xmind");
    fs::write(&broken, "junk").unwrap();
    let second = h.kit.view(2, Surface::Mindmap);
    h.app.create_view(ViewId(2), Some(&path_str(&broken)));
    h.call_as(second, "mindmap:open-file", vec![string(path_str(&broken))]);
    h.call_as(second, "mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    assert_eq!(
        h.call_as(second, "mindmap:recovery-copy", vec![copy("x")]),
        string("skipped")
    );
    assert_eq!(h.copies().len(), 1);

    // the map reaching the disk takes its copy away
    h.pick_save(Some(&h.dir.join("saved")));
    assert!(is_ok(
        &h.ok("mindmap:save", vec![save_request("XMIND-1", "save")])
    ));
    assert!(h.copies().is_empty());

    // "Don't Save" drops the copy and holds new ones off until the hold ends
    h.ok("mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    assert_eq!(
        h.ok("mindmap:recovery-copy", vec![copy("x")]),
        string("written")
    );
    assert_eq!(h.copies().len(), 1);
    h.answer_message(1);
    let (closed, then) = slot::<bool>();
    h.editor(1).request_close(ViewId(1), None, then);
    assert_eq!(closed.borrow().clone(), Some(Ok(true)));
    assert!(h.copies().is_empty());
    assert_eq!(
        h.ok("mindmap:recovery-copy", vec![copy("x")]),
        string("superseded")
    );
    h.kit.take_events();
    h.app.run_due(Instant::now() + Duration::from_secs(6));
    assert_eq!(h.events("mindmap:recovery-reset").len(), 1);
    // the shared store still turns the copy away until the window's close is
    // cancelled (a later tab's Cancel), then the open tab copies again
    assert_eq!(
        h.ok("mindmap:recovery-copy", vec![copy("x")]),
        string("superseded")
    );
    main_core::packages::electron_utils::recovery_store::resume_discarded_copies(
        &h.app.recovery_state(),
    );
    assert_eq!(
        h.ok("mindmap:recovery-copy", vec![copy("x")]),
        string("written")
    );
}

#[test]
fn a_copy_left_for_a_file_is_asked_about_when_the_file_opens() {
    let h = plain("recovery-open");
    let file = open_granted_file(&h, "m.xmind", "XMIND-file");
    h.ok("mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    let copy = obj([("bytes", bytes("XMIND-copy"))]);
    assert_eq!(h.ok("mindmap:recovery-copy", vec![copy]), string("written"));
    // the view ends with unsaved changes: its copy stays
    h.kit.destroy(ViewId(1));
    assert_eq!(h.copies().len(), 1);
    h.kit.take_log();

    // the file reopens in another view: Restore (the default) loads the copy over the file
    let second = h.kit.view(2, Surface::Mindmap);
    h.app.create_view(ViewId(2), Some(&path_str(&file)));
    let restored = h.call_as(second, "mindmap:open-file", vec![string(path_str(&file))]);
    assert_eq!(
        restored,
        obj([
            ("status", string("ok")),
            ("bytes", bytes("XMIND-copy")),
            ("restored", string("file")),
        ])
    );
    assert!(
        h.kit
            .take_log()
            .iter()
            .any(|line| line.starts_with("message"))
    );
    assert_eq!(h.app.file_path(ViewId(2)), Some(path_str(&file)));
}

#[test]
fn a_discarded_copy_opens_the_file_as_saved() {
    let h = plain("recovery-discard");
    let file = open_granted_file(&h, "m.xmind", "XMIND-file");
    h.ok("mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    h.ok(
        "mindmap:recovery-copy",
        vec![obj([("bytes", bytes("XMIND-copy"))])],
    );
    h.kit.destroy(ViewId(1));
    let second = h.kit.view(2, Surface::Mindmap);
    h.app.create_view(ViewId(2), Some(&path_str(&file)));
    h.answer_message(1);
    let opened = h.call_as(second, "mindmap:open-file", vec![string(path_str(&file))]);
    assert_eq!(
        opened,
        obj([("status", string("ok")), ("bytes", bytes("XMIND-file"))])
    );
    assert!(h.copies().is_empty());
}

#[test]
fn a_view_that_is_gone_leaves_no_state_and_resumes_what_waited_on_it() {
    let h = plain("view-gone");
    // view 1: clean map with a copy, a pending save, a queued seed, a pending read
    h.app.create_view(ViewId(1), None);
    h.ok("mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    h.ok(
        "mindmap:recovery-copy",
        vec![obj([("bytes", bytes("XMIND-1"))])],
    );
    h.ok("mindmap:dirty-changed", vec![IpcValue::Bool(false)]);
    assert_eq!(h.copies().len(), 1);
    // view 2: unsaved when it ends
    let second = h.kit.view(2, Surface::Mindmap);
    h.app.create_view(ViewId(2), None);
    h.call_as(second, "mindmap:dirty-changed", vec![IpcValue::Bool(true)]);
    h.call_as(
        second,
        "mindmap:recovery-copy",
        vec![obj([("bytes", bytes("XMIND-2"))])],
    );
    assert_eq!(h.copies().len(), 2);

    let (saving, then) = slot::<bool>();
    h.editor(1).request_save(ViewId(1), SaveMode::SaveAs, then);
    h.app.queue_seed(ViewId(1), Seed::Outline("# a".into()));
    let (reading, then) = slot::<String>();
    h.editor(1).read_text(ViewId(1), then);
    assert!(saving.borrow().is_none() && reading.borrow().is_none());

    h.kit.destroy(ViewId(1));
    assert_eq!(saving.borrow().clone(), Some(Ok(false)));
    assert_eq!(
        reading.borrow().clone().unwrap().unwrap_err().message,
        "the map is no longer open"
    );
    // the clean view took its copy along; the unsaved one left it for the next launch
    assert_eq!(h.copies().len(), 1);
    assert_eq!(h.ok("mindmap:consume-pending", vec![]), IpcValue::Null);
    assert_eq!(h.ok("mindmap:seed-take", vec![]), IpcValue::Null);
    assert!(h.app.recovery_identity(ViewId(1)).is_none());
    assert!(h.app.recovery_identity(ViewId(2)).is_some());
    assert!(h.app.take_seed(ViewId(1)).is_none());
    h.kit.destroy(ViewId(2));
    assert_eq!(h.copies().len(), 1);
}

#[test]
fn the_outline_and_bytes_of_the_map_come_from_the_renderer_and_are_asked_again() {
    let h = plain("reads");
    h.app.create_view(ViewId(1), None);
    let editor = h.editor(1);
    let (outline, then) = slot::<String>();
    editor.read_text(ViewId(1), then);
    assert_eq!(h.events("mindmap:outline-request").len(), 1);
    // a reply that is not an answer changes nothing; an unanswered request is sent again
    h.ok("mindmap:outline-result", vec![string("nope")]);
    h.ok(
        "mindmap:outline-result",
        vec![obj([("other", string("x"))])],
    );
    assert!(outline.borrow().is_none());
    h.app.run_due(Instant::now() + Duration::from_millis(300));
    assert_eq!(h.events("mindmap:outline-request").len(), 1);
    h.ok(
        "mindmap:outline-result",
        vec![obj([("text", string("# map"))])],
    );
    assert_eq!(outline.borrow().clone(), Some(Ok("# map".to_owned())));

    let (failed, then) = slot::<String>();
    editor.read_text(ViewId(1), then);
    h.ok(
        "mindmap:outline-result",
        vec![obj([("error", string("boom"))])],
    );
    assert_eq!(
        failed.borrow().clone().unwrap().unwrap_err().message,
        "boom"
    );

    let (timed_out, then) = slot::<String>();
    editor.read_text(ViewId(1), then);
    h.app.run_due(Instant::now() + Duration::from_secs(31));
    assert_eq!(
        timed_out.borrow().clone().unwrap().unwrap_err().message,
        "timed out reading the map"
    );

    type Bytes = Rc<RefCell<Option<Result<Vec<u8>, String>>>>;
    let got: Bytes = Rc::default();
    let keep = got.clone();
    h.app
        .read_bytes(ViewId(1), Box::new(move |r| *keep.borrow_mut() = Some(r)));
    assert_eq!(h.events("mindmap:bytes-request").len(), 1);
    h.ok(
        "mindmap:bytes-result",
        vec![obj([("bytes", bytes("XMIND-live"))])],
    );
    assert_eq!(got.borrow().clone(), Some(Ok(b"XMIND-live".to_vec())));
}

#[test]
fn a_new_tab_takes_its_handoff_and_seed_once_and_a_file_tab_never() {
    let h = harness("handoff", |_| {
        json!({"mindmap": {"handoff": {"id": "h1", "text": "do it",
            "materials": [{"path": "/a.md", "name": "a.md"}]}}})
    });
    h.app.create_view(ViewId(1), None);
    let taken = h.ok("mindmap:handoff-take", vec![]);
    assert_eq!(
        taken,
        obj([
            ("id", string("h1")),
            ("sourcePaths", IpcValue::Array(vec![string("/a.md")])),
            ("prompt", string("do it")),
        ])
    );
    assert_eq!(h.ok("mindmap:handoff-take", vec![]), IpcValue::Null);
    let report = |id: &str| {
        obj([
            ("id", string(id)),
            (
                "attachedPaths",
                IpcValue::Array(vec![string("/a.md"), string(""), IpcValue::Number(3.0)]),
            ),
        ])
    };
    h.ok("mindmap:handoff-run-started", vec![report("other")]);
    assert!(
        h.logged("generate-handoff.reportGenerateRunStarted")
            .is_empty()
    );
    h.ok("mindmap:handoff-run-started", vec![report("h1")]);
    assert_eq!(
        h.logged("generate-handoff.reportGenerateRunStarted"),
        vec![json!([
            "generate-handoff.reportGenerateRunStarted",
            "mindmap",
            1,
            ["/a.md"]
        ])]
    );

    h.app.queue_seed(ViewId(1), Seed::Outline("# x".into()));
    assert_eq!(
        h.ok("mindmap:seed-take", vec![]),
        obj([("outline", string("# x"))])
    );
    assert_eq!(h.ok("mindmap:seed-take", vec![]), IpcValue::Null);

    // a view made for a file is not a new map
    let file = h.dir.join("f.xmind");
    fs::write(&file, "XMIND").unwrap();
    let second = h.kit.view(2, Surface::Mindmap);
    h.app.create_view(ViewId(2), Some(&path_str(&file)));
    h.app.queue_seed(ViewId(2), Seed::Copy(b"XMIND".to_vec()));
    assert_eq!(
        h.call_as(second, "mindmap:seed-take", vec![]),
        IpcValue::Null
    );
    assert_eq!(
        h.call_as(second, "mindmap:handoff-take", vec![]),
        IpcValue::Null
    );
}

#[test]
fn owned_seed_rollback_fences_replacement_and_accepts_consumption() {
    let h = plain("owned-seed-rollback");
    h.app.create_view(h.sender.view, None);
    let effects = h.kit.host::<shell_contract::hosts::OwnedEffectsHost>();
    let generation = h
        .kit
        .services
        .views
        .renderer_generation(h.sender.view)
        .unwrap();
    let mut owner = effects.capture_owner(h.sender.view, &generation).unwrap();
    owner.context = owner
        .context
        .with_authority(shell_contract::hosts::synthetic::SyntheticNativeOrigin::new());
    let (issuer, reservation) =
        shell_contract::ipc::NativeClosureIssuer::new(h.sender.view, generation.clone());
    let cancel = shell_contract::runtime::CancelToken::default();
    let queue = |text: &str| {
        h.app
            .queue_seed_owned(
                h.sender.view,
                Seed::Outline(text.into()),
                owner.clone(),
                &[],
                cancel.clone(),
                cancel.clone(),
                generation.clone(),
                reservation.clone(),
                &mut || Ok(()),
            )
            .unwrap()
    };

    let first = queue("# same");
    let replacement = queue("# same");
    assert!(!first.was_consumed());
    assert!(!replacement.was_consumed());
    assert!(first.rollback().is_err());
    replacement.rollback().unwrap();
    first.rollback().unwrap();
    first.rollback().unwrap();

    let consumed = queue("# consumed");
    let response = crate::testkit::start_with_owner(
        &h.reg,
        h.sender,
        "mindmap:seed-take",
        vec![],
        &owner,
        cancel.clone(),
    );
    assert_eq!(
        response.try_recv().unwrap().unwrap(),
        obj([("outline", string("# consumed"))])
    );
    assert!(consumed.was_consumed());
    consumed.rollback().unwrap();
    consumed.rollback().unwrap();

    let pending = shell_contract::runtime::CancelToken::default();
    pending.cancel();
    let mut initialized = false;
    assert!(
        h.app
            .queue_seed_owned(
                h.sender.view,
                Seed::Outline("# denied".into()),
                owner.clone(),
                &[],
                cancel.clone(),
                pending,
                generation.clone(),
                reservation.clone(),
                &mut || {
                    initialized = true;
                    Ok(())
                },
            )
            .is_err()
    );
    assert!(!initialized);

    let displaced = queue("# displaced");
    let failed = queue("# staged");
    let (forged, _) =
        shell_contract::ipc::NativeClosureIssuer::new(h.sender.view, generation.clone());
    assert!(failed.retire_closed(&forged.closed()).is_err());
    let closed = issuer.closed();
    h.kit.destroy(h.sender.view);
    assert!(failed.rollback().is_err());
    failed.retire_closed(&closed).unwrap();
    failed.rollback().unwrap();
    displaced.rollback().unwrap();
}

#[test]
fn prepared_seed_requires_exact_browser_and_preserves_prior_seed_on_failure() {
    let h = plain("prepared-seed-browser");
    h.app.create_view(h.sender.view, None);
    let effects = h.kit.host::<shell_contract::hosts::OwnedEffectsHost>();
    let generation = h
        .kit
        .services
        .views
        .renderer_generation(h.sender.view)
        .unwrap();
    let owner = effects.capture_owner(h.sender.view, &generation).unwrap();
    let (_, reservation, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(h.sender.view, generation.clone());
    let (_, foreign, foreign_browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(h.sender.view, generation.clone());
    let cancel = shell_contract::runtime::CancelToken::default();
    let prepare = |text: &str| {
        h.app
            .prepare_seed_in_transition(
                h.sender.view,
                Seed::Outline(text.into()),
                owner.clone(),
                cancel.clone(),
                generation.clone(),
                reservation.clone(),
                browser.clone(),
            )
            .unwrap()
    };
    let mut initialized = 0;
    assert!(
        foreign
            .with_initial_owner(&foreign_browser, |transition| prepare("denied").commit(
                transition,
                &mut || {
                    initialized += 1;
                    Ok(())
                }
            ))
            .is_err()
    );
    assert_eq!(initialized, 0);
    let prior = reservation
        .with_initial_owner(&browser, |transition| {
            prepare("prior").commit(transition, &mut || Ok(()))
        })
        .unwrap();
    assert!(
        reservation
            .with_owner(&browser, |transition| prepare("failed")
                .commit(transition, &mut || Err("rejected".into())))
            .is_err()
    );
    let replacement = reservation
        .with_owner(&browser, |transition| {
            prepare("new").commit(transition, &mut || {
                initialized += 1;
                Ok(())
            })
        })
        .unwrap();
    assert_eq!(initialized, 1);
    assert!(prior.rollback().is_err());
    replacement.rollback().unwrap();
    prior.rollback().unwrap();
}

#[test]
fn superseded_mindmap_seed_is_not_restored_by_successor_rollback() {
    let h = plain("prepared-seed-superseded");
    h.app.create_view(h.sender.view, None);
    let effects = h.kit.host::<shell_contract::hosts::OwnedEffectsHost>();
    let generation = h
        .kit
        .services
        .views
        .renderer_generation(h.sender.view)
        .unwrap();
    let source = effects.capture_owner(h.sender.view, &generation).unwrap();
    let (_, reservation, browser) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(h.sender.view, generation.clone());
    let cancel = shell_contract::runtime::CancelToken::default();
    let first = h
        .app
        .prepare_seed_in_transition(
            h.sender.view,
            Seed::Outline("first".into()),
            source.clone(),
            cancel.clone(),
            generation.clone(),
            reservation.clone(),
            browser.clone(),
        )
        .unwrap();
    let first = reservation
        .with_initial_owner(&browser, |transition| {
            first.commit(transition, &mut || Ok(()))
        })
        .unwrap();
    let replacement = reservation.prepare_replacement(&browser).unwrap();
    let second = h
        .app
        .prepare_seed_in_transition(
            h.sender.view,
            Seed::Outline("second".into()),
            source,
            cancel,
            generation,
            reservation.clone(),
            replacement.successor().clone(),
        )
        .unwrap();
    let (_, proof, second) = replacement
        .commit(|transition, _| second.commit(transition, &mut || Ok(())))
        .unwrap();
    first.retire_superseded(&proof).unwrap();
    second.rollback().unwrap();
    assert!(
        !h.app
            .0
            .state
            .borrow()
            .owned_seeds
            .contains_key(&h.sender.view)
    );
}

#[test]
fn adopted_seed_close_never_restores_displaced_content() {
    let h = plain("owned-seed-adopted-close");
    h.app.create_view(h.sender.view, None);
    let effects = h.kit.host::<shell_contract::hosts::OwnedEffectsHost>();
    let generation = h
        .kit
        .services
        .views
        .renderer_generation(h.sender.view)
        .unwrap();
    let owner = effects.capture_owner(h.sender.view, &generation).unwrap();
    let (issuer, reservation) =
        shell_contract::ipc::NativeClosureIssuer::new(h.sender.view, generation.clone());
    let cancel = shell_contract::runtime::CancelToken::default();
    let queue = |text: &str| {
        h.app
            .queue_seed_owned(
                h.sender.view,
                Seed::Outline(text.into()),
                owner.clone(),
                &[],
                cancel.clone(),
                cancel.clone(),
                generation.clone(),
                reservation.clone(),
                &mut || Ok(()),
            )
            .unwrap()
    };
    let displaced = queue("# old");
    let adopted = queue("# adopted");
    let (forged, _) =
        shell_contract::ipc::NativeClosureIssuer::new(h.sender.view, generation.clone());
    assert!(adopted.retire_adopted(&forged.closed()).is_err());
    let closed = issuer.closed();
    adopted.retire_adopted(&closed).unwrap();
    adopted.retire_adopted(&closed).unwrap();
    assert!(displaced.rollback().is_err());
}

#[test]
fn attachments_are_judged_by_extension_and_size_and_read_in_pages() {
    let h = harness(
        "attach",
        |dir| json!({"mindmap": {"unsaved": [path_str(&dir.join("open.txt"))]}}),
    );
    h.app.create_view(ViewId(1), None);
    let text = h.dir.join("n.txt");
    fs::write(&text, "0123456789").unwrap();
    let zip = h.dir.join("x.zip");
    fs::write(&zip, "zz").unwrap();
    let added = h.ok(
        "mindmap:files-add",
        vec![IpcValue::Array(vec![
            string(path_str(&text)),
            string(path_str(&zip)),
            IpcValue::Number(1.0),
        ])],
    );
    assert_eq!(
        added,
        obj([
            (
                "accepted",
                IpcValue::Array(vec![obj([
                    ("path", string(path_str(&text))),
                    ("name", string("n.txt")),
                    ("ext", string("txt")),
                    ("sizeBytes", IpcValue::Number(10.0)),
                ])])
            ),
            (
                "rejected",
                IpcValue::Array(vec![string("x.zip: .zip files are not supported")])
            ),
        ])
    );

    let page = h.ok(
        "mindmap:files-read",
        vec![
            string(path_str(&text)),
            IpcValue::Number(2.0),
            IpcValue::Number(3.0),
        ],
    );
    assert_eq!(
        page,
        obj([
            ("ok", IpcValue::Bool(true)),
            ("name", string("n.txt")),
            ("totalChars", IpcValue::Number(10.0)),
            ("offset", IpcValue::Number(2.0)),
            ("text", string("234")),
        ])
    );
    // the second page comes from the cache: the file is parsed once
    h.ok(
        "mindmap:files-read",
        vec![
            string(path_str(&text)),
            IpcValue::Number(5.0),
            IpcValue::Number(99.0),
        ],
    );
    assert_eq!(h.logged("file-parse.parseFileToText").len(), 1);

    // a file open with unsaved changes is not read
    let open = h.dir.join("open.txt");
    fs::write(&open, "x").unwrap();
    let refused = h.ok(
        "mindmap:files-read",
        vec![
            string(path_str(&open)),
            IpcValue::Number(0.0),
            IpcValue::Number(5.0),
        ],
    );
    assert_eq!(
        error_of(&refused),
        "This file is open with unsaved changes. Save it, then try again."
    );

    // pictures: pasted bytes land in the temp folder; the others are refused
    let pasted = h.ok(
        "mindmap:files-add-pasted-image",
        vec![bytes("png-bytes"), string("png")],
    );
    let accepted = match field(&pasted, "accepted") {
        IpcValue::Array(items) => items.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(accepted.len(), 1);
    let pasted_path = match field(&accepted[0], "path") {
        IpcValue::String(path) => PathBuf::from(path),
        other => panic!("{other:?}"),
    };
    assert!(pasted_path.starts_with(h.dir.join("temp").join("seuoffice-pasted")));
    let image = h.ok(
        "mindmap:files-read-image",
        vec![string(path_str(&pasted_path))],
    );
    assert_eq!(field(&image, "mime"), &string("image/png"));
    let refused = h.ok(
        "mindmap:files-add-pasted-image",
        vec![bytes("x"), string("exe")],
    );
    assert_eq!(
        field(&refused, "rejected"),
        &IpcValue::Array(vec![string("not a supported image type")])
    );

    // the panel's first request names an untitled map
    h.ok("mindmap:provisional-title", vec![string("  My   plan ")]);
    assert_eq!(
        h.logged("ai-attachments.provisionalTitleHook"),
        vec![json!(["ai-attachments.provisionalTitleHook", 1, "My plan"])]
    );
}

#[test]
fn a_link_preview_reads_the_page_and_its_picture_and_links_open_by_protocol() {
    let h = harness("preview", |_| {
        json!({"mindmap": {
            "dns": {
                "example.com": [{"address": "93.184.216.34", "family": 4}],
                "inside.example": [{"address": "10.0.0.5", "family": 4}]},
            "network": {
                "https://example.com/p": {
                    "headers": {"content-type": "text/html; charset=utf-8"},
                    "body": "<title>T</title><meta property=\"og:image\" content=\"/i.png\">"},
                "https://example.com/i.png": {
                    "headers": {"content-type": "image/png"}, "body": "A"},
                "https://inside.example/p": {"headers": {"content-type": "text/html"}}}}})
    });
    h.app.create_view(ViewId(1), None);
    let preview = h.ok(
        "mindmap:read-link-preview",
        vec![string("https://example.com/p")],
    );
    assert_eq!(
        preview,
        obj([
            ("ok", IpcValue::Bool(true)),
            ("title", string("T")),
            (
                "picture",
                obj([("base64", string("QQ==")), ("mime", string("image/png"))])
            ),
        ])
    );
    let reason = |url: &str| {
        field(
            &h.ok("mindmap:read-link-preview", vec![string(url)]),
            "reason",
        )
        .clone()
    };
    assert_eq!(reason("ftp://example.com/p"), string("not-web"));
    assert_eq!(reason("https://example.com/none"), string("unreachable"));
    // a host that resolves into the intranet is never asked
    assert_eq!(reason("https://inside.example/p"), string("unreachable"));
    assert!(
        !h.logged("undici.fetch")
            .iter()
            .any(|line| line[1] == "https://inside.example/p")
    );

    assert_eq!(
        h.ok(
            "mindmap:fetch-image",
            vec![string("https://example.com/i.png")]
        ),
        obj([("base64", string("QQ==")), ("mime", string("image/png"))])
    );
    assert_eq!(
        h.ok("mindmap:fetch-image", vec![IpcValue::Number(1.0)]),
        IpcValue::Null
    );

    assert_eq!(
        h.ok("mindmap:open-link", vec![string("javascript:alert(1)")]),
        IpcValue::Bool(false)
    );
    assert_eq!(
        h.ok("mindmap:open-link", vec![string("mailto:a@b.c")]),
        IpcValue::Bool(true)
    );
    assert!(
        h.kit
            .os_shell
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l == "openExternal mailto:a@b.c")
    );
}

#[test]
fn a_new_map_goes_into_its_project_only_when_the_shell_has_one() {
    let without = plain("project-none");
    without.app.create_view(ViewId(1), None);
    let request = obj([("bytes", bytes("XMIND-1")), ("defaultName", string("Plan"))]);
    assert_eq!(
        without.ok("mindmap:save-into-project", vec![request.clone()]),
        obj([
            ("ok", IpcValue::Bool(false)),
            ("skipped", IpcValue::Bool(true))
        ])
    );

    let with = harness(
        "project-some",
        |dir| json!({"mindmap": {"writeNewDocument": {"path": path_str(&dir.join("proj").join("Plan.xmind"))}}}),
    );
    with.app.create_view(ViewId(1), None);
    let saved = with.ok("mindmap:save-into-project", vec![request]);
    let written = with.dir.join("proj").join("Plan.xmind");
    assert_eq!(
        saved,
        obj([
            ("ok", IpcValue::Bool(true)),
            ("path", string(path_str(&written)))
        ])
    );
    assert_eq!(
        with.logged("mindmap-main.writeNewDocument"),
        vec![json!(["mindmap-main.writeNewDocument", 1, "Plan.xmind", 7])]
    );
    assert_eq!(with.app.file_path(ViewId(1)), Some(path_str(&written)));
    // the map has a file now
    assert_eq!(
        with.ok(
            "mindmap:save-into-project",
            vec![obj([("bytes", bytes("XMIND-1"))])]
        ),
        obj([
            ("ok", IpcValue::Bool(false)),
            ("skipped", IpcValue::Bool(true))
        ])
    );
}

#[test]
fn an_imported_freemind_map_is_read_and_never_written() {
    let h = plain("freemind");
    let mm = h.dir.join("Ideas.MM");
    fs::write(&mm, "<map/>").unwrap();
    h.app.create_view(ViewId(1), Some(&path_str(&mm)));
    let opened = h.ok("mindmap:open-file", vec![string(path_str(&mm))]);
    assert_eq!(
        opened,
        obj([
            ("status", string("import")),
            ("importer", string("freemind")),
            ("bytes", bytes("<map/>")),
            ("title", string("Ideas")),
        ])
    );
    assert_eq!(h.app.file_path(ViewId(1)), None);
    // its first save is offered an .xmind beside the .mm
    h.pick_save(None);
    h.ok("mindmap:save", vec![save_request("XMIND-1", "save")]);
    let dialog_log = h.kit.take_log().join("\n");
    assert!(
        dialog_log.contains(&path_str(&h.dir.join("Ideas.xmind"))),
        "{dialog_log}"
    );
    assert_eq!(fs::read_to_string(&mm).unwrap(), "<map/>");
}

#[test]
fn a_map_open_in_two_panes_asks_before_overwriting_the_sibling_s_write() {
    let h = harness(
        "split",
        |dir| json!({"mindmap": {"openElsewhere": [path_str(&dir.join("m.xmind"))]}}),
    );
    let file = open_granted_file(&h, "m.xmind", "XMIND-1");
    // the sibling pane writes the file
    fs::write(&file, "XMIND-sibling-longer").unwrap();
    // an automatic save defers silently
    let auto = obj([
        ("bytes", bytes("XMIND-2")),
        ("mode", string("save")),
        ("auto", IpcValue::Bool(true)),
    ]);
    assert_eq!(
        error_of(&h.ok("mindmap:save", vec![auto])),
        "external-modified"
    );
    // a manual save asks: keeping the disk copy (Cancel, index 1) stops it, Overwrite goes on
    h.answer_message(1);
    assert_eq!(
        error_of(&h.ok("mindmap:save", vec![save_request("XMIND-2", "save")])),
        "external-modified"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "XMIND-sibling-longer");
    h.answer_message(0);
    assert!(is_ok(
        &h.ok("mindmap:save", vec![save_request("XMIND-2", "save")])
    ));
    assert_eq!(fs::read_to_string(&file).unwrap(), "XMIND-2");
}

#[test]
fn renamed_files_keep_their_grants_and_tell_the_renderer() {
    let h = plain("renamed");
    let file = open_granted_file(&h, "m.xmind", "XMIND-1");
    let moved = h.dir.join("n.xmind");
    h.editor(1).file_renamed(ViewId(1), &file, &moved);
    assert_eq!(h.editor(1).document_path(ViewId(1)), Some(moved.clone()));
    assert_eq!(
        h.ok("mindmap:consume-pending", vec![]),
        string(path_str(&moved))
    );
    assert_eq!(
        h.events("mindmap:file-renamed"),
        vec![vec![string(path_str(&moved))]]
    );
    h.editor(1).export_request(ViewId(1), "pdf");
    let events = h.kit.take_events();
    assert!(events.contains(&(
        Target::View(ViewId(1)),
        "mindmap:export-request",
        vec![string("pdf")]
    )));
}

/// A mind map renderer: its view plus the native origin its calls carry
struct Renderer {
    sender: Sender,
    origin: shell_contract::ipc::NativeRequestOwner,
}

impl Renderer {
    fn new(h: &H, id: u32) -> Renderer {
        let sender = h.kit.view(id, Surface::Mindmap);
        h.app.create_view(sender.view, None);
        let generation = h
            .kit
            .services
            .views
            .renderer_generation(sender.view)
            .unwrap();
        let origin = h
            .kit
            .host::<Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .capture_invocation_root(
                sender.view,
                &generation,
                &shell_contract::hosts::synthetic::SyntheticNativeOrigin::new(),
            )
            .unwrap();
        Renderer { sender, origin }
    }

    fn call(&self, h: &H, channel: &str, args: Vec<IpcValue>) -> Result<IpcValue, IpcError> {
        crate::testkit::start_with_owner(
            &h.reg,
            self.sender,
            channel,
            args,
            &self.origin,
            shell_contract::runtime::CancelToken::default(),
        )
        .try_recv()
        .unwrap()
    }

    fn acknowledge(&self, h: &H, token: &str, ok: bool) -> Result<IpcValue, IpcError> {
        self.call(
            h,
            "mindmap:recovery-initialized",
            vec![string(token), IpcValue::Bool(ok)],
        )
    }
}

/// A recovery file the shell committed and the renderer opened: the renderer
/// now owes `mindmap:recovery-initialized` with `token`
struct Recovering {
    renderer: Renderer,
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    token: String,
    // the closure and target must outlive the wait
    _issuer: shell_contract::ipc::NativeClosureIssuer,
    _target: shell_contract::ipc::RecoveryOpenTarget,
}

/// Mind map view `id` on a recovery file, and the target the host hands the editor for it. The
/// browser's owner is not committed yet: the host does that before the hand-over.
fn recovery_target(
    h: &H,
    id: u32,
    name: &str,
) -> (
    Renderer,
    String,
    shell_contract::ipc::NativeClosureIssuer,
    shell_contract::ipc::RecoveryOpenTarget,
) {
    use shell_contract::ipc::{RecoveryOpenContext, RecoveryOpenTarget};
    use shell_contract::runtime::CancelToken;
    let file = h.dir.join(name);
    fs::write(&file, "XMIND-1").unwrap();
    let renderer = Renderer::new(h, id);
    let view = renderer.sender.view;
    let generation = h.kit.services.views.renderer_generation(view).unwrap();
    let cancel = CancelToken::default();
    let (issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(view, generation.clone());
    let target = RecoveryOpenTarget::bind(
        RecoveryOpenContext::Renderer {
            source: renderer.origin.clone(),
            participants: vec![],
            cancel: cancel.clone(),
            pending: cancel,
        },
        view,
        generation,
        closure,
        browser_owner,
    )
    .unwrap();
    (renderer, path_str(&file), issuer, target)
}

/// The shell's recovery open of `path` into the target's view; whether the editor took it
fn start_recovery(h: &H, path: &str, target: &shell_contract::ipc::RecoveryOpenTarget) -> bool {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    let done = Rc::new(RefCell::new(None));
    let completed = done.clone();
    h.app.open_file_with_decision(
        target.view(),
        &RecoveryOpenFileRequest {
            file_path: path.to_owned(),
            decision: RecoveryOpenDecision::Original,
        },
        target,
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    done.borrow_mut().take().unwrap().is_ok()
}

/// Drives the recovery open of mind map view `id` through the real handlers
/// up to the token the renderer must acknowledge
fn recover(h: &H, id: u32, name: &str) -> Recovering {
    let (renderer, path, issuer, target) = recovery_target(h, id, name);
    // the host commits the browser's owner before it hands the target to the editor
    target
        .closure_reservation()
        .with_initial_owner(target.browser_owner(), |_| Ok(()))
        .unwrap();
    assert!(start_recovery(h, &path, &target));
    let receipt = target.committed_seed().unwrap();
    target
        .retain_native_registration(renderer.origin.clone())
        .unwrap();
    assert_eq!(
        renderer.call(h, "mindmap:consume-pending", vec![]).unwrap(),
        string(path.clone())
    );
    let opened = renderer
        .call(h, "mindmap:open-file", vec![string(path)])
        .unwrap();
    assert_eq!(field(&opened, "status"), &string("ok"));
    let IpcValue::String(token) = field(&opened, "recoveryToken").clone() else {
        panic!("no recovery token in {opened:?}");
    };
    assert!(receipt.settlement().is_none());
    Recovering {
        renderer,
        receipt,
        token,
        _issuer: issuer,
        _target: target,
    }
}

/// The host commits the browser's owner when it makes the view, and a recovery is staged only
/// under that owner. A browser nobody committed, and one whose owner was replaced after the
/// host committed it, are refused: no seed, no pending recovery, no request number taken.
#[test]
fn a_recovery_is_staged_only_under_the_browser_owner_the_host_committed() {
    let h = plain("mindmap-recovery-owner");
    let staged = |view: ViewId| {
        let state = h.app.0.state.borrow();
        (state.recovery_preparing.contains_key(&view), state.next_gen)
    };

    let (renderer, path, _issuer, target) = recovery_target(&h, 21, "uncommitted.xmind");
    let before = staged(renderer.sender.view);
    assert!(!start_recovery(&h, &path, &target));
    assert!(target.committed_seed().is_none());
    assert_eq!(staged(renderer.sender.view), before);
    assert!(!before.0);

    let (renderer, path, _issuer, target) = recovery_target(&h, 22, "replaced.xmind");
    let reservation = target.closure_reservation();
    reservation
        .with_initial_owner(target.browser_owner(), |_| Ok(()))
        .unwrap();
    let _successor = reservation
        .prepare_replacement(target.browser_owner())
        .unwrap()
        .commit(|_, _| Ok(()))
        .unwrap();
    let before = staged(renderer.sender.view);
    assert!(!start_recovery(&h, &path, &target));
    assert!(target.committed_seed().is_none());
    assert_eq!(staged(renderer.sender.view), before);
    assert!(!before.0);

    // the same start under the owner the host committed is taken
    let committed = recover(&h, 23, "committed.xmind");
    assert!(committed.receipt.settlement().is_none());
}

#[test]
fn native_evidence_mindmap_recovery_initialized_settles_only_the_exact_ack() {
    let h = plain("mindmap-recovery-exact");
    let a = recover(&h, 11, "a.xmind");
    let b = recover(&h, 12, "b.xmind");
    let bystander = Renderer::new(&h, 13);
    let message = |result: Result<IpcValue, IpcError>| result.unwrap_err().message;

    // missing-receipt: no token at all, a wrong argument type, and an
    // acknowledgement from a view with no recovery pending
    let no_token = message(a.renderer.call(&h, "mindmap:recovery-initialized", vec![]));
    assert!(no_token.contains("token is required"), "{no_token}");
    let no_text_token = message(a.renderer.call(
        &h,
        "mindmap:recovery-initialized",
        vec![IpcValue::Bool(true)],
    ));
    assert!(
        no_text_token.contains("token is required"),
        "{no_text_token}"
    );
    let nothing_pending = message(bystander.acknowledge(&h, "any-token", true));
    assert!(
        nothing_pending.contains("No map recovery is awaiting"),
        "{nothing_pending}"
    );
    assert!(a.receipt.settlement().is_none() && b.receipt.settlement().is_none());

    // foreign-receipt: an unknown token, and the token of the other view's wait
    let unknown = message(a.renderer.acknowledge(&h, "stale-token", true));
    assert!(unknown.contains("belongs to another map"), "{unknown}");
    let other_wait = message(b.renderer.acknowledge(&h, &a.token, true));
    assert!(
        other_wait.contains("belongs to another map"),
        "{other_wait}"
    );
    assert!(a.receipt.settlement().is_none() && b.receipt.settlement().is_none());

    // foreign-renderer: a mind map view that was never handed the recovery
    // copy cannot settle it, even with the exact token
    let foreign = message(bystander.acknowledge(&h, &a.token, true));
    assert!(foreign.contains("No map recovery is awaiting"), "{foreign}");
    assert!(a.receipt.settlement().is_none() && b.receipt.settlement().is_none());

    // successful-ack: the exact acknowledgement settles only its own wait
    assert_eq!(
        a.renderer.acknowledge(&h, &a.token, true).unwrap(),
        IpcValue::Null
    );
    assert!(matches!(a.receipt.settlement(), Some(Ok(()))));
    assert!(b.receipt.settlement().is_none());

    // duplicate-ack: a second acknowledgement changes nothing
    let duplicate = message(a.renderer.acknowledge(&h, &a.token, false));
    assert!(
        duplicate.contains("No map recovery is awaiting"),
        "{duplicate}"
    );
    assert!(matches!(a.receipt.settlement(), Some(Ok(()))));
    assert!(b.receipt.settlement().is_none());

    shell_contract::native_evidence::record(
        "mindmap:recovery-initialized",
        &[
            "successful-ack",
            "missing-receipt",
            "foreign-receipt",
            "foreign-renderer",
            "duplicate-ack",
        ],
        &json!({
            "missing-receipt": {"noToken": no_token, "nonTextToken": no_text_token, "nothingPending": nothing_pending, "settled": false},
            "foreign-receipt": {"unknownToken": unknown, "otherWaitsToken": other_wait, "settled": false},
            "foreign-renderer": {"error": foreign, "settled": false},
            "successful-ack": {"answer": null, "settlement": "ok", "otherWaitSettled": false},
            "duplicate-ack": {"error": duplicate, "settlement": "ok"},
        })
        .to_string(),
    );
}

#[test]
fn native_evidence_mindmap_recovery_initialized_failed_apply_settles_the_wait_as_failure() {
    let h = plain("mindmap-recovery-failed");
    let a = recover(&h, 21, "a.xmind");
    let failure = "the renderer could not apply the recovery copy";
    assert_eq!(
        a.renderer
            .call(
                &h,
                "mindmap:recovery-initialized",
                vec![string(&a.token), IpcValue::Bool(false), string(failure)],
            )
            .unwrap(),
        IpcValue::Null
    );
    assert_eq!(a.receipt.settlement(), Some(Err(failure.to_owned())));
    // the wait is cleaned up: a later success claim cannot revive it
    assert!(a.renderer.acknowledge(&h, &a.token, true).is_err());
    assert_eq!(a.receipt.settlement(), Some(Err(failure.to_owned())));
    shell_contract::native_evidence::record(
        "mindmap:recovery-initialized",
        &["failed-apply"],
        &json!({"failed-apply": {"answer": null, "settlement": {"error": failure}, "lateSuccessRefused": true}})
            .to_string(),
    );
}

#[test]
fn native_evidence_mindmap_recovery_initialized_is_refused_to_other_surfaces() {
    let dir = temp_dir("mindmap-recovery-surface");
    let kit = Kit::new(&dir, crate::insert_fakes);
    let mut table = shell_core::table::Table::default();
    crate::register(&mut table, &kit.services).unwrap();
    let dispatcher = shell_core::dispatch::Dispatcher::new(table);
    let mut refused = serde_json::Map::new();
    for (id, surface) in [(31, Surface::Docs), (32, Surface::Diagram)] {
        let foreign = kit.view(id, surface);
        let (send, receive) = std::sync::mpsc::channel();
        dispatcher.dispatch(
            foreign,
            shell_contract::ipc::ChannelKind::Invoke,
            Some(shell_contract::RequestId(1)),
            None,
            "mindmap:recovery-initialized",
            vec![string("token"), IpcValue::Bool(true)],
            Box::new(move |result| send.send(result).unwrap()),
        );
        let message = receive.try_recv().unwrap().unwrap_err().message;
        assert!(
            message.contains(&format!("not open to {surface:?}")),
            "{message}"
        );
        refused.insert(format!("{surface:?}"), message.into());
    }
    shell_contract::native_evidence::record(
        "mindmap:recovery-initialized",
        &["unauthorized-surface"],
        &json!({"unauthorized-surface": refused}).to_string(),
    );
}

/// A saved map opens in a new view of the editor (crates/shell, w_tabs.rs
/// `open_in_path_editor`): the editor takes the saved token only on a view it
/// created, and the shell creates the view in it on no document, as here,
/// before the token
#[cfg(unix)]
#[test]
fn a_saved_map_token_needs_the_view_created_in_the_editor() {
    use shell_contract::hosts::OwnedEffectsHost;
    use shell_contract::ipc::{
        NativeClosureIssuer, NativeInvocationIssuer, NativeSaveDestination, NativeSaveSelection,
        NativeSavedInput, NativeSavedPath, NativeSavedTarget,
    };
    use shell_contract::runtime::CancelToken;
    use std::ffi::CString;

    let h = plain("saved-token-needs-view");
    h.app.create_view(h.sender.view, None);
    let editor = h
        .reg
        .provided
        .all::<dyn DocumentEditors>()
        .into_iter()
        .find(|editor| editor.saved_surface() == Surface::Mindmap)
        .unwrap();
    let effects = h.kit.host::<OwnedEffectsHost>();
    let generation = h
        .kit
        .services
        .views
        .renderer_generation(h.sender.view)
        .unwrap();
    let existing = editor
        .capture_document_owner(
            h.sender.view,
            effects.capture_owner(h.sender.view, &generation).unwrap(),
        )
        .unwrap();
    let path = h.dir.join("saved.xmind");
    fs::write(&path, b"old map").unwrap();
    let issuer = NativeInvocationIssuer::new(CancelToken::default());
    let binding = issuer
        .issue_existing_document_save(
            Surface::Mindmap,
            existing.view,
            NativeSaveSelection::ExactPath(path.clone()),
            existing.clone(),
            CancelToken::default(),
        )
        .unwrap();
    let selection = NativeSaveDestination::from_exact_selection(binding).unwrap();
    let parent = fs::File::open(path.parent().unwrap()).unwrap();
    let directory = file_commit::PrivateDirectory::create(&parent).unwrap();
    let stage = directory
        .create_stage(0o600)
        .unwrap()
        .quarantine()
        .ok()
        .unwrap();
    let stage = stage.write_owned(b"saved map".to_vec()).ok().unwrap();
    let name = CString::new(path.file_name().unwrap().to_str().unwrap()).unwrap();
    let stage = stage
        .prepare(&parent, &name, &path, file_commit::PublicationMode::Replace)
        .ok()
        .unwrap();
    let (publication, snapshot) = effects
        .publish_native_staged_write(
            &selection,
            &[],
            &CancelToken::default(),
            &CancelToken::default(),
            &[],
            stage,
        )
        .ok()
        .unwrap();
    let input = NativeSavedInput::from_written_snapshot(publication.clone(), snapshot).unwrap();

    let view = h.kit.view(2, Surface::Mindmap).view;
    let generation = h.kit.services.views.renderer_generation(view).unwrap();
    let (_issuer, reservation, browser) =
        NativeClosureIssuer::new_with_owner(view, generation.clone());
    let target = NativeSavedTarget::prepare(
        input,
        NativeSavedPath::Selected(publication),
        Surface::Mindmap,
        view,
        generation,
        reservation,
        browser,
        vec![],
        CancelToken::default(),
        CancelToken::default(),
        vec![],
    )
    .unwrap();
    assert!(
        editor.prepare_saved_document(target.clone()).is_err(),
        "a view the editor never created takes no saved token"
    );
    h.app.create_view(view, None);
    let prepared = editor
        .prepare_saved_document(target)
        .map(|_| ())
        .map_err(|error| error.message);
    assert_eq!(
        prepared,
        Ok(()),
        "the view created on no document takes the saved token"
    );
}
