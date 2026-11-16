//! PDF tools hooks into live shell tabs.

use std::cell::RefCell;
use std::rc::Rc;
use main_core::shell::tab_manager::{TabKind as CoreKind, canonical_path};
use shell_contract::ViewId;
use shell_ipc_d2::pdf::editor::editors::PdfEditors;
use shell_ipc_d2::pdf::tools::{PdfTab, TabKind};
use shell_ipc_w::tabs::TabsState;
use crate::host::w::{on_core, on_core_local};

pub(super) fn active_pdf_tab() -> Option<PdfTab> {
    on_core(|provided| {
        let tabs = provided.all::<TabsState>().into_iter().next()?;
        let tab = tabs.with_manager(|manager, _| manager.active_tab_of_kind(CoreKind::Pdf)).flatten()?;
        Some(PdfTab { id: tab.id, view: tab.view, file_path: tab.file_path, window: tabs.dialog_parent(tab.view) })
    }).flatten()
}

pub(super) fn find_tab(kind: TabKind, path: &str) -> Option<String> {
    let kind = match kind { TabKind::Docs => CoreKind::Docs, TabKind::Slides => CoreKind::Slides, TabKind::Sheets => CoreKind::Sheets, TabKind::Markdown => CoreKind::Markdown };
    let path = path.to_owned();
    on_core(move |provided| provided.all::<TabsState>().into_iter().next()?.with_manager(|manager, _| manager.find_tab_of_kind_by_path(kind, Some(&path))).flatten()).flatten()
}

pub(super) fn open_document_path(path: &str, then: Box<dyn FnOnce(bool) + Send>) {
    let shown = path.to_owned();
    crate::host::d2::docs_tabs::open_path_native(path.to_owned(), Box::new(move |result| match result {
        Ok(opened) => then(opened),
        Err(error) => {
            eprintln!("shell: opening {shown}: {error}");
            then(false);
        }
    }));
}

/// A tab already showing `path` reloads in place; otherwise the file opens as a generated one.
/// `true`: handled here, including the reveal-in-folder fallback of a failed open.
pub(super) fn open_result(path: &str, include_text: bool) -> bool {
    let owned = path.to_owned();
    let reused = on_core(move |provided| {
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else { return false };
        if include_text { tabs.reveal_shell_window(); }
        let lower = owned.to_ascii_lowercase();
        let kind = if lower.ends_with(".pdf") {
            Some(CoreKind::Pdf)
        } else if include_text && [".md", ".markdown", ".txt", ".json"].iter().any(|ext| lower.ends_with(ext)) {
            Some(CoreKind::Markdown)
        } else {
            None
        };
        let existing = kind.and_then(|kind| tabs.with_manager(|manager, _| manager.find_tab_of_kind_by_path(kind, Some(&owned))).flatten());
        let Some(id) = existing else { return false };
        tabs.with_manager(|manager, host| manager.reload_tab(host, &id));
        tabs.reveal_tab(&id);
        true
    }).unwrap_or(false);
    reused || crate::host::d2::docs_tabs::open_generated(path).unwrap_or(false)
}

pub(super) fn open_for_editing(path: &str) {
    on_core(|provided| {
        if let Some(tabs) = provided.all::<TabsState>().into_iter().next() { tabs.reveal_shell_window(); }
    });
    open_document_path(path, Box::new(|_| {}));
}

/// Opens (and, with `text`, appends to) a note tab; `done` runs once on the core thread:
/// `Ok(false)` when the path is no note or the tab or editor is missing.
pub(super) fn note(path: &str, text: Option<&str>, done: Box<dyn FnOnce(Result<bool, String>)>) {
    if !main_core::shell::pdf_note_tabs::is_note_path(path) ||
        text.is_some_and(|value| jscompat::trim(value).is_empty()) {
        return done(Ok(false));
    }
    let pending = Rc::new(RefCell::new(Some(done)));
    let local = pending.clone();
    let path = path.to_owned();
    let text = text.map(str::to_owned);
    let started = on_core_local(move |provided| {
        let Some(done) = local.borrow_mut().take() else { return };
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else { return done(Ok(false)) };
        let editor = provided.all::<shell_ipc_d2::markdown::MarkdownEditors>().into_iter().next();
        let opened = tabs.clone();
        let shown = path.clone();
        tabs.open_document_path(&shown, Box::new(move |result| {
            let outcome = match result {
                Ok(true) => match opened.view_of_file(&path) {
                    None => Ok(false),
                    Some(view) => match (text, editor) {
                        (Some(text), Some(editor)) => {
                            editor.queue_note_append(view, &path, &text);
                            Ok(true)
                        }
                        (Some(_), None) => Ok(false),
                        (None, _) => Ok(true),
                    },
                },
                other => other,
            };
            done(outcome);
        }));
    });
    if !started && let Some(done) = pending.borrow_mut().take() { done(Err("Core services are not available".to_owned())); }
}

