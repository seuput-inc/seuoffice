//! What other areas call into the PDF editor: the functions pdf-main.ts
//! exported to the shell, as [`PdfEditors`]. The ones that waited on the
//! renderer keep a continuation and emit the request; the renderer's
//! `*-result` send (or the TypeScript's timeout) resumes it.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use jscompat::node_path::host::basename;
use main_core::packages::electron_utils::recovery_store::RecoveryIdentity;
use main_core::packages::electron_utils::safe_external_url::{
    SafeExternalUrlOptions, safe_external_url,
};
use main_core::shell::renderer_disposal::Failure;
use shell_contract::editors::{DocumentEditors, SaveMode, Then};
use shell_contract::hosts::{FileDialog, FileFilter, MessageBox};
use shell_contract::{ViewId, WindowId};

use super::dialogs::{Dlg, tm};
use super::recovery::{Decision, WORK_FILE_MAX_AGE};
use super::{Editor, Waiting, resolve};
use crate::kit::{self, IntoIpc, error};

/// How long the main side waits for the renderer's save (`requestRendererSave`, Save As)
const RENDERER_SAVE_TIMEOUT: Duration = Duration::from_secs(120);
/// `COPY_FLUSH_TIMEOUT_MS`
const COPY_FLUSH_TIMEOUT: Duration = Duration::from_secs(8);

/// `PdfSaveAsOutcome`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveAsOutcome {
    Saved,
    Cancelled,
    Failed,
}

impl Editor {
    /// `requestRendererSave(contents, flushAll)`
    pub(super) fn request_renderer_save(
        &self,
        view: ViewId,
        flush_all: bool,
        then: Box<dyn FnOnce(bool)>,
    ) {
        self.request_renderer_save_impl(view, flush_all, None, then)
    }

    fn request_renderer_save_impl(
        &self,
        view: ViewId,
        flush_all: bool,
        authority: Option<super::owned_save::SaveAuthority>,
        then: Box<dyn FnOnce(bool)>,
    ) {
        if authority.is_none()
            && self
                .st
                .borrow()
                .views
                .get(&view)
                .is_some_and(|slot| slot.owned_save.is_some())
        {
            return then(false);
        }
        let waiting: Waiting<bool> = Rc::new(RefCell::new(Some(then)));
        let me = self.me.clone();
        let mine = waiting.clone();
        let timer = kit::after(&self.services, RENDERER_SAVE_TIMEOUT, move || {
            if let Some(ed) = me.upgrade() {
                ed.with_view(view, |v| {
                    if v.close_save
                        .as_ref()
                        .is_some_and(|(waiting, _)| Rc::ptr_eq(waiting, &mine))
                    {
                        v.close_save = None;
                    }
                });
            }
            resolve(&mine, false);
        });
        // a second request replaces the first in the table; the first still
        // ends with its own timeout, as the TypeScript's did
        self.with_view(view, |v| v.close_save = Some((waiting, timer)));
        let owned = authority.as_ref().map(|owned| owned.id.clone());
        let mut options = vec![("flushAll".to_owned(), flush_all.into_ipc())];
        if let Some(id) = owned {
            options.push(("ownedSaveId".to_owned(), id.into_ipc()));
        }
        let sent = match authority {
            Some(owned) => owned.emit(
                "pdf:close-save-request",
                vec![shell_contract::IpcValue::Object(options)],
            ),
            None => {
                self.emit(
                    view,
                    "pdf:close-save-request",
                    vec![shell_contract::IpcValue::Object(options)],
                );
                Ok(())
            }
        };
        if sent.is_err() {
            self.close_save_result(view, false);
        }
    }

    /// `pdf:close-save-result`
    pub(super) fn close_save_result(&self, view: ViewId, ok: bool) {
        let waiter = self.with_view(view, |v| v.close_save.take());
        if let Some((waiting, timer)) = waiter {
            kit::clear(timer);
            resolve(&waiting, ok);
        }
    }

    /// `setPdfSaveAsInFlight(contents, inFlight)`
    pub(super) fn set_save_as_in_flight(&self, view: ViewId, in_flight: bool) {
        self.emit(view, "pdf:save-as-flow", vec![in_flight.into_ipc()]);
    }

    /// `requestPdfSaveAsOutcome(contents, targetPath)`
    pub(super) fn request_save_as_outcome(
        &self,
        view: ViewId,
        target: &str,
        then: Box<dyn FnOnce(SaveAsOutcome)>,
    ) {
        self.request_save_as_outcome_impl(view, target, None, then)
    }

