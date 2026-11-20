//! What a diagram renderer can observe: answers, state across calls, events,
//! files, and the end of a view.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use shell_contract::editors::{SaveMode, editor_for};
use shell_contract::ipc::{Sender, Target};
use shell_contract::{IpcError, IpcValue, Surface, ViewId};

use super::views;
use crate::testkit::{Kit, TestRegistry, call, registered, temp_dir};
use crate::value::{buffer, field, num, obj, string};

const DIAGRAM: &str = "<mxfile><diagram id=\"1\" name=\"Page-1\"></diagram></mxfile>";

struct Setup {
    kit: Kit,
    registry: TestRegistry,
    dir: PathBuf,
}

fn setup(name: &str) -> Setup {
    let dir = temp_dir(name);
    let kit = Kit::new(&dir, crate::insert_fakes);
    let registry = registered(&kit);
    Setup { kit, registry, dir }
}

impl Setup {
    /// A diagram view `id` that opens `file` (written with `content`)
    fn tab(&self, id: u32, file: Option<(&str, &str)>) -> (Sender, Option<String>) {
        let sender = self.kit.view(id, Surface::Diagram);
        let path = file.map(|(name, content)| {
            let path = self.dir.join(name);
            std::fs::write(&path, content).unwrap();
            path.to_string_lossy().into_owned()
        });
        views().unwrap().open_view(ViewId(id), path.as_deref());
        (sender, path)
    }

    fn call(
        &self,
        sender: Sender,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<IpcValue, IpcError> {
        call(&self.registry, sender, channel, args)
    }

    fn ok(&self, sender: Sender, channel: &str, args: Vec<IpcValue>) -> IpcValue {
        self.call(sender, channel, args).unwrap()
    }

    fn copies(&self) -> Vec<PathBuf> {
        let root = self.dir.join("userdata").join("diagram-autosave");
        let mut found = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if !path.to_string_lossy().ends_with(".meta.json") {
                    found.push(path);
                }
            }
        }
        found
    }
}

fn text(value: &IpcValue, key: &str) -> String {
    match field(value, key) {
        IpcValue::String(text) => text.clone(),
        other => panic!("{key} is not a string: {other:?}"),
    }
}

fn is_true(value: &IpcValue, key: &str) -> bool {
    matches!(field(value, key), IpcValue::Bool(true))
}

fn bytes(text: &str) -> IpcValue {
    buffer(text.as_bytes().to_vec())
}

fn request(entries: [(&str, IpcValue); 2]) -> IpcValue {
    obj(entries)
}

/// A continuation that remembers its outcome
type Slot<T> = Rc<RefCell<Option<Result<T, IpcError>>>>;
type Then<T> = Box<dyn FnOnce(Result<T, IpcError>)>;

fn outcome<T: 'static>() -> (Slot<T>, Then<T>) {
    let slot = Rc::new(RefCell::new(None));
    let got = slot.clone();
    (
        slot,
        Box::new(move |result| *got.borrow_mut() = Some(result)),
    )
}

fn emitted(kit: &Kit, channel: &str) -> Vec<Vec<IpcValue>> {
    kit.take_events()
        .into_iter()
        .filter(|(_, name, _)| *name == channel)
        .map(|(_, _, args)| args)
        .collect()
}

#[test]
fn recovery_flush_rejects_stale_ack_and_reports_write_failure() {
    let s = setup("diagram-flush-result");
    let (tab, path) = s.tab(1, Some(("flow.drawio", DIAGRAM)));
    s.ok(tab, "diagram:open-file", vec![string(path.unwrap())]);
    s.ok(tab, "diagram:dirty-changed", vec![IpcValue::Bool(true)]);
    let first = Rc::new(RefCell::new(None));
    let sink = first.clone();
    views().unwrap().write_copies_now(
        std::time::Duration::from_secs(5),
        Box::new(move |result| *sink.borrow_mut() = Some(result)),
    );
    let old = emitted(&s.kit, "diagram:recovery-copy-now")[0][0].clone();
    let second = Rc::new(RefCell::new(None));
    let sink = second.clone();
    views().unwrap().write_copies_now(
        std::time::Duration::from_secs(5),
        Box::new(move |result| *sink.borrow_mut() = Some(result)),
    );
    let current = emitted(&s.kit, "diagram:recovery-copy-now")[0][0].clone();
    assert!(matches!(&*first.borrow(), Some(Err(_))));
    s.ok(
        tab,
        "diagram:recovery-copy-now-done",
        vec![old, IpcValue::Null],
    );
    assert!(second.borrow().is_none());
    s.ok(
        tab,
        "diagram:recovery-copy-now-done",
        vec![current, string("disk failed")],
    );
    assert_eq!(
        second.borrow().as_ref().unwrap().as_ref().unwrap_err().0,
        "disk failed"
    );
}
#[test]
fn attachment_read_waits_for_cross_editor_save_guard_without_blocking_core() {
    let dir = temp_dir("diagram-deferred-save-guard");
    let kit = Kit::new(&dir, |hosts| {
        crate::insert_fakes(hosts);
        super::insert_fakes(
            hosts,
            &serde_json::json!({
                "diagram": {"deferSaveGuard": true}
            }),
        );
    });
    let registry = registered(&kit);
    let s = Setup { kit, registry, dir };
    let (tab, _) = s.tab(1, None);
    let path = s.dir.join("note.txt");
    std::fs::write(&path, "secret").unwrap();
    let reply = crate::testkit::start(
        &s.registry,
        tab,
        "diagram:files-read",
        vec![string(path.to_string_lossy()), num(0.0), num(20.0)],
    );
    assert!(reply.try_recv().is_err());
    assert!(s.call(tab, "diagram:consume-pending", vec![]).is_ok());
    let guards = s.kit.host::<std::sync::Arc<super::fakes::DeferredGuards>>();
    let complete = guards.0.lock().unwrap().remove(0);
    complete(Err("unsaved changes".into()));
    let answer = reply.try_recv().unwrap().unwrap();
    assert_eq!(text(&answer, "error"), "unsaved changes");

    let reply = crate::testkit::start(
        &s.registry,
        tab,
        "diagram:files-read",
        vec![string(path.to_string_lossy()), num(0.0), num(20.0)],
    );
    assert!(reply.try_recv().is_err());
    let complete = guards.0.lock().unwrap().remove(0);
    complete(Ok(()));
    let answer = reply.try_recv().unwrap().unwrap();
    assert_eq!(text(&answer, "text"), "secret");
}

