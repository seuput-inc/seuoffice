//! The EPUB handlers as a renderer and the shell see them

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use shell_contract::editors::{SaveMode, Then};
use shell_contract::ipc::{Sender, Target};
use shell_contract::{DocumentEditors, IpcError, IpcValue, Surface, editor_for};

use super::hosts::{FakeEngine, FakeShell, Log};
use super::{EpubEditors, EpubEngineHost, EpubShellHost};
use crate::kit::{IntoIpc, field, obj};
use crate::testing::{HostCall, Rt};

const CHAPTER: &str = "<p>OEBPS/c1.xhtml</p>";

#[test]
fn book_dialog_keeps_existing_extension_and_appends_missing_extension() {
    assert_eq!(
        super::with_book_extension("/books/Novel.EPUB"),
        "/books/Novel.EPUB"
    );
    assert_eq!(
        super::with_book_extension("/books/Novel.txt"),
        "/books/Novel.txt.epub"
    );
    assert_eq!(
        super::with_book_extension("/books/Novel"),
        "/books/Novel.epub"
    );
    assert_eq!(
        super::with_book_extension("/books/.epub"),
        "/books/.epub.epub"
    );
}

/// A shell with the EPUB editor's fakes; the log is what they were asked
fn rt(case: Value) -> (Rt, Log) {
    let log = Log::default();
    let (engine, shell, into) = (case["engine"].clone(), case["shell"].clone(), log.clone());
    let mut rt = Rt::new(move |h| {
        h.insert::<EpubEngineHost>(Arc::new(FakeEngine::new(&engine, into.clone())));
        h.insert::<EpubShellHost>(Arc::new(FakeShell::new(&shell, into)));
    });
    rt.register(super::entries).unwrap();
    (rt, log)
}

fn editors(rt: &Rt) -> Rc<EpubEditors> {
    rt.provided().all::<EpubEditors>()[0].clone()
}

fn s(text: &str) -> IpcValue {
    text.into_ipc()
}

fn n(x: f64) -> IpcValue {
    IpcValue::Number(x)
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A tracked view whose book file exists and was granted (`createEpubView`)
fn granted(rt: &mut Rt, id: u32) -> (Sender, String) {
    let path = rt.dir.join(format!("book{id}.epub"));
    std::fs::write(&path, b"BOOK").unwrap();
    let sender = rt.view(id, Surface::Epub);
    editors(rt).open_view(sender.view, Some(&path));
    (sender, path_str(&path))
}

/// A view with its book open
fn opened(rt: &mut Rt, id: u32) -> (Sender, String) {
    let (sender, path) = granted(rt, id);
    let reply = rt
        .invoke_now("epub:open-file", sender, vec![s(&path)])
        .unwrap();
    assert_eq!(field(&reply, "status"), &s("ok"));
    (sender, path)
}

fn edits(rt: &mut Rt, sender: Sender, edits: Vec<IpcValue>) -> IpcValue {
    let request = obj([("spineIndex", n(0.0)), ("edits", edits.into_ipc())]);
    rt.invoke_now("epub:apply-edits", sender, vec![request])
        .unwrap()
}

/// replaces the text of the chapter's first block
fn edit(rt: &mut Rt, sender: Sender, text: &str) -> IpcValue {
    let edit = obj([
        ("op", s("replace")),
        ("blockId", s("b1")),
        ("text", s(text)),
    ]);
    edits(rt, sender, vec![edit])
}

fn ids(answer: &IpcValue) -> Vec<IpcValue> {
    match field(answer, "blocks") {
        IpcValue::Array(blocks) => blocks.iter().map(|b| field(b, "id").clone()).collect(),
        other => panic!("{other:?}"),
    }
}

type Slot<T> = Rc<RefCell<Option<Result<T, IpcError>>>>;

fn slot<T: 'static>() -> (Slot<T>, Then<T>) {
    let slot: Slot<T> = Rc::default();
    let into = slot.clone();
    (slot, Box::new(move |r| *into.borrow_mut() = Some(r)))
}

fn message(r: Option<Result<impl std::fmt::Debug, IpcError>>) -> String {
    r.expect("answered").unwrap_err().message
}

fn events(rt: &Rt) -> Vec<(Target, &'static str, Vec<IpcValue>)> {
    rt.events()
        .into_iter()
        .map(|e| (e.target, e.channel, e.args))
        .collect()
}

#[test]
fn chapter_engine_failure_rejects_and_does_not_cache_a_failed_load() {
    let (mut rt, _) = rt(json!({ "engine": {
        "openChapter": [{"throw": "chapter engine failed"}]
    } }));
    let (sender, _) = opened(&mut rt, 1);
    let error = rt
        .invoke_now("epub:read-chapter", sender, vec![n(0.0)])
        .unwrap_err();
    assert_eq!(error.message, "chapter engine failed");
    let chapter = rt
        .invoke_now("epub:read-chapter", sender, vec![n(0.0)])
        .unwrap();
    assert_eq!(field(&chapter, "source"), &s(CHAPTER));
}

#[test]
fn replacement_engine_failures_reject_without_mutating_the_chapter() {
    for operation in ["applyReplacements", "remapChapter"] {
        let mut engine = serde_json::Map::new();
        engine.insert(
            operation.into(),
            json!([{"throw": "replacement engine failed"}]),
        );
        let (mut rt, _) = rt(json!({"engine": engine}));
        let (sender, _) = opened(&mut rt, 1);
        let replacement = obj([("start", n(3.0)), ("end", n(4.0)), ("text", s("X"))]);
        let request = obj([
            ("spineIndex", n(0.0)),
            ("version", n(0.0)),
            ("replacements", vec![replacement].into_ipc()),
        ]);
        let error = rt
            .invoke_now("epub:replace-ranges", sender, vec![request])
            .unwrap_err();
        assert_eq!(error.message, "replacement engine failed");
        let chapter = rt
            .invoke_now("epub:read-chapter", sender, vec![n(0.0)])
            .unwrap();
        assert_eq!(field(&chapter, "source"), &s(CHAPTER));
        assert_eq!(field(&chapter, "version"), &n(0.0));
    }
}

#[test]
fn text_extraction_failure_reaches_the_editor_caller() {
    let (mut rt, _) = rt(json!({ "engine": {
        "extractChapterText": [{"throw": "text engine failed"}]
    } }));
    let (sender, _) = opened(&mut rt, 1);
    let editor = editor_for(&rt.provided(), sender.view).unwrap();
    let (answer, then) = slot();
    editor.read_text(sender.view, then);
    let (_, channel, args) = events(&rt).pop().unwrap();
    assert_eq!(channel, "epub:flush-request");
    rt.send(
        "epub:flush-result",
        sender,
        vec![args[0].clone(), true.into_ipc()],
    );
    assert_eq!(message(answer.take()), "text engine failed");
}

