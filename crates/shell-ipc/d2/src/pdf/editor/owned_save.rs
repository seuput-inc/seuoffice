//! Native authority retained across renderer save continuations.
use super::Editor;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::{
    ViewId,
    ipc::NativeRequestOwner,
    runtime::{CancelSubscription, CancelToken},
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
pub(super) enum SaveOrigin {
    Renderer(NativeRequestOwner),
    Native(shell_contract::ipc::NativeExistingDocumentBinding),
}

pub(super) enum SaveDestination {
    Renderer(std::path::PathBuf),
    Native(shell_contract::ipc::NativeSaveDestination),
}

impl SaveDestination {
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::Renderer(path) => path,
            Self::Native(selection) => selection.path(),
        }
    }
}

#[derive(Clone)]
pub(super) struct SaveAuthority {
    pub id: String,
    pub origin: SaveOrigin,
    pub destination: NativeRequestOwner,
    pub cancel: CancelToken,
    pub pending: CancelToken,
    pub effects: OwnedEffectsHost,
    _subscriptions: Arc<Vec<CancelSubscription>>,
    written: Arc<AtomicBool>,
    request_subscription: Option<Arc<CancelSubscription>>,
    request_cancel: Option<CancelToken>,
    caller_pending: Option<CancelToken>,
    callback_owner: Option<Arc<std::sync::Mutex<NativeRequestOwner>>>,
    receipt_subscription: Option<Arc<CancelSubscription>>,
    selection: Arc<std::sync::Mutex<Option<shell_contract::ipc::NativeSaveDestination>>>,
    revision_authority:
        Arc<std::sync::Mutex<Option<shell_contract::ipc::NativeSaveRevisionAuthority>>>,
    revision_subscription: Arc<std::sync::Mutex<Option<CancelSubscription>>>,
}
impl SaveAuthority {
    pub fn mark_written(&self) {
        self.written.store(true, Ordering::Release);
    }
    pub fn did_write(&self) -> bool {
        self.written.load(Ordering::Acquire)
    }
    pub fn admit_file(
        &self,
        effect: &mut dyn FnMut(),
    ) -> Result<(), main_core::node_fs::NodeError> {
        self.commit(&mut || {
            effect();
            Ok(())
        })
        .map_err(|message| main_core::node_fs::NodeError {
            code: Some("ECANCELED"),
            message,
        })
    }
    pub fn current(&self) -> Result<(), String> {
        self.commit(&mut || Ok(()))
    }
    fn native_snapshot(
        &self,
    ) -> Result<Option<shell_contract::ipc::NativeSaveRevisionSnapshot>, String> {
        let cell = self
            .revision_authority
            .lock()
            .map_err(|_| "PDF save authority unavailable")?
            .clone();
        cell.map(|cell| cell.snapshot()).transpose()
    }
    fn native_binding(&self) -> Result<shell_contract::ipc::NativeExistingDocumentBinding, String> {
        if let Some(snapshot) = self.native_snapshot()? {
            return Ok(snapshot.selection().binding().clone());
        }
        match &self.origin {
            SaveOrigin::Native(binding) => Ok(binding.clone()),
            SaveOrigin::Renderer(_) => Err("PDF save is not native-owned".into()),
        }
    }
    fn native_scope(
        &self,
    ) -> Result<
        (
            Option<shell_contract::ipc::NativeSaveRevisionSnapshot>,
            shell_contract::ipc::NativeExistingDocumentBinding,
            Option<shell_contract::ipc::NativeSaveDestination>,
            Vec<NativeRequestOwner>,
        ),
        String,
    > {
        let snapshot = self.native_snapshot()?;
        let (binding, selection, mut owners) = match &snapshot {
            Some(snapshot) => (
                snapshot.selection().binding().clone(),
                Some(snapshot.selection().clone()),
                snapshot.aliases().to_vec(),
            ),
            None => {
                let SaveOrigin::Native(binding) = &self.origin else {
                    return Err("PDF save is not native-owned".into());
                };
                (
                    binding.clone(),
                    self.selection
                        .lock()
                        .map_err(|_| "PDF save destination unavailable")?
                        .clone(),
                    vec![self.destination.clone()],
                )
            }
        };
        if let Some(owner) = &self.callback_owner {
            owners.push(
                owner
                    .lock()
                    .map_err(|_| "PDF save callback unavailable")?
                    .clone(),
            );
        }
        Ok((snapshot, binding, selection, owners))
    }
    pub fn emit(&self, channel: &str, args: Vec<shell_contract::IpcValue>) -> Result<(), String> {
        let additional: Vec<_> = self
            .request_cancel
            .iter()
            .chain(self.caller_pending.iter())
            .collect();
        match &self.origin {
            SaveOrigin::Renderer(origin) => self.effects.emit_owned_many_extra(
                origin,
                &self.participants()?,
                0,
                &self.cancel,
                &self.pending,
                &additional,
                channel,
                args,
            ),
            SaveOrigin::Native(_) => {
                let (snapshot, binding, _, participants) = self.native_scope()?;
                match snapshot {
                    Some(snapshot) => self.effects.emit_native_save_revision_snapshot(
                        &snapshot,
                        &participants,
                        0,
                        &self.cancel,
                        &self.pending,
                        &additional,
                        channel,
                        args,
                    ),
                    None => self.effects.emit_existing_document_save_many_extra(
                        &binding,
                        &participants,
                        0,
                        &self.cancel,
                        &self.pending,
                        &additional,
                        channel,
                        args,
                    ),
                }
            }
        }
    }
    pub fn participants(&self) -> Result<Vec<NativeRequestOwner>, String> {
        if matches!(&self.origin, SaveOrigin::Native(_)) {
            return self.native_scope().map(|(_, _, _, owners)| owners);
        }
        let mut owners = vec![self.destination.clone()];
        if let Some(owner) = &self.callback_owner {
            owners.push(
                owner
                    .lock()
                    .map_err(|_| "PDF save callback unavailable")?
                    .clone(),
            );
        }
        Ok(owners)
    }
    pub fn additional(&self) -> Vec<CancelToken> {
        self.request_cancel
            .iter()
            .chain(self.caller_pending.iter())
            .cloned()
            .collect()
    }
    fn accept_revision_receipt(
        &self,
        receipt: &shell_contract::ipc::NativeSaveRevisionReceipt,
    ) -> Result<(), String> {
        let current = self
            .native_snapshot()?
            .ok_or("PDF save authority unavailable")?;
        if !current.same_snapshot(receipt.snapshot()) {
            return Err("PDF save revision receipt belongs to another operation".into());
        }
        let destination = receipt
            .snapshot()
            .aliases()
            .first()
            .ok_or("PDF save destination unavailable")?;
        let loss = destination
            .loss_token()
            .ok_or("PDF save owner lifetime unavailable")?;
        let pending = self.pending.clone();
        let next = loss.on_cancel(move || pending.cancel());
        let replacement = match &self.callback_owner {
            Some(owner) => {
                let old = owner
                    .lock()
                    .map_err(|_| "PDF save callback unavailable")?
                    .clone();
                Some(
                    receipt
                        .renewed_request_owner(&old)
                        .ok_or("PDF save callback successor unavailable")?
                        .clone(),
                )
            }
            None => None,
        };
        let old = self
            .revision_subscription
            .lock()
            .map_err(|_| "PDF save subscription unavailable")?
            .replace(next);
        if let (Some(owner), Some(replacement)) = (&self.callback_owner, replacement) {
            *owner.lock().map_err(|_| "PDF save callback unavailable")? = replacement;
        }
        drop(old);
        Ok(())
    }
    pub fn set_selection(
        &self,
        selection: shell_contract::ipc::NativeSaveDestination,
    ) -> Result<(), String> {
        let binding = self.native_binding()?;
        if !selection.matches_binding(&binding) {
            return Err("PDF save destination belongs to another operation".into());
        }
        let current = {
            let mut cell = self
                .revision_authority
                .lock()
                .map_err(|_| "PDF save authority unavailable")?;
            match cell.as_ref() {
                Some(current) => Some(current.clone()),
                None => {
                    *cell = Some(shell_contract::ipc::NativeSaveRevisionAuthority::new(
                        selection.clone(),
                        vec![self.destination.clone()],
                    ));
                    None
                }
            }
        };
        if let Some(current) = current {
            let snapshot = current.snapshot()?;
            let mut prepared = snapshot.prepare_selection(selection.clone())?;
            let participants = self.participants()?;
            let additional: Vec<_> = self
                .request_cancel
                .iter()
                .chain(self.caller_pending.iter())
                .collect();
            self.effects.with_native_save_selection_transition(
                &mut prepared,
                &participants,
                &self.cancel,
                &self.pending,
                &additional,
                &mut || {},
                &mut |permit, prepared| {
                    prepared.commit(permit)?;
                    Ok(())
                },
            )?;
        }
        *self
            .selection
            .lock()
            .map_err(|_| "PDF save destination unavailable")? = Some(selection);
        Ok(())
    }
    pub fn selection(&self) -> Result<shell_contract::ipc::NativeSaveDestination, String> {
        if let Some(snapshot) = self.native_snapshot()? {
            return Ok(snapshot.selection().clone());
        }
        self.selection
            .lock()
            .map_err(|_| "PDF save destination unavailable")?
            .clone()
            .ok_or_else(|| "PDF save destination was not selected".into())
    }
    pub fn save_dialog(
        &self,
        dialogs: &shell_contract::DialogsHost,
        parent: Option<shell_contract::WindowId>,
        options: shell_contract::hosts::FileDialog,
        done: Box<dyn FnOnce(Result<Option<SaveDestination>, String>) + Send>,
    ) {
        match &self.origin {
            SaveOrigin::Renderer(origin) => match self.participants() {
                Ok(participants) => dialogs.save_owned_many_extra(
                    parent,
                    options,
                    origin.clone(),
                    participants,
                    self.cancel.clone(),
                    self.pending.clone(),
                    self.additional(),
                    Box::new(move |result| {
                        done(result.map(|path| path.map(SaveDestination::Renderer)))
                    }),
                ),
                Err(error) => done(Err(error)),
            },
            SaveOrigin::Native(_) => match self.native_scope() {
                Ok((Some(snapshot), _, _, participants)) => dialogs
                    .save_existing_document_revision(
                        parent,
                        options,
                        snapshot,
                        participants,
                        self.cancel.clone(),
                        self.pending.clone(),
                        self.additional(),
                        Box::new(move |result| {
                            done(result.map(|path| path.map(SaveDestination::Native)))
                        }),
                    ),
                Ok((None, binding, _, participants)) => dialogs.save_existing_document(
                    parent,
                    options,
                    binding,
                    participants,
                    self.cancel.clone(),
                    self.pending.clone(),
                    self.additional(),
                    Box::new(move |result| {
                        done(result.map(|path| path.map(SaveDestination::Native)))
                    }),
                ),
                Err(error) => done(Err(error)),
            },
        }
    }
    pub fn message_dialog(
        &self,
        dialogs: &shell_contract::DialogsHost,
        parent: Option<shell_contract::WindowId>,
        options: shell_contract::hosts::MessageBox,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::MessageBoxAnswer, String>) + Send>,
    ) {
        match &self.origin {
            SaveOrigin::Renderer(origin) => match self.participants() {
                Ok(participants) => dialogs.message_owned_many_extra(
                    parent,
                    options,
                    origin.clone(),
                    participants,
                    self.cancel.clone(),
                    self.pending.clone(),
                    self.additional(),
                    done,
                ),
                Err(error) => done(Err(error)),
            },
            SaveOrigin::Native(_) => match self.native_scope() {
                Ok((Some(snapshot), _, _, participants)) => dialogs
                    .message_existing_document_revision(
                        parent,
                        options,
                        snapshot,
                        participants,
                        self.cancel.clone(),
                        self.pending.clone(),
                        self.additional(),
                        done,
                    ),
                Ok((None, binding, _, participants)) => dialogs.message_existing_document(
                    parent,
                    options,
                    binding,
                    participants,
                    self.cancel.clone(),
                    self.pending.clone(),
                    self.additional(),
                    done,
                ),
                Err(error) => done(Err(error)),
            },
        }
    }
    pub fn validate_target(&self, target: &std::path::Path) -> Result<(), String> {
        match &self.origin {
            SaveOrigin::Renderer(_) => Ok(()),
            SaveOrigin::Native(_) if self.selection()?.path() == target => Ok(()),
            SaveOrigin::Native(_) => {
                Err("PDF save target does not match its selected destination".into())
            }
        }
    }
    pub fn redaction_saved(
        &self,
        shell: &super::PdfShellHost,
        view: ViewId,
        path: &str,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::SavedFile<String>, String>)>,
    ) {
        match &self.origin {
            SaveOrigin::Renderer(origin) => match self.participants() {
                Ok(participants) => shell.redaction_saved_owned(
                    view,
                    path,
                    origin.clone(),
                    self.destination.clone(),
                    participants,
                    self.cancel.clone(),
                    self.pending.clone(),
                    self.additional(),
                    self.effects.clone(),
                    done,
                ),
                Err(error) => done(Err(error)),
            },
            SaveOrigin::Native(_) => {
                let authority = match self.revision_authority.lock() {
                    Ok(cell) => cell.clone(),
                    Err(_) => return done(Err("PDF save authority unavailable".into())),
                };
                let Some(authority) = authority else {
                    return done(Err("PDF save destination was not selected".into()));
                };
                let snapshot = match authority.snapshot() {
                    Ok(snapshot) => snapshot,
                    Err(error) => return done(Err(error)),
                };
                if snapshot.selection().path() != std::path::Path::new(path) {
                    return done(Err(
                        "PDF save target does not match its selected destination".into(),
                    ));
                }
                let mut participants = snapshot.aliases().to_vec();
                if let Some(owner) = &self.callback_owner {
                    match owner.lock() {
                        Ok(owner) => participants.push(owner.clone()),
                        Err(_) => return done(Err("PDF save callback unavailable".into())),
                    }
                }
                let scope = self.clone();
                let on_revision = std::rc::Rc::new(
                    move |receipt: &shell_contract::ipc::NativeSaveRevisionReceipt| {
                        scope.accept_revision_receipt(receipt)
                    },
                );
                shell.redaction_saved_existing_document(
                    view,
                    authority,
                    path,
                    participants,
                    self.cancel.clone(),
                    self.pending.clone(),
                    self.additional(),
                    self.effects.clone(),
                    on_revision,
                    done,
                );
            }
        }
    }
    pub fn with_destination_receipt(
        mut self,
        destination: NativeRequestOwner,
    ) -> Result<Self, String> {
        let current = self
            .participants()?
            .into_iter()
            .next()
            .ok_or("PDF target owner unavailable")?;
        let matching_slot = match (current.pdf_document(), destination.pdf_document()) {
            (Some(a), Some(b)) => a.same_document(b),
            _ => false,
        };
        let matching_renderer = match (current.context.renderer(), destination.context.renderer()) {
            (Some(a), Some(b)) => a.same_generation(b),
            _ => false,
        };
        let matching_revision = match (current.pdf_revision(), destination.pdf_revision()) {
            (Some(a), Some(b)) => a.same_document(b),
            (None, _) if matches!(&self.origin, SaveOrigin::Renderer(_)) => true,
            _ => false,
        };
        if current.view != destination.view
            || !matching_slot
            || !matching_renderer
            || !matching_revision
        {
            return Err("PDF target grant belongs to another document".into());
        }
        if matches!(&self.origin, SaveOrigin::Renderer(_)) {
            let loss = destination
                .loss_token()
                .ok_or("PDF target grant owner unavailable")?;
            let pending = self.pending.clone();
            self.receipt_subscription = Some(Arc::new(loss.on_cancel(move || pending.cancel())));
        }
        self.destination = destination;
        self.current()?;
        Ok(self)
    }
    pub fn begin_save_as_flow(&self) -> Result<(), String> {
        use crate::kit::IntoIpc;
        self.emit(
            "pdf:save-as-flow",
            vec![true.into_ipc(), self.id.clone().into_ipc()],
        )
    }
    fn clear_save_as_flow(&self) {
        use crate::kit::IntoIpc;
        if let Ok(Some(destination)) = self.participants().map(|owners| owners.into_iter().next()) {
            if let Some(loss) = destination.loss_token() {
                let _ = self.effects.emit_owned(
                    &destination,
                    &destination,
                    &loss,
                    &loss,
                    "pdf:save-as-flow",
                    vec![false.into_ipc(), self.id.clone().into_ipc()],
                );
            }
        }
    }
    pub fn with_request_scope(
        mut self,
        view: ViewId,
        context: Option<shell_contract::ipc::NativeContext>,
        cancel: &CancelToken,
    ) -> Result<Self, String> {
        let context = context.ok_or("PDF save callback is not authenticated")?;
        let callback = NativeRequestOwner { view, context };
        self.callback_owner = Some(Arc::new(std::sync::Mutex::new(callback.clone())));
        self.request_cancel = Some(cancel.clone());
        let additional: Vec<_> = self
            .request_cancel
            .iter()
            .chain(self.caller_pending.iter())
            .collect();
        let destination = self
            .participants()?
            .into_iter()
            .next()
            .ok_or("PDF save destination unavailable")?;
        self.effects.with_same_native_origin_effect(
            &destination,
            &callback,
            &[],
            &self.cancel,
            &self.pending,
            &additional,
            &mut || Ok(()),
        )?;
        self.current()?;
        let pending = self.pending.clone();
        self.request_subscription = Some(Arc::new(cancel.on_cancel(move || pending.cancel())));
        Ok(self)
    }
    pub fn write(&self, target: &str, bytes: &[u8]) -> Result<(), String> {
        let native = match &self.origin {
            SaveOrigin::Native(_) => {
                let (snapshot, _, selection, participants) = self.native_scope()?;
                let selection = selection.ok_or("PDF save destination was not selected")?;
                if selection.path() != std::path::Path::new(target) {
                    return Err("PDF save target does not match its selected destination".into());
                }
                Some((snapshot, selection, participants))
            }
            SaveOrigin::Renderer(_) => None,
        };
        main_core::packages::electron_utils::atomic_write::atomic_write_file_authorized(
            target,
            bytes,
            &mut |effect| match &native {
                None => self.admit_file(effect),
                Some((snapshot, selection, participants)) => {
                    let additional: Vec<_> = self
                        .request_cancel
                        .iter()
                        .chain(self.caller_pending.iter())
                        .collect();
                    let mut used = false;
                    let mut guarded = || {
                        if used {
                            return Err("PDF file effect was invoked more than once".into());
                        }
                        used = true;
                        effect();
                        Ok(())
                    };
                    let admitted = match snapshot {
                        Some(snapshot) => self.effects.with_native_save_revision_snapshot_effect(
                            snapshot,
                            participants,
                            &self.cancel,
                            &self.pending,
                            &additional,
                            &mut guarded,
                        ),
                        None => self.effects.with_existing_document_destination_effect(
                            selection,
                            std::path::Path::new(target),
                            participants,
                            &self.cancel,
                            &self.pending,
                            &additional,
                            &mut guarded,
                        ),
                    };
                    admitted
                        .and_then(|()| {
                            if used {
                                Ok(())
                            } else {
                                Err("PDF file effect was not admitted".into())
                            }
                        })
                        .map_err(|message| main_core::node_fs::NodeError {
                            code: Some("ECANCELED"),
                            message,
                        })
                }
            },
        )
        .map_err(|error| error.message)
    }
    pub fn commit(&self, action: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        let mut used = false;
        let mut guarded = || {
            if used {
                return Err("PDF effect admission was invoked more than once".into());
            }
            used = true;
            action()
        };
        let additional: Vec<_> = self
            .request_cancel
            .iter()
            .chain(self.caller_pending.iter())
            .collect();
        match &self.origin {
            SaveOrigin::Renderer(origin) => self.effects.with_owned_effect_many_extra(
                origin,
                &self.participants()?,
                &self.cancel,
                &self.pending,
                &additional,
                &mut guarded,
            ),
            SaveOrigin::Native(_) => {
                let (snapshot, binding, _, participants) = self.native_scope()?;
                match snapshot {
                    Some(snapshot) => self.effects.with_native_save_revision_snapshot_effect(
                        &snapshot,
                        &participants,
                        &self.cancel,
                        &self.pending,
                        &additional,
                        &mut guarded,
                    ),
                    None => self.effects.with_existing_document_save_effect_extra(
                        &binding,
                        &participants,
                        &self.cancel,
                        &self.pending,
                        &additional,
                        &mut guarded,
                    ),
                }
            }
        }
    }
    pub fn admit_node(
        &self,
        effect: &mut dyn FnMut() -> Result<(), main_core::node_fs::NodeError>,
    ) -> Result<(), main_core::node_fs::NodeError> {
        let mut original_error = None;
        let admitted = self.commit(&mut || match effect() {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = error.message.clone();
                original_error = Some(error);
                Err(message)
            }
        });
        if let Some(error) = original_error {
            return Err(error);
        }
        admitted.map_err(|message| main_core::node_fs::NodeError {
            code: Some("ECANCELED".into()),
            message,
        })
    }
}