#[test]
fn engine_failure_rejects_open_instead_of_reporting_a_document_verdict() {
    let dir = temp_dir("diagram-engine-open");
    let kit = Kit::new(&dir, |hosts| {
        crate::insert_fakes(hosts);
        super::insert_fakes(
            hosts,
            &serde_json::json!({
                "diagram": {"openDiagram": {"broken.drawio": {"error": "engine unavailable"}}}
            }),
        );
    });
    let registry = registered(&kit);
    let s = Setup { kit, registry, dir };
    let (tab, path) = s.tab(1, Some(("broken.drawio", DIAGRAM)));
    let path = path.unwrap();
    let error = s
        .call(tab, "diagram:open-file", vec![string(&path)])
        .unwrap_err();
    assert_eq!(error.message, "engine unavailable");
    assert_eq!(std::fs::read_to_string(path).unwrap(), DIAGRAM);
}

#[test]
fn engine_failure_rejects_save_and_settles_waiter_without_writing() {
    let dir = temp_dir("diagram-engine-save");
    let kit = Kit::new(&dir, |hosts| {
        crate::insert_fakes(hosts);
        super::insert_fakes(
            hosts,
            &serde_json::json!({
                "diagram": {"openDiagram": {"target.drawio": {"error": "engine unavailable"}}}
            }),
        );
    });
    let registry = registered(&kit);
    let s = Setup { kit, registry, dir };
    let (tab, path) = s.tab(1, Some(("original.drawio", DIAGRAM)));
    let path = path.unwrap();
    s.ok(tab, "diagram:open-file", vec![string(&path)]);
    let (waiting, then) = outcome();
    editor_for(&s.registry.provided, ViewId(1))
        .unwrap()
        .request_save(ViewId(1), SaveMode::SaveAs, then);
    let target = s.dir.join("target.drawio");
    s.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(target.clone()));
    let selected = s.ok(
        tab,
        "diagram:save-target",
        vec![request([
            ("mode", string("saveAs")),
            ("defaultName", IpcValue::Undefined),
        ])],
    );
    let error = s
        .call(
            tab,
            "diagram:save",
            vec![request([
                ("token", string(text(&selected, "token"))),
                ("bytes", bytes(DIAGRAM)),
            ])],
        )
        .unwrap_err();
    assert_eq!(error.message, "engine unavailable");
    assert_eq!(*waiting.borrow(), Some(Ok(false)));
    assert_eq!(std::fs::read_to_string(path).unwrap(), DIAGRAM);
    assert!(!target.exists());
}

#[test]
fn engine_failure_rejects_project_save_before_creating_file() {
    let dir = temp_dir("diagram-engine-project");
    let target = dir.join("project").join("map.drawio");
    let output = target.to_string_lossy().into_owned();
    let kit = Kit::new(&dir, |hosts| {
        crate::insert_fakes(hosts);
        super::insert_fakes(
            hosts,
            &serde_json::json!({
                "diagram": {
                    "openDiagram": {"Plan.drawio": {"error": "engine unavailable"}},
                    "writeNewDocument": {"path": output}
                }
            }),
        );
    });
    let registry = registered(&kit);
    let s = Setup { kit, registry, dir };
    let (tab, _) = s.tab(1, None);
    let generation = s.kit.services.views.renderer_generation(tab.view).unwrap();
    let origin = s
        .kit
        .host::<std::sync::Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
        .capture_invocation_root(
            tab.view,
            &generation,
            &shell_contract::hosts::synthetic::SyntheticNativeOrigin::new(),
        )
        .unwrap();
    let error = crate::testkit::start_with_owner(
        &s.registry,
        tab,
        "diagram:save-into-project",
        vec![request([
            ("bytes", bytes(DIAGRAM)),
            ("defaultName", string("Plan")),
        ])],
        &origin,
        shell_contract::runtime::CancelToken::default(),
    )
    .try_recv()
    .unwrap()
    .unwrap_err();
    assert_eq!(error.message, "engine unavailable");
    assert!(!target.exists());
}

#[test]
fn opening_a_file_needs_the_grant_and_tells_how_it_opened() {
    let s = setup("open");
    let (tab, path) = s.tab(1, Some(("a.drawio", DIAGRAM)));
    let path = path.unwrap();

    assert_eq!(s.ok(tab, "diagram:consume-pending", vec![]), string(&path));
    let err = s
        .call(tab, "diagram:open-file", vec![string("/etc/passwd")])
        .unwrap_err();
    assert_eq!(err.message, "diagram: path not granted to this view");
    let err = s
        .call(tab, "diagram:open-file", vec![num(3.0)])
        .unwrap_err();
    assert_eq!(err.message, "diagram: path not granted to this view");

    let opened = s.ok(tab, "diagram:open-file", vec![string(&path)]);
    assert_eq!(text(&opened, "status"), "ok");
    assert_eq!(text(&opened, "format"), "drawio");
    assert_eq!(text(&opened, "xml"), DIAGRAM);
    assert!(!is_true(&opened, "compressed"));

    // a file that is not a diagram opens nothing and gets no save target
    let (broken, broken_path) = s.tab(2, Some(("b.drawio", "not a diagram")));
    let opened = s.ok(
        broken,
        "diagram:open-file",
        vec![string(broken_path.as_ref().unwrap())],
    );
    assert_eq!(text(&opened, "status"), "no_diagram");
    let target = s.ok(
        broken,
        "diagram:save-target",
        vec![request([
            ("mode", string("save")),
            ("defaultName", string("x")),
        ])],
    );
    assert_eq!(
        target,
        obj([
            ("ok", IpcValue::Bool(false)),
            ("error", string("diagram: the file did not open"))
        ])
    );

    let (plain, plain_path) = s.tab(3, Some(("c.txt", "hi")));
    let opened = s.ok(
        plain,
        "diagram:open-file",
        vec![string(plain_path.as_ref().unwrap())],
    );
    assert_eq!(opened, obj([("status", string("unsupported"))]));
}

#[test]
fn a_save_target_is_used_once_and_only_for_a_diagram() {
    let s = setup("save");
    let (tab, path) = s.tab(1, Some(("a.drawio", DIAGRAM)));
    let path = path.unwrap();
    s.ok(tab, "diagram:open-file", vec![string(&path)]);

    let target = s.ok(
        tab,
        "diagram:save-target",
        vec![request([
            ("mode", string("save")),
            ("defaultName", IpcValue::Undefined),
        ])],
    );
    assert!(is_true(&target, "ok"));
    assert_eq!(text(&target, "path"), path);
    let token = text(&target, "token");
    assert_eq!(token.len(), 32);

    // what is not a diagram is refused, and the token is spent by it
    s.ok(tab, "diagram:dirty-changed", vec![IpcValue::Bool(true)]);
    let refused = s.ok(
        tab,
        "diagram:save",
        vec![request([
            ("token", string(&token)),
            ("bytes", bytes("<html/>")),
        ])],
    );
    assert_eq!(
        text(&refused, "error"),
        "diagram: refusing to write a file that would not open (no_diagram)"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), DIAGRAM);
    let again = s.ok(
        tab,
        "diagram:save",
        vec![request([
            ("token", string(&token)),
            ("bytes", bytes(DIAGRAM)),
        ])],
    );
    assert_eq!(text(&again, "error"), "diagram: unknown save target");

    // a fresh target with a diagram writes it
    let changed = "<mxfile><diagram id=\"1\" name=\"Changed\"></diagram></mxfile>";
    let target = s.ok(
        tab,
        "diagram:save-target",
        vec![request([
            ("mode", string("save")),
            ("defaultName", IpcValue::Undefined),
        ])],
    );
    s.ok(tab, "diagram:dirty-changed", vec![IpcValue::Bool(true)]);
    let token = text(&target, "token");
    let saved = s.ok(
        tab,
        "diagram:save",
        vec![request([
            ("token", string(&token)),
            ("bytes", bytes(changed)),
        ])],
    );
    assert_eq!(
        saved,
        obj([("ok", IpcValue::Bool(true)), ("path", string(&path))])
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
    let bad = s.ok(
        tab,
        "diagram:save",
        vec![request([
            ("token", string("nope")),
            ("bytes", bytes(changed)),
        ])],
    );
    assert_eq!(text(&bad, "error"), "diagram: unknown save target");
}

