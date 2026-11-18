//! What the shell asks of a diagram view besides the renderer's own channels:
//! the exported functions of `diagram-main.ts` (`grantAndTrack`,
//! `requestDiagramClose`, `requestDiagramSave`, `diagramSaveToPath`,
//! `writeDiagramCopiesNow`, `diagramFileRenamed`, ...). A TypeScript promise
//! is a `done` callback that runs on the core thread when the wait ends.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use main_core::packages::electron_utils::recovery_store::RecoveryIdentity;
use shell_contract::editors::SaveMode;
use shell_contract::hosts::MessageBox;
use shell_contract::{IpcValue, ViewId, WindowId};

use super::DiagramViews;
use super::formats::{Detected, DiagramFormat, detect_format, diagram_base_name};
use super::state::{CopyNow, Decision, Import, Unopened, WaitEnd, Waiter};
use crate::value::string;

/// How long a save or a close waits for the renderer
const SAVE_TIMEOUT: Duration = Duration::from_secs(120);

pub fn prepare_hidden_open_path(path: Option<&str>) -> Option<&str> {
    path.filter(|path| !path.is_empty() && Path::new(path).exists())
}

fn mode_name(mode: SaveMode) -> &'static str {
    match mode {
        SaveMode::Save => "save",
        SaveMode::SaveAs => "saveAs",
    }
}

/// `DiagramTabIdentity`: what the shell keeps about a tab to find it again
#[derive(Clone, Debug, PartialEq)]
pub struct TabIdentity {
    pub source_path: Option<String>,
    pub untitled: bool,
    pub title: String,
    /// `imported`: `vsdx` for a diagram made from a Visio drawing
    pub imported: Option<&'static str>,
}

/// An ordinary diagram actor prepared before the native browser is submitted.
pub struct PreparedNativeDiagram {
    instance: u64,
    view: ViewId,
    owner: shell_contract::ipc::NativeBrowserOwner,
    path: Option<String>,
    allowed: Option<HashSet<String>>,
    import: Option<Import>,
    document: Option<shell_contract::runtime::RendererGeneration>,
    committed: bool,
}

impl PreparedNativeDiagram {
    pub fn commit(
        &mut self, transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if !transition.matches_owner(&self.owner) || self.committed {
            return Err("The diagram browser transition does not match".into());
        }
        let views = super::lookup(self.instance).ok_or("The original diagram actor is unavailable on this core thread")?;
        views.with_state(|state| {
            if state.views.contains_key(&self.view) || state.views.len() == state.views.capacity() {
                return Err("The reserved diagram actor is unavailable".into());
            }
            next()?;
            let staged = super::state::ViewState {
                tracked: true, save_revision: Some(0),
                unopened: self.path.as_ref().map(|_| Unopened::Pending),
                open_path: self.path.take(), allowed: self.allowed.take(), import: self.import.take(),
                document: self.document.take(), browser_owner: Some(self.owner.clone()), ..Default::default()
            };
            state.views.insert(self.view, staged);
            self.committed = true;
            Ok(())
        })
    }
}

impl DiagramViews {
    pub fn prepare_native_tab(
        &self, view: ViewId, created: &shell_contract::runtime::RendererGeneration,
        open_path: Option<&str>, owner: shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<PreparedNativeDiagram, String> {
        if !owner.reservation().matches(view, created) {
            return Err("The diagram reservation does not match".into());
        }
        self.with_state(|state| {
            if state.views.contains_key(&view) { return Err("The diagram actor already exists".into()); }
            state.views.try_reserve(1).map_err(|_| "The diagram actor could not be reserved".to_owned())
        })?;
        let path = prepare_hidden_open_path(open_path).map(str::to_owned);
        let allowed = path.as_ref().map(|path| HashSet::from([path.clone()]));
        let import = path.as_deref().filter(|path| detect_format(path) == Some(Detected::Vsdx))
            .map(|path| Import { path: path.to_owned(), title: diagram_base_name(path) });
        Ok(PreparedNativeDiagram {
            instance: self.0.id, view, owner, path, allowed, import,
            document: Some(shell_contract::runtime::RendererGeneration::new()), committed: false,
        })
    }
}

impl DiagramViews {
    /// `grantAndTrack(wc, openPath)` without the webContents guards: the
    /// shell calls it when it creates a diagram tab. `open_path` is the file
    /// the tab opens, if any; it is granted to the view when it exists.
    pub fn open_view(&self, view: ViewId, open_path: Option<&str>) {
        self.replace_save_document(view);
        self.commit_open_view(view, prepare_hidden_open_path(open_path));
    }