pub(super) struct NativeSaveRevisionEntry {
    pub view: ViewId,
    pub document: shell_contract::ipc::PdfDocumentLease,
    pub previous: shell_contract::ipc::PdfDocumentLease,
    pub successor: shell_contract::ipc::PdfDocumentLease,
}

pub(super) struct NativeSaveRevisionStage {
    path: String,
    entries: Vec<NativeSaveRevisionEntry>,
}

impl NativeSaveRevisionStage {
    pub fn prepare(
        editor: &Editor,
        view: ViewId,
        path: &str,
        owner: &NativeRequestOwner,
    ) -> Result<Self, String> {
        let state = editor.st.borrow();
        let source = state.views.get(&view).ok_or("PDF document unavailable")?;
        if owner.view != view
            || !owner
                .pdf_document()
                .zip(source.document_lease.as_ref())
                .is_some_and(|(a, b)| a.same_document(b))
            || !owner
                .pdf_revision()
                .zip(source.document_revision.as_ref())
                .is_some_and(|(a, b)| a.same_document(b))
        {
            return Err("PDF save revision changed before preparation".into());
        }
        let mut entries = Vec::new();
        for (view, slot) in &state.views {
            if slot.open_path.as_deref() != Some(path) {
                continue;
            }
            if let Some(document) = &slot.document_lease {
                let previous = slot
                    .document_revision
                    .clone()
                    .ok_or("PDF document revision is unavailable")?;
                entries.push(NativeSaveRevisionEntry {
                    view: *view,
                    document: document.clone(),
                    previous,
                    successor: shell_contract::ipc::PdfDocumentLease::new(),
                });
            }
        }
        Ok(Self {
            path: path.to_owned(),
            entries,
        })
    }

