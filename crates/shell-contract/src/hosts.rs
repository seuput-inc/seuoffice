//! Hosts: what a handler reaches beyond [`crate::runtime::Services`].
//!
//! Two kinds, both carried in one typed map ([`Hosts`]):
//!
//! - shared hosts, defined here because several batches call them (dialogs,
//!   the OS shell, the clipboard, a view's web contents). Batch W implements
//!   them in the shell; ipc-probe inserts fakes.
//! - batch hosts, defined in the batch's own crate (M's menus and theme, U's
//!   updater, A's secrets, D2's engines that are not in Rust yet). The batch
//!   that owns the area implements them in its host file in `crates/shell`.
//!
//! The shell inserts every host before it registers the batches. A batch's
//! `register` takes what it needs with [`Hosts::need`], so a shell built
//! without a host refuses to start (`RegisterError::MissingHost`), the same
//! way it refuses an unregistered channel.

mod pdf_reload;
pub mod synthetic;
mod synthetic_recovery_dialog;
pub use pdf_reload::{
    AcceptedPdfReload, AdmittedSavedReload, CommittedGeneratedPath, CommittedPdfReload,
    GeneratedCleanupRegistration, GeneratedObserveFailure, GeneratedPathDispatchOutcome,
    GeneratedPathObserver, GeneratedPathOutcome, GeneratedPathPending, GeneratedPathWake,
    GeneratedStartFailure, GeneratedTerminalAck, GeneratedTerminalDelivery, NativeSaveReloadDone,
    NativeSaveReloadHandoff, NativeSaveReloadReceipt, PdfReload, PdfReloadAccepted, PdfReloadDone,
    PdfReloadHost, PreparedAcceptedPdfReload, PreparedGeneratedPath,
};

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::channels::Batch;
use crate::ids::{ViewId, WindowId};
use crate::ipc::RegisterError;
use crate::value::IpcValue;

/// A worker-owned save step executed on the original actor's core turn.
/// The actor supplies only its synchronous captured-state revision check.
pub type SaveCoreAction = Box<dyn FnOnce(&mut dyn FnMut() -> Result<(), String>) + Send>;
pub type SaveCoreDispatch = Arc<dyn Fn(SaveCoreAction) + Send + Sync>;

/// One synchronous step run inside a native admission.
pub type AdmitStep<'a> = &'a mut dyn FnMut() -> Result<(), String>;
/// A native admission wrapper that runs one synchronous step inside the admission.
pub type SaveAdmitRef<'a> = &'a mut dyn FnMut(AdmitStep<'_>) -> Result<(), String>;
/// Retains one request owner while running a synchronous step.
pub type RetainAdmitRef<'a> =
    &'a mut dyn FnMut(&crate::ipc::NativeRequestOwner, AdmitStep<'_>) -> Result<(), String>;
/// Completion callback of a session publication.
pub type SavedSessionDone = Box<dyn FnOnce(Result<(), String>)>;
/// Completion callback of a multi-path picker.
pub type PickedPathsDone = Box<dyn FnOnce(Result<Option<Vec<PathBuf>>, String>) + Send>;

/// A core-only post-save publication. Dropping it publishes nothing.
#[must_use]
pub enum SavedSessionReceipt {
    Pending(Box<dyn FnOnce(SavedSessionDone)>),
    /// The original operation has no session publication, including headless fixtures.
    NoPublication,
}

impl SavedSessionReceipt {
    pub fn finalize(self, done: Box<dyn FnOnce(Result<(), String>)>) {
        match self {
            Self::Pending(publish) => publish(done),
            Self::NoPublication => done(Ok(())),
        }
    }
}

/// The actual final path and the publication to consume after editor state commits.
#[must_use]
pub struct SavedFile<T> {
    pub path: T,
    pub session: SavedSessionReceipt,
}

impl<T> SavedFile<T> {
    pub fn map_path<U>(self, map: impl FnOnce(T) -> U) -> SavedFile<U> {
        SavedFile {
            path: map(self.path),
            session: self.session,
        }
    }
}
/// Hosts by type. Insert `Arc<dyn Trait>`; read it back by the same type.
#[derive(Default)]
pub struct Hosts(HashMap<TypeId, Box<dyn Any + Send + Sync>>);

impl Hosts {
    pub fn insert<T: Any + Send + Sync>(&mut self, host: T) {
        self.0.insert(TypeId::of::<T>(), Box::new(host));
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.0
            .get(&TypeId::of::<T>())
            .and_then(|h| h.downcast_ref())
    }

    /// The host batch `by` needs, or the error that stops the shell
    pub fn need<T: Any + Send + Sync + Clone>(&self, by: Batch) -> Result<T, RegisterError> {
        self.get::<T>().cloned().ok_or(RegisterError::MissingHost {
            batch: by,
            host: type_name::<T>(),
        })
    }
}

/// Electron's `dialog` filter
#[derive(Clone, Debug, PartialEq)]
pub struct FileFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

/// Electron's open and save dialog options, the fields the handlers pass
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileDialog {
    pub title: Option<String>,
    pub default_path: Option<PathBuf>,
    pub button_label: Option<String>,
    pub filters: Vec<FileFilter>,
    /// `properties` as Electron spells them (openFile, multiSelections, …)
    pub properties: Vec<String>,
}