    fn commit_open_view(&self, view: ViewId, open_path: Option<&str>) {
        self.with_state(|state| {
            let entry = state.view(view);
            entry.tracked = true;
            if let Some(path) = open_path {
                entry.open_path = Some(path.to_owned());
                entry.allowed = Some(HashSet::from([path.to_owned()]));
                entry.unopened = Some(Unopened::Pending);
                if detect_format(path) == Some(Detected::Vsdx) {
                    entry.import = Some(Import {
                        path: path.to_owned(),
                        title: diagram_base_name(path),
                    });
                }
            }
        });
    }

    /// `createDiagramHiddenWindow`'s state: the same grant, and the file as
    /// saved (no recovery question)
    pub fn open_hidden_view(&self, view: ViewId, path: &str) {
        self.replace_save_document(view);
        self.commit_open_view(view, prepare_hidden_open_path(Some(path)));
        self.set_view_open_decision(view, Decision::Original);
    }

    /// The caller checked the path off the core thread before admission.
    pub fn open_hidden_view_prepared(&self, view: ViewId, path: Option<&str>) {
        self.replace_save_document(view);
        self.commit_open_view(view, path);
        self.set_view_open_decision(view, Decision::Original);
    }

    pub fn open_fresh_hidden_view_prepared(&self, view: ViewId, path: Option<&str>) -> Result<(), String> {
        if self.with_state(|state| state.views.contains_key(&view) || state.recovery.contains_key(&view)) {
            return Err("The diagram hidden view is already in use".into());
        }
        self.open_hidden_view_prepared(view, path);
        Ok(())
    }

    /// `diagramIsDirty(webContentsId)`
    pub fn is_dirty(&self, view: ViewId) -> bool {
        self.with_state(|state| state.views.get(&view).is_some_and(|v| v.dirty))
    }

    /// `diagramFilePath(webContentsId)`
    pub fn file_path(&self, view: ViewId) -> Option<String> {
        self.with_state(|state| state.views.get(&view).and_then(|v| v.save_path.clone()))
    }

    /// `diagramTabIdentity(webContentsId)`: none for a view that is not a
    /// diagram view
    pub fn tab_identity(&self, view: ViewId) -> Option<TabIdentity> {
        let (saved, import, opening) = self.with_state(|state| {
            let entry = state.views.get(&view).filter(|v| v.tracked)?;
            Some((
                entry.save_path.clone(),
                entry.import.clone(),
                entry.open_path.clone(),
            ))
        })?;
        if let Some(saved) = saved {
            return Some(TabIdentity {
                title: diagram_base_name(&saved),
                source_path: Some(saved),
                untitled: false,
                imported: None,
            });
        }
        if let Some(import) = import {
            return Some(TabIdentity {
                source_path: Some(import.path),
                untitled: true,
                title: import.title,
                imported: Some("vsdx"),
            });
        }
        if let Some(opening) = opening {
            return Some(TabIdentity {
                title: diagram_base_name(&opening),
                source_path: Some(opening),
                untitled: false,
                imported: None,
            });
        }
        let title = self
            .0
            .shell
            .recovery_title(view)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| self.tm("untitled"));
        Some(TabIdentity {
            source_path: None,
            untitled: true,
            title,
            imported: None,
        })
    }

    /// `diagramFileRenamed(contents, oldPath, newPath)`: the file was renamed
    /// on disk; re-grant the new path and tell the renderer
    pub fn file_renamed(&self, view: ViewId, old_path: &str, new_path: &str) {
        self.with_state(|state| {
            let entry = state.view(view);
            if entry.save_path.as_deref() == Some(old_path) {
                entry.save_path = Some(new_path.to_owned());
                entry.advance_save_revision();
            }
            if entry.open_path.as_deref() == Some(old_path) {
                entry.advance_save_revision();
                entry.open_path = Some(new_path.to_owned());
            }
            if let Some(allowed) = &mut entry.allowed
                && allowed.contains(old_path)
            {
                allowed.insert(new_path.to_owned());
            }
            if let Some(recorded) = entry.disk_states.remove(old_path) {
                entry.disk_states.insert(new_path.to_owned(), recorded);
            }
        });
        self.recovery_view_file_renamed(view, old_path, new_path);
        if self.alive(view) {
            self.emit(view, "diagram:file-renamed", vec![string(new_path)]);
        }
    }