    pub fn entries(&self) -> &[NativeSaveRevisionEntry] {
        &self.entries
    }

    pub fn commit_admitted<T>(
        &self,
        editor: &Editor,
        retire_and_renew: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = editor.st.borrow_mut();
        let count = state
            .views
            .values()
            .filter(|slot| {
                slot.open_path.as_deref() == Some(self.path.as_str())
                    && slot.document_lease.is_some()
            })
            .count();
        if count != self.entries.len() {
            return Err("PDF save documents changed before revision transition".into());
        }
        for entry in &self.entries {
            let slot = state
                .views
                .get(&entry.view)
                .ok_or("PDF save document was closed")?;
            if slot.open_path.as_deref() != Some(self.path.as_str())
                || !slot
                    .document_lease
                    .as_ref()
                    .is_some_and(|document| document.same_document(&entry.document))
                || !slot
                    .document_revision
                    .as_ref()
                    .is_some_and(|revision| revision.same_document(&entry.previous))
            {
                return Err("PDF save revision changed before transition".into());
            }
        }
        let output = retire_and_renew()?;
        for entry in &self.entries {
            state
                .views
                .get_mut(&entry.view)
                .expect("validated PDF save view")
                .document_revision = Some(entry.successor.clone());
        }
        Ok(output)
    }
}

