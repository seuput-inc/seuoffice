//! What other areas call on the Word editor: [`DocumentEditors`] for the
//! tabs, menus and MCP, and the functions `docs-main.ts` exported to the
//! shell (the first-document queues, teardown, crash recovery's contract,
//! the MCP write grant, the headless export), public for the shell.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use main_core::packages::electron_utils::recovery_contract::{
    RecoveryEditorContract, RecoveryOpenDecision, RecoveryOpenFileRequest,
    RecoveryOpenUntitledRequest,
};
use main_core::packages::electron_utils::recovery_store::{
    RecoveryIdentity, RecoveryUntitledIdentity,
};
use shell_contract::editors::{DocumentEditors, SaveMode, Then};
use shell_contract::ipc::Target;
use shell_contract::{IpcValue, Surface, ViewId, WindowId};

use super::Docs;
pub use super::blank_seed::{DocsBlankReceipt, DocsPreparedBlank, DocsValidatedBlank};
pub use super::disposal::{
    DisposalAdmission, DisposalDone, DocsActorDisposal, DocsDisposalOutcome,
};
pub use super::open_queue::{DocsOpenReceipt, DocsPreparedOpen, DocsValidatedOpen};
pub use super::prepared_registration::{
    DocsPreparedRegistration, DocsRegisteredView, DocsValidatedRegistration,
};
use crate::kit;
pub use crate::kit::{CoreReplay, back_on_core, replay_on_core};

/// The Word editor of every docs view
pub struct DocsEditors(pub(super) Rc<Docs>);

impl DocumentEditors for DocsEditors {
    fn saved_surface(&self) -> Surface {
        Surface::Docs
    }

    fn prepare_saved_document(
        &self,
        target: shell_contract::ipc::NativeSavedTarget,
    ) -> Result<Box<dyn shell_contract::editors::PreparedSavedDocument>, shell_contract::IpcError>
    {
        self.prepare_saved_actor(target)
            .map(|(prepared, _lease)| prepared)
    }