#[test]
fn migration_failure_keeps_the_successfully_saved_book() {
    let (mut rt, _) = rt(json!({ "engine": {
        "migrationInfo": [{"throw": "migration engine failed"}]
    } }));
    let (sender, path) = opened(&mut rt, 1);
    edit(&mut rt, sender, "hello");
    let saved = rt.invoke_now("epub:save", sender, vec![]).unwrap();
    assert_eq!(field(&saved, "written"), &true.into_ipc());
    assert_eq!(std::fs::read(path).unwrap(), b"BOOKhello");
    assert!(!editors(&rt).is_dirty(sender.view));
}

#[test]
fn opens_only_a_granted_path() {
    let (mut rt, _) = rt(Value::Null);
    let stranger = rt.view(9, Surface::Epub);
    assert_eq!(
        rt.invoke_now("epub:consume-pending", stranger, vec![]),
        Ok(IpcValue::Null)
    );
    let (sender, path) = granted(&mut rt, 1);
    assert_eq!(
        rt.invoke_now("epub:consume-pending", sender, vec![]),
        Ok(s(&path))
    );
    let refused = rt
        .invoke_now("epub:open-file", stranger, vec![s(&path)])
        .unwrap_err();
    assert_eq!(
        refused.remote_message("epub:open-file"),
        "Error invoking remote method 'epub:open-file': Error: epub: path not granted to this view"
    );
    let reply = rt
        .invoke_now("epub:open-file", sender, vec![s(&path)])
        .unwrap();
    let token = match field(&reply, "assetBase") {
        IpcValue::String(base) => base
            .strip_prefix("epub-asset://")
            .and_then(|b| b.strip_suffix('/'))
            .unwrap()
            .to_owned(),
        other => panic!("{other:?}"),
    };
    assert_eq!(field(&reply, "readOnly"), &IpcValue::Null);
    assert_eq!(
        editors(&rt).asset_book(&token).as_deref(),
        Some(&b"BOOK".to_vec())
    );
    assert_eq!(editors(&rt).asset_book("other"), None);
}

#[test]
fn a_book_that_did_not_open_is_never_written() {
    let (mut rt, _) = rt(json!({ "engine": { "open": [{ "refused": "drm" }] } }));
    let (sender, path) = granted(&mut rt, 1);
    let reply = rt
        .invoke_now("epub:open-file", sender, vec![s(&path)])
        .unwrap();
    assert_eq!(reply, obj([("status", s("refused")), ("reason", s("drm"))]));
    std::fs::remove_file(&path).unwrap();
    let reply = rt
        .invoke_now("epub:open-file", sender, vec![s(&path)])
        .unwrap();
    assert_eq!(field(&reply, "reason"), &s("unreadable"));
    let saved = rt
        .invoke_now("epub:save", sender, vec![obj([("mode", s("save"))])])
        .unwrap();
    assert_eq!(
        saved,
        obj([
            ("ok", false.into_ipc()),
            (
                "error",
                s("epub: the file did not open, nothing is written")
            )
        ])
    );
    assert_eq!(
        rt.invoke_now("epub:read-chapter", sender, vec![n(0.0)]),
        Ok(obj([("ok", false.into_ipc()), ("error", s("not-open"))]))
    );
    assert!(rt.script().calls.is_empty(), "no dialog");
}

#[test]
fn an_edit_makes_the_book_dirty_and_a_save_writes_only_then() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, path) = opened(&mut rt, 1);
    let chapter = rt
        .invoke_now("epub:read-chapter", sender, vec![n(0.0)])
        .unwrap();
    assert_eq!(field(&chapter, "source"), &s(CHAPTER));
    assert_eq!(field(&chapter, "version"), &n(0.0));
    assert_eq!(field(&chapter, "broken"), &false.into_ipc());
    assert_eq!(
        field(
            &rt.invoke_now("epub:read-chapter", sender, vec![n(4.0)])
                .unwrap(),
            "error"
        ),
        &s("invalid-request")
    );

    let edited = edit(&mut rt, sender, "hello");
    assert_eq!(field(&edited, "source"), &s("<p>hello</p>"));
    assert_eq!(field(&edited, "version"), &n(1.0));
    assert_eq!(field(&edited, "dirty"), &true.into_ipc());
    let inverse = obj([
        ("start", n(3.0)),
        ("end", n(8.0)),
        ("text", s("OEBPS/c1.xhtml")),
    ]);
    assert_eq!(field(&edited, "inverse"), &vec![inverse].into_ipc());
    let eds = editors(&rt);
    assert!(eds.is_dirty(sender.view));

    let before = rt.invoke_now("epub:document-hash", sender, vec![]).unwrap();
    let saved = rt.invoke_now("epub:save", sender, vec![]).unwrap();
    assert_eq!(field(&saved, "written"), &true.into_ipc());
    assert_ne!(field(&saved, "docHash"), field(&before, "docHash"));
    assert_eq!(std::fs::read(&path).unwrap(), b"BOOKhello");
    assert!(!eds.is_dirty(sender.view));
    let again = rt.invoke_now("epub:save", sender, vec![]).unwrap();
    assert_eq!(field(&again, "written"), &false.into_ipc());
    assert_eq!(field(&again, "docHash"), field(&saved, "docHash"));
    assert_eq!(
        field(
            &rt.invoke_now("epub:document-hash", sender, vec![]).unwrap(),
            "docHash"
        ),
        field(&saved, "docHash")
    );
}

#[test]
fn edits_are_checked_and_refused_while_translating() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, _) = opened(&mut rt, 1);
    let bad = rt
        .invoke_now(
            "epub:apply-edits",
            sender,
            vec![obj([
                ("spineIndex", n(0.0)),
                ("edits", vec![s("x")].into_ipc()),
            ])],
        )
        .unwrap();
    assert_eq!(field(&bad, "message"), &s("not a block edit"));
    assert_eq!(
        rt.invoke_now("epub:set-translation-lock", sender, vec![true.into_ipc()]),
        Ok(true.into_ipc())
    );
    assert!(editors(&rt).is_translation_locked(sender.view));
    assert_eq!(field(&edit(&mut rt, sender, "x"), "error"), &s("locked"));
    assert_eq!(
        rt.invoke_now("epub:set-translation-lock", sender, vec![s("yes")]),
        Ok(false.into_ipc())
    );
    assert_eq!(field(&edit(&mut rt, sender, "x"), "ok"), &true.into_ipc());
}