#[test]
fn an_untitled_diagram_asks_where_and_then_has_a_file() {
    let s = setup("saveas");
    let (tab, _) = s.tab(1, None);
    let chosen = s.dir.join("plan");
    s.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(chosen.clone()));
    let target = s.ok(
        tab,
        "diagram:save-target",
        vec![request([
            ("mode", string("save")),
            ("defaultName", string("plan")),
        ])],
    );
    let path = s.dir.join("plan.drawio").to_string_lossy().into_owned();
    assert_eq!(text(&target, "path"), path);
    assert_eq!(text(&target, "format"), "drawio");
    s.ok(
        tab,
        "diagram:save",
        vec![request([
            ("token", string(text(&target, "token"))),
            ("bytes", bytes(DIAGRAM)),
        ])],
    );
    assert!(Path::new(&path).exists());
    let editors = editor_for(&s.registry.provided, ViewId(1)).unwrap();
    assert_eq!(editors.document_path(ViewId(1)), Some(PathBuf::from(&path)));
    // now `save` goes straight to the file; Save As asks again, and cancelling answers so
    let again = s.ok(
        tab,
        "diagram:save-target",
        vec![request([
            ("mode", string("save")),
            ("defaultName", IpcValue::Undefined),
        ])],
    );
    assert_eq!(text(&again, "path"), path);
    let cancelled = s.ok(
        tab,
        "diagram:save-target",
        vec![request([
            ("mode", string("saveAs")),
            ("defaultName", IpcValue::Undefined),
        ])],
    );
    assert_eq!(
        cancelled,
        obj([
            ("ok", IpcValue::Bool(true)),
            ("canceled", IpcValue::Bool(true))
        ])
    );
}

#[test]
fn a_recovery_copy_is_kept_only_while_there_are_unsaved_changes() {
    let s = setup("recovery");
    let (tab, path) = s.tab(1, Some(("a.drawio", DIAGRAM)));
    s.ok(
        tab,
        "diagram:open-file",
        vec![string(path.as_ref().unwrap())],
    );
    let copy = || {
        vec![request([
            ("bytes", bytes(DIAGRAM)),
            ("name", IpcValue::Undefined),
        ])]
    };
    let lookup = views().unwrap().recovery_lookup();
    let selected = path.as_ref().unwrap().clone();
    let before = lookup.clone();
    let selected_before = selected.clone();
    assert!(
        !std::thread::spawn(move || before.has_copy(&selected_before))
            .join()
            .unwrap()
    );

    assert_eq!(
        s.ok(tab, "diagram:recovery-copy", copy()),
        string("skipped")
    );
    assert!(s.copies().is_empty());
    s.ok(tab, "diagram:dirty-changed", vec![IpcValue::Bool(true)]);
    assert_eq!(
        s.ok(tab, "diagram:recovery-copy", copy()),
        string("written")
    );
    assert_eq!(s.copies().len(), 1);
    let existing = lookup.clone();
    let selected_after = selected.clone();
    assert!(
        std::thread::spawn(move || existing.has_copy(&selected_after))
            .join()
            .unwrap()
    );
    assert!(
        !std::thread::spawn(move || lookup.has_copy(&format!("{selected}.other")))
            .join()
            .unwrap()
    );
    assert_eq!(
        s.ok(
            tab,
            "diagram:recovery-copy",
            vec![request([
                ("bytes", num(1.0)),
                ("name", IpcValue::Undefined)
            ])]
        ),
        string("skipped")
    );
    s.ok(tab, "diagram:dirty-changed", vec![IpcValue::Bool(false)]);
    assert_eq!(
        s.ok(tab, "diagram:recovery-copy", copy()),
        string("skipped")
    );

    // a save takes the view's copy with it
    s.ok(tab, "diagram:dirty-changed", vec![IpcValue::Bool(true)]);
    let target = s.ok(
        tab,
        "diagram:save-target",
        vec![request([
            ("mode", string("save")),
            ("defaultName", IpcValue::Undefined),
        ])],
    );
    s.ok(
        tab,
        "diagram:save",
        vec![request([
            ("token", string(text(&target, "token"))),
            ("bytes", bytes(DIAGRAM)),
        ])],
    );
    assert!(s.copies().is_empty());
}

#[test]
fn a_save_request_waits_for_the_renderer_and_ends_on_its_answer() {
    let s = setup("saverequest");
    let (tab, path) = s.tab(1, Some(("a.drawio", DIAGRAM)));
    s.ok(
        tab,
        "diagram:open-file",
        vec![string(path.as_ref().unwrap())],
    );
    let editors = editor_for(&s.registry.provided, ViewId(1)).unwrap();
    assert!(editors.owns(ViewId(1)) && !editors.owns(ViewId(7)));

    // nothing unsaved and a file: saved at once, no request to the renderer
    let (done, then) = outcome();
    editors.request_save(ViewId(1), SaveMode::Save, then);
    assert_eq!(*done.borrow(), Some(Ok(true)));
    assert!(emitted(&s.kit, "diagram:save-request").is_empty());

    s.ok(tab, "diagram:dirty-changed", vec![IpcValue::Bool(true)]);
    let (done, then) = outcome();
    editors.request_save(ViewId(1), SaveMode::Save, then);
    assert_eq!(*done.borrow(), None);
    let events = s.kit.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, Target::View(ViewId(1)));
    assert_eq!(events[0].1, "diagram:save-request");
    assert_eq!(events[0].2, vec![string("save")]);
    s.ok(tab, "diagram:save-request-ack", vec![IpcValue::Bool(false)]);
    assert_eq!(*done.borrow(), Some(Ok(false)));

    // the save call of the renderer ends the wait with its own outcome
    let (done, then) = outcome();
    editors.request_save(ViewId(1), SaveMode::SaveAs, then);
    assert_eq!(
        emitted(&s.kit, "diagram:save-request"),
        vec![vec![string("saveAs")]]
    );
    let chosen = s.dir.join("b.drawio");
    s.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(chosen.clone()));
    let target = s.ok(
        tab,
        "diagram:save-target",
        vec![request([
            ("mode", string("saveAs")),
            ("defaultName", IpcValue::Undefined),
        ])],
    );
    assert_eq!(*done.borrow(), None);
    s.ok(
        tab,
        "diagram:save",
        vec![request([
            ("token", string(text(&target, "token"))),
            ("bytes", bytes(DIAGRAM)),
        ])],
    );
    assert_eq!(*done.borrow(), Some(Ok(true)));
    assert!(chosen.exists());

    // a Save As the person cancelled ends the wait without a save
    let (done, then) = outcome();
    editors.request_save(ViewId(1), SaveMode::SaveAs, then);
    s.ok(
        tab,
        "diagram:save-target",
        vec![request([
            ("mode", string("saveAs")),
            ("defaultName", IpcValue::Undefined),
        ])],
    );
    assert_eq!(*done.borrow(), Some(Ok(false)));
}

