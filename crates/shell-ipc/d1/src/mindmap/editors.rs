//! `DocumentEditors` of the mind map app: what tabs, menus and the MCP
//! server ask of a map view, each the exported function of `mindmap-main.ts`
//! and `mcp-queries.ts` it stands for.

use std::path::{Path, PathBuf};

use shell_contract::editors::{DocumentEditors, SaveMode, Then};
use shell_contract::{ViewId, WindowId};
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::CancelToken;

use super::app::MindmapApp;
use crate::value::thrown;

pub(super) struct MindmapEditors(pub MindmapApp);

impl DocumentEditors for MindmapEditors {
    fn saved_surface(&self) -> shell_contract::Surface { shell_contract::Surface::Mindmap }
    fn prepare_saved_document(&self, target: shell_contract::ipc::NativeSavedTarget) -> Result<Box<dyn shell_contract::editors::PreparedSavedDocument>, shell_contract::IpcError> {
        super::saved_open::prepare(&self.0, target)
    }

    fn load_saved_document(&self, document: shell_contract::ipc::NativeSavedDocument, then: Then<shell_contract::editors::SavedDocumentLoadOutcome>) {
        super::saved_open::load(&self.0, document, then);
    }
    fn owns(&self, view: ViewId) -> bool {
        self.0.0.state.borrow().live.contains(&view)
    }

    fn capture_document_owner(&self, view: ViewId, native: NativeRequestOwner) -> Result<NativeRequestOwner, shell_contract::IpcError> {
        self.0.capture_save_owner(view, native)
    }

    /// `requestMindmapSave`
    fn request_save(&self, view: ViewId, mode: SaveMode, then: Then<bool>) {
        self.0
            .request_save(view, mode, Box::new(move |saved| then(Ok(saved))));
    }

    fn request_save_owned(&self, view: ViewId, mode: SaveMode, origin: NativeRequestOwner, cancel: CancelToken, then: Then<bool>) {
        self.0.request_save_owned(view, mode, origin, cancel, None, then);
    }

    fn request_save_native(&self, view: ViewId, mode: SaveMode, binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken, pending: CancelToken, then: Then<bool>) {
        self.0.request_save_native(view, mode, binding, cancel, pending, None, then);
    }

    /// `requestMindmapClose`
    fn request_close_owned(&self, view: ViewId, parent: Option<WindowId>, origin: NativeRequestOwner, cancel: CancelToken, then: Then<bool>) {
        self.0.request_close_owned(view, parent, origin, cancel, then);
    }

    fn request_close(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>) {
        self.0
            .request_close(view, parent, Box::new(move |proceed| then(Ok(proceed))));
    }

    /// `mindmapReadOutline`: the map as outline text, unsaved edits included
    fn read_text(&self, view: ViewId, then: Then<String>) {
        self.0
            .read_outline(view, Box::new(move |outline| then(outline.map_err(thrown))));
    }

    /// `mindmapSaveToPath`
    fn save_to_path(&self, view: ViewId, path: PathBuf, then: Then<()>) {
        self.0.save_to_path(
            view,
            &path.to_string_lossy(),
            Box::new(move |saved| then(saved.map_err(thrown))),
        );
    }

    fn save_to_path_owned(&self, view: ViewId, path: PathBuf, origin: NativeRequestOwner, cancel: CancelToken, then: Then<()>) {
        self.0.request_save_owned(view, SaveMode::Save, origin, cancel, Some(path.to_string_lossy().into_owned()), Box::new(move |saved| {
            then(saved.and_then(|saved| if saved { Ok(()) } else { Err(thrown("Could not save the map")) }));
        }));
    }

    fn save_to_path_native(&self, view: ViewId, path: PathBuf, binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken, pending: CancelToken, then: Then<()>) {
        let Some(path) = path.to_str().map(str::to_owned) else { return then(Err(thrown("The save path is not valid Unicode"))); };
        self.0.request_save_native(view, SaveMode::Save, binding, cancel, pending, Some(path), Box::new(move |saved| {
            then(saved.and_then(|saved| if saved { Ok(()) } else { Err(thrown("Could not save the map")) }));
        }));
    }

    /// `mindmapFileRenamed`
    fn file_renamed(&self, view: ViewId, old_path: &Path, new_path: &Path) {
        self.0.file_renamed(
            view,
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
        );
    }

    fn file_renamed_owned(&self, view: ViewId, old_path: &Path, new_path: &Path, origin: &NativeRequestOwner, destination: &NativeRequestOwner, cancel: &CancelToken, pending: &CancelToken, then: Then<()>) {
        let Some(old) = old_path.to_str() else { return then(Err(thrown("The old path is not valid Unicode"))); };
        let Some(new) = new_path.to_str() else { return then(Err(thrown("The new path is not valid Unicode"))); };
        self.0.rename_path_owned(view, old, new, origin, destination, cancel, pending, then);
    }

    fn file_renamed_many_owned_extra(&self, view: ViewId, old_path: &Path, new_path: &Path,
        origin: &NativeRequestOwner, destination: &NativeRequestOwner, participants: &[NativeRequestOwner],
        cancel: &CancelToken, pending: &CancelToken, additional: &[CancelToken], then: Then<()>) {
        let Some(old) = old_path.to_str() else { return then(Err(thrown("The old path is not valid Unicode"))); };
        let Some(new) = new_path.to_str() else { return then(Err(thrown("The new path is not valid Unicode"))); };
        self.0.rename_path_many_owned_extra(view, old, new, origin, destination, participants, cancel, pending, additional, then);
    }

    fn file_renamed_existing_document(&self, view: ViewId, old_path: &Path, new_path: &Path,
        authority: &shell_contract::ipc::NativeSaveRevisionAuthority, expected_path: &Path, destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken,
        additional: &[CancelToken], then: Then<Option<shell_contract::ipc::NativeSaveRevisionReceipt>>) {
        let Some(old) = old_path.to_str() else { return then(Err(thrown("The old path is not valid Unicode"))); };
        let Some(new) = new_path.to_str() else { return then(Err(thrown("The new path is not valid Unicode"))); };
        let snapshot = match authority.snapshot() { Ok(snapshot) => snapshot, Err(error) => return then(Err(thrown(error))) };
        self.0.rename_path_native_revision(view, old, new, &snapshot, expected_path, destination, participants, cancel, pending, additional, Box::new(move |result| then(result.map(|()| None))));
    }

    fn file_renamed_recovery(&self, view: ViewId, old_path: &Path, new_path: &Path,
        receipt: &shell_contract::ipc::RecoverySeedReceipt, receiver: &NativeRequestOwner, destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner], cancel: &CancelToken, pending: &CancelToken, then: Then<()>) {
        let Some(old) = old_path.to_str() else { return then(Err(thrown("The old path is not valid Unicode"))); };
        let Some(new) = new_path.to_str() else { return then(Err(thrown("The new path is not valid Unicode"))); };
        self.0.rename_path_recovery(view, old, new, receipt, receiver, destination, participants, cancel, pending, then);
    }

    /// `sendMindmapExportRequest`; `format` is png, svg, pdf or markdown
    fn export_request(&self, view: ViewId, format: &str) {
        self.0.send_export_request(view, format);
    }

    /// `mindmapFilePath`
    fn document_path(&self, view: ViewId) -> Option<PathBuf> {
        self.0.file_path(view).map(PathBuf::from)
    }
}