#[test]
fn range_replacements_need_the_current_version() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, _) = opened(&mut rt, 1);
    edit(&mut rt, sender, "hello");
    let replace = |version: IpcValue| {
        let r = obj([
            ("start", n(3.0)),
            ("end", n(8.0)),
            ("text", s("OEBPS/c1.xhtml")),
        ]);
        obj([
            ("spineIndex", n(0.0)),
            ("version", version),
            ("replacements", vec![r].into_ipc()),
        ])
    };
    let stale = rt
        .invoke_now("epub:replace-ranges", sender, vec![replace(n(0.0))])
        .unwrap();
    assert_eq!(
        stale,
        obj([
            ("ok", false.into_ipc()),
            ("error", s("stale")),
            ("message", s("the chapter is at version 1"))
        ])
    );
    let bad = rt
        .invoke_now("epub:replace-ranges", sender, vec![replace(s("1"))])
        .unwrap();
    assert_eq!(field(&bad, "message"), &s("bad replacements"));
    let back = rt
        .invoke_now("epub:replace-ranges", sender, vec![replace(n(1.0))])
        .unwrap();
    assert_eq!(field(&back, "source"), &s(CHAPTER));
    assert_eq!(field(&back, "version"), &n(2.0));
    assert_eq!(field(&back, "dirty"), &false.into_ipc());
}

/// `OpenChapter.nextId` goes back to the engine with the map: after the
/// highest block is deleted, a new block must not take its id again
#[test]
fn fresh_block_ids_continue_after_the_last_block_is_deleted() {
    let (mut rt, _) =
        rt(json!({ "engine": { "readText": { "OEBPS/c1.xhtml": "<p>a</p><p>b</p>" } } }));
    let (sender, _) = opened(&mut rt, 1);
    let chapter = rt
        .invoke_now("epub:read-chapter", sender, vec![n(0.0)])
        .unwrap();
    assert_eq!(ids(&chapter), vec![s("b1"), s("b2")]);
    let deleted = edits(
        &mut rt,
        sender,
        vec![obj([("op", s("delete")), ("blockId", s("b2"))])],
    );
    assert_eq!(ids(&deleted), vec![s("b1")]);
    let inserted = edits(
        &mut rt,
        sender,
        vec![obj([
            ("op", s("insert_after")),
            ("blockId", s("b1")),
            ("text", s("c")),
        ])],
    );
    assert_eq!(field(&inserted, "created"), &vec![s("b3")].into_ipc());
    assert_eq!(ids(&inserted), vec![s("b1"), s("b3")]);

    // a range replacement that splits the last block numbers on from there too
    let split = obj([
        ("start", n(12.0)),
        ("end", n(12.0)),
        ("text", s("</p><p>d")),
    ]);
    let request = obj([
        ("spineIndex", n(0.0)),
        ("version", field(&inserted, "version").clone()),
        ("replacements", vec![split].into_ipc()),
    ]);
    let remapped = rt
        .invoke_now("epub:replace-ranges", sender, vec![request])
        .unwrap();
    assert_eq!(field(&remapped, "source"), &s("<p>a</p><p>c</p><p>d</p>"));
    assert_eq!(field(&remapped, "created"), &vec![s("b4")].into_ipc());
    assert_eq!(ids(&remapped), vec![s("b1"), s("b3"), s("b4")]);
}

#[test]
fn a_broken_chapter_is_read_only() {
    let (mut rt, _) = rt(json!({ "engine": { "readText": { "OEBPS/c1.xhtml": "<broken" } } }));
    let (sender, _) = opened(&mut rt, 1);
    let chapter = rt
        .invoke_now("epub:read-chapter", sender, vec![n(0.0)])
        .unwrap();
    assert_eq!(field(&chapter, "broken"), &true.into_ipc());
    assert_eq!(field(&chapter, "readOnly"), &s("broken-chapter"));
    let refused = edit(&mut rt, sender, "x");
    assert_eq!(field(&refused, "error"), &s("read-only"));
    assert_eq!(
        field(&refused, "message"),
        &s("the chapter is read only (broken-chapter)")
    );
}

#[test]
fn a_menu_save_waits_for_the_renderer() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, _) = opened(&mut rt, 1);
    let view = sender.view;
    let editor = editor_for(&rt.provided(), view).unwrap();

    // clean, with a file: nothing to ask
    let (answer, then) = slot();
    editor.request_save(view, SaveMode::Save, then);
    assert_eq!(*answer.borrow(), Some(Ok(true)));
    assert!(rt.events().is_empty());

    edit(&mut rt, sender, "hello");
    let (answer, then) = slot();
    editor.request_save(view, SaveMode::Save, then);
    assert_eq!(
        events(&rt),
        vec![(Target::View(view), "epub:save-request", vec![s("save")])]
    );
    assert_eq!(*answer.borrow(), None);
    rt.invoke_now("epub:save", sender, vec![]).unwrap();
    assert_eq!(*answer.borrow(), Some(Ok(true)));

    let (answer, then) = slot();
    editor.request_save(view, SaveMode::SaveAs, then);
    rt.send("epub:save-request-ack", sender, vec![false.into_ipc()]);
    assert_eq!(*answer.borrow(), Some(Ok(false)));

    let (answer, then) = slot();
    editor.request_save(view, SaveMode::SaveAs, then);
    rt.advance(Duration::from_secs(119));
    assert_eq!(*answer.borrow(), None);
    rt.advance(Duration::from_secs(1));
    assert_eq!(*answer.borrow(), Some(Ok(false)));
}

#[test]
fn closing_a_dirty_book_asks_and_saves() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, _) = opened(&mut rt, 1);
    let view = sender.view;
    let editor = editor_for(&rt.provided(), view).unwrap();
    let (answer, then) = slot();
    editor.request_close(view, None, then);
    assert_eq!(*answer.borrow(), Some(Ok(true)));
    assert!(rt.script().calls.is_empty());

    rt.send("epub:dirty-changed", sender, vec![true.into_ipc()]);
    rt.script().message.extend([2, 1, 0]);
    let (answer, then) = slot();
    editor.request_close(view, None, then);
    assert_eq!(*answer.borrow(), Some(Ok(false)));
    let (answer, then) = slot();
    editor.request_close(view, None, then);
    assert_eq!(*answer.borrow(), Some(Ok(true)));
    let (answer, then) = slot();
    editor.request_close(view, Some(sender.window), then);
    assert_eq!(
        events(&rt),
        vec![(Target::View(view), "epub:close-save-request", vec![])]
    );
    rt.send("epub:close-save-result", sender, vec![true.into_ipc()]);
    assert_eq!(*answer.borrow(), Some(Ok(true)));
    let calls = rt.script().calls.clone();
    let HostCall::Message(parent, question) = &calls[2] else {
        panic!("{calls:?}")
    };
    assert_eq!(*parent, Some(sender.window));
    assert_eq!(question.buttons, ["Save", "Don't Save", "Cancel"]);
    assert_eq!(question.cancel_id, Some(2));
}