#[test]
fn closing_a_dirty_diagram_asks_and_saves_through_the_renderer() {
    let s = setup("close");
    let (tab, path) = s.tab(1, Some(("a.drawio", DIAGRAM)));
    s.ok(
        tab,
        "diagram:open-file",
        vec![string(path.as_ref().unwrap())],
    );
    let editors = editor_for(&s.registry.provided, ViewId(1)).unwrap();

    // clean: no question
    let (done, then) = outcome();
    editors.request_close(ViewId(1), Some(tab.window), then);
    assert_eq!(*done.borrow(), Some(Ok(true)));
    assert!(s.kit.take_log().is_empty());

    s.ok(tab, "diagram:dirty-changed", vec![IpcValue::Bool(true)]);
    // Cancel keeps the tab open and asks the renderer nothing
    s.kit.dialogs.messages.lock().unwrap().push_back(2);
    let (done, then) = outcome();
    editors.request_close(ViewId(1), Some(tab.window), then);
    assert_eq!(*done.borrow(), Some(Ok(false)));
    assert!(emitted(&s.kit, "diagram:close-save-request").is_empty());

    // Save: the renderer saves and reports; the close waits for it
    s.kit.dialogs.messages.lock().unwrap().push_back(0);
    let (done, then) = outcome();
    editors.request_close(ViewId(1), Some(tab.window), then);
    assert_eq!(*done.borrow(), None);
    assert_eq!(emitted(&s.kit, "diagram:close-save-request").len(), 1);
    s.ok(tab, "diagram:close-save-result", vec![IpcValue::Bool(true)]);
    assert_eq!(*done.borrow(), Some(Ok(true)));

    // a save that did not happen keeps the tab open
    s.kit.dialogs.messages.lock().unwrap().push_back(0);
    let (done, then) = outcome();
    editors.request_close(ViewId(1), Some(tab.window), then);
    s.ok(
        tab,
        "diagram:close-save-result",
        vec![IpcValue::Bool(false)],
    );
    assert_eq!(*done.borrow(), Some(Ok(false)));

    // Don't Save closes at once
    s.kit.dialogs.messages.lock().unwrap().push_back(1);
    let (done, then) = outcome();
    editors.request_close(ViewId(1), Some(tab.window), then);
    assert_eq!(*done.borrow(), Some(Ok(true)));
}

#[test]
fn a_view_that_goes_away_ends_what_waited_on_it_and_its_copies_follow_its_changes() {
    let s = setup("gone");
    let (dirty, dirty_path) = s.tab(1, Some(("a.drawio", DIAGRAM)));
    let (clean, clean_path) = s.tab(2, Some(("b.drawio", DIAGRAM)));
    s.ok(
        dirty,
        "diagram:open-file",
        vec![string(dirty_path.as_ref().unwrap())],
    );
    s.ok(
        clean,
        "diagram:open-file",
        vec![string(clean_path.as_ref().unwrap())],
    );
    let copy = vec![request([
        ("bytes", bytes(DIAGRAM)),
        ("name", IpcValue::Undefined),
    ])];
    s.ok(dirty, "diagram:dirty-changed", vec![IpcValue::Bool(true)]);
    s.ok(clean, "diagram:dirty-changed", vec![IpcValue::Bool(true)]);
    s.ok(dirty, "diagram:recovery-copy", copy.clone());
    s.ok(clean, "diagram:recovery-copy", copy);
    s.ok(clean, "diagram:dirty-changed", vec![IpcValue::Bool(false)]);
    assert_eq!(s.copies().len(), 2);

    let editors = editor_for(&s.registry.provided, ViewId(1)).unwrap();
    let (saving, then) = outcome();
    editors.request_save(ViewId(1), SaveMode::Save, then);
    let (closing, then) = outcome();
    editors.request_close(ViewId(2), None, then);
    assert_eq!(*closing.borrow(), Some(Ok(true)));
    let (reading, then) = outcome();
    editors.read_text(ViewId(1), then);

    s.kit.destroy(ViewId(1));
    s.kit.destroy(ViewId(2));
    assert_eq!(*saving.borrow(), Some(Ok(false)));
    assert_eq!(
        reading
            .borrow()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .message,
        "the diagram is no longer open"
    );
    // the unsaved diagram keeps its copy for the next launch; the clean one's went
    let left = s.copies();
    assert_eq!(left.len(), 1);
    assert!(!editors.owns(ViewId(1)));
    assert_eq!(
        s.ok(dirty, "diagram:consume-pending", vec![]),
        IpcValue::Null
    );
}

