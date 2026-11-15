//! The document editors as other areas call them: tabs and windows (W) ask
//! an editor to save before a close, menus (M) to save or export, MCP (A) to
//! read or save a document. In the Electron build these were the functions
//! each app's main module exported (`requestHtmlSave`, `requestHtmlClose`,
//! `htmlReadText`, `htmlSaveToPath`, `htmlFileRenamed`,
//! `sendHtmlExportRequest`, and their twins in the other apps).
//!
//! The document batches (D1, D2) implement [`DocumentEditors`] once per app
//! and add it to [`crate::provided::Provided`] while they register; a caller
//! finds the editor of a view with [`editor_for`]. Every call that waited on
//! the renderer in TypeScript takes a continuation the editor runs on the
//! core thread when the renderer answered: nothing blocks the core thread.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::error::IpcError;
use crate::ids::{ViewId, WindowId};
use crate::ipc::NativeRequestOwner;
use crate::provided::Provided;
use crate::runtime::CancelToken;

/// What a waiting call does with its outcome; runs once, on the core thread.
/// An `Err` is the TypeScript promise's rejection.
pub type Then<T> = Box<dyn FnOnce(Result<T, IpcError>)>;

/// The content actually applied by the original registered editor. Recovery
/// preserves the existing user choice; it does not attest saved bytes applied
/// or authorize closing/replacing a different browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SavedDocumentLoadOutcome {
    Applied,
    RecoveryApplied,
}

/// Actor registration and saved-input barrier prepared before browser admission.
/// Validation and the supplied manager transition run before any actor mutation.
pub trait PreparedSavedDocument {
    fn with_validated(
        &mut self,
        transition: &crate::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), IpcError>,
    ) -> Result<(), IpcError>;
    /// Return the preallocated exact registration after all gates have dropped.
    /// An uncommitted receipt cannot retire a published actor; rollback cancels
    /// only its own unpublished preparation.
    fn finish(self: Box<Self>) -> std::rc::Rc<dyn SavedActorRegistration>;
}

/// Exact actor insertion retained until its native browser is closed or replaced.
pub trait SavedActorRegistration {
    fn retire_closed(&self, receipt: crate::ipc::NativeClosureReceipt, then: Then<()>);
    fn retire_superseded(&self, proof: crate::ipc::NativeBrowserSuperseded, then: Then<()>);
    fn rollback(&self, then: Then<()>);
}

/// The apps' `SaveMode`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SaveMode {
    /// `'save'`: write to the document's file, asking for one if it has none
    Save,
    /// `'saveAs'`: always ask for a file
    SaveAs,
}
/// Captured authority for the original PDF-producing save, not an invented
/// renderer request when the save was initiated by a trusted native service.
#[derive(Clone)]
pub enum OwnedPdfGrantAuthority {
    Renderer(NativeRequestOwner),
    ExistingDocument {
        selection: crate::ipc::NativeSaveDestination,
        participants: Vec<NativeRequestOwner>,
    },
}

pub struct OwnedPdfGrant {
    pub path: PathBuf,
    pub request_token: String,
    pub authority: OwnedPdfGrantAuthority,
    pub destination: NativeRequestOwner,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    pub effects: crate::hosts::OwnedEffectsHost,
    pub accepted: crate::hosts::PdfReloadAccepted,
    pub accepted_losses: std::sync::Arc<std::sync::Mutex<Option<crate::ipc::AcceptedOwnerLosses>>>,
    pub complete: Box<dyn FnOnce(Result<(), String>) + Send>,
}

pub struct NativeHeadlessPdfGrant {
    pub path: PathBuf,
    pub document: crate::ipc::NativeInvocationDocument,
    pub sender: NativeRequestOwner,
    pub commit: Box<dyn FnOnce() + Send>,
}

/// One app's editors, all its views
pub trait DocumentEditors {
    /// The view shows a document of this app
    fn owns(&self, view: ViewId) -> bool;
    /// Editor family selection before a new saved actor is registered.
    fn saved_surface(&self) -> crate::Surface;

    fn capture_document_owner(
        &self,
        view: ViewId,
        native: NativeRequestOwner,
    ) -> Result<NativeRequestOwner, IpcError>;

    /// Prepare outside admission. No parser or renderer work starts here.
    /// A failed commit must leave the actor unchanged; original disposal owns
    /// the installed barrier and rejects its waiters on close or loss.
    fn prepare_saved_document(
        &self,
        target: crate::ipc::NativeSavedTarget,
    ) -> Result<Box<dyn PreparedSavedDocument>, IpcError>;

    /// Load the retained committed object into this exact registered native
    /// document. Queue and publish under the document's authority; parse off
    /// gate. Complete only after actual parsing and publication, never enqueue.
    fn load_saved_document(
        &self,
        document: crate::ipc::NativeSavedDocument,
        then: Then<SavedDocumentLoadOutcome>,
    );