#[test]
fn reading_the_text_waits_for_the_flush() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, _) = opened(&mut rt, 1);
    let view = sender.view;
    let editor = editor_for(&rt.provided(), view).unwrap();
    edit(&mut rt, sender, "hello");

    let flush = |rt: &mut Rt, result: IpcValue| {
        let mut events = events(rt);
        let (_, channel, args) = events.pop().unwrap();
        assert_eq!(channel, "epub:flush-request");
        rt.send("epub:flush-result", sender, vec![args[0].clone(), result]);
    };
    let (answer, then) = slot();
    editor.read_text(view, then);
    flush(&mut rt, true.into_ipc());
    assert_eq!(*answer.borrow(), Some(Ok("hello".to_owned())));

    let (answer, then) = slot();
    editor.read_text(view, then);
    flush(&mut rt, s("composing"));
    assert_eq!(
        message(answer.take()),
        "typing in progress, try again shortly"
    );

    let (answer, then) = slot();
    editor.read_text(view, then);
    rt.advance(Duration::from_secs(10));
    assert_eq!(message(answer.take()), "timed out flushing the book");
}

#[test]
fn saving_to_a_path_and_its_failure_restores_the_target() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, path) = opened(&mut rt, 1);
    let view = sender.view;
    let editor = editor_for(&rt.provided(), view).unwrap();
    edit(&mut rt, sender, "hello");
    let elsewhere = rt.dir.join("copy.epub");

    let (answer, then) = slot();
    editor.save_to_path(view, PathBuf::from("/nowhere/x.epub"), then);
    rt.send("epub:save-request-ack", sender, vec![false.into_ipc()]);
    assert_eq!(message(answer.take()), "could not save the book");
    assert_eq!(editor.document_path(view), Some(PathBuf::from(&path)));

    let (answer, then) = slot();
    editor.save_to_path(view, elsewhere.clone(), then);
    assert_eq!(events(&rt).last().map(|e| e.1), Some("epub:save-request"));
    let saved = rt.invoke_now("epub:save", sender, vec![]).unwrap();
    assert_eq!(field(&saved, "path"), &s(&path_str(&elsewhere)));
    assert_eq!(*answer.borrow(), Some(Ok(())));
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"BOOKhello");
    assert_eq!(editor.document_path(view), Some(elsewhere));
}

#[test]
fn save_as_asks_for_a_file_and_tells_the_shell() {
    let (mut rt, shell) = rt(Value::Null);
    let (sender, path) = opened(&mut rt, 1);
    let save_as = || vec![obj([("mode", s("saveAs"))])];
    rt.script().save.push_back(None);
    assert_eq!(
        rt.invoke_now("epub:save", sender, save_as()),
        Ok(obj([
            ("ok", true.into_ipc()),
            ("canceled", true.into_ipc())
        ]))
    );
    let copy = rt.dir.join("copy");
    rt.script().save.push_back(Some(copy.clone()));
    let saved = rt.invoke_now("epub:save", sender, save_as()).unwrap();
    let target = format!("{}.epub", path_str(&copy));
    assert_eq!(field(&saved, "path"), &s(&target));
    assert_eq!(field(&saved, "written"), &true.into_ipc());
    assert_eq!(std::fs::read(&target).unwrap(), b"BOOK");
    assert_eq!(
        *shell.lock().unwrap(),
        vec![json!(["fileSavedHook", sender.view.0, target])]
    );
    let HostCall::Save(_, dialog) = &rt.script().calls[0] else {
        panic!()
    };
    assert_eq!(dialog.default_path.as_deref(), Some(Path::new(&path)));
    assert_eq!(dialog.title.as_deref(), Some("Save E-book"));
    assert_eq!(
        editors(&rt).document_path(sender.view),
        Some(PathBuf::from(target))
    );
}

#[test]
fn a_destroyed_view_answers_its_waiters_false() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, _) = opened(&mut rt, 1);
    let view = sender.view;
    let editor = editor_for(&rt.provided(), view).unwrap();
    edit(&mut rt, sender, "hello");
    let (save, then) = slot();
    editor.request_save(view, SaveMode::Save, then);
    let (read, then) = slot();
    editor.read_text(view, then);
    rt.destroy(view);
    assert_eq!(*save.borrow(), Some(Ok(false)));
    assert_eq!(message(read.take()), "could not flush the book");
    assert!(!editor.owns(view));
    assert_eq!(editor.document_path(view), None);
    assert!(editor_for(&rt.provided(), view).is_none());
}

#[test]
fn a_renamed_file_is_granted_and_announced() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, path) = opened(&mut rt, 1);
    let editor = editor_for(&rt.provided(), sender.view).unwrap();
    let renamed = rt.dir.join("renamed.epub");
    std::fs::rename(&path, &renamed).unwrap();
    editor.file_renamed(sender.view, Path::new(&path), &renamed);
    let new = path_str(&renamed);
    assert_eq!(
        events(&rt),
        vec![(
            Target::View(sender.view),
            "epub:file-renamed",
            vec![s(&new)]
        )]
    );
    assert_eq!(editor.document_path(sender.view), Some(renamed));
    assert_eq!(
        rt.invoke_now("epub:consume-pending", sender, vec![]),
        Ok(s(&new))
    );
    let reopened = rt
        .invoke_now("epub:open-file", sender, vec![s(&new)])
        .unwrap();
    assert_eq!(field(&reopened, "status"), &s("ok"));
}

#[test]
fn view_settings_reach_the_other_books_and_positions_stay() {
    let (mut rt, _) = rt(Value::Null);
    let (a, _) = opened(&mut rt, 1);
    let (b, _) = opened(&mut rt, 2);
    let prefs = rt
        .invoke_now(
            "epub:set-view-prefs",
            a,
            vec![obj([("fontScale", n(9.0)), ("dark", true.into_ipc())])],
        )
        .unwrap();
    let want = obj([
        ("fontScale", n(3.0)),
        ("lineHeight", IpcValue::Null),
        ("dark", true.into_ipc()),
    ]);
    assert_eq!(prefs, want);
    assert_eq!(
        events(&rt),
        vec![(
            Target::View(b.view),
            "epub:view-prefs-changed",
            vec![want.clone()]
        )]
    );

    let position = |spine: f64| obj([("spineIndex", n(spine)), ("blockId", s("b0"))]);
    rt.send("epub:set-read-position", a, vec![position(0.0)]);
    rt.send("epub:set-read-position", a, vec![position(7.0)]);
    let state = rt.invoke_now("epub:get-reading-state", a, vec![]).unwrap();
    assert_eq!(field(&state, "position"), &position(0.0));
    assert_eq!(field(&state, "viewPrefs"), &want);
    let other = rt.invoke_now("epub:get-reading-state", b, vec![]).unwrap();
    assert_eq!(field(&other, "position"), &IpcValue::Null);
}