    fn request_save_as_outcome_impl(
        &self,
        view: ViewId,
        target: &str,
        authority: Option<super::owned_save::SaveAuthority>,
        then: Box<dyn FnOnce(SaveAsOutcome)>,
    ) {
        if authority.is_none()
            && self
                .st
                .borrow()
                .views
                .get(&view)
                .is_some_and(|slot| slot.owned_save.is_some())
        {
            return then(SaveAsOutcome::Failed);
        }
        if self.services.views.sender_of(view).is_none() {
            return then(SaveAsOutcome::Failed);
        }
        self.with_view(view, |v| v.save_as_target = Some(target.to_owned()));
        let me = self.me.clone();
        let done: Box<dyn FnOnce(SaveAsOutcome)> = Box::new(move |outcome| {
            if let Some(ed) = me.upgrade() {
                ed.with_view(view, |v| v.save_as_target = None);
            }
            then(outcome)
        });
        let waiting: Waiting<SaveAsOutcome> = Rc::new(RefCell::new(Some(done)));
        let me = self.me.clone();
        let mine = waiting.clone();
        let timer = kit::after(&self.services, RENDERER_SAVE_TIMEOUT, move || {
            if let Some(ed) = me.upgrade() {
                ed.with_view(view, |v| {
                    if v.save_as
                        .as_ref()
                        .is_some_and(|(waiting, _)| Rc::ptr_eq(waiting, &mine))
                    {
                        v.save_as = None;
                    }
                });
            }
            resolve(&mine, SaveAsOutcome::Failed);
        });
        self.with_view(view, |v| v.save_as = Some((waiting, timer)));
        let owned = authority.as_ref().map(|owned| owned.id.clone());
        let mut args = vec![target.into_ipc()];
        if let Some(id) = owned {
            args.push(id.into_ipc());
        }
        let sent = match authority {
            Some(owned) => owned.emit("pdf:save-as-request", args),
            None => {
                self.emit(view, "pdf:save-as-request", args);
                Ok(())
            }
        };
        if sent.is_err() {
            self.save_as_result(view, SaveAsOutcome::Failed);
        }
    }

    /// `pdf:save-as-result`
    pub(super) fn save_as_result(&self, view: ViewId, outcome: SaveAsOutcome) {
        let waiter = self.with_view(view, |v| v.save_as.take());
        if let Some((waiting, timer)) = waiter {
            kit::clear(timer);
            resolve(&waiting, outcome);
        }
    }

    pub(super) fn save_as_waiting(&self, view: ViewId) -> bool {
        self.st
            .borrow()
            .views
            .get(&view)
            .is_some_and(|v| v.save_as.is_some())
    }

    /// `discardPdfRecovery(contents)`: "Don't Save"
    pub(super) fn discard_recovery(&self, view: ViewId) {
        let open = self.open_path(view);
        let untitled_copy = open.as_deref().is_some_and(|p| self.is_untitled_copy(p));
        self.rec
            .borrow_mut()
            .dont_save(view, open.as_deref(), untitled_copy);
        self.emit(view, "pdf:recovery-dropped", vec![true.into_ipc()]);
    }

    /// `pdf:recovery-flush-done`
    pub(super) fn copy_flush_done(&self, view: ViewId, token: &str, result: Result<(), Failure>) {
        let waiter = self
            .st
            .borrow_mut()
            .views
            .get_mut(&view)
            .and_then(|v| v.copy_flush.remove(token));
        if let Some((waiting, timer)) = waiter {
            kit::clear(timer);
            resolve(&waiting, result);
        }
    }

    /// `reopenView(contents, filePath)`
    fn reopen_view(&self, view: ViewId, path: &str) {
        self.grants.0.with(view, |set| {
            if let Some(set) = set {
                set.insert(path.to_owned());
            }
        });
        self.replace_document(view, Some(path.to_owned()));
        if self.rec.borrow().has_consumed(view) {
            self.with_view(view, |v| v.dirty = false);
            self.shell.reload(view);
        }
    }
}

/// The PDF editor as the shell, the menus and MCP call it
pub struct PdfEditors(pub(super) Rc<Editor>);

pub use super::recovery_open::PdfRecoveryRetirement;

impl PdfEditors {
    pub fn recovery_retirement(
        &self,
        target: shell_contract::ipc::RecoveryOpenTarget,
    ) -> Result<PdfRecoveryRetirement, String> {
        if !target
            .closure_reservation()
            .matches(target.view(), target.created())
        {
            return Err("PDF recovery closure reservation does not match its target".into());
        }
        Ok(PdfRecoveryRetirement::new(self.0.clone(), target))
    }
}

