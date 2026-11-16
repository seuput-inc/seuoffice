use shell_contract::IpcError;
use shell_contract::ipc::{
    CallContext, NativeExistingDocumentBinding, NativeRequestOwner, NativeSaveDestination,
    NativeSavedInput, SavedPublication,
};
use shell_contract::runtime::{CancelToken, RendererGeneration};

use crate::staged::StagedOutput;
use crate::value::thrown;

/// A committed publication and the exact bytes its descriptor wrote.
pub(crate) type PublishedOutput = (SavedPublication, NativeSavedInput);

#[derive(Clone)]
pub(crate) enum SaveAuthority {
    Renderer(NativeRequestOwner),
    Native(NativeExistingDocumentBinding),
}

/// A native selection must retain its operation-bound destination through commit.
pub(crate) enum SaveDestination {
    Renderer(std::path::PathBuf),
    Native(shell_contract::ipc::NativeSaveDestination),
}

impl SaveDestination {
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::Renderer(path) => path,
            Self::Native(destination) => destination.path(),
        }
    }
}

impl SaveAuthority {
    pub fn renderer(&self) -> Result<&NativeRequestOwner, IpcError> {
        match self {
            Self::Renderer(owner) => Ok(owner),
            Self::Native(_) => Err(thrown("This save is owned by a native operation")),
        }
    }

    pub fn loss_token(&self) -> Option<CancelToken> {
        match self {
            Self::Renderer(owner) => owner.loss_token(),
            Self::Native(binding) => Some(binding.operation().loss_token()),
        }
    }
}
/// Origin authority and the destination document are independent lifetimes.
#[derive(Clone)]
pub(crate) struct OwnedSave {
    pub authority: SaveAuthority,
    pub destination: NativeRequestOwner,
    pub cancel: CancelToken,
    pub document: RendererGeneration,
    pub pending: CancelToken,
    pub additional: Vec<CancelToken>,
    pub callback_owner: Option<NativeRequestOwner>,
    pub prior_callback_owners: Vec<NativeRequestOwner>,
    pub revision: Option<shell_contract::ipc::NativeSaveRevisionAuthority>,
    pub revision_snapshot: Option<shell_contract::ipc::NativeSaveRevisionSnapshot>,
    pub effects: shell_contract::hosts::OwnedEffectsHost,
}