#[test]
fn result_paths_avoid_the_book_and_open_files() {
    let (mut rt, _) = rt(json!({ "shell": { "openInTab": ["/tabs/open.epub"] } }));
    let (sender, path) = opened(&mut rt, 1);
    let (_, other) = opened(&mut rt, 2);
    let mut check = |p: &str| {
        let answer = rt
            .invoke_now("epub:check-result-path", sender, vec![s(p)])
            .unwrap();
        field(&answer, "reason").clone()
    };
    assert_eq!(check("relative.epub"), s("invalid"));
    assert_eq!(check("/tmp/a.txt"), s("not-epub"));
    assert_eq!(check(&path), s("source"));
    assert_eq!(check(&other), s("open-in-tab"));
    assert_eq!(check("/tabs/open.epub"), s("open-in-tab"));
    assert_eq!(check("/tmp/free.epub"), IpcValue::Undefined);
}

#[test]
fn a_translated_book_goes_only_to_a_picked_path() {
    let (mut rt, _) =
        rt(json!({ "engine": { "readTranslation": { "blocks": [{ "id": "b0" }] } } }));
    let (sender, path) = opened(&mut rt, 1);
    let doc_hash = field(
        &rt.invoke_now("epub:document-hash", sender, vec![]).unwrap(),
        "docHash",
    )
    .clone();
    let request = |kind: &str| obj([("kind", s(kind)), ("lang", s("de"))]);
    assert_eq!(
        rt.invoke_now("epub:pick-result-target", sender, vec![request("other")]),
        Ok(IpcValue::Null)
    );
    rt.script().save.push_back(Some(PathBuf::from(&path)));
    let refused = rt
        .invoke_now(
            "epub:pick-result-target",
            sender,
            vec![request("translated")],
        )
        .unwrap();
    assert_eq!(field(&refused, "reason"), &s("source"));
    let out = rt.dir.join("out");
    rt.script().save.push_back(Some(out.clone()));
    let picked = rt
        .invoke_now("epub:pick-result-path", sender, vec![request("bilingual")])
        .unwrap();
    let target = format!("{}.epub", path_str(&out));
    assert_eq!(picked, s(&target));
    let HostCall::Save(_, dialog) = rt.script().calls.last().unwrap().clone() else {
        panic!()
    };
    assert_eq!(
        dialog.default_path,
        Some(rt.dir.join("book1.de.bilingual.epub"))
    );
    assert_eq!(dialog.title.as_deref(), Some("Save Bilingual Book"));

    let write = |p: &str, hash: IpcValue| {
        obj([
            ("kind", s("bilingual")),
            ("lang", s("de")),
            ("path", s(p)),
            ("docHash", hash),
        ])
    };
    let unknown = rt
        .invoke_now(
            "epub:write-translated",
            sender,
            vec![write(&target, s("nope"))],
        )
        .unwrap();
    assert_eq!(field(&unknown, "message"), &s("unknown document hash"));
    let unpicked = rt
        .invoke_now(
            "epub:write-translated",
            sender,
            vec![write("/tmp/x.epub", doc_hash.clone())],
        )
        .unwrap();
    assert_eq!(field(&unpicked, "error"), &s("not-granted"));
    let written = rt
        .invoke_now(
            "epub:write-translated",
            sender,
            vec![write(&target, doc_hash.clone())],
        )
        .unwrap();
    assert_eq!(field(&written, "ok"), &true.into_ipc());
    assert_eq!(field(&written, "identifier"), &s("urn:uuid:fake"));
    assert_eq!(std::fs::read(&target).unwrap(), b"bilingual de 1 blocks");
    let twice = rt
        .invoke_now(
            "epub:write-translated",
            sender,
            vec![write(&target, doc_hash)],
        )
        .unwrap();
    assert_eq!(field(&twice, "error"), &s("not-granted"));
}

#[test]
fn the_store_answers_only_for_hashes_of_the_view() {
    let (mut rt, _) = rt(json!({ "engine": { "listTranslations": [["de", "ko"]] } }));
    let (sender, _) = opened(&mut rt, 1);
    let (stranger, _) = granted(&mut rt, 2);
    let doc_hash = field(
        &rt.invoke_now("epub:document-hash", sender, vec![]).unwrap(),
        "docHash",
    )
    .clone();
    let denied = rt
        .invoke_now("epub:store-translations", stranger, vec![doc_hash.clone()])
        .unwrap();
    assert_eq!(field(&denied, "error"), &s("not-granted"));
    assert_eq!(
        rt.invoke_now("epub:store-translations", sender, vec![doc_hash.clone()]),
        Ok(obj([
            ("ok", true.into_ipc()),
            ("langs", vec![s("de"), s("ko")].into_ipc())
        ]))
    );
    let bad_mode = rt
        .invoke_now(
            "epub:store-translation",
            sender,
            vec![doc_hash.clone(), s("de"), s("loud")],
        )
        .unwrap();
    assert_eq!(field(&bad_mode, "message"), &s("bad mode"));
    assert_eq!(
        rt.invoke_now("epub:store-unfinished-jobs", sender, vec![doc_hash.clone()]),
        Ok(obj([
            ("ok", true.into_ipc()),
            ("jobs", IpcValue::Array(vec![]))
        ]))
    );
    let written = rt
        .invoke_now(
            "epub:store-write",
            sender,
            vec![obj([
                ("kind", s("deleteTranslation")),
                ("docHash", doc_hash),
                ("lang", s("de")),
            ])],
        )
        .unwrap();
    assert_eq!(written, obj([("ok", true.into_ipc())]));
    assert_eq!(
        field(
            &rt.invoke_now("epub:store-write", sender, vec![s("x")])
                .unwrap(),
            "message"
        ),
        &s("request must be an object")
    );
}

#[test]
fn translation_units_check_the_chapters() {
    let (mut rt, _) = rt(Value::Null);
    let (stranger, _) = granted(&mut rt, 2);
    assert_eq!(
        field(
            &rt.invoke_now("epub:translation-units", stranger, vec![])
                .unwrap(),
            "error"
        ),
        &s("not-open")
    );
    let (sender, _) = opened(&mut rt, 1);
    let chapters = |c: Vec<IpcValue>| vec![obj([("chapters", c.into_ipc())])];
    let bad = rt
        .invoke_now("epub:translation-units", sender, chapters(vec![n(1.0)]))
        .unwrap();
    assert_eq!(field(&bad, "message"), &s("bad chapters"));
    let units = rt
        .invoke_now("epub:translation-units", sender, chapters(vec![n(0.0)]))
        .unwrap();
    assert_eq!(field(&units, "ok"), &true.into_ipc());
    assert_eq!(field(&units, "units"), &IpcValue::Array(vec![]));
}