#[cfg(any(test, feature = "probe"))]
impl main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract<ViewId, ()>
    for PdfEditors
{
    fn kind(&self) -> &str {
        "pdf"
    }

    fn write_copies_now(&self, done: Box<dyn FnOnce(Result<(), Failure>)>) {
        PdfEditors::write_copies_now(self, done);
    }

    fn open_file_with_decision(
        &self,
        view: &ViewId,
        request: &main_core::packages::electron_utils::recovery_contract::RecoveryOpenFileRequest,
        _authority: &(),
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        use main_core::packages::electron_utils::recovery_contract::RecoveryOpenDecision;
        let ed = &self.0;
        if !ed.registered(*view)
            || ed.services.views.sender_of(*view).is_none()
            || !Path::new(&request.file_path).exists()
        {
            return done(Err(Failure("The recovery file is unavailable".into())));
        }
        let decision = if request.decision == RecoveryOpenDecision::Restore {
            Decision::Restore
        } else {
            Decision::Original
        };
        ed.rec
            .borrow_mut()
            .set_decision(*view, &request.file_path, decision);
        ed.grants.0.with(*view, |set| {
            if let Some(set) = set {
                set.insert(request.file_path.clone());
            }
        });
        ed.replace_document(*view, Some(request.file_path.clone()));
        done(Ok(()));
    }

    fn open_untitled_copy(
        &self,
        view: &ViewId,
        request: &main_core::packages::electron_utils::recovery_contract::RecoveryOpenUntitledRequest,
        _authority: &(),
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        let ed = &self.0;
        if !ed.registered(*view) || ed.services.views.sender_of(*view).is_none() {
            return done(Err(Failure("The recovery view is unavailable".into())));
        }
        let path = match ed.rec.borrow_mut().open_untitled(
            *view,
            &request.copy,
            &request.title,
            request.save_as_default_path.as_deref(),
        ) {
            Ok(Some(path)) => path,
            Ok(None) => return done(Err(Failure("The recovery copy is unavailable".into()))),
            Err(error) => return done(Err(Failure(error))),
        };
        {
            let mut state = ed.st.borrow_mut();
            state.untitled_copy.insert(path.clone());
            state.unsaved_untitled_copy.insert(path.clone());
        }
        ed.grants.0.with(*view, |set| {
            if let Some(set) = set {
                set.insert(path.clone());
            }
        });
        ed.replace_document(*view, Some(path));
        done(Ok(()));
    }

    fn is_unprotectable(&self, view: &ViewId) -> bool {
        PdfEditors::is_unprotectable(self, *view)
    }
}

impl
    main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract<
        ViewId,
        shell_contract::ipc::RecoveryOpenTarget,
    > for PdfEditors
{
    fn kind(&self) -> &str {
        "pdf"
    }
    fn write_copies_now(&self, done: Box<dyn FnOnce(Result<(), Failure>)>) {
        PdfEditors::write_copies_now(self, done);
    }
    fn open_file_with_decision(
        &self,
        view: &ViewId,
        request: &main_core::packages::electron_utils::recovery_contract::RecoveryOpenFileRequest,
        authority: &shell_contract::ipc::RecoveryOpenTarget,
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        super::recovery_open::open_file(&self.0, *view, request.clone(), authority.clone(), done);
    }
    fn open_untitled_copy(
        &self,
        view: &ViewId,
        request: &main_core::packages::electron_utils::recovery_contract::RecoveryOpenUntitledRequest,
        authority: &shell_contract::ipc::RecoveryOpenTarget,
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        super::recovery_open::open_copy(&self.0, *view, request.clone(), authority.clone(), done);
    }
    fn is_unprotectable(&self, view: &ViewId) -> bool {
        PdfEditors::is_unprotectable(self, *view)
    }
}

impl PdfEditors {
    /// `createPdfView(openPath)`'s `grantAndTrack`: the view starts with its
    /// file (when it exists) as its only grant
    pub fn open_view(&self, view: ViewId, open_path: Option<&str>) {
        let ed = &self.0;
        let path = open_path.filter(|p| !p.is_empty() && Path::new(p).exists());
        let mut allowed = HashSet::new();
        if let Some(path) = path {
            allowed.insert(path.to_owned());
        }
        ed.replace_document(view, path.map(str::to_owned));
        ed.grants.0.insert(view, allowed);
    }

    /// `setWindowOpenHandler` of the view: external links go to the system
    /// browser, nothing opens in the app; answers `deny` always
    pub fn window_open(&self, url: &str) {
        let options = SafeExternalUrlOptions {
            allowed_protocols: Some(vec!["http:".into(), "https:".into(), "mailto:".into()]),
        };
        if let Some(target) = safe_external_url(Some(url), Some(&options)) {
            let _ = self.0.os.open_external(&target);
        }
    }

    /// `render-process-gone`: a crashed renderer keeps its recovery copy
    pub fn render_gone(&self, view: ViewId) {
        self.0.rec.borrow_mut().mark_crashed(view);
    }

    /// `did-start-loading` (reload): the remount drops the edits and the watch
    pub fn start_loading(&self, view: ViewId) {
        self.0.with_view(view, |v| v.dirty = false);
        self.0.stop_view_watch(view);
    }

    /// `pdfIsDirty(wcId)`
    pub fn is_dirty(&self, view: ViewId) -> bool {
        self.0.is_dirty(view)
    }

    /// `clearPdfDirty(wcId)`
    pub fn clear_dirty(&self, view: ViewId) {
        self.0.with_view(view, |v| v.dirty = false);
    }

    /// `pdfPathGranted(wcId, path)`
    pub fn path_granted(&self, view: ViewId, path: &str) -> bool {
        self.0.grants.granted(view, path)
    }

    /// `markPdfUntitledPath(path)`: "New PDF" wrote a blank
    pub fn mark_untitled_path(&self, path: &str) {
        let mut st = self.0.st.borrow_mut();
        st.untitled.insert(path.to_owned());
        st.unsaved_blank.insert(path.to_owned());
    }