impl OwnedSave {
    pub fn new(
        origin: NativeRequestOwner,
        mut destination: NativeRequestOwner,
        cancel: CancelToken,
        document: RendererGeneration,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Self, IpcError> {
        if destination
            .context
            .document_lease()
            .is_some_and(|captured| !captured.same_generation(&document))
        {
            return Err(thrown("The captured destination document was replaced"));
        }
        destination.context = destination.context.with_document_lease(document.clone());
        let request = Self {
            authority: SaveAuthority::Renderer(origin),
            destination,
            cancel,
            document,
            pending: CancelToken::default(),
            additional: Vec::new(),
            callback_owner: None,
            prior_callback_owners: Vec::new(),
            revision: None,
            revision_snapshot: None,
            effects,
        };
        request.check()?;
        Ok(request)
    }

    pub fn new_native(
        binding: NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        document: RendererGeneration,
        effects: shell_contract::hosts::OwnedEffectsHost,
    ) -> Result<Self, IpcError> {
        let destination = binding.owner().clone();
        if !destination
            .context
            .document_lease()
            .is_some_and(|captured| captured.same_generation(&document))
        {
            return Err(thrown("The captured destination document was replaced"));
        }
        let request = Self {
            authority: SaveAuthority::Native(binding),
            destination,
            cancel,
            document,
            pending,
            additional: Vec::new(),
            callback_owner: None,
            prior_callback_owners: Vec::new(),
            revision: None,
            revision_snapshot: None,
            effects,
        };
        request.check()?;
        Ok(request)
    }

    pub fn retain_native_selection(
        &mut self,
        selection: &shell_contract::ipc::NativeSaveDestination,
    ) -> Result<(), IpcError> {
        if !matches!(&self.authority, SaveAuthority::Native(binding) if selection.matches_binding(binding))
        {
            return Err(thrown("The selected destination belongs to another save"));
        }
        if self.revision.is_some() {
            return Err(thrown("The native save destination was already selected"));
        }
        self.effect(|| Ok(()))?;
        let revision = shell_contract::ipc::NativeSaveRevisionAuthority::new(
            selection.clone(),
            vec![self.destination.clone()],
        );
        self.revision_snapshot = Some(revision.snapshot().map_err(thrown)?);
        self.revision = Some(revision);
        Ok(())
    }

    pub fn apply_revision_receipt(
        &mut self,
        receipt: &shell_contract::ipc::NativeSaveRevisionReceipt,
    ) -> Result<(), IpcError> {
        let previous = self
            .revision_snapshot
            .as_ref()
            .ok_or_else(|| thrown("The native save revision is unavailable"))?;
        if !receipt.succeeds(previous) || !matches!(&self.authority, SaveAuthority::Native(_)) {
            return Err(thrown(
                "The native save revision does not follow this operation",
            ));
        }
        let renew = |owner: &NativeRequestOwner| {
            receipt
                .renewed_owner(owner)
                .cloned()
                .unwrap_or_else(|| owner.clone())
        };
        let destination = renew(&self.destination);
        let callback_owner = self.callback_owner.as_ref().map(&renew);
        let prior_callback_owners = self.prior_callback_owners.iter().map(&renew).collect();
        let snapshot = receipt.snapshot().clone();
        self.authority = SaveAuthority::Native(snapshot.selection().binding().clone());
        self.destination = destination;
        self.callback_owner = callback_owner;
        self.prior_callback_owners = prior_callback_owners;
        self.revision_snapshot = Some(snapshot);
        Ok(())
    }

    pub fn capture_callback(&self, cx: &CallContext) -> Result<Self, IpcError> {
        self.validate_sender(cx)?;
        let actual = NativeRequestOwner {
            view: cx.sender.view,
            context: cx
                .native
                .clone()
                .ok_or_else(|| thrown("The save callback has no native authority"))?,
        };
        let mut additional = self.additional.clone();
        additional.push(cx.cancel.clone());
        let refs = additional.iter().collect::<Vec<_>>();
        let participants = self.participants(&[]);
        let mut admitted = || Ok(());
        match &self.authority {
            SaveAuthority::Renderer(_) => self.effects.with_same_native_origin_effect(
                &self.destination,
                &actual,
                &participants,
                &self.cancel,
                &self.pending,
                &refs,
                &mut admitted,
            ),
            SaveAuthority::Native(binding) => match &self.revision_snapshot {
                Some(snapshot) => self.effects.with_native_save_revision_same_origin_effect(
                    snapshot,
                    &actual,
                    &participants,
                    &self.cancel,
                    &self.pending,
                    &refs,
                    &mut admitted,
                ),
                None => self.effects.with_existing_document_same_origin_effect(
                    binding,
                    &actual,
                    &participants,
                    &self.cancel,
                    &self.pending,
                    &refs,
                    &mut admitted,
                ),
            },
        }
        .map_err(thrown)?;
        let mut captured = self.clone();
        if let Some(previous) = captured.callback_owner.take() {
            captured.prior_callback_owners.push(previous);
        }
        captured.additional = additional;
        captured.callback_owner = Some(actual);
        Ok(captured)
    }

    pub fn participants(&self, participants: &[NativeRequestOwner]) -> Vec<NativeRequestOwner> {
        let mut targets = participants.to_vec();
        targets.push(self.destination.clone());
        targets.extend(self.prior_callback_owners.iter().cloned());
        if let Some(owner) = &self.callback_owner {
            targets.push(owner.clone());
        }
        targets
    }

    /// The destination a native save retained, if it has selected one.
    pub fn native_selection(&self) -> Option<&NativeSaveDestination> {
        self.revision_snapshot
            .as_ref()
            .map(shell_contract::ipc::NativeSaveRevisionSnapshot::selection)
    }

    /// Publishes a staged output on the core turn. A native authority validates
    /// inside the shell's publication admission; a renderer authority has no
    /// validating variant, so `validate` runs on the same core turn immediately
    /// before the admitted publication, with nothing able to interleave.
    pub fn publish_staged(
        &self,
        selection: Option<&NativeSaveDestination>,
        mut staged: StagedOutput,
        validate: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<PublishedOutput, IpcError> {
        self.check()?;
        let commit = staged.take()?;
        let path = commit.path().to_owned();
        let result = match (&self.authority, selection) {
            (SaveAuthority::Renderer(origin), None) => {
                if let Err(error) = validate() {
                    staged.restore(commit);
                    return Err(thrown(error));
                }
                let mut callbacks = self.prior_callback_owners.clone();
                callbacks.extend(self.callback_owner.clone());
                self.effects.publish_owned_staged_write(
                    origin,
                    &self.destination,
                    &callbacks,
                    &self.cancel,
                    &self.pending,
                    &self.additional,
                    &path,
                    commit,
                )
            }
            (SaveAuthority::Native(binding), Some(selection))
                if selection.matches_binding(binding) && selection.path() == path =>
            {
                self.effects.publish_native_staged_write_validated(
                    selection,
                    &self.participants(&[]),
                    &self.cancel,
                    &self.pending,
                    &self.additional,
                    commit,
                    validate,
                )
            }
            _ => {
                staged.restore(commit);
                return Err(thrown(
                    "The output destination does not match its save authority",
                ));
            }
        };
        staged.conclude(result)
    }

    /// Worker-side await. Native UI admission releases every gate before action.
    pub fn ui_effect(
        &self,
        action: Box<dyn FnOnce() -> Result<(), String> + Send>,
    ) -> Result<(), IpcError> {
        self.check()?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let done = Box::new(move |result| {
            let _ = tx.send(result);
        });
        let targets = self.participants(&[]);
        match &self.authority {
            SaveAuthority::Renderer(origin) => self.effects.post_ui_owned_many_extra(
                origin.clone(),
                targets,
                self.cancel.clone(),
                self.pending.clone(),
                self.additional.clone(),
                action,
                done,
            ),
            SaveAuthority::Native(binding) => match &self.revision_snapshot {
                Some(snapshot) => self.effects.post_ui_native_save_revision_snapshot(
                    snapshot.clone(),
                    targets,
                    self.cancel.clone(),
                    self.pending.clone(),
                    self.additional.clone(),
                    action,
                    done,
                ),
                None => self.effects.post_ui_existing_document_save_extra(
                    binding.clone(),
                    targets,
                    self.cancel.clone(),
                    self.pending.clone(),
                    self.additional.clone(),
                    action,
                    done,
                ),
            },
        }
        rx.recv()
            .map_err(|_| thrown("The owned native UI action did not complete"))?
            .map_err(thrown)?;
        self.check()
    }

    pub fn effect_many<T>(
        &self,
        participants: &[NativeRequestOwner],
        effect: impl FnOnce() -> Result<T, IpcError>,
    ) -> Result<T, IpcError> {
        let targets = self.participants(participants);
        let additional = self.additional.iter().collect::<Vec<_>>();
        self.effect_admitted(
            |run| match &self.authority {
                SaveAuthority::Renderer(origin) => self.effects.with_owned_effect_many_extra(
                    origin,
                    &targets,
                    &self.cancel,
                    &self.pending,
                    &additional,
                    run,
                ),
                SaveAuthority::Native(binding) => match &self.revision_snapshot {
                    Some(snapshot) => self.effects.with_native_save_revision_snapshot_effect(
                        snapshot,
                        &targets,
                        &self.cancel,
                        &self.pending,
                        &additional,
                        run,
                    ),
                    None => self.effects.with_existing_document_save_effect_extra(
                        binding,
                        &targets,
                        &self.cancel,
                        &self.pending,
                        &additional,
                        run,
                    ),
                },
            },
            effect,
        )
    }

    pub fn effect_destination<T>(
        &self,
        destination: &shell_contract::ipc::NativeSaveDestination,
        path: &std::path::Path,
        effect: impl FnOnce() -> Result<T, IpcError>,
    ) -> Result<T, IpcError> {
        if !matches!(&self.authority, SaveAuthority::Native(binding) if destination.matches_binding(binding))
        {
            return Err(thrown("The selected destination belongs to another save"));
        }
        let targets = self.participants(&[]);
        let additional = self.additional.iter().collect::<Vec<_>>();
        match &self.revision_snapshot {
            Some(snapshot) => {
                if snapshot.selection().path() != path || destination.path() != path {
                    return Err(thrown("The selected save destination changed"));
                }
                self.effect_admitted(
                    |run| {
                        self.effects.with_native_save_revision_snapshot_effect(
                            snapshot,
                            &targets,
                            &self.cancel,
                            &self.pending,
                            &additional,
                            run,
                        )
                    },
                    effect,
                )
            }
            None => self.effect_admitted(
                |run| {
                    self.effects.with_existing_document_destination_effect(
                        destination,
                        path,
                        &targets,
                        &self.cancel,
                        &self.pending,
                        &additional,
                        run,
                    )
                },
                effect,
            ),
        }
    }

    pub fn effect_invocation<T>(
        &self,
        document: &shell_contract::ipc::NativeInvocationDocument,
        participants: &[NativeRequestOwner],
        effect: impl FnOnce() -> Result<T, IpcError>,
    ) -> Result<T, IpcError> {
        let targets = self.participants(participants);
        let additional = self.additional.iter().collect::<Vec<_>>();
        let origin = self.authority.renderer()?;
        self.effect_admitted(
            |run| {
                self.effects.with_native_invocation_effect_extra(
                    document,
                    origin,
                    &targets,
                    &self.cancel,
                    &self.pending,
                    &additional,
                    run,
                )
            },
            effect,
        )
    }

    fn effect_admitted<T>(
        &self,
        admit: impl FnOnce(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
        effect: impl FnOnce() -> Result<T, IpcError>,
    ) -> Result<T, IpcError> {
        let mut effect = Some(effect);
        let mut output = None;
        let mut error = None;
        let admitted = admit(&mut || {
            self.check().map_err(|error| error.message)?;
            let run = effect
                .take()
                .ok_or_else(|| "The save effect was already committed".to_owned())?;
            match run() {
                Ok(value) => {
                    output = Some(value);
                    Ok(())
                }
                Err(failure) => {
                    let message = failure.message.clone();
                    error = Some(failure);
                    Err(message)
                }
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
        admitted.map_err(thrown)?;
        output.ok_or_else(|| thrown("The native save effect did not run"))
    }

    pub fn effect<T>(&self, effect: impl FnOnce() -> Result<T, IpcError>) -> Result<T, IpcError> {
        self.effect_many(&[], effect)
    }

    pub fn emit(
        &self,
        destination: &NativeRequestOwner,
        channel: &str,
        args: Vec<shell_contract::IpcValue>,
    ) -> Result<(), IpcError> {
        self.emit_many(std::slice::from_ref(destination), 0, channel, args)
    }

    pub fn emit_many(
        &self,
        participants: &[NativeRequestOwner],
        target: usize,
        channel: &str,
        args: Vec<shell_contract::IpcValue>,
    ) -> Result<(), IpcError> {
        let targets = self.participants(participants);
        let additional = self.additional.iter().collect::<Vec<_>>();
        match &self.authority {
            SaveAuthority::Renderer(origin) => self.effects.emit_owned_many_extra(
                origin,
                &targets,
                target,
                &self.cancel,
                &self.pending,
                &additional,
                channel,
                args,
            ),
            SaveAuthority::Native(binding) => match &self.revision_snapshot {
                Some(snapshot) => self.effects.emit_native_save_revision_snapshot(
                    snapshot,
                    &targets,
                    target,
                    &self.cancel,
                    &self.pending,
                    &additional,
                    channel,
                    args,
                ),
                None => self.effects.emit_existing_document_save_many_extra(
                    binding,
                    &targets,
                    target,
                    &self.cancel,
                    &self.pending,
                    &additional,
                    channel,
                    args,
                ),
            },
        }
        .map_err(thrown)
    }

    pub fn save_dialog(
        &self,
        dialogs: &shell_contract::DialogsHost,
        parent: Option<shell_contract::WindowId>,
        options: shell_contract::hosts::FileDialog,
        done: Box<dyn FnOnce(Result<Option<SaveDestination>, String>) + Send>,
    ) {
        match &self.authority {
            SaveAuthority::Renderer(origin) => dialogs.save_owned_many_extra(
                parent,
                options,
                origin.clone(),
                self.participants(&[]),
                self.cancel.clone(),
                self.pending.clone(),
                self.additional.clone(),
                Box::new(move |result| {
                    done(result.map(|path| path.map(SaveDestination::Renderer)))
                }),
            ),
            SaveAuthority::Native(binding) => {
                let done = Box::new(
                    move |result: Result<
                        Option<shell_contract::ipc::NativeSaveDestination>,
                        String,
                    >| {
                        done(result.map(|path| path.map(SaveDestination::Native)))
                    },
                );
                match &self.revision_snapshot {
                    Some(snapshot) => dialogs.save_existing_document_revision(
                        parent,
                        options,
                        snapshot.clone(),
                        self.participants(&[]),
                        self.cancel.clone(),
                        self.pending.clone(),
                        self.additional.clone(),
                        done,
                    ),
                    None => dialogs.save_existing_document(
                        parent,
                        options,
                        binding.clone(),
                        self.participants(&[]),
                        self.cancel.clone(),
                        self.pending.clone(),
                        self.additional.clone(),
                        done,
                    ),
                }
            }
        }
    }

    pub fn message_dialog(
        &self,
        dialogs: &shell_contract::DialogsHost,
        parent: Option<shell_contract::WindowId>,
        options: shell_contract::hosts::MessageBox,
        done: Box<dyn FnOnce(Result<shell_contract::hosts::MessageBoxAnswer, String>) + Send>,
    ) {
        match &self.authority {
            SaveAuthority::Renderer(origin) => dialogs.message_owned_many_extra(
                parent,
                options,
                origin.clone(),
                self.participants(&[]),
                self.cancel.clone(),
                self.pending.clone(),
                self.additional.clone(),
                done,
            ),
            SaveAuthority::Native(binding) => match &self.revision_snapshot {
                Some(snapshot) => dialogs.message_existing_document_revision(
                    parent,
                    options,
                    snapshot.clone(),
                    self.participants(&[]),
                    self.cancel.clone(),
                    self.pending.clone(),
                    self.additional.clone(),
                    done,
                ),
                None => dialogs.message_existing_document(
                    parent,
                    options,
                    binding.clone(),
                    self.participants(&[]),
                    self.cancel.clone(),
                    self.pending.clone(),
                    self.additional.clone(),
                    done,
                ),
            },
        }
    }

    pub fn check(&self) -> Result<(), IpcError> {
        if self.cancel.is_cancelled()
            || self.pending.is_cancelled()
            || self.additional.iter().any(CancelToken::is_cancelled)
        {
            return Err(thrown("The save request was cancelled"));
        }
        if !self.document.is_active() {
            return Err(thrown("The destination document was replaced"));
        }
        if let SaveAuthority::Native(binding) = &self.authority {
            let operation = binding.operation();
            if operation.loss_token().is_cancelled()
                || operation.cancel().is_cancelled()
                || operation.shutdown().is_cancelled()
            {
                return Err(thrown("The native save operation was released"));
            }
        }
        let renderer_origin = match &self.authority {
            SaveAuthority::Renderer(owner) => Some(owner),
            SaveAuthority::Native(_) => None,
        };
        for owner in renderer_origin
            .into_iter()
            .chain(std::iter::once(&self.destination))
        {
            let generation = owner
                .context
                .renderer()
                .ok_or_else(|| thrown("The save has no captured native renderer"))?;
            let loss = owner
                .loss_token()
                .ok_or_else(|| thrown("The native save owner lifetime is unavailable"))?;
            if !generation.is_active() || loss.is_cancelled() {
                return Err(thrown("The native save owner was released"));
            }
        }
        Ok(())
    }

    pub fn validate_sender(&self, cx: &CallContext) -> Result<(), IpcError> {
        self.check()?;
        let actual = cx.native.as_ref().and_then(|native| native.renderer());
        let expected = self.destination.context.renderer();
        if cx.sender.view != self.destination.view
            || !actual
                .zip(expected)
                .is_some_and(|(actual, expected)| actual.same_generation(expected))
        {
            return Err(thrown("The save response belongs to another renderer"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::hosts::OwnedEffects;
    use std::sync::Arc;

    #[derive(Default)]
    struct Effects {
        emitted: std::sync::Mutex<Vec<(String, Vec<shell_contract::IpcValue>)>>,
    }
    impl OwnedEffects for Effects {
        fn capture_owner(
            &self,
            view: shell_contract::ViewId,
            expected: &RendererGeneration,
        ) -> Result<NativeRequestOwner, String> {
            Ok(owner(view.0, expected.clone()))
        }
        fn with_exact_browser_owner_effect(
            &self,
            _: &shell_contract::ipc::NativeBrowserOwner,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Browser ownership is not scripted in the slides fixture".into())
        }
        fn docs_document_identity(
            &self,
            _owner: &NativeRequestOwner,
        ) -> Option<(shell_contract::ViewId, u64)> {
            None
        }

        fn with_native_command_preparation_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native command preparation is not scripted in the slides fixture".into())
        }
        fn with_startup_command_creation_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &NativeRequestOwner,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &RendererGeneration,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Startup command creation is not scripted in the slides fixture".into())
        }
        fn with_startup_command_target_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &NativeRequestOwner,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &shell_contract::ipc::NativeInvocationDocument,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Startup command target is not scripted in the slides fixture".into())
        }
        fn with_startup_command_preparation_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &NativeRequestOwner,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Startup command preparation is not scripted in the slides fixture".into())
        }
        fn with_startup_recovery_creation_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &NativeRequestOwner,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &RendererGeneration,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Startup recovery creation is not scripted in the slides fixture".into())
        }
        fn with_startup_recovery_seed_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &NativeRequestOwner,
            _: &shell_contract::ipc::NativeInvocationTarget,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Startup recovery seed is not scripted in the slides fixture".into())
        }
        fn with_native_command_creation_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &RendererGeneration,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native command creation is not scripted in the slides fixture".into())
        }
        fn with_native_command_target_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &shell_contract::ipc::NativeInvocationDocument,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native command target is not scripted in the slides fixture".into())
        }
        fn settle_recovery_seed_snapshot(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery settlement is not scripted in the slides fixture".into())
        }
        fn emit_recovery_open_snapshot(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Recovery events are not scripted in the slides fixture".into())
        }
        fn with_recovery_dialog_snapshot(
            &self,
            _: &shell_contract::ipc::RecoveryOpenTarget,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &NativeRequestOwner,
            _: &RendererGeneration,
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery dialog is not scripted in the slides fixture".into())
        }
        fn emit_recovery_dialog_snapshot(
            &self,
            _: &shell_contract::ipc::RecoveryOpenTarget,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &NativeRequestOwner,
            _: &RendererGeneration,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Recovery dialog events are not scripted in the slides fixture".into())
        }
        fn post_ui_existing_document_save(
            &self,
            _: shell_contract::ipc::NativeExistingDocumentBinding,
            _: Vec<NativeRequestOwner>,
            _: CancelToken,
            _: CancelToken,
            _: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            done(Err(
                "Native existing-document save is not scripted in the slides fixture".into(),
            ));
        }
        fn with_existing_document_save_effect(
            &self,
            _: &shell_contract::ipc::NativeExistingDocumentBinding,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native existing-document save is not scripted in the slides fixture".into())
        }
        fn with_existing_document_save_effect_extra(
            &self,
            binding: &shell_contract::ipc::NativeExistingDocumentBinding,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            additional: &[&CancelToken],
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_existing_document_save_extra(
                binding,
                participants,
                cancel,
                pending,
                additional,
                effect,
            )
        }
        fn with_existing_document_same_origin_effect(
            &self,
            _: &shell_contract::ipc::NativeExistingDocumentBinding,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native existing-document save is not scripted in the slides fixture".into())
        }
        fn emit_existing_document_save_many_extra(
            &self,
            _: &shell_contract::ipc::NativeExistingDocumentBinding,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Native existing-document save is not scripted in the slides fixture".into())
        }
        fn post_ui_existing_document_save_extra(
            &self,
            _: shell_contract::ipc::NativeExistingDocumentBinding,
            _: Vec<NativeRequestOwner>,
            _: CancelToken,
            _: CancelToken,
            _: Vec<CancelToken>,
            _: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            done(Err(
                "Native existing-document save is not scripted in the slides fixture".into(),
            ));
        }
        fn emit_existing_document_save_many(
            &self,
            _: &shell_contract::ipc::NativeExistingDocumentBinding,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err(
                "Native existing-document save events are not scripted in the slides fixture"
                    .into(),
            )
        }
        fn with_recovery_seed_snapshot(
            &self,
            _: &shell_contract::ipc::RecoveryOpenTarget,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery seed is not scripted in the slides fixture".into())
        }
        fn commit_recovery_seed_snapshot(
            &self,
            _: &shell_contract::ipc::RecoveryOpenTarget,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<shell_contract::ipc::RecoverySeedReceipt, String> {
            Err("Recovery seed is not scripted in the slides fixture".into())
        }
        fn with_recovery_open_snapshot(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &shell_contract::ipc::RecoveryAuthoritySnapshot,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery open is not scripted in the slides fixture".into())
        }
        fn with_command_recovery_target_effect(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeInvocationDocument,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Command recovery is not scripted in the slides fixture".into())
        }
        fn with_recovery_pdf_revision_transition(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &shell_contract::ipc::PdfDocumentLease,
            _: &shell_contract::ipc::PdfDocumentLease,
            _: &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal,
            _: &mut dyn FnMut(),
            _: &mut dyn FnMut(
                &mut shell_contract::ipc::AdmittedPdfRevisionRetirement<'_, '_>,
                &mut shell_contract::ipc::PreparedRecoveryAuthorityRenewal,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery PDF revision transition is not scripted in the slides fixture".into())
        }
        fn with_recovery_open_effect_many_extra(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery open is not scripted in the slides fixture".into())
        }
        fn with_recovery_open_effect_many(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Recovery open is not scripted in the slides fixture".into())
        }
        fn emit_native_command_creation_many(
            &self,
            _: &shell_contract::ipc::NativeInvocationOperation,
            _: &shell_contract::ipc::NativeEditorCommand,
            _: &RendererGeneration,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Native command creation events are not scripted in the slides fixture".into())
        }
        fn emit_recovery_open_many(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: usize,
            _: &CancelToken,
            _: &CancelToken,
            _: &str,
            _: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            Err("Recovery events are not scripted in the slides fixture".into())
        }
        fn stage_docs_media(
            &self,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &dyn shell_contract::hosts::DocsMediaStage,
            _: Option<&shell_contract::ipc::RecoverySeedReceipt>,
        ) -> Result<(), String> {
            Err("Docs media staging is not scripted in the slides fixture".into())
        }
        fn with_startup_recovery_effect(
            &self,
            startup: &shell_contract::ipc::NativeInvocationOperation,
            shell: &NativeRequestOwner,
            child: &shell_contract::ipc::NativeInvocationDocument,
            owner: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_startup_recovery(
                startup,
                shell,
                child,
                owner,
                participants,
                cancel,
                pending,
                effect,
            )
        }
        fn emit_startup_recovery_many(
            &self,
            startup: &shell_contract::ipc::NativeInvocationOperation,
            shell: &NativeRequestOwner,
            child: &shell_contract::ipc::NativeInvocationDocument,
            owner: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            target: usize,
            cancel: &CancelToken,
            pending: &CancelToken,
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            participants
                .get(target)
                .ok_or("synthetic event target unavailable")?;
            let mut event = Some((channel.to_owned(), args));
            shell_contract::hosts::synthetic::with_startup_recovery(
                startup,
                shell,
                child,
                owner,
                participants,
                cancel,
                pending,
                &mut || {
                    self.emitted
                        .lock()
                        .unwrap()
                        .push(event.take().ok_or("event already emitted")?);
                    Ok(())
                },
            )
        }
        fn transition_docs_media(
            &self,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &dyn shell_contract::hosts::DocsMediaTransition,
            _: Option<&shell_contract::ipc::RecoverySeedReceipt>,
            _: &mut dyn FnMut(
                &NativeRequestOwner,
                &mut dyn FnMut() -> Result<(), String>,
            ) -> Result<(), String>,
        ) -> Result<NativeRequestOwner, String> {
            Err("Docs media transition is not scripted in the slides fixture".into())
        }
        fn with_native_operation_effect(
            &self,
            operation: &shell_contract::ipc::NativeInvocationOperation,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_native_operation(
                operation,
                origin,
                participants,
                cancel,
                pending,
                effect,
            )
        }
        fn emit_native_operation_many(
            &self,
            operation: &shell_contract::ipc::NativeInvocationOperation,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            target: usize,
            cancel: &CancelToken,
            pending: &CancelToken,
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            participants
                .get(target)
                .ok_or("synthetic event target unavailable")?;
            let mut event = Some((channel.to_owned(), args));
            shell_contract::hosts::synthetic::with_native_operation(
                operation,
                origin,
                participants,
                cancel,
                pending,
                &mut || {
                    self.emitted
                        .lock()
                        .unwrap()
                        .push(event.take().ok_or("event already emitted")?);
                    Ok(())
                },
            )
        }
        fn with_native_invocation_effect(
            &self,
            document: &shell_contract::ipc::NativeInvocationDocument,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_native_invocation(
                document,
                origin,
                participants,
                cancel,
                pending,
                effect,
            )
        }
        fn emit_native_invocation_many(
            &self,
            document: &shell_contract::ipc::NativeInvocationDocument,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            target: usize,
            cancel: &CancelToken,
            pending: &CancelToken,
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            participants
                .get(target)
                .ok_or("synthetic event target unavailable")?;
            let mut event = Some((channel.to_owned(), args));
            shell_contract::hosts::synthetic::with_native_invocation(
                document,
                origin,
                participants,
                cancel,
                pending,
                &mut || {
                    self.emitted
                        .lock()
                        .unwrap()
                        .push(event.take().ok_or("event already emitted")?);
                    Ok(())
                },
            )
        }
        fn with_same_native_origin_effect(
            &self,
            expected: &NativeRequestOwner,
            actual: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            additional: &[&CancelToken],
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_same_native_origin(
                expected,
                actual,
                participants,
                cancel,
                pending,
                additional,
                effect,
            )
        }
        fn with_recovery_dialog_effect(
            &self,
            target: &shell_contract::ipc::RecoveryOpenTarget,
            dialog: &NativeRequestOwner,
            generation: &RendererGeneration,
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_recovery_dialog(
                target, dialog, generation, cancel, pending, effect,
            )
        }
        fn emit_recovery_dialog(
            &self,
            target: &shell_contract::ipc::RecoveryOpenTarget,
            dialog: &NativeRequestOwner,
            generation: &RendererGeneration,
            cancel: &CancelToken,
            pending: &CancelToken,
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            let mut event = Some((channel.to_owned(), args));
            shell_contract::hosts::synthetic::with_recovery_dialog(
                target,
                dialog,
                generation,
                cancel,
                pending,
                &mut || {
                    self.emitted
                        .lock()
                        .unwrap()
                        .push(event.take().ok_or("event already emitted")?);
                    Ok(())
                },
            )
        }
        fn with_recovery_seed_effect(
            &self,
            target: &shell_contract::ipc::RecoveryOpenTarget,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_recovery_seed(target, effect)
        }
        fn commit_recovery_seed(
            &self,
            target: &shell_contract::ipc::RecoveryOpenTarget,
            enqueue: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<shell_contract::ipc::RecoverySeedReceipt, String> {
            shell_contract::hosts::synthetic::commit_recovery_seed(target, enqueue)
        }
        fn with_recovery_open_effect_pending(
            &self,
            _: &shell_contract::ipc::RecoverySeedReceipt,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Pending recovery admission is not scripted in the slides fixture".into())
        }
        fn with_recovery_open_effect(
            &self,
            receipt: &shell_contract::ipc::RecoverySeedReceipt,
            receiver: &NativeRequestOwner,
            cancel: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_recovery_open(receipt, receiver, cancel, effect)
        }
        fn settle_recovery_seed(
            &self,
            receipt: &shell_contract::ipc::RecoverySeedReceipt,
            receiver: &NativeRequestOwner,
            cancel: &CancelToken,
            result: Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::settle_recovery_seed(
                receipt, receiver, cancel, result,
            )
        }

        fn with_created_many_effect(
            &self,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &RendererGeneration,
            _: &CancelToken,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Multi-owner creation is not scripted in the slides fixture".into())
        }
        fn with_created_effect(
            &self,
            source: &NativeRequestOwner,
            created: &RendererGeneration,
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            self.with_owned_effect(source, &owner(3, created.clone()), cancel, pending, effect)
        }

        fn emit_owned(
            &self,
            origin: &NativeRequestOwner,
            destination: &NativeRequestOwner,
            cancel: &CancelToken,
            pending: &CancelToken,
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            let mut args = Some(args);
            self.with_owned_effect(origin, destination, cancel, pending, &mut || {
                let args = args
                    .take()
                    .ok_or_else(|| "event already emitted".to_owned())?;
                self.emitted
                    .lock()
                    .unwrap()
                    .push((channel.to_owned(), args));
                Ok(())
            })
        }

        fn with_owned_effect(
            &self,
            origin: &NativeRequestOwner,
            destination: &NativeRequestOwner,
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            let generations = [
                origin.context.renderer(),
                destination.context.renderer(),
                origin.context.document_lease(),
                destination.context.document_lease(),
            ];
            RendererGeneration::with_active_all(generations, || {
                CancelToken::with_active_all([Some(cancel), Some(pending)], effect)
                    .ok_or_else(|| "cancelled".to_owned())?
            })
            .ok_or_else(|| "released".to_owned())?
        }

        fn with_owned_effect_many_extra(
            &self,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            additional: &[&CancelToken],
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_owned_effect_many_extra(
                origin,
                participants,
                cancel,
                pending,
                additional,
                effect,
            )
        }
        fn emit_owned_many_extra(
            &self,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            target: usize,
            cancel: &CancelToken,
            pending: &CancelToken,
            additional: &[&CancelToken],
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            participants
                .get(target)
                .ok_or("synthetic event target unavailable")?;
            let mut event = Some((channel.to_owned(), args));
            self.with_owned_effect_many_extra(
                origin,
                participants,
                cancel,
                pending,
                additional,
                &mut || {
                    self.emitted
                        .lock()
                        .unwrap()
                        .push(event.take().ok_or("event already emitted")?);
                    Ok(())
                },
            )
        }
        fn post_ui_owned_many_extra(
            &self,
            origin: NativeRequestOwner,
            participants: Vec<NativeRequestOwner>,
            cancel: CancelToken,
            pending: CancelToken,
            additional: Vec<CancelToken>,
            action: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            let admitted = self.with_owned_effect_many_extra(
                &origin,
                &participants,
                &cancel,
                &pending,
                &additional.iter().collect::<Vec<_>>(),
                &mut || Ok(()),
            );
            done(admitted.and_then(|()| action()));
        }
        fn with_native_invocation_effect_extra(
            &self,
            document: &shell_contract::ipc::NativeInvocationDocument,
            origin: &NativeRequestOwner,
            participants: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            additional: &[&CancelToken],
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            shell_contract::hosts::synthetic::with_native_invocation_extra(
                document,
                origin,
                participants,
                cancel,
                pending,
                additional,
                effect,
            )
        }
        fn with_saved_reload_acceptance_effect(
            &self,
            _: &shell_contract::ipc::NativeSavedInput,
            _: &shell_contract::ipc::NativeSavedPath,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload is not scripted in the slides fixture".into())
        }
        fn with_native_save_reload_parser_effect(
            &self,
            _: &shell_contract::hosts::NativeSaveReloadHandoff,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload is not scripted in the slides fixture".into())
        }
        fn with_native_save_reload_reader_effect(
            &self,
            _: &shell_contract::hosts::NativeSaveReloadReceipt,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload is not scripted in the slides fixture".into())
        }
        fn with_native_save_reload_effect(
            &self,
            _: &shell_contract::hosts::NativeSaveReloadReceipt,
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload is not scripted in the slides fixture".into())
        }
        fn with_native_save_reload_many_effect_extra(
            &self,
            _: &shell_contract::hosts::NativeSaveReloadReceipt,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload is not scripted in the slides fixture".into())
        }
        fn stage_saved_docs_media(
            &self,
            _: &shell_contract::ipc::NativeSavedDocument,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &dyn shell_contract::hosts::DocsMediaStage,
        ) -> Result<(), String> {
            Err("Saved Docs media is not scripted in the slides fixture".into())
        }
        fn post_ui_native_saved_document_input(
            &self,
            _: shell_contract::ipc::NativeSavedDocument,
            _: Vec<NativeRequestOwner>,
            _: CancelToken,
            _: CancelToken,
            _: Vec<CancelToken>,
            _: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            done(Err(
                "Saved Docs UI is not scripted in the slides fixture".into()
            ));
        }
        fn with_prepared_saved_publication(
            &self,
            _: &shell_contract::ipc::SavedPublication,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut(
                &mut dyn FnMut(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String>,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Saved publication is not scripted in the slides fixture".into())
        }
        fn transition_saved_docs_media(
            &self,
            _: &shell_contract::ipc::NativeSavedDocument,
            _: &NativeRequestOwner,
            _: &CancelToken,
            _: &NativeRequestOwner,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &dyn shell_contract::hosts::DocsMediaTransition,
            _: &mut dyn FnMut(
                &NativeRequestOwner,
                &mut dyn FnMut() -> Result<(), String>,
            ) -> Result<(), String>,
            _: &mut dyn FnMut(
                &shell_contract::ipc::NativeSavedDocument,
                &NativeRequestOwner,
            ) -> Result<(), String>,
        ) -> Result<(shell_contract::ipc::NativeSavedDocument, NativeRequestOwner), String>
        {
            Err("Saved Docs media is not scripted in the slides fixture".into())
        }
        fn with_native_save_reload_completion_effect(
            &self,
            _: &shell_contract::hosts::NativeSaveReloadHandoff,
            _: &NativeRequestOwner,
            _: &mut dyn FnMut(),
            _: &mut dyn FnMut(
                &mut shell_contract::ipc::AdmittedPdfReloadRetirement<'_, '_>,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save reload is not scripted in the slides fixture".into())
        }
        fn with_native_save_revision_transition(
            &self,
            _: &mut shell_contract::ipc::PreparedNativeSaveRevision,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut(),
            _: &mut dyn FnMut(
                &mut shell_contract::ipc::AdmittedPdfRevisionSet<'_, '_>,
                &mut shell_contract::ipc::PreparedNativeSaveRevision,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native save revision is not scripted in the slides fixture".into())
        }
        fn with_native_saved_target_seed(
            &self,
            _: &shell_contract::ipc::NativeSavedTarget,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native saved target is not scripted in the slides fixture".into())
        }
        fn with_native_saved_document_input(
            &self,
            _: &shell_contract::ipc::NativeSavedDocument,
            _: &[NativeRequestOwner],
            _: &CancelToken,
            _: &CancelToken,
            _: &[&CancelToken],
            _: &mut dyn FnMut(&shell_contract::ipc::NativeSavedInput) -> Result<(), String>,
        ) -> Result<(), String> {
            Err("Native saved document is not scripted in the slides fixture".into())
        }
        fn emit_native_save_revision_snapshot(
            &self,
            snapshot: &shell_contract::ipc::NativeSaveRevisionSnapshot,
            participants: &[NativeRequestOwner],
            target: usize,
            cancel: &CancelToken,
            pending: &CancelToken,
            additional: &[&CancelToken],
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            participants
                .get(target)
                .ok_or("synthetic event target unavailable")?;
            let mut event = Some((channel.to_owned(), args));
            self.with_native_save_revision_snapshot_effect(
                snapshot,
                participants,
                cancel,
                pending,
                additional,
                &mut || {
                    self.emitted
                        .lock()
                        .unwrap()
                        .push(event.take().ok_or("event already emitted")?);
                    Ok(())
                },
            )
        }
        fn post_ui_native_save_revision_snapshot(
            &self,
            snapshot: shell_contract::ipc::NativeSaveRevisionSnapshot,
            participants: Vec<NativeRequestOwner>,
            cancel: CancelToken,
            pending: CancelToken,
            additional: Vec<CancelToken>,
            action: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            let admitted = self.with_native_save_revision_snapshot_effect(
                &snapshot,
                &participants,
                &cancel,
                &pending,
                &additional.iter().collect::<Vec<_>>(),
                &mut || Ok(()),
            );
            done(admitted.and_then(|()| action()));
        }
        fn with_owned_effect_many(
            &self,
            origin: &NativeRequestOwner,
            destinations: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut() -> Result<(), String>,
        ) -> Result<(), String> {
            self.with_owned_transition_many(origin, destinations, cancel, pending, &mut |_| {
                effect()
            })
        }
        fn post_ui_recovery_open_many(
            &self,
            receipt: shell_contract::ipc::RecoverySeedReceipt,
            receiver: NativeRequestOwner,
            participants: Vec<NativeRequestOwner>,
            cancel: CancelToken,
            pending: CancelToken,
            action: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            let admitted = shell_contract::hosts::synthetic::with_recovery_open_many(
                &receipt,
                &receiver,
                &participants,
                &cancel,
                &pending,
                &mut || Ok(()),
            );
            done(admitted.and_then(|()| action()));
        }
        fn post_ui_owned_many(
            &self,
            origin: NativeRequestOwner,
            destinations: Vec<NativeRequestOwner>,
            cancel: CancelToken,
            pending: CancelToken,
            action: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            let admitted =
                self.with_owned_effect_many(&origin, &destinations, &cancel, &pending, &mut || {
                    Ok(())
                });
            done(admitted.and_then(|()| action()));
        }

        fn with_owned_transition(
            &self,
            origin: &NativeRequestOwner,
            destination: &NativeRequestOwner,
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut(
                &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            self.with_owned_transition_many(
                origin,
                std::slice::from_ref(destination),
                cancel,
                pending,
                effect,
            )
        }
        fn with_owned_transition_many(
            &self,
            origin: &NativeRequestOwner,
            destinations: &[NativeRequestOwner],
            cancel: &CancelToken,
            pending: &CancelToken,
            effect: &mut dyn FnMut(
                &mut shell_contract::runtime::AdmittedDocumentRetirement<'_>,
            ) -> Result<(), String>,
        ) -> Result<(), String> {
            let count = destinations.len() + 1;
            let mut generations = Vec::with_capacity(count * 2);
            let mut documents = Vec::with_capacity(count);
            let mut pdf = Vec::with_capacity(count * 2);
            let mut losses = Vec::with_capacity(count);
            for owner in std::iter::once(origin).chain(destinations) {
                generations.push(
                    owner
                        .context
                        .renderer()
                        .ok_or("synthetic renderer unavailable")?,
                );
                if let Some(document) = owner.context.document_lease() {
                    generations.push(document);
                    documents.push(document);
                }
                if let Some(document) = owner.pdf_document() {
                    pdf.push(document);
                }
                if let Some(revision) = owner.pdf_revision() {
                    pdf.push(revision);
                }
                losses.push(
                    owner
                        .loss_token()
                        .ok_or("synthetic owner loss unavailable")?,
                );
            }
            let mut tokens = Vec::with_capacity(losses.len() + 2);
            tokens.push(cancel);
            tokens.push(pending);
            tokens.extend(losses.iter());
            let mut generations =
                shell_contract::runtime::RendererGenerationGatePlan::from_generations(generations);
            let mut retirement = generations.prepare_retirement(&documents)?;
            let mut pdf = shell_contract::ipc::PdfDocumentGatePlan::new(&pdf);
            let mut tokens = shell_contract::runtime::CancelTokenGatePlan::from_tokens(tokens);
            generations
                .with_active_retiring(&mut retirement, |admitted| {
                    pdf.with_live(|| tokens.with_active(|| effect(admitted)))
                })
                .flatten()
                .flatten()
                .ok_or_else(|| "synthetic document ownership released".to_owned())?
        }
        fn emit_owned_many(
            &self,
            origin: &NativeRequestOwner,
            destinations: &[NativeRequestOwner],
            target: usize,
            cancel: &CancelToken,
            pending: &CancelToken,
            channel: &str,
            args: Vec<shell_contract::IpcValue>,
        ) -> Result<(), String> {
            destinations
                .get(target)
                .ok_or("synthetic event target unavailable")?;
            let mut event = Some((channel.to_owned(), args));
            self.with_owned_transition_many(origin, destinations, cancel, pending, &mut |_| {
                self.emitted
                    .lock()
                    .unwrap()
                    .push(event.take().ok_or("event already emitted")?);
                Ok(())
            })
        }
        fn post_ui_owned(
            &self,
            origin: NativeRequestOwner,
            destination: NativeRequestOwner,
            cancel: CancelToken,
            pending: CancelToken,
            action: Box<dyn FnOnce() -> Result<(), String> + Send>,
            done: Box<dyn FnOnce(Result<(), String>) + Send>,
        ) {
            let admitted =
                self.with_owned_effect(&origin, &destination, &cancel, &pending, &mut || Ok(()));
            done(admitted.and_then(|()| action()));
        }
    }

    fn owner(view: u32, generation: RendererGeneration) -> NativeRequestOwner {
        NativeRequestOwner {
            view: shell_contract::ViewId(view),
            context: shell_contract::ipc::NativeContext::with_owner_loss(
                (),
                generation.clone(),
                generation.loss_token(),
            ),
        }
    }

    fn request() -> OwnedSave {
        OwnedSave::new(
            owner(1, RendererGeneration::new()),
            owner(2, RendererGeneration::new()),
            CancelToken::default(),
            RendererGeneration::new(),
            Arc::new(Effects::default()),
        )
        .unwrap()
    }

    #[test]
    fn origin_destination_document_and_cancellation_independently_prevent_commit() {
        for lost in 0..5 {
            let scope = request();
            match lost {
                0 => scope
                    .authority
                    .renderer()
                    .unwrap()
                    .context
                    .renderer()
                    .unwrap()
                    .revoke(),
                1 => scope.destination.context.renderer().unwrap().revoke(),
                2 => scope.document.revoke(),
                3 => scope.cancel.cancel(),
                _ => scope.pending.cancel(),
            }
            let mut writes = 0;
            assert!(
                scope
                    .effect(|| {
                        writes += 1;
                        Ok(())
                    })
                    .is_err()
            );
            assert_eq!(writes, 0);
        }
    }

    #[test]
    fn callback_cancellation_blocks_a_later_save_effect_without_cancelling_the_request() {
        let scope = request();
        let callback = CancelToken::default();
        let cx = CallContext {
            sender: shell_contract::ipc::Sender {
                view: scope.destination.view,
                window: shell_contract::WindowId(1),
                surface: shell_contract::Surface::Sheets,
            },
            request: None,
            cancel: callback.clone(),
            native: Some(scope.destination.context.clone()),
        };
        let captured = scope.capture_callback(&cx).unwrap();
        callback.cancel();
        let mut writes = 0;
        assert!(
            captured
                .effect(|| {
                    writes += 1;
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(writes, 0);
        assert!(!scope.pending.is_cancelled());
    }

    #[test]
    fn native_commit_preserves_effect_result_and_original_error() {
        let scope = request();
        assert_eq!(scope.effect(|| Ok(17)).unwrap(), 17);
        let failure = IpcError {
            name: Some("RangeError".into()),
            message: "original failure".into(),
        };
        assert_eq!(
            scope.effect::<()>(|| Err(failure.clone())).unwrap_err(),
            failure
        );
    }

    #[test]
    fn response_from_successor_renderer_cannot_claim_original_request() {
        let scope = request();
        let actual = owner(2, RendererGeneration::new());
        let cx = CallContext {
            sender: shell_contract::ipc::Sender {
                view: actual.view,
                window: shell_contract::WindowId(1),
                surface: shell_contract::Surface::Sheets,
            },
            request: None,
            cancel: CancelToken::default(),
            native: Some(actual.context),
        };
        assert!(scope.validate_sender(&cx).is_err());
        assert!(scope.check().is_ok());
    }
}
