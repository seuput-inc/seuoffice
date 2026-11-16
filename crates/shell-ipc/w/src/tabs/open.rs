//! Opening documents into tabs: the extension router (`routeDocumentPath`),
//! the "new document" tabs of home and the "+" menu (`newDocTab`,
//! `newSheetTab`, `newPdfTab`, ...), the folder a new file's first save goes
//! to (`rememberPendingDir`, `bindPendingDir`) and files dropped from the OS
//! (`app:open-dropped-files`).

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;

thread_local! {
    static DIAGRAM_CHECKING: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

use jscompat::node_path::host::{basename, join, resolve};
use jscompat::{js_regex, trim};
use main_core::shell::app_routing::{
    diagram_re, docx_re, epub_re, html_re, hwp_re, mindmap_re, odt_re, pdf_re, pptx_re, text_re,
    word97_re, xlsx_re,
};
use main_core::shell::app_settings::read_app_settings;
use main_core::shell::folder_roots::read_extra_roots;
use main_core::shell::tab_manager::TabKind;
use shell_contract::ids::ViewId;
use shell_contract::value::IpcValue;

mod owned;
pub(super) use owned::{AdoptedEditorLifetime, OwnedCreationCleanup, PendingOwnedOpen};
pub use owned::{
    AdoptedLifetimeFailure, AdoptedLifetimeHandle, CallerOwnedAdoptedLifetime,
    RetainedAdoptedLifetime,
};
pub use owned::{
    CommittedGeneratedTab, GeneratedEditorOpen, PreparedGeneratedOperation, native_open_surface,
    retire_adopted_editor_receipts, rollback_editor_receipts,
};

fn document_kind(path: &str) -> Option<TabKind> {
    if docx_re().test(path) || odt_re().test(path) || word97_re().test(path) {
        Some(TabKind::Docs)
    } else if xlsx_re().test(path) {
        Some(TabKind::Sheets)
    } else if pptx_re().test(path) {
        Some(TabKind::Slides)
    } else if pdf_re().test(path) {
        Some(TabKind::Pdf)
    } else if text_re().test(path) {
        Some(TabKind::Markdown)
    } else if html_re().test(path) {
        Some(TabKind::Html)
    } else if mindmap_re().test(path) {
        Some(TabKind::Mindmap)
    } else if hwp_re().test(path) {
        Some(TabKind::Hwp)
    } else if epub_re().test(path) {
        Some(TabKind::Epub)
    } else if diagram_re().test(path) {
        Some(TabKind::Diagram)
    } else {
        None
    }
}

use super::state::{McpBlank, TabsState, app_settings_path};
use super::wire::string;
use shell_contract::runtime::PathName;

/// `PENDING_DIR_TTL_MS`: a pending folder only applies to a file created
/// within this window after the click
const PENDING_DIR_TTL_MS: f64 = 30.0 * 60.0 * 1000.0;

/// `MAX_DROPPED_FILES`
const MAX_DROPPED_FILES: usize = 20;

/// What a "new document" entry opens
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NewKind {
    Doc,
    Sheet,
    Slide,
    Markdown,
    Html,
    Mindmap,
    Diagram,
    Hwp,
    Pdf,
}

impl NewKind {
    /// `rememberPendingDir`'s key
    fn key(self) -> &'static str {
        match self {
            NewKind::Doc => "doc",
            NewKind::Sheet => "sheet",
            NewKind::Slide => "slide",
            NewKind::Markdown => "markdown",
            NewKind::Html => "html",
            NewKind::Mindmap => "mindmap",
            NewKind::Diagram => "diagram",
            NewKind::Hwp => "hwp",
            NewKind::Pdf => "pdf",
        }
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

impl TabsState {
    /// `revealShellWindow`: show does not un-minimize, and on macOS closing the
    /// window leaves the app running, so a file opened from the outside would
    /// land out of sight
    pub fn reveal_shell_window(&self) {
        let shell = {
            let mut g = self.inner.borrow_mut();
            g.ensure_manager();
            g.bridge.shell
        };
        let Some(shell) = shell else {
            return;
        };
        if self.windows.window_state(shell.window).minimized {
            self.windows.window_restore(shell.window);
        }
        self.windows.window_show(shell.window);
        self.windows.window_focus(shell.window);
    }

    /// `openDocumentPath`: a copy of a Drive file reopens through Drive, else the extension
    /// decides which module owns the file. `done` runs once: `Ok(false)` when nothing opened.
    pub fn open_document_path(&self, path: &str, done: Box<dyn FnOnce(Result<bool, String>)>) {
        if self.hooks.drive_reopen(path) {
            return done(Ok(true));
        }
        self.route_document_path(path, done);
    }

    /// `routeDocumentPath` through the native `OpenFile` command: a detached window showing the
    /// file is focused, else the extension picks the editor. `done` runs once.
    pub fn route_document_path(&self, path: &str, done: Box<dyn FnOnce(Result<bool, String>)>) {
        self.hooks.route_path_native(path.to_owned(), done);
    }

    /// The completion of an open nobody waits for: a failure is logged, never dropped.
    pub fn report_open(&self, path: &str) -> Box<dyn FnOnce(Result<bool, String>)> {
        let state = self.rc();
        let shown = path.to_owned();
        Box::new(move |result| {
            if let Err(error) = result {
                state.hooks.warn(
                    "[shell] opening a document failed:",
                    &format!("{shown}: {error}"),
                );
            }
        })
    }

    // ---- new documents ----

    /// `newDocTab`, `newSheetTab`, ... one tab for the kind, in the folder the
    /// click remembered
    pub(super) fn new_tab(&self, kind: NewKind) {
        match kind {
            NewKind::Sheet => self.new_sheet_tab(),
            NewKind::Pdf => self.new_pdf_tab(),
            _ => self.open_new_native(kind),
        }
    }

    /// `surfaceNewTabError`: a throw anywhere in the create-tab path must show,
    /// or the click looks like a no-op
    fn surface_new_tab_error(&self, error: &str) {
        self.hooks.warn("[shell] new tab failed:", error);
        self.hooks
            .show_error_dialog(&self.hooks.tm("errNewTabFailed"), error);
    }

    /// `newSheetTab`: the blank workbook is made in a temp folder and its first
    /// Save goes through Save As, so a new tab closed unsaved leaves nothing behind
    fn new_sheet_tab(&self) {
        let hooks = &self.hooks;
        let dir = self.new_file_dir("sheet");
        let untitled = hooks.tm("untitledSheet");
        let mut suggested = hooks.unique_path_in(&dir, &format!("{untitled}.xlsx"));
        let mut i = 2;
        while hooks.sheets_suggested_path_taken(&suggested) {
            suggested = hooks.unique_path_in(&dir, &format!("{untitled} {i}.xlsx"));
            i += 1;
        }
        let temp = self.services.paths.path(PathName::Temp);
        let temp_dir = join(&[
            &temp.to_string_lossy(),
            "seuoffice-imports",
            &hooks.random_uuid(),
        ]);
        let attempt = (|| -> Result<String, String> {
            std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
            let backing = join(&[&temp_dir, &basename(&suggested, None)]);
            hooks.mark_sheets_unsaved_new(&backing, &suggested, &temp_dir);
            if let Err(error) = hooks.blank_xlsx().and_then(|bytes| {
                main_core::packages::electron_utils::atomic_write::atomic_write_file(
                    &backing, &bytes,
                )
                .map_err(|error| error.message)
            }) {
                hooks.unmark_sheets_unsaved_new(&backing);
                return Err(error);
            }
            // eligible for content-derived auto-rename after the first AI generation
            hooks.mark_sheets_untitled_path(&backing);
            Ok(backing)
        })();
        let backing = match attempt {
            Ok(backing) => backing,
            Err(error) => return self.new_sheet_fallback(&suggested, &error),
        };
        let state = self.rc();
        self.route_document_path(
            &backing,
            Box::new(move |result| match result {
                Ok(true) => state.hooks.start_queued_workbook_nudge(),
                Ok(false) => state.new_sheet_fallback(&suggested, "the workbook did not open"),
                Err(error) => state.new_sheet_fallback(&suggested, &error),
            }),
        );
    }

    /// The temp workbook could not be made or opened: write the blank into the default folder,
    /// else open an in-memory blank tab
    fn new_sheet_fallback(&self, suggested: &str, error: &str) {
        let hooks = &self.hooks;
        hooks.warn(
            "[shell] temp workbook create failed, writing to the default folder:",
            error,
        );
        let fallback = hooks.blank_xlsx().and_then(|bytes| {
            main_core::packages::electron_utils::atomic_write::atomic_write_file(suggested, &bytes)
                .map_err(|error| error.message)
        });
        match fallback {
            Ok(()) => {
                hooks.mark_sheets_untitled_path(suggested);
                self.route_document_path(suggested, self.report_open(suggested));
            }
            Err(e) => {
                hooks.warn(
                    "[shell] blank workbook create failed, opening in-memory blank tab:",
                    &e,
                );
                self.open_new_native(NewKind::Sheet);
            }
        }
    }

    /// `newPdfTab`: a blank single-page .pdf in the chosen folder, opened as a
    /// regular file tab (the PDF module has no in-memory blank mode)
    fn new_pdf_tab(&self) {
        let hooks = &self.hooks;
        let dir = self.new_file_dir("pdf");
        let path = hooks.unique_path_in(&dir, &format!("{}.pdf", hooks.tm("untitledPdf")));
        match main_core::packages::electron_utils::atomic_write::atomic_write_file(
            &path,
            &hooks.blank_pdf(),
        ) {
            Ok(()) => {
                // opt the file into content-derived auto-naming on its first save
                hooks.mark_pdf_untitled_path(&path);
                self.route_document_path(&path, self.report_open(&path));
            }
            Err(e) => self.surface_new_tab_error(&e.message),
        }
    }

    // ---- the folder of a new file ----

    /// `rememberPendingDir`: the click remembers the folder per kind
    pub(super) fn remember_pending_dir(&self, kind: &str, opts: Option<&IpcValue>) {
        let dir = opts
            .and_then(|o| crate::convert::field(o, "dir"))
            .and_then(|d| string(Some(d)))
            .filter(|d| !d.is_empty());
        let default_dir = self.hooks.default_save_dir();
        let mut g = self.inner.borrow_mut();
        match dir {
            Some(dir)
                if resolve(&[dir]) != resolve(&[&default_dir]) && self.inside_any_root(dir) =>
            {
                g.pending_new_dir
                    .insert(kind.to_owned(), (dir.to_owned(), now_ms()));
            }
            _ => {
                g.pending_new_dir.remove(kind);
            }
        }
    }

    /// `insideAnyRoot`: the default save folder or a folder the person added
    fn inside_any_root(&self, dir: &str) -> bool {
        let default_dir = self.hooks.default_save_dir();
        let settings = read_app_settings(&app_settings_path(&self.services));
        let mut roots = vec![default_dir.clone()];
        roots.extend(read_extra_roots(&settings, &default_dir));
        roots
            .iter()
            .any(|root| self.hooks.is_inside_root(root, dir))
    }

    /// `takePendingDir`: the folder remembered for this kind, consumed; `None`
    /// when none, expired or gone
    fn take_pending_dir(&self, kind: &str) -> Option<(String, f64)> {
        let pending = self.inner.borrow_mut().pending_new_dir.remove(kind)?;
        if now_ms() - pending.1 > PENDING_DIR_TTL_MS {
            return None;
        }
        Path::new(&pending.0).exists().then_some(pending)
    }

    /// `newFileDir`: where a shell-created blank file lands
    fn new_file_dir(&self, kind: &str) -> String {
        self.take_pending_dir(kind)
            .map_or_else(|| self.hooks.default_save_dir(), |p| p.0)
    }

    /// `bindPendingDir`: hand the remembered folder to the tab just opened for it
    fn bind_pending_dir(&self, kind: &str, tab_id: Option<&str>) {
        let pending = self.take_pending_dir(kind);
        let view = {
            let g = self.inner.borrow();
            tab_id.and_then(|id| g.manager.as_ref().and_then(|m| m.web_contents_for_tab(id)))
        };
        if let (Some(pending), Some(view)) = (pending, view) {
            self.inner
                .borrow_mut()
                .pending_dir_by_wc
                .insert(view.0 as i64, pending);
        }
    }

    /// The folder bound to a new tab's view, until its first save consumes it
    /// (`pendingDirByWc.get(wcId)`): the saved-file hooks of the editors call
    /// this and move the fresh file there
    pub fn pending_dir_for(&self, view: ViewId) -> Option<(String, f64)> {
        self.inner
            .borrow()
            .pending_dir_by_wc
            .get(&(view.0 as i64))
            .cloned()
    }

    /// `pendingDirByWc.delete(wcId)`
    pub fn clear_pending_dir_for(&self, view: ViewId) {
        self.inner
            .borrow_mut()
            .pending_dir_by_wc
            .remove(&(view.0 as i64));
    }
    pub fn apply_pending_dir(&self, view: ViewId, file_path: &str) -> String {
        let Some((dir, set_at)) = self.pending_dir_for(view) else {
            return file_path.to_owned();
        };
        if now_ms() - set_at > PENDING_DIR_TTL_MS {
            self.clear_pending_dir_for(view);
            return file_path.to_owned();
        }
        let path = Path::new(file_path);
        let root = self.hooks.default_save_dir();
        if !super::owned_saved::pending_source_is_eligible(file_path, &root, set_at) {
            return file_path.to_owned();
        }
        self.clear_pending_dir_for(view);
        if !Path::new(&dir).exists() {
            return file_path.to_owned();
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return file_path.to_owned();
        };
        let target = self.hooks.unique_path_in(&dir, name);
        if let Err(error) = std::fs::rename(path, &target) {
            self.hooks
                .warn("move new file into folder failed", &error.to_string());
            return file_path.to_owned();
        }
        match self
            .provided
            .all::<crate::home_files::Home>()
            .into_iter()
            .next()
        {
            Some(home) => {
                if let Err(error) = home.after_file_moved(file_path, &target) {
                    self.hooks
                        .warn("updating moved file failed", &error.to_string());
                }
            }
            None => self
                .hooks
                .warn("updating moved file failed", "home service unavailable"),
        }
        target
    }

    // ---- dropped files ----

    /// `handleDroppedFiles(raw, deps)`: open each openable path (last drop wins
    /// activation), reveal the shell if anything opened, and surface one
    /// combined warning for known-but-unsupported formats. A skill file goes to
    /// the skills import preview; a zip or `SKILL.md` that holds no skill gets
    /// one notice, and only when the drop gave the app nothing else to do.
    pub fn handle_dropped_files(&self, raw: Option<&IpcValue>) {
        let (supported, skill_files, unsupported) = partition_drop_payload(raw);
        let state = self.rc();
        let rest = std::rc::Rc::new(RefCell::new(Some((skill_files, unsupported))));
        let finish: std::rc::Rc<dyn Fn(bool)> = std::rc::Rc::new(move |opened: bool| {
            let Some((skill_files, unsupported)) = rest.borrow_mut().take() else {
                return;
            };
            if opened {
                state.reveal_shell_window();
            }
            if !unsupported.is_empty() {
                let message = state
                    .hooks
                    .tm_params("errUnsupportedExt", &[("ext", &unsupported.join(", "))]);
                state.hooks.show_app_warning(&message, None);
            }
            state.drop_skill_files(
                skill_files.into_iter(),
                opened || !unsupported.is_empty(),
                false,
            );
        });
        if supported.is_empty() {
            return finish(false);
        }
        let remaining = std::rc::Rc::new(std::cell::Cell::new(supported.len()));
        let opened = std::rc::Rc::new(std::cell::Cell::new(false));
        for path in &supported {
            let (finish, remaining, opened) = (finish.clone(), remaining.clone(), opened.clone());
            let shown = path.clone();
            let state = self.rc();
            self.open_document_path(
                path,
                Box::new(move |result| {
                    match result {
                        Ok(true) => opened.set(true),
                        Ok(false) => {}
                        Err(error) => state.hooks.warn(
                            "[shell] opening a dropped file failed:",
                            &format!("{shown}: {error}"),
                        ),
                    }
                    remaining.set(remaining.get() - 1);
                    if remaining.get() == 0 {
                        finish(opened.get());
                    }
                }),
            );
        }
    }

    fn drop_skill_files(&self, mut paths: std::vec::IntoIter<String>, acted: bool, missed: bool) {
        let Some(path) = paths.next() else {
            if missed && !acted {
                self.hooks.drop_no_skills_found();
            }
            return;
        };
        let state = self.rc();
        self.hooks.drop_skill_file(
            &path,
            Box::new(move |taken| {
                state.drop_skill_files(paths, acted || taken, missed || !taken);
            }),
        );
    }
}

impl TabsState {
    pub fn follow_opener_project(
        &self,
        view: ViewId,
        opener: &main_core::shell::projects::new_documents::NewDocumentOpener,
    ) -> Result<(), String> {
        if self.services.views.sender_of(view).is_none() {
            return Err("the new document view no longer exists".to_owned());
        }
        let projects = self
            .provided
            .all::<crate::projects::Projects>()
            .into_iter()
            .next()
            .ok_or_else(|| "project services are not registered".to_owned())?;
        let placer = projects.new_documents();
        let mut placer = placer.borrow_mut();
        let home = self.shell_window().map(|shell| shell.view.0 as i64);
        let placement = placer.placement_of_opener(Some(opener), home);
        placer.preset(view.0 as i64, placement);
        Ok(())
    }

    /// Writes a Sheets template copy as the backing workbook of a new unsaved workbook and
    /// returns `(backing path, temp folder)`; `Ok(None)` when there is no shell window.
    fn prepare_sheets_template(
        &self,
        copy: &main_core::shell::user_skills::template_tabs::TemplateCopy,
    ) -> Result<Option<(String, String)>, String> {
        if self.shell_window().is_none() {
            return Ok(None);
        }
        let path = Path::new(&copy.file_name);
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or(&copy.title);
        let save_extension = if extension.eq_ignore_ascii_case("xlsm") {
            "xlsm"
        } else {
            "xlsx"
        };
        let dir = self.new_file_dir("sheet");
        let mut suggested = self
            .hooks
            .unique_path_in(&dir, &format!("{stem}.{save_extension}"));
        let mut number = 2;
        while self.hooks.sheets_suggested_path_taken(&suggested) {
            suggested = self
                .hooks
                .unique_path_in(&dir, &format!("{stem} {number}.{save_extension}"));
            number += 1;
        }
        let temp = self.services.paths.path(PathName::Temp);
        let temp_dir = join(&[
            &temp.to_string_lossy(),
            "seuoffice-imports",
            &self.hooks.random_uuid(),
        ]);
        std::fs::create_dir_all(&temp_dir).map_err(|error| error.to_string())?;
        let backing = join(&[&temp_dir, &copy.file_name]);
        self.hooks
            .mark_sheets_unsaved_new(&backing, &suggested, &temp_dir);
        if let Err(error) = main_core::packages::electron_utils::atomic_write::atomic_write_file(
            &backing,
            &copy.bytes,
        ) {
            self.abort_sheets_template(&backing, &temp_dir);
            return Err(error.message);
        }
        Ok(Some((backing, temp_dir)))
    }

    fn abort_sheets_template(&self, backing: &str, temp_dir: &str) {
        self.hooks.unmark_sheets_unsaved_new(backing);
        if let Err(cleanup) = std::fs::remove_dir_all(temp_dir) {
            self.hooks.warn(
                "[shell] could not remove the unused template workbook:",
                &cleanup.to_string(),
            );
        }
    }

    /// `openTemplateCopy`: a skill template's copy as a new untitled document of its editor,
    /// opened through the owned creation path for `requester`'s captured owner (none: the shell
    /// window). `Ok(None)` for EPUB and when there is no shell window. `done` runs once, after
    /// the tab exists or the open failed and its creation cleanup settled.
    #[allow(clippy::type_complexity)]
    pub fn open_template_copy_owned(
        &self,
        copy: &main_core::shell::user_skills::template_tabs::TemplateCopy,
        requester: Option<ViewId>,
        done: Box<dyn FnOnce(Result<Option<(String, ViewId)>, String>)>,
    ) {
        use main_core::shell::tab_manager::{EditorOpen, TabManager, TemplateCopy as TabCopy};
        fn editor_open<V: Clone + PartialEq, B: Clone + PartialEq>(
            _: &TabManager<V, B>,
            copy: &TabCopy,
        ) -> Option<EditorOpen> {
            TabManager::<V, B>::template_editor_open(copy)
        }
        let Some(shell) = self.shell_window() else {
            return done(Ok(None));
        };
        let view = requester.unwrap_or(shell.view);
        let Some(effects) = self
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .cloned()
        else {
            return done(Err("Owned creation effects are unavailable".into()));
        };
        let captured = self
            .services
            .views
            .renderer_generation(view)
            .ok_or("The requesting renderer is unavailable")
            .and_then(|generation| {
                effects
                    .capture_owner(view, &generation)
                    .map_err(|_| "The requesting owner is unavailable")
            });
        let owner = match captured {
            Ok(owner) => owner,
            Err(error) => return done(Err(error.into())),
        };
        let Some(cancel) = owner.loss_token() else {
            return done(Err("The requesting owner has no lifetime".into()));
        };
        let tab_copy = TabCopy {
            editor: copy.editor.as_str().to_owned(),
            path: copy.path.clone(),
            bytes: copy.bytes.clone(),
            file_name: copy.file_name.clone(),
            title: copy.title.clone(),
        };
        let sheets = copy.editor.as_str() == "sheets";
        let (request, backing) = if sheets {
            match self.prepare_sheets_template(copy) {
                Ok(Some((backing, temp_dir))) => (
                    Some(EditorOpen::Sheets {
                        path: Some(backing.clone()),
                        new_blank: false,
                    }),
                    Some((backing, temp_dir)),
                ),
                Ok(None) => return done(Ok(None)),
                Err(error) => return done(Err(error)),
            }
        } else {
            let request = self
                .with_manager(|manager, _| editor_open(manager, &tab_copy))
                .flatten();
            (request, None)
        };
        let Some(request) = request else {
            return done(Ok(None));
        };
        if tab_copy.editor == "pdf" {
            self.hooks.mark_pdf_untitled_copy(&tab_copy.path);
        }
        let state = self.rc();
        self.open_editor_owned(
            request,
            owner,
            cancel,
            Box::new(move |result| match result {
                Ok((id, view, _)) if sheets => {
                    state.hooks.start_queued_workbook_nudge();
                    done(Ok(Some((id, view))));
                }
                Ok((id, _, _)) => {
                    let tab = state
                        .with_manager(|manager, host| {
                            manager.finish_template_copy(host, &tab_copy, id)
                        })
                        .flatten();
                    done(Ok(tab));
                }
                Err(error) => {
                    if let Some((backing, temp_dir)) = backing {
                        state.abort_sheets_template(&backing, &temp_dir);
                    }
                    done(Err(error));
                }
            }),
        );
    }

    /// `openHtmlPresentTab`: a chrome-free html tab showing `owner`'s live preview. The view is
    /// created through the host's asynchronous creation hook; `done` runs once the tab exists,
    /// or with the failure after any creation cleanup settled. It never runs twice.
    pub fn open_html_present_owned(
        &self,
        owner: ViewId,
        title: String,
        done: Box<dyn FnOnce(Result<ViewId, String>)>,
    ) {
        use main_core::shell::tab_manager::{ViewCreationCleanupState, ViewCreationFailure};
        let state = self.rc();
        let epoch = self.manager_epoch.get();
        let spec_title = title.clone();
        self.hooks.create_html_present_view(
            i64::from(owner.0),
            &title,
            Box::new(move |result| match result {
                Ok(ready) => {
                    let view = ready.view;
                    let binding = super::host::RetainedSpareBinding::new(
                        ready.reservation,
                        std::rc::Rc::default(),
                        None,
                    );
                    let tab = if epoch == state.manager_epoch.get() {
                        state.with_manager(|manager, host| {
                            manager.resume_html_present_tab_bound(host, view, &spec_title, binding)
                        })
                    } else {
                        None
                    };
                    match tab {
                        Some(_) => done(Ok(view)),
                        None => {
                            let failed = state.clone();
                            state.retain_editor_cleanup(
                                Some(ready.cleanup),
                                None,
                                std::rc::Rc::default(),
                                true,
                                Box::new(move |failure| {
                                    failed
                                        .hooks
                                        .warn("[shell] html present view cleanup failed:", failure);
                                }),
                                Box::new(move || {
                                    done(Err("The shell window is no longer available".into()));
                                }),
                            );
                        }
                    }
                }
                Err(ViewCreationFailure { error, cleanup }) => {
                    let (cleanup, proof) = match cleanup {
                        ViewCreationCleanupState::Complete => (None, None),
                        ViewCreationCleanupState::Acknowledged(proof) => (None, Some(proof)),
                        ViewCreationCleanupState::Pending(cleanup) => (Some(cleanup), None),
                    };
                    let original = error.0.clone();
                    let failed = state.clone();
                    state.retain_editor_cleanup(
                        cleanup,
                        proof,
                        std::rc::Rc::default(),
                        true,
                        Box::new(move |failure| {
                            failed.hooks.warn(
                                "[shell] html present view cleanup failed:",
                                &format!("{original}; {failure}"),
                            );
                        }),
                        Box::new(move || done(Err(error.0))),
                    );
                }
            }),
        );
    }

    pub fn abandon_mcp_blank_docs(&self, view: ViewId) {
        if !matches!(
            self.inner.borrow().mcp_blank.get(&view),
            Some(McpBlank::Docs)
        ) {
            return;
        }
        let state = self.this.clone();
        self.hooks.docs_query_dirty(
            view,
            Box::new(move |dirty| {
                if matches!(dirty, Ok(false))
                    && let Some(state) = state.upgrade()
                {
                    let owned = matches!(
                        state.inner.borrow().mcp_blank.get(&view),
                        Some(McpBlank::Docs)
                    );
                    if owned && state.close_mcp_blank(view, TabKind::Docs) {
                        state.inner.borrow_mut().mcp_blank.remove(&view);
                    }
                }
            }),
        );
    }

    pub fn abandon_mcp_blank_sheets(&self, view: ViewId) {
        if !matches!(
            self.inner.borrow().mcp_blank.get(&view),
            Some(McpBlank::Sheets)
        ) {
            return;
        }
        let dirty = match self.with_manager(|manager, host| {
            manager
                .dirty_tabs(host, TabKind::Sheets)
                .map(|tabs| tabs.iter().any(|(_, candidate)| *candidate == view))
        }) {
            Some(Ok(dirty)) => dirty,
            Some(Err(error)) => {
                self.hooks
                    .warn("[mcp] could not check blank workbook:", &error.0);
                return;
            }
            None => {
                self.hooks.warn(
                    "[mcp] could not check blank workbook:",
                    "SeuOffice is not ready",
                );
                return;
            }
        };
        if !dirty && self.close_mcp_blank(view, TabKind::Sheets) {
            self.inner.borrow_mut().mcp_blank.remove(&view);
        }
    }

    /// Closes a blank deck tab whose editing session never became ready. Only called for a
    /// tab the MCP opened itself, so there is no user content to lose.
    pub fn abandon_mcp_blank_slides(&self, view: ViewId) {
        if matches!(
            self.inner.borrow().mcp_blank.get(&view),
            Some(McpBlank::Slides)
        ) && self.close_mcp_blank(view, TabKind::Slides)
        {
            self.inner.borrow_mut().mcp_blank.remove(&view);
        }
    }

    fn close_mcp_blank(&self, view: ViewId, kind: TabKind) -> bool {
        self.with_manager(|manager, host| {
            let Some(id) = manager.tab_id_for_web_contents(host, view.0 as i64) else {
                return false;
            };
            if !manager
                .list()
                .iter()
                .any(|tab| tab.id == id && tab.kind == kind)
            {
                return false;
            }
            manager.close_tab_without_prompt(host, &id)
        })
        .unwrap_or(false)
    }
}

/// `partitionDropPayload`: strings only, trimmed, deduped, capped, then split
/// into directly-openable paths, files that may hold skills, and
/// known-unsupported extensions (unique, first-seen order)
fn partition_drop_payload(raw: Option<&IpcValue>) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut supported, mut skills, mut unsupported) = (Vec::new(), Vec::new(), Vec::new());
    let Some(IpcValue::Array(entries)) = raw else {
        return (supported, skills, unsupported);
    };
    let mut seen: Vec<String> = Vec::new();
    for entry in entries {
        let IpcValue::String(entry) = entry else {
            continue;
        };
        let path = trim(entry).to_owned();
        if path.is_empty() || seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        if js_regex!(r"(?:^|[\\/])skill\.md$|\.zip$", "i").test(&path) {
            if skills.len() < MAX_DROPPED_FILES {
                skills.push(path);
            }
        } else if js_regex!(
            r"\.(docx|doc|dot|odt|xlsx|xlsm|xls|csv|tsv|ods|pptx|ppt|odp|otp|pdf|md|markdown|html|htm|xmind|mm|hwp|hwpx|epub|drawio|dio|vsdx|drawio\.svg|drawio\.png)$",
            "i"
        )
        .test(&path)
        {
            if supported.len() < MAX_DROPPED_FILES {
                supported.push(path);
            }
        } else if js_regex!(r"\.(rtf|pps|xlsb|pages|key|numbers)$", "i").test(&path) {
            let ext = path[path.rfind('.').map_or(0, |i| i + 1)..].to_lowercase();
            if !unsupported.contains(&ext) {
                unsupported.push(ext);
            }
        }
    }
    (supported, skills, unsupported)
}