    /// `markPdfUntitledCopy(path)`
    pub fn mark_untitled_copy(&self, path: &str) {
        self.0.st.borrow_mut().untitled_copy.insert(path.to_owned());
    }

    /// `pdfRecoveryIdentity(wcId)`
    pub fn recovery_identity(&self, view: ViewId) -> Option<RecoveryIdentity> {
        self.0.rec.borrow().untitled_identity(view)
    }

    /// Ask every dirty, protectable view for a matching flush acknowledgement.
    pub fn write_copies_now(&self, then: Box<dyn FnOnce(Result<(), Failure>)>) {
        let ed = &self.0;
        let targets: Vec<ViewId> = ed
            .st
            .borrow()
            .views
            .iter()
            .filter(|(id, view)| view.dirty && !ed.rec.borrow().is_unprotectable(**id))
            .map(|(id, _)| *id)
            .collect();
        if targets.is_empty() {
            then(Ok(()));
            return;
        }
        let left = Rc::new(RefCell::new((targets.len(), None::<Failure>, Some(then))));
        for view in targets {
            let finish = {
                let left = Rc::clone(&left);
                move |result: Result<(), Failure>| {
                    let mut state = left.borrow_mut();
                    if let Err(error) = result {
                        if state.1.is_none() {
                            state.1 = Some(error);
                        }
                    }
                    state.0 -= 1;
                    if state.0 == 0 {
                        let result = state.1.take().map_or(Ok(()), Err);
                        let done = state.2.take();
                        drop(state);
                        if let Some(done) = done {
                            done(result);
                        }
                    }
                }
            };
            if ed.services.views.sender_of(view).is_none() {
                finish(Err(Failure("PDF recovery view destroyed".into())));
                continue;
            }
            let token = kit::unique().to_string();
            let waiting: Waiting<Result<(), Failure>> =
                Rc::new(RefCell::new(Some(Box::new(finish))));
            let me = ed.me.clone();
            let mine = waiting.clone();
            let timeout_token = token.clone();
            let timer = kit::after(&ed.services, COPY_FLUSH_TIMEOUT, move || {
                if let Some(ed) = me.upgrade() {
                    if let Some(view) = ed.st.borrow_mut().views.get_mut(&view) {
                        view.copy_flush.remove(&timeout_token);
                    }
                }
                resolve(&mine, Err(Failure("PDF recovery flush timed out".into())));
            });
            ed.with_view(view, |v| {
                v.copy_flush.insert(token.clone(), (waiting, timer));
            });
            ed.emit(view, "pdf:recovery-flush-request", vec![token.into_ipc()]);
        }
    }

    /// `pdfRecoveryEditor.openFileWithDecision(contents, request)`
    pub fn open_file_with_decision(&self, view: ViewId, path: &str, restore: bool) {
        let ed = &self.0;
        if ed.services.views.sender_of(view).is_none()
            || !ed.registered(view)
            || !Path::new(path).exists()
        {
            return;
        }
        let decision = if restore {
            Decision::Restore
        } else {
            Decision::Original
        };
        ed.rec.borrow_mut().set_decision(view, path, decision);
        ed.reopen_view(view, path);
    }

    /// `pdfRecoveryEditor.openUntitledCopy(contents, request)`
    pub fn open_untitled_copy(
        &self,
        view: ViewId,
        copy: &RecoveryIdentity,
        title: &str,
        save_as_default_path: Option<&str>,
    ) {
        let ed = &self.0;
        if ed.services.views.sender_of(view).is_none() || !ed.registered(view) {
            return;
        }
        let opened = ed
            .rec
            .borrow_mut()
            .open_untitled(view, copy, title, save_as_default_path);
        let path = match opened {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(err) => {
                eprintln!("[pdf] recovery copy could not be opened: {err}");
                return;
            }
        };
        {
            let mut st = ed.st.borrow_mut();
            st.untitled_copy.insert(path.clone());
            st.unsaved_untitled_copy.insert(path.clone());
        }
        ed.reopen_view(view, &path);
    }

    /// `pdfRecoveryEditor.isUnprotectable(contents)`
    pub fn is_unprotectable(&self, view: ViewId) -> bool {
        self.0.rec.borrow().is_unprotectable(view)
    }

    /// `registerPdfRecoveryEditor()`'s sweep of a crash's working files
    pub fn sweep_work_files(&self) {
        self.0.rec.borrow().sweep_work_files(WORK_FILE_MAX_AGE);
    }

    /// `discardPdfRecovery(contents)`
    pub fn discard_recovery(&self, view: ViewId) {
        self.0.discard_recovery(view)
    }

    /// `pdfCloseCancelled(contents)`
    pub fn close_cancelled(&self, view: ViewId) {
        if self.0.rec.borrow_mut().close_cancelled(view) {
            self.0
                .emit(view, "pdf:recovery-dropped", vec![false.into_ipc()]);
        }
    }