#[test]
fn the_in_app_questions_are_answered_by_the_view_that_was_asked() {
    let s = setup("queries");
    let (tab, _) = s.tab(1, None);
    let (other, _) = s.tab(2, None);
    let editors = editor_for(&s.registry.provided, ViewId(1)).unwrap();

    let (read, then) = outcome();
    editors.read_text(ViewId(1), then);
    let asked = emitted(&s.kit, "diagram:read-request");
    assert_eq!(asked.len(), 1);
    let id = text(&asked[0][0], "id");
    // another view's answer, and one for another question, change nothing
    s.ok(
        other,
        "diagram:read-result",
        vec![obj([("id", string(&id)), ("text", string("stolen"))])],
    );
    s.ok(
        tab,
        "diagram:read-result",
        vec![obj([("id", string("other")), ("text", string("x"))])],
    );
    assert_eq!(*read.borrow(), None);
    s.ok(
        tab,
        "diagram:read-result",
        vec![obj([("id", string(&id)), ("text", string("| a | b |"))])],
    );
    assert_eq!(*read.borrow(), Some(Ok("| a | b |".to_owned())));

    // a renderer that could not read says so
    let (read, then) = outcome();
    editors.read_text(ViewId(1), then);
    let id = text(&emitted(&s.kit, "diagram:read-request")[0][0], "id");
    s.ok(
        tab,
        "diagram:read-result",
        vec![obj([("id", string(&id)), ("error", string("no editor"))])],
    );
    assert_eq!(
        read.borrow()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .message,
        "no editor"
    );

    // an apply first needs a read answered, then asks once
    let applied: Rc<RefCell<Option<Result<IpcValue, String>>>> = Rc::default();
    let got = applied.clone();
    views().unwrap().apply_ops(
        ViewId(1),
        vec![string("op")],
        Box::new(move |r| *got.borrow_mut() = Some(r)),
    );
    let events = s.kit.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, "diagram:read-request");
    let id = text(&events[0].2[0], "id");
    s.ok(
        tab,
        "diagram:read-result",
        vec![obj([("id", string(&id)), ("text", string(""))])],
    );
    let asked = emitted(&s.kit, "diagram:apply-request");
    assert_eq!(asked.len(), 1);
    assert_eq!(
        field(&asked[0][0], "ops"),
        &IpcValue::Array(vec![string("op")])
    );
    let id = text(&asked[0][0], "id");
    s.ok(
        tab,
        "diagram:apply-result",
        vec![obj([
            ("id", string(&id)),
            ("ok", IpcValue::Bool(true)),
            ("result", num(3.0)),
        ])],
    );
    assert_eq!(*applied.borrow(), Some(Ok(num(3.0))));
}

#[test]
fn content_queued_for_a_new_view_is_taken_once() {
    let s = setup("seed");
    let (tab, _) = s.tab(1, None);
    assert_eq!(s.ok(tab, "diagram:seed-take", vec![]), IpcValue::Null);
    views().unwrap().queue_seed(ViewId(1), DIAGRAM);
    assert_eq!(
        s.ok(tab, "diagram:seed-take", vec![]),
        obj([("xml", string(DIAGRAM))])
    );
    assert_eq!(s.ok(tab, "diagram:seed-take", vec![]), IpcValue::Null);
    // a view that opens a file has no seed to take
    let (file, _) = s.tab(2, Some(("a.drawio", DIAGRAM)));
    views().unwrap().queue_seed(ViewId(2), DIAGRAM);
    assert_eq!(s.ok(file, "diagram:seed-take", vec![]), IpcValue::Null);
}

#[test]
fn hidden_open_preparation_preserves_grant_and_refuses_reused_view() {
    let s = setup("hidden-prepared");
    let sender = s.kit.view(1, Surface::Diagram);
    let path = s.dir.join("queued.drawio");
    std::fs::write(&path, DIAGRAM).unwrap();
    let path = path.to_str().unwrap();
    let prepared = super::prepare_hidden_open_path(Some(path)).map(str::to_owned);
    std::fs::remove_file(path).unwrap();
    let views = views().unwrap();
    views
        .open_fresh_hidden_view_prepared(ViewId(1), prepared.as_deref())
        .unwrap();
    assert!(
        views
            .open_fresh_hidden_view_prepared(ViewId(1), None)
            .is_err()
    );
    assert_eq!(
        s.ok(sender, "diagram:consume-pending", vec![]),
        string(path)
    );
    assert_eq!(
        s.ok(sender, "diagram:open-file", vec![string(path)]),
        obj([("status", string("unreadable"))])
    );
}

#[test]
fn a_headless_job_is_given_to_its_window_only_and_ends_on_the_report() {
    let s = setup("headless");
    let (job_tab, _) = s.tab(1, None);
    let (other, _) = s.tab(2, None);
    let report: Rc<RefCell<Option<IpcValue>>> = Rc::default();
    let got = report.clone();
    let job = obj([("kind", string("export")), ("format", string("png"))]);
    views().unwrap().run_headless_job(
        ViewId(1),
        job.clone(),
        Box::new(|_| true),
        Box::new(move |r| *got.borrow_mut() = Some(r)),
    );
    assert_eq!(s.ok(job_tab, "diagram:headless-job", vec![]), job);
    assert_eq!(s.ok(other, "diagram:headless-job", vec![]), IpcValue::Null);
    s.ok(
        other,
        "diagram:headless-done",
        vec![obj([("ok", IpcValue::Bool(true))])],
    );
    assert_eq!(*report.borrow(), None);
    s.ok(job_tab, "diagram:headless-done", vec![string("junk")]);
    assert_eq!(
        *report.borrow(),
        Some(obj([
            ("ok", IpcValue::Bool(false)),
            ("error", string("the diagram page sent no report"))
        ]))
    );
    assert_eq!(
        s.ok(job_tab, "diagram:headless-job", vec![]),
        IpcValue::Null
    );
}

#[test]
fn headless_report_requires_the_registered_origin() {
    let s = setup("headless-origin");
    let (tab, _) = s.tab(1, None);
    let report: Rc<RefCell<Option<IpcValue>>> = Rc::default();
    let got = report.clone();
    views().unwrap().run_headless_job(
        ViewId(1),
        obj([("kind", string("export"))]),
        Box::new(|_| false),
        Box::new(move |value| *got.borrow_mut() = Some(value)),
    );
    assert_eq!(s.ok(tab, "diagram:headless-job", vec![]), IpcValue::Null);
    s.ok(
        tab,
        "diagram:headless-done",
        vec![obj([("ok", IpcValue::Bool(true))])],
    );
    assert_eq!(*report.borrow(), None);
    views().unwrap().fail_headless_job(ViewId(1), "closed");
    assert_eq!(
        *report.borrow(),
        Some(obj([
            ("ok", IpcValue::Bool(false)),
            ("error", string("closed"))
        ]))
    );
}

const PNG: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0, 0, 0, 0];