#[test]
fn only_safe_links_are_opened() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, _) = opened(&mut rt, 1);
    assert_eq!(
        rt.invoke_now("epub:open-link", sender, vec![s(" https://example.org/a ")]),
        Ok(true.into_ipc())
    );
    assert_eq!(
        rt.invoke_now("epub:open-link", sender, vec![s("javascript:alert(1)")]),
        Ok(false.into_ipc())
    );
    assert_eq!(
        rt.script().calls,
        vec![HostCall::OpenExternal("https://example.org/a".into())]
    );
}

#[test]
fn the_renderers_dirty_flag_counts_only_for_an_open_book() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, path) = granted(&mut rt, 1);
    let eds = editors(&rt);
    rt.send("epub:dirty-changed", sender, vec![true.into_ipc()]);
    assert!(!eds.is_dirty(sender.view));
    rt.invoke_now("epub:open-file", sender, vec![s(&path)])
        .unwrap();
    rt.send("epub:dirty-changed", sender, vec![true.into_ipc()]);
    assert!(eds.is_dirty(sender.view));
    eds.menu_command(sender.view, "undo");
    assert_eq!(
        events(&rt),
        vec![(
            Target::View(sender.view),
            "epub:menu-command",
            vec![s("undo")]
        )]
    );
}

#[test]
fn attachments_are_checked_by_kind_and_size() {
    let (mut rt, _) = rt(json!({ "shell": { "uiLang": "ko" } }));
    let sender = rt.view(1, Surface::Epub);
    let text = rt.dir.join("notes.txt");
    std::fs::write(&text, "abcdefgh").unwrap();
    let exe = rt.dir.join("tool.exe");
    std::fs::write(&exe, "x").unwrap();
    let added = rt
        .invoke_now(
            "epub:files-add",
            sender,
            vec![
                vec![
                    s(&path_str(&text)),
                    s(&path_str(&exe)),
                    s(&path_str(&rt.dir.join("gone.txt"))),
                    n(5.0),
                ]
                .into_ipc(),
            ],
        )
        .unwrap();
    assert_eq!(
        field(&added, "accepted"),
        &vec![obj([
            ("path", s(&path_str(&text))),
            ("name", s("notes.txt")),
            ("ext", s("txt")),
            ("sizeBytes", n(8.0)),
        ])]
        .into_ipc()
    );
    assert_eq!(
        field(&added, "rejected"),
        &vec![
            s("tool.exe: .exe 형식은 지원되지 않습니다"),
            s("gone.txt: 읽을 수 없습니다")
        ]
        .into_ipc()
    );

    let png = shell_contract::BytesKind::Uint8Array;
    let pasted = rt
        .invoke_now(
            "epub:files-add-pasted-image",
            sender,
            vec![IpcValue::Bytes(png, vec![1, 2, 3]), s("PNG")],
        )
        .unwrap();
    let IpcValue::Array(accepted) = field(&pasted, "accepted") else {
        panic!()
    };
    let IpcValue::String(saved) = field(&accepted[0], "path") else {
        panic!()
    };
    assert!(saved.contains("seuoffice-pasted"));
    let big = rt
        .invoke_now(
            "epub:files-add-pasted-image",
            sender,
            vec![IpcValue::Bytes(png, vec![0; 5 * 1024 * 1024 + 1]), s("png")],
        )
        .unwrap();
    assert_eq!(
        field(&big, "rejected"),
        &vec![s("이미지가 5MB 제한을 초과했습니다")].into_ipc()
    );

    let image = rt
        .invoke_now("epub:files-read-image", sender, vec![s(saved)])
        .unwrap();
    assert_eq!(
        image,
        obj([
            ("ok", true.into_ipc()),
            ("base64", s("AQID")),
            ("mime", s("image/png"))
        ])
    );
    let not_image = rt
        .invoke_now("epub:files-read-image", sender, vec![s(&path_str(&text))])
        .unwrap();
    assert_eq!(
        field(&not_image, "error"),
        &s("notes.txt: 지원되지 않는 이미지 형식입니다")
    );

    rt.script().open.extend([None, Some(vec![text.clone()])]);
    assert_eq!(
        rt.invoke_now("epub:files-pick", sender, vec![]),
        Ok(IpcValue::Null)
    );
    let picked = rt.invoke_now("epub:files-pick", sender, vec![]).unwrap();
    assert_eq!(field(&picked, "rejected"), &IpcValue::Array(vec![]));
}

#[test]
fn an_attachment_is_read_a_page_at_a_time() {
    let (mut rt, _) = rt(Value::Null);
    let sender = rt.view(1, Surface::Epub);
    let text = rt.dir.join("notes.md");
    std::fs::write(&text, "abcdefgh").unwrap();
    let read = |rt: &mut Rt, offset: IpcValue, max: IpcValue| {
        rt.invoke_now(
            "epub:files-read",
            sender,
            vec![s(&path_str(&text)), offset, max],
        )
        .unwrap()
    };
    assert_eq!(
        read(&mut rt, n(2.0), s("3")),
        obj([
            ("ok", true.into_ipc()),
            ("name", s("notes.md")),
            ("totalChars", n(8.0)),
            ("offset", n(2.0)),
            ("text", s("cde")),
        ])
    );
    let all = read(&mut rt, IpcValue::Undefined, n(1e9));
    assert_eq!(field(&all, "text"), &s("abcdefgh"));
    let image = rt
        .invoke_now("epub:files-read", sender, vec![s("/tmp/a.png")])
        .unwrap();
    assert_eq!(
        field(&image, "error"),
        &s("Image attachments have no text; the image is sent along with the user message")
    );
}

#[test]
fn an_unsaved_open_file_is_not_read() {
    let (mut rt, _) = rt(Value::Null);
    let text = rt.dir.join("draft.txt");
    std::fs::write(&text, "secret").unwrap();
    let (mut rt2, _) = self::rt(json!({
        "shell": { "openFiles": [{ "filePath": path_str(&text), "dirty": true }] }
    }));
    let sender = rt2.view(1, Surface::Epub);
    let refused = rt2
        .invoke_now(
            "epub:files-read",
            sender,
            vec![s(&path_str(&text)), n(0.0), n(10.0)],
        )
        .unwrap();
    assert_eq!(
        field(&refused, "error"),
        &s("This file is open with unsaved changes. Save it, then try again.")
    );
    drop(rt2);
    let sender = rt.view(1, Surface::Epub);
    let ok = rt
        .invoke_now(
            "epub:files-read",
            sender,
            vec![s(&path_str(&text)), n(0.0), n(10.0)],
        )
        .unwrap();
    assert_eq!(field(&ok, "text"), &s("secret"));
}

#[test]
fn a_failed_open_files_probe_does_not_read_an_attachment() {
    let (mut rt, _) = rt(json!({ "shell": { "openFiles": "throws" } }));
    let sender = rt.view(1, Surface::Epub);
    let text = rt.dir.join("draft.txt");
    std::fs::write(&text, "secret").unwrap();
    let refused = rt
        .invoke_now(
            "epub:files-read",
            sender,
            vec![s(&path_str(&text)), n(0.0), n(10.0)],
        )
        .unwrap();
    assert_eq!(
        field(&refused, "error"),
        &s("This file is open with unsaved changes. Save it, then try again.")
    );
}