impl SaveAuthority {
    pub(super) fn advance_native_write_revisions(
        &self,
        editor: &Editor,
        view: ViewId,
        path: &str,
    ) -> Result<bool, String> {
        if !matches!(&self.origin, SaveOrigin::Native(_)) {
            return Ok(false);
        }
        let selection = self.selection()?;
        let stage =
            NativeSaveRevisionStage::prepare(editor, view, path, selection.binding().owner())?;
        if stage.entries().is_empty() {
            return Ok(true);
        }
        let cell = {
            let mut shared = self
                .revision_authority
                .lock()
                .map_err(|_| "PDF save authority unavailable")?;
            if shared.is_none() {
                *shared = Some(shell_contract::ipc::NativeSaveRevisionAuthority::new(
                    selection,
                    vec![self.destination.clone()],
                ));
            }
            shared
                .as_ref()
                .ok_or("PDF save authority unavailable")?
                .clone()
        };
        let snapshot = cell.snapshot()?;
        let pairs = stage
            .entries()
            .iter()
            .map(|entry| (entry.previous.clone(), entry.successor.clone()))
            .collect();
        let callback = self
            .callback_owner
            .as_ref()
            .map(|owner| {
                owner
                    .lock()
                    .map(|owner| owner.clone())
                    .map_err(|_| "PDF save callback unavailable")
            })
            .transpose()?;
        let request_aliases: Vec<_> = callback.iter().cloned().collect();
        let mut prepared = snapshot.prepare_with_request_aliases(pairs, &request_aliases)?;
        let participants = self.participants()?;
        let additional: Vec<_> = self
            .request_cancel
            .iter()
            .chain(self.caller_pending.iter())
            .collect();
        let updated = std::cell::RefCell::new(
            None::<(
                shell_contract::ipc::NativeSaveRevisionSnapshot,
                Option<NativeRequestOwner>,
            )>,
        );
        let cleanup_error = std::cell::RefCell::new(None::<String>);
        let mut before_notify = || {
            let result = (|| -> Result<(), String> {
                let updated = updated.borrow();
                let Some((snapshot, callback)) = updated.as_ref() else {
                    return Ok(());
                };
                let destination = snapshot
                    .aliases()
                    .first()
                    .ok_or("PDF save destination unavailable")?;
                let loss = destination
                    .loss_token()
                    .ok_or("PDF save owner lifetime unavailable")?;
                let pending = self.pending.clone();
                let next = loss.on_cancel(move || pending.cancel());
                let old = self
                    .revision_subscription
                    .lock()
                    .map_err(|_| "PDF save subscription unavailable")?
                    .replace(next);
                if let (Some(cell), Some(callback)) = (&self.callback_owner, callback) {
                    *cell.lock().map_err(|_| "PDF save callback unavailable")? = callback.clone();
                }
                drop(old);
                Ok(())
            })();
            if let Err(error) = result {
                self.pending.cancel();
                *cleanup_error.borrow_mut() = Some(error);
            }
        };
        self.effects.with_native_save_revision_transition(
            &mut prepared,
            &participants,
            &self.cancel,
            &self.pending,
            &additional,
            &mut before_notify,
            &mut |retirement, prepared| {
                let next = stage.commit_admitted(editor, || prepared.commit(retirement))?;
                let callback = callback.as_ref().map(|owner| {
                    prepared
                        .renewed_request_owner(owner)
                        .expect("validated PDF callback successor")
                        .clone()
                });
                *updated.borrow_mut() = Some((next, callback));
                Ok(())
            },
        )?;
        drop(before_notify);
        if let Some(error) = cleanup_error.into_inner() {
            return Err(error);
        }
        Ok(true)
    }
}

