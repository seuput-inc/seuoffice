//! The diagram editor as the other areas call it ([`DocumentEditors`]): tabs
//! ask it to save before a close, menus to save or export, MCP to read or
//! save. Each call is the exported function of `diagram-main.ts` /
//! `mcp-queries.ts` of the same job.

use std::path::{Path, PathBuf};

use shell_contract::editors::{DocumentEditors, SaveMode, Then};
use shell_contract::{IpcError, ViewId, WindowId};

use super::DiagramViews;
use crate::host::path_string;
use crate::value::thrown;

pub(super) struct DiagramEditors(pub DiagramViews);

fn rejected(message: String) -> IpcError {
    thrown(message)
}

impl DocumentEditors for DiagramEditors {
    fn saved_surface(&self) -> shell_contract::Surface {
        shell_contract::Surface::Diagram
    }

    fn owns(&self, view: ViewId) -> bool {
        self.0
            .with_state(|state| state.views.get(&view).is_some_and(|v| v.tracked))
    }

    fn prepare_saved_document(
        &self,
        target: shell_contract::ipc::NativeSavedTarget,
    ) -> Result<Box<dyn shell_contract::editors::PreparedSavedDocument>, IpcError> {
        super::saved_load::prepare(self.0.clone(), target)
    }

    fn load_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        then: Then<shell_contract::editors::SavedDocumentLoadOutcome>,
    ) {
        super::saved_load::load(&self.0, document, then);
    }

    /// `requestDiagramSave(contents, mode)`
    fn request_save(&self, view: ViewId, mode: SaveMode, then: Then<bool>) {
        self.0
            .request_save(view, mode, Box::new(move |saved| then(Ok(saved))));
    }

    fn request_save_owned(
        &self,
        view: ViewId,
        mode: SaveMode,
        origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        self.0
            .request_owned_save(view, mode, None, origin, cancel, then);
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
        self.0
            .request_native_save(view, mode, None, binding, cancel, pending, then);
    }

    fn request_close_owned(
        &self,
        view: ViewId,
        parent: Option<WindowId>,
        origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        self.0
            .request_owned_close(view, parent, origin, cancel, then);
    }

    /// `requestDiagramClose(contents, parent)`
    fn request_close(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>) {
        self.0
            .request_close(view, parent, Box::new(move |close| then(Ok(close))));
    }

    /// `diagramReadText(contents)`: every page as the read table text
    fn read_text(&self, view: ViewId, then: Then<String>) {
        self.0
            .read_text(view, Box::new(move |text| then(text.map_err(rejected))));
    }

    /// `diagramSaveToPath(contents, filePath)`
    fn save_to_path(&self, view: ViewId, path: PathBuf, then: Then<()>) {
        self.0.save_to_path(
            view,
            &path_string(path),
            Box::new(move |saved| then(saved.map_err(rejected))),
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
        self.0.request_owned_save(
            view,
            SaveMode::Save,
            Some(path),
            origin,
            cancel,
            Box::new(move |result| {
                then(result.and_then(|saved| {
                    if saved {
                        Ok(())
                    } else {
                        Err(thrown("could not save the diagram"))
                    }
                }));
            }),
        );
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
        self.0.request_native_save(
            view,
            SaveMode::Save,
            Some(path),
            binding,
            cancel,
            pending,
            Box::new(move |result| {
                then(result.and_then(|saved| {
                    if saved {
                        Ok(())
                    } else {
                        Err(thrown("could not save the diagram"))
                    }
                }));
            }),
        );
    }

    /// `diagramFileRenamed(contents, oldPath, newPath)`
    fn file_renamed(&self, view: ViewId, old_path: &Path, new_path: &Path) {
        self.0.file_renamed(
            view,
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
        );
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
        self.0.rename_owned_diagram(
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
        self.0.rename_owned_diagram(
            view,
            old_path,
            new_path,
            origin,
            destination,
            participants,
            cancel,
            pending,
            additional,
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
        let snapshot = match authority.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return then(Err(thrown(error))),
        };
        self.0.rename_native_diagram(
            view,
            old_path,
            new_path,
            &snapshot,
            expected_path,
            destination,
            participants,
            cancel,
            pending,
            additional,
            Box::new(move |result| then(result.map(|()| None))),
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
        self.0.rename_recovery_diagram(
            view,
            old_path,
            new_path,
            receipt,
            receiver,
            destination,
            participants,
            cancel,
            pending,
            then,
        );
    }

    fn capture_document_owner(
        &self,
        view: ViewId,
        mut native: shell_contract::ipc::NativeRequestOwner,
    ) -> Result<shell_contract::ipc::NativeRequestOwner, IpcError> {
        let document = self
            .0
            .with_state(|state| state.views.get(&view)?.document.clone())
            .ok_or_else(|| thrown("The diagram document is unavailable"))?;
        if native.view != view || !document.is_active() {
            return Err(thrown("The diagram document was released"));
        }
        if !native
            .context
            .renderer()
            .zip(self.0.0.services.views.renderer_generation(view).as_ref())
            .is_some_and(|(expected, current)| {
                expected.is_active() && expected.same_generation(current)
            })
            || !native.loss_token().is_some_and(|loss| !loss.is_cancelled())
        {
            return Err(thrown("The diagram renderer was replaced"));
        }
        if let Some(expected) = native.context.document_lease()
            && !expected.same_generation(&document)
        {
            return Err(thrown("The diagram document was replaced"));
        }
        native.context = native.context.with_document_lease(document);
        Ok(native)
    }

    /// `sendDiagramExportRequest(contents, format)`: `png`, `svg` or `pdf`
    fn export_request(&self, view: ViewId, format: &str) {
        self.0.send_export_request(view, format);
    }

    /// `diagramFilePath(webContentsId)`
    fn document_path(&self, view: ViewId) -> Option<PathBuf> {
        self.0.file_path(view).map(PathBuf::from)
    }
}