#[test]
fn delayed_open_files_probe_preserves_guard_and_ignores_destroyed_view() {
    let (mut rt, _) = rt(json!({ "shell": {
        "deferOpenFiles": true, "openFiles": "throws"
    } }));
    let text = rt.dir.join("draft.txt");
    std::fs::write(&text, "secret").unwrap();
    let sender = rt.view(1, Surface::Epub);
    let pending = rt.invoke(
        "epub:files-read",
        sender,
        vec![s(&path_str(&text)), n(0.0), n(10.0)],
    );
    assert!(pending.take().is_none());
    super::hosts::answer_open_files();
    assert_eq!(
        field(&pending.take().unwrap().unwrap(), "error"),
        &s("This file is open with unsaved changes. Save it, then try again.")
    );

    let sender = rt.view(2, Surface::Epub);
    let pending = rt.invoke(
        "epub:files-read",
        sender,
        vec![s(&path_str(&text)), n(0.0), n(10.0)],
    );
    rt.destroy(sender.view);
    super::hosts::answer_open_files();
    assert!(pending.take().is_none());
}

/// The probe's trigger steps reach the editor as the shell would
#[test]
fn probe_triggers_call_the_editor() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, path) = opened(&mut rt, 1);
    let view = sender.view;
    let (provided, services) = (rt.provided(), rt.services.clone());
    let trigger = |what: &str, args: Value| {
        let (answer, then) = slot();
        let known = super::probe_trigger(what, view, &args, &services, &provided, then);
        (known, answer)
    };
    let (known, answer) = trigger("file-path", json!([]));
    assert!(known);
    assert_eq!(*answer.borrow(), Some(Ok(s(&path))));
    let (_, answer) = trigger("menu-command", json!(["find"]));
    assert_eq!(*answer.borrow(), Some(Ok(IpcValue::Undefined)));
    assert_eq!(
        events(&rt),
        vec![(Target::View(view), "epub:menu-command", vec![s("find")])]
    );
    edit(&mut rt, sender, "x");
    let (_, answer) = trigger("d2/epub:request-save", json!(["save"]));
    assert_eq!(*answer.borrow(), Some(Ok(IpcValue::Undefined)));
    assert_eq!(
        events(&rt),
        vec![(Target::View(view), "epub:save-request", vec![s("save")])]
    );
    let (_, pending) = trigger("outcome", json!([]));
    assert_eq!(*pending.borrow(), Some(Ok(IpcValue::Null)));
    rt.send("epub:save-request-ack", sender, vec![true.into_ipc()]);
    let (_, settled) = trigger("outcome", json!([]));
    assert_eq!(
        *settled.borrow(),
        Some(Ok(obj([("value", true.into_ipc())])))
    );
    assert!(!trigger("export-request", json!([])).0);
}

#[test]
fn owned_document_capture_rejects_old_lease_without_relabeling_it() {
    use shell_contract::ipc::{NativeContext, NativeRequestOwner};
    use shell_contract::runtime::CancelToken;
    let (mut rt, _) = rt(Value::Null);
    let (sender, path) = opened(&mut rt, 91);
    let editor = editors(&rt);
    let generation = rt.services.views.renderer_generation(sender.view).unwrap();
    let native = NativeRequestOwner {
        view: sender.view,
        context: NativeContext::with_owner_loss((), generation, CancelToken::default()),
    };
    let captured = editor
        .capture_document_owner(sender.view, native.clone())
        .unwrap();
    editor.open_view(sender.view, Some(Path::new(&path)));
    assert!(!captured.context.document_lease().unwrap().is_active());
    assert!(
        editor
            .capture_document_owner(sender.view, captured)
            .is_err()
    );
    assert!(editor.capture_document_owner(sender.view, native).is_ok());
}

#[test]
fn owned_document_capture_does_not_accept_a_foreign_renderer() {
    use shell_contract::ipc::{NativeContext, NativeRequestOwner};
    use shell_contract::runtime::{CancelToken, RendererGeneration};
    let (mut rt, _) = rt(Value::Null);
    let (sender, path) = opened(&mut rt, 92);
    let native = NativeRequestOwner {
        view: sender.view,
        context: NativeContext::with_owner_loss(
            (),
            RendererGeneration::new(),
            CancelToken::default(),
        ),
    };
    assert!(
        editors(&rt)
            .capture_document_owner(sender.view, native)
            .is_err()
    );
    assert_eq!(
        editors(&rt).document_path(sender.view),
        Some(PathBuf::from(path))
    );
}

#[test]
fn a_foreign_owned_save_id_cannot_retarget_or_write_the_book() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, path) = opened(&mut rt, 93);
    let before = std::fs::read(&path).unwrap();
    let result = rt.invoke_now(
        "epub:save",
        sender,
        vec![obj([
            ("mode", s("save")),
            ("requestId", s("another-request")),
        ])],
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        editors(&rt).document_path(sender.view),
        Some(PathBuf::from(path))
    );
}

#[test]
fn a_foreign_owned_flush_cannot_apply_edits_to_the_current_book() {
    let (mut rt, _) = rt(Value::Null);
    let (sender, _) = opened(&mut rt, 95);
    let before = rt
        .invoke_now("epub:read-chapter", sender, vec![n(0.0)])
        .unwrap();
    let result = rt.invoke_now(
        "epub:apply-edits",
        sender,
        vec![obj([
            ("requestId", s("another-request")),
            ("spineIndex", n(0.0)),
            (
                "edits",
                vec![obj([
                    ("op", s("replace")),
                    ("blockId", s("b1")),
                    ("text", s("foreign")),
                ])]
                .into_ipc(),
            ),
        ])],
    );
    assert!(result.is_err());
    let after = rt
        .invoke_now("epub:read-chapter", sender, vec![n(0.0)])
        .unwrap();
    assert_eq!(after, before);
}