    /// `request<App>Save(contents, mode)`: `true` when the document is saved
    /// (or had nothing to save), `false` when the user cancelled or it failed
    fn request_save(&self, view: ViewId, mode: SaveMode, then: Then<bool>);

    fn request_save_owned(
        &self,
        view: ViewId,
        mode: SaveMode,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<bool>,
    );

    fn request_save_native(
        &self,
        view: ViewId,
        mode: SaveMode,
        binding: crate::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<bool>,
    );

    /// `request<App>Close(contents, parent)`: asks about unsaved changes
    /// (modal to `parent`) and saves when told; `true` when the view may close
    fn request_close(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>);
    fn request_close_owned(
        &self,
        view: ViewId,
        parent: Option<WindowId>,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<bool>,
    );

    /// `<app>ReadText(contents)`: the document's text as the renderer gives it
    fn read_text(&self, view: ViewId, then: Then<String>);

    /// `<app>SaveToPath(contents, path)`: saves the document to `path`
    /// without a dialog
    fn save_to_path(&self, view: ViewId, path: PathBuf, then: Then<()>);

    fn save_to_path_owned(
        &self,
        view: ViewId,
        path: PathBuf,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<()>,
    );

    fn save_to_path_native(
        &self,
        view: ViewId,
        path: PathBuf,
        binding: crate::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<()>,
    );

    /// `<app>FileRenamed(contents, oldPath, newPath)`: the document's file was
    /// renamed or moved outside the editor
    fn file_renamed(&self, view: ViewId, old_path: &Path, new_path: &Path);

    fn file_renamed_owned(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        cancel: &CancelToken,
        pending: &CancelToken,
        then: Then<()>,
    );

    fn file_renamed_many_owned_extra(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    );

    fn file_renamed_existing_document(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        authority: &crate::ipc::NativeSaveRevisionAuthority,
        expected_path: &Path,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<Option<crate::ipc::NativeSaveRevisionReceipt>>,
    );

    fn file_renamed_recovery(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        receipt: &crate::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        then: Then<()>,
    );

    /// `send<App>ExportRequest(contents, format)`: the File > Export menu;
    /// the renderer runs the export. `format` is the app's own name for it
    /// (`"pdf"`, `"docx"`, ...)
    fn export_request(&self, view: ViewId, format: &str);

    /// The document's file (`savePathByWc.get(contents)`), `None` while it
    /// is unsaved
    fn document_path(&self, view: ViewId) -> Option<PathBuf>;

    /// hwp only: the PDF file this view chose in Save As or was handed in a
    /// headless run (`pdfSaveAsByWc ?? headlessPdfByWc`), taken once
    fn take_pdf_grant(&self, _view: ViewId) -> Option<PathBuf> {
        None
    }

    fn take_owned_pdf_grant(
        &self,
        _view: ViewId,
        _request_token: &str,
        _generation: &crate::runtime::RendererGeneration,
        _actual: Option<&NativeRequestOwner>,
        _caller_cancel: &CancelToken,
    ) -> Option<OwnedPdfGrant> {
        None
    }

    fn take_native_headless_pdf_grant(
        &self,
        _view: ViewId,
        _native: Option<&crate::ipc::NativeContext>,
    ) -> Result<Option<NativeHeadlessPdfGrant>, String> {
        Ok(None)
    }

    /// docs only: the view's writer is gone (`tornDownWcIds`)
    fn torn_down(&self, _view: ViewId) -> bool {
        false
    }
}