/// Electron's message box options and answer
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageBox {
    pub kind: Option<String>,
    pub title: Option<String>,
    pub message: String,
    pub detail: Option<String>,
    pub buttons: Vec<String>,
    pub default_id: Option<usize>,
    pub cancel_id: Option<usize>,
    pub checkbox_label: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MessageBoxAnswer {
    pub response: usize,
    pub checkbox_checked: bool,
}

/// `dialog.*`. Runs on the UI thread through `run_on`; `parent` makes it
/// modal to that window as Electron's `BrowserWindow` argument did.
pub trait Dialogs: Send + Sync {
    /// `None` when cancelled
    fn open(&self, parent: Option<WindowId>, options: &FileDialog) -> Option<Vec<PathBuf>>;
    fn save(&self, parent: Option<WindowId>, options: &FileDialog) -> Option<PathBuf>;
    /// Recovery dialog bound to the original pre-registration reservation.
    fn message_prepared(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        target: crate::ipc::RecoveryOpenTarget,
        snapshot: crate::ipc::RecoveryAuthoritySnapshot,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    );
    fn save_prepared(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        target: crate::ipc::RecoveryOpenTarget,
        snapshot: crate::ipc::RecoveryAuthoritySnapshot,
        done: Box<
            dyn FnOnce(Result<Option<crate::ipc::RecoveryPreparedDestination>, String>) + Send,
        >,
    );
    #[allow(clippy::too_many_arguments)]
    fn message_recovery_owned(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        receipt: crate::ipc::RecoverySeedReceipt,
        snapshot: crate::ipc::RecoveryAuthoritySnapshot,
        receiver: crate::ipc::NativeRequestOwner,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn save_recovery_owned(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        receipt: crate::ipc::RecoverySeedReceipt,
        snapshot: crate::ipc::RecoveryAuthoritySnapshot,
        receiver: crate::ipc::NativeRequestOwner,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        done: Box<
            dyn FnOnce(Result<Option<crate::ipc::RecoveryCommittedDestination>, String>) + Send,
        >,
    );
    #[allow(clippy::too_many_arguments)]
    fn save_existing_document(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        binding: crate::ipc::NativeExistingDocumentBinding,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<Option<crate::ipc::NativeSaveDestination>, String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn message_existing_document(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        binding: crate::ipc::NativeExistingDocumentBinding,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn save_existing_document_revision(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        snapshot: crate::ipc::NativeSaveRevisionSnapshot,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<Option<crate::ipc::NativeSaveDestination>, String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn message_existing_document_revision(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        snapshot: crate::ipc::NativeSaveRevisionSnapshot,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn open_owned(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: crate::ipc::NativeRequestOwner,
        destination: crate::ipc::NativeRequestOwner,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        done: PickedPathsDone,
    );
    #[allow(clippy::too_many_arguments)]
    fn save_owned(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: crate::ipc::NativeRequestOwner,
        destination: crate::ipc::NativeRequestOwner,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<PathBuf>, String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn message_owned(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        origin: crate::ipc::NativeRequestOwner,
        destination: crate::ipc::NativeRequestOwner,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn save_owned_many(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: crate::ipc::NativeRequestOwner,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<PathBuf>, String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn message_owned_many(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        origin: crate::ipc::NativeRequestOwner,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn save_owned_many_extra(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: crate::ipc::NativeRequestOwner,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<Option<PathBuf>, String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn message_owned_many_extra(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        origin: crate::ipc::NativeRequestOwner,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    );
    fn message(&self, parent: Option<WindowId>, options: &MessageBox) -> MessageBoxAnswer;
    fn error(&self, title: &str, content: &str);
}

/// `shell.*`: hand a path or URL to the OS
pub trait OsShell: Send + Sync {
    fn open_external(&self, url: &str) -> Result<(), String>;
    /// `""` on success, else the OS's message, as Electron's `openPath`
    fn open_path(&self, path: &std::path::Path) -> String;
    fn show_item_in_folder(&self, path: &std::path::Path);
    fn trash_item(&self, path: &std::path::Path) -> Result<(), String>;
    /// Prepare outside admission; commit only through the captured owner.
    /// Native path operations may finish after their one-shot admission.
    fn trash_item_authorized(
        &self,
        path: &std::path::Path,
        admit: SaveAdmitRef<'_>,
    ) -> Result<(), String>;
}

/// `clipboard.*`, the formats the handlers use
pub trait Clipboard: Send + Sync {
    fn read_text(&self) -> String;
    fn write_text(&self, text: &str);
    fn read_html(&self) -> String;
    fn write_html(&self, html: &str, text: Option<&str>);
    /// PNG bytes, empty when there is no image
    fn read_image_png(&self) -> Vec<u8>;
    fn write_image_png(&self, png: &[u8]);
    fn available_formats(&self) -> Vec<String>;
}

/// A captured headless PDF print request, retained across chunk retries.
/// It is issued by the Docs actor from its existing native invocation waiter.
#[derive(Clone)]
pub struct NativeInvocationPrint {
    pub document: crate::ipc::NativeInvocationDocument,
    pub owner: crate::ipc::NativeRequestOwner,
    pub participants: Vec<crate::ipc::NativeRequestOwner>,
    pub cancel: crate::runtime::CancelToken,
    pub pending: crate::runtime::CancelToken,
}

/// Core-only Provided service. `None` means an ordinary interactive request
/// or an explicitly synthetic fixture, never a missing native headless owner.
pub trait DocsHeadlessPrint {
    fn capture(
        &self,
        context: &crate::ipc::CallContext,
    ) -> Result<Option<NativeInvocationPrint>, crate::IpcError>;
}

/// `webContents` of a view, the operations handlers ask of the sender or
/// of another view
pub trait Contents: Send + Sync {
    fn window_of(&self, view: ViewId) -> Option<WindowId>;
    /// PNG of the visible page (`capturePage`)
    fn capture_png(&self, view: ViewId) -> Result<Vec<u8>, String>;
    /// `printToPDF` with Electron's options as passed
    fn print_to_pdf(&self, view: ViewId, options: IpcValue) -> Result<Vec<u8>, String>;
    /// Worker-only PDF production bound to the captured native document, not a current view.
    fn print_to_pdf_owned(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destination: &crate::ipc::NativeRequestOwner,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        options: IpcValue,
    ) -> Result<Vec<u8>, String>;
    #[allow(clippy::too_many_arguments)]
    fn print_to_pdf_owned_many_extra(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destination: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[crate::runtime::CancelToken],
        options: IpcValue,
    ) -> Result<Vec<u8>, String>;
    fn print_to_pdf_existing_document(
        &self,
        binding: &crate::ipc::NativeExistingDocumentBinding,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[crate::runtime::CancelToken],
        options: IpcValue,
    ) -> Result<Vec<u8>, String>;
    /// Worker-only PDF print bound to the first native registration and every captured participant.
    fn print_to_pdf_native_invocation(
        &self,
        document: &crate::ipc::NativeInvocationDocument,
        owner: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        options: IpcValue,
    ) -> Result<Vec<u8>, String>;
    fn print(&self, view: ViewId, options: IpcValue) -> Result<bool, String>;
    fn zoom_factor(&self, view: ViewId) -> f64;
    fn set_zoom_factor(&self, view: ViewId, factor: f64);
    fn focus(&self, view: ViewId);
    fn close_window(&self, window: WindowId);
}

/// The shared hosts as they sit in [`Hosts`]
pub type DialogsHost = Arc<dyn Dialogs>;
pub type OsShellHost = Arc<dyn OsShell>;
pub type ClipboardHost = Arc<dyn Clipboard>;
pub type ContentsHost = Arc<dyn Contents>;

/// Exact staged Docs media transition, supplied by the document actor.
pub trait DocsMediaTransition: Send + Sync {
    fn lease(&self) -> &(dyn std::any::Any + Send + Sync);
    fn old(&self) -> &(dyn std::any::Any + Send + Sync);
    fn successor(&self) -> &(dyn std::any::Any + Send + Sync);
    fn old_identity(&self) -> (ViewId, u64);
    fn successor_identity(&self) -> (ViewId, u64);
    /// Test-only document lifetime and retained permit-loss subscription.
    fn synthetic_successor(
        &self,
    ) -> Result<
        (
            crate::runtime::RendererGeneration,
            Arc<dyn std::any::Any + Send + Sync>,
        ),
        String,
    >;
    /// Synthetic hosts admit the supplied one-shot commit inside their own gate.
    fn synthetic_transition(
        &self,
        admit: SaveAdmitRef<'_>,
        after_unlock: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
}

/// Exact captured pending state and a preallocated, not yet staged Docs permit.
pub trait DocsMediaStage: Send + Sync {
    fn lease(&self) -> &(dyn std::any::Any + Send + Sync);
    fn old(&self) -> &(dyn std::any::Any + Send + Sync);
    fn old_identity(&self) -> (ViewId, u64);
    fn prior(&self) -> Option<&(dyn std::any::Any + Send + Sync)>;
    fn candidate(&self) -> &(dyn std::any::Any + Send + Sync);
    fn synthetic_stage(&self, admit: SaveAdmitRef<'_>) -> Result<(), String>;
}

/// Captured document authority for short effects and accepted native UI actions.
pub trait OwnedEffects: Send + Sync {
    fn capture_owner(
        &self,
        view: ViewId,
        expected: &crate::runtime::RendererGeneration,
    ) -> Result<crate::ipc::NativeRequestOwner, String>;
    /// Admit a short effect only for the exact current native browser owner.
    fn with_exact_browser_owner_effect(
        &self,
        expected: &crate::ipc::NativeBrowserOwner,
        actual: &crate::ipc::NativeRequestOwner,
        cancel: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    /// Immutable captured Docs media identity; this does not admit an effect.
    fn docs_document_identity(
        &self,
        owner: &crate::ipc::NativeRequestOwner,
    ) -> Option<(ViewId, u64)>;
    /// Admit only the short enqueue of an already-adopted target's retained input.
    fn with_native_saved_target_seed(
        &self,
        target: &crate::ipc::NativeSavedTarget,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut(&crate::ipc::NativeSavedInput) -> Result<(), String>,
    ) -> Result<(), String>;
    /// Admit publication for the same registered document and retained input.
    fn with_native_saved_document_input(
        &self,
        document: &crate::ipc::NativeSavedDocument,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut(&crate::ipc::NativeSavedInput) -> Result<(), String>,
    ) -> Result<(), String>;
    /// Enqueue for a captured registered target after saved-input admission.
    #[allow(clippy::too_many_arguments)]
    fn emit_native_saved_document_input(
        &self,
        document: &crate::ipc::NativeSavedDocument,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        channel: &str,
        args: Vec<crate::IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn post_ui_native_saved_document_input(
        &self,
        document: crate::ipc::NativeSavedDocument,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn with_native_saved_document_same_origin(
        &self,
        document: &crate::ipc::NativeSavedDocument,
        actual: &crate::ipc::NativeRequestOwner,
        actual_cancel: &crate::runtime::CancelToken,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut(&crate::ipc::NativeSavedInput) -> Result<(), String>,
    ) -> Result<(), String>;
    /// Return after admission and enqueue, not renderer delivery.
    #[allow(clippy::too_many_arguments)]
    fn emit_native_saved_document_same_origin(
        &self,
        document: &crate::ipc::NativeSavedDocument,
        actual: &crate::ipc::NativeRequestOwner,
        actual_cancel: &crate::runtime::CancelToken,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        channel: &str,
        args: Vec<crate::IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_native_saved_document_transition(
        &self,
        document: &crate::ipc::NativeSavedDocument,
        actual: &crate::ipc::NativeRequestOwner,
        actual_cancel: &crate::runtime::CancelToken,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut(
            &crate::ipc::NativeSavedInput,
            &mut crate::runtime::AdmittedDocumentRetirement<'_>,
        ) -> Result<(), String>,
    ) -> Result<(), String>;
    fn stage_docs_media(
        &self,
        owner: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        stage: &dyn DocsMediaStage,
        recovery: Option<&crate::ipc::RecoverySeedReceipt>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn stage_saved_docs_media(
        &self,
        document: &crate::ipc::NativeSavedDocument,
        actual: &crate::ipc::NativeRequestOwner,
        actual_cancel: &crate::runtime::CancelToken,
        original: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        stage: &dyn DocsMediaStage,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn transition_saved_docs_media(
        &self,
        document: &crate::ipc::NativeSavedDocument,
        actual: &crate::ipc::NativeRequestOwner,
        actual_cancel: &crate::runtime::CancelToken,
        original: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        transition: &dyn DocsMediaTransition,
        retain: RetainAdmitRef<'_>,
        before_notify: &mut dyn FnMut(
            &crate::ipc::NativeSavedDocument,
            &crate::ipc::NativeRequestOwner,
        ) -> Result<(), String>,
    ) -> Result<
        (
            crate::ipc::NativeSavedDocument,
            crate::ipc::NativeRequestOwner,
        ),
        String,
    >;
    /// Retire the exact old Docs permit and return the admitted successor.
    /// `retain` prepares its core-local candidate before calling the one-shot
    /// commit, then publishes it infallibly before returning. It must not invoke
    /// callbacks, perform I/O, or reenter any media or authority gate.
    #[allow(clippy::too_many_arguments)]
    fn transition_docs_media(
        &self,
        owner: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        transition: &dyn DocsMediaTransition,
        recovery: Option<&crate::ipc::RecoverySeedReceipt>,
        retain: RetainAdmitRef<'_>,
    ) -> Result<crate::ipc::NativeRequestOwner, String>;
    fn with_native_command_preparation_effect(
        &self,
        operation: &crate::ipc::NativeInvocationOperation,
        expected: &crate::ipc::NativeEditorCommand,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_native_command_creation_effect(
        &self,
        operation: &crate::ipc::NativeInvocationOperation,
        expected: &crate::ipc::NativeEditorCommand,
        created: &crate::runtime::RendererGeneration,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_native_command_target_effect(
        &self,
        operation: &crate::ipc::NativeInvocationOperation,
        expected: &crate::ipc::NativeEditorCommand,
        document: &crate::ipc::NativeInvocationDocument,
        owner: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_startup_command_preparation_effect(
        &self,
        startup: &crate::ipc::NativeInvocationOperation,
        shell: &crate::ipc::NativeRequestOwner,
        child: &crate::ipc::NativeInvocationOperation,
        expected: &crate::ipc::NativeEditorCommand,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_startup_command_creation_effect(
        &self,
        startup: &crate::ipc::NativeInvocationOperation,
        shell: &crate::ipc::NativeRequestOwner,
        child: &crate::ipc::NativeInvocationOperation,
        expected: &crate::ipc::NativeEditorCommand,
        created: &crate::runtime::RendererGeneration,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_startup_command_target_effect(
        &self,
        startup: &crate::ipc::NativeInvocationOperation,
        shell: &crate::ipc::NativeRequestOwner,
        child: &crate::ipc::NativeInvocationOperation,
        expected: &crate::ipc::NativeEditorCommand,
        document: &crate::ipc::NativeInvocationDocument,
        owner: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_created_effect(
        &self,
        source: &crate::ipc::NativeRequestOwner,
        created: &crate::runtime::RendererGeneration,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_created_many_effect(
        &self,
        source: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        created: &crate::runtime::RendererGeneration,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_owned_effect(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destination: &crate::ipc::NativeRequestOwner,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    /// Both owners must retain the same authenticated native context and renderer.
    #[allow(clippy::too_many_arguments)]
    fn with_same_native_origin_effect(
        &self,
        expected: &crate::ipc::NativeRequestOwner,
        actual: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional_cancellation: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_owned_effect_many(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destinations: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_owned_effect_many_extra(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destinations: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_native_invocation_effect(
        &self,
        document: &crate::ipc::NativeInvocationDocument,
        origin: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_native_invocation_effect_extra(
        &self,
        document: &crate::ipc::NativeInvocationDocument,
        origin: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_native_operation_effect(
        &self,
        operation: &crate::ipc::NativeInvocationOperation,
        origin: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    /// Admit a short state effect for the exact existing editor captured by the
    /// trusted native save selector. Run external work only after admission.
    fn with_existing_document_save_effect(
        &self,
        binding: &crate::ipc::NativeExistingDocumentBinding,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_existing_document_save_effect_extra(
        &self,
        binding: &crate::ipc::NativeExistingDocumentBinding,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    /// Prepare the full publication plan before an outer service takes its locks.
    fn with_prepared_saved_publication(
        &self,
        publication: &crate::ipc::SavedPublication,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        use_plan: &mut dyn FnMut(SaveAdmitRef<'_>) -> Result<(), String>,
    ) -> Result<(), String>;
    /// Open only the captured native destination inside its original admission.
    fn begin_native_direct_write(
        &self,
        selection: &crate::ipc::NativeSaveDestination,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[crate::runtime::CancelToken],
        prepared: file_commit::PreparedDirectWrite,
    ) -> Result<crate::ipc::AdmittedDirectWrite, crate::ipc::DirectWriteStartError> {
        let scope = crate::ipc::DirectWriteScope {
            origin: crate::ipc::DirectWriteOrigin::Native(selection.clone()),
            path: selection.path().to_owned(),
            participants: participants.to_vec(),
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
        };
        crate::ipc::AdmittedDirectWrite::begin(self, scope, prepared)
    }
    /// Check the actor revision inside the admission before a truncating open.
    #[allow(clippy::too_many_arguments)]
    fn begin_native_direct_write_validated(
        &self,
        selection: &crate::ipc::NativeSaveDestination,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[crate::runtime::CancelToken],
        prepared: file_commit::PreparedDirectWrite,
        validate: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<crate::ipc::AdmittedDirectWrite, crate::ipc::DirectWriteStartError> {
        let scope = crate::ipc::DirectWriteScope {
            origin: crate::ipc::DirectWriteOrigin::Native(selection.clone()),
            path: selection.path().to_owned(),
            participants: participants.to_vec(),
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
        };
        crate::ipc::AdmittedDirectWrite::begin_validated(self, scope, prepared, validate)
    }
    /// Open a renderer-owned destination under the original captured owners.
    #[allow(clippy::too_many_arguments)]
    fn begin_owned_direct_write(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destination: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[crate::runtime::CancelToken],
        path: &std::path::Path,
        prepared: file_commit::PreparedDirectWrite,
    ) -> Result<crate::ipc::AdmittedDirectWrite, crate::ipc::DirectWriteStartError> {
        let scope = crate::ipc::DirectWriteScope {
            origin: crate::ipc::DirectWriteOrigin::Renderer {
                origin: origin.clone(),
                destination: destination.clone(),
            },
            path: path.to_owned(),
            participants: participants.to_vec(),
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
        };
        crate::ipc::AdmittedDirectWrite::begin(self, scope, prepared)
    }
    /// Publish a prepared native output under its original selection and controls.
    fn publish_native_staged_write(
        &self,
        selection: &crate::ipc::NativeSaveDestination,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[crate::runtime::CancelToken],
        stage: file_commit::OwnedWrittenCommit,
    ) -> Result<
        (
            crate::ipc::SavedPublication,
            file_commit::CommittedWriteSnapshot,
        ),
        crate::ipc::StagedWriteError,
    > {
        let scope = crate::ipc::DirectWriteScope {
            origin: crate::ipc::DirectWriteOrigin::Native(selection.clone()),
            path: selection.path().to_owned(),
            participants: participants.to_vec(),
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
        };
        crate::ipc::publish_staged(self, scope, stage)
    }
    /// Check actor state on the current core turn inside the publication
    /// admission. The validator receives neither the stage nor its receipt.
    #[allow(clippy::too_many_arguments)]
    fn publish_native_staged_write_validated(
        &self,
        selection: &crate::ipc::NativeSaveDestination,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[crate::runtime::CancelToken],
        stage: file_commit::OwnedWrittenCommit,
        validate: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<
        (
            crate::ipc::SavedPublication,
            file_commit::CommittedWriteSnapshot,
        ),
        crate::ipc::StagedWriteError,
    > {
        let scope = crate::ipc::DirectWriteScope {
            origin: crate::ipc::DirectWriteOrigin::Native(selection.clone()),
            path: selection.path().to_owned(),
            participants: participants.to_vec(),
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
        };
        crate::ipc::publish_staged_validated(self, scope, stage, validate)
    }
    /// Publish a prepared renderer output under the original authenticated owners.
    #[allow(clippy::too_many_arguments)]
    fn publish_owned_staged_write(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destination: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[crate::runtime::CancelToken],
        path: &std::path::Path,
        stage: file_commit::OwnedWrittenCommit,
    ) -> Result<
        (
            crate::ipc::SavedPublication,
            file_commit::CommittedWriteSnapshot,
        ),
        crate::ipc::StagedWriteError,
    > {
        let scope = crate::ipc::DirectWriteScope {
            origin: crate::ipc::DirectWriteOrigin::Renderer {
                origin: origin.clone(),
                destination: destination.clone(),
            },
            path: path.to_owned(),
            participants: participants.to_vec(),
            cancel: cancel.clone(),
            pending: pending.clone(),
            additional: additional.to_vec(),
        };
        crate::ipc::publish_staged(self, scope, stage)
    }
    /// Admit the actual authenticated callback only if it is the same native
    /// origin and renderer as the existing-document binding.
    #[allow(clippy::too_many_arguments)]
    fn with_existing_document_same_origin_effect(
        &self,
        binding: &crate::ipc::NativeExistingDocumentBinding,
        actual: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    /// The immutable selected path is compared before the same binding's
    /// canonical admission; no path can be substituted by the callback.
    #[allow(clippy::too_many_arguments)]
    fn with_existing_document_destination_effect(
        &self,
        destination: &crate::ipc::NativeSaveDestination,
        path: &std::path::Path,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if destination.path() != path {
            return Err("The selected save destination changed.".into());
        }
        self.with_existing_document_save_effect_extra(
            destination.binding(),
            participants,
            cancel,
            pending,
            additional,
            effect,
        )
    }
    /// Admit only the exact numbered page derived from its retained selection.
    #[allow(clippy::too_many_arguments)]
    fn with_numbered_save_destination_effect(
        &self,
        page: &crate::ipc::NativeSaveNumberedDestination,
        path: &std::path::Path,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if page.path() != path {
            return Err("The numbered save destination changed.".into());
        }
        self.with_existing_document_save_effect_extra(
            page.selection().binding(),
            participants,
            cancel,
            pending,
            additional,
            effect,
        )
    }
    /// Admit the exact retained input and Drive state before a native reload starts.
    #[allow(clippy::too_many_arguments)]
    fn with_saved_reload_acceptance(
        &self,
        input: &crate::ipc::NativeSavedInput,
        path: &crate::ipc::NativeSavedPath,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut(&mut AdmittedSavedReload<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut attempted = false;
        self.with_saved_reload_acceptance_effect(
            input,
            path,
            participants,
            cancel,
            pending,
            additional,
            &mut || {
                if attempted {
                    return Err("The PDF reload acceptance was already attempted".into());
                }
                attempted = true;
                let mut permit = AdmittedSavedReload::new(input, path);
                effect(&mut permit)
            },
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn with_saved_reload_acceptance_effect(
        &self,
        input: &crate::ipc::NativeSavedInput,
        path: &crate::ipc::NativeSavedPath,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    /// Admit only the short actor completion for the actual loaded owner of this
    /// accepted reservation; callbacks and native I/O remain outside the gate.
    fn with_native_save_reload_completion_effect(
        &self,
        handoff: &NativeSaveReloadHandoff,
        loaded: &crate::ipc::NativeRequestOwner,
        before_notify: &mut dyn FnMut(),
        effect: &mut dyn FnMut(
            &mut crate::ipc::AdmittedPdfReloadRetirement<'_, '_>,
        ) -> Result<(), String>,
    ) -> Result<(), String>;
    /// Admit only parser input and acknowledgement from the exact loaded successor.
    fn with_native_save_reload_parser_effect(
        &self,
        handoff: &NativeSaveReloadHandoff,
        actual: &crate::ipc::NativeRequestOwner,
        actual_cancel: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut(&crate::ipc::NativeSavedInput) -> Result<(), String>,
    ) -> Result<(), String>;
    /// After parser acknowledgement, read retained bytes under the successor actor alone.
    fn with_native_save_reload_reader_effect(
        &self,
        receipt: &NativeSaveReloadReceipt,
        actual: &crate::ipc::NativeRequestOwner,
        actual_cancel: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    /// Admit a completed native reload's exact successor without re-admitting the
    /// intentionally retired predecessor document.
    fn with_native_save_reload_effect(
        &self,
        receipt: &NativeSaveReloadReceipt,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_native_save_reload_many_effect(
        &self,
        receipt: &NativeSaveReloadReceipt,
        owner: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.with_native_save_reload_many_effect_extra(
            receipt,
            owner,
            participants,
            cancel,
            pending,
            &[],
            effect,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn with_native_save_reload_many_effect_extra(
        &self,
        receipt: &NativeSaveReloadReceipt,
        owner: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_startup_recovery_creation_effect(
        &self,
        startup: &crate::ipc::NativeInvocationOperation,
        shell: &crate::ipc::NativeRequestOwner,
        child: &crate::ipc::NativeInvocationOperation,
        created: &crate::runtime::RendererGeneration,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_startup_recovery_seed_effect(
        &self,
        startup: &crate::ipc::NativeInvocationOperation,
        shell: &crate::ipc::NativeRequestOwner,
        target: &crate::ipc::NativeInvocationTarget,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_startup_recovery_effect(
        &self,
        startup: &crate::ipc::NativeInvocationOperation,
        shell: &crate::ipc::NativeRequestOwner,
        child: &crate::ipc::NativeInvocationDocument,
        owner: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_command_recovery_target_effect(
        &self,
        command: &crate::ipc::NativeInvocationOperation,
        child: &crate::ipc::NativeInvocationDocument,
        owner: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    /// The selected recovery's initial read/adopt/mark only, before navigation.
    /// This does not authorize renderer services, CDP, or arbitrary engine calls.
    fn with_recovery_seed_effect(
        &self,
        target: &crate::ipc::RecoveryOpenTarget,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_recovery_seed_snapshot(
        &self,
        target: &crate::ipc::RecoveryOpenTarget,
        snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn commit_recovery_seed(
        &self,
        target: &crate::ipc::RecoveryOpenTarget,
        enqueue: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<crate::ipc::RecoverySeedReceipt, String>;
    fn commit_recovery_seed_snapshot(
        &self,
        target: &crate::ipc::RecoveryOpenTarget,
        snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        enqueue: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<crate::ipc::RecoverySeedReceipt, String>;
    fn with_recovery_open_effect(
        &self,
        receipt: &crate::ipc::RecoverySeedReceipt,
        receiver: &crate::ipc::NativeRequestOwner,
        cancel: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_recovery_open_snapshot(
        &self,
        receipt: &crate::ipc::RecoverySeedReceipt,
        snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        receiver: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_recovery_open_effect_pending(
        &self,
        receipt: &crate::ipc::RecoverySeedReceipt,
        receiver: &crate::ipc::NativeRequestOwner,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    fn with_recovery_open_effect_many(
        &self,
        receipt: &crate::ipc::RecoverySeedReceipt,
        receiver: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_recovery_open_effect_many_extra(
        &self,
        receipt: &crate::ipc::RecoverySeedReceipt,
        receiver: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_recovery_pdf_revision_transition(
        &self,
        receipt: &crate::ipc::RecoverySeedReceipt,
        receiver: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        old: &crate::ipc::PdfDocumentLease,
        successor: &crate::ipc::PdfDocumentLease,
        renewal: &mut crate::ipc::PreparedRecoveryAuthorityRenewal,
        before_notify: &mut dyn FnMut(),
        effect: &mut dyn FnMut(
            &mut crate::ipc::AdmittedPdfRevisionRetirement<'_, '_>,
            &mut crate::ipc::PreparedRecoveryAuthorityRenewal,
        ) -> Result<(), String>,
    ) -> Result<(), String>;
    /// Atomically retire the exact multi-view PDF revisions and replace the
    /// retained native save authority. Cleanup precedes loss notifications.
    #[allow(clippy::too_many_arguments)]
    fn with_native_save_revision_transition(
        &self,
        prepared: &mut crate::ipc::PreparedNativeSaveRevision,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        before_notify: &mut dyn FnMut(),
        effect: &mut dyn FnMut(
            &mut crate::ipc::AdmittedPdfRevisionSet<'_, '_>,
            &mut crate::ipc::PreparedNativeSaveRevision,
        ) -> Result<(), String>,
    ) -> Result<(), String>;
    /// Capture aliases and admit the exact current epoch in the same plan.
    /// The effect must be a short state mutation, never an external callback.
    fn with_native_save_revision_snapshot_effect(
        &self,
        snapshot: &crate::ipc::NativeSaveRevisionSnapshot,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut owners = snapshot.aliases().to_vec();
        owners.extend_from_slice(participants);
        self.with_existing_document_save_effect_extra(
            snapshot.selection().binding(),
            &owners,
            cancel,
            pending,
            additional,
            &mut || {
                snapshot
                    .with_current(&mut *effect)
                    .ok_or_else(|| "The native save snapshot was replaced.".to_owned())?
            },
        )
    }
    /// Admit the actual callback and the exact save epoch in one owner plan.
    #[allow(clippy::too_many_arguments)]
    fn with_native_save_revision_same_origin_effect(
        &self,
        snapshot: &crate::ipc::NativeSaveRevisionSnapshot,
        actual: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut owners = snapshot.aliases().to_vec();
        owners.extend_from_slice(participants);
        self.with_existing_document_same_origin_effect(
            snapshot.selection().binding(),
            actual,
            &owners,
            cancel,
            pending,
            additional,
            &mut || {
                snapshot
                    .with_current(&mut *effect)
                    .ok_or_else(|| "The native save snapshot was replaced.".to_owned())?
            },
        )
    }
    /// Atomically replace a selected destination under its original binding.
    #[allow(clippy::too_many_arguments)]
    fn with_native_save_selection_transition(
        &self,
        prepared: &mut crate::ipc::PreparedNativeSaveSelection,
        participants: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        before_notify: &mut dyn FnMut(),
        effect: &mut dyn FnMut(
            &crate::ipc::AdmittedNativeSaveSelection,
            &mut crate::ipc::PreparedNativeSaveSelection,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        let snapshot = prepared.snapshot().clone();
        let mut owners = snapshot.aliases().to_vec();
        owners.extend_from_slice(participants);
        let permit = prepared.admit();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.with_existing_document_save_effect_extra(
                snapshot.selection().binding(),
                &owners,
                cancel,
                pending,
                additional,
                &mut || {
                    effect(&permit, prepared)?;
                    if !prepared.is_committed() {
                        return Err("The native save selection was not committed.".into());
                    }
                    Ok(())
                },
            )
        }));
        let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(before_notify));
        let notification =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepared.notify_replaced()));
        match (result, cleanup, notification) {
            (Err(panic), _, _) | (Ok(_), Err(panic), _) | (Ok(_), Ok(()), Err(panic)) => {
                std::panic::resume_unwind(panic)
            }
            (Ok(result), Ok(()), Ok(())) => result,
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn emit_native_save_revision_snapshot(
        &self,
        snapshot: &crate::ipc::NativeSaveRevisionSnapshot,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn post_ui_native_save_revision_snapshot(
        &self,
        snapshot: crate::ipc::NativeSaveRevisionSnapshot,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn emit_recovery_open_many(
        &self,
        receipt: &crate::ipc::RecoverySeedReceipt,
        receiver: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<crate::IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn emit_recovery_open_snapshot(
        &self,
        receipt: &crate::ipc::RecoverySeedReceipt,
        snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        receiver: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    fn settle_recovery_seed(
        &self,
        receipt: &crate::ipc::RecoverySeedReceipt,
        receiver: &crate::ipc::NativeRequestOwner,
        cancel: &crate::runtime::CancelToken,
        result: Result<(), String>,
    ) -> Result<(), String>;
    fn settle_recovery_seed_snapshot(
        &self,
        receipt: &crate::ipc::RecoverySeedReceipt,
        snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        receiver: &crate::ipc::NativeRequestOwner,
        cancel: &crate::runtime::CancelToken,
        result: Result<(), String>,
    ) -> Result<(), String>;
    /// The protected password dialog's captured renderer, before recovery navigation.
    fn with_recovery_dialog_effect(
        &self,
        target: &crate::ipc::RecoveryOpenTarget,
        dialog: &crate::ipc::NativeRequestOwner,
        dialog_generation: &crate::runtime::RendererGeneration,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn with_recovery_dialog_snapshot(
        &self,
        target: &crate::ipc::RecoveryOpenTarget,
        snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        dialog: &crate::ipc::NativeRequestOwner,
        dialog_generation: &crate::runtime::RendererGeneration,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn emit_recovery_dialog(
        &self,
        target: &crate::ipc::RecoveryOpenTarget,
        dialog: &crate::ipc::NativeRequestOwner,
        dialog_generation: &crate::runtime::RendererGeneration,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn emit_recovery_dialog_snapshot(
        &self,
        target: &crate::ipc::RecoveryOpenTarget,
        snapshot: &crate::ipc::RecoveryAuthoritySnapshot,
        dialog: &crate::ipc::NativeRequestOwner,
        dialog_generation: &crate::runtime::RendererGeneration,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    /// Atomically replace document state and retire the old captured document.
    /// Move retired state out of the callback; drop it after this method returns.
    fn with_owned_transition(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destination: &crate::ipc::NativeRequestOwner,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut(
            &mut crate::runtime::AdmittedDocumentRetirement<'_>,
        ) -> Result<(), String>,
    ) -> Result<(), String>;
    /// One atomic transition over every captured participant, with no view-count cap.
    fn with_owned_transition_many(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destinations: &[crate::ipc::NativeRequestOwner],
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        effect: &mut dyn FnMut(
            &mut crate::runtime::AdmittedDocumentRetirement<'_>,
        ) -> Result<(), String>,
    ) -> Result<(), String>;
    fn emit_owned(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destination: &crate::ipc::NativeRequestOwner,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    /// Enqueue an immutable event to the indexed captured participant.
    #[allow(clippy::too_many_arguments)]
    fn emit_owned_many(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destinations: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn emit_owned_many_extra(
        &self,
        origin: &crate::ipc::NativeRequestOwner,
        destinations: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn emit_native_command_creation_many(
        &self,
        operation: &crate::ipc::NativeInvocationOperation,
        expected: &crate::ipc::NativeEditorCommand,
        created: &crate::runtime::RendererGeneration,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn emit_native_invocation_many(
        &self,
        document: &crate::ipc::NativeInvocationDocument,
        origin: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn emit_native_operation_many(
        &self,
        operation: &crate::ipc::NativeInvocationOperation,
        origin: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    /// Enqueue to an exact captured participant; draining happens outside gates.
    #[allow(clippy::too_many_arguments)]
    fn emit_existing_document_save_many(
        &self,
        binding: &crate::ipc::NativeExistingDocumentBinding,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn emit_existing_document_save_many_extra(
        &self,
        binding: &crate::ipc::NativeExistingDocumentBinding,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        additional: &[&crate::runtime::CancelToken],
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn emit_startup_recovery_many(
        &self,
        startup: &crate::ipc::NativeInvocationOperation,
        shell: &crate::ipc::NativeRequestOwner,
        child: &crate::ipc::NativeInvocationDocument,
        owner: &crate::ipc::NativeRequestOwner,
        participants: &[crate::ipc::NativeRequestOwner],
        target: usize,
        cancel: &crate::runtime::CancelToken,
        pending: &crate::runtime::CancelToken,
        channel: &str,
        args: Vec<IpcValue>,
    ) -> Result<(), String>;
    #[allow(clippy::too_many_arguments)]
    fn post_ui_recovery_open_many(
        &self,
        receipt: crate::ipc::RecoverySeedReceipt,
        receiver: crate::ipc::NativeRequestOwner,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    fn post_ui_existing_document_save(
        &self,
        binding: crate::ipc::NativeExistingDocumentBinding,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn post_ui_existing_document_save_extra(
        &self,
        binding: crate::ipc::NativeExistingDocumentBinding,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    /// Accept the exact selected path, then execute the native action outside all gates.
    #[allow(clippy::too_many_arguments)]
    fn post_ui_existing_document_destination(
        &self,
        destination: crate::ipc::NativeSaveDestination,
        participants: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        action: Box<dyn FnOnce(std::path::PathBuf) -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let path = destination.path().to_owned();
        let binding = destination.binding().clone();
        self.post_ui_existing_document_save_extra(
            binding,
            participants,
            cancel,
            pending,
            additional,
            Box::new(move || action(path)),
            done,
        );
    }
    fn post_ui_owned_many(
        &self,
        origin: crate::ipc::NativeRequestOwner,
        destinations: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    #[allow(clippy::too_many_arguments)]
    fn post_ui_owned_many_extra(
        &self,
        origin: crate::ipc::NativeRequestOwner,
        destinations: Vec<crate::ipc::NativeRequestOwner>,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        additional: Vec<crate::runtime::CancelToken>,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
    /// Called on the UI task. The action starts immediately after all admission gates drop.
    fn post_ui_owned(
        &self,
        origin: crate::ipc::NativeRequestOwner,
        destination: crate::ipc::NativeRequestOwner,
        cancel: crate::runtime::CancelToken,
        pending: crate::runtime::CancelToken,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    );
}

pub type OwnedEffectsHost = Arc<dyn OwnedEffects>;

/// The shell window (Electron's `shellWindow`): the parent of app-wide
/// dialogs and the only sender some channels accept
pub trait MainWindow: Send + Sync {
    fn view(&self) -> Option<ViewId>;
    fn window(&self) -> Option<WindowId>;
}

pub type MainWindowHost = Arc<dyn MainWindow>;