#[test]
fn native_recovery_requires_the_registered_renderer_and_real_book_before_ack() {
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision,
        RecoveryOpenContext, RecoveryOpenTarget,
    };
    use shell_contract::runtime::CancelToken;
    let (mut rt, _) = rt(Value::Null);
    let path = rt.dir.join("recovered.epub");
    std::fs::write(&path, b"BOOK").unwrap();
    let sender = rt.view(201, Surface::Epub);
    let other = rt.view(202, Surface::Epub);
    let generation = rt.services.views.renderer_generation(sender.view).unwrap();
    let effects = rt
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap();
    let owner = effects.capture_owner(sender.view, &generation).unwrap();
    let other_generation = rt.services.views.renderer_generation(other.view).unwrap();
    let foreign = effects
        .capture_owner(other.view, &other_generation)
        .unwrap();
    let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
        NativeInvocationSelection::RecoveryFile {
            surface: Surface::Epub,
            path: path.clone(),
            decision: NativeRecoveryDecision::Original,
        },
        CancelToken::default(),
    );
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let target = RecoveryOpenTarget::bind(
        RecoveryOpenContext::NativeStartup(operation),
        sender.view,
        generation,
        closure,
        browser_owner,
    )
    .unwrap();
    let completion = Rc::new(RefCell::new(None));
    let answer = completion.clone();
    editors(&rt).queue_recovery_direct(
        sender.view,
        &target,
        path_str(&path),
        Box::new(move |result| *answer.borrow_mut() = Some(result)),
    );
    assert!(matches!(&*completion.borrow(), Some(Ok(()))));
    let receipt = target.committed_seed().unwrap();
    target
        .invocation_target()
        .unwrap()
        .register_from_native_admitted(owner.clone())
        .unwrap();
    target.retain_native_registration(owner.clone()).unwrap();
    assert!(
        rt.invoke_native(
            "epub:consume-pending",
            sender,
            vec![],
            foreign.context,
            CancelToken::default()
        )
        .take()
        .unwrap()
        .is_err()
    );
    let seed = rt
        .invoke_native(
            "epub:consume-pending",
            sender,
            vec![],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap();
    assert_eq!(field(&seed, "path"), &s(&path_str(&path)));
    let token = field(&seed, "receipt").clone();
    assert!(
        rt.invoke_native(
            "epub:recovery-initialized",
            sender,
            vec![token.clone(), true.into_ipc()],
            owner.context.clone(),
            CancelToken::default()
        )
        .take()
        .unwrap()
        .is_err()
    );
    assert!(receipt.settlement().is_none());
    assert!(
        rt.invoke_native(
            "epub:open-file",
            sender,
            vec![s("other.epub")],
            owner.context.clone(),
            CancelToken::default()
        )
        .take()
        .unwrap()
        .is_err()
    );
    let book = rt
        .invoke_native(
            "epub:open-file",
            sender,
            vec![s(&path_str(&path))],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap();
    assert_eq!(field(&book, "status"), &s("ok"));
    assert!(receipt.settlement().is_none());
    assert!(
        rt.invoke_native(
            "epub:recovery-initialized",
            sender,
            vec![token.clone(), true.into_ipc()],
            owner.context.clone(),
            CancelToken::default()
        )
        .take()
        .unwrap()
        .is_ok()
    );
    assert!(matches!(receipt.settlement(), Some(Ok(()))));
    assert!(
        rt.invoke_native(
            "epub:recovery-initialized",
            sender,
            vec![token, true.into_ipc()],
            owner.context,
            CancelToken::default()
        )
        .take()
        .unwrap()
        .is_err()
    );
}

#[test]
fn native_recovery_refusal_settles_failure_without_a_book() {
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision,
        RecoveryOpenContext, RecoveryOpenTarget,
    };
    use shell_contract::runtime::CancelToken;
    let (mut rt, _) = rt(json!({ "engine": { "open": [{ "refused": "drm" }] } }));
    let path = rt.dir.join("protected.epub");
    std::fs::write(&path, b"BOOK").unwrap();
    let sender = rt.view(203, Surface::Epub);
    let generation = rt.services.views.renderer_generation(sender.view).unwrap();
    let effects = rt
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .unwrap();
    let owner = effects.capture_owner(sender.view, &generation).unwrap();
    let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
        NativeInvocationSelection::RecoveryFile {
            surface: Surface::Epub,
            path: path.clone(),
            decision: NativeRecoveryDecision::Original,
        },
        CancelToken::default(),
    );
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(sender.view, generation.clone());
    let target = RecoveryOpenTarget::bind(
        RecoveryOpenContext::NativeStartup(operation),
        sender.view,
        generation,
        closure,
        browser_owner,
    )
    .unwrap();
    editors(&rt).queue_recovery_direct(
        sender.view,
        &target,
        path_str(&path),
        Box::new(|result| assert!(result.is_ok())),
    );
    let receipt = target.committed_seed().unwrap();
    target
        .invocation_target()
        .unwrap()
        .register_from_native_admitted(owner.clone())
        .unwrap();
    target.retain_native_registration(owner.clone()).unwrap();
    let seed = rt
        .invoke_native(
            "epub:consume-pending",
            sender,
            vec![],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap();
    let token = field(&seed, "receipt").clone();
    let result = rt
        .invoke_native(
            "epub:open-file",
            sender,
            vec![s(&path_str(&path))],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap();
    assert_eq!(field(&result, "reason"), &s("drm"));
    assert!(
        rt.invoke_native(
            "epub:recovery-initialized",
            sender,
            vec![token, false.into_ipc(), s("drm")],
            owner.context,
            CancelToken::default()
        )
        .take()
        .unwrap()
        .is_ok()
    );
    assert!(matches!(receipt.settlement(), Some(Err(reason)) if reason == "drm"));
}

#[test]
fn native_recovery_never_queues_into_another_created_view() {
    use shell_contract::ipc::{
        NativeInvocationIssuer, NativeInvocationSelection, NativeRecoveryDecision,
        RecoveryOpenContext, RecoveryOpenTarget,
    };
    use shell_contract::runtime::CancelToken;
    let (mut rt, _) = rt(Value::Null);
    let path = rt.dir.join("selected.epub");
    let selected = rt.view(204, Surface::Epub);
    let other = rt.view(205, Surface::Epub);
    let generation = rt
        .services
        .views
        .renderer_generation(selected.view)
        .unwrap();
    let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
        NativeInvocationSelection::RecoveryFile {
            surface: Surface::Epub,
            path: path.clone(),
            decision: NativeRecoveryDecision::Original,
        },
        CancelToken::default(),
    );
    let (_issuer, closure, browser_owner) =
        shell_contract::ipc::NativeClosureIssuer::new_with_owner(selected.view, generation.clone());
    let target = RecoveryOpenTarget::bind(
        RecoveryOpenContext::NativeStartup(operation),
        selected.view,
        generation,
        closure,
        browser_owner,
    )
    .unwrap();
    let answer = Rc::new(RefCell::new(None));
    let result = answer.clone();
    editors(&rt).queue_recovery_direct(
        other.view,
        &target,
        path_str(&path),
        Box::new(move |value| *result.borrow_mut() = Some(value)),
    );
    assert!(matches!(&*answer.borrow(), Some(Err(_))));
    assert!(target.committed_seed().is_none());
    assert_eq!(
        rt.invoke_now("epub:consume-pending", other, vec![])
            .unwrap(),
        IpcValue::Null
    );
}