    fn load_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        then: Then<shell_contract::editors::SavedDocumentLoadOutcome>,
    ) {
        let view = document.target().view();
        if let Ok(true) = super::saved_load::queue_document(&self.0, document, then) {
            super::saved_load::start(&self.0, view);
        }
    }

    fn owns(&self, view: ViewId) -> bool {
        self.0
            .services
            .views
            .sender_of(view)
            .is_some_and(|s| s.surface == Surface::Docs)
    }

    fn capture_document_owner(
        &self,
        view: ViewId,
        native: shell_contract::ipc::NativeRequestOwner,
    ) -> Result<shell_contract::ipc::NativeRequestOwner, shell_contract::IpcError> {
        self.0.checked_document_owner(view, native)
    }

    /// The shell's tab menu saved a Word tab with `menu:command 'save'` or
    /// `'save-as'` (tab-document-actions.ts): the renderer runs its own save
    /// flow and reports nothing back, so the answer is that the request went out.
    fn request_save(&self, view: ViewId, mode: SaveMode, then: Then<bool>) {
        let command = match mode {
            SaveMode::Save => "save",
            SaveMode::SaveAs => "save-as",
        };
        self.0.services.events.emit(
            Target::View(view),
            "menu:command",
            vec![IpcValue::String(command.into())],
        );
        then(Ok(true))
    }

    fn request_close(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>) {
        self.0
            .request_close(view, parent, Box::new(move |proceed| then(Ok(proceed))))
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
            .request_close_owned(view, parent, origin, cancel, then);
    }

    /// The Word editor had no `docsReadText`: MCP reads its text through
    /// the renderer bridge (`docs:mcp-command`, batch A)
    fn read_text(&self, _view: ViewId, then: Then<String>) {
        then(Err(kit::error(
            "the Word editor's text is read through its MCP bridge",
        )))
    }

    fn save_to_path(&self, view: ViewId, path: PathBuf, then: Then<()>) {
        super::renderer_save::save_to_path(&self.0, view, path, then)
    }

    fn save_to_path_owned(
        &self,
        view: ViewId,
        path: PathBuf,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        super::owned_save::start(
            &self.0,
            view,
            SaveMode::Save,
            Some(path),
            owner,
            cancel,
            Box::new(move |result| {
                then(result.and_then(|saved| {
                    if saved {
                        Ok(())
                    } else {
                        Err(kit::error("the document could not be saved"))
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
        super::owned_save::start_native(
            &self.0,
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
                        Err(kit::error("the document could not be saved"))
                    }
                }));
            }),
        );
    }

    fn request_save_owned(
        &self,
        view: ViewId,
        mode: SaveMode,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        super::owned_save::start(&self.0, view, mode, None, owner, cancel, then);
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
        super::owned_save::start_native(&self.0, view, mode, None, binding, cancel, pending, then);
    }

    fn file_renamed(&self, view: ViewId, old_path: &Path, new_path: &Path) {
        self.0.file_renamed(
            view,
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
        )
    }

    fn file_renamed_owned(
        &self,
        view: ViewId,
        old: &Path,
        new: &Path,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        self.file_renamed_many_owned_extra(
            view,
            old,
            new,
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
        old: &Path,
        new: &Path,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        then: Then<()>,
    ) {
        super::owned_save::file_renamed_many(
            &self.0,
            view,
            old,
            new,
            origin.clone(),
            destination.clone(),
            participants.to_vec(),
            cancel.clone(),
            pending.clone(),
            additional.to_vec(),
            then,
        );
    }

    fn file_renamed_existing_document(
        &self,
        view: ViewId,
        old: &Path,
        new: &Path,
        authority: &shell_contract::ipc::NativeSaveRevisionAuthority,
        expected_path: &Path,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        then: Then<Option<shell_contract::ipc::NativeSaveRevisionReceipt>>,
    ) {
        super::owned_save::file_renamed_existing_document(
            &self.0,
            view,
            old,
            new,
            authority,
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
        old: &Path,
        new: &Path,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        super::owned_save::file_renamed_recovery(
            &self.0,
            view,
            old,
            new,
            receipt,
            receiver,
            destination,
            participants,
            cancel,
            pending,
            then,
        );
    }

    /// The File > Export menu: `sendCommand('export-<format>')`
    fn export_request(&self, view: ViewId, format: &str) {
        self.0.services.events.emit(
            Target::View(view),
            "menu:command",
            vec![IpcValue::String(format!("export-{format}"))],
        )
    }

    /// `openDocByWc.get(id)`
    fn document_path(&self, view: ViewId) -> Option<PathBuf> {
        self.0.open_doc.get(view).map(PathBuf::from)
    }

    /// `tornDownWcIds.has(id)`
    fn torn_down(&self, view: ViewId) -> bool {
        self.0.is_torn_down(view)
    }
}

impl shell_contract::hosts::DocsHeadlessPrint for DocsEditors {
    fn capture(
        &self,
        cx: &shell_contract::ipc::CallContext,
    ) -> Result<Option<shell_contract::hosts::NativeInvocationPrint>, shell_contract::IpcError>
    {
        if !self.0.shell.is_headless() {
            return Ok(None);
        }
        let selected = self
            .0
            .headless_waiters
            .borrow()
            .get(&cx.sender.view)
            .and_then(|(target, _)| target.clone());
        let Some((target, _)) = selected else {
            return self
                .0
                .capture_headless_publication(cx, "", "pdf")
                .map(|_| None)
                .map_err(kit::error);
        };
        let path = match target.operation().selection() {
            shell_contract::ipc::NativeInvocationSelection::HeadlessExport {
                surface: Surface::Docs,
                output,
                format,
                ..
            } if format == "pdf" => output
                .to_str()
                .ok_or_else(|| kit::error("the Docs PDF output path is not Unicode"))?
                .to_owned(),
            _ => return Err(kit::error("the native Docs request is not a PDF export")),
        };
        self.0
            .capture_headless_publication(cx, &path, "pdf")
            .map(|capture| {
                capture.map(|capture| shell_contract::hosts::NativeInvocationPrint {
                    document: capture.document,
                    owner: capture.owner,
                    participants: Vec::new(),
                    cancel: capture.cancel,
                    pending: capture.pending,
                })
            })
            .map_err(kit::error)
    }
}

impl RecoveryEditorContract<ViewId, shell_contract::ipc::RecoveryOpenTarget> for DocsEditors {
    fn kind(&self) -> &str {
        "docs"
    }

    fn write_copies_now(
        &self,
        done: Box<dyn FnOnce(Result<(), main_core::shell::renderer_disposal::Failure>)>,
    ) {
        DocsEditors::write_copies_now(self, done);
    }

    fn open_file_with_decision(
        &self,
        view: &ViewId,
        request: &RecoveryOpenFileRequest,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        super::recovery_owned::file(&self.0, *view, request, target, done);
    }

    fn open_untitled_copy(
        &self,
        view: &ViewId,
        request: &RecoveryOpenUntitledRequest,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        super::recovery_owned::untitled(&self.0, *view, request, target, done);
    }

    fn is_unprotectable(&self, _view: &ViewId) -> bool {
        false
    }
}

impl DocsEditors {
    pub fn prepare_saved_actor(
        &self,
        target: shell_contract::ipc::NativeSavedTarget,
    ) -> Result<
        (
            Box<dyn shell_contract::editors::PreparedSavedDocument>,
            main_core::docs::byte_handoff::MediaViewLease,
        ),
        shell_contract::IpcError,
    > {
        let registration = self
            .prepare_saved_registration(&target)
            .map_err(kit::error)?;
        let lease = registration.lease();
        let prepared = super::saved_load::prepare(&self.0, target, registration)?;
        Ok((prepared, lease))
    }
    pub fn wake_generated_cleanups(&self) {
        super::cleanup_mailbox::wake(&self.0);
    }
    pub fn wake_saved_load(
        &self,
        target: &shell_contract::ipc::NativeSavedTarget,
    ) -> Result<(), String> {
        let effects = self
            .0
            .services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .ok_or("Native saved document effects are unavailable")?;
        let extra = target.additional().iter().collect::<Vec<_>>();
        effects.with_native_saved_target_seed(
            target,
            &[],
            target.cancel(),
            target.pending(),
            &extra,
            &mut |_| Ok(()),
        )?;
        super::saved_load::wake_target(&self.0, target);
        Ok(())
    }

    pub fn queue_blank_recovery(
        &self,
        view: ViewId,
        target: &shell_contract::ipc::RecoveryOpenTarget,
        title: String,
        done: main_core::packages::electron_utils::recovery_contract::RecoveryOpenDone,
    ) {
        super::recovery_owned::blank(&self.0, view, target, title, done);
    }

    pub fn capture_disposal(
        &self,
        view: ViewId,
        target: &shell_contract::ipc::NativeRequestOwner,
    ) -> Result<DocsActorDisposal, String> {
        super::disposal::capture(&self.0, view, target)
    }
    pub fn prepare_saved_registration(
        &self,
        target: &shell_contract::ipc::NativeSavedTarget,
    ) -> Result<DocsPreparedRegistration, String> {
        super::prepared_registration::prepare_saved(&self.0, target)
    }

    pub fn prepare_view_registration(
        &self,
        view: ViewId,
        created: shell_contract::runtime::RendererGeneration,
        path: Option<String>,
        reservation: &shell_contract::ipc::NativeClosureReservation,
        owner: &shell_contract::ipc::NativeBrowserOwner,
    ) -> Result<DocsPreparedRegistration, String> {
        super::prepared_registration::prepare(&self.0, view, created, path, reservation, owner)
    }
    pub fn prepare_queue_open(
        &self,
        view: ViewId,
        path: String,
        created: shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
    ) -> Result<DocsPreparedOpen, String> {
        super::open_queue::prepare(&self.0, view, path, created, reservation)
    }
    pub fn prepare_blank_seed(
        &self,
        view: ViewId,
        created: shell_contract::runtime::RendererGeneration,
        reservation: &shell_contract::ipc::NativeClosureReservation,
    ) -> Result<DocsPreparedBlank, String> {
        super::blank_seed::prepare(&self.0, view, created, reservation)
    }

    #[cfg(any(test, feature = "probe"))]
    pub fn register_view(
        &self,
        view: ViewId,
    ) -> Result<main_core::docs::byte_handoff::MediaViewLease, String> {
        self.0.register_media_view(view)
    }

    pub fn media_permit(&self, view: ViewId) -> Option<main_core::docs::byte_handoff::MediaPermit> {
        self.0.media_permit(view)
    }

    /// Asks the renderer for its current dirty state without blocking the core thread.
    pub fn query_dirty(&self, view: ViewId, then: Then<bool>) {
        if !self.0.alive(view) {
            return then(Err(shell_contract::IpcError {
                name: Some("Error".into()),
                message: "The document view is no longer available".into(),
            }));
        }
        self.0.query_close_state(
            view,
            Box::new(move |state| {
                if state.unresponsive {
                    then(Err(shell_contract::IpcError {
                        name: Some("Error".into()),
                        message: "The document renderer did not answer".into(),
                    }));
                } else {
                    then(Ok(state.dirty));
                }
            }),
        );
    }

    /// A tokenized query scoped to both the requesting and target document.
    pub fn query_dirty_owned(
        &self,
        view: ViewId,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        super::dirty_owned::start(&self.0, view, owner, cancel, then)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn query_dirty_captured_many(
        &self,
        view: ViewId,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        if destination.view != view {
            return then(Err(crate::kit::error(
                "The captured document does not match the queried view",
            )));
        }
        super::dirty_owned::start_captured(
            &self.0,
            origin,
            participants,
            destination,
            cancel,
            pending,
            false,
            Box::new(move |result| then(result.map(|state| state.dirty))),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn request_close_captured_many(
        &self,
        view: ViewId,
        parent: Option<shell_contract::WindowId>,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        self.0.request_close_captured_many(
            view,
            parent,
            origin,
            participants,
            destination,
            cancel,
            pending,
            then,
        );
    }

    /// `activeViewMenuState()` for one view: the last `docs:view-menu-state`
    /// it sent as `[aiSidebar, darkCanvas, odt]`, `None` before its first
    /// (the menu then shows the defaults). The Docs menu reads it when it is
    /// rebuilt for a tab that becomes active.
    pub fn view_menu_state(&self, view: ViewId) -> Option<[bool; 3]> {
        self.0.view_menu.get(view)
    }

    /// `markDocsNewBlank(wcId)`
    pub fn mark_new_blank(&self, view: ViewId) {
        self.0.new_blank.borrow_mut().insert(view);
    }

    /// `queueDocsCopy(wcId, { path, name })`
    pub fn queue_copy(&self, view: ViewId, path: String, name: String) {
        self.0.doc_copies.insert(view, (path, name));
    }

    /// `createDocsView(openPath)`'s queue: the tab's first document
    pub fn queue_open(&self, view: ViewId, path: String) {
        self.0.window_opens.insert(view, path);
    }

    /// `authorizeMcpDocWrite(wcId, filePath)`
    pub fn authorize_mcp_doc_write(&self, view: ViewId, path: &str) {
        self.0.allow_doc_write(view, path)
    }

    /// `teardownDocsRenderer(contents)`; `crashed` is `contents.isCrashed()`
    pub fn teardown(&self, view: ViewId, crashed: bool) {
        self.0.teardown(view, crashed)
    }

    /// `discardDocsRecovery(contents)`
    pub fn discard_recovery(&self, view: ViewId) {
        let open = self.0.open_doc.get(view);
        self.0.discard_tab_copies(view, open.as_deref())
    }

    /// `docsRecoveryIdentity(webContentsId)`
    pub fn recovery_identity(&self, view: ViewId) -> Option<RecoveryUntitledIdentity> {
        self.0.book.borrow().untitled_of(view)
    }

    /// `docsRecoveryEditor.writeCopiesNow`
    pub fn write_copies_now(
        &self,
        then: Box<dyn FnOnce(Result<(), main_core::shell::renderer_disposal::Failure>)>,
    ) {
        self.0.write_copies_now(then)
    }

    /// `docsRecoveryEditor.openFileWithDecision`
    #[cfg(any(test, feature = "probe"))]
    pub fn open_file_with_decision(
        &self,
        view: ViewId,
        path: String,
        decision: RecoveryOpenDecision,
        owner: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
    ) {
        self.0.open_file_with_decision(
            view,
            path,
            decision,
            super::open::LoadAuthority { owner, cancel },
        )
    }

    /// `docsRecoveryEditor.openUntitledCopy`
    #[cfg(any(test, feature = "probe"))]
    pub fn open_untitled_copy(
        &self,
        view: ViewId,
        copy: RecoveryIdentity,
        title: String,
        save_as_default_path: Option<String>,
    ) {
        self.0
            .open_untitled_copy(view, copy, title, save_as_default_path)
    }

    /// `createAiDocument(request, ownerWebContentsId)` as other apps call it
    #[cfg(any(test, feature = "probe"))]
    pub fn create_ai_document(
        &self,
        request: &IpcValue,
        owner: ViewId,
        then: Box<dyn FnOnce(IpcValue)>,
    ) {
        self.0.create_ai_document(request, owner, then)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_ai_document_owned(
        &self,
        request: IpcValue,
        owner: ViewId,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        admission: main_core::packages::electron_utils::atomic_write::owned::Admission,
        then: Box<dyn FnOnce(Result<IpcValue, String>) + Send>,
    ) {
        self.0.create_ai_document_owned(
            request,
            owner,
            origin,
            destination,
            cancel,
            pending,
            admission,
            then,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_headless_export_native(
        &self,
        view: ViewId,
        input: String,
        out_path: String,
        format: &str,
        timeout: std::time::Duration,
        target: &shell_contract::ipc::NativeInvocationTarget,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.0
            .begin_headless_export_native(view, input, out_path, format, timeout, target, then)
    }

    /// `exportDocsHeadless` past its hidden window
    #[cfg(any(test, feature = "probe"))]
    pub fn begin_headless_export(
        &self,
        view: ViewId,
        input: String,
        out_path: String,
        format: &str,
        timeout: std::time::Duration,
        then: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        self.0
            .begin_headless_export(view, input, out_path, format, timeout, then)
    }

    pub fn fail_headless_export(&self, view: ViewId, error: String) -> bool {
        let waiter = self.0.headless_waiters.borrow_mut().remove(&view);
        if let Some((_, settle)) = waiter {
            settle(Err(error));
            true
        } else {
            false
        }
    }
}