impl Editor {
    pub(super) fn owned_save_authority(
        &self,
        view: ViewId,
        id: Option<&str>,
    ) -> Result<Option<SaveAuthority>, String> {
        let state = self.st.borrow();
        match (
            id,
            state
                .views
                .get(&view)
                .and_then(|slot| slot.owned_save.as_ref()),
        ) {
            (Some(id), Some(owned)) if owned.id == id => {
                owned.current()?;
                Ok(Some(owned.clone()))
            }
            (None, None) => Ok(None),
            _ => Err("PDF save correlation expired".into()),
        }
    }
}
impl Editor {
    pub(super) fn direct_save_authority(
        &self,
        view: ViewId,
        context: Option<shell_contract::ipc::NativeContext>,
        cancel: CancelToken,
    ) -> Result<Option<SaveAuthority>, String> {
        #[cfg(any(test, feature = "probe"))]
        if self.engine.synthetic_save_capability()
            && self.services.hosts.get::<OwnedEffectsHost>().is_none()
        {
            return Ok(None);
        }
        let context = context.ok_or("pdf: save request is not authenticated")?;
        self.capture_save_authority(view, NativeRequestOwner { view, context }, cancel)
            .map(Some)
    }
    pub(super) fn capture_save_authority(
        &self,
        view: ViewId,
        origin: NativeRequestOwner,
        cancel: CancelToken,
    ) -> Result<SaveAuthority, String> {
        let effects = self
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .ok_or("PDF owned effects unavailable")?;
        let renderer = self
            .services
            .views
            .renderer_generation(view)
            .ok_or("PDF document unavailable")?;
        let destination = effects.capture_owner(view, &renderer)?;
        let destination = self
            .bind_pdf_owner(view, destination.context)
            .ok_or("PDF document unavailable")?;
        let pending = CancelToken::default();
        let mut subscriptions = Vec::new();
        for token in [
            origin.loss_token(),
            destination.loss_token(),
            Some(cancel.clone()),
        ] {
            let token = token.ok_or("PDF save owner lifetime unavailable")?;
            let pending = pending.clone();
            subscriptions.push(token.on_cancel(move || pending.cancel()));
        }
        let authority = SaveAuthority {
            id: format!("pdf-save-{}", crate::kit::unique()),
            origin: SaveOrigin::Renderer(origin),
            destination,
            cancel,
            pending,
            effects,
            _subscriptions: Arc::new(subscriptions),
            request_subscription: None,
            request_cancel: None,
            caller_pending: None,
            callback_owner: None,
            receipt_subscription: None,
            written: Arc::new(AtomicBool::new(false)),
            selection: Arc::new(std::sync::Mutex::new(None)),
            revision_authority: Arc::new(std::sync::Mutex::new(None)),
            revision_subscription: Arc::new(std::sync::Mutex::new(None)),
        };
        authority.current()?;
        Ok(authority)
    }