    /// `requestDiagramClose(contents, parent)`: `done(true)` means proceed
    /// with closing. Clean: true; dirty: Save / Don't Save / Cancel. On Save
    /// the renderer saves and its result is awaited (`close-save-result`).
    pub fn request_close(
        &self,
        view: ViewId,
        parent: Option<WindowId>,
        done: Box<dyn FnOnce(bool)>,
    ) {
        let (may_write, dirty) = self.with_state(|state| {
            state
                .views
                .get(&view)
                .map_or((true, false), |v| (v.may_write(), v.dirty))
        });
        if !may_write || !dirty || !self.alive(view) {
            return done(true);
        }
        let options = MessageBox {
            kind: Some("warning".into()),
            message: self.tm("closeUnsavedMsg"),
            detail: Some(self.tm("closeUnsavedDetail")),
            buttons: vec![
                self.tm("btnSave"),
                self.tm("btnDontSave"),
                self.tm("btnCancel"),
            ],
            default_id: Some(0),
            cancel_id: Some(2),
            ..MessageBox::default()
        };
        let response = self.0.dialogs.message(parent, &options).response;
        if response == 2 {
            return done(false);
        }
        if response == 1 {
            self.view_dropped(view);
            return done(true);
        }
        let generation = self.with_state(|state| {
            let generation = state.next_generation();
            state.view(view).close_save = Some(Waiter {
                generation,
                resume: Box::new(move |end| done(end.ok())),
            });
            generation
        });
        self.after(SAVE_TIMEOUT, move |views| {
            let waiter = views.with_state(|state| {
                let entry = state.views.get_mut(&view)?;
                if entry.close_save.as_ref().map(|w| w.generation) == Some(generation) {
                    entry.close_save.take()
                } else {
                    None
                }
            });
            if let Some(waiter) = waiter {
                (waiter.resume)(WaitEnd::TimedOut);
            }
        });
        self.emit(view, "diagram:close-save-request", vec![]);
    }

    /// `requestDiagramSave(contents, mode)`: the menu's Save / Save As. The
    /// renderer saves; a clean diagram with a file resolves true at once and
    /// writes nothing.
    pub fn request_save(&self, view: ViewId, mode: SaveMode, done: Box<dyn FnOnce(bool)>) {
        let (may_write, dirty, has_path) = self.with_state(|state| {
            state.views.get(&view).map_or((true, false, false), |v| {
                (v.may_write(), v.dirty, v.save_path.is_some())
            })
        });
        if !self.alive(view) || !may_write {
            return done(false);
        }
        if mode == SaveMode::Save && !dirty && has_path {
            return done(true);
        }
        self.wait_for_save(view, Box::new(move |end| done(end.ok())));
        self.emit(view, "diagram:save-request", vec![string(mode_name(mode))]);
    }

    /// Stores `done` as the view's save waiter and arms its timeout
    fn wait_for_save(&self, view: ViewId, done: Box<dyn FnOnce(WaitEnd)>) {
        let generation = self.with_state(|state| {
            let generation = state.next_generation();
            state.view(view).save = Some(Waiter {
                generation,
                resume: done,
            });
            generation
        });
        self.after(SAVE_TIMEOUT, move |views| {
            let waiter = views.with_state(|state| {
                let entry = state.views.get_mut(&view)?;
                if entry.save.as_ref().map(|w| w.generation) == Some(generation) {
                    entry.save.take()
                } else {
                    None
                }
            });
            if let Some(waiter) = waiter {
                (waiter.resume)(WaitEnd::TimedOut);
            }
        });
    }

    /// `diagramSaveToPath(contents, filePath)`: save the live diagram to
    /// `file_path` with no dialog (the in-app MCP server's save). The format
    /// follows the name. A diagram with no change that already lives there
    /// is not written.
    pub fn save_to_path(
        &self,
        view: ViewId,
        file_path: &str,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        if !self.alive(view) {
            return done(Err("the diagram is no longer open".into()));
        }
        let (may_write, same_clean, previous) = self.with_state(|state| {
            state.views.get(&view).map_or((true, false, None), |v| {
                (
                    v.may_write(),
                    v.save_path.as_deref() == Some(file_path) && !v.dirty,
                    v.save_path.clone(),
                )
            })
        });
        if !may_write {
            return done(Err(
                "the file did not open, so there is no diagram to save".into()
            ));
        }
        if same_clean {
            return done(Ok(()));
        }
        self.with_state(|state| {
            let entry = state.view(view);
            entry.save_path = Some(file_path.to_owned());
            entry.advance_save_revision();
            entry
                .allowed
                .get_or_insert_with(HashSet::new)
                .insert(file_path.to_owned());
        });
        let retarget = previous.as_deref() != Some(file_path);
        let views = self.clone();
        self.wait_for_save(
            view,
            Box::new(move |end| {
                let failure = match end {
                    WaitEnd::Answer(true) => return done(Ok(())),
                    WaitEnd::Answer(false) => "could not save the diagram",
                    WaitEnd::TimedOut => "timed out saving the diagram",
                };
                views.with_state(|state| { let entry = state.view(view); entry.save_path = previous; entry.advance_save_revision(); });
                done(Err(failure.into()));
            }),
        );
        self.emit(
            view,
            "diagram:save-request",
            vec![string(if retarget { "retarget" } else { "save" })],
        );
    }