    /// `flushPdfSave(contents, flushAll)`: clean views answer true at once
    pub fn flush_save(&self, view: ViewId, flush_all: bool, then: Box<dyn FnOnce(bool)>) {
        if self.0.services.views.sender_of(view).is_none() || !self.0.is_dirty(view) {
            return then(true);
        }
        self.0.request_renderer_save(view, flush_all, then)
    }

    /// `setPdfSaveAsInFlight(contents, inFlight)`
    pub fn set_save_as_in_flight(&self, view: ViewId, in_flight: bool) {
        self.0.set_save_as_in_flight(view, in_flight)
    }

    /// `requestPdfSaveAsOutcome(contents, targetPath)`
    pub fn request_save_as_outcome(
        &self,
        view: ViewId,
        target: &str,
        then: Box<dyn FnOnce(SaveAsOutcome)>,
    ) {
        self.0.request_save_as_outcome(view, target, then)
    }

    /// `requestPdfSaveAs(contents, targetPath)`
    pub fn request_save_as(&self, view: ViewId, target: &str, then: Box<dyn FnOnce(bool)>) {
        self.0.request_save_as_outcome(
            view,
            target,
            Box::new(move |o| then(o == SaveAsOutcome::Saved)),
        )
    }

    /// `requestPdfPageImages(contents, targetPath)`: false = not sent
    pub fn request_page_images(&self, view: ViewId, target: &str) -> bool {
        let ed = &self.0;
        if ed.services.views.sender_of(view).is_none()
            || !target.to_ascii_lowercase().ends_with(".png")
        {
            return false;
        }
        ed.with_view(view, |v| v.image_target = Some(target.to_owned()));
        ed.emit(view, "pdf:export-images-request", vec![]);
        true
    }
}

fn not_supported(what: &str) -> shell_contract::IpcError {
    error(format!("pdf: {what} is not supported by the PDF editor"))
}

impl DocumentEditors for PdfEditors {
    fn saved_surface(&self) -> shell_contract::Surface {
        shell_contract::Surface::Pdf
    }

    fn request_close_owned(
        &self,
        view: ViewId,
        parent: Option<WindowId>,
        origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        let ed = &self.0;
        let authority = match ed.capture_save_authority(view, origin, cancel) {
            Ok(authority) => authority,
            Err(message) => return then(Err(error(message))),
        };
        if let Err(message) = ed.start_owned_save(view, authority.clone()) {
            return then(Err(error(message)));
        }
        let completed = authority.clone();
        let me = ed.me.clone();
        let done: Then<bool> = Box::new(move |result| {
            let admitted = completed.current();
            if let Some(editor) = me.upgrade() {
                editor.end_owned_save(view, &completed.id);
            }
            then(admitted.map_err(error).and(result));
        });
        if !ed.is_dirty(view) || ed.services.views.sender_of(view).is_none() {
            return done(Ok(true));
        }
        let lang = ed.lang();
        let options = MessageBox {
            kind: Some("warning".into()),
            message: tm(&lang, Dlg::CloseUnsavedMsg).into(),
            detail: Some(tm(&lang, Dlg::CloseUnsavedDetail).into()),
            buttons: vec![
                tm(&lang, Dlg::BtnSave).into(),
                tm(&lang, Dlg::BtnDontSave).into(),
                tm(&lang, Dlg::BtnCancel).into(),
            ],
            default_id: Some(0),
            cancel_id: Some(2),
            ..Default::default()
        };
        let editor = ed.clone();
        let checked = authority.clone();
        let back = kit::back_on_core(
            &ed.services,
            move |answer: Result<shell_contract::hosts::MessageBoxAnswer, String>| {
                let answer = match answer {
                    Ok(answer) => answer,
                    Err(message) => return done(Err(error(message))),
                };
                if let Err(message) = checked.current() {
                    return done(Err(error(message)));
                }
                match answer.response {
                    2 => done(Ok(false)),
                    1 => {
                        let path = editor.open_path(view);
                        let untitled = path
                            .as_deref()
                            .is_some_and(|path| editor.is_untitled_copy(path));
                        let mut cleanup = Vec::new();
                        if let Err(message) = checked.commit(&mut || {
                            editor.rec.borrow_mut().dont_save_owned(
                                view,
                                path.as_deref(),
                                untitled,
                                &mut cleanup,
                            );
                            Ok(())
                        }) {
                            return done(Err(error(message)));
                        }
                        let store = editor.rec.borrow().store.clone();
                        let worker = checked.clone();
                        let notify = checked.clone();
                        kit::on_worker(
                            &editor.services,
                            move || {
                                for effect in cleanup {
                                    if let Err(message) = effect.run(&store, &worker) {
                                        eprintln!("[pdf] recovery cleanup failed: {message}");
                                    }
                                }
                            },
                            move |()| {
                                let sent =
                                    notify.emit("pdf:recovery-dropped", vec![true.into_ipc()]);
                                done(sent.map(|()| true).map_err(error));
                            },
                        );
                    }
                    _ => editor.request_renderer_save_impl(
                        view,
                        true,
                        Some(checked),
                        Box::new(move |saved| done(Ok(saved))),
                    ),
                }
            },
        );
        authority.message_dialog(&ed.dialogs, parent, options, back);
    }

