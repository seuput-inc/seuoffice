//! A shell without Tauri for this crate's tests: the registry checks every
//! registration as the shell's start-up check does. The core queue is pumped
//! until asynchronous work replies, events are recorded, and userData is fresh.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use serde_json::{Value, json};
use shell_contract::channels::{Batch, CHANNELS, Surface};
use shell_contract::editors::{DocumentEditors, SaveMode, Then};
use shell_contract::hosts::{
    Contents, ContentsHost, Dialogs, DialogsHost, FileDialog, MainWindow, MainWindowHost,
    MessageBox, MessageBoxAnswer, OsShell, OsShellHost,
};
use shell_contract::ipc::{
    CallContext, ChannelKind, EventSink, Handler, RegisterError, Registry, Reply, Responder,
    Sender, Target,
};
use shell_contract::provided::Provided;
use shell_contract::runtime::{AppPaths, CancelToken, PathName, Services, Thread, Views, Workers};
use shell_contract::{Hosts, IpcValue, RequestId, ViewId, WindowId};

use crate::probe::{self, ProbeLog};

/// The shared hosts (batch W's in the shell), faked from the same config:
/// `savePath` answers a save dialog (null cancels), `contentsPrint`
/// `{ "ok": bool, "error": string }` and `pdf` the sender's own print calls,
/// `mainView` the shell window's view
struct Shared {
    config: Value,
    log: ProbeLog,
}

impl Shared {
    fn log(&self, line: Value) {
        self.log.lock().unwrap().push(line)
    }
}

fn synthetic_owned_dialog_admit(
    origin: &shell_contract::ipc::NativeRequestOwner,
    destination: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
    pending: &shell_contract::runtime::CancelToken,
) -> Result<(), String> {
    let source = origin
        .context
        .renderer()
        .ok_or("synthetic dialog source is unbound")?;
    let target = destination
        .context
        .renderer()
        .ok_or("synthetic dialog destination is unbound")?;
    let source_loss = origin.loss_token();
    let target_loss = destination.loss_token();
    source
        .with_active_pair(target, || {
            shell_contract::runtime::CancelToken::with_active_all(
                [
                    Some(cancel),
                    Some(pending),
                    source_loss.as_ref(),
                    target_loss.as_ref(),
                ],
                || (),
            )
        })
        .flatten()
        .ok_or_else(|| "Renderer document is no longer available.".into())
}

impl Dialogs for Shared {
    shell_contract::synthetic_recovery_dialog_methods!();
    fn message_prepared(
        &self,
        parent: Option<shell_contract::WindowId>,
        options: shell_contract::hosts::MessageBox,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::MessageBoxAnswer, String>) + Send>,
    ) {
        done(shell_contract::hosts::synthetic::recovery_dialog(
            &target,
            &snapshot,
            || self.message(parent, &options),
        ));
    }
    fn save_prepared(
        &self,
        parent: Option<shell_contract::WindowId>,
        options: shell_contract::hosts::FileDialog,
        target: shell_contract::ipc::RecoveryOpenTarget,
        snapshot: shell_contract::ipc::RecoveryAuthoritySnapshot,
        done: Box<
            dyn FnOnce(Result<Option<shell_contract::ipc::RecoveryPreparedDestination>, String>)
                + Send,
        >,
    ) {
        done(shell_contract::hosts::synthetic::recovery_save(
            &target,
            &snapshot,
            || self.save(parent, &options),
        ));
    }
    fn open_owned(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<Vec<std::path::PathBuf>>, String>) + Send>,
    ) {
        done(
            synthetic_owned_dialog_admit(&origin, &destination, &cancel, &pending)
                .map(|()| self.open(parent, &options)),
        );
    }
    fn save_owned(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<std::path::PathBuf>, String>) + Send>,
    ) {
        done(
            synthetic_owned_dialog_admit(&origin, &destination, &cancel, &pending)
                .map(|()| self.save(parent, &options)),
        );
    }
    fn message_owned(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        origin: shell_contract::ipc::NativeRequestOwner,
        destination: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        done(
            synthetic_owned_dialog_admit(&origin, &destination, &cancel, &pending)
                .map(|()| self.message(parent, &options)),
        );
    }
    fn save_owned_many(
        &self,
        parent: Option<WindowId>,
        options: FileDialog,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<Option<std::path::PathBuf>, String>) + Send>,
    ) {
        done(shell_contract::hosts::synthetic::admit_many(
            &origin,
            &participants,
            &cancel,
            &pending,
            || self.save(parent, &options),
        ));
    }
    fn message_owned_many(
        &self,
        parent: Option<WindowId>,
        options: MessageBox,
        origin: shell_contract::ipc::NativeRequestOwner,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<MessageBoxAnswer, String>) + Send>,
    ) {
        done(shell_contract::hosts::synthetic::admit_many(
            &origin,
            &participants,
            &cancel,
            &pending,
            || self.message(parent, &options),
        ));
    }
    fn open(&self, _parent: Option<WindowId>, _options: &FileDialog) -> Option<Vec<PathBuf>> {
        None
    }

    fn save(&self, _parent: Option<WindowId>, options: &FileDialog) -> Option<PathBuf> {
        let default_path = options
            .default_path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.log(json!(["saveDialog", default_path]));
        self.config
            .get("savePath")
            .and_then(Value::as_str)
            .map(PathBuf::from)
    }

    fn message(&self, _parent: Option<WindowId>, options: &MessageBox) -> MessageBoxAnswer {
        self.log(json!(["messageBox", options.message]));
        MessageBoxAnswer {
            response: 0,
            checkbox_checked: false,
        }
    }

    fn error(&self, title: &str, content: &str) {
        self.log(json!(["errorBox", title, content]))
    }
}