/// The editor of the app that shows `view`, if any
pub fn editor_for(provided: &Provided, view: ViewId) -> Option<Rc<dyn DocumentEditors>> {
    provided
        .all::<dyn DocumentEditors>()
        .into_iter()
        .find(|editor| editor.owns(view))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// An app that owns some views and records what it was asked
    struct Fake {
        views: Vec<u32>,
        asked: RefCell<Vec<String>>,
        surface: crate::Surface,
        saved: crate::hosts::synthetic::SyntheticSavedActors,
        effects: crate::hosts::OwnedEffectsHost,
    }

    impl DocumentEditors for Fake {
        fn owns(&self, view: ViewId) -> bool {
            self.views.contains(&view.0) || self.saved.owns(view)
        }
        fn saved_surface(&self) -> crate::Surface {
            self.surface
        }
        fn prepare_saved_document(
            &self,
            target: crate::ipc::NativeSavedTarget,
        ) -> Result<Box<dyn PreparedSavedDocument>, IpcError> {
            if target.surface() != self.surface {
                return Err(IpcError {
                    name: Some("Error".into()),
                    message: "The saved target has another editor family".into(),
                });
            }
            self.saved.prepare(target)
        }
        fn load_saved_document(
            &self,
            document: crate::ipc::NativeSavedDocument,
            then: Then<SavedDocumentLoadOutcome>,
        ) {
            self.saved.load(document, &self.effects, then);
        }
        fn capture_document_owner(
            &self,
            _view: ViewId,
            native: NativeRequestOwner,
        ) -> Result<NativeRequestOwner, IpcError> {
            Ok(native)
        }
        fn request_save(&self, view: ViewId, mode: SaveMode, then: Then<bool>) {
            self.asked
                .borrow_mut()
                .push(format!("save {} {mode:?}", view.0));
            then(Ok(true))
        }
        fn request_save_owned(
            &self,
            view: ViewId,
            mode: SaveMode,
            _origin: NativeRequestOwner,
            _cancel: CancelToken,
            then: Then<bool>,
        ) {
            self.request_save(view, mode, then)
        }
        fn request_save_native(
            &self,
            view: ViewId,
            mode: SaveMode,
            binding: crate::ipc::NativeExistingDocumentBinding,
            cancel: CancelToken,
            pending: CancelToken,
            then: Then<bool>,
        ) {
            let result = self.effects.with_existing_document_save_effect(&binding, &[], &cancel, &pending, &mut || {
                if !matches!(binding.operation().selection(), crate::ipc::NativeInvocationSelection::NativeCommand(
                    crate::ipc::NativeEditorCommand::SaveDocument { view: selected, selection: crate::ipc::NativeSaveSelection::Mode(selected_mode), .. })
                    if *selected == view && *selected_mode == mode) || !self.owns(view) {
                    return Err("The synthetic editor save selection changed".into());
                }
                Ok(())
            });
            match result {
                Ok(()) => self.request_save(view, mode, then),
                Err(message) => then(Err(IpcError {
                    name: Some("Error".into()),
                    message,
                })),
            }
        }
        fn request_close(&self, view: ViewId, _parent: Option<WindowId>, then: Then<bool>) {
            self.asked.borrow_mut().push(format!("close {}", view.0));
            then(Ok(false))
        }
        fn request_close_owned(
            &self,
            view: ViewId,
            parent: Option<WindowId>,
            _origin: NativeRequestOwner,
            _cancel: CancelToken,
            then: Then<bool>,
        ) {
            self.request_close(view, parent, then)
        }
        fn read_text(&self, view: ViewId, then: Then<String>) {
            then(Err(IpcError {
                name: Some("Error".into()),
                message: format!("no text in {}", view.0),
            }))
        }
        fn save_to_path(&self, _view: ViewId, _path: PathBuf, then: Then<()>) {
            then(Ok(()))
        }
        fn save_to_path_owned(
            &self,
            view: ViewId,
            path: PathBuf,
            _origin: NativeRequestOwner,
            _cancel: CancelToken,
            then: Then<()>,
        ) {
            self.save_to_path(view, path, then)
        }
        fn save_to_path_native(
            &self,
            view: ViewId,
            path: PathBuf,
            binding: crate::ipc::NativeExistingDocumentBinding,
            cancel: CancelToken,
            pending: CancelToken,
            then: Then<()>,
        ) {
            let result = self.effects.with_existing_document_save_effect(&binding, &[], &cancel, &pending, &mut || {
                if !matches!(binding.operation().selection(), crate::ipc::NativeInvocationSelection::NativeCommand(
                    crate::ipc::NativeEditorCommand::SaveDocument { view: selected, selection: crate::ipc::NativeSaveSelection::ExactPath(selected_path), .. })
                    if *selected == view && selected_path == &path) || !self.owns(view) {
                    return Err("The synthetic editor save selection changed".into());
                }
                Ok(())
            });
            match result {
                Ok(()) => self.save_to_path(view, path, then),
                Err(message) => then(Err(IpcError {
                    name: Some("Error".into()),
                    message,
                })),
            }
        }
        fn file_renamed(&self, view: ViewId, old_path: &Path, new_path: &Path) {
            self.asked.borrow_mut().push(format!(
                "renamed {} {} {}",
                view.0,
                old_path.display(),
                new_path.display()
            ));
        }
        fn file_renamed_owned(
            &self,
            view: ViewId,
            old_path: &Path,
            new_path: &Path,
            _origin: &NativeRequestOwner,
            _destination: &NativeRequestOwner,
            _cancel: &CancelToken,
            _pending: &CancelToken,
            then: Then<()>,
        ) {
            self.file_renamed(view, old_path, new_path);
            then(Ok(()))
        }
        fn file_renamed_many_owned_extra(
            &self,
            view: ViewId,
            old_path: &Path,
            new_path: &Path,
            origin: &NativeRequestOwner,
            destination: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            additional: &[CancelToken],
            then: Then<()>,
        ) {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(destination.clone());
            captured.extend_from_slice(participants);
            let additional = additional.iter().collect::<Vec<_>>();
            let result = crate::hosts::synthetic::with_owned_effect_many_extra(
                origin,
                &captured,
                cancel,
                pending,
                &additional,
                &mut || {
                    self.file_renamed(view, old_path, new_path);
                    Ok(())
                },
            );
            then(result.map_err(|message| IpcError {
                name: Some("Error".into()),
                message,
            }));
        }
        fn file_renamed_existing_document(
            &self,
            view: ViewId,
            old: &Path,
            new: &Path,
            authority: &crate::ipc::NativeSaveRevisionAuthority,
            expected_path: &Path,
            destination: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            additional: &[CancelToken],
            then: Then<Option<crate::ipc::NativeSaveRevisionReceipt>>,
        ) {
            let mut captured = participants.to_vec();
            captured.push(destination.clone());
            let additional = additional.iter().collect::<Vec<_>>();
            let result = authority.snapshot().and_then(|snapshot| {
                if snapshot.selection().path() != expected_path {
                    return Err("The native save path changed".into());
                }
                self.effects.with_native_save_revision_snapshot_effect(
                    &snapshot,
                    &captured,
                    cancel,
                    pending,
                    &additional,
                    &mut || {
                        if destination.view != view || !self.owns(view) {
                            return Err("The synthetic editor was replaced".into());
                        }
                        self.file_renamed(view, old, new);
                        Ok(())
                    },
                )
            });
            then(result.map(|()| None).map_err(|message| IpcError {
                name: Some("Error".into()),
                message,
            }));
        }
        fn file_renamed_recovery(
            &self,
            view: ViewId,
            old_path: &Path,
            new_path: &Path,
            receipt: &crate::ipc::RecoverySeedReceipt,
            receiver: &NativeRequestOwner,
            destination: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            then: Then<()>,
        ) {
            let mut captured = Vec::with_capacity(participants.len() + 1);
            captured.push(destination.clone());
            captured.extend_from_slice(participants);
            let result = crate::hosts::synthetic::with_recovery_open_many(
                receipt,
                receiver,
                &captured,
                cancel,
                pending,
                &mut || {
                    self.file_renamed(view, old_path, new_path);
                    Ok(())
                },
            );
            then(result.map_err(|message| IpcError {
                name: Some("Error".into()),
                message,
            }));
        }
        fn export_request(&self, view: ViewId, format: &str) {
            self.asked
                .borrow_mut()
                .push(format!("export {} {format}", view.0));
        }
        fn document_path(&self, view: ViewId) -> Option<PathBuf> {
            self.owns(view)
                .then(|| PathBuf::from(format!("/doc/{}", view.0)))
        }
    }

    fn fake(views: &[u32], surface: crate::Surface) -> Rc<Fake> {
        Rc::new(Fake {
            views: views.to_vec(),
            asked: RefCell::default(),
            surface,
            saved: crate::hosts::synthetic::SyntheticSavedActors::default(),
            effects: crate::hosts::synthetic::tests::editor_effects_fixture(),
        })
    }

    #[test]
    fn a_view_reaches_the_editor_that_owns_it() {
        let provided = Provided::default();
        let docs = fake(&[1, 2], crate::Surface::Docs);
        let pdf = fake(&[3], crate::Surface::Pdf);
        provided.add::<dyn DocumentEditors>(docs.clone());
        provided.add::<dyn DocumentEditors>(pdf.clone());

        let answer = Rc::new(RefCell::new(None));
        let got = answer.clone();
        editor_for(&provided, ViewId(3)).unwrap().request_save(
            ViewId(3),
            SaveMode::SaveAs,
            Box::new(move |r| *got.borrow_mut() = Some(r)),
        );
        assert_eq!(*answer.borrow(), Some(Ok(true)));
        assert_eq!(*pdf.asked.borrow(), vec!["save 3 SaveAs"]);
        assert!(docs.asked.borrow().is_empty());

        editor_for(&provided, ViewId(2))
            .unwrap()
            .export_request(ViewId(2), "pdf");
        assert_eq!(*docs.asked.borrow(), vec!["export 2 pdf"]);

        assert!(editor_for(&provided, ViewId(9)).is_none());
    }

    #[test]
    fn types_do_not_mix() {
        let provided = Provided::default();
        provided.add::<dyn DocumentEditors>(fake(&[1], crate::Surface::Docs));
        provided.add::<u32>(Rc::new(7));
        assert_eq!(provided.all::<dyn DocumentEditors>().len(), 1);
        assert_eq!(provided.all::<u32>().iter().map(|n| **n).sum::<u32>(), 7);
        assert!(provided.all::<String>().is_empty());
    }
}