#[test]
fn pictures_are_checked_before_the_dialog_and_written_one_file_per_page() {
    let s = setup("export");
    let (tab, _) = s.tab(1, None);
    let page =
        |number: f64, data: &[u8]| obj([("number", num(number)), ("bytes", buffer(data.to_vec()))]);
    let export = |format: &str, pages: Vec<IpcValue>, scope: &str| {
        s.ok(
            tab,
            "diagram:export-images",
            vec![obj([
                ("format", string(format)),
                ("pages", IpcValue::Array(pages)),
                ("scope", string(scope)),
                ("suggestedName", string("plan")),
            ])],
        )
    };
    assert_eq!(
        text(&export("gif", vec![], "all"), "error"),
        "bad export format"
    );
    assert_eq!(
        text(&export("png", vec![page(0.0, &PNG)], "all"), "error"),
        "bad export page"
    );
    assert_eq!(
        text(&export("png", vec![page(1.0, b"<svg/>")], "all"), "error"),
        "not a png picture"
    );
    assert_eq!(
        text(
            &export("png", vec![page(1.0, &PNG), page(1.0, &PNG)], "all"),
            "error"
        ),
        "bad export pages"
    );
    assert!(s.kit.take_log().is_empty());

    // cancelled in the dialog
    assert_eq!(
        export("png", vec![page(1.0, &PNG)], "all"),
        obj([
            ("ok", IpcValue::Bool(true)),
            ("canceled", IpcValue::Bool(true))
        ])
    );
    s.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(s.dir.join("out")));
    let done = export("png", vec![page(1.0, &PNG), page(2.0, &PNG)], "all");
    let first = s.dir.join("out-p1.png");
    let second = s.dir.join("out-p2.png");
    assert_eq!(
        field(&done, "paths"),
        &IpcValue::Array(vec![
            string(first.to_string_lossy()),
            string(second.to_string_lossy())
        ])
    );
    assert!(first.exists() && second.exists());
    // the page on screen goes to the chosen file itself
    s.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(s.dir.join("one.png")));
    let done = export("png", vec![page(3.0, &PNG)], "current");
    assert_eq!(
        field(&done, "paths"),
        &IpcValue::Array(vec![string(s.dir.join("one.png").to_string_lossy())])
    );
}

#[test]
fn a_pdf_is_rendered_by_the_shell_and_written_where_the_person_chose() {
    let s = setup("pdf");
    let (tab, _) = s.tab(1, None);
    let page = obj([
        ("svg", string("<svg></svg>")),
        ("width", num(10.0)),
        ("height", num(10.0)),
        ("pageWidth", num(827.0)),
        ("pageHeight", num(1169.0)),
    ]);
    let bad = s.ok(
        tab,
        "diagram:export-pdf",
        vec![obj([("pages", IpcValue::Array(vec![string("x")]))])],
    );
    assert_eq!(text(&bad, "error"), "bad export pages");
    s.kit
        .dialogs
        .saves
        .lock()
        .unwrap()
        .push_back(Some(s.dir.join("plan")));
    let done = s.ok(
        tab,
        "diagram:export-pdf",
        vec![obj([
            ("pages", IpcValue::Array(vec![page])),
            ("suggestedName", string("plan")),
        ])],
    );
    let target = s.dir.join("plan.pdf");
    assert_eq!(
        field(&done, "paths"),
        &IpcValue::Array(vec![string(target.to_string_lossy())])
    );
    assert!(std::fs::read(&target).unwrap().starts_with(b"%PDF"));
}

#[test]
fn attachments_are_accepted_by_extension_and_read_in_pages() {
    let s = setup("attach");
    let (tab, _) = s.tab(1, None);
    let note = s.dir.join("note.txt");
    std::fs::write(&note, "héllo wörld").unwrap();
    let blob = s.dir.join("blob.bin");
    std::fs::write(&blob, "x").unwrap();
    let added = s.ok(
        tab,
        "diagram:files-add",
        vec![IpcValue::Array(vec![
            string(note.to_string_lossy()),
            string(blob.to_string_lossy()),
            num(1.0),
        ])],
    );
    let IpcValue::Array(accepted) = field(&added, "accepted") else {
        panic!()
    };
    assert_eq!(accepted.len(), 1);
    assert_eq!(text(&accepted[0], "ext"), "txt");
    assert_eq!(
        field(&added, "rejected"),
        &IpcValue::Array(vec![string("blob.bin: .bin files are not supported")])
    );

    let page = s.ok(
        tab,
        "diagram:files-read",
        vec![string(note.to_string_lossy()), num(1.0), num(4.0)],
    );
    assert_eq!(text(&page, "text"), "éllo");
    assert_eq!(field(&page, "totalChars"), &num(11.0));
    assert_eq!(field(&page, "offset"), &num(1.0));
    let image = s.ok(
        tab,
        "diagram:files-read",
        vec![string(s.dir.join("a.png").to_string_lossy())],
    );
    assert!(!is_true(&image, "ok"));
    let missing = s.ok(
        tab,
        "diagram:files-read-image",
        vec![string(s.dir.join("a.txt").to_string_lossy())],
    );
    assert!(text(&missing, "error").ends_with("not a supported image type"));
    s.ok(
        tab,
        "diagram:provisional-title",
        vec![string("  A   plan ")],
    );
}

#[test]
fn document_owner_cannot_be_recaptured_after_replacement() {
    use shell_contract::DocumentEditors;
    use shell_contract::ipc::{NativeContext, NativeRequestOwner};
    use shell_contract::runtime::CancelToken;
    let s = setup("diagram-owned-document");
    let (sender, path) = s.tab(91, Some(("owned.drawio", DIAGRAM)));
    let editor = super::editors::DiagramEditors(views().unwrap());
    let generation = s
        .kit
        .services
        .views
        .renderer_generation(sender.view)
        .unwrap();
    let native = NativeRequestOwner {
        view: sender.view,
        context: NativeContext::with_owner_loss((), generation, CancelToken::default()),
    };
    let captured = editor
        .capture_document_owner(sender.view, native.clone())
        .unwrap();
    views().unwrap().open_view(sender.view, path.as_deref());
    assert!(!captured.context.document_lease().unwrap().is_active());
    assert!(
        editor
            .capture_document_owner(sender.view, captured)
            .is_err()
    );
    assert!(editor.capture_document_owner(sender.view, native).is_ok());
}

#[test]
fn document_owner_rejects_another_native_view_without_changing_path() {
    use shell_contract::DocumentEditors;
    use shell_contract::ipc::{NativeContext, NativeRequestOwner};
    use shell_contract::runtime::CancelToken;
    let s = setup("diagram-owned-foreign-view");
    let (sender, path) = s.tab(92, Some(("owned.drawio", DIAGRAM)));
    s.ok(
        sender,
        "diagram:open-file",
        vec![string(path.as_deref().unwrap())],
    );
    let editor = super::editors::DiagramEditors(views().unwrap());
    let generation = s
        .kit
        .services
        .views
        .renderer_generation(sender.view)
        .unwrap();
    let foreign = NativeRequestOwner {
        view: ViewId(93),
        context: NativeContext::with_owner_loss((), generation, CancelToken::default()),
    };
    assert!(editor.capture_document_owner(sender.view, foreign).is_err());
    assert_eq!(views().unwrap().file_path(sender.view), path);
}

#[test]
fn a_foreign_owned_save_id_cannot_write_or_retarget_a_diagram() {
    let s = setup("diagram-owned-request-id");
    let (sender, path) = s.tab(94, Some(("owned.drawio", DIAGRAM)));
    let path = path.unwrap();
    s.ok(sender, "diagram:open-file", vec![string(&path)]);
    let before = std::fs::read(&path).unwrap();
    let result = s.call(
        sender,
        "diagram:save",
        vec![obj([
            ("requestId", string("another-request")),
            ("token", string("foreign-token")),
            ("bytes", bytes(DIAGRAM)),
        ])],
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        views().unwrap().file_path(sender.view).as_deref(),
        Some(path.as_str())
    );
}