    fn file_renamed_owned(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        self.file_renamed_many_owned_extra(
            view,
            old_path,
            new_path,
            origin,
            destination,
            &[],
            cancel,
            pending,
            &[],
            then,
        );
    }

    fn file_renamed_many_owned_extra(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        then: Then<()>,
    ) {
        super::rename::run(
            &self.0,
            view,
            old_path,
            new_path,
            origin,
            destination,
            participants,
            cancel,
            pending,
            additional,
            None,
            then,
        );
    }

    fn file_renamed_existing_document(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        authority: &shell_contract::ipc::NativeSaveRevisionAuthority,
        expected_path: &Path,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        then: Then<Option<shell_contract::ipc::NativeSaveRevisionReceipt>>,
    ) {
        super::rename::run_native(
            &self.0,
            view,
            old_path,
            new_path,
            authority,
            expected_path,
            destination,
            participants,
            cancel,
            pending,
            additional,
            then,
        );
    }

    fn file_renamed_recovery(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        super::rename::run(
            &self.0,
            view,
            old_path,
            new_path,
            receiver,
            destination,
            participants,
            cancel,
            pending,
            &[],
            Some(receipt),
            then,
        );
    }

    fn capture_document_owner(
        &self,
        view: ViewId,
        native: shell_contract::ipc::NativeRequestOwner,
    ) -> Result<shell_contract::ipc::NativeRequestOwner, shell_contract::IpcError> {
        self.0.capture_document_owner(view, native)
    }

    fn prepare_saved_document(
        &self,
        target: shell_contract::ipc::NativeSavedTarget,
    ) -> Result<Box<dyn shell_contract::editors::PreparedSavedDocument>, shell_contract::IpcError>
    {
        crate::saved_slots::prepare(&self.0, target)
    }