    pub(super) fn capture_native_save_authority(
        &self,
        view: ViewId,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        caller_pending: CancelToken,
    ) -> Result<SaveAuthority, String> {
        let destination = binding.owner().clone();
        if destination.view != view {
            return Err("PDF save binding belongs to another view".into());
        }
        let state = self.st.borrow();
        let slot = state.views.get(&view).ok_or("PDF document unavailable")?;
        if !destination
            .pdf_document()
            .zip(slot.document_lease.as_ref())
            .is_some_and(|(a, b)| a.same_document(b))
            || !destination
                .pdf_revision()
                .zip(slot.document_revision.as_ref())
                .is_some_and(|(a, b)| a.same_document(b))
        {
            return Err("PDF save binding belongs to another document revision".into());
        }
        drop(state);
        let effects = self
            .services
            .hosts
            .get::<OwnedEffectsHost>()
            .cloned()
            .ok_or("PDF owned effects unavailable")?;
        let pending = CancelToken::default();
        let loss = destination
            .loss_token()
            .ok_or("PDF save owner lifetime unavailable")?;
        let notify = pending.clone();
        let revision_subscription = loss.on_cancel(move || notify.cancel());
        let mut subscriptions = Vec::new();
        for token in [
            Some(binding.operation().loss_token()),
            Some(cancel.clone()),
            Some(caller_pending.clone()),
        ] {
            let token = token.ok_or("PDF save owner lifetime unavailable")?;
            let pending = pending.clone();
            subscriptions.push(token.on_cancel(move || pending.cancel()));
        }
        let authority = SaveAuthority {
            id: format!("pdf-save-{}", crate::kit::unique()),
            origin: SaveOrigin::Native(binding),
            destination,
            cancel,
            pending,
            effects,
            _subscriptions: Arc::new(subscriptions),
            request_subscription: None,
            request_cancel: None,
            caller_pending: Some(caller_pending),
            callback_owner: None,
            receipt_subscription: None,
            written: Arc::new(AtomicBool::new(false)),
            selection: Arc::new(std::sync::Mutex::new(None)),
            revision_authority: Arc::new(std::sync::Mutex::new(None)),
            revision_subscription: Arc::new(std::sync::Mutex::new(Some(revision_subscription))),
        };
        authority.current()?;
        Ok(authority)
    }