/// A diagram renderer: its view plus the native origin its calls carry
struct Renderer {
    tab: Sender,
    origin: shell_contract::ipc::NativeRequestOwner,
}

impl Renderer {
    fn call(&self, s: &Setup, channel: &str, args: Vec<IpcValue>) -> Result<IpcValue, IpcError> {
        crate::testkit::start_with_owner(
            &s.registry,
            self.tab,
            channel,
            args,
            &self.origin,
            shell_contract::runtime::CancelToken::default(),
        )
        .try_recv()
        .unwrap()
    }

    fn acknowledge(&self, s: &Setup, token: &str, ok: bool) -> Result<IpcValue, IpcError> {
        self.call(
            s,
            "diagram:recovery-initialized",
            vec![string(token), IpcValue::Bool(ok)],
        )
    }
}

/// A recovery file the shell committed and the renderer opened: the renderer
/// now owes `diagram:recovery-initialized` with `token`
struct Recovering {
    renderer: Renderer,
    receipt: shell_contract::ipc::RecoverySeedReceipt,
    token: String,
    // the closure and target must outlive the wait
    _issuer: shell_contract::ipc::NativeClosureIssuer,
    _target: shell_contract::ipc::RecoveryOpenTarget,
}

impl Setup {
    fn renderer(&self, tab: Sender) -> Renderer {
        let generation = self
            .kit
            .services
            .views
            .renderer_generation(tab.view)
            .unwrap();
        let origin = self
            .kit
            .host::<std::sync::Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>()
            .capture_invocation_root(
                tab.view,
                &generation,
                &shell_contract::hosts::synthetic::SyntheticNativeOrigin::new(),
            )
            .unwrap();
        Renderer { tab, origin }
    }

    /// Diagram view `id` on a recovery file, and the target the host hands the editor for
    /// it. The browser's owner is not committed yet: the host does that before the hand-over.
    fn recovery_target(
        &self,
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
        let (tab, path) = self.tab(id, Some((name, DIAGRAM)));
        let renderer = self.renderer(tab);
        let generation = self
            .kit
            .services
            .views
            .renderer_generation(tab.view)
            .unwrap();
        let cancel = CancelToken::default();
        let (issuer, closure, browser_owner) =
            shell_contract::ipc::NativeClosureIssuer::new_with_owner(tab.view, generation.clone());
        let target = RecoveryOpenTarget::bind(
            RecoveryOpenContext::Renderer {
                source: renderer.origin.clone(),
                participants: vec![],
                cancel: cancel.clone(),
                pending: cancel,
            },
            tab.view,
            generation,
            closure,
            browser_owner,
        )
        .unwrap();
        (renderer, path.unwrap(), issuer, target)
    }

    /// The shell's recovery open of `path` into the target's view; whether the editor took it
    fn start_recovery(&self, path: &str, target: &shell_contract::ipc::RecoveryOpenTarget) -> bool {
        use main_core::packages::electron_utils::recovery_contract::{
            RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
        };
        let done = Rc::new(RefCell::new(None));
        let completed = done.clone();
        views().unwrap().open_file_with_decision(
            &target.view(),
            &RecoveryOpenFileRequest {
                file_path: path.to_owned(),
                decision: RecoveryOpenDecision::Original,
            },
            target,
            Box::new(move |result| *completed.borrow_mut() = Some(result)),
        );
        done.borrow_mut().take().unwrap().is_ok()
    }

    /// Drives the recovery open of diagram view `id` through the real
    /// handlers up to the token the renderer must acknowledge
    fn recover(&self, id: u32, name: &str) -> Recovering {
        let (renderer, path, issuer, target) = self.recovery_target(id, name);
        // the host commits the browser's owner before it hands the target to the editor
        target
            .closure_reservation()
            .with_initial_owner(target.browser_owner(), |_| Ok(()))
            .unwrap();
        assert!(self.start_recovery(&path, &target));
        let receipt = target.committed_seed().unwrap();
        target
            .retain_native_registration(renderer.origin.clone())
            .unwrap();
        assert_eq!(
            renderer
                .call(self, "diagram:consume-pending", vec![])
                .unwrap(),
            string(path.clone())
        );
        let opened = renderer
            .call(self, "diagram:open-file", vec![string(path)])
            .unwrap();
        assert_eq!(field(&opened, "status"), &string("ok"));
        let token = text(&opened, "recoveryToken");
        assert!(receipt.settlement().is_none());
        Recovering {
            renderer,
            receipt,
            token,
            _issuer: issuer,
            _target: target,
        }
    }
}

/// The host commits the browser's owner when it makes the view, and a recovery is staged only
/// under that owner. A browser nobody committed, and one whose owner was replaced after the
/// host committed it, are refused: no seed, and nothing staged for the view.
#[test]
fn a_recovery_is_staged_only_under_the_browser_owner_the_host_committed() {
    let s = setup("diagram-recovery-owner");
    let staged = |view: ViewId| {
        views().unwrap().with_state(|state| {
            state.views.get(&view).is_some_and(|entry| {
                entry.recovery_preparing.is_some()
                    || entry.recovery_seed.is_some()
                    || entry.recovery_applying.is_some()
            })
        })
    };

    let (renderer, path, _issuer, target) = s.recovery_target(411, "uncommitted.drawio");
    assert!(!s.start_recovery(&path, &target));
    assert!(target.committed_seed().is_none());
    assert!(!staged(renderer.tab.view));

    let (renderer, path, _issuer, target) = s.recovery_target(412, "replaced.drawio");
    let reservation = target.closure_reservation();
    reservation
        .with_initial_owner(target.browser_owner(), |_| Ok(()))
        .unwrap();
    let _successor = reservation
        .prepare_replacement(target.browser_owner())
        .unwrap()
        .commit(|_, _| Ok(()))
        .unwrap();
    assert!(!s.start_recovery(&path, &target));
    assert!(target.committed_seed().is_none());
    assert!(!staged(renderer.tab.view));

    // the same start under the owner the host committed is taken
    let committed = s.recover(413, "committed.drawio");
    assert!(committed.receipt.settlement().is_none());
}