    /// `discardDiagramRecovery(contents)`: a tool closes the diagram without
    /// saving; the unsaved work and its recovery copy are dropped
    pub fn discard_recovery(&self, view: ViewId) {
        self.view_dropped(view);
    }

    /// `diagramRecoveryIdentity(webContentsId)`: the copy this view writes,
    /// for the shell's session record; none for a non-diagram view
    pub fn recovery_identity(&self, view: ViewId) -> Option<RecoveryIdentity> {
        let save_path = self.with_state(|state| {
            state
                .views
                .get(&view)
                .filter(|v| v.tracked)
                .map(|v| v.save_path.clone())
        })?;
        Some(self.view_recovery_identity(view, save_path.as_deref()))
    }

    /// `writeDiagramCopiesNow(timeoutMs)`: the OS is ending the session; every
    /// diagram view with unsaved changes writes a copy now. `done` runs when
    /// every view answered (`recovery-copy-now-done`) or `timeout` passed.
    pub fn write_copies_now(&self, timeout: Duration, done: Box<dyn FnOnce(Result<(), main_core::shell::renderer_disposal::Failure>)>) {
        let mut waiting: Vec<ViewId> = self.with_state(|state| {
            state
                .views
                .iter()
                .filter(|(_, v)| v.tracked && v.dirty && v.may_write())
                .map(|(view, _)| *view)
                .collect()
        });
        let mut unavailable = false;
        waiting.retain(|view| {
            let alive = self.alive(*view);
            unavailable |= !alive;
            alive
        });
        waiting.sort();
        let initial_failure = unavailable.then(|| main_core::shell::renderer_disposal::Failure("The diagram view was closed".into()));
        if waiting.is_empty() {
            return done(initial_failure.map_or(Ok(()), Err));
        }
        let remaining = Rc::new(Cell::new(waiting.len()));
        let done = Rc::new(RefCell::new(Some(done)));
        let failure = Rc::new(RefCell::new(initial_failure));
        for view in waiting {
            let finish = {
                let remaining = remaining.clone();
                let done = done.clone();
                let failure = failure.clone();
                move |result: Result<(), main_core::shell::renderer_disposal::Failure>| {
                    if let Err(error) = result {
                        let mut first = failure.borrow_mut();
                        if first.is_none() { *first = Some(error); }
                    }
                    remaining.set(remaining.get() - 1);
                    if remaining.get() == 0 {
                        let callback = done.borrow_mut().take();
                        let result = failure.borrow_mut().take().map_or(Ok(()), Err);
                        if let Some(done) = callback { done(result); }
                    }
                }
            };
            let (previous, generation) = self.with_state(|state| {
                let generation = state.next_generation();
                let previous = state.view(view).copy_now.replace(CopyNow {
                    generation,
                    finish: Box::new(finish),
                });
                (previous, generation)
            });
            if let Some(previous) = previous {
                (previous.finish)(Err(main_core::shell::renderer_disposal::Failure("Diagram recovery request was replaced".into())));
            }
            self.after(timeout, move |views| {
                let waiter = views.with_state(|state| {
                    let entry = state.views.get_mut(&view)?;
                    if entry.copy_now.as_ref().map(|w| w.generation) == Some(generation) {
                        entry.copy_now.take()
                    } else {
                        None
                    }
                });
                if let Some(waiter) = waiter {
                    (waiter.finish)(Err(main_core::shell::renderer_disposal::Failure("Timed out writing the diagram recovery copy".into())));
                }
            });
            self.emit(view, "diagram:recovery-copy-now", vec![crate::value::num(generation as f64)]);
        }
    }


    /// `queueDiagramSeedFor(contents, seed)`: content for a new, unsaved
    /// diagram view, taken once by `seed-take`
    pub fn queue_seed(&self, view: ViewId, xml: &str) {
        if !self.alive(view) {
            return;
        }
        self.with_state(|state| state.view(view).seed = Some(xml.to_owned()));
    }

    /// `sendDiagramMenuCommand(contents, command)`: undo, redo, zoom and the like
    pub fn send_menu_command(&self, view: ViewId, command: &str) {
        if self.alive(view) {
            self.emit(view, "diagram:menu-command", vec![string(command)]);
        }
    }

    /// `sendDiagramExportRequest(contents, format)`: the menu's export items
    pub fn send_export_request(&self, view: ViewId, format: &str) {
        if self.alive(view) {
            self.emit(view, "diagram:export-request", vec![string(format)]);
        }
    }

    /// The `{ xml }` object `seed-take` answers
    pub(super) fn seed_object(xml: String) -> IpcValue {
        crate::value::obj([("xml", string(xml))])
    }

    /// `formatOfPath` for the shell: a diagram format by file name
    pub fn format_of(path: &str) -> DiagramFormat {
        super::formats::format_of_path(path)
    }
}