    pub(super) fn end_owned_save(&self, view: ViewId, id: &str) {
        let authority = {
            let mut state = self.st.borrow_mut();
            let Some(slot) = state.views.get_mut(&view) else {
                return;
            };
            if !slot.owned_save.as_ref().is_some_and(|owned| owned.id == id) {
                return;
            }
            slot.untitled_copy_target = None;
            slot.owned_save_subscription.take();
            slot.save_as_target = None;
            slot.owned_save.take()
        };
        if let Some(authority) = authority {
            authority.clear_save_as_flow();
            authority.pending.cancel();
        }
    }

    pub(super) fn start_owned_save(
        &self,
        view: ViewId,
        authority: SaveAuthority,
    ) -> Result<(), String> {
        if self.st.borrow().views.get(&view).is_none_or(|slot| {
            slot.owned_save.is_some() || slot.close_save.is_some() || slot.save_as.is_some()
        }) {
            return Err("PDF save already pending".into());
        }
        let id = authority.id.clone();
        let me = self.me.clone();
        let back = crate::kit::back_on_core(&self.services, move |(): ()| {
            if let Some(ed) = me.upgrade() {
                let waiters = {
                    let mut state = ed.st.borrow_mut();
                    let Some(slot) = state.views.get_mut(&view) else {
                        return;
                    };
                    if !slot.owned_save.as_ref().is_some_and(|owned| owned.id == id) {
                        return;
                    }
                    (slot.close_save.take(), slot.save_as.take())
                };
                ed.end_owned_save(view, &id);
                if let Some((waiting, timer)) = waiters.0 {
                    crate::kit::clear(timer);
                    super::resolve(&waiting, false);
                }
                if let Some((waiting, timer)) = waiters.1 {
                    crate::kit::clear(timer);
                    super::resolve(&waiting, super::editors::SaveAsOutcome::Failed);
                }
            }
        });
        let subscription = authority.pending.on_cancel(move || back(()));
        authority.current()?;
        self.with_view(view, |slot| {
            slot.owned_save = Some(authority);
            slot.owned_save_subscription = Some(subscription);
        });
        Ok(())
    }