    fn load_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        then: Then<shell_contract::editors::SavedDocumentLoadOutcome>,
    ) {
        crate::saved_slots::load(&self.0, document, then, super::saved_open::install);
    }

    fn owns(&self, view: ViewId) -> bool {
        self.0.registered(view)
    }

    /// Save: `flushPdfSave(contents)` as the File menu did. Save As: the
    /// PDF copy of the shell's Save As (a save dialog with the PDF filter,
    /// then `requestPdfSaveAsOutcome`); the conversions stay the shell's.
    fn request_save(&self, view: ViewId, mode: SaveMode, then: Then<bool>) {
        match mode {
            SaveMode::Save => self.flush_save(view, false, Box::new(move |ok| then(Ok(ok)))),
            SaveMode::SaveAs => {
                let ed = &self.0;
                let Some(path) = ed.open_path(view) else {
                    return then(Ok(false));
                };
                ed.set_save_as_in_flight(view, true);
                let parent = ed.contents.window_of(view);
                let picked = ed.dialogs.save(
                    parent,
                    &FileDialog {
                        default_path: Some(PathBuf::from(&path)),
                        filters: vec![FileFilter {
                            name: tm(&ed.lang(), Dlg::FilterPdf).into(),
                            extensions: vec!["pdf".into()],
                        }],
                        ..Default::default()
                    },
                );
                let Some(target) = picked.map(|p| p.to_string_lossy().into_owned()) else {
                    ed.set_save_as_in_flight(view, false);
                    return then(Ok(false));
                };
                let me = ed.me.clone();
                ed.request_save_as_outcome(
                    view,
                    &target,
                    Box::new(move |outcome| {
                        if let Some(ed) = me.upgrade() {
                            ed.set_save_as_in_flight(view, false);
                        }
                        then(Ok(outcome == SaveAsOutcome::Saved))
                    }),
                );
            }
        }
    }

    fn request_save_owned(
        &self,
        view: ViewId,
        mode: SaveMode,
        origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        let ed = &self.0;
        let authority = match ed.capture_save_authority(view, origin, cancel) {
            Ok(authority) => authority,
            Err(message) => return then(Err(error(message))),
        };
        if let Err(message) = ed.start_owned_save(view, authority.clone()) {
            return then(Err(error(message)));
        }
        let me = ed.me.clone();
        let done_authority = authority.clone();
        let done: Then<bool> = Box::new(move |result| {
            let current = done_authority.current();
            if let Some(ed) = me.upgrade() {
                ed.end_owned_save(view, &done_authority.id);
            }
            then(current.map_err(error).and(result))
        });
        if mode == SaveMode::Save {
            if ed.services.views.sender_of(view).is_none() || !ed.is_dirty(view) {
                return done(Ok(true));
            }
            return ed.request_renderer_save_impl(
                view,
                false,
                Some(authority),
                Box::new(move |saved| done(Ok(saved))),
            );
        }
        let Some(path) = ed.open_path(view) else {
            return done(Ok(false));
        };
        if let Err(message) = authority.begin_save_as_flow() {
            return done(Err(error(message)));
        }
        let options = FileDialog {
            default_path: Some(path.into()),
            filters: vec![FileFilter {
                name: tm(&ed.lang(), Dlg::FilterPdf).into(),
                extensions: vec!["pdf".into()],
            }],
            ..Default::default()
        };
        let editor = ed.clone();
        let dialog_authority = authority.clone();
        let back = kit::back_on_core(
            &ed.services,
            move |picked: Result<Option<super::owned_save::SaveDestination>, String>| match picked {
                Ok(Some(path)) => {
                    if let Err(message) = authority.current() {
                        return done(Err(error(message)));
                    }
                    let target = path.path().to_string_lossy().into_owned();
                    if let super::owned_save::SaveDestination::Native(selection) = path {
                        if let Err(message) = authority.set_selection(selection) {
                            return done(Err(error(message)));
                        }
                    }
                    editor.request_save_as_outcome_impl(
                        view,
                        &target,
                        Some(authority),
                        Box::new(move |outcome| done(Ok(outcome == SaveAsOutcome::Saved))),
                    );
                }
                Ok(None) => done(Ok(false)),
                Err(message) => done(Err(error(message))),
            },
        );
        dialog_authority.save_dialog(&ed.dialogs, ed.contents.window_of(view), options, back);
    }

    fn request_save_native(
        &self,
        view: ViewId,
        mode: SaveMode,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        use shell_contract::ipc::{
            NativeEditorCommand, NativeInvocationSelection, NativeSaveDestination,
            NativeSaveSelection,
        };
        if !matches!(binding.operation().selection(), NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument { surface: shell_contract::Surface::Pdf, view: selected_view, selection: NativeSaveSelection::Mode(selected) }) if *selected_view == view && *selected == mode)
        {
            return then(Err(error("pdf: native save selection mismatch")));
        }
        let ed = &self.0;
        let authority =
            match ed.capture_native_save_authority(view, binding.clone(), cancel, pending) {
                Ok(authority) => authority,
                Err(message) => return then(Err(error(message))),
            };
        if let Err(message) = ed.start_owned_save(view, authority.clone()) {
            return then(Err(error(message)));
        }
        let me = ed.me.clone();
        let completed = authority.clone();
        let done: Then<bool> = Box::new(move |result| {
            let current = completed.current();
            if let Some(ed) = me.upgrade() {
                ed.end_owned_save(view, &completed.id);
            }
            then(current.map_err(error).and(result))
        });
        if mode == SaveMode::Save {
            if ed.services.views.sender_of(view).is_none() || !ed.is_dirty(view) {
                return done(Ok(true));
            }
            if let Some(path) = ed.open_path(view) {
                let mut selection = None;
                if let Err(message) = authority.commit(&mut || {
                    selection = Some(NativeSaveDestination::from_current_document(
                        binding.clone(),
                        path.clone().into(),
                    )?);
                    Ok(())
                }) {
                    return done(Err(error(message)));
                }
                if let Some(selection) = selection {
                    if let Err(message) = authority.set_selection(selection) {
                        return done(Err(error(message)));
                    }
                }
            }
            return ed.request_renderer_save_impl(
                view,
                false,
                Some(authority),
                Box::new(move |saved| done(Ok(saved))),
            );
        }
        let Some(path) = ed.open_path(view) else {
            return done(Ok(false));
        };
        if let Err(message) = authority.begin_save_as_flow() {
            return done(Err(error(message)));
        }
        let options = FileDialog {
            default_path: Some(path.into()),
            filters: vec![FileFilter {
                name: tm(&ed.lang(), Dlg::FilterPdf).into(),
                extensions: vec!["pdf".into()],
            }],
            ..Default::default()
        };
        let editor = ed.clone();
        let dialog_authority = authority.clone();
        let back = kit::back_on_core(
            &ed.services,
            move |picked: Result<Option<super::owned_save::SaveDestination>, String>| match picked {
                Ok(Some(super::owned_save::SaveDestination::Native(selection))) => {
                    let target = selection.path().to_string_lossy().into_owned();
                    if let Err(message) = authority.set_selection(selection) {
                        return done(Err(error(message)));
                    }
                    editor.request_save_as_outcome_impl(
                        view,
                        &target,
                        Some(authority),
                        Box::new(move |outcome| done(Ok(outcome == SaveAsOutcome::Saved))),
                    );
                }
                Ok(None) => done(Ok(false)),
                Ok(Some(_)) => done(Err(error("pdf: native save destination mismatch"))),
                Err(message) => done(Err(error(message))),
            },
        );
        dialog_authority.save_dialog(&ed.dialogs, ed.contents.window_of(view), options, back);
    }

    fn save_to_path_native(
        &self,
        view: ViewId,
        path: PathBuf,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        if !matches!(binding.operation().selection(), shell_contract::ipc::NativeInvocationSelection::NativeCommand(shell_contract::ipc::NativeEditorCommand::SaveDocument { surface: shell_contract::Surface::Pdf, view: selected_view, selection: shell_contract::ipc::NativeSaveSelection::ExactPath(selected_path) }) if *selected_view == view && selected_path == &path)
        {
            return then(Err(error("pdf: native save selection mismatch")));
        }
        let selection =
            match shell_contract::ipc::NativeSaveDestination::from_exact_selection(binding.clone())
            {
                Ok(selection) if selection.path() == path => selection,
                Ok(_) => return then(Err(error("pdf: native save destination mismatch"))),
                Err(message) => return then(Err(error(message))),
            };
        let ed = &self.0;
        let authority = match ed.capture_native_save_authority(view, binding, cancel, pending) {
            Ok(authority) => authority,
            Err(message) => return then(Err(error(message))),
        };
        if let Err(message) = authority.set_selection(selection) {
            return then(Err(error(message)));
        }
        if let Err(message) = ed.start_owned_save(view, authority.clone()) {
            return then(Err(error(message)));
        }
        let me = ed.me.clone();
        let request_authority = authority.clone();
        ed.request_save_as_outcome_impl(
            view,
            &path.to_string_lossy(),
            Some(request_authority),
            Box::new(move |outcome| {
                let current = authority.current();
                if let Some(ed) = me.upgrade() {
                    ed.end_owned_save(view, &authority.id);
                }
                then(current.map_err(error).and_then(|()| {
                    if outcome == SaveAsOutcome::Saved {
                        Ok(())
                    } else {
                        Err(error("pdf: could not save"))
                    }
                }));
            }),
        );
    }

    fn save_to_path_owned(
        &self,
        view: ViewId,
        path: PathBuf,
        origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        let ed = &self.0;
        let authority = match ed.capture_save_authority(view, origin, cancel) {
            Ok(authority) => authority,
            Err(message) => return then(Err(error(message))),
        };
        if let Err(message) = ed.start_owned_save(view, authority.clone()) {
            return then(Err(error(message)));
        }
        let me = ed.me.clone();
        let request_authority = authority.clone();
        ed.request_save_as_outcome_impl(
            view,
            &path.to_string_lossy(),
            Some(request_authority),
            Box::new(move |outcome| {
                let current = authority.current();
                if let Some(ed) = me.upgrade() {
                    ed.end_owned_save(view, &authority.id);
                }
                then(current.map_err(error).and_then(|()| {
                    if outcome == SaveAsOutcome::Saved {
                        Ok(())
                    } else {
                        Err(error("pdf: could not save"))
                    }
                }));
            }),
        );
    }

    /// `requestPdfClose(contents, parent)`
    fn request_close(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>) {
        let ed = &self.0;
        if !ed.is_dirty(view) || ed.services.views.sender_of(view).is_none() {
            return then(Ok(true));
        }
        let lang = ed.lang();
        let answer = ed.dialogs.message(
            parent,
            &MessageBox {
                kind: Some("warning".into()),
                message: tm(&lang, Dlg::CloseUnsavedMsg).into(),
                detail: Some(tm(&lang, Dlg::CloseUnsavedDetail).into()),
                buttons: vec![
                    tm(&lang, Dlg::BtnSave).into(),
                    tm(&lang, Dlg::BtnDontSave).into(),
                    tm(&lang, Dlg::BtnCancel).into(),
                ],
                default_id: Some(0),
                cancel_id: Some(2),
                ..Default::default()
            },
        );
        match answer.response {
            2 => then(Ok(false)),
            1 => {
                ed.discard_recovery(view);
                then(Ok(true))
            }
            _ => ed.request_renderer_save(view, true, Box::new(move |ok| then(Ok(ok)))),
        }
    }

    /// pdf-main.ts exported no text reader: MCP reads PDFs from the file
    fn read_text(&self, _view: ViewId, then: Then<String>) {
        then(Err(not_supported("reading the document's text")))
    }

    /// `requestPdfSaveAs(contents, path)`: the renderer writes its edits to `path`
    fn save_to_path(&self, view: ViewId, path: PathBuf, then: Then<()>) {
        let target = path.to_string_lossy().into_owned();
        let failed = format!("pdf: could not save to {target}");
        self.request_save_as(
            view,
            &target,
            Box::new(move |saved| then(if saved { Ok(()) } else { Err(error(failed)) })),
        )
    }

    /// `pdfFileRenamed(contents, oldPath, newPath)`
    fn file_renamed(&self, view: ViewId, old_path: &Path, new_path: &Path) {
        self.0.file_renamed(
            view,
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
        )
    }

    /// pdf-main.ts had no renderer export request (its exports are shell
    /// flows and `pdf:print-request` is M's): nothing to send
    fn export_request(&self, _view: ViewId, _format: &str) {}

    /// `openPathByWc.get(wc)`
    fn document_path(&self, view: ViewId) -> Option<PathBuf> {
        self.0.open_path(view).map(PathBuf::from)
    }
}

/// `basename(path)` without `.pdf`
pub(super) fn stem(path: &str) -> String {
    let name = basename(path, None);
    if name.to_ascii_lowercase().ends_with(".pdf") {
        name[..name.len() - 4].to_owned()
    } else {
        name
    }
}