impl OsShell for Shared {
    fn open_external(&self, url: &str) -> Result<(), String> {
        self.log(json!(["openExternal", url]));
        Ok(())
    }

    fn open_path(&self, path: &Path) -> String {
        self.log(json!(["openPath", path.display().to_string()]));
        self.config
            .get("openPath")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    fn show_item_in_folder(&self, path: &Path) {
        self.log(json!(["showItemInFolder", path.display().to_string()]))
    }

    fn trash_item(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
    fn trash_item_authorized(
        &self,
        path: &std::path::Path,
        admit: &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
    ) -> Result<(), String> {
        admit(&mut || self.trash_item(path))
    }
}

impl Contents for Shared {
    fn window_of(&self, view: ViewId) -> Option<WindowId> {
        Some(WindowId(view.0))
    }

    fn capture_png(&self, _view: ViewId) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }

    fn print_to_pdf_owned(
        &self,
        _: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        if cancel.is_cancelled() || pending.is_cancelled() {
            return Err("Print request expired.".into());
        }
        self.print_to_pdf(destination.view, options)
    }
    fn print_to_pdf_existing_document(
        &self,
        binding: &shell_contract::ipc::NativeExistingDocumentBinding,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let extra = additional.iter().collect::<Vec<_>>();
        let mut admitted = false;
        shell_contract::hosts::synthetic::with_existing_document_save_extra(
            binding,
            participants,
            cancel,
            pending,
            &extra,
            &mut || {
                admitted = true;
                Ok(())
            },
        )?;
        if !admitted {
            return Err("Print request expired.".into());
        }
        self.print_to_pdf(binding.owner().view, options)
    }
    fn print_to_pdf_owned_many_extra(
        &self,
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let mut owners = participants.to_vec();
        owners.push(destination.clone());
        let extra = additional.iter().collect::<Vec<_>>();
        shell_contract::hosts::synthetic::with_owned_effect_many_extra(
            origin,
            &owners,
            cancel,
            pending,
            &extra,
            &mut || Ok(()),
        )?;
        self.print_to_pdf(destination.view, options)
    }
    fn print_to_pdf_native_invocation(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        owner: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        options: IpcValue,
    ) -> Result<Vec<u8>, String> {
        let mut options = Some(options);
        let mut admitted = None;
        shell_contract::hosts::synthetic::with_native_invocation(
            document,
            owner,
            participants,
            cancel,
            pending,
            &mut || {
                admitted = options.take();
                Ok(())
            },
        )?;
        self.print_to_pdf(owner.view, admitted.ok_or("Print request expired.")?)
    }
    fn print_to_pdf(&self, view: ViewId, options: IpcValue) -> Result<Vec<u8>, String> {
        self.log(json!([
            "contentsPrintToPDF",
            view.0,
            format!("{options:?}")
        ]));
        match self.config.get("pdfError").and_then(Value::as_str) {
            Some(error) => Err(error.to_owned()),
            None => Ok(b"%PDF-fake".to_vec()),
        }
    }

    fn print(&self, view: ViewId, options: IpcValue) -> Result<bool, String> {
        self.log(json!(["contentsPrint", view.0, format!("{options:?}")]));
        let print = self.config.get("print");
        let success = print
            .and_then(|p| p.get("success"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let reason = print
            .and_then(|p| p.get("failureReason"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match (success, reason) {
            (true, _) => Ok(true),
            (false, "") => Ok(false),
            (false, reason) => Err(reason.to_owned()),
        }
    }

    fn zoom_factor(&self, _view: ViewId) -> f64 {
        1.0
    }

    fn set_zoom_factor(&self, _view: ViewId, _factor: f64) {}

    fn focus(&self, view: ViewId) {
        self.log(json!(["focus", view.0]))
    }

    fn close_window(&self, _window: WindowId) {}
}

impl MainWindow for Shared {
    fn view(&self) -> Option<ViewId> {
        self.config
            .get("mainView")
            .and_then(Value::as_u64)
            .map(|v| ViewId(v as u32))
    }

    fn window(&self) -> Option<WindowId> {
        self.view().map(|v| WindowId(v.0))
    }
}

/// The document batches' editors as the tests need them, from the config:
/// `sheetsViews` (views with a workbook), `htmlPaths` and `hwpPaths` (a
/// view's document file), `hwpGrants` (a view's Save As PDF path, taken
/// once) and `tornDown` (docs views whose writer is gone)
struct FakeEditors(
    Rc<Value>,
    Rc<std::cell::RefCell<Vec<u32>>>,
    Surface,
    shell_contract::hosts::synthetic::SyntheticSavedActors,
    shell_contract::hosts::OwnedEffectsHost,
);

impl FakeEditors {
    fn by_view(&self, key: &str, view: ViewId) -> Option<&Value> {
        self.0.get(key)?.get(view.0.to_string())
    }

    fn listed(&self, key: &str, view: ViewId) -> bool {
        self.0
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|v| v.as_u64() == Some(u64::from(view.0))))
    }
}

impl DocumentEditors for FakeEditors {
    fn saved_surface(&self) -> Surface {
        self.2
    }
    fn prepare_saved_document(
        &self,
        target: shell_contract::ipc::NativeSavedTarget,
    ) -> Result<Box<dyn shell_contract::editors::PreparedSavedDocument>, IpcError> {
        if target.surface() != self.2 {
            return Err(IpcError {
                name: Some("Error".into()),
                message: "The saved target has another editor family".into(),
            });
        }
        self.3.prepare(target)
    }

    fn load_saved_document(
        &self,
        document: shell_contract::ipc::NativeSavedDocument,
        then: Then<shell_contract::editors::SavedDocumentLoadOutcome>,
    ) {
        self.3.load(document, &self.4, then);
    }

    fn owns(&self, view: ViewId) -> bool {
        self.3.owns(view)
            || match self.2 {
                Surface::Sheets => self.listed("sheetsViews", view),
                Surface::Docs => self.listed("tornDown", view),
                Surface::Hwp => {
                    self.listed("hwpNativePending", view)
                        || ["hwpPaths", "hwpGrants"]
                            .iter()
                            .any(|key| self.by_view(key, view).is_some())
                }
                Surface::Html => self.by_view("htmlPaths", view).is_some(),
                _ => false,
            }
    }

    fn capture_document_owner(
        &self,
        _view: ViewId,
        native: shell_contract::ipc::NativeRequestOwner,
    ) -> Result<shell_contract::ipc::NativeRequestOwner, IpcError> {
        Ok(native)
    }

    fn request_save(&self, _view: ViewId, _mode: SaveMode, then: Then<bool>) {
        then(Ok(true))
    }

    fn request_save_owned(
        &self,
        view: ViewId,
        mode: SaveMode,
        _origin: shell_contract::ipc::NativeRequestOwner,
        _cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        self.request_save(view, mode, then)
    }

    fn request_save_native(
        &self,
        view: ViewId,
        mode: SaveMode,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<bool>,
    ) {
        let result = self.4.with_existing_document_save_effect(&binding, &[], &cancel, &pending, &mut || {
            if !matches!(binding.operation().selection(), shell_contract::ipc::NativeInvocationSelection::NativeCommand(
                shell_contract::ipc::NativeEditorCommand::SaveDocument { view: selected, selection: shell_contract::ipc::NativeSaveSelection::Mode(selected_mode), .. })
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

    fn request_close(&self, _view: ViewId, _parent: Option<WindowId>, then: Then<bool>) {
        then(Ok(true))
    }
    fn request_close_owned(
        &self,
        view: ViewId,
        parent: Option<WindowId>,
        _origin: shell_contract::ipc::NativeRequestOwner,
        _cancel: shell_contract::runtime::CancelToken,
        then: Then<bool>,
    ) {
        self.request_close(view, parent, then)
    }

    fn read_text(&self, _view: ViewId, then: Then<String>) {
        then(Ok(String::new()))
    }

    fn save_to_path(&self, _view: ViewId, _path: PathBuf, then: Then<()>) {
        then(Ok(()))
    }

    fn save_to_path_owned(
        &self,
        view: ViewId,
        path: PathBuf,
        _origin: shell_contract::ipc::NativeRequestOwner,
        _cancel: shell_contract::runtime::CancelToken,
        then: Then<()>,
    ) {
        self.save_to_path(view, path, then)
    }

    fn save_to_path_native(
        &self,
        view: ViewId,
        path: PathBuf,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<()>,
    ) {
        let result = self.4.with_existing_document_save_effect(&binding, &[], &cancel, &pending, &mut || {
            if !matches!(binding.operation().selection(), shell_contract::ipc::NativeInvocationSelection::NativeCommand(
                shell_contract::ipc::NativeEditorCommand::SaveDocument { view: selected, selection: shell_contract::ipc::NativeSaveSelection::ExactPath(selected_path), .. })
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

    fn file_renamed(&self, _view: ViewId, _old_path: &Path, _new_path: &Path) {}

    fn file_renamed_owned(
        &self,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        _origin: &shell_contract::ipc::NativeRequestOwner,
        _destination: &shell_contract::ipc::NativeRequestOwner,
        _cancel: &shell_contract::runtime::CancelToken,
        _pending: &shell_contract::runtime::CancelToken,
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
        origin: &shell_contract::ipc::NativeRequestOwner,
        destination: &shell_contract::ipc::NativeRequestOwner,
        participants: &[shell_contract::ipc::NativeRequestOwner],
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
        additional: &[shell_contract::runtime::CancelToken],
        then: Then<()>,
    ) {
        let mut captured = Vec::with_capacity(participants.len() + 1);
        captured.push(destination.clone());
        captured.extend_from_slice(participants);
        let additional = additional.iter().collect::<Vec<_>>();
        let result = shell_contract::hosts::synthetic::with_owned_effect_many_extra(
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
        then(result.map_err(|message| shell_contract::IpcError {
            name: Some("Error".into()),
            message,
        }));
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
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<Option<shell_contract::ipc::NativeSaveRevisionReceipt>>,
    ) {
        let mut captured = participants.to_vec();
        captured.push(destination.clone());
        let additional = additional.iter().collect::<Vec<_>>();
        let result = authority.snapshot().and_then(|snapshot| {
            if snapshot.selection().path() != expected_path {
                return Err("The native save path changed".into());
            }
            self.4.with_native_save_revision_snapshot_effect(
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
        let mut captured = participants.to_vec();
        captured.push(destination.clone());
        let result = shell_contract::hosts::synthetic::with_recovery_open_many(
            receipt,
            receiver,
            &captured,
            cancel,
            pending,
            &mut || {
                self.file_renamed(view, old, new);
                Ok(())
            },
        );
        then(result.map_err(|message| shell_contract::IpcError {
            name: Some("Error".into()),
            message,
        }));
    }

    fn export_request(&self, _view: ViewId, _format: &str) {}

    fn document_path(&self, view: ViewId) -> Option<PathBuf> {
        ["htmlPaths", "hwpPaths"]
            .iter()
            .find_map(|key| self.by_view(key, view)?.as_str())
            .map(PathBuf::from)
    }

    fn take_pdf_grant(&self, view: ViewId) -> Option<PathBuf> {
        let mut taken_views = self.1.borrow_mut();
        let taken = taken_views.contains(&view.0);
        taken_views.push(view.0);
        if taken {
            return None;
        }
        self.by_view("hwpGrants", view)?.as_str().map(PathBuf::from)
    }

    fn take_native_headless_pdf_grant(
        &self,
        view: ViewId,
        _native: Option<&shell_contract::ipc::NativeContext>,
    ) -> Result<Option<shell_contract::editors::NativeHeadlessPdfGrant>, String> {
        if self.listed("hwpNativePending", view) {
            Err("hwp: native headless request is not authenticated".into())
        } else {
            Ok(None)
        }
    }

    fn torn_down(&self, view: ViewId) -> bool {
        self.listed("tornDown", view)
    }
}

#[derive(Default)]
pub struct FakeRegistry {
    pub handlers: HashMap<&'static str, (ChannelKind, Rc<dyn Handler>)>,
    pub provided: Rc<Provided>,
}

impl Registry for FakeRegistry {
    fn register(
        &mut self,
        by: Batch,
        channel: &'static str,
        kind: ChannelKind,
        handler: Rc<dyn Handler>,
    ) -> Result<(), RegisterError> {
        let spec = CHANNELS
            .iter()
            .find(|spec| spec.name == channel)
            .ok_or(RegisterError::Unknown(channel))?;
        if spec.batch != by {
            return Err(RegisterError::WrongBatch {
                channel,
                owner: spec.batch,
                by,
            });
        }
        if kind == ChannelKind::Event || !spec.kinds.contains(&kind) {
            return Err(RegisterError::WrongKind { channel, kind });
        }
        if self.handlers.insert(channel, (kind, handler)).is_some() {
            return Err(RegisterError::Twice(channel));
        }
        Ok(())
    }

    fn provided(&self) -> Rc<Provided> {
        self.provided.clone()
    }
}

/// Runs a posted job at once on the calling thread; `is_current` answers
/// as the fake says
struct Inline {
    current: bool,
    queue: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    wake: Condvar,
}

impl Thread for Inline {
    fn post(&self, job: Box<dyn FnOnce() + Send>) {
        if self.current {
            // the core thread: run after the current job, as a queue would
            self.queue.lock().unwrap().push(job);
            self.wake.notify_one();
        } else {
            job()
        }
    }

    fn is_current(&self) -> bool {
        self.current
    }
}

impl Inline {
    fn drain(&self) {
        loop {
            let jobs = std::mem::take(&mut *self.queue.lock().unwrap());
            if jobs.is_empty() {
                return;
            }
            for job in jobs {
                job()
            }
        }
    }
}

struct InlineWorkers;

impl Workers for InlineWorkers {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
        job()
    }
}

#[derive(Default)]
pub struct Recorder(pub Mutex<Vec<(Target, &'static str, Vec<IpcValue>)>>);

impl EventSink for Recorder {
    fn emit(&self, target: Target, channel: &'static str, args: Vec<IpcValue>) {
        self.0.lock().unwrap().push((target, channel, args))
    }

    fn emit_to_owner_checked(
        &self,
        owner: &shell_contract::ipc::NativeRequestOwner,
        channel: &'static str,
        args: Vec<IpcValue>,
    ) -> Result<(), String> {
        let generation = owner
            .context
            .renderer()
            .ok_or("synthetic renderer generation unavailable")?;
        if !generation.is_active() {
            return Err("synthetic renderer generation is retired".into());
        }
        self.emit(Target::View(owner.view), channel, args);
        Ok(())
    }
}

struct FakeViews {
    views: Vec<Sender>,
    generations: std::collections::HashMap<ViewId, shell_contract::runtime::RendererGeneration>,
    replacements:
        Mutex<Vec<Box<dyn Fn(ViewId, shell_contract::runtime::RendererGeneration) + Send + Sync>>>,
}

impl Views for FakeViews {
    fn sender_of(&self, view: ViewId) -> Option<Sender> {
        self.views.iter().copied().find(|s| s.view == view)
    }

    fn all(&self) -> Vec<Sender> {
        self.views.clone()
    }

    fn on_destroyed(&self, _listener: Box<dyn Fn(ViewId) + Send + Sync>) {}
    fn renderer_generation(
        &self,
        view: ViewId,
    ) -> Option<shell_contract::runtime::RendererGeneration> {
        self.generations.get(&view).cloned()
    }
    fn on_renderer_replaced(
        &self,
        listener: Box<dyn Fn(ViewId, shell_contract::runtime::RendererGeneration) + Send + Sync>,
    ) {
        self.replacements.lock().unwrap().push(listener);
    }
}

struct Folder(PathBuf);

impl AppPaths for Folder {
    fn path(&self, name: PathName) -> PathBuf {
        let sub = match name {
            PathName::UserData => "userData",
            PathName::Temp => "temp",
            PathName::Home => "home",
            PathName::AppData => "appData",
            PathName::Exe => "exe",
            PathName::Desktop => "desktop",
            PathName::Documents => "documents",
            PathName::Downloads => "downloads",
            PathName::Logs => "logs",
        };
        self.0.join(sub)
    }
}

/// One test's shell with batch M registered
pub struct Probe {
    pub registry: FakeRegistry,
    pub events: Arc<Recorder>,
    pub services: Services,
    /// the case's host log: the M fakes' lines and the shared hosts'
    pub log: ProbeLog,
    core: Arc<Inline>,
    dir: PathBuf,
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// The sender of a call: view `n` in window `n`, of `surface`
pub fn sender(n: u32, surface: Surface) -> Sender {
    Sender {
        view: ViewId(n),
        window: WindowId(n),
        surface,
    }
}

impl Probe {
    /// A shell whose M hosts answer from `config` (the `hosts.m` object of
    /// probe.rs), with these views open, and batch M registered
    pub fn new(config: Value, views: Vec<Sender>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "shell-ipc-m-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["userData", "temp", "home", "documents"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let mut hosts = Hosts::default();
        let log = probe::insert(&mut hosts, &config);
        let shared = Arc::new(Shared {
            config: config.clone(),
            log: log.clone(),
        });
        hosts.insert::<DialogsHost>(shared.clone());
        hosts.insert::<OsShellHost>(shared.clone());
        hosts.insert::<ContentsHost>(shared.clone());
        hosts.insert::<MainWindowHost>(shared);
        let core = Arc::new(Inline {
            current: true,
            queue: Mutex::default(),
            wake: Condvar::new(),
        });
        let events = Arc::new(Recorder::default());
        let services = shell_contract::hosts::synthetic::SyntheticOwnedEffects::install(Services {
            ui: Arc::new(Inline {
                current: false,
                queue: Mutex::default(),
                wake: Condvar::new(),
            }),
            core: core.clone(),
            workers: Arc::new(InlineWorkers),
            events: events.clone(),
            views: Arc::new(FakeViews {
                generations: views
                    .iter()
                    .map(|sender| {
                        (
                            sender.view,
                            shell_contract::runtime::RendererGeneration::new(),
                        )
                    })
                    .collect(),
                views,
                replacements: Mutex::default(),
            }),
            paths: Arc::new(Folder(dir.clone())),
            hosts: Arc::new(hosts),
        });
        let mut registry = FakeRegistry::default();
        let config = Rc::new(config);
        let effects = services
            .hosts
            .get::<shell_contract::hosts::OwnedEffectsHost>()
            .expect("synthetic ownership service is installed")
            .clone();
        let grants = Rc::new(std::cell::RefCell::new(Vec::new()));
        for surface in [Surface::Sheets, Surface::Docs, Surface::Hwp, Surface::Html] {
            registry
                .provided
                .add::<dyn DocumentEditors>(Rc::new(FakeEditors(
                    config.clone(),
                    grants.clone(),
                    surface,
                    shell_contract::hosts::synthetic::SyntheticSavedActors::default(),
                    effects.clone(),
                )));
        }
        crate::register(&mut registry, &services).expect("batch M registers");
        Probe {
            registry,
            events,
            services,
            log,
            core,
            dir,
        }
    }

    /// The host log so far, one line per call, and forget it: a line's
    /// strings as they are, a list of strings joined by spaces, any other
    /// value as JSON, all joined by spaces
    pub fn take_calls(&self) -> Vec<String> {
        let lines = std::mem::take(&mut *self.log.lock().unwrap());
        render(&lines)
    }

    /// As [`Probe::take_calls`], keeping the log
    pub fn peek_calls(&self) -> Vec<String> {
        render(&self.log.lock().unwrap())
    }
}

fn render(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let items = line.as_array().cloned().unwrap_or_default();
            items
                .iter()
                .map(|item| match item {
                    Value::String(s) => s.clone(),
                    Value::Array(list) if list.iter().all(Value::is_string) => list
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

impl Probe {
    pub fn user_data(&self) -> PathBuf {
        self.dir.join("userData")
    }

    pub fn root(&self) -> &Path {
        &self.dir
    }

    /// Calls `channel` as `from` would and returns the answer once every job
    /// the call posted has run. A handler that never answered panics.
    pub fn call(&self, from: Sender, channel: &str, args: Vec<IpcValue>) -> Reply {
        let (_, handler) = self
            .registry
            .handlers
            .get(channel)
            .unwrap_or_else(|| panic!("no handler for {channel}"));
        let cx = CallContext {
            sender: from,
            request: Some(RequestId(1)),
            cancel: CancelToken::default(),
            native: None,
        };
        let slot: Arc<Mutex<Option<Reply>>> = Arc::default();
        let answer = slot.clone();
        let core = self.core.clone();
        handler.call(
            &cx,
            args,
            Responder::new(move |reply| {
                let previous = answer.lock().unwrap().replace(reply);
                assert!(previous.is_none(), "answered twice");
                let _guard = core.queue.lock().unwrap();
                core.wake.notify_one();
            }),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            self.core.drain();
            let queue = self.core.queue.lock().unwrap();
            if let Some(reply) = slot.lock().unwrap().take() {
                return reply;
            }
            let now = std::time::Instant::now();
            assert!(now < deadline, "{channel} did not answer");
            if queue.is_empty() {
                drop(self.core.wake.wait_timeout(queue, deadline - now).unwrap());
            }
        }
    }

    pub fn call_many(&self, from: Sender, channel: &str, calls: Vec<Vec<IpcValue>>) -> Vec<Reply> {
        let (_, handler) = self
            .registry
            .handlers
            .get(channel)
            .expect("handler not found");
        let mut slots = Vec::new();
        for args in calls {
            let slot: Arc<Mutex<Option<Reply>>> = Arc::default();
            let answer = slot.clone();
            let core = self.core.clone();
            let cx = CallContext {
                sender: from,
                request: Some(RequestId(1)),
                cancel: CancelToken::default(),
                native: None,
            };
            handler.call(
                &cx,
                args,
                Responder::new(move |reply| {
                    assert!(answer.lock().unwrap().replace(reply).is_none());
                    let _guard = core.queue.lock().unwrap();
                    core.wake.notify_one();
                }),
            );
            slots.push(slot);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            self.core.drain();
            let queue = self.core.queue.lock().unwrap();
            if slots.iter().all(|slot| slot.lock().unwrap().is_some()) {
                return slots
                    .into_iter()
                    .map(|slot| slot.lock().unwrap().take().unwrap())
                    .collect();
            }
            let now = std::time::Instant::now();
            assert!(now < deadline, "{channel} did not answer");
            if queue.is_empty() {
                drop(self.core.wake.wait_timeout(queue, deadline - now).unwrap());
            }
        }
    }

    /// The events emitted so far, and forget them
    pub fn take_events(&self) -> Vec<(Target, &'static str, Vec<IpcValue>)> {
        std::mem::take(&mut *self.events.0.lock().unwrap())
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