pub(super) fn open_tools(path: &str, tool: &str) -> Option<bool> {
    let path = path.to_owned();
    let tool = tool.to_owned();
    on_core(move |provided| provided.all::<shell_ipc_d2::pdf::tools::PdfTools>()
        .into_iter().next().map(|tools| tools.open_with_file(&path, &tool))).flatten()
}

pub(super) fn create_document_owned(request: shell_contract::IpcValue, owner: ViewId, origin: shell_contract::ipc::NativeRequestOwner, destination: shell_contract::ipc::NativeRequestOwner, cancel: shell_contract::runtime::CancelToken, pending: shell_contract::runtime::CancelToken, admission: main_core::packages::electron_utils::atomic_write::owned::Admission, done: Box<dyn FnOnce(Result<shell_contract::IpcValue, String>) + Send>) {
    crate::host::w::post_core(move |provided, _| {
        let Some(editor) = provided.all::<shell_ipc_d2::docs::editors::DocsEditors>().into_iter().next() else { return done(Err("pdf: document creation is unavailable in this host".into())); };
        editor.create_ai_document_owned(request, owner, origin, destination, cancel, pending, admission, done);
    });
}

pub(super) fn activate_tab(id: &str) {
    let id = id.to_owned();
    on_core(move |provided| { if let Some(tabs) = provided.all::<TabsState>().into_iter().next() { tabs.reveal_tab(&id); } });
}

pub(super) fn activate_view(view: ViewId) {
    on_core(move |provided| { if let Some(tabs) = provided.all::<TabsState>().into_iter().next() && let Some(id) = tabs.tab_id_for(view) { tabs.reveal_tab(&id); } });
}

pub(super) fn has_tab_manager() -> bool {
    on_core(|provided| provided.all::<TabsState>().into_iter().next().is_some_and(|tabs| tabs.with_manager(|_, _| ()).is_some())).unwrap_or(false)
}

/// Opens the imported-PDF Hangul copy through the native `HwpCopy` command; `done` runs on the
/// core thread once the tab exists or the open failed.
pub(super) fn open_hwp_copy(copy: &str, title: &str, done: Box<dyn FnOnce(Result<Option<ViewId>, String>)>) {
    let pending = Rc::new(RefCell::new(Some(done)));
    let local = pending.clone();
    let copy = copy.to_owned();
    let title = title.to_owned();
    let started = on_core_local(move |provided| {
        let Some(done) = local.borrow_mut().take() else { return };
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
            return done(Err("The tab manager is not available".to_owned()));
        };
        let cancel = shell_contract::runtime::CancelToken::default();
        let command = shell_contract::ipc::NativeEditorCommand::HwpCopy { path: std::path::PathBuf::from(&copy), title: title.clone() };
        let operation = match crate::host::native_commands::issue(command, cancel.clone()) {
            Ok(operation) => operation,
            Err(error) => return done(Err(error)),
        };
        tabs.open_hwp_copy_native(copy, title, operation, Box::new(move |result| {
            drop(cancel);
            done(result.map(Some));
        }));
    });
    if !started && let Some(done) = pending.borrow_mut().take() { done(Err("Core services are not available".to_owned())); }
}

pub(super) fn close_tab(id: &str, done: Box<dyn FnOnce(Result<(), String>)>) {
    let pending = Rc::new(RefCell::new(Some(done)));
    let local = pending.clone();
    let id = id.to_owned();
    let started = on_core_local(move |provided| {
        let Some(done) = local.borrow_mut().take() else { return };
        match provided.all::<TabsState>().into_iter().next() {
            Some(tabs) => tabs.close_tab(&id, done),
            None => done(Err("The tab manager is not available".to_owned())),
        }
    });
    if !started && let Some(done) = pending.borrow_mut().take() { done(Err("Core services are not available".to_owned())); }
}

pub(super) fn flush_pdf_save(view: ViewId, done: Box<dyn FnOnce(bool)>) {
    let pending = Rc::new(RefCell::new(Some(done)));
    let local = pending.clone();
    let started = on_core_local(move |provided| {
        let Some(done) = local.borrow_mut().take() else { return };
        match provided.all::<PdfEditors>().into_iter().next() {
            Some(editors) => editors.flush_save(view, true, done),
            None => done(false),
        }
    });
    if !started && let Some(done) = pending.borrow_mut().take() { done(false); }
}

pub(super) fn file_has_unsaved_changes(path: &str, done: Box<dyn FnOnce(bool)>) {
    let pending = Rc::new(RefCell::new(Some(done)));
    let local = pending.clone();
    let path = path.to_owned();
    let started = on_core_local(move |provided| {
        let Some(done) = local.borrow_mut().take() else { return };
        match provided.all::<TabsState>().into_iter().next() {
            Some(tabs) => tabs.open_documents(Box::new(move |result| {
                let dirty = result.map(|documents| {
                    let wanted = canonical_path(Some(&path));
                    documents.iter().any(|doc| doc.dirty && wanted.is_some() && canonical_path(doc.file_path.as_deref()) == wanted)
                }).unwrap_or(true);
                done(dirty);
            })),
            None => done(true),
        }
    });
    if !started && let Some(done) = pending.borrow_mut().take() { done(true); }
}