    pub(super) fn pick_owned_untitled(
        &self,
        view: ViewId,
        path: &shell_contract::IpcValue,
        name: Option<&str>,
        id: Option<&str>,
        context: Option<shell_contract::ipc::NativeContext>,
        cancel: &CancelToken,
        reply: shell_contract::ipc::Responder,
    ) {
        use crate::kit::IntoIpc;
        let owned = match self.owned_save_authority(view, id) {
            Ok(Some(owned)) => match owned.with_request_scope(view, context, cancel) {
                Ok(owned) => owned,
                Err(message) => return reply.reply(Err(crate::kit::error(message))),
            },
            _ => return reply.reply(Ok(false.into_ipc())),
        };
        self.pick_untitled_with_authority(view, path, name, owned, reply)
    }

    pub(super) fn pick_untitled_with_authority(
        &self,
        view: ViewId,
        path: &shell_contract::IpcValue,
        name: Option<&str>,
        owned: SaveAuthority,
        reply: shell_contract::ipc::Responder,
    ) {
        use crate::kit::IntoIpc;
        let Some(path) = self
            .granted(view, path)
            .filter(|path| self.is_untitled_copy(path))
        else {
            return reply.reply(Ok(false.into_ipc()));
        };
        if self.open_path(view).as_deref() != Some(path.as_str()) {
            return reply.reply(Ok(false.into_ipc()));
        }
        if let Err(message) = owned.commit(&mut || {
            self.with_view(view, |slot| slot.untitled_copy_target = None);
            Ok(())
        }) {
            return reply.reply(Err(crate::kit::error(message)));
        }
        let name = name
            .filter(|name| {
                name.to_ascii_lowercase().ends_with(".pdf") && name.encode_utf16().count() < 256
            })
            .map_or_else(
                || jscompat::node_path::host::basename(&path, None),
                str::to_owned,
            );
        let default_path = match self.rec.borrow().save_as_default_path(view, &path) {
            Some(path) => path,
            None => {
                let dir = match self.shell.new_document_dir(view) {
                    Some(dir) => dir,
                    None => match self.default_save_dir() {
                        Ok(dir) => dir,
                        Err(message) => return reply.reply(Err(crate::kit::error(message))),
                    },
                };
                jscompat::node_path::host::join(&[&dir, &name])
            }
        };
        let options = shell_contract::hosts::FileDialog {
            default_path: Some(default_path.into()),
            filters: vec![shell_contract::hosts::FileFilter {
                name: "PDF".into(),
                extensions: vec!["pdf".into()],
            }],
            ..Default::default()
        };
        let ed = self.rc();
        let check = owned.clone();
        let back = crate::kit::back_on_core(
            &self.services,
            move |picked: Result<Option<SaveDestination>, String>| {
                let picked = match picked {
                    Ok(Some(path)) => path,
                    Ok(None) => return reply.reply(Ok(false.into_ipc())),
                    Err(message) => return reply.reply(Err(crate::kit::error(message))),
                };
                let (target, selection) = match picked {
                    SaveDestination::Renderer(path) => {
                        let picked = path.to_string_lossy().into_owned();
                        if picked.is_empty() {
                            return reply.reply(Ok(false.into_ipc()));
                        }
                        (
                            if picked.to_ascii_lowercase().ends_with(".pdf") {
                                picked
                            } else {
                                format!("{picked}.pdf")
                            },
                            None,
                        )
                    }
                    SaveDestination::Native(selection) => match selection
                        .normalize_dialog_format(shell_contract::ipc::NativeSaveFormat::PdfUntitled)
                    {
                        Ok(selection) => (
                            selection.path().to_string_lossy().into_owned(),
                            Some(selection),
                        ),
                        Err(message) => return reply.reply(Err(crate::kit::error(message))),
                    },
                };
                if let Some(selection) = selection {
                    if let Err(message) = check.set_selection(selection) {
                        return reply.reply(Err(crate::kit::error(message)));
                    }
                }
                let destination = match check.participants() {
                    Ok(owners) => match owners.into_iter().next() {
                        Some(owner) => owner,
                        None => {
                            return reply
                                .reply(Err(crate::kit::error("PDF save destination unavailable")));
                        }
                    },
                    Err(message) => return reply.reply(Err(crate::kit::error(message))),
                };
                let result = check.commit(&mut || {
                    ed.with_view(view, |slot| {
                        slot.untitled_copy_target =
                            Some((path.clone(), target.clone(), Some(destination.clone())))
                    });
                    Ok(())
                });
                reply.reply(result.map(|()| true.into_ipc()).map_err(crate::kit::error));
            },
        );
        owned.save_dialog(&self.dialogs, self.contents.window_of(view), options, back);
    }
}

#[cfg(test)]
mod tests;