#[test]
fn native_evidence_diagram_recovery_initialized_settles_only_the_exact_ack() {
    let s = setup("diagram-recovery-exact");
    let a = s.recover(401, "a.drawio");
    let b = s.recover(402, "b.drawio");
    let bystander = s.renderer(s.tab(403, None).0);
    let message = |result: Result<IpcValue, IpcError>| result.unwrap_err().message;

    // missing-receipt: no token at all, then a wrong argument type
    let no_token = message(a.renderer.call(&s, "diagram:recovery-initialized", vec![]));
    assert!(no_token.contains("token is required"), "{no_token}");
    let no_text_token = message(a.renderer.call(
        &s,
        "diagram:recovery-initialized",
        vec![IpcValue::Bool(true)],
    ));
    assert!(
        no_text_token.contains("token is required"),
        "{no_text_token}"
    );
    assert!(a.receipt.settlement().is_none() && b.receipt.settlement().is_none());

    // foreign-receipt: an unknown token, and the token of the other view's wait
    let unknown = message(a.renderer.acknowledge(&s, "stale-token", true));
    assert!(unknown.contains("does not match"), "{unknown}");
    let other_wait = message(b.renderer.acknowledge(&s, &a.token, true));
    assert!(other_wait.contains("does not match"), "{other_wait}");
    assert!(a.receipt.settlement().is_none() && b.receipt.settlement().is_none());

    // foreign-renderer: a diagram view that was never handed the recovery
    // copy cannot settle it, even with the exact token
    let foreign = message(bystander.acknowledge(&s, &a.token, true));
    assert!(foreign.contains("does not match"), "{foreign}");
    assert!(a.receipt.settlement().is_none() && b.receipt.settlement().is_none());

    // successful-ack: the exact acknowledgement settles only its own wait
    assert_eq!(
        a.renderer.acknowledge(&s, &a.token, true).unwrap(),
        IpcValue::Null
    );
    assert!(matches!(a.receipt.settlement(), Some(Ok(()))));
    assert!(b.receipt.settlement().is_none());

    // duplicate-ack: a second acknowledgement changes nothing
    let duplicate = message(a.renderer.acknowledge(&s, &a.token, false));
    assert!(duplicate.contains("does not match"), "{duplicate}");
    assert!(matches!(a.receipt.settlement(), Some(Ok(()))));
    assert!(b.receipt.settlement().is_none());

    shell_contract::native_evidence::record(
        "diagram:recovery-initialized",
        &[
            "successful-ack",
            "missing-receipt",
            "foreign-receipt",
            "foreign-renderer",
            "duplicate-ack",
        ],
        &serde_json::json!({
            "missing-receipt": {"noToken": no_token, "nonTextToken": no_text_token, "settled": false},
            "foreign-receipt": {"unknownToken": unknown, "otherWaitsToken": other_wait, "settled": false},
            "foreign-renderer": {"error": foreign, "settled": false},
            "successful-ack": {"answer": null, "settlement": "ok", "otherWaitSettled": false},
            "duplicate-ack": {"error": duplicate, "settlement": "ok"},
        })
        .to_string(),
    );
}

#[test]
fn native_evidence_diagram_recovery_initialized_failed_apply_settles_the_wait_as_failure() {
    let s = setup("diagram-recovery-failed");
    let a = s.recover(411, "a.drawio");
    let failure = "the renderer could not apply the recovery copy";
    assert_eq!(
        a.renderer
            .call(
                &s,
                "diagram:recovery-initialized",
                vec![string(&a.token), IpcValue::Bool(false), string(failure)],
            )
            .unwrap(),
        IpcValue::Null
    );
    assert_eq!(a.receipt.settlement(), Some(Err(failure.to_owned())));
    // the wait is cleaned up: a later success claim cannot revive it
    assert!(a.renderer.acknowledge(&s, &a.token, true).is_err());
    assert_eq!(a.receipt.settlement(), Some(Err(failure.to_owned())));
    shell_contract::native_evidence::record(
        "diagram:recovery-initialized",
        &["failed-apply"],
        &serde_json::json!({"failed-apply": {"answer": null, "settlement": {"error": failure}, "lateSuccessRefused": true}})
            .to_string(),
    );
}

#[test]
fn native_evidence_diagram_recovery_initialized_is_refused_to_other_surfaces() {
    let dir = temp_dir("diagram-recovery-surface");
    let kit = Kit::new(&dir, crate::insert_fakes);
    let mut table = shell_core::table::Table::default();
    crate::register(&mut table, &kit.services).unwrap();
    let dispatcher = shell_core::dispatch::Dispatcher::new(table);
    let mut refused = serde_json::Map::new();
    for (id, surface) in [(421, Surface::Docs), (422, Surface::Mindmap)] {
        let foreign = kit.view(id, surface);
        let (send, receive) = std::sync::mpsc::channel();
        dispatcher.dispatch(
            foreign,
            shell_contract::ipc::ChannelKind::Invoke,
            Some(shell_contract::RequestId(1)),
            None,
            "diagram:recovery-initialized",
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
        "diagram:recovery-initialized",
        &["unauthorized-surface"],
        &serde_json::json!({"unauthorized-surface": refused}).to_string(),
    );
}

#[test]
fn recovery_preparation_cancelled_before_worker_cannot_commit_seed() {
    use main_core::packages::electron_utils::recovery_contract::{
        RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
    };
    use shell_contract::ipc::{RecoveryOpenContext, RecoveryOpenTarget};
    use shell_contract::runtime::CancelToken;
    let s = setup("diagram-recovery-cancel");
    let (tab, path) = s.tab(402, Some(("diagram.drawio", DIAGRAM)));
    let generation = s.kit.services.views.renderer_generation(tab.view).unwrap();
    let synthetic = s
        .kit
        .host::<std::sync::Arc<shell_contract::hosts::synthetic::SyntheticOwnedEffects>>();
    let origin = synthetic
        .capture_invocation_root(
            tab.view,
            &generation,
            &shell_contract::hosts::synthetic::SyntheticNativeOrigin::new(),
        )
        .unwrap();
    let cancel = CancelToken::default();
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(tab.view, generation.clone());
    let target = RecoveryOpenTarget::bind(
        RecoveryOpenContext::Renderer {
            source: origin,
            participants: vec![],
            cancel: cancel.clone(),
            pending: CancelToken::default(),
        },
        tab.view,
        generation,
        closure,
        browser_owner,
    )
    .unwrap();
    // the host commits the browser's owner before it hands the target to the editor
    target
        .closure_reservation()
        .with_initial_owner(target.browser_owner(), |_| Ok(()))
        .unwrap();
    let done = Rc::new(RefCell::new(None));
    let completed = done.clone();
    s.kit.hold_workers();
    views().unwrap().open_file_with_decision(
        &tab.view,
        &RecoveryOpenFileRequest {
            file_path: path.unwrap(),
            decision: RecoveryOpenDecision::Original,
        },
        &target,
        Box::new(move |result| *completed.borrow_mut() = Some(result)),
    );
    cancel.cancel();
    s.kit.release_workers();
    assert!(matches!(&*done.borrow(), Some(Err(_))));
    assert!(target.committed_seed().is_none());
}
