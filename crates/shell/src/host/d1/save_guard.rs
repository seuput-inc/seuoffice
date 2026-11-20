//! Cross-editor dirty-file guard without blocking the core thread.

use std::sync::{Arc, Mutex};

use main_core::packages::electron_utils::unsaved_files::{
    OpenFileState, OpenFilesHolder, OpenFilesSourceFailed, UNSAVED_CHANGES_MESSAGE,
    assert_file_saved,
};
use shell_ipc_w::tabs::TabsState;

use super::super::w::on_core;

type Done = Box<dyn FnOnce(Result<(), String>) + Send>;
type Pending = Arc<Mutex<Option<Done>>>;

fn finish(pending: &Pending, result: Result<(), String>) {
    let callback = pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(callback) = callback {
        callback(result);
    }
}

pub(super) fn check(path: String, done: Done) {
    let pending = Arc::new(Mutex::new(Some(done)));
    let core_pending = pending.clone();
    let started = on_core(move |provided| {
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
            finish(&core_pending, Err(UNSAVED_CHANGES_MESSAGE.to_owned()));
            return;
        };
        tabs.open_documents(Box::new(move |documents| {
            let result = match documents {
                Ok(documents) => {
                    let files: Vec<OpenFileState> = documents
                        .into_iter()
                        .map(|document| OpenFileState {
                            file_path: document.file_path,
                            dirty: document.dirty,
                        })
                        .collect();
                    let files = std::cell::RefCell::new(Some(files));
                    let mut holder = OpenFilesHolder::new();
                    holder.set_open_files_source(Some(Box::new(move || {
                        files.borrow_mut().take().ok_or(OpenFilesSourceFailed)
                    })));
                    assert_file_saved(&holder, &path).map_err(str::to_owned)
                }
                Err(_) => Err(UNSAVED_CHANGES_MESSAGE.to_owned()),
            };
            finish(&core_pending, result);
        }));
    });
    if started.is_none() {
        finish(&pending, Err(UNSAVED_CHANGES_MESSAGE.to_owned()));
    }
}

/// Called on the core thread; the callback and its continuation stay there.
pub(crate) fn check_local(path: String, done: Box<dyn FnOnce(Result<(), String>)>) {
    let pending = std::rc::Rc::new(std::cell::RefCell::new(Some(done)));
    let core_pending = pending.clone();
    let started = super::super::w::on_core_local(move |provided| {
        let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
            let done = core_pending.borrow_mut().take();
            if let Some(done) = done {
                done(Err(UNSAVED_CHANGES_MESSAGE.to_owned()));
            }
            return;
        };
        tabs.open_documents(Box::new(move |documents| {
            let result = match documents {
                Ok(documents) => {
                    let files: Vec<OpenFileState> = documents
                        .into_iter()
                        .map(|document| OpenFileState {
                            file_path: document.file_path,
                            dirty: document.dirty,
                        })
                        .collect();
                    let files = std::cell::RefCell::new(Some(files));
                    let mut holder = OpenFilesHolder::new();
                    holder.set_open_files_source(Some(Box::new(move || {
                        files.borrow_mut().take().ok_or(OpenFilesSourceFailed)
                    })));
                    assert_file_saved(&holder, &path).map_err(str::to_owned)
                }
                Err(_) => Err(UNSAVED_CHANGES_MESSAGE.to_owned()),
            };
            let done = core_pending.borrow_mut().take();
            if let Some(done) = done {
                done(result);
            }
        }));
    });
    if !started {
        let done = pending.borrow_mut().take();
        if let Some(done) = done {
            done(Err(UNSAVED_CHANGES_MESSAGE.to_owned()));
        }
    }
}
