//! Opening documents into tabs: the extension router (`routeDocumentPath`),
//! the "new document" tabs of home and the "+" menu (`newDocTab`,
//! `newSheetTab`, `newPdfTab`, ...), the folder a new file's first save goes
//! to (`rememberPendingDir`, `bindPendingDir`) and files dropped from the OS
//! (`app:open-dropped-files`).

use std::path::Path;
use std::cell::RefCell;
use std::collections::HashSet;

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
use main_core::shell::tab_manager::{OpenDocsOptions, TabHost, TabKind};
use shell_contract::ids::ViewId;
use shell_contract::value::IpcValue;

mod owned;
pub(super) use owned::{AdoptedEditorLifetime, OwnedCreationCleanup, PendingOwnedOpen};
pub use owned::{CommittedGeneratedTab, GeneratedEditorOpen, PreparedGeneratedOperation, native_open_surface, rollback_editor_receipts, retire_adopted_editor_receipts};
pub use owned::{AdoptedLifetimeFailure, AdoptedLifetimeHandle, CallerOwnedAdoptedLifetime, RetainedAdoptedLifetime};

fn document_kind(path: &str) -> Option<TabKind> {
    if docx_re().test(path) || odt_re().test(path) || word97_re().test(path) { Some(TabKind::Docs) }
    else if xlsx_re().test(path) { Some(TabKind::Sheets) }
    else if pptx_re().test(path) { Some(TabKind::Slides) }
    else if pdf_re().test(path) { Some(TabKind::Pdf) }
    else if text_re().test(path) { Some(TabKind::Markdown) }
    else if html_re().test(path) { Some(TabKind::Html) }
    else if mindmap_re().test(path) { Some(TabKind::Mindmap) }
    else if hwp_re().test(path) { Some(TabKind::Hwp) }
    else if epub_re().test(path) { Some(TabKind::Epub) }
    else if diagram_re().test(path) { Some(TabKind::Diagram) }
    else { None }
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

    /// `openDocumentPath`: a copy of a Drive file reopens through Drive, else
    /// the extension decides which module owns the file
    pub fn open_document_path(&self, path: &str) -> bool {
        if self.hooks.drive_reopen(path) {
            return true;
        }
        self.route_document_path(path)
    }

    /// `routeDocumentPath`: `false` means nothing opened
    pub fn route_document_path(&self, path: &str) -> bool {
        if !Path::new(path).exists() {
            return false;
        }
        let hooks = &self.hooks;
        let mut g = self.inner.borrow_mut();
        // a detached editor window already shows this file: focus it, never a second copy
        {
            let super::state::Inner {
                detached, bridge, ..
            } = &mut *g;
            if detached.focus_detached_by_path(bridge, path) {
                g.drain();
                return true;
            }
        }
        if g.manager.is_none() {
            return false;
        }
        let kind = document_kind(path);
        let Some(kind) = kind else {
            drop(g);
            hooks.notify_unsupported_file(path);
            return false;
        };
        if kind == TabKind::Diagram {
            // a Visio drawing opens as a new diagram made from it; opening it
            // again comes back to that tab. A file that does not open gets no
            // tab, only the reason.
            let existing = g
                .manager_call(|m, b| m.find_diagram_tab_by_path(b, Some(path)))
                .flatten();
            if let Some(existing) = existing {
                hooks.record_recent_file(path);
                g.manager_call(|m, b| m.activate_tab(b, &existing));
                return true;
            }
            let key = resolve(&[path]);
            let key = if cfg!(target_os = "linux") { key } else { key.to_lowercase() };
            if !DIAGRAM_CHECKING.with(|checking| checking.borrow_mut().insert(key.clone())) {
                return true;
            }
            let state = self.this.clone();
            let path = path.to_owned();
            let accepted = std::rc::Rc::new(std::cell::Cell::new(true));
            let answer = accepted.clone();
            drop(g);
            hooks.diagram_open_refusal(&path.clone(), Box::new(move |reason| {
                let _checking = owned::DiagramCheckGuard::new(key);
                let Some(state) = state.upgrade() else { return };
                if let Some(reason) = reason {
                    answer.set(false);
                    state.hooks.show_app_warning(&reason, Some(&basename(&path, None)));
                    return;
                }
                let mut g = state.inner.borrow_mut();
                let existing = g.manager_call(|m, b| m.find_diagram_tab_by_path(b, Some(&path))).flatten();
                if g.manager.is_none() { return; }
                state.hooks.record_recent_file(&path);
                g.manager_call(|m, b| {
                    if let Some(existing) = existing {
                        m.activate_tab(b, &existing);
                    } else if let Err(e) = m.open_diagram_tab(b, Some(&path)) {
                        b.warn("[shell] open failed:", &e.0);
                    }
                });
            }));
            return accepted.get();
        }
        hooks.record_recent_file(path);
        let opened = g.manager_call(|m, b| {
            if let Some(existing) = m.find_tab_of_kind_by_path(kind, Some(path)) {
                m.activate_tab(b, &existing);
                return false;
            }
            let result = match kind {
                TabKind::Docs => m.open_docs_tab(b, Some(path), None),
                TabKind::Sheets => m.open_sheets_tab(b, Some(path), false),
                TabKind::Slides => m.open_slides_tab(b, Some(path)),
                TabKind::Pdf => m.open_pdf_tab(b, path),
                TabKind::Markdown => m.open_markdown_tab(b, Some(path)),
                TabKind::Html => m.open_html_tab(b, Some(path)),
                TabKind::Mindmap => m.open_mindmap_tab(b, Some(path)),
                TabKind::Hwp => m.open_hwp_tab(b, Some(path), None),
                _ => m.open_epub_tab(b, path),
            };
            if let Err(e) = result {
                b.warn("[shell] open failed:", &e.0);
                return false;
            }
            true
        });
        drop(g);
        if opened == Some(true) && kind == TabKind::Sheets {
            hooks.start_queued_workbook_nudge();
        }
        true
    }

    // ---- new documents ----

    /// `newDocTab`, `newSheetTab`, ... one tab for the kind, in the folder the
    /// click remembered
    pub(super) fn new_tab(&self, kind: NewKind) {
        match kind {
            NewKind::Sheet => self.new_sheet_tab(),
            NewKind::Pdf => self.new_pdf_tab(),
            _ => {
                let opened = self.inner.borrow_mut().manager_call(|m, b| match kind {
                    NewKind::Doc => m.open_docs_tab(
                        b,
                        None,
                        Some(&OpenDocsOptions {
                            new_blank: true,
                            ai_content: None,
                        }),
                    ),
                    NewKind::Slide => m.open_slides_tab(b, None),
                    NewKind::Markdown => m.open_markdown_tab(b, None),
                    NewKind::Html => m.open_html_tab(b, None),
                    NewKind::Mindmap => m.open_mindmap_tab(b, None),
                    NewKind::Diagram => m.open_diagram_tab(b, None),
                    _ => m.open_hwp_tab(b, None, None),
                });
                match opened {
                    Some(Err(e)) => self.surface_new_tab_error(&e.0),
                    Some(Ok(id)) => self.bind_pending_dir(kind.key(), Some(&id)),
                    None => self.bind_pending_dir(kind.key(), None),
                }
            }
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
        let attempt = (|| -> Result<(), String> {
            std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
            let backing = join(&[&temp_dir, &basename(&suggested, None)]);
            hooks.mark_sheets_unsaved_new(&backing, &suggested, &temp_dir);
            if let Err(error) = hooks.blank_xlsx().and_then(|bytes| {
                main_core::packages::electron_utils::atomic_write::atomic_write_file(&backing, &bytes)
                    .map_err(|error| error.message)
            }) {
                hooks.unmark_sheets_unsaved_new(&backing);
                return Err(error);
            }
            // eligible for content-derived auto-rename after the first AI generation
            hooks.mark_sheets_untitled_path(&backing);
            self.inner
                .borrow_mut()
                .manager_call(|m, b| m.open_sheets_tab(b, Some(&backing), false))
                .transpose()
                .map_err(|e| e.0)?;
            hooks.start_queued_workbook_nudge();
            Ok(())
        })();
        let Err(error) = attempt else {
            return;
        };
        hooks.warn(
            "[shell] temp workbook create failed, writing to the default folder:",
            &error,
        );
        let fallback = hooks.blank_xlsx().and_then(|bytes| {
            main_core::packages::electron_utils::atomic_write::atomic_write_file(&suggested, &bytes)
                .map_err(|error| error.message)
        });
        match fallback {
            Ok(()) => {
                hooks.mark_sheets_untitled_path(&suggested);
                self.route_document_path(&suggested);
            }
            Err(e) => {
                hooks.warn(
                    "[shell] blank workbook create failed, opening in-memory blank tab:",
                    &e,
                );
                let opened = self
                    .inner
                    .borrow_mut()
                    .manager_call(|m, b| m.open_sheets_tab(b, None, true));
                if let Some(Err(e)) = opened {
                    self.surface_new_tab_error(&e.0);
                }
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
                self.route_document_path(&path);
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
        if !super::owned_saved::pending_source_is_eligible(file_path,&root,set_at) {
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
            self.hooks.warn("move new file into folder failed", &error.to_string());
            return file_path.to_owned();
        }
        match self.provided.all::<crate::home_files::Home>().into_iter().next() {
            Some(home) => {
                if let Err(error) = home.after_file_moved(file_path, &target) {
                    self.hooks.warn("updating moved file failed", &error.to_string());
                }
            }
            None => self.hooks.warn("updating moved file failed", "home service unavailable"),
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
        let mut opened = false;
        for path in &supported {
            opened = self.open_document_path(path) || opened;
        }
        if opened {
            self.reveal_shell_window();
        }
        if !unsupported.is_empty() {
            let message = self
                .hooks
                .tm_params("errUnsupportedExt", &[("ext", &unsupported.join(", "))]);
            self.hooks.show_app_warning(&message, None);
        }
        self.drop_skill_files(skill_files.into_iter(), opened || !unsupported.is_empty(), false);
    }

    fn drop_skill_files(&self, mut paths: std::vec::IntoIter<String>, acted: bool, missed: bool) {
        let Some(path) = paths.next() else {
            if missed && !acted {
                self.hooks.drop_no_skills_found();
            }
            return;
        };
        let state = self.rc();
        self.hooks.drop_skill_file(&path, Box::new(move |taken| {
            state.drop_skill_files(paths, acted || taken, missed || !taken);
        }));
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
        let projects = self.provided.all::<crate::projects::Projects>().into_iter().next()
            .ok_or_else(|| "project services are not registered".to_owned())?;
        let placer = projects.new_documents();
        let mut placer = placer.borrow_mut();
        let home = self.shell_window().map(|shell| shell.view.0 as i64);
        let placement = placer.placement_of_opener(Some(opener), home);
        placer.preset(view.0 as i64, placement);
        Ok(())
    }

    pub fn open_sheets_template_copy(
        &self,
        copy: &main_core::shell::user_skills::template_tabs::TemplateCopy,
    ) -> Result<Option<(String, ViewId)>, String> {
        if self.shell_window().is_none() {
            return Ok(None);
        }
        let path = Path::new(&copy.file_name);
        let extension = path.extension().and_then(|value| value.to_str()).unwrap_or("");
        let stem = path.file_stem().and_then(|value| value.to_str())
            .filter(|value| !value.is_empty()).unwrap_or(&copy.title);
        let save_extension = if extension.eq_ignore_ascii_case("xlsm") { "xlsm" } else { "xlsx" };
        let dir = self.new_file_dir("sheet");
        let mut suggested = self.hooks.unique_path_in(&dir, &format!("{stem}.{save_extension}"));
        let mut number = 2;
        while self.hooks.sheets_suggested_path_taken(&suggested) {
            suggested = self.hooks.unique_path_in(&dir, &format!("{stem} {number}.{save_extension}"));
            number += 1;
        }
        let temp = self.services.paths.path(PathName::Temp);
        let temp_dir = join(&[&temp.to_string_lossy(), "seuoffice-imports", &self.hooks.random_uuid()]);
        std::fs::create_dir_all(&temp_dir).map_err(|error| error.to_string())?;
        let backing = join(&[&temp_dir, &copy.file_name]);
        self.hooks.mark_sheets_unsaved_new(&backing, &suggested, &temp_dir);
        let result = (|| {
            main_core::packages::electron_utils::atomic_write::atomic_write_file(&backing, &copy.bytes)
                .map_err(|error| error.message)?;
            self.with_manager(|manager, host| {
                let id = manager.open_sheets_tab(host, Some(&backing), false).map_err(|error| error.0)?;
                let view = manager.web_contents_for_tab(&id)
                    .ok_or_else(|| "the template spreadsheet tab could not be opened".to_owned())?;
                Ok((id, view))
            }).ok_or_else(|| "SeuOffice is not ready".to_owned())?
        })();
        match result {
            Ok(opened) => {
                self.hooks.start_queued_workbook_nudge();
                Ok(Some(opened))
            }
            Err(error) => {
                self.hooks.unmark_sheets_unsaved_new(&backing);
                if let Err(cleanup) = std::fs::remove_dir_all(&temp_dir) {
                    self.hooks.warn("[shell] could not remove the unused template workbook:", &cleanup.to_string());
                }
                Err(error)
            }
        }
    }

    pub fn open_ai_doc_tab(&self, content: serde_json::Value, owner: Option<ViewId>) -> Result<ViewId, String> {
        let view = self.with_manager(|manager, host| {
            let id = manager.open_docs_tab(host, None, Some(&OpenDocsOptions {
                new_blank: true,
                ai_content: Some(content),
            })).map_err(|error| error.0)?;
            manager.web_contents_for_tab(&id)
                .ok_or_else(|| "the new document tab could not be opened".to_owned())
        }).ok_or_else(|| "SeuOffice is not ready".to_owned())??;
        if let Some(owner) = owner {
            self.follow_opener_project(view, &main_core::shell::projects::new_documents::NewDocumentOpener::Window {
                web_contents_id: i64::from(owner.0),
            })?;
        }
        Ok(view)
    }

    pub fn open_mcp_blank_docs(&self) -> Result<ViewId, String> {
        let view = self.with_manager(|manager, host| {
            let id = manager.open_docs_tab(host, None, Some(&OpenDocsOptions {
                new_blank: true,
                ai_content: None,
            })).map_err(|error| error.0)?;
            manager.web_contents_for_tab(&id)
                .ok_or_else(|| "the new document tab could not be opened".to_owned())
        }).ok_or_else(|| "SeuOffice is not ready".to_owned())??;
        self.inner.borrow_mut().mcp_blank.insert(view, McpBlank::Docs);
        Ok(view)
    }

    pub fn open_mcp_blank_sheets(&self) -> Result<ViewId, String> {
        if self.shell_window().is_none() {
            return Err("SeuOffice is not ready".to_owned());
        }
        let path = self.hooks.unique_path_in(
            &self.hooks.default_save_dir(),
            &format!("{}.xlsx", self.hooks.tm("untitledSheet")),
        );
        let bytes = self.hooks.blank_xlsx()?;
        main_core::packages::electron_utils::atomic_write::atomic_write_file(&path, &bytes)
            .map_err(|error| error.message)?;
        let result = self.with_manager(|manager, host| {
            let id = manager.open_sheets_tab(host, Some(&path), false)
                .map_err(|error| error.0)?;
            manager.web_contents_for_tab(&id)
                .ok_or_else(|| "the new spreadsheet tab could not be opened".to_owned())
        }).ok_or_else(|| "SeuOffice is not ready".to_owned()).and_then(|result| result);
        match result {
            Ok(view) => {
                self.inner.borrow_mut().mcp_blank.insert(view, McpBlank::Sheets(path));
                self.hooks.start_queued_workbook_nudge();
                Ok(view)
            }
            Err(error) => {
                if let Err(cleanup) = std::fs::remove_file(&path) {
                    self.hooks.warn("[mcp] could not remove the unused blank workbook:", &cleanup.to_string());
                }
                Err(error)
            }
        }
    }

    pub fn open_mcp_blank_slides(&self) -> Result<ViewId, String> {
        let view = self.with_manager(|manager, host| {
            let id = manager.open_slides_tab(host, None).map_err(|error| error.0)?;
            manager.web_contents_for_tab(&id)
                .ok_or_else(|| "the new presentation tab could not be opened".to_owned())
        }).ok_or_else(|| "SeuOffice is not ready".to_owned())??;
        self.inner.borrow_mut().mcp_blank.insert(view, McpBlank::Slides);
        Ok(view)
    }

    pub fn abandon_mcp_blank_slides(&self, view: ViewId) -> Result<(), String> {
        if !matches!(self.inner.borrow().mcp_blank.get(&view), Some(McpBlank::Slides)) {
            return Ok(());
        }
        let dirty = self.with_manager(|manager, host| {
            manager.dirty_tabs(host, TabKind::Slides)
                .map(|tabs| tabs.iter().any(|(_, candidate)| *candidate == view))
                .map_err(|error| error.0)
        }).ok_or_else(|| "SeuOffice is not ready".to_owned())??;
        if !dirty && self.close_mcp_blank(view, TabKind::Slides) {
            self.inner.borrow_mut().mcp_blank.remove(&view);
        }
        Ok(())
    }

    pub fn abandon_mcp_blank_docs(&self, view: ViewId) {
        if !matches!(self.inner.borrow().mcp_blank.get(&view), Some(McpBlank::Docs)) {
            return;
        }
        let state = self.this.clone();
        self.hooks.docs_query_dirty(view, Box::new(move |dirty| {
            if matches!(dirty, Ok(false)) {
                if let Some(state) = state.upgrade() {
                    let owned = matches!(state.inner.borrow().mcp_blank.get(&view), Some(McpBlank::Docs));
                    if owned && state.close_mcp_blank(view, TabKind::Docs) {
                        state.inner.borrow_mut().mcp_blank.remove(&view);
                    }
                }
            }
        }));
    }

    pub fn abandon_mcp_blank_sheets(&self, view: ViewId) {
        let Some(McpBlank::Sheets(path)) = self.inner.borrow().mcp_blank.get(&view).cloned() else {
            return;
        };
        let dirty = match self.with_manager(|manager, host| {
            manager.dirty_tabs(host, TabKind::Sheets)
                .map(|tabs| tabs.iter().any(|(_, candidate)| *candidate == view))
        }) {
            Some(Ok(dirty)) => dirty,
            Some(Err(error)) => {
                self.hooks.warn("[mcp] could not check blank workbook:", &error.0);
                return;
            }
            None => {
                self.hooks.warn("[mcp] could not check blank workbook:", "SeuOffice is not ready");
                return;
            }
        };
        if !dirty && self.close_mcp_blank(view, TabKind::Sheets) {
            self.inner.borrow_mut().mcp_blank.remove(&view);
            if let Err(error) = std::fs::remove_file(&path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    self.hooks.warn("[mcp] could not remove the unused blank workbook:", &error.to_string());
                }
            }
        }
    }

    fn close_mcp_blank(&self, view: ViewId, kind: TabKind) -> bool {
        self.with_manager(|manager, host| {
            let Some(id) = manager.tab_id_for_web_contents(host, view.0 as i64) else {
                return false;
            };
            if !manager.list().iter().any(|tab| tab.id == id && tab.kind == kind) {
                return false;
            }
            manager.close_tab_without_prompt(host, &id)
        }).unwrap_or(false)
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
